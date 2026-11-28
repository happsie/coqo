// SPDX-License-Identifier: Apache-2.0

//! The per-workspace file watcher behind `fs/changes`.
//!
//! `notify` watches the roots: on Linux with one inotify watch per folder
//! the ignore rules keep, so nothing inside an ignored folder is watched
//! ([`dirs`], issue #308); on macOS and Windows with one recursive stream
//! per root. Its callback only records changed paths in a bounded set and
//! wakes the debounce task, so a burst of events (a branch switch, a build)
//! costs a hash insert each. Every debounce window the task turns the set into one
//! [`FsChanges`] per root: paths made root-relative, deduplicated and sorted,
//! with noise inside `.git` and inside ignored directories dropped. Past
//! [`WatchConfig::max_paths`] the set stops growing and the batch becomes a
//! `rescan`. Before batching, the task brings the per-folder watches up to
//! date with what changed.
//!
//! Batches go to an in-memory [`SeqLog`], so subscribers get the usual
//! sequence numbers, and a slow subscriber gets `RESUME_GAP` instead of
//! making the daemon buffer without bound.
//!
//! Renames the platform reports as one (inotify pairs a rename's two
//! halves into a `Both` event; a `From` directly followed by its `To` is
//! paired here) are also sent, in order and root-relative, as
//! [`PathMove`]s, so anchored items follow renames made outside Coqo
//! (issue #288). macOS's FSEvents reports each half on its own, unpaired:
//! those renames are not detected, and the items stay findable on the old
//! path as orphaned.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod dirs;

use coqo_protocol::methods::fs::FsChanges;
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{ErrorObject, EventFeed, SeqLog};
use futures_util::StreamExt;
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher};
use tokio::sync::{Notify, broadcast, watch};

use self::dirs::Watches;
use super::ignores::IgnoreCache;
use crate::path_moves::PathMove;

/// Renames collected per window at most (more are dropped).
const MAX_RENAMES: usize = 1024;

/// Where renames the watcher sees go: the workspace and the sender.
pub(crate) type RenameSink = (WorkspaceId, broadcast::Sender<PathMove>);

/// Watcher tuning.
#[derive(Clone, Debug)]
pub struct WatchConfig {
    /// How long to collect changes before publishing a batch.
    pub debounce: Duration,
    /// Changed paths collected per window before falling back to `rescan`.
    pub max_paths: usize,
    /// Batches retained for subscribers that are catching up.
    pub retained_batches: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(100),
            max_paths: 10_000,
            retained_batches: 1024,
        }
    }
}

#[derive(Debug, Default)]
struct Pending {
    paths: HashSet<PathBuf>,
    overflow: bool,
    /// Renames seen, `(from, to)`, in order.
    renames: Vec<(PathBuf, PathBuf)>,
    /// The `From` half of a rename, waiting for its `To`.
    rename_from: Option<PathBuf>,
    /// Paths removed or moved away: a watched folder among them lost its
    /// watches.
    gone: HashSet<PathBuf>,
}

impl Pending {
    fn renamed(&mut self, from: PathBuf, to: PathBuf) {
        // inotify reports a rename three times: `From`, `To`, then `Both`.
        if self
            .renames
            .last()
            .is_some_and(|(f, t)| *f == from && *t == to)
        {
            return;
        }
        if self.renames.len() < MAX_RENAMES {
            self.renames.push((from, to));
        }
    }

    /// Note the rename halves of `ev`.
    fn rename_event(&mut self, ev: &notify::Event) {
        let EventKind::Modify(ModifyKind::Name(mode)) = ev.kind else {
            self.rename_from = None;
            return;
        };
        match (mode, ev.paths.as_slice()) {
            (RenameMode::Both, [from, to]) => {
                self.rename_from = None;
                self.renamed(from.clone(), to.clone());
            }
            (RenameMode::From, [from]) => self.rename_from = Some(from.clone()),
            (RenameMode::To, [to]) => {
                if let Some(from) = self.rename_from.take() {
                    self.renamed(from, to.clone());
                }
            }
            _ => self.rename_from = None,
        }
    }
}

#[derive(Debug)]
struct Shared {
    pending: Mutex<Pending>,
    wake: Notify,
    max_paths: usize,
    /// Events recorded (access events left out), for tests.
    events: AtomicU64,
}

