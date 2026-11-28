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
//! - **Writes in progress.** A writer that truncates and then writes (many
//!   tools, `std::fs::write` among them) leaves the file empty for a moment,
//!   and the watcher can read it then. An empty read of a non-empty file is
//!   therefore held back for [`BufferConfig::truncate_settle`] and the file
//!   read again; while an announced agent write of some text to the file is
//!   pending, it is held until that text lands (up to [`MAX_AGENT_HOLD`]).
//!   Merging the empty moment would turn one change into a deletion plus a
//!   re-insertion, lose the agent attribution and misplace unsaved edits.
//! - **Renames.** `fs/rename` moves the open buffers of the files it moves
//!   ([`BufferService::move_files`]): same id, text and unsaved edits, new
//!   path. A deleted file's buffer keeps its text; saving it writes the file
//!   again.
//! - **Save** writes atomically and then makes the written text the disk
//!   baseline, so the watcher seeing Coqo's own write is a no-op. If the file
//!   changed on disk since it was read, the change is merged first and the
//!   save fails with `SAVE_CONFLICT` (unless `force`).
//! - **Session replicas.** A session-scoped connection (`coqod mcp`) may
//!   edit and close only the replicas it opened (`SessionReplicas`), may
//!   reopen only those by id, and its replicas are released when the
//!   connection closes (#198). The daemon token is not restricted.
//! - **Journal.** A buffer with unsaved edits has a snapshot plus the
//!   changes since in the workspace database ([`journal`]); after a daemon
//!   restart, opening the file restores the unsaved text and the revision
//!   numbering.

mod agent;
mod disk;
mod journal;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use coqo_buffers::{Anchor, AnchorId, BufferError, LogLimits, Rope, ServerBuffer, text as buftext};
use coqo_protocol::methods::buffer::{
    self as bufm, BufferChange, BufferId, ChangesParams, CloseParams, EditParams,
    EditRejectedParams, OpenParams, OpenResult, Origin, OriginKind, ReplicaId, RevertParams,
    RevertResult, SaveParams, SaveResult,
};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{FileTooLarge, MAX_READ_LIMIT};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{Access, ErrorObject, EventFeed, Peer, RequestContext, Router, Scope, SeqLog};
use coqo_storage::DataDirs;
use futures_util::StreamExt;
use tokio::sync::watch;

use self::agent::AgentWrites;
use self::journal::Journal;
use crate::workspace::{Renamed, ResolvedFile, WorkspaceService};

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
    /// How long an open file that reads as empty (but was not) is given to
    /// get its new contents before the emptiness is merged: a writer that
    /// truncates first is usually mid-write.
    pub truncate_settle: Duration,
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
            truncate_settle: Duration::from_millis(250),
            max_file_bytes: MAX_READ_LIMIT,
            log: LogLimits::default(),
        }
    }
}

/// The longest an empty read is held back while an announced agent write
/// to the file is pending (see *Writes in progress* above).
pub const MAX_AGENT_HOLD: Duration = Duration::from_secs(5);

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

/// Where an open buffer's file is. It changes when `fs/rename` moves the
/// file ([`BufferService::moved`]).
#[derive(Debug)]
struct Location {
    /// Canonical path.
    real: PathBuf,
    /// The journal key (the canonical path, as a string).
    key: String,
    /// `(root, path)` pairs `fs/changes` reports the file under.
    watch_keys: Vec<(RootId, String)>,
}

/// One open buffer.
#[derive(Debug)]
struct OpenBuffer {
    id: BufferId,
    workspace: WorkspaceId,
    /// Lock order: `state`, then `location`.
    location: Mutex<Location>,
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

    fn location(&self) -> MutexGuard<'_, Location> {
        self.location.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The file's canonical path.
    fn real(&self) -> PathBuf {
        self.location().real.clone()
    }

    /// The journal key.
    fn key(&self) -> String {
        self.location().key.clone()
    }

