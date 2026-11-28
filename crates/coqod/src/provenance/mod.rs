// SPDX-License-Identifier: Apache-2.0

//! Which agent session changed a file (issue #286, `agents/changedBy`).
//!
//! The daemon keeps, per workspace, the last session that changed each file
//! (root-relative path), with when and how sure it is
//! ([`ChangeStrength`]), in the global database (`file_changes`). The
//! review's send popover defaults to that session and the diff header names
//! it ("made by Claude in \"Volume discount\""), even after the chat is
//! closed or the daemon restarts.
//!
//! - **`write`: the session wrote the file.** Two sources, both from the
//!   agent itself:
//!   - a **completed tool call** whose `diff` content names the file, or an
//!     `edit` / `delete` / `move` tool call whose locations do (the session
//!     manager hands every recorded update to [`Signals::observer`]). A
//!     call that fails or is rejected records nothing;
//!   - an ACP **`fs/write_text_file`** that succeeded, into an open buffer
//!     or to disk (the ACP host's [`AgentFs::wrote`], through
//!     [`crate::agents::BufferFs`]).
//! - **`turn`: it changed during the session's turn.** An agent may edit
//!   through a tool of its own that reports nothing (a shell command such as
//!   `sed -i`). While a session's turn runs, the workspace's `fs/changes`
//!   are followed, and a file that changes then, or up to [`LINGER`] after
//!   the turn ended, is recorded as the session's, but only when that
//!   session is the only one in a turn in the workspace, and not when the
//!   user just saved it (`buffer/save`). It is weaker: the user's own edits
//!   outside Coqo during the turn count too. It never replaces the same
//!   session's `write` record.
//! - **Last change wins.** A file has one record: the latest session's.
//! - **Renames.** Records follow a file or folder that is renamed or moved
//!   ([`crate::path_moves`], #288).
//!
//! `agents/changedBy {workspace, root, paths}` (daemon token only) returns
//! the sessions that last changed any of `paths`, the most recent change
//! first, with each one's title, provider and whether it was closed.
//! `agents/changedSince {since, workspace?}` counts the files whose last
//! change is at or after `since`, in all and by session: the welcome back's
//! files changed while the UI was away (#265).
//!
//! [`AgentFs::wrote`]: coqo_acp_host::AgentFs::wrote

mod store;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use coqo_agents::{SessionManager, UpdateObserver};
use coqo_protocol::methods::agents::{
    self as agm, ChangeStrength, ChangedByParams, ChangedByResult, ChangedSinceParams,
    ChangedSinceResult, ListSessionsParams, SessionChange, SessionFiles, SessionId, SessionInfo,
    SessionUpdate, ToolCallContent, ToolCallId, ToolCallStatus, ToolKind,
};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{ChangesParams, FsChanges};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{Access, ErrorObject, Router};
use coqo_storage::Timestamp;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

pub use self::store::StoreError;
use self::store::{Row, Store};
use crate::buffers::BufferService;
use crate::path_moves::{FollowsMoves, PathMove};
use crate::workspace::WorkspaceService;

/// How long after a turn ends a change on disk still counts as the turn's:
/// the watcher reports changes after a short debounce, and the agent's last
/// write can land just before the turn ends.
pub const LINGER: Duration = Duration::from_secs(2);

/// How long after the user's `buffer/save` the watcher's report of it is
/// not the agent's.
const SAVE_WINDOW: Duration = Duration::from_secs(3);

/// Most paths one `agents/changedBy` may ask about.
const MAX_PATHS: usize = 10_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn storage_error(e: &StoreError) -> ErrorObject {
    ErrorObject::new(codes::STORAGE_ERROR, format!("provenance store: {e}"))
}

/// What reaches the service's task.
#[derive(Debug)]
enum Signal {
    /// A session recorded an update.
    Update {
        session: SessionId,
        workspace: Option<WorkspaceId>,
        update: Box<SessionUpdate>,
    },
    /// A session wrote `path` (absolute) with `fs/write_text_file`.
    Wrote { session: SessionId, path: PathBuf },
    /// The watcher saw files of `workspace` change.
    Fs {
        workspace: WorkspaceId,
        changes: FsChanges,
    },
    /// Everything sent before this was handled.
    Settled(oneshot::Sender<()>),
}