impl Shared {
    fn record(&self, res: notify::Result<notify::Event>) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        match res {
            Ok(ev) => {
                if matches!(ev.kind, EventKind::Access(_)) {
                    return;
                }
                self.events.fetch_add(1, Ordering::Relaxed);
                if ev.need_rescan() {
                    p.overflow = true;
                }
                p.rename_event(&ev);
                let gone = match ev.kind {
                    EventKind::Remove(_) => ev.paths.len(),
                    EventKind::Modify(ModifyKind::Name(
                        RenameMode::From | RenameMode::Both | RenameMode::Any,
                    )) => 1,
                    _ => 0,
                };
                for path in ev.paths.iter().take(gone) {
                    if p.gone.len() >= self.max_paths {
                        p.overflow = true;
                        break;
                    }
                    p.gone.insert(path.clone());
                }
                for path in ev.paths {
                    if p.paths.len() >= self.max_paths {
                        p.overflow = true;
                        break;
                    }
                    p.paths.insert(path);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "file watcher error; asking clients to rescan");
                p.overflow = true;
            }
        }
        drop(p);
        self.wake.notify_one();
    }

    fn take(&self) -> Pending {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let rename_from = p.rename_from.take();
        let mut taken = std::mem::take(&mut *p);
        // A rename's `To` may come in the next window.
        p.rename_from = rename_from;
        taken.rename_from = None;
        taken
    }
}

type RootList = Arc<Mutex<Vec<(RootId, PathBuf)>>>;
type OsWatches = Arc<Mutex<Watches<RecommendedWatcher>>>;

/// Watches one open workspace. Dropping it stops the watcher and ends every
/// `fs/changes` subscription to it.
#[derive(Debug)]
pub(crate) struct WorkspaceWatcher {
    log: Arc<SeqLog<FsChanges>>,
    roots: RootList,
    ignores: Arc<IgnoreCache>,
    watches: OsWatches,
    #[cfg(test)]
    shared: Arc<Shared>,
    closed: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WorkspaceWatcher {
    fn drop(&mut self) {
        self.closed.send_replace(true);
        self.task.abort();
    }
}

impl WorkspaceWatcher {
    /// Start watching `roots` (canonical). Must run inside a Tokio runtime.
    pub fn start(
        roots: Vec<(RootId, PathBuf)>,
        ignores: Arc<IgnoreCache>,
        config: &WatchConfig,
        renames: RenameSink,
    ) -> notify::Result<Self> {
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            wake: Notify::new(),
            max_paths: config.max_paths,
            events: AtomicU64::new(0),
        });
        let cb = Arc::clone(&shared);
        let watcher = notify::recommended_watcher(move |res| cb.record(res))?;
        let mut watches = Watches::new(watcher, dirs::PER_DIRECTORY);
        for (_, path) in &roots {
            watches.add_root(&roots, path, &ignores)?;
        }
        let watches = Arc::new(Mutex::new(watches));
        let roots: RootList = Arc::new(Mutex::new(roots));
        let log = Arc::new(SeqLog::new(config.retained_batches));
        let task = tokio::spawn(debounce(
            Arc::clone(&shared),
            Arc::clone(&roots),
            Arc::clone(&watches),
            Arc::clone(&ignores),
            Arc::clone(&log),
            config.debounce,
            renames,
        ));
        Ok(Self {
            log,
            roots,
            ignores,
            watches,
            #[cfg(test)]
            shared,
            closed: watch::channel(false).0,
            task,
        })
    }

    /// Replace the watched roots (after `workspace/addRoot` / `removeRoot`).
    /// Walks the new roots' folders: call it on a blocking thread.
    pub fn set_roots(&self, new: Vec<(RootId, PathBuf)>) {
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        let mut watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
        for (id, path) in roots.iter() {
            if !new.iter().any(|(n, _)| n == id) {
                watches.remove_root(&new, path, &self.ignores);
            }
        }
        for (id, path) in &new {
            if !roots.iter().any(|(r, _)| r == id)
                && let Err(e) = watches.add_root(&new, path, &self.ignores)
            {
                tracing::warn!(path = %path.display(), error = %e, "cannot watch new root");
            }
        }
        *roots = new;
    }

    /// The folders with a watch of their own (on Linux).
    #[cfg(test)]
    pub fn watched(&self) -> Vec<PathBuf> {
        let watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
        watches.watched()
    }

    /// The events the OS reported so far (access events left out).
    #[cfg(test)]
    pub fn events_seen(&self) -> u64 {
        self.shared.events.load(Ordering::Relaxed)
    }

    /// Subscribe to the change batches; the feed ends when the watcher is
    /// dropped (the workspace was closed or removed).
    pub fn subscribe(&self, since: Option<u64>) -> Result<EventFeed<FsChanges>, ErrorObject> {
        let feed = self.log.subscribe(since)?;
        let mut closed = self.closed.subscribe();
        let until = async move {
            let _ = closed.wait_for(|c| *c).await;
        };
        Ok(EventFeed {
            head: feed.head,
            events: feed.events.take_until(until).boxed(),
        })
    }
}

