// SPDX-License-Identifier: Apache-2.0

//! The workspace model (PLAN §4.2, issue #22).
//!
//! A workspace is one or more folder roots with a stable id, a name and
//! timestamps, plus a small key/value state document the UI owns (open tabs,
//! layout hints, settings overrides). Workspaces live in the global database
//! (`coqo.db`, global migration 2), so they survive UI and daemon restarts.
//! Every change is appended to the event log in the same transaction and
//! published on `workspace/events`.
//!
//! The service also gives the file tree access inside a workspace's roots
//! (`fs/list`, `fs/stat`, `fs/read`, confined to the roots, with git ignore
//! rules), the tree's file operations (`fs/create`, `fs/rename`,
//! `fs/delete`, registered with the buffers in [`crate::file_ops`]), and
//! debounced change events (`fs/changes`) from a watcher that starts with
//! the first subscriber of an open workspace (or its file index) and stops
//! when the workspace closes.
//!
//! Each open workspace has a file index ([`index`]): built in the background
//! when the workspace opens, kept current from the watcher, and searched by
//! `fs/find` with the fuzzy matcher in [`find`].
//!
//! The method and event types are in [`coqo_protocol::methods`]; this module
//! implements them. Wire it into the daemon's router with [`register`]:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use coqod::workspace::{self, WorkspaceService};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dirs = coqo_storage::DataDirs::platform()?;
//! let workspaces = Arc::new(WorkspaceService::open(dirs)?);
//! let router = workspace::register(coqo_protocol::Router::new(), workspaces);
//! # let _ = router; Ok(()) }
//! ```

mod error;
mod events;
mod find;
mod fs;
mod ignores;
mod index;
mod ops;
mod paths;
mod project;
mod restore;
mod store;
mod watch;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;

use coqo_protocol::methods::fs as fsm;
use coqo_protocol::methods::workspace::{
    self as wsm, AddRootParams, ListResult, OpenParams, RemoveRootParams, RenameParams, RootId,
    SetStateParams, WorkspaceEvent, WorkspaceId, WorkspaceInfo, WorkspaceParams, WorkspaceState,
};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, Scope};
use coqo_storage::{DataDirs, GlobalDb, StorageError};

use self::ignores::IgnoreCache;
use self::index::FileIndex;
pub(crate) use self::ops::Renamed;
pub use self::ops::Trash;
#[cfg(test)]
pub(crate) use self::project::common_dir as project_of;
pub use self::restore::COPY_LIMIT as UNDO_COPY_LIMIT;
use self::restore::Restores;
pub use self::watch::WatchConfig;
use self::watch::WorkspaceWatcher;
use crate::path_moves::PathMove;

/// Longest accepted state key.
const MAX_STATE_KEY: usize = 128;
/// Renames seen by the watchers, buffered for a slow follower.
const RENAMES_SEEN: usize = 256;
/// Largest accepted state value, serialized.
const MAX_STATE_VALUE: usize = 1024 * 1024;
/// How long `fs/find` waits for an index that is still being built before
/// it answers with what there is (and `indexing`).
const FIND_WAIT: Duration = Duration::from_secs(2);

/// A file in a workspace, resolved by [`WorkspaceService::resolve_file`].
#[derive(Clone, Debug)]
pub(crate) struct ResolvedFile {
    /// The canonical path.
    pub real: PathBuf,
    /// `(root, root-relative path)` pairs under which `fs/changes` reports
    /// changes to the file.
    pub watch_keys: Vec<(RootId, String)>,
}

/// The daemon's workspaces. Create one with [`WorkspaceService::open`] and
/// serve it with [`register`].
#[derive(Debug)]
pub struct WorkspaceService {
    db: Mutex<GlobalDb>,
    dirs: DataDirs,
    /// Event-log head after the last workspace change.
    head: tokio::sync::watch::Sender<u64>,
    watchers: Mutex<HashMap<WorkspaceId, Arc<WorkspaceWatcher>>>,
    /// The file index of each open workspace (`fs/find`).
    indexes: Mutex<HashMap<WorkspaceId, Arc<FileIndex>>>,
    ignores: Arc<IgnoreCache>,
    watch_config: WatchConfig,
    trash: Trash,
    /// Renames the watchers saw, for [`crate::path_moves`].
    renames: tokio::sync::broadcast::Sender<PathMove>,
    /// What deleted entries can be restored from (undo).
    restores: Arc<Restores>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // `RandomState` is seeded randomly per process; mixing in time, pid and a
    // counter makes collisions practically impossible (and they are checked).
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("ws-{:016x}", h.finish())
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| path.display().to_string(), str::to_owned)
}