/// Where the session manager and the ACP host report what agents do, made
/// before the [`Provenance`] service exists (the manager is built first).
/// Cheap to clone.
#[derive(Clone, Debug)]
pub struct Signals {
    tx: mpsc::UnboundedSender<Signal>,
}

/// The receiving end of [`Signals`], for [`Provenance::open`].
#[derive(Debug)]
pub struct SignalReceiver(mpsc::UnboundedReceiver<Signal>);

/// A [`Signals`] and its receiver.
pub fn channel() -> (Signals, SignalReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Signals { tx }, SignalReceiver(rx))
}

impl Signals {
    /// The session manager's [`UpdateObserver`]: hands on turns starting
    /// and ending and tool calls.
    pub fn observer(&self) -> UpdateObserver {
        let tx = self.tx.clone();
        Arc::new(move |info: &SessionInfo, update: &SessionUpdate| {
            let relevant = matches!(
                update,
                SessionUpdate::TurnStarted { .. }
                    | SessionUpdate::TurnEnded { .. }
                    | SessionUpdate::ToolCall(_)
                    | SessionUpdate::ToolCallUpdate(_)
            );
            if relevant {
                let _ = tx.send(Signal::Update {
                    session: info.id.clone(),
                    workspace: info.workspace.clone().map(WorkspaceId),
                    update: Box::new(update.clone()),
                });
            }
        })
    }

    /// `session` wrote `path` with ACP `fs/write_text_file`.
    pub fn wrote(&self, session: &SessionId, path: &Path) {
        let _ = self.tx.send(Signal::Wrote {
            session: session.clone(),
            path: path.to_path_buf(),
        });
    }
}

/// A session in (or just out of) a turn.
#[derive(Debug)]
struct Turn {
    workspace: WorkspaceId,
    /// When it ended; `None` while it runs.
    ended: Option<Instant>,
}

impl Turn {
    fn active(&self) -> bool {
        self.ended.is_none_or(|at| at.elapsed() < LINGER)
    }
}

/// A tool call of a running turn, as reported so far.
#[derive(Debug, Default)]
struct Call {
    kind: ToolKind,
    /// Paths of its `diff` content.
    diffs: Vec<String>,
    /// Its locations' paths.
    locations: Vec<String>,
    /// It completed (and was recorded).
    done: bool,
}

impl Call {
    /// The files a completed call changed.
    fn changed(&self) -> Vec<String> {
        let mut paths = self.diffs.clone();
        if matches!(
            self.kind,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move
        ) {
            paths.extend(self.locations.iter().cloned());
        }
        paths.sort();
        paths.dedup();
        paths
    }
}

