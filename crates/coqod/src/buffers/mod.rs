// SPDX-License-Identifier: Apache-2.0

//! Replicated buffers (`buffer/*`, ADR 0006, issue #40).
//!
//! One [`ServerBuffer`] per open file, shared by every replica that opens
//! it (keyed by workspace and canonical path, so a symlink and its target
//! are one buffer). Each open buffer serialises its changes under a lock,
//! publishes every committed change to an in-memory [`SeqLog`] whose `seq`
//! is the buffer's `rev` (the `buffer/changes` topic), and queues it for the
//! journal. File I/O (load, save, re-reading after a disk change) runs on
//! blocking threads under a separate per-buffer I/O lock, so typing never
//! waits for the disk.
//!
//! - **Lifetime.** `buffer/changes` subscribers keep a buffer loaded. When
//!   the last one goes (or a buffer is opened and nobody subscribes), it is
//!   unloaded after [`BufferConfig::unload_after`]. Unsaved edits stay in
//!   the journal, so reopening restores them.
//! - **Disk changes.** While a workspace has open buffers, the service
//!   follows its `fs/changes`. When an open file changes, the file is read
//!   and diffed against the buffer's disk baseline (not the live text), then
//!   merged as a `disk` change, or an `agent` change if it matches a write
//!   announced with [`BufferService::expect_agent_write`].
//! - **Save** writes atomically and then makes the written text the disk
//!   baseline, so the watcher seeing Coqo's own write is a no-op. If the file
//!   changed on disk since it was read, the change is merged first and the
//!   save fails with `SAVE_CONFLICT` (unless `force`).
//! - **Journal.** A buffer with unsaved edits has a snapshot plus the
//!   changes since in the workspace database ([`journal`]); after a daemon
//!   restart, opening the file restores the unsaved text and the revision
//!   numbering.

mod agent;
mod disk;
mod journal;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use coqo_buffers::{Anchor, AnchorId, BufferError, LogLimits, Rope, ServerBuffer, text as buftext};
use coqo_protocol::methods::buffer::{
    self as bufm, BufferChange, BufferId, ChangesParams, CloseParams, EditParams,
    EditRejectedParams, OpenParams, OpenResult, Origin, OriginKind, ReplicaId, RevertParams,
    RevertResult, SaveParams, SaveResult,
};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{FileTooLarge, MAX_READ_LIMIT};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{Access, ErrorObject, EventFeed, Peer, Router, Scope, SeqLog};
use coqo_storage::DataDirs;
use futures_util::StreamExt;
use tokio::sync::watch;

use self::agent::AgentWrites;
use self::journal::Journal;
use crate::workspace::{ResolvedFile, WorkspaceService};

/// Buffer service tuning.
#[derive(Clone, Debug)]
pub struct BufferConfig {
    /// How long a buffer nobody subscribes to stays loaded.
    pub unload_after: Duration,
    /// Changes kept per buffer for `since` replays; older resumes get
    /// `RESUME_GAP` and reopen.
    pub replay: usize,
    /// Journal changes appended before a new snapshot is written.
    pub snapshot_every: usize,
    /// How long an announced agent write waits for the file to change.
    pub agent_write_ttl: Duration,
    /// Largest file that can be opened.
    pub max_file_bytes: u64,
    /// How much change history each buffer keeps.
    pub log: LogLimits,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            unload_after: Duration::from_secs(30),
            replay: 4096,
            snapshot_every: 512,
            agent_write_ttl: Duration::from_secs(120),
            max_file_bytes: MAX_READ_LIMIT,
            log: LogLimits::default(),
        }
    }
}

fn unknown_buffer(id: BufferId) -> ErrorObject {
    ErrorObject::new(
        codes::UNKNOWN_BUFFER,
        format!("no open buffer {id}; open the file again"),
    )
}

fn buffer_error(e: &BufferError) -> ErrorObject {
    let code = match e {
        BufferError::RevisionGone { .. } | BufferError::FutureRevision { .. } => {
            codes::REVISION_GONE
        }
        BufferError::InvalidEdit(_) | BufferError::ZeroSeq => codes::INVALID_EDIT,
        BufferError::NotReplayable(_) => codes::IO_ERROR,
    };
    ErrorObject::new(code, e.to_string())
}