fn path_str(path: &Path) -> &str {
    // Roots are checked to be UTF-8 when added.
    path.to_str().unwrap_or_default()
}

fn clean_name(name: &str) -> Result<String, ErrorObject> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ErrorObject::invalid_params(
            "a workspace name cannot be empty",
        ));
    }
    Ok(name.to_owned())
}

impl WorkspaceService {
    /// Open the service on the daemon's data directories (creating and
    /// migrating `coqo.db` if needed).
    pub fn open(dirs: DataDirs) -> Result<Self, StorageError> {
        Self::open_with(dirs, WatchConfig::default())
    }

    /// Like [`open`](Self::open), with custom watcher tuning.
    pub fn open_with(dirs: DataDirs, watch_config: WatchConfig) -> Result<Self, StorageError> {
        let db = GlobalDb::open(&dirs)?;
        Self::refresh_projects(db.connection());
        let head = db.events().head()?.0;
        Ok(Self {
            db: Mutex::new(db),
            head: tokio::sync::watch::channel(head).0,
            watchers: Mutex::default(),
            indexes: Mutex::default(),
            ignores: Arc::default(),
            watch_config,
            trash: Trash::default(),
            renames: tokio::sync::broadcast::channel(RENAMES_SEEN).0,
            restores: Arc::new(Restores::new(
                dirs.root().join("undo"),
                Restores::copies_by_default(),
            )),
            dirs,
        })
    }

    /// Where `fs/delete` puts deleted entries (the OS trash by default).
    #[must_use]
    pub fn with_trash(mut self, trash: Trash) -> Self {
        self.trash = trash;
        self
    }

    /// Undo deletes from copies Coqo keeps (up to the size limit) instead
    /// of from the trash, as where the OS trash can't restore (the default
    /// on macOS).
    #[must_use]
    pub fn with_undo_copies(mut self, copies: bool) -> Self {
        if let Some(r) = Arc::get_mut(&mut self.restores) {
            r.set_copies(copies);
        }
        self
    }

    /// The renames the file watchers see from now on (in the order seen),
    /// Coqo's own `fs/rename`s included. Open workspaces are watched (their
    /// file index follows the watcher), and so is any workspace someone
    /// follows `fs/changes` of.
    pub fn renames_seen(&self) -> tokio::sync::broadcast::Receiver<PathMove> {
        self.renames.subscribe()
    }