async fn debounce(
    shared: Arc<Shared>,
    roots: RootList,
    watches: OsWatches,
    ignores: Arc<IgnoreCache>,
    log: Arc<SeqLog<FsChanges>>,
    window: Duration,
    (workspace, renames): RenameSink,
) {
    loop {
        shared.wake.notified().await;
        tokio::time::sleep(window).await;
        let mut pending = shared.take();
        if pending.paths.is_empty() && !pending.overflow && pending.renames.is_empty() {
            continue;
        }
        let (roots, ignores, watches) = (
            Arc::clone(&roots),
            Arc::clone(&ignores),
            Arc::clone(&watches),
        );
        let max_paths = shared.max_paths;
        let batched = tokio::task::spawn_blocking(move || {
            // In `set_roots`' order, so the roots can't change meanwhile.
            let roots = {
                let roots = roots.lock().unwrap_or_else(|e| e.into_inner());
                let mut watches = watches.lock().unwrap_or_else(|e| e.into_inner());
                watches.update(&mut pending, &roots, &ignores, max_paths);
                roots.clone()
            };
            let moved = moves(&pending.renames, &roots);
            (batch(&pending, &roots, &ignores), moved)
        })
        .await;
        let (batches, moved) = match batched {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "fs change batching failed");
                continue;
            }
        };
        for (root, from, to) in moved {
            let _ = renames.send(PathMove {
                workspace: workspace.clone(),
                root,
                from,
                to,
            });
        }
        for b in batches {
            log.publish(b);
        }
    }
}

/// The deepest root that holds `path`, and `path` relative to it.
fn in_root<'a>(
    path: &Path,
    roots: &'a [(RootId, PathBuf)],
) -> Option<(RootId, &'a Path, Vec<String>)> {
    let (id, root) = roots
        .iter()
        .filter(|(_, r)| path.starts_with(r))
        .max_by_key(|(_, r)| r.as_os_str().len())?;
    let comps = relative(path, root)?;
    Some((*id, root.as_path(), comps))
}

/// `renames` as root-relative moves within one root, outside `.git`.
fn moves(
    renames: &[(PathBuf, PathBuf)],
    roots: &[(RootId, PathBuf)],
) -> Vec<(RootId, String, String)> {
    renames
        .iter()
        .filter_map(|(from, to)| {
            let (root, _, from) = in_root(from, roots)?;
            let (to_root, _, to) = in_root(to, roots)?;
            let git = |c: &[String]| c.iter().any(|c| c == ".git");
            (root == to_root && !from.is_empty() && !to.is_empty() && !git(&from) && !git(&to))
                .then(|| (root, from.join("/"), to.join("/")))
        })
        .collect()
}