fn join_error(e: tokio::task::JoinError) -> ErrorObject {
    ErrorObject::internal(format!("background task failed: {e}"))
}

fn io_error(path: &Path, e: &std::io::Error) -> ErrorObject {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => codes::NOT_FOUND,
        _ => codes::IO_ERROR,
    };
    ErrorObject::new(code, format!("{}: {e}", path.display()))
}

fn new_replica_id() -> ReplicaId {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    ReplicaId(format!("r-{:016x}", h.finish()))
}

/// A buffer's state behind its lock.
#[derive(Debug)]
struct State {
    buf: ServerBuffer,
    /// Whether the journal has a snapshot for it.
    journaled: bool,
    /// Changes journaled since that snapshot.
    since_snapshot: usize,
}

/// One open buffer.
#[derive(Debug)]
struct OpenBuffer {
    id: BufferId,
    workspace: WorkspaceId,
    /// Canonical path; also the journal key.
    real: PathBuf,
    key: String,
    watch_keys: Vec<(RootId, String)>,
    state: Mutex<State>,
    log: Arc<SeqLog<BufferChange>>,
    /// Serialises file I/O (save, re-reading after a disk change).
    io: tokio::sync::Mutex<()>,
    subscribers: AtomicUsize,
    unloaded: AtomicBool,
    closed: watch::Sender<bool>,
}

impl OpenBuffer {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn open_result(&self, st: &State, replica: ReplicaId) -> OpenResult {
        let b = &st.buf;
        OpenResult {
            buffer: self.id,
            last_seq: b.last_seq(&replica),
            replica,
            rev: b.rev(),
            text: b.text().to_string(),
            line_ending: b.line_ending(),
            encoding: b.encoding(),
            saved_rev: b.saved_rev(),
            dirty: b.is_dirty(),
        }
    }
}

#[derive(Debug, Default)]
struct Registry {
    buffers: HashMap<BufferId, Arc<OpenBuffer>>,
    by_path: HashMap<(WorkspaceId, PathBuf), BufferId>,
    /// `fs/changes` followers, one per workspace with open buffers.
    watches: HashMap<WorkspaceId, tokio::task::JoinHandle<()>>,
}

/// The daemon's buffers. Create one with [`BufferService::new`] and serve
/// it with [`register`].
#[derive(Debug)]
pub struct BufferService {
    workspaces: Arc<WorkspaceService>,
    dirs: DataDirs,
    config: BufferConfig,
    journal: Journal,
    registry: Mutex<Registry>,
    /// Serialises loading, so one file is never loaded twice.
    open_lock: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    agent_writes: Mutex<AgentWrites>,
}