    /// Whether `fs/changes` of `root` reporting `paths` (sorted) covers the
    /// file.
    fn watched_in(&self, root: RootId, paths: &[String]) -> bool {
        self.location()
            .watch_keys
            .iter()
            .any(|(r, path)| *r == root && paths.binary_search(path).is_ok())
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
    sessions: Mutex<SessionReplicas>,
    /// Files `buffer/save` wrote lately (canonical path, when), newest
    /// last: the user's saves, which no agent made (#286).
    saves: Mutex<VecDeque<(PathBuf, Instant)>>,
}

/// Saves remembered for [`BufferService::saved_recently`] at most.
const RECENT_SAVES: usize = 64;

/// The replicas session-scoped connections opened (#198). A session token
/// may edit and close only these, and reopen only these by id: replica ids
/// show up in every change's `origin`, so naming one is no proof of owning
/// it. Daemon-token replicas (the UI's) are never recorded here.
#[derive(Debug, Default)]
struct SessionReplicas {
    owned: HashMap<ReplicaId, Owned>,
}

#[derive(Debug)]
struct Owned {
    /// The connection that opened the replica.
    connection: u64,
    /// The buffers it has joined and not closed.
    buffers: HashSet<BufferId>,
}

impl SessionReplicas {
    fn owns_replica(&self, connection: u64, replica: &ReplicaId) -> bool {
        self.owned
            .get(replica)
            .is_some_and(|o| o.connection == connection)
    }

    fn owns(&self, connection: u64, replica: &ReplicaId, buffer: BufferId) -> bool {
        self.owned
            .get(replica)
            .is_some_and(|o| o.connection == connection && o.buffers.contains(&buffer))
    }

    /// Record `replica` joining `buffer` for `connection`. True if it is the
    /// connection's first replica (so nobody watches the connection yet).
    fn record(&mut self, connection: u64, replica: &ReplicaId, buffer: BufferId) -> bool {
        let first = !self.owned.values().any(|o| o.connection == connection);
        self.owned
            .entry(replica.clone())
            .or_insert_with(|| Owned {
                connection,
                buffers: HashSet::new(),
            })
            .buffers
            .insert(buffer);
        first
    }

    fn closed(&mut self, replica: &ReplicaId, buffer: BufferId) {
        if let Some(o) = self.owned.get_mut(replica) {
            o.buffers.remove(&buffer);
        }
    }

