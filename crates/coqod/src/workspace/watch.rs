// SPDX-License-Identifier: Apache-2.0

//! The per-workspace file watcher behind `fs/changes`.
//!
//! `notify` watches every root recursively (inotify on Linux, FSEvents on
//! macOS). Its callback only records changed paths in a bounded set and wakes
//! the debounce task, so a burst of events (a branch switch, a build) costs a
//! hash insert each. Every debounce window the task turns the set into one
//! [`FsChanges`] per root: paths made root-relative, deduplicated and sorted,
//! with noise inside `.git` and inside ignored directories dropped. Past
//! [`WatchConfig::max_paths`] the set stops growing and the batch becomes a
//! `rescan`.
//!
//! Batches go to an in-memory [`SeqLog`], so subscribers get the usual
//! sequence numbers, and a slow subscriber gets `RESUME_GAP` instead of
//! making the daemon buffer without bound.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coqo_protocol::methods::fs::FsChanges;
use coqo_protocol::methods::workspace::RootId;
use coqo_protocol::{ErrorObject, EventFeed, SeqLog};
use futures_util::StreamExt;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Notify, watch};

use super::ignores::IgnoreCache;

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
}

#[derive(Debug)]
struct Shared {
    pending: Mutex<Pending>,
    wake: Notify,
    max_paths: usize,
}

impl Shared {
    fn record(&self, res: notify::Result<notify::Event>) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        match res {
            Ok(ev) => {
                if matches!(ev.kind, EventKind::Access(_)) {
                    return;
                }
                if ev.need_rescan() {
                    p.overflow = true;
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
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

type RootList = Arc<Mutex<Vec<(RootId, PathBuf)>>>;

/// Watches one open workspace. Dropping it stops the watcher and ends every
/// `fs/changes` subscription to it.
#[derive(Debug)]
pub(crate) struct WorkspaceWatcher {
    log: Arc<SeqLog<FsChanges>>,
    roots: RootList,
    watcher: Mutex<RecommendedWatcher>,
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
    ) -> notify::Result<Self> {
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            wake: Notify::new(),
            max_paths: config.max_paths,
        });
        let cb = Arc::clone(&shared);
        let mut watcher = notify::recommended_watcher(move |res| cb.record(res))?;
        for (_, path) in &roots {
            watcher.watch(path, RecursiveMode::Recursive)?;
        }
        let roots: RootList = Arc::new(Mutex::new(roots));
        let log = Arc::new(SeqLog::new(config.retained_batches));
        let task = tokio::spawn(debounce(
            shared,
            Arc::clone(&roots),
            ignores,
            Arc::clone(&log),
            config.debounce,
        ));
        Ok(Self {
            log,
            roots,
            watcher: Mutex::new(watcher),
            closed: watch::channel(false).0,
            task,
        })
    }

    /// Replace the watched roots (after `workspace/addRoot` / `removeRoot`).
    pub fn set_roots(&self, new: Vec<(RootId, PathBuf)>) {
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        let mut watcher = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        for (id, path) in roots.iter() {
            if !new.iter().any(|(n, _)| n == id)
                && let Err(e) = watcher.unwatch(path)
            {
                tracing::debug!(path = %path.display(), error = %e, "unwatch failed");
            }
        }
        for (id, path) in &new {
            if !roots.iter().any(|(r, _)| r == id)
                && let Err(e) = watcher.watch(path, RecursiveMode::Recursive)
            {
                tracing::warn!(path = %path.display(), error = %e, "cannot watch new root");
            }
        }
        *roots = new;
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
    ignores: Arc<IgnoreCache>,
    log: Arc<SeqLog<FsChanges>>,
    window: Duration,
) {
    loop {
        shared.wake.notified().await;
        tokio::time::sleep(window).await;
        let pending = shared.take();
        if pending.paths.is_empty() && !pending.overflow {
            continue;
        }
        let roots = roots.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let ignores = Arc::clone(&ignores);
        let batches =
            match tokio::task::spawn_blocking(move || batch(&pending, &roots, &ignores)).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "fs change batching failed");
                    continue;
                }
            };
        for b in batches {
            log.publish(b);
        }
    }
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
        let Some((id, root)) = roots
            .iter()
            .filter(|(_, r)| path.starts_with(r))
            .max_by_key(|(_, r)| r.as_os_str().len())
        else {
            continue;
        };
        let Some(comps) = relative(path, root) else {
            continue;
        };
        if comps.is_empty() || comps.iter().any(|c| c == ".git") {
            continue;
        }
        if ignores.parent_ignored(root, &comps) {
            continue;
        }
        by_root.entry(*id).or_default().insert(comps.join("/"));
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
}