/// Root-relative `/`-separated components, if `path` is under `root`.
fn relative(path: &Path, root: &Path) -> Option<Vec<String>> {
    let rest = path.strip_prefix(root).ok()?;
    rest.components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

fn batch(pending: &Pending, roots: &[(RootId, PathBuf)], ignores: &IgnoreCache) -> Vec<FsChanges> {
    if pending.overflow {
        return roots
            .iter()
            .map(|(id, _)| FsChanges {
                root: *id,
                paths: Vec::new(),
                rescan: true,
            })
            .collect();
    }
    let mut by_root: BTreeMap<RootId, BTreeSet<String>> = BTreeMap::new();
    for path in &pending.paths {
        // Nested roots: the deepest root that contains the path wins.
        let Some((id, root, comps)) = in_root(path, roots) else {
            continue;
        };
        if comps.is_empty() || comps.iter().any(|c| c == ".git") {
            continue;
        }
        if ignores.parent_ignored(root, &comps) {
            continue;
        }
        by_root.entry(id).or_default().insert(comps.join("/"));
    }
    by_root
        .into_iter()
        .map(|(root, paths)| FsChanges {
            root,
            paths: paths.into_iter().collect(),
            rescan: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_by_root_and_drops_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join(".gitignore"), "target/\n").unwrap();
        let roots = vec![(RootId(1), a.clone()), (RootId(2), b.clone())];
        let pending = Pending {
            paths: [
                a.join("src/main.rs"),
                a.join("src/main.rs"),
                a.join("target"),
                a.join("target/debug/x.o"),
                a.join(".git/index"),
                b.join("README.md"),
                tmp.path().join("elsewhere"),
            ]
            .into_iter()
            .collect(),
            overflow: false,
            ..Pending::default()
        };
        let out = batch(&pending, &roots, &IgnoreCache::default());
        assert_eq!(
            out,
            vec![
                FsChanges {
                    root: RootId(1),
                    paths: vec!["src/main.rs".into(), "target".into()],
                    rescan: false,
                },
                FsChanges {
                    root: RootId(2),
                    paths: vec!["README.md".into()],
                    rescan: false,
                },
            ]
        );
    }

    #[test]
    fn overflow_means_rescan_of_every_root() {
        let roots = vec![
            (RootId(1), PathBuf::from("/a")),
            (RootId(3), PathBuf::from("/b")),
        ];
        let pending = Pending {
            paths: HashSet::new(),
            overflow: true,
            ..Pending::default()
        };
        let out = batch(&pending, &roots, &IgnoreCache::default());
        assert!(out.iter().all(|c| c.rescan && c.paths.is_empty()));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn record_caps_pending_paths() {
        let shared = Shared {
            pending: Mutex::default(),
            wake: Notify::new(),
            max_paths: 3,
            events: AtomicU64::new(0),
        };
        let mut ev = notify::Event::new(EventKind::Create(notify::event::CreateKind::File));
        for i in 0..5 {
            ev = ev.add_path(PathBuf::from(format!("/r/{i}")));
        }
        shared.record(Ok(ev));
        let p = shared.take();
        assert_eq!(p.paths.len(), 3);
        assert!(p.overflow);
        assert!(!shared.take().overflow, "take resets");
    }

    fn rename(mode: RenameMode, paths: &[&str]) -> notify::Result<notify::Event> {
        let mut ev = notify::Event::new(EventKind::Modify(ModifyKind::Name(mode)));
        for p in paths {
            ev = ev.add_path(PathBuf::from(p));
        }
        Ok(ev)
    }

    #[test]
    fn renames_are_paired_once_and_made_root_relative() {
        let shared = Shared {
            pending: Mutex::default(),
            wake: Notify::new(),
            max_paths: 100,
            events: AtomicU64::new(0),
        };
        // inotify: both halves, then the pair.
        shared.record(rename(RenameMode::From, &["/r/a.rs"]));
        shared.record(rename(RenameMode::To, &["/r/b.rs"]));
        shared.record(rename(RenameMode::Both, &["/r/a.rs", "/r/b.rs"]));
        // A `From` whose `To` comes in the next window.
        shared.record(rename(RenameMode::From, &["/r/src"]));
        let p = shared.take();
        assert_eq!(p.renames, [("/r/a.rs".into(), "/r/b.rs".into())]);
        shared.record(rename(RenameMode::To, &["/r/lib"]));
        // Unpaired halves (FSEvents) are not renames.
        shared.record(rename(RenameMode::Any, &["/r/x"]));
        shared.record(rename(RenameMode::To, &["/r/y"]));
        let q = shared.take();
        assert_eq!(q.renames, [("/r/src".into(), "/r/lib".into())]);
        let roots = vec![
            (RootId(1), PathBuf::from("/r")),
            (RootId(2), PathBuf::from("/s")),
        ];
        let renames = vec![
            (PathBuf::from("/r/src"), PathBuf::from("/r/lib")),
            (PathBuf::from("/r/a"), PathBuf::from("/s/a")),
            (PathBuf::from("/r/.git/x"), PathBuf::from("/r/.git/y")),
            (PathBuf::from("/elsewhere/a"), PathBuf::from("/r/a")),
        ];
        assert_eq!(
            moves(&renames, &roots),
            [(RootId(1), "src".to_owned(), "lib".to_owned())],
            "within one root, outside the repository folder"
        );
    }
}