impl BufferService {
    /// A buffer service over `workspaces`, journaling into the workspace
    /// databases under `dirs`. Must be called inside a Tokio runtime.
    pub fn new(
        workspaces: Arc<WorkspaceService>,
        dirs: DataDirs,
        config: BufferConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspaces,
            journal: Journal::start(dirs.clone()),
            dirs,
            config,
            registry: Mutex::default(),
            open_lock: tokio::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            agent_writes: Mutex::default(),
        })
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, id: BufferId) -> Result<Arc<OpenBuffer>, ErrorObject> {
        self.registry()
            .buffers
            .get(&id)
            .cloned()
            .ok_or_else(|| unknown_buffer(id))
    }

    /// The router's check for buffer-id routes: a session token may use only
    /// buffers of its own workspace. An unknown id passes, since the handler
    /// can do nothing with it but report it unknown (ids are never reused).
    pub(crate) fn require_buffer_access(
        &self,
        scope: &Scope,
        id: BufferId,
    ) -> Result<(), ErrorObject> {
        let workspace = self
            .registry()
            .buffers
            .get(&id)
            .map(|b| b.workspace.clone());
        match workspace {
            Some(workspace) => scope.require_workspace(&workspace.0),
            None => Ok(()),
        }
    }

    fn find(&self, path: &Path) -> Option<Arc<OpenBuffer>> {
        self.registry()
            .buffers
            .values()
            .find(|b| b.real == path)
            .cloned()
    }

    /// Wait until every journal write queued so far is on disk (on
    /// shutdown, and in tests).
    pub async fn flush(&self) {
        self.journal.flush().await;
    }

    /// Publish a committed change and journal it. Call with the buffer's
    /// state lock held, so changes are published and journaled in order.
    ///
    /// `moved_baseline`: the change came from the disk (a merge or a save),
    /// which the journal's change rows cannot express, so it needs a new
    /// snapshot.
    fn commit(&self, b: &OpenBuffer, st: &mut State, change: BufferChange, moved_baseline: bool) {
        let seq = b.log.publish(change.clone());
        debug_assert_eq!(seq, change.rev, "buffer/changes seq is the rev");
        let resets_baseline =
            moved_baseline || matches!(change.origin.kind, OriginKind::Disk | OriginKind::Save);
        if !st.buf.is_dirty() {
            if st.journaled {
                self.journal.delete(&b.workspace, &b.key);
                st.journaled = false;
                st.since_snapshot = 0;
            }
        } else if !st.journaled
            || resets_baseline
            || st.since_snapshot >= self.config.snapshot_every
        {
            self.journal
                .snapshot(&b.workspace, &b.key, st.buf.snapshot());
            st.journaled = true;
            st.since_snapshot = 0;
        } else {
            self.journal.change(&b.workspace, &b.key, change);
            st.since_snapshot += 1;
        }
    }

    // ---- buffer/open ----------------------------------------------------------

    /// `buffer/open`.
    pub async fn open(self: &Arc<Self>, p: OpenParams) -> Result<OpenResult, ErrorObject> {
        let file = self
            .workspaces
            .resolve_file(p.workspace.clone(), p.root, p.path.clone())
            .await?;
        let b = {
            let _loading = self.open_lock.lock().await;
            let existing = {
                let reg = self.registry();
                reg.by_path
                    .get(&(p.workspace.clone(), file.real.clone()))
                    .and_then(|id| reg.buffers.get(id))
                    .cloned()
            };
            match existing {
                Some(b) => b,
                None => self.load(&p.workspace, file).await?,
            }
        };
        let replica = p.replica.unwrap_or_else(new_replica_id);
        let result = {
            let mut st = b.lock();
            st.buf.join(&replica);
            b.open_result(&st, replica)
        };
        self.schedule_unload(&b);
        Ok(result)
    }

    /// Load a file (and its journal) into a new open buffer.
    async fn load(
        self: &Arc<Self>,
        workspace: &WorkspaceId,
        file: ResolvedFile,
    ) -> Result<Arc<OpenBuffer>, ErrorObject> {
        let key = file.real.to_string_lossy().into_owned();
        // A journal write for this file may still be queued (it was just
        // unloaded): let it land first.
        self.journal.flush().await;
        let (dirs, ws, k, path, limit) = (
            self.dirs.clone(),
            workspace.clone(),
            key.clone(),
            file.real.clone(),
            self.config.max_file_bytes,
        );
        let (bytes, entry) = tokio::task::spawn_blocking(move || {
            let bytes = disk::read(&path, limit);
            let entry = match journal::load(&dirs, &ws, &k) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "discarding an unreadable buffer journal");
                    journal::discard(&dirs, &ws, &k);
                    None
                }
            };
            (bytes, entry)
        })
        .await
        .map_err(join_error)?;
        let bytes = bytes.map_err(|e| match e {
            disk::ReadError::TooLarge { size } => ErrorObject::new(
                codes::FILE_TOO_LARGE,
                format!(
                    "{} is {size} bytes, more than the limit of {limit}",
                    file.real.display()
                ),
            )
            .with_data(FileTooLarge { size, limit }),
            disk::ReadError::Io(e) => io_error(&file.real, &e),
        })?;
        let decoded = buftext::decode(bytes).map_err(|e| {
            ErrorObject::new(
                codes::UNSUPPORTED_ENCODING,
                format!("{}: {e}", file.real.display()),
            )
        })?;

        let mut restored = None;
        if let Some(entry) = entry {
            match restore(entry) {
                Ok(r) => restored = Some(r),
                Err(e) => {
                    tracing::warn!(path = %file.real.display(), error = %e, "discarding a buffer journal that does not replay");
                    self.journal.delete(workspace, &key);
                }
            }
        }
        let journaled = restored.is_some();
        let (buf, replayed, snapshot_rev) = match restored {
            Some(r) => r,
            None => (ServerBuffer::new(decoded.clone()), Vec::new(), 0),
        };
        let log = Arc::new(SeqLog::starting_after(snapshot_rev, self.config.replay));
        for c in replayed {
            log.publish(c);
        }
        let id = BufferId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut buf = buf;
        buf.set_limits(self.config.log);
        let b = Arc::new(OpenBuffer {
            id,
            workspace: workspace.clone(),
            real: file.real.clone(),
            key,
            watch_keys: file.watch_keys,
            state: Mutex::new(State {
                buf,
                journaled,
                since_snapshot: 0,
            }),
            log,
            io: tokio::sync::Mutex::new(()),
            subscribers: AtomicUsize::new(0),
            unloaded: AtomicBool::new(false),
            closed: watch::channel(false).0,
        });
        if journaled {
            // The file may have changed while the daemon was down.
            let mut st = b.lock();
            let disk = Rope::from_str(&decoded.text);
            if let Some(c) = st.buf.apply_disk(disk, Origin::of(OriginKind::Disk)) {
                self.commit(&b, &mut st, c, true);
            }
            tracing::info!(path = %b.real.display(), rev = st.buf.rev(), dirty = st.buf.is_dirty(), "restored unsaved edits from the journal");
        }
        {
            let mut reg = self.registry();
            reg.by_path.insert((workspace.clone(), b.real.clone()), id);
            reg.buffers.insert(id, Arc::clone(&b));
        }
        self.ensure_watch(workspace);
        Ok(b)
    }

    // ---- lifetime ---------------------------------------------------------------

    /// Unload `b` after the grace period if nobody subscribes by then.
    fn schedule_unload(self: &Arc<Self>, b: &Arc<OpenBuffer>) {
        let svc = Arc::clone(self);
        let b = Arc::clone(b);
        tokio::spawn(async move {
            tokio::time::sleep(svc.config.unload_after).await;
            svc.unload_if_unused(&b);
        });
    }

    fn unload_if_unused(&self, b: &Arc<OpenBuffer>) {
        let mut reg = self.registry();
        if b.subscribers.load(Ordering::Acquire) > 0 || b.unloaded.load(Ordering::Acquire) {
            return;
        }
        b.unloaded.store(true, Ordering::Release);
        reg.buffers.remove(&b.id);
        reg.by_path.remove(&(b.workspace.clone(), b.real.clone()));
        let workspace_done = !reg.buffers.values().any(|o| o.workspace == b.workspace);
        if workspace_done && let Some(task) = reg.watches.remove(&b.workspace) {
            task.abort();
        }
        drop(reg);
        let st = b.lock();
        if st.buf.is_dirty() {
            // Keep the unsaved edits, compactly.
            self.journal
                .snapshot(&b.workspace, &b.key, st.buf.snapshot());
        } else if st.journaled {
            self.journal.delete(&b.workspace, &b.key);
        }
        drop(st);
        b.closed.send_replace(true);
        tracing::debug!(buffer = %b.id, path = %b.real.display(), "buffer unloaded");
    }

    // ---- disk changes ---------------------------------------------------------

    /// Follow `fs/changes` of `workspace` while it has open buffers.
    fn ensure_watch(self: &Arc<Self>, workspace: &WorkspaceId) {
        let mut reg = self.registry();
        if reg.watches.get(workspace).is_some_and(|t| !t.is_finished()) {
            return;
        }
        let svc = Arc::downgrade(self);
        let ws = workspace.clone();
        let task = tokio::spawn(async move {
            loop {
                let Some(s) = svc.upgrade() else { return };
                let params = coqo_protocol::methods::fs::ChangesParams {
                    workspace: ws.clone(),
                };
                let feed = match s.workspaces.fs_changes(params, None).await {
                    Ok(f) => f,
                    Err(e) => {
                        // Typically a closed workspace: no watcher, so no
                        // disk changes (saves still check the file).
                        tracing::debug!(workspace = %ws, error = %e, "buffers cannot follow fs/changes");
                        return;
                    }
                };
                // Changes between loading and the watcher starting would be
                // missed otherwise.
                s.check_all(&ws);
                drop(s);
                let mut events = feed.events;
                let mut lagged = false;
                while let Some(item) = events.next().await {
                    let Some(s) = svc.upgrade() else { return };
                    match item {
                        Ok(batch) => s.on_fs_changes(&ws, &batch.event),
                        Err(e) => {
                            tracing::debug!(workspace = %ws, error = %e, "fs/changes lagged; rechecking open buffers");
                            lagged = true;
                            break;
                        }
                    }
                }
                let Some(s) = svc.upgrade() else { return };
                if lagged {
                    s.check_all(&ws);
                } else {
                    return; // the workspace was closed
                }
            }
        });
        reg.watches.insert(workspace.clone(), task);
    }

    fn on_fs_changes(
        self: &Arc<Self>,
        workspace: &WorkspaceId,
        c: &coqo_protocol::methods::fs::FsChanges,
    ) {
        let hit: Vec<Arc<OpenBuffer>> = self
            .registry()
            .buffers
            .values()
            .filter(|b| &b.workspace == workspace)
            .filter(|b| {
                c.rescan
                    || b.watch_keys
                        .iter()
                        .any(|(root, path)| *root == c.root && c.paths.binary_search(path).is_ok())
            })
            .cloned()
            .collect();
        for b in hit {
            let svc = Arc::clone(self);
            tokio::spawn(async move { svc.check_disk(&b).await });
        }
    }

    fn check_all(self: &Arc<Self>, workspace: &WorkspaceId) {
        let all: Vec<_> = self
            .registry()
            .buffers
            .values()
            .filter(|b| &b.workspace == workspace)
            .cloned()
            .collect();
        for b in all {
            let svc = Arc::clone(self);
            tokio::spawn(async move { svc.check_disk(&b).await });
        }
    }

    /// Re-read `b`'s file and merge it if it changed.
    async fn check_disk(&self, b: &OpenBuffer) {
        let _io = b.io.lock().await;
        if b.unloaded.load(Ordering::Acquire) {
            return;
        }
        let (path, limit) = (b.real.clone(), self.config.max_file_bytes);
        let read = tokio::task::spawn_blocking(move || disk::read(&path, limit)).await;
        let bytes = match read {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(disk::ReadError::Io(e))) if e.kind() == std::io::ErrorKind::NotFound => {
                // Deleted (or mid-rename); the buffer keeps its text.
                tracing::debug!(path = %b.real.display(), "open file is gone from disk");
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(path = %b.real.display(), error = ?e, "cannot re-read an open file");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "re-reading an open file failed");
                return;
            }
        };
        let decoded = match buftext::decode(bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(path = %b.real.display(), error = %e, "open file is no longer UTF-8; keeping the buffer");
                return;
            }
        };
        self.merge_disk(b, Rope::from_str(&decoded.text));
    }

    fn merge_disk(&self, b: &OpenBuffer, new_disk: Rope) {
        let mut st = b.lock();
        if new_disk == *st.buf.disk() {
            return;
        }
        let origin = self
            .agent_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_match(
                &b.real,
                st.buf.disk(),
                &new_disk,
                self.config.agent_write_ttl,
            )
            .map_or_else(|| Origin::of(OriginKind::Disk), Origin::agent);
        if let Some(c) = st.buf.apply_disk(new_disk, origin) {
            tracing::debug!(path = %b.real.display(), rev = c.rev, origin = ?c.origin.kind, conflict = c.conflict, "merged a disk change");
            self.commit(b, &mut st, c, true);
        }
    }

    // ---- buffer/edit, revertChange, save, close -------------------------------

    /// `buffer/edit`: apply a replica's change, or tell it why not.
    pub async fn edit(&self, peer: &Peer, p: EditParams) {
        let result = self.get(p.buffer).and_then(|b| {
            let mut st = b.lock();
            match st
                .buf
                .apply_client(&p.replica, p.client_seq, p.base_rev, &p.edits)
            {
                Ok(Some(c)) => {
                    self.commit(&b, &mut st, c, false);
                    Ok(())
                }
                Ok(None) => Ok(()), // a resend of a committed change
                Err(e) => Err(buffer_error(&e)),
            }
        });
        if let Err(error) = result {
            tracing::debug!(buffer = %p.buffer, replica = %p.replica, %error, "rejecting buffer/edit");
            let note = EditRejectedParams {
                buffer: p.buffer,
                replica: p.replica,
                client_seq: p.client_seq,
                error,
            };
            if let Err(e) = peer.notify::<bufm::EditRejected>(note).await {
                tracing::debug!(error = %e, "cannot send buffer/editRejected");
            }
        }
    }

    /// `buffer/revertChange`.
    pub async fn revert(&self, p: RevertParams) -> Result<RevertResult, ErrorObject> {
        let b = self.get(p.buffer)?;
        let mut st = b.lock();
        let c = st.buf.revert(p.rev).map_err(|e| buffer_error(&e))?;
        let rev = c.rev;
        self.commit(&b, &mut st, c, false);
        Ok(RevertResult { rev })
    }

    /// `buffer/save`.
    pub async fn save(&self, p: SaveParams) -> Result<SaveResult, ErrorObject> {
        let b = self.get(p.buffer)?;
        let _io = b.io.lock().await;
        let (text, rev, baseline, encoding) = {
            let st = b.lock();
            (
                st.buf.text().clone(),
                st.buf.rev(),
                st.buf.disk().clone(),
                st.buf.encoding(),
            )
        };
        let (path, limit, force) = (b.real.clone(), self.config.max_file_bytes, p.force);
        let written = text.clone();
        let outcome = tokio::task::spawn_blocking(move || -> Result<Option<Rope>, ErrorObject> {
            if !force {
                // Never overwrite a change we have not seen yet.
                match disk::read(&path, limit) {
                    Ok(bytes) => {
                        if let Ok(d) = buftext::decode(bytes) {
                            let on_disk = Rope::from_str(&d.text);
                            if on_disk != baseline {
                                return Ok(Some(on_disk));
                            }
                        }
                    }
                    Err(disk::ReadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => {}
                }
            }
            let bytes = buftext::encode(&written.to_string(), encoding);
            disk::write_atomic(&path, &bytes).map_err(|e| io_error(&path, &e))?;
            Ok(None)
        })
        .await
        .map_err(join_error)??;
        if let Some(on_disk) = outcome {
            self.merge_disk(&b, on_disk);
            let rev = b.lock().buf.rev();
            return Err(ErrorObject::new(
                codes::SAVE_CONFLICT,
                format!(
                    "{} changed on disk; the change was merged into the buffer (now at rev {rev}). Review it and save again, or save with force",
                    b.real.display()
                ),
            )
            .with_data(serde_json::json!({ "rev": rev })));
        }
        let mut st = b.lock();
        let c = st.buf.mark_saved(text, rev);
        let saved_rev = c.rev;
        self.commit(&b, &mut st, c, true);
        Ok(SaveResult { saved_rev })
    }

    /// `buffer/close`.
    pub async fn close(self: &Arc<Self>, p: CloseParams) -> Result<(), ErrorObject> {
        let b = self.get(p.buffer)?;
        b.lock().buf.leave(&p.replica);
        if b.subscribers.load(Ordering::Acquire) == 0 {
            self.schedule_unload(&b);
        }
        Ok(())
    }

    /// The `buffer/changes` topic. The subscription keeps the buffer loaded.
    pub fn changes(
        self: &Arc<Self>,
        p: ChangesParams,
        since: Option<u64>,
    ) -> Result<EventFeed<BufferChange>, ErrorObject> {
        let b = self.get(p.buffer)?;
        let feed = b.log.subscribe(since)?;
        let guard = Subscriber::new(self, &b);
        let mut closed = b.closed.subscribe();
        let until = async move {
            let _ = closed.wait_for(|c| *c).await;
        };
        Ok(EventFeed {
            head: feed.head,
            events: feed
                .events
                .take_until(until)
                .map(move |e| {
                    let _keep = &guard;
                    e
                })
                .boxed(),
        })
    }

    // ---- daemon-side API (agents, anchors) ------------------------------------

    /// An agent is about to write `path` (from an ACP `tool_call` diff:
    /// `old_text` → `new_text`, either the whole file or the replaced
    /// snippet). When the file then changes on disk in a way that matches,
    /// the change is attributed to `session` (`origin.kind = "agent"`)
    /// rather than `disk`.
    pub fn expect_agent_write(
        &self,
        session: &str,
        path: &Path,
        old_text: Option<String>,
        new_text: String,
    ) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.agent_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect(session.to_owned(), path, old_text, new_text);
    }

    /// The text of the open buffer for `path`, unsaved edits included (for
    /// ACP `fs/read_text_file`). `None` if the file is not open.
    pub fn open_text(&self, path: &Path) -> Option<String> {
        let path = std::fs::canonicalize(path).ok()?;
        let b = self.find(&path)?;
        Some(b.lock().buf.text().to_string())
    }

    /// Replace the text of the open buffer for `path` with an agent's new
    /// version (ACP `fs/write_text_file`), diffed against the live text so
    /// it lands as a tracked, revertible `agent` change. Returns its
    /// revision, or `None` if the file is not open (then write the file;
    /// the watcher brings it into buffers opened later).
    pub fn agent_replace(&self, session: &str, path: &Path, text: &str) -> Option<u64> {
        let path = std::fs::canonicalize(path).ok()?;
        let b = self.find(&path)?;
        let mut st = b.lock();
        let c = st
            .buf
            .replace_text(Origin::agent(session), &Rope::from_str(text));
        let rev = c.rev;
        self.commit(&b, &mut st, c, false);
        Some(rev)
    }

    /// Track a byte range of an open buffer through later changes.
    pub fn add_anchor(
        &self,
        buffer: BufferId,
        range: Range<usize>,
    ) -> Result<AnchorId, ErrorObject> {
        let b = self.get(buffer)?;
        let mut st = b.lock();
        st.buf.add_anchor(range).map_err(|e| buffer_error(&e))
    }

    /// An anchor's current range (`None` if unknown).
    pub fn anchor(&self, buffer: BufferId, id: AnchorId) -> Option<Anchor> {
        self.get(buffer).ok()?.lock().buf.anchor(id)
    }

    /// The current revision of an open buffer.
    pub fn rev(&self, buffer: BufferId) -> Option<u64> {
        Some(self.get(buffer).ok()?.lock().buf.rev())
    }

    /// The current text of an open buffer.
    pub fn text(&self, buffer: BufferId) -> Option<String> {
        Some(self.get(buffer).ok()?.lock().buf.text().to_string())
    }

    /// What the syntax service needs to bring its parse of `buffer` from
    /// revision `since` to the current one: the changes in between if the
    /// log still has them all, else the whole text.
    pub fn syntax_view(
        &self,
        buffer: BufferId,
        since: Option<u64>,
    ) -> Result<SyntaxView, ErrorObject> {
        let b = self.get(buffer)?;
        let st = b.lock();
        let rev = st.buf.rev();
        let changes = since.filter(|&s| s <= rev).and_then(|since| {
            (since + 1..=rev)
                .map(|r| st.buf.entry(r).map(|e| e.op.to_edits()))
                .collect::<Option<Vec<_>>>()
        });
        let text = changes.is_none().then(|| st.buf.text().to_string());
        Ok(SyntaxView {
            workspace: b.workspace.clone(),
            path: b.real.clone(),
            rev,
            changes,
            text,
        })
    }

    /// An agent edit at the head of an open buffer (edits against its
    /// current text).
    pub fn agent_edit(
        &self,
        buffer: BufferId,
        session: &str,
        edits: &[bufm::TextEdit],
    ) -> Result<u64, ErrorObject> {
        let b = self.get(buffer)?;
        let mut st = b.lock();
        let c = st
            .buf
            .apply_head(Origin::agent(session), edits)
            .map_err(|e| buffer_error(&e))?;
        let rev = c.rev;
        self.commit(&b, &mut st, c, false);
        Ok(rev)
    }
}