    fn db(&self) -> MutexGuard<'_, GlobalDb> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn watchers(&self) -> MutexGuard<'_, HashMap<WorkspaceId, Arc<WorkspaceWatcher>>> {
        self.watchers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` with the database on a blocking thread.
    async fn blocking<T, F>(self: &Arc<Self>, f: F) -> Result<T, ErrorObject>
    where
        T: Send + 'static,
        F: FnOnce(&WorkspaceService) -> Result<T, ErrorObject> + Send + 'static,
    {
        let svc = Arc::clone(self);
        tokio::task::spawn_blocking(move || f(&svc))
            .await
            .map_err(error::join)?
    }

    /// Apply a change and its events in one transaction, then wake
    /// `workspace/events` subscribers.
    fn change<T>(
        &self,
        f: impl FnOnce(
            &rusqlite::Connection,
        ) -> Result<(T, Vec<(WorkspaceId, WorkspaceEvent)>), ErrorObject>,
    ) -> Result<T, ErrorObject> {
        let mut db = self.db();
        // Immediate: `f` reads before it writes, and a deferred transaction
        // that another connection wrote under fails at once with "database
        // is locked" (SQLite can't wait out an upgrade).
        let tx = db
            .connection_mut()
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(error::storage)?;
        let (out, evs) = f(&tx)?;
        let mut last = None;
        for (id, ev) in &evs {
            last = Some(events::append(&tx, id, ev).map_err(error::storage)?);
        }
        tx.commit().map_err(error::storage)?;
        if let Some(seq) = last {
            // Still under the db lock, so heads are sent in order.
            self.head.send_replace(seq.0);
        }
        Ok(out)
    }

    /// Look again at which project every workspace is in (a folder may
    /// have become a repository, or a worktree, while the daemon was off).
    /// A workspace whose folder is not there keeps what it had.
    fn refresh_projects(conn: &rusqlite::Connection) {
        let roots = match store::first_roots(conn) {
            Ok(roots) => roots,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list workspaces to find their projects");
                return;
            }
        };
        for (id, root) in roots {
            if let Some(project) = project::stored(Path::new(&root))
                && let Err(e) = store::set_project(conn, &id, &project)
            {
                tracing::warn!(workspace = id, error = %e, "cannot store the project");
            }
        }
    }

    /// Look again at which project `id` is in, from its first root.
    fn refresh_project(conn: &rusqlite::Connection, id: &WorkspaceId) -> Result<(), ErrorObject> {
        let info = Self::load(conn, id)?;
        if let Some(project) = info
            .roots
            .first()
            .and_then(|r| project::stored(Path::new(&r.path)))
        {
            store::set_project(conn, &id.0, &project).map_err(error::storage)?;
        }
        Ok(())
    }

    fn load(conn: &rusqlite::Connection, id: &WorkspaceId) -> Result<WorkspaceInfo, ErrorObject> {
        store::get(conn, id)
            .map_err(error::storage)?
            .ok_or_else(|| error::unknown_workspace(id))
    }

    /// The canonical roots of a workspace.
    fn roots_of(&self, id: &WorkspaceId) -> Result<Vec<(RootId, PathBuf)>, ErrorObject> {
        let info = Self::load(self.db().connection(), id)?;
        Ok(info
            .roots
            .into_iter()
            .map(|r| (r.id, PathBuf::from(r.path)))
            .collect())
    }

    fn refresh_watcher(&self, info: &WorkspaceInfo) {
        let watcher = self.watchers().get(&info.id).cloned();
        if let Some(w) = watcher {
            w.set_roots(
                info.roots
                    .iter()
                    .map(|r| (r.id, PathBuf::from(&r.path)))
                    .collect(),
            );
        }
    }

    /// Stop watching and indexing a workspace (it closed, or was removed).
    fn stop_watcher(&self, id: &WorkspaceId) {
        let removed = self.watchers().remove(id);
        drop(removed);
        let index = self.indexes().remove(id);
        if let Some(index) = index {
            index.stop();
        }
    }

    fn indexes(&self) -> MutexGuard<'_, HashMap<WorkspaceId, Arc<FileIndex>>> {
        self.indexes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start indexing `info`'s files in the background, unless an index of
    /// the same roots runs already. Must run inside a Tokio runtime.
    fn start_index(self: &Arc<Self>, info: &WorkspaceInfo) -> Arc<FileIndex> {
        let roots: Vec<(RootId, PathBuf)> = info
            .roots
            .iter()
            .map(|r| (r.id, PathBuf::from(&r.path)))
            .collect();
        let index = {
            let mut indexes = self.indexes();
            if let Some(existing) = indexes.get(&info.id)
                && existing.roots() == roots
            {
                return Arc::clone(existing);
            }
            let index = Arc::new(FileIndex::new(roots));
            if let Some(old) = indexes.insert(info.id.clone(), Arc::clone(&index)) {
                old.stop();
            }
            index
        };
        // The task holds the service only while it subscribes, so it never
        // keeps a dropped service alive.
        let service = Arc::downgrade(self);
        let (id, ignores, built) = (
            info.id.clone(),
            Arc::clone(&self.ignores),
            Arc::clone(&index),
        );
        let task = tokio::spawn(async move {
            loop {
                // Subscribe before walking, so nothing that changes during the
                // walk is missed: the batches wait in the watcher's log.
                let feed = match service.upgrade() {
                    Some(svc) => svc.watcher(&id).await.and_then(|w| w.subscribe(None)),
                    None => return,
                };
                let mut events = match feed {
                    Ok(feed) => Some(feed.events),
                    Err(e) => {
                        tracing::warn!(workspace = %id, error = %e.message, "file index can't follow changes");
                        None
                    }
                };
                let (index, ig) = (Arc::clone(&built), Arc::clone(&ignores));
                if tokio::task::spawn_blocking(move || index.rebuild(&ig))
                    .await
                    .is_err()
                {
                    return;
                }
                let Some(events) = events.as_mut() else {
                    return;
                };
                // Follow the watcher until it stops (the workspace closed) or
                // the index falls behind it (then walk again).
                loop {
                    match events.next().await {
                        None => return,
                        Some(Ok(batch)) => {
                            let (index, ig) = (Arc::clone(&built), Arc::clone(&ignores));
                            let applied = tokio::task::spawn_blocking(move || {
                                index.apply(&batch.event, &ig);
                            });
                            if applied.await.is_err() {
                                return;
                            }
                        }
                        Some(Err(_)) => break,
                    }
                }
            }
        });
        index.set_task(task.abort_handle());
        index
    }

    /// Re-index a workspace whose roots changed, if it is indexed.
    fn refresh_index(self: &Arc<Self>, info: &WorkspaceInfo) {
        let indexed = self.indexes().contains_key(&info.id);
        if indexed {
            self.start_index(info);
        }
    }

    // ---- workspace/* --------------------------------------------------------

    /// `workspace/open`.
    pub async fn open_workspace(
        self: &Arc<Self>,
        p: OpenParams,
    ) -> Result<WorkspaceInfo, ErrorObject> {
        let name = p.name.as_deref().map(clean_name).transpose()?;
        let info = match (p.path, p.id) {
            (Some(path), None) => {
                self.blocking(move |s| {
                    let root = paths::canonical_dir(&path)?;
                    let root_str = path_str(&root).to_owned();
                    s.change(|conn| {
                        let now = now_ms();
                        let id = match store::find_by_primary_path(conn, &root_str)
                            .map_err(error::storage)?
                        {
                            Some(id) => {
                                store::set_open(conn, &id.0, true, Some(now))
                                    .map_err(error::storage)?;
                                id
                            }
                            None => {
                                let mut id = new_id();
                                while store::exists(conn, &id).map_err(error::storage)? {
                                    id = new_id();
                                }
                                let name = name.unwrap_or_else(|| folder_name(&root));
                                store::create(conn, &id, &name, now).map_err(error::storage)?;
                                store::add_root(conn, &id, &root_str, &folder_name(&root))
                                    .map_err(error::storage)?;
                                WorkspaceId(id)
                            }
                        };
                        Self::refresh_project(conn, &id)?;
                        let info = Self::load(conn, &id)?;
                        let ev = WorkspaceEvent::Opened {
                            workspace: info.clone(),
                        };
                        Ok((info, vec![(id, ev)]))
                    })
                })
                .await
            }
            (None, Some(id)) => {
                self.blocking(move |s| {
                    s.change(|conn| {
                        Self::load(conn, &id)?;
                        store::set_open(conn, &id.0, true, Some(now_ms()))
                            .map_err(error::storage)?;
                        if let Some(name) = &name {
                            store::rename(conn, &id.0, name).map_err(error::storage)?;
                        }
                        Self::refresh_project(conn, &id)?;
                        let info = Self::load(conn, &id)?;
                        let ev = WorkspaceEvent::Opened {
                            workspace: info.clone(),
                        };
                        Ok((info, vec![(id, ev)]))
                    })
                })
                .await
            }
            _ => Err(ErrorObject::invalid_params(
                "workspace/open takes exactly one of `path` and `id`",
            )),
        }?;
        // `fs/find` should have the files by the time the user asks.
        self.start_index(&info);
        Ok(info)
    }

    /// `workspace/list`.
    pub async fn list(self: &Arc<Self>) -> Result<ListResult, ErrorObject> {
        self.blocking(|s| {
            let db = s.db();
            let seq = db.events().head().map_err(error::storage)?.0;
            let workspaces = store::list(db.connection()).map_err(error::storage)?;
            Ok(ListResult { workspaces, seq })
        })
        .await
    }

    /// `workspace/get`.
    pub async fn get(self: &Arc<Self>, p: WorkspaceParams) -> Result<WorkspaceInfo, ErrorObject> {
        self.blocking(move |s| Self::load(s.db().connection(), &p.id))
            .await
    }

    /// `workspace/close`.
    pub async fn close(self: &Arc<Self>, p: WorkspaceParams) -> Result<WorkspaceInfo, ErrorObject> {
        let info = self
            .blocking(move |s| {
                s.change(|conn| {
                    let info = Self::load(conn, &p.id)?;
                    if !info.open {
                        return Ok((info, vec![]));
                    }
                    store::set_open(conn, &p.id.0, false, None).map_err(error::storage)?;
                    let info = Self::load(conn, &p.id)?;
                    let ev = WorkspaceEvent::Closed {
                        workspace: info.clone(),
                    };
                    Ok((info, vec![(p.id, ev)]))
                })
            })
            .await?;
        self.stop_watcher(&info.id);
        Ok(info)
    }

    /// `workspace/remove`.
    pub async fn remove(self: &Arc<Self>, p: WorkspaceParams) -> Result<bool, ErrorObject> {
        let id = p.id.clone();
        let existed = self
            .blocking(move |s| {
                s.change(|conn| {
                    if !store::remove(conn, &p.id.0).map_err(error::storage)? {
                        return Ok((false, vec![]));
                    }
                    let ev = WorkspaceEvent::Removed { id: p.id.clone() };
                    Ok((true, vec![(p.id, ev)]))
                })
            })
            .await?;
        self.stop_watcher(&id);
        if existed {
            // Daemon-side data only (annotations, local walkthroughs, …); the
            // workspace's folders are never touched.
            self.blocking(move |s| {
                if let Ok(dir) = s.dirs.workspace_dir(&id.0)
                    && let Err(e) = std::fs::remove_dir_all(&dir)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(dir = %dir.display(), error = %e, "cannot delete workspace data");
                }
                Ok(())
            })
            .await?;
        }
        Ok(existed)
    }

    /// `workspace/addRoot`.
    pub async fn add_root(
        self: &Arc<Self>,
        p: AddRootParams,
    ) -> Result<WorkspaceInfo, ErrorObject> {
        let name = p.name.as_deref().map(clean_name).transpose()?;
        let info = self
            .blocking(move |s| {
                let root = paths::canonical_dir(&p.path)?;
                s.change(|conn| {
                    Self::load(conn, &p.id)?;
                    let name = name.unwrap_or_else(|| folder_name(&root));
                    let (_, added) = store::add_root(conn, &p.id.0, path_str(&root), &name)
                        .map_err(error::storage)?;
                    Self::refresh_project(conn, &p.id)?;
                    let info = Self::load(conn, &p.id)?;
                    let evs = if added {
                        vec![(
                            p.id,
                            WorkspaceEvent::Changed {
                                workspace: info.clone(),
                            },
                        )]
                    } else {
                        vec![]
                    };
                    Ok((info, evs))
                })
            })
            .await?;
        self.refresh_watcher(&info);
        self.refresh_index(&info);
        Ok(info)
    }

    /// `workspace/removeRoot`.
    pub async fn remove_root(
        self: &Arc<Self>,
        p: RemoveRootParams,
    ) -> Result<WorkspaceInfo, ErrorObject> {
        let info = self
            .blocking(move |s| {
                s.change(|conn| {
                    let info = Self::load(conn, &p.id)?;
                    if !info.roots.iter().any(|r| r.id == p.root) {
                        return Err(error::unknown_root(&p.id, p.root));
                    }
                    if info.roots.len() == 1 {
                        return Err(ErrorObject::invalid_params(
                            "cannot remove the last root of a workspace; remove the workspace instead",
                        ));
                    }
                    store::remove_root(conn, &p.id.0, p.root).map_err(error::storage)?;
                    Self::refresh_project(conn, &p.id)?;
                    let info = Self::load(conn, &p.id)?;
                    let ev = WorkspaceEvent::Changed {
                        workspace: info.clone(),
                    };
                    Ok((info, vec![(p.id, ev)]))
                })
            })
            .await?;
        self.refresh_watcher(&info);
        self.refresh_index(&info);
        Ok(info)
    }

    /// `workspace/rename`.
    pub async fn rename(self: &Arc<Self>, p: RenameParams) -> Result<WorkspaceInfo, ErrorObject> {
        let name = clean_name(&p.name)?;
        self.blocking(move |s| {
            s.change(|conn| {
                Self::load(conn, &p.id)?;
                store::rename(conn, &p.id.0, &name).map_err(error::storage)?;
                let info = Self::load(conn, &p.id)?;
                let ev = WorkspaceEvent::Changed {
                    workspace: info.clone(),
                };
                Ok((info, vec![(p.id, ev)]))
            })
        })
        .await
    }

    /// `workspace/getState`.
    pub async fn get_state(
        self: &Arc<Self>,
        p: WorkspaceParams,
    ) -> Result<WorkspaceState, ErrorObject> {
        self.blocking(move |s| {
            let db = s.db();
            Self::load(db.connection(), &p.id)?;
            let seq = db.events().head().map_err(error::storage)?.0;
            let entries = store::state(db.connection(), &p.id.0).map_err(error::storage)?;
            Ok(WorkspaceState { entries, seq })
        })
        .await
    }

    /// `workspace/setState`.
    pub async fn set_state(self: &Arc<Self>, p: SetStateParams) -> Result<(), ErrorObject> {
        let len = p.key.chars().count();
        if len == 0 || len > MAX_STATE_KEY {
            return Err(ErrorObject::invalid_params(format!(
                "state keys are 1–{MAX_STATE_KEY} characters"
            )));
        }
        let value = (!p.value.is_null()).then(|| p.value.to_string());
        if value.as_ref().is_some_and(|v| v.len() > MAX_STATE_VALUE) {
            return Err(ErrorObject::invalid_params(format!(
                "state values are at most {MAX_STATE_VALUE} bytes"
            )));
        }
        self.blocking(move |s| {
            s.change(|conn| {
                Self::load(conn, &p.id)?;
                store::set_state(conn, &p.id.0, &p.key, value.as_deref())
                    .map_err(error::storage)?;
                let ev = WorkspaceEvent::StateChanged {
                    id: p.id.clone(),
                    key: p.key,
                    value: p.value,
                };
                Ok(((), vec![(p.id, ev)]))
            })
        })
        .await
    }

    /// The `workspace/events` topic.
    pub async fn events(
        self: &Arc<Self>,
        p: wsm::EventsParams,
        since: Option<u64>,
    ) -> Result<EventFeed<WorkspaceEvent>, ErrorObject> {
        events::subscribe(Arc::clone(self), p.workspace, since).await
    }

    // ---- fs/* ---------------------------------------------------------------

    /// Run an fs function on a workspace root, on a blocking thread.
    async fn with_root<T, F>(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        root: RootId,
        f: F,
    ) -> Result<T, ErrorObject>
    where
        T: Send + 'static,
        F: FnOnce(&IgnoreCache, &fs::Roots<'_>) -> Result<T, ErrorObject> + Send + 'static,
    {
        self.blocking(move |s| {
            let roots = s.roots_of(&workspace)?;
            let all: Vec<PathBuf> = roots.iter().map(|(_, p)| p.clone()).collect();
            let root = roots
                .iter()
                .find(|(id, _)| *id == root)
                .map(|(_, p)| p.as_path())
                .ok_or_else(|| error::unknown_root(&workspace, root))?;
            f(&s.ignores, &fs::Roots { root, all: &all })
        })
        .await
    }

    /// `fs/list`.
    pub async fn list_dir(
        self: &Arc<Self>,
        p: fsm::PathParams,
    ) -> Result<fsm::ListDirResult, ErrorObject> {
        self.with_root(p.workspace, p.root, move |ig, roots| {
            fs::list(ig, roots, &p.path)
        })
        .await
    }

    /// `fs/stat`.
    pub async fn stat(self: &Arc<Self>, p: fsm::PathParams) -> Result<fsm::FileStat, ErrorObject> {
        self.with_root(p.workspace, p.root, move |ig, roots| {
            fs::stat(ig, roots, &p.path)
        })
        .await
    }

    /// `fs/read`.
    pub async fn read_file(
        self: &Arc<Self>,
        p: fsm::ReadParams,
    ) -> Result<fsm::ReadResult, ErrorObject> {
        self.with_root(p.workspace, p.root, move |_, roots| {
            fs::read(roots, &p.path, p.max_bytes)
        })
        .await
    }

    /// `fs/create`.
    pub async fn create_entry(
        self: &Arc<Self>,
        p: fsm::CreateParams,
    ) -> Result<fsm::EntryResult, ErrorObject> {
        self.with_root(p.workspace, p.root, move |_, roots| {
            ops::create(roots, &p.path, p.kind).map(|path| fsm::EntryResult { path })
        })
        .await
    }

    /// The real path of the entry `rel` names (its parent resolved, the
    /// entry itself not followed), as `fs/rename` and `fs/delete` see it,
    /// and `rel` normalised.
    pub(crate) async fn entry_path(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        root: RootId,
        rel: String,
    ) -> Result<(PathBuf, String), ErrorObject> {
        self.with_root(workspace, root, move |_, roots| {
            ops::entry(roots, &rel).map(|e| {
                let rel = e.rel();
                (e.path, rel)
            })
        })
        .await
    }

    /// `fs/rename`, on disk only: [`crate::file_ops`] moves open buffers.
    pub(crate) async fn rename_entry(
        self: &Arc<Self>,
        p: fsm::RenameParams,
    ) -> Result<Renamed, ErrorObject> {
        self.with_root(p.workspace, p.root, move |_, roots| {
            ops::rename(roots, &p.from, &p.to)
        })
        .await
    }

    /// `fs/delete`.
    pub async fn delete_entry(
        self: &Arc<Self>,
        p: fsm::DeleteParams,
    ) -> Result<fsm::DeleteResult, ErrorObject> {
        let trash = self.trash.clone();
        let restores = Arc::clone(&self.restores);
        self.with_root(p.workspace, p.root, move |_, roots| {
            restores.delete(roots, &p.path, p.permanent, &trash)
        })
        .await
    }

    /// `fs/restore`: bring back what `fs/delete` deleted. Returns the path.
    pub async fn restore_entry(
        self: &Arc<Self>,
        p: fsm::RestoreParams,
    ) -> Result<fsm::EntryResult, ErrorObject> {
        let restores = Arc::clone(&self.restores);
        self.with_root(p.workspace, p.root, move |_, roots| {
            restores
                .restore(roots, &p.path, &p.restore)
                .map(|path| fsm::EntryResult { path })
        })
        .await
    }

    /// The `(root, path)` pairs under which `fs/changes` reports changes to
    /// the real path `real`: one per root it is under.
    pub(crate) async fn watch_keys(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        real: PathBuf,
    ) -> Result<Vec<(RootId, String)>, ErrorObject> {
        self.blocking(move |s| {
            let roots = s.roots_of(&workspace)?;
            Ok(roots
                .iter()
                .filter_map(|(id, path)| {
                    let rest = real.strip_prefix(path).ok()?.to_str()?;
                    (!rest.is_empty()).then(|| (*id, rest.replace(std::path::MAIN_SEPARATOR, "/")))
                })
                .collect())
        })
        .await
    }

    /// Resolve a file for the buffer service: confined to the workspace's
    /// roots like `fs/read`, and with every `(root, path)` the watcher may
    /// report it under (the path as named, and its real path if that is
    /// under a root, e.g. through a symlink).
    pub(crate) async fn resolve_file(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        root: RootId,
        rel: String,
    ) -> Result<ResolvedFile, ErrorObject> {
        self.blocking(move |s| {
            let roots = s.roots_of(&workspace)?;
            let all: Vec<PathBuf> = roots.iter().map(|(_, p)| p.clone()).collect();
            let root_path = roots
                .iter()
                .find(|(id, _)| *id == root)
                .map(|(_, p)| p.as_path())
                .ok_or_else(|| error::unknown_root(&workspace, root))?;
            let (comps, r) = paths::resolve(root_path, &all, &rel)?;
            if !r.meta.is_file() {
                return Err(error::not_a_file(&rel));
            }
            let mut watch_keys = vec![(root, comps.join("/"))];
            for (id, path) in &roots {
                if let Ok(rest) = r.real.strip_prefix(path)
                    && let Some(rest) = rest.to_str()
                    && !rest.is_empty()
                {
                    let key = (*id, rest.replace(std::path::MAIN_SEPARATOR, "/"));
                    if !watch_keys.contains(&key) {
                        watch_keys.push(key);
                    }
                }
            }
            Ok(ResolvedFile {
                real: r.real,
                watch_keys,
            })
        })
        .await
    }

    /// A workspace's roots and their canonical folders, for services that
    /// work on whole roots (git).
    pub(crate) async fn roots(
        self: &Arc<Self>,
        workspace: WorkspaceId,
    ) -> Result<Vec<(RootId, PathBuf)>, ErrorObject> {
        self.blocking(move |s| s.roots_of(&workspace)).await
    }

    /// `fs/find`: the best matches in the workspace's file index (started
    /// if it isn't running, and waited for a little while it is built).
    pub async fn find(
        self: &Arc<Self>,
        p: fsm::FindParams,
    ) -> Result<fsm::FindResult, ErrorObject> {
        let limit = p.limit.unwrap_or(fsm::DEFAULT_FIND_LIMIT);
        if limit > fsm::MAX_FIND_LIMIT {
            return Err(ErrorObject::invalid_params(format!(
                "limit {limit} is above the maximum of {}",
                fsm::MAX_FIND_LIMIT
            )));
        }
        let info = {
            let id = p.workspace.clone();
            self.blocking(move |s| Self::load(s.db().connection(), &id))
                .await?
        };
        if !info.open {
            return Err(error::closed(&info.id));
        }
        let existing = self.indexes().get(&info.id).cloned();
        let index = existing.unwrap_or_else(|| self.start_index(&info));
        let ready = index.is_ready() || index.wait_ready(FIND_WAIT).await;
        let pattern = find::Pattern::new(&p.query);
        tokio::task::spawn_blocking(move || {
            let snapshot = index.snapshot();
            let files = snapshot.iter().map(|(_, f)| f.len() as u64).sum();
            let (matches, total) = find::find(&snapshot, &pattern, limit as usize);
            fsm::FindResult {
                matches,
                total,
                files,
                indexing: !ready,
            }
        })
        .await
        .map_err(error::join)
    }

    /// The `fs/changes` topic. Starts the workspace's watcher if needed.
    pub async fn fs_changes(
        self: &Arc<Self>,
        p: fsm::ChangesParams,
        since: Option<u64>,
    ) -> Result<EventFeed<fsm::FsChanges>, ErrorObject> {
        self.watcher(&p.workspace).await?.subscribe(since)
    }

    /// An open workspace's watcher, started if needed.
    async fn watcher(
        self: &Arc<Self>,
        id: &WorkspaceId,
    ) -> Result<Arc<WorkspaceWatcher>, ErrorObject> {
        let id = id.clone();
        let info = {
            let id = id.clone();
            self.blocking(move |s| Self::load(s.db().connection(), &id))
                .await?
        };
        if !info.open {
            return Err(error::closed(&id));
        }
        let existing = self.watchers().get(&id).cloned();
        let watcher = match existing {
            Some(w) => w,
            None => {
                let roots: Vec<_> = info
                    .roots
                    .iter()
                    .map(|r| (r.id, PathBuf::from(&r.path)))
                    .collect();
                let ignores = Arc::clone(&self.ignores);
                let config = self.watch_config.clone();
                let renames = (id.clone(), self.renames.clone());
                // Setting up the watches walks the tree: keep it off the
                // async workers. The runtime handle lets the watcher spawn its
                // debounce task from the blocking thread.
                let rt = tokio::runtime::Handle::current();
                let started = tokio::task::spawn_blocking(move || {
                    let _guard = rt.enter();
                    WorkspaceWatcher::start(roots, ignores, &config, renames)
                })
                .await
                .map_err(error::join)?
                .map_err(|e| {
                    ErrorObject::new(
                        coqo_protocol::methods::codes::IO_ERROR,
                        format!("cannot watch workspace {id}: {e}"),
                    )
                })?;
                let started = Arc::new(started);
                let mut watchers = self.watchers();
                Arc::clone(watchers.entry(id.clone()).or_insert(started))
            }
        };
        // The workspace may have been closed while the watcher started.
        let still_open = {
            let id = id.clone();
            self.blocking(move |s| Ok(Self::load(s.db().connection(), &id)?.open))
                .await?
        };
        if !still_open {
            self.stop_watcher(&id);
            return Err(error::closed(&id));
        }
        Ok(watcher)
    }
}

macro_rules! route {
    ($router:expr, $svc:expr, $method:ty, $access:expr, |$s:ident, $p:ident| $body:expr) => {{
        let svc = Arc::clone(&$svc);
        $router.method_with::<$method, _, _>($access, move |_ctx, $p| {
            let $s = Arc::clone(&svc);
            async move { $body.await }
        })
    }};
}

/// Register every `workspace/*` and `fs/*` method and topic on `router`.
///
/// Who may call each is declared here and checked by the router before the
/// handler runs (ADR 0007, *Socket security*). A session token (what
/// `coqod mcp` gets) may read its own workspace: `workspace/get`,
/// `workspace/getState`, `fs/*` and `workspace/events` filtered to it.
/// Opening, listing and managing workspaces (adding a root widens file
/// access) and writing the UI's state need the daemon token. The file
/// operations (`fs/create`, `fs/rename`, `fs/delete`) are registered by
/// [`crate::file_ops::register`], also for the daemon token only.
pub fn register(router: Router, svc: Arc<WorkspaceService>) -> Router {
    let router = route!(router, svc, wsm::Open, Access::daemon(), |s, p| s
        .open_workspace(p));
    let router = route!(router, svc, wsm::List, Access::daemon(), |s, _p| s.list());
    let router = route!(router, svc, wsm::Close, Access::daemon(), |s, p| s.close(p));
    let router = route!(router, svc, wsm::Remove, Access::daemon(), |s, p| s
        .remove(p));
    let router = route!(router, svc, wsm::AddRoot, Access::daemon(), |s, p| s
        .add_root(p));
    let router = route!(router, svc, wsm::RemoveRoot, Access::daemon(), |s, p| s
        .remove_root(p));
    let router = route!(router, svc, wsm::Rename, Access::daemon(), |s, p| s
        .rename(p));
    let router = route!(router, svc, wsm::SetState, Access::daemon(), |s, p| s
        .set_state(p));
    let router = route!(
        router,
        svc,
        wsm::Get,
        Access::workspace(|p| &p.id.0),
        |s, p| s.get(p)
    );
    let router = route!(
        router,
        svc,
        wsm::GetState,
        Access::workspace(|p| &p.id.0),
        |s, p| s.get_state(p)
    );
    let router = route!(
        router,
        svc,
        fsm::ListDir,
        Access::workspace(|p| &p.workspace.0),
        |s, p| s.list_dir(p)
    );
    let router = route!(
        router,
        svc,
        fsm::Stat,
        Access::workspace(|p| &p.workspace.0),
        |s, p| s.stat(p)
    );
    let router = route!(
        router,
        svc,
        fsm::ReadFile,
        Access::workspace(|p| &p.workspace.0),
        |s, p| s.read_file(p)
    );
    // Agents have their own ways to look for files; the finder is the user's.
    let router = route!(router, svc, fsm::Find, Access::daemon(), |s, p| s.find(p));
    let events = Arc::clone(&svc);
    // Without a workspace filter the feed covers every workspace.
    let one_workspace = Access::check(|scope: &Scope, p: &wsm::EventsParams| match &p.workspace {
        Some(id) => scope.require_workspace(&id.0),
        None => scope.require_daemon("workspace/events without a workspace"),
    });
    let router = router.topic_with::<wsm::Events, _, _>(one_workspace, move |_ctx, p, since| {
        let s = Arc::clone(&events);
        async move { s.events(p, since).await }
    });
    router.topic_with::<fsm::Changes, _, _>(
        Access::workspace(|p| &p.workspace.0),
        move |_ctx, p, since| {
            let s = Arc::clone(&svc);
            async move { s.fs_changes(p, since).await }
        },
    )
}