fn diff_paths(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Diff { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// What the task keeps between signals.
#[derive(Debug, Default)]
struct Live {
    turns: HashMap<SessionId, Turn>,
    calls: HashMap<(SessionId, ToolCallId), Call>,
}

/// `path` resolved through symlinks as far as it exists (a deleted or new
/// file keeps its name under its resolved folder).
fn real(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => real(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// `path` (absolute) as `(root, root-relative path)` of the first of
/// `roots` (resolved) it is in.
fn locate(roots: &[(RootId, PathBuf)], path: &Path) -> Option<(RootId, String)> {
    let path = real(path);
    roots.iter().find_map(|(id, root)| {
        let rest = path.strip_prefix(root).ok()?;
        let parts: Vec<&str> = rest
            .components()
            .map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect::<Option<_>>()?;
        (!parts.is_empty()).then(|| (*id, parts.join("/")))
    })
}

/// The daemon's record of who changed which file. See the module docs.
#[derive(Debug)]
pub struct Provenance {
    store: Mutex<Store>,
    workspaces: Arc<WorkspaceService>,
    buffers: Option<Arc<BufferService>>,
    sessions: SessionManager,
    tx: mpsc::UnboundedSender<Signal>,
    /// The `fs/changes` followers, per workspace.
    followers: Mutex<HashMap<WorkspaceId, tokio::task::JoinHandle<()>>>,
}

impl Provenance {
    /// The service over the global database at `db`, taking what `rx`'s
    /// [`Signals`] report. `buffers` tells the user's saves apart. Must be
    /// called inside a Tokio runtime.
    pub fn open(
        db: &Path,
        workspaces: Arc<WorkspaceService>,
        buffers: Option<Arc<BufferService>>,
        sessions: SessionManager,
        signals: &Signals,
        rx: SignalReceiver,
    ) -> Result<Arc<Self>, StoreError> {
        let svc = Arc::new(Self {
            store: Mutex::new(Store::open(db)?),
            workspaces,
            buffers,
            sessions,
            tx: signals.tx.clone(),
            followers: Mutex::default(),
        });
        let weak = Arc::downgrade(&svc);
        tokio::spawn(run(weak, rx.0));
        Ok(svc)
    }

    /// Waits until everything reported so far is recorded (for tests).
    pub async fn settled(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Signal::Settled(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// `agents/changedBy`.
    pub async fn changed_by(&self, p: ChangedByParams) -> Result<ChangedByResult, ErrorObject> {
        if p.paths.len() > MAX_PATHS {
            return Err(ErrorObject::invalid_params(format!(
                "at most {MAX_PATHS} paths at once"
            )));
        }
        let wanted: std::collections::HashSet<&str> = p.paths.iter().map(String::as_str).collect();
        let rows = lock(&self.store)
            .rows(&p.workspace, p.root)
            .map_err(|e| storage_error(&e))?;
        let mut by_session: HashMap<SessionId, Vec<Row>> = HashMap::new();
        for row in rows
            .into_iter()
            .filter(|r| wanted.contains(r.path.as_str()))
        {
            by_session.entry(row.session.clone()).or_default().push(row);
        }
        if by_session.is_empty() {
            return Ok(ChangedByResult::default());
        }
        let listed = self
            .sessions
            .list(&ListSessionsParams {
                workspace: Some(p.workspace.0.clone()),
                include_closed: true,
            })
            .map_err(|e| e.to_error())?;
        let infos: HashMap<SessionId, SessionInfo> = listed
            .sessions
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
        let mut sessions: Vec<SessionChange> = by_session
            .into_iter()
            .filter_map(|(id, rows)| {
                let info = infos
                    .get(&id)
                    .cloned()
                    .or_else(|| self.sessions.snapshot(&id).ok().map(|s| s.info))?;
                let mut paths: Vec<String> = rows.iter().map(|r| r.path.clone()).collect();
                paths.sort();
                Some(SessionChange {
                    session: id,
                    provider: info.provider,
                    title: info.title,
                    closed: info.closed,
                    paths,
                    at: rows.iter().map(|r| r.at).max().unwrap_or(0),
                    strength: rows
                        .iter()
                        .map(|r| r.strength)
                        .max()
                        .unwrap_or(ChangeStrength::Turn),
                })
            })
            .collect();
        sessions.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.session.0.cmp(&b.session.0)));
        Ok(ChangedByResult { sessions })
    }

    /// `agents/changedSince`.
    pub fn changed_since(&self, p: &ChangedSinceParams) -> Result<ChangedSinceResult, ErrorObject> {
        let counts = lock(&self.store)
            .counts_since(p.since, p.workspace.as_ref())
            .map_err(|e| storage_error(&e))?;
        Ok(ChangedSinceResult {
            files: counts.iter().map(|(_, n)| *n).fold(0, u32::saturating_add),
            sessions: counts
                .into_iter()
                .map(|(session, files)| SessionFiles { session, files })
                .collect(),
        })
    }

    // ---- recording ------------------------------------------------------------

    /// `workspace`'s roots, resolved.
    async fn roots(&self, workspace: &WorkspaceId) -> Vec<(RootId, PathBuf)> {
        match self.workspaces.roots(workspace.clone()).await {
            Ok(roots) => roots.into_iter().map(|(id, p)| (id, real(&p))).collect(),
            Err(e) => {
                tracing::debug!(%workspace, error = %e.message, "no roots to place a change in");
                Vec::new()
            }
        }
    }

    fn record(
        &self,
        workspace: &WorkspaceId,
        root: RootId,
        path: String,
        session: &SessionId,
        strength: ChangeStrength,
    ) {
        let row = Row {
            path,
            session: session.clone(),
            strength,
            at: Timestamp::now().0,
        };
        if let Err(e) = lock(&self.store).record(workspace, root, &row) {
            tracing::warn!(%workspace, path = %row.path, error = %e, "cannot record who changed a file");
        }
    }

    /// `session` wrote `paths` (absolute) of `workspace`.
    async fn wrote(&self, workspace: &WorkspaceId, session: &SessionId, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        let roots = self.roots(workspace).await;
        for path in paths {
            match locate(&roots, Path::new(path)) {
                Some((root, rel)) => {
                    tracing::debug!(%workspace, session = %session.0, path = %rel, "an agent wrote a file");
                    self.record(workspace, root, rel, session, ChangeStrength::Write);
                }
                None => {
                    tracing::debug!(%workspace, path, "an agent wrote a file outside the workspace");
                }
            }
        }
    }

    /// The workspace of `session`: its turn's, else its info's.
    fn workspace_of(&self, live: &Live, session: &SessionId) -> Option<WorkspaceId> {
        if let Some(t) = live.turns.get(session) {
            return Some(t.workspace.clone());
        }
        let info = self.sessions.snapshot(session).ok()?.info;
        info.workspace.map(WorkspaceId)
    }

    async fn handle(self: &Arc<Self>, live: &mut Live, signal: Signal) {
        match signal {
            Signal::Update {
                session,
                workspace,
                update,
            } => {
                let Some(workspace) = workspace else { return };
                self.on_update(live, session, workspace, *update).await;
            }
            Signal::Wrote { session, path } => {
                if let Some(workspace) = self.workspace_of(live, &session) {
                    let path = path.to_string_lossy().into_owned();
                    self.wrote(&workspace, &session, &[path]).await;
                }
            }
            Signal::Fs { workspace, changes } => self.on_fs(live, &workspace, &changes).await,
            Signal::Settled(done) => {
                let _ = done.send(());
            }
        }
    }

    async fn on_update(
        self: &Arc<Self>,
        live: &mut Live,
        session: SessionId,
        workspace: WorkspaceId,
        update: SessionUpdate,
    ) {
        let done = match update {
            SessionUpdate::TurnStarted { .. } => {
                self.follow(&workspace);
                live.turns.insert(
                    session,
                    Turn {
                        workspace,
                        ended: None,
                    },
                );
                return;
            }
            SessionUpdate::TurnEnded { .. } => {
                if let Some(t) = live.turns.get_mut(&session) {
                    t.ended = Some(Instant::now());
                }
                live.calls.retain(|(s, _), _| *s != session);
                return;
            }
            SessionUpdate::ToolCall(tc) => {
                let call = Call {
                    kind: tc.kind,
                    diffs: diff_paths(&tc.content),
                    locations: tc.locations.into_iter().map(|l| l.path).collect(),
                    done: false,
                };
                let key = (session.clone(), tc.id);
                live.calls.insert(key.clone(), call);
                (tc.status == ToolCallStatus::Completed).then_some(key)
            }
            SessionUpdate::ToolCallUpdate(u) => {
                let key = (session.clone(), u.id);
                let call = live.calls.entry(key.clone()).or_default();
                if let Some(kind) = u.kind {
                    call.kind = kind;
                }
                if let Some(content) = &u.content {
                    call.diffs = diff_paths(content);
                }
                if let Some(locations) = u.locations {
                    call.locations = locations.into_iter().map(|l| l.path).collect();
                }
                (u.status == Some(ToolCallStatus::Completed)).then_some(key)
            }
            _ => None,
        };
        let Some(key) = done else { return };
        let Some(call) = live.calls.get_mut(&key).filter(|c| !c.done) else {
            return;
        };
        call.done = true;
        let paths = call.changed();
        self.wrote(&workspace, &session, &paths).await;
    }

    /// Files of `workspace` changed on disk.
    async fn on_fs(&self, live: &mut Live, workspace: &WorkspaceId, changes: &FsChanges) {
        live.turns
            .retain(|_, t| t.ended.is_none_or(|at| at.elapsed() < LINGER * 4));
        if changes.rescan || changes.paths.is_empty() {
            return;
        }
        let mut active = live
            .turns
            .iter()
            .filter(|(_, t)| &t.workspace == workspace && t.active())
            .map(|(s, _)| s.clone());
        let (Some(session), None) = (active.next(), active.next()) else {
            return; // no turn, or several: nobody to credit
        };
        let roots = self.roots(workspace).await;
        let Some((_, root_path)) = roots.iter().find(|(id, _)| *id == changes.root) else {
            return;
        };
        for rel in &changes.paths {
            let path = root_path.join(rel);
            if path.is_dir() {
                continue;
            }
            let saved = self
                .buffers
                .as_ref()
                .is_some_and(|b| b.saved_recently(&real(&path), SAVE_WINDOW));
            if saved {
                continue;
            }
            self.record(
                workspace,
                changes.root,
                rel.clone(),
                &session,
                ChangeStrength::Turn,
            );
        }
    }

    /// Follow `fs/changes` of `workspace` (once), for changes made during
    /// turns.
    fn follow(self: &Arc<Self>, workspace: &WorkspaceId) {
        let mut followers = lock(&self.followers);
        if followers.get(workspace).is_some_and(|t| !t.is_finished()) {
            return;
        }
        let weak = Arc::downgrade(self);
        let ws = workspace.clone();
        let task = tokio::spawn(async move {
            loop {
                let Some(svc) = weak.upgrade() else { return };
                let params = ChangesParams {
                    workspace: ws.clone(),
                };
                let feed = match svc.workspaces.fs_changes(params, None).await {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::debug!(workspace = %ws, error = %e.message, "provenance cannot follow fs/changes");
                        return;
                    }
                };
                let tx = svc.tx.clone();
                drop(svc);
                let mut events = feed.events;
                let mut lagged = false;
                while let Some(item) = events.next().await {
                    match item {
                        Ok(batch) => {
                            let signal = Signal::Fs {
                                workspace: ws.clone(),
                                changes: batch.event,
                            };
                            if tx.send(signal).is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            lagged = true;
                            break;
                        }
                    }
                }
                if !lagged {
                    return; // the workspace was closed
                }
            }
        });
        followers.insert(workspace.clone(), task);
    }
}

/// The service's task: handles signals in order.
async fn run(svc: Weak<Provenance>, mut rx: mpsc::UnboundedReceiver<Signal>) {
    let mut live = Live::default();
    while let Some(signal) = rx.recv().await {
        let Some(s) = svc.upgrade() else { return };
        s.handle(&mut live, signal).await;
    }
}

impl FollowsMoves for Provenance {
    fn path_moved(self: Arc<Self>, mv: PathMove) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            match lock(&self.store).rekey(&mv) {
                Ok(0) => {}
                Ok(n) => {
                    tracing::debug!(workspace = %mv.workspace, from = %mv.from, to = %mv.to, moved = n, "change records follow a rename");
                }
                Err(e) => {
                    tracing::warn!(workspace = %mv.workspace, error = %e, "cannot move change records");
                }
            }
        })
    }
}

/// Registers `agents/changedBy` and `agents/changedSince` (daemon token
/// only: they are the user's view of who changed what).
pub fn register(router: Router, svc: Arc<Provenance>) -> Router {
    let since = Arc::clone(&svc);
    router
        .method_with::<agm::ChangedBy, _, _>(Access::daemon(), move |_ctx, p| {
            let svc = Arc::clone(&svc);
            async move { svc.changed_by(p).await }
        })
        .method_with::<agm::ChangedSince, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(since.changed_since(&p))
        })
}