/// An open buffer as [`BufferService::syntax_view`] sees it.
#[derive(Debug)]
pub struct SyntaxView {
    /// The buffer's workspace.
    pub workspace: WorkspaceId,
    /// The file's canonical path.
    pub path: PathBuf,
    /// The current revision.
    pub rev: u64,
    /// The changes after the asked-for revision, oldest first, each a list
    /// of edits in the coordinates before it. `None` if they are not all in
    /// the log (or no revision was asked for): then `text` is set.
    pub changes: Option<Vec<Vec<bufm::TextEdit>>>,
    /// The text at `rev`, when `changes` is `None`.
    pub text: Option<String>,
}

/// Rebuild a buffer from its journal: the buffer, the replayed changes (for
/// the replay log) and the snapshot's revision.
fn restore(entry: journal::Entry) -> Result<(ServerBuffer, Vec<BufferChange>, u64), BufferError> {
    let snapshot_rev = entry.snapshot.rev;
    let mut buf = ServerBuffer::restore(entry.snapshot)?;
    for c in &entry.changes {
        buf.replay(c)?;
    }
    Ok((buf, entry.changes, snapshot_rev))
}

/// Counts a `buffer/changes` subscriber; the last one leaving schedules the
/// buffer's unload.
#[derive(Debug)]
struct Subscriber {
    svc: std::sync::Weak<BufferService>,
    buffer: Arc<OpenBuffer>,
}

