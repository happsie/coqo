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
//! The service also gives the file tree read-only access inside a
//! workspace's roots (`fs/list`, `fs/stat`, `fs/read`, confined to the roots,
//! with git ignore rules) and debounced change events (`fs/changes`) from a
//! watcher that starts with the first subscriber of an open workspace and
//! stops when the workspace closes.
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
mod fs;
mod ignores;
mod paths;
mod store;
mod watch;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use coqo_protocol::methods::fs as fsm;
use coqo_protocol::methods::workspace::{
    self as wsm, AddRootParams, ListResult, OpenParams, RemoveRootParams, RenameParams, RootId,
    SetStateParams, WorkspaceEvent, WorkspaceId, WorkspaceInfo, WorkspaceParams, WorkspaceState,
};
use coqo_protocol::{ErrorObject, EventFeed, Method, Router, Scope, Topic};
use coqo_storage::{DataDirs, GlobalDb, StorageError};

use self::ignores::IgnoreCache;
pub use self::watch::WatchConfig;
use self::watch::WorkspaceWatcher;

/// Longest accepted state key.
const MAX_STATE_KEY: usize = 128;
/// Largest accepted state value, serialized.
const MAX_STATE_VALUE: usize = 1024 * 1024;

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
    ignores: Arc<IgnoreCache>,
    watch_config: WatchConfig,
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
        let head = db.events().head()?.0;
        Ok(Self {
            db: Mutex::new(db),
            dirs,
            head: tokio::sync::watch::channel(head).0,
            watchers: Mutex::default(),
            ignores: Arc::default(),
            watch_config,
        })
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
        let tx = db.connection_mut().transaction().map_err(error::storage)?;
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

    fn stop_watcher(&self, id: &WorkspaceId) {
        let removed = self.watchers().remove(id);
        drop(removed);
    }

    // ---- workspace/* --------------------------------------------------------

    /// `workspace/open`.
    pub async fn open_workspace(
        self: &Arc<Self>,
        p: OpenParams,
    ) -> Result<WorkspaceInfo, ErrorObject> {
        let name = p.name.as_deref().map(clean_name).transpose()?;
        match (p.path, p.id) {
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
        }
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
                    let info = Self::load(conn, &p.id)?;
                    let ev = WorkspaceEvent::Changed {
                        workspace: info.clone(),
                    };
                    Ok((info, vec![(p.id, ev)]))
                })
            })
            .await?;
        self.refresh_watcher(&info);
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

    /// The `fs/changes` topic. Starts the workspace's watcher if needed.
    pub async fn fs_changes(
        self: &Arc<Self>,
        p: fsm::ChangesParams,
        since: Option<u64>,
    ) -> Result<EventFeed<fsm::FsChanges>, ErrorObject> {
        let id = p.workspace;
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
                // Setting up recursive watches walks the tree: keep it off the
                // async workers. The runtime handle lets the watcher spawn its
                // debounce task from the blocking thread.
                let rt = tokio::runtime::Handle::current();
                let started = tokio::task::spawn_blocking(move || {
                    let _guard = rt.enter();
                    WorkspaceWatcher::start(roots, ignores, &config)
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
        watcher.subscribe(since)
    }
}

/// Who may call a route, checked against the connection's token scope
/// before the handler runs (ADR 0007, *Socket security*).
///
/// A session token (what `coqod mcp` gets) may read and use its own
/// workspace. Opening, listing and managing workspaces (adding roots gives
/// access to more of the disk) needs the daemon token.
fn daemon_only<P>(name: &'static str) -> impl Fn(&Scope, &P) -> Result<(), ErrorObject> {
    move |scope, _| scope.require_daemon(name)
}

fn in_workspace(scope: &Scope, id: &WorkspaceId) -> Result<(), ErrorObject> {
    scope.require_workspace(&id.0)
}

macro_rules! route {
    ($router:expr, $svc:expr, $method:ty, $guard:expr, |$s:ident, $p:ident| $body:expr) => {{
        let svc = Arc::clone(&$svc);
        let guard = $guard;
        $router.method::<$method, _, _>(move |ctx, $p| {
            let $s = Arc::clone(&svc);
            let allowed = guard(&ctx.scope, &$p);
            async move {
                allowed?;
                $body.await
            }
        })
    }};
}

/// Register every `workspace/*` and `fs/*` method and topic on `router`.
pub fn register(router: Router, svc: Arc<WorkspaceService>) -> Router {
    let mine = |scope: &Scope, p: &wsm::WorkspaceParams| in_workspace(scope, &p.id);
    let router = route!(
        router,
        svc,
        wsm::Open,
        daemon_only(wsm::Open::NAME),
        |s, p| s.open_workspace(p)
    );
    let router = route!(
        router,
        svc,
        wsm::List,
        daemon_only(wsm::List::NAME),
        |s, _p| s.list()
    );
    let router = route!(router, svc, wsm::Get, mine, |s, p| s.get(p));
    let router = route!(
        router,
        svc,
        wsm::Close,
        daemon_only(wsm::Close::NAME),
        |s, p| s.close(p)
    );
    let router = route!(
        router,
        svc,
        wsm::Remove,
        daemon_only(wsm::Remove::NAME),
        |s, p| s.remove(p)
    );
    let router = route!(
        router,
        svc,
        wsm::AddRoot,
        daemon_only(wsm::AddRoot::NAME),
        |s, p| s.add_root(p)
    );
    let router = route!(
        router,
        svc,
        wsm::RemoveRoot,
        daemon_only(wsm::RemoveRoot::NAME),
        |s, p| s.remove_root(p)
    );
    let router = route!(
        router,
        svc,
        wsm::Rename,
        daemon_only(wsm::Rename::NAME),
        |s, p| s.rename(p)
    );
    let router = route!(router, svc, wsm::GetState, mine, |s, p| s.get_state(p));
    let router = route!(
        router,
        svc,
        wsm::SetState,
        |scope: &Scope, p: &wsm::SetStateParams| in_workspace(scope, &p.id),
        |s, p| s.set_state(p)
    );
    let on_path = |scope: &Scope, p: &fsm::PathParams| in_workspace(scope, &p.workspace);
    let router = route!(router, svc, fsm::ListDir, on_path, |s, p| s.list_dir(p));
    let router = route!(router, svc, fsm::Stat, on_path, |s, p| s.stat(p));
    let router = route!(
        router,
        svc,
        fsm::ReadFile,
        |scope: &Scope, p: &fsm::ReadParams| in_workspace(scope, &p.workspace),
        |s, p| s.read_file(p)
    );
    let events = Arc::clone(&svc);
    let router = router.topic::<wsm::Events, _, _>(move |ctx, p, since| {
        let s = Arc::clone(&events);
        // Without a workspace filter the feed covers every workspace.
        let allowed = match &p.workspace {
            Some(id) => in_workspace(&ctx.scope, id),
            None => ctx.scope.require_daemon(wsm::Events::NAME),
        };
        async move {
            allowed?;
            s.events(p, since).await
        }
    });
    router.topic::<fsm::Changes, _, _>(move |ctx, p, since| {
        let s = Arc::clone(&svc);
        let allowed = in_workspace(&ctx.scope, &p.workspace);
        async move {
            allowed?;
            s.fs_changes(p, since).await
        }
    })
}