    /// Forget `connection`'s replicas; returns the buffers each still joined.
    fn release(&mut self, connection: u64) -> Vec<(ReplicaId, BufferId)> {
        let mut released = Vec::new();
        self.owned.retain(|replica, o| {
            if o.connection != connection {
                return true;
            }
            released.extend(o.buffers.iter().map(|b| (replica.clone(), *b)));
            false
        });
        released
    }
}

fn forbidden(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(coqo_protocol::codes::FORBIDDEN, message)
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
            sessions: Mutex::default(),
            saves: Mutex::default(),
        })
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sessions(&self) -> MutexGuard<'_, SessionReplicas> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
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
            .find(|b| b.real() == path)
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
                self.journal.delete(&b.workspace, &b.key());
                st.journaled = false;
                st.since_snapshot = 0;
            }
        } else if !st.journaled
            || resets_baseline
            || st.since_snapshot >= self.config.snapshot_every
        {
            self.journal
                .snapshot(&b.workspace, &b.key(), st.buf.snapshot());
            st.journaled = true;
            st.since_snapshot = 0;
        } else {
            self.journal.change(&b.workspace, &b.key(), change);
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

    /// `buffer/open` on a connection: [`open`](Self::open), plus the
    /// session-replica rules. A session token may name only a replica its
    /// connection opened; the replica it gets is recorded as its own and
    /// released when the connection closes.
    pub async fn open_as(
        self: &Arc<Self>,
        ctx: &RequestContext,
        p: OpenParams,
    ) -> Result<OpenResult, ErrorObject> {
        if *ctx.scope == Scope::Daemon {
            return self.open(p).await;
        }
        let connection = ctx.peer.connection();
        if let Some(replica) = &p.replica
            && !self.sessions().owns_replica(connection, replica)
        {
            return Err(forbidden(format!(
                "replica {replica} was not opened on this connection; open without `replica` to get one"
            )));
        }
        let result = self.open(p).await?;
        let first = self
            .sessions()
            .record(connection, &result.replica, result.buffer);
        if first {
            let svc = Arc::downgrade(self);
            let peer = ctx.peer.clone();
            tokio::spawn(async move {
                peer.closed().await;
                if let Some(svc) = svc.upgrade() {
                    svc.release_connection(connection);
                }
            });
        }
        Ok(result)
    }

    /// The handler's check for routes that act as a replica (`buffer/edit`,
    /// `buffer/close`): a session token must have opened `replica` in
    /// `buffer` on this connection.
    pub(crate) fn require_replica(
        &self,
        ctx: &RequestContext,
        buffer: BufferId,
        replica: &ReplicaId,
    ) -> Result<(), ErrorObject> {
        if *ctx.scope == Scope::Daemon
            || self.sessions().owns(ctx.peer.connection(), replica, buffer)
        {
            Ok(())
        } else {
            Err(forbidden(format!(
                "replica {replica} of buffer {buffer} was not opened on this connection; a session token may edit and close only its own replicas"
            )))
        }
    }

    /// Release the replicas a closed session connection opened.
    fn release_connection(self: &Arc<Self>, connection: u64) {
        let released = self.sessions().release(connection);
        for (replica, buffer) in &released {
            if let Ok(b) = self.get(*buffer) {
                self.leave(&b, replica);
            }
        }
        if !released.is_empty() {
            tracing::debug!(
                connection,
                replicas = released.len(),
                "released a closed session connection's replicas"
            );
        }
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
            location: Mutex::new(Location {
                real: file.real.clone(),
                key,
                watch_keys: file.watch_keys,
            }),
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
            tracing::info!(path = %b.real().display(), rev = st.buf.rev(), dirty = st.buf.is_dirty(), "restored unsaved edits from the journal");
        }
        {
            let mut reg = self.registry();
            reg.by_path.insert((workspace.clone(), b.real()), id);
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
        reg.by_path.remove(&(b.workspace.clone(), b.real()));
        let workspace_done = !reg.buffers.values().any(|o| o.workspace == b.workspace);
        if workspace_done && let Some(task) = reg.watches.remove(&b.workspace) {
            task.abort();
        }
        drop(reg);
        let st = b.lock();
        if st.buf.is_dirty() {
            // Keep the unsaved edits, compactly.
            self.journal
                .snapshot(&b.workspace, &b.key(), st.buf.snapshot());
        } else if st.journaled {
            self.journal.delete(&b.workspace, &b.key());
        }
        drop(st);
        b.closed.send_replace(true);
        tracing::debug!(buffer = %b.id, path = %b.real().display(), "buffer unloaded");
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
            .filter(|b| c.rescan || b.watched_in(c.root, &c.paths))
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
        let started = tokio::time::Instant::now();
        let mut held = false;
        loop {
            if b.unloaded.load(Ordering::Acquire) {
                return;
            }
            let Some(disk) = self.read_disk(b).await else {
                return;
            };
            if disk.len() == 0 && self.maybe_mid_write(b, held, started.elapsed()) {
                tracing::debug!(path = %b.real().display(), "open file reads as empty; waiting for the rest of the write");
                held = true;
                tokio::time::sleep(self.config.truncate_settle).await;
                continue;
            }
            self.merge_disk(b, disk);
            return;
        }
    }

    /// Whether `b`'s file reading as empty may be a writer's truncation, to
    /// be read again before merging. `held`: it was held back already, for
    /// `waited` so far.
    fn maybe_mid_write(&self, b: &OpenBuffer, held: bool, waited: Duration) -> bool {
        let st = b.lock();
        let before = st.buf.disk();
        if before.len() == 0 {
            return false; // no change at all
        }
        let ttl = self.config.agent_write_ttl;
        let mut writes = self.agent_writes.lock().unwrap_or_else(|e| e.into_inner());
        if writes.awaits_text(&b.real(), ttl) {
            return waited < MAX_AGENT_HOLD;
        }
        !held && !writes.explains(&b.real(), before, &Rope::new(), ttl)
    }

    /// Read and decode `b`'s file; `None` if it cannot be (logged).
    async fn read_disk(&self, b: &OpenBuffer) -> Option<Rope> {
        let (path, limit) = (b.real(), self.config.max_file_bytes);
        let read = tokio::task::spawn_blocking(move || disk::read(&path, limit)).await;
        let bytes = match read {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(disk::ReadError::Io(e))) if e.kind() == std::io::ErrorKind::NotFound => {
                // Deleted (or mid-rename); the buffer keeps its text.
                tracing::debug!(path = %b.real().display(), "open file is gone from disk");
                return None;
            }
            Ok(Err(e)) => {
                tracing::warn!(path = %b.real().display(), error = ?e, "cannot re-read an open file");
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "re-reading an open file failed");
                return None;
            }
        };
        match buftext::decode(bytes) {
            Ok(d) => Some(Rope::from_str(&d.text)),
            Err(e) => {
                tracing::warn!(path = %b.real().display(), error = %e, "open file is no longer UTF-8; keeping the buffer");
                None
            }
        }
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
                &b.real(),
                st.buf.disk(),
                &new_disk,
                self.config.agent_write_ttl,
            )
            .map_or_else(|| Origin::of(OriginKind::Disk), Origin::agent);
        if let Some(c) = st.buf.apply_disk(new_disk, origin) {
            tracing::debug!(path = %b.real().display(), rev = c.rev, origin = ?c.origin.kind, conflict = c.conflict, "merged a disk change");
            self.commit(b, &mut st, c, true);
        }
    }

    // ---- buffer/edit, revertChange, save, close -------------------------------

    /// `buffer/edit` on a connection: [`edit`](Self::edit) if the replica
    /// may edit ([`require_replica`](Self::require_replica)), else
    /// `buffer/editRejected` with `FORBIDDEN`.
    pub async fn edit_as(&self, ctx: &RequestContext, p: EditParams) {
        match self.require_replica(ctx, p.buffer, &p.replica) {
            Ok(()) => self.edit(&ctx.peer, p).await,
            Err(error) => reject_edit(&ctx.peer, &p, error).await,
        }
    }

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
            reject_edit(peer, &p, error).await;
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
        let (path, limit, force) = (b.real(), self.config.max_file_bytes, p.force);
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
                    b.real().display()
                ),
            )
            .with_data(serde_json::json!({ "rev": rev })));
        }
        {
            let mut saves = self.saves.lock().unwrap_or_else(|e| e.into_inner());
            if saves.len() >= RECENT_SAVES {
                saves.pop_front();
            }
            saves.push_back((b.real(), Instant::now()));
        }
        let mut st = b.lock();
        let c = st.buf.mark_saved(text, rev);
        let saved_rev = c.rev;
        self.commit(&b, &mut st, c, true);
        Ok(SaveResult { saved_rev })
    }

    /// Whether `buffer/save` wrote `path` (canonical) in the last `within`:
    /// the watcher then reports the user's save, not an agent's change.
    pub fn saved_recently(&self, path: &Path, within: Duration) -> bool {
        self.saves
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .take_while(|(_, at)| at.elapsed() < within)
            .any(|(p, _)| p == path)
    }

    /// `buffer/close` on a connection: [`close`](Self::close) if the
    /// replica may ([`require_replica`](Self::require_replica)).
    pub async fn close_as(
        self: &Arc<Self>,
        ctx: &RequestContext,
        p: CloseParams,
    ) -> Result<(), ErrorObject> {
        self.require_replica(ctx, p.buffer, &p.replica)?;
        self.close(p.clone()).await?;
        if *ctx.scope != Scope::Daemon {
            self.sessions().closed(&p.replica, p.buffer);
        }
        Ok(())
    }

    /// `buffer/close`.
    pub async fn close(self: &Arc<Self>, p: CloseParams) -> Result<(), ErrorObject> {
        let b = self.get(p.buffer)?;
        self.leave(&b, &p.replica);
        Ok(())
    }

    /// `replica` is done with `b`.
    fn leave(self: &Arc<Self>, b: &Arc<OpenBuffer>, replica: &ReplicaId) {
        b.lock().buf.leave(replica);
        if b.subscribers.load(Ordering::Acquire) == 0 {
            self.schedule_unload(b);
        }
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
            path: b.real(),
            rev,
            changes,
            text,
        })
    }

    /// The open buffer of file `real` (canonical) in `workspace`, if any.
    pub fn buffer_at(&self, workspace: &WorkspaceId, real: &Path) -> Option<BufferId> {
        self.registry()
            .by_path
            .get(&(workspace.clone(), real.to_path_buf()))
            .copied()
    }

    /// Run `rename` (`fs/rename` on disk) and move the open buffers of the
    /// files it moved with it: their path, `fs/changes` keys and journal
    /// entry change, their id, text and unsaved edits stay. `from` is the
    /// real path being moved; while `rename` runs, those buffers' I/O is held,
    /// so a save cannot write the old path in the middle.
    pub(crate) async fn move_files<F>(
        &self,
        workspace: &WorkspaceId,
        from: &Path,
        rename: F,
    ) -> Result<Renamed, ErrorObject>
    where
        F: Future<Output = Result<Renamed, ErrorObject>>,
    {
        let mut affected: Vec<Arc<OpenBuffer>> = self
            .registry()
            .buffers
            .values()
            .filter(|b| &b.workspace == workspace && b.real().starts_with(from))
            .cloned()
            .collect();
        affected.sort_by_key(|b| b.id);
        let mut held = Vec::with_capacity(affected.len());
        for b in &affected {
            held.push(b.io.lock().await);
        }
        let renamed = rename.await?;
        for b in &affected {
            let old = b.real();
            let Ok(rest) = old.strip_prefix(&renamed.from) else {
                continue;
            };
            let real = if rest.as_os_str().is_empty() {
                renamed.to.clone()
            } else {
                renamed.to.join(rest)
            };
            let watch_keys = self
                .workspaces
                .watch_keys(workspace.clone(), real.clone())
                .await
                .unwrap_or_default();
            self.relocate(b, real, watch_keys);
        }
        drop(held);
        Ok(renamed)
    }

    /// Point `b` at its file's new path.
    fn relocate(&self, b: &OpenBuffer, real: PathBuf, watch_keys: Vec<(RootId, String)>) {
        let key = real.to_string_lossy().into_owned();
        let st = b.lock();
        let (old_real, old_key) = {
            let mut loc = b.location();
            loc.watch_keys = watch_keys;
            (
                std::mem::replace(&mut loc.real, real.clone()),
                std::mem::replace(&mut loc.key, key.clone()),
            )
        };
        if st.journaled {
            // The unsaved edits move to the new key.
            self.journal.snapshot(&b.workspace, &key, st.buf.snapshot());
            self.journal.delete(&b.workspace, &old_key);
        }
        drop(st);
        let mut reg = self.registry();
        if !b.unloaded.load(Ordering::Acquire) {
            reg.by_path.remove(&(b.workspace.clone(), old_real.clone()));
            reg.by_path
                .insert((b.workspace.clone(), real.clone()), b.id);
        }
        drop(reg);
        tracing::debug!(buffer = %b.id, from = %old_real.display(), to = %real.display(), "an open file was moved");
    }

    /// What anchors need of an open buffer: its text and revision and, if
    /// `since` is given and the log still has them, the changes after
    /// `since`, taken together under the buffer's lock.
    pub fn anchor_view(
        &self,
        buffer: BufferId,
        since: Option<u64>,
    ) -> Result<AnchorView, ErrorObject> {
        let b = self.get(buffer)?;
        let st = b.lock();
        let rev = st.buf.rev();
        let changes = since.filter(|&s| s <= rev).and_then(|since| {
            (since + 1..=rev)
                .map(|r| st.buf.entry(r).map(|e| e.op.to_edits()))
                .collect::<Option<Vec<_>>>()
        });
        Ok(AnchorView {
            rev,
            text: st.buf.text().to_string(),
            since: since.filter(|_| changes.is_some()),
            changes,
        })
    }

    /// The live changes of an open buffer, until it is unloaded. Unlike a
    /// `buffer/changes` subscription this does not keep the buffer loaded.
    pub fn follow_changes(
        &self,
        buffer: BufferId,
    ) -> Result<coqo_protocol::EventStream<BufferChange>, ErrorObject> {
        let b = self.get(buffer)?;
        let feed = b.log.subscribe(None)?;
        let mut closed = b.closed.subscribe();
        let until = async move {
            let _ = closed.wait_for(|c| *c).await;
        };
        Ok(feed.events.take_until(until).boxed())
    }

    // ---- project search and replace (`search/*`) -------------------------------

    /// The text of every open buffer of `workspace`, unsaved edits included,
    /// by the file's canonical path: what `search/run` searches instead of
    /// the disk.
    pub fn open_texts(&self, workspace: &WorkspaceId) -> HashMap<PathBuf, String> {
        let open: Vec<Arc<OpenBuffer>> = self
            .registry()
            .buffers
            .values()
            .filter(|b| &b.workspace == workspace)
            .cloned()
            .collect();
        open.into_iter()
            .map(|b| {
                let text = b.lock().buf.text().to_string();
                (b.real(), text)
            })
            .collect()
    }

    /// The revision and text of an open buffer, taken together.
    pub fn head(&self, buffer: BufferId) -> Result<(u64, String), ErrorObject> {
        let b = self.get(buffer)?;
        let st = b.lock();
        Ok((st.buf.rev(), st.buf.text().to_string()))
    }

    /// Edit an open buffer at its head with the edits `compute` makes of its
    /// current text (under the buffer's lock, so nothing changes in
    /// between), as one `replace` change. Returns the new revision and the
    /// number of edits, or `None` if `compute` made none.
    pub fn replace_with(
        &self,
        buffer: BufferId,
        compute: impl FnOnce(&str) -> Vec<bufm::TextEdit>,
    ) -> Result<Option<(u64, usize)>, ErrorObject> {
        let b = self.get(buffer)?;
        let mut st = b.lock();
        let edits = compute(&st.buf.text().to_string());
        if edits.is_empty() {
            return Ok(None);
        }
        let c = st
            .buf
            .apply_head(Origin::of(OriginKind::Replace), &edits)
            .map_err(|e| buffer_error(&e))?;
        let rev = c.rev;
        self.commit(&b, &mut st, c, false);
        Ok(Some((rev, edits.len())))
    }

    /// Replace in file `real` (canonical) of `workspace` with the edits
    /// `compute` makes of its text. An open buffer is edited
    /// ([`replace_with`](Self::replace_with)); otherwise the file is read,
    /// edited and written atomically, keeping its encoding (a byte-order
    /// mark) and line endings. No buffer is loaded meanwhile, so none can
    /// miss the write.
    pub async fn replace_in_file<F>(
        &self,
        workspace: &WorkspaceId,
        real: PathBuf,
        compute: F,
    ) -> Result<FileReplace, ErrorObject>
    where
        F: FnOnce(&str) -> Vec<bufm::TextEdit> + Send + 'static,
    {
        let _loading = self.open_lock.lock().await;
        if let Some(id) = self.buffer_at(workspace, &real) {
            return Ok(match self.replace_with(id, compute)? {
                Some((rev, n)) => FileReplace::Buffer {
                    buffer: id,
                    rev,
                    replaced: n,
                },
                None => FileReplace::Unchanged,
            });
        }
        let limit = self.config.max_file_bytes;
        tokio::task::spawn_blocking(move || {
            let bytes = disk::read(&real, limit).map_err(|e| match e {
                disk::ReadError::TooLarge { size } => ErrorObject::new(
                    codes::FILE_TOO_LARGE,
                    format!(
                        "{} is {size} bytes, more than the limit of {limit}",
                        real.display()
                    ),
                ),
                disk::ReadError::Io(e) => io_error(&real, &e),
            })?;
            let decoded = buftext::decode(bytes).map_err(|e| {
                ErrorObject::new(
                    codes::UNSUPPORTED_ENCODING,
                    format!("{}: {e}", real.display()),
                )
            })?;
            let edits = compute(&decoded.text);
            if edits.is_empty() {
                return Ok(FileReplace::Unchanged);
            }
            let mut text = decoded.text;
            for e in edits.iter().rev() {
                if e.start > e.end
                    || e.end > text.len()
                    || !text.is_char_boundary(e.start)
                    || !text.is_char_boundary(e.end)
                {
                    return Err(ErrorObject::new(
                        codes::INVALID_EDIT,
                        format!("{}: a replacement is out of range", real.display()),
                    ));
                }
                text.replace_range(e.start..e.end, &e.text);
            }
            let bytes = buftext::encode(&text, decoded.encoding);
            disk::write_atomic(&real, &bytes).map_err(|e| io_error(&real, &e))?;
            Ok(FileReplace::Written {
                replaced: edits.len(),
            })
        })
        .await
        .map_err(join_error)?
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

/// What [`BufferService::replace_in_file`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileReplace {
    /// The file's open buffer was edited.
    Buffer {
        buffer: BufferId,
        rev: u64,
        replaced: usize,
    },
    /// The file was written.
    Written { replaced: usize },
    /// Nothing to replace.
    Unchanged,
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

async fn reject_edit(peer: &Peer, p: &EditParams, error: ErrorObject) {
    tracing::debug!(buffer = %p.buffer, replica = %p.replica, %error, "rejecting buffer/edit");
    let note = EditRejectedParams {
        buffer: p.buffer,
        replica: p.replica.clone(),
        client_seq: p.client_seq,
        error,
    };
    if let Err(e) = peer.notify::<bufm::EditRejected>(note).await {
        tracing::debug!(error = %e, "cannot send buffer/editRejected");
    }
}

/// An open buffer as [`BufferService::anchor_view`] sees it.
#[derive(Debug)]
pub struct AnchorView {
    /// The current revision.
    pub rev: u64,
    /// The text at `rev`.
    pub text: String,
    /// The changes after `since`, oldest first, each a list of edits in the
    /// coordinates before it; `None` if none was asked for or the log no
    /// longer has them all.
    pub changes: Option<Vec<Vec<bufm::TextEdit>>>,
    /// The revision `changes` start after, when they are known.
    pub since: Option<u64>,
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
/// A session token (`coqod mcp`) may open and follow buffers of its own
/// workspace, and edit and close the replicas it opened there (#198).
/// Saving and reverting a change are the user's review actions and need the
/// daemon token.
pub fn register(router: Router, svc: Arc<BufferService>) -> Router {
    let open = Arc::clone(&svc);
    let edit = Arc::clone(&svc);
    let revert = Arc::clone(&svc);
    let save = Arc::clone(&svc);
    let close = Arc::clone(&svc);
    let changes = Arc::clone(&svc);
    router
        .method_with::<bufm::Open, _, _>(Access::workspace(|p| &p.workspace.0), move |ctx, p| {
            let s = Arc::clone(&open);
            async move { s.open_as(&ctx, p).await }
        })
        .notification_with::<bufm::Edit, _, _>(
            own_buffer(&svc, |p: &EditParams| p.buffer),
            move |ctx, p| {
                let s = Arc::clone(&edit);
                async move { s.edit_as(&ctx, p).await }
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
            move |ctx, p| {
                let s = Arc::clone(&close);
                async move { s.close_as(&ctx, p).await }
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