impl Subscriber {
    fn new(svc: &Arc<BufferService>, b: &Arc<OpenBuffer>) -> Self {
        b.subscribers.fetch_add(1, Ordering::AcqRel);
        Self {
            svc: Arc::downgrade(svc),
            buffer: Arc::clone(b),
        }
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        if self.buffer.subscribers.fetch_sub(1, Ordering::AcqRel) == 1
            && let Some(svc) = self.svc.upgrade()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let b = Arc::clone(&self.buffer);
            rt.spawn(async move {
                tokio::time::sleep(svc.config.unload_after).await;
                svc.unload_if_unused(&b);
            });
        }
    }
}

/// Write out queued journal entries when `shutdown` is cancelled.
pub fn flush_on_shutdown(svc: &Arc<BufferService>, shutdown: tokio_util::sync::CancellationToken) {
    let svc = Arc::clone(svc);
    tokio::spawn(async move {
        shutdown.cancelled().await;
        svc.flush().await;
    });
}

/// Session tokens, for buffers open in their own workspace.
fn own_buffer<P: 'static>(svc: &Arc<BufferService>, buffer: fn(&P) -> BufferId) -> Access<P> {
    let svc = Arc::clone(svc);
    Access::check(move |scope, p| svc.require_buffer_access(scope, buffer(p)))
}

/// Register every `buffer/*` method and topic on `router`.
///
/// A session token (`coqod mcp`) may open, edit, follow and close buffers
/// of its own workspace. Saving and reverting a change are the user's
/// review actions and need the daemon token.
pub fn register(router: Router, svc: Arc<BufferService>) -> Router {
    let open = Arc::clone(&svc);
    let edit = Arc::clone(&svc);
    let revert = Arc::clone(&svc);
    let save = Arc::clone(&svc);
    let close = Arc::clone(&svc);
    let changes = Arc::clone(&svc);
    router
        .method_with::<bufm::Open, _, _>(Access::workspace(|p| &p.workspace.0), move |_ctx, p| {
            let s = Arc::clone(&open);
            async move { s.open(p).await }
        })
        .notification_with::<bufm::Edit, _, _>(
            own_buffer(&svc, |p: &EditParams| p.buffer),
            move |ctx, p| {
                let s = Arc::clone(&edit);
                async move { s.edit(&ctx.peer, p).await }
            },
        )
        .method_with::<bufm::RevertChange, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&revert);
            async move { s.revert(p).await }
        })
        .method_with::<bufm::Save, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&save);
            async move { s.save(p).await }
        })
        .method_with::<bufm::Close, _, _>(
            own_buffer(&svc, |p: &CloseParams| p.buffer),
            move |_ctx, p| {
                let s = Arc::clone(&close);
                async move { s.close(p).await }
            },
        )
        .topic_with::<bufm::Changes, _, _>(
            own_buffer(&svc, |p: &ChangesParams| p.buffer),
            move |_ctx, p, since| {
                let s = Arc::clone(&changes);
                async move { s.changes(p, since) }
            },
        )
}
