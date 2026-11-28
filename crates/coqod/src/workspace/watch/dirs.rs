// SPDX-License-Identifier: Apache-2.0

//! Which folders get an OS watch (issue #308, ADR 0027).
//!
//! inotify watches one folder at a time, and `notify`'s recursive mode puts
//! a watch on every folder below a root, ignored ones included: a busy
//! `target/` or `node_modules/` then costs a watch per folder and wakes the
//! daemon for every file a build writes, only for the events to be dropped.
//! On Linux the watcher therefore walks each root itself and puts a
//! non-recursive watch on every folder the ignore rules keep
//! ([`IgnoreCache`], `stands_apart` included). Nothing inside `.git`, an
//! ignored folder, a nested repository or worktree, a `CACHEDIR.TAG` cache
//! or `node_modules` is watched. The ignored folder itself is still an
//! entry of a watched one, so it appearing or going away is seen.
//!
//! The set follows the tree: after every debounce window [`Watches::update`]
//! re-syncs the folders whose entry changed (created, removed, moved in or
//! out) and the folders whose rules may have changed (a `.gitignore` was
//! written, a `CACHEDIR.TAG` or `.git` appeared). A folder that becomes
//! watched has its entries reported as changed, since files written before
//! its watch was in place made no events. After lost events (an overflow)
//! every root is re-synced.
//!
//! On macOS and Windows one recursive stream per root costs nothing per
//! folder, so the roots are watched recursively there and events from
//! ignored folders are dropped when batching, as before.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};

use coqo_protocol::methods::workspace::RootId;
use notify::{ErrorKind, RecursiveMode, Watcher};

use super::{Pending, in_root, relative};
use crate::workspace::ignores::{IgnoreCache, IgnoreChain};

/// Whether this platform's watcher needs a watch per folder (inotify).
pub(super) const PER_DIRECTORY: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// Names whose appearance or change can change which folders are watched.
const RULE_FILES: [&str; 3] = [".gitignore", "CACHEDIR.TAG", ".git"];

/// The OS side of a watch: `notify`'s watcher, or a fake in tests.
pub(super) trait OsWatch {
    fn add(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()>;
    fn remove(&mut self, path: &Path);
}

impl<W: Watcher> OsWatch for W {
    fn add(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        self.watch(path, mode)
    }

    fn remove(&mut self, path: &Path) {
        // Gone already when the folder was removed or moved away.
        if let Err(e) = self.unwatch(path) {
            tracing::trace!(path = %path.display(), error = %e, "unwatch failed");
        }
    }
}

/// The OS watches of one workspace.
#[derive(Debug)]
pub(super) struct Watches<W> {
    os: W,
    per_dir: bool,
    /// Folders with a watch of their own (per-folder mode only).
    dirs: BTreeSet<PathBuf>,
    /// Whether the OS ran out of watches (warned once).
    full: bool,
}

/// Roots as the watcher keeps them: id and canonical folder.
type Roots = [(RootId, PathBuf)];

impl<W: OsWatch> Watches<W> {
    pub fn new(os: W, per_dir: bool) -> Self {
        Self {
            os,
            per_dir,
            dirs: BTreeSet::new(),
            full: false,
        }
    }

    /// Start watching `root`, one of `roots`.
    pub fn add_root(
        &mut self,
        roots: &Roots,
        root: &Path,
        ignores: &IgnoreCache,
    ) -> notify::Result<()> {
        if !self.per_dir {
            return self.os.add(root, RecursiveMode::Recursive);
        }
        self.sync(roots, root, root, ignores, false, &mut Vec::new())
    }

    /// Stop watching `root`, which is no longer one of `roots`.
    pub fn remove_root(&mut self, roots: &Roots, root: &Path, ignores: &IgnoreCache) {
        if !self.per_dir {
            self.os.remove(root);
            return;
        }
        match in_root(root, roots) {
            // Inside another root: that root's rules decide now.
            Some((_, owner, _)) => {
                let owner = owner.to_path_buf();
                let _ = self.sync(roots, &owner, root, ignores, false, &mut Vec::new());
            }
            None => {
                let below: Vec<PathBuf> = self
                    .below(root)
                    .filter(|d| in_root(d, roots).is_none())
                    .cloned()
                    .collect();
                for d in below {
                    self.unwatch(&d);
                }
            }
        }
    }

    /// Follow the tree after a debounce window: re-sync the folders the
    /// events in `pending` may have changed, and add the entries of newly
    /// watched folders to it.
    pub fn update(
        &mut self,
        pending: &mut Pending,
        roots: &Roots,
        ignores: &IgnoreCache,
        max_paths: usize,
    ) {
        if !self.per_dir {
            return;
        }
        let mut targets = BTreeSet::new();
        // Events were lost: check everything, and renew every watch in case
        // a folder was replaced unseen.
        let force = pending.overflow;
        if force {
            targets.extend(roots.iter().map(|(_, r)| r.clone()));
        } else {
            for gone in &pending.gone {
                if self.dirs.contains(gone) {
                    // Its watch went with it (`notify` drops a removed or
                    // moved folder's watches); whatever is there now is new.
                    let below: Vec<PathBuf> = self.below(gone).cloned().collect();
                    for d in below {
                        self.unwatch(&d);
                    }
                    targets.insert(gone.clone());
                    // A root inside it went too, or was replaced.
                    targets.extend(
                        roots
                            .iter()
                            .filter(|(_, r)| r.starts_with(gone))
                            .map(|(_, r)| r.clone()),
                    );
                }
            }
            for path in &pending.paths {
                if let Some(target) = self.target(path, roots) {
                    targets.insert(target);
                }
            }
        }
        let mut found = Vec::new();
        let mut synced: Vec<(&Path, &PathBuf)> = Vec::new();
        for target in &targets {
            let Some((_, root, _)) = in_root(target, roots) else {
                continue;
            };
            // Sorted, so a folder comes after the one above it that covers it.
            if synced
                .iter()
                .any(|(r, t)| *r == root && target.starts_with(t))
            {
                continue;
            }
            synced.push((root, target));
            let root = root.to_path_buf();
            if let Err(e) = self.sync_with(roots, &root, target, ignores, true, force, &mut found) {
                tracing::debug!(path = %target.display(), error = %e, "cannot watch folder");
            }
        }
        if pending.overflow {
            return;
        }
        for path in found {
            if pending.paths.len() >= max_paths {
                pending.overflow = true;
                break;
            }
            pending.paths.insert(path);
        }
    }

    /// The folder to re-sync because `path` changed, if any.
    fn target(&self, path: &Path, roots: &Roots) -> Option<PathBuf> {
        let parent = path.parent()?;
        let name = path.file_name()?;
        if RULE_FILES.iter().any(|r| name == *r) {
            // A root is never ignored, so only its `.gitignore` matters.
            let root = roots.iter().any(|(_, r)| r == parent);
            return (self.dirs.contains(parent) && (name == ".gitignore" || !root))
                .then(|| parent.to_path_buf());
        }
        if self.dirs.contains(path) {
            // A watched folder's own entry changed (removed, replaced, ...).
            return Some(path.to_path_buf());
        }
        (self.dirs.contains(parent) && is_dir(path)).then(|| path.to_path_buf())
    }

    /// Make the watches at and below `dir` (in `root`, the deepest root that
    /// holds it) match the rules. With `report`, the entries of folders that
    /// were not watched before go to `found`.
    fn sync(
        &mut self,
        roots: &Roots,
        root: &Path,
        dir: &Path,
        ignores: &IgnoreCache,
        report: bool,
        found: &mut Vec<PathBuf>,
    ) -> notify::Result<()> {
        self.sync_with(roots, root, dir, ignores, report, false, found)
    }

    #[allow(clippy::too_many_arguments)]
    fn sync_with(
        &mut self,
        roots: &Roots,
        root: &Path,
        dir: &Path,
        ignores: &IgnoreCache,
        report: bool,
        renew: bool,
        found: &mut Vec<PathBuf>,
    ) -> notify::Result<()> {
        let mut keep = HashSet::new();
        let mut result = Ok(());
        if let Some(comps) = relative(dir, root) {
            let chain = ignores.chain(root, &comps);
            if !chain.dir_ignored && is_dir(dir) {
                let walk = Walk {
                    roots,
                    ignores,
                    report,
                    renew,
                };
                result = walk.run(self, dir.to_path_buf(), chain, &mut keep, found);
            }
        }
        // Whatever the rules no longer keep, or what is gone. Folders of a
        // deeper root are that root's business.
        let stale: Vec<PathBuf> = self
            .below(dir)
            .filter(|d| !keep.contains(*d))
            .filter(|d| in_root(d, roots).is_some_and(|(_, r, _)| r == root))
            .cloned()
            .collect();
        for d in stale {
            self.unwatch(&d);
        }
        result
    }

    /// The watched folders at and below `dir`.
    fn below<'a>(&'a self, dir: &'a Path) -> impl Iterator<Item = &'a PathBuf> + 'a {
        // Paths order by component, so a folder's descendants follow it.
        self.dirs
            .range::<Path, _>((Bound::Included(dir), Bound::Unbounded))
            .take_while(move |d| d.starts_with(dir))
    }

    fn unwatch(&mut self, dir: &Path) {
        self.os.remove(dir);
        self.dirs.remove(dir);
    }

    /// Put a watch on `dir`. `Ok(false)`: the OS has no watches left.
    fn watch(&mut self, dir: &Path) -> notify::Result<bool> {
        if self.full {
            return Ok(false);
        }
        match self.os.add(dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.dirs.insert(dir.to_path_buf());
                Ok(true)
            }
            Err(e) if matches!(e.kind, ErrorKind::MaxFilesWatch) => {
                tracing::warn!(
                    "out of inotify watches; changes in some folders won't be seen \
                     (raise fs.inotify.max_user_watches)"
                );
                self.full = true;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// The folders with a watch of their own.
    #[cfg(test)]
    pub fn watched(&self) -> Vec<PathBuf> {
        self.dirs.iter().cloned().collect()
    }
}

/// One walk over the folders the rules keep.
struct Walk<'a> {
    roots: &'a Roots,
    ignores: &'a IgnoreCache,
    report: bool,
    renew: bool,
}

impl Walk<'_> {
    /// Watch `start` and the kept folders below it, noting each in `keep`.
    /// Fails only if `start` itself can't be watched.
    fn run<W: OsWatch>(
        &self,
        watches: &mut Watches<W>,
        start: PathBuf,
        chain: IgnoreChain,
        keep: &mut HashSet<PathBuf>,
        found: &mut Vec<PathBuf>,
    ) -> notify::Result<()> {
        let mut stack = vec![(start.clone(), chain)];
        while let Some((dir, chain)) = stack.pop() {
            let new = !watches.dirs.contains(&dir);
            if new || self.renew {
                // Watch before reading, so nothing written in between is
                // missed.
                match watches.watch(&dir) {
                    Ok(_) => {}
                    Err(e) if dir == start => return Err(e),
                    Err(e) => {
                        // Usually removed meanwhile.
                        tracing::debug!(path = %dir.display(), error = %e, "cannot watch folder");
                        continue;
                    }
                }
            }
            keep.insert(dir.clone());
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if new && self.report {
                    found.push(path.clone());
                }
                // Symbolic links aren't followed: a link into the workspace
                // is watched where it points, and one out of it isn't
                // listed.
                if !entry.file_type().is_ok_and(|t| t.is_dir())
                    || self.roots.iter().any(|(_, r)| *r == path)
                    || chain.is_ignored(&path, true)
                {
                    continue;
                }
                let inner = self.ignores.descend(&chain, &path);
                stack.push((path, inner));
            }
        }
        Ok(())
    }
}

/// Whether `path` is a folder (not a link to one).
fn is_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| m.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the watches instead of asking the OS.
    #[derive(Debug, Default)]
    struct Fake {
        watched: BTreeSet<PathBuf>,
        adds: usize,
    }

    impl OsWatch for Fake {
        fn add(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            assert_eq!(mode, RecursiveMode::NonRecursive);
            if !path.is_dir() {
                return Err(notify::Error::path_not_found());
            }
            self.adds += 1;
            self.watched.insert(path.to_path_buf());
            Ok(())
        }

        fn remove(&mut self, path: &Path) {
            self.watched.remove(path);
        }
    }

    fn write(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "").unwrap();
    }

    fn rel(root: &Path, w: &Watches<Fake>) -> Vec<String> {
        assert_eq!(
            w.os.watched.iter().collect::<Vec<_>>(),
            w.dirs.iter().collect::<Vec<_>>()
        );
        w.dirs
            .iter()
            .map(|d| {
                let r = d.strip_prefix(root).unwrap().to_str().unwrap();
                if r.is_empty() { ".".into() } else { r.into() }
            })
            .collect()
    }

    /// A workspace with every kind of folder that stands apart.
    fn tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".git/HEAD");
        write(root, ".git/objects/ab/cd");
        fs::write(root.join(".gitignore"), "gen/\n").unwrap();
        write(root, "src/lib.rs");
        write(root, "src/deep/er/x.rs");
        write(root, "gen/out.rs");
        write(root, "target/CACHEDIR.TAG");
        write(root, "target/debug/build/x.o");
        write(root, "web/node_modules/left-pad/index.js");
        write(root, ".claude/worktrees/agent/.git");
        write(root, ".claude/worktrees/agent/src/lib.rs");
        write(root, "vendor/dep/.git/HEAD");
        write(root, "vendor/dep/src/x.rs");
        tmp
    }

    fn start(root: &Path) -> (Watches<Fake>, Vec<(RootId, PathBuf)>, IgnoreCache) {
        let roots = vec![(RootId(1), root.to_path_buf())];
        let ignores = IgnoreCache::default();
        let mut w = Watches::new(Fake::default(), true);
        w.add_root(&roots, root, &ignores).unwrap();
        (w, roots, ignores)
    }

    #[test]
    fn watches_only_the_folders_the_rules_keep() {
        let tmp = tree();
        let root = tmp.path();
        let (w, ..) = start(root);
        assert_eq!(
            rel(root, &w),
            [
                ".",
                ".claude",
                ".claude/worktrees",
                "src",
                "src/deep",
                "src/deep/er",
                "vendor",
                "web"
            ]
        );
    }

    fn changed(paths: &[PathBuf]) -> Pending {
        Pending {
            paths: paths.iter().cloned().collect(),
            ..Pending::default()
        }
    }

    #[test]
    fn follows_new_moved_and_newly_ignored_folders() {
        let tmp = tree();
        let root = tmp.path();
        let (mut w, roots, ig) = start(root);

        // A new folder with files written before its watch: they're reported.
        write(root, "src/new/inner/a.rs");
        let mut p = changed(&[root.join("src/new")]);
        w.update(&mut p, &roots, &ig, 100);
        assert!(rel(root, &w).contains(&"src/new/inner".to_owned()));
        assert!(p.paths.contains(&root.join("src/new/inner/a.rs")));
        assert!(p.paths.contains(&root.join("src/new/inner")));

        // Moved: the old folders go, the new ones come.
        fs::rename(root.join("src/new"), root.join("src/moved")).unwrap();
        let mut p = changed(&[root.join("src/new"), root.join("src/moved")]);
        p.gone.insert(root.join("src/new"));
        w.update(&mut p, &roots, &ig, 100);
        let now = rel(root, &w);
        assert!(!now.iter().any(|d| d.starts_with("src/new")), "{now:?}");
        assert!(now.contains(&"src/moved/inner".to_owned()));

        // Cargo makes `target/`, then tags it: its watches go.
        write(root, "build/debug/x.o");
        let mut p = changed(&[root.join("build")]);
        w.update(&mut p, &roots, &ig, 100);
        assert!(rel(root, &w).contains(&"build/debug".to_owned()));
        write(root, "build/CACHEDIR.TAG");
        let mut p = changed(&[root.join("build/CACHEDIR.TAG")]);
        w.update(&mut p, &roots, &ig, 100);
        assert!(!rel(root, &w).iter().any(|d| d.starts_with("build")));

        // `git worktree add` / `git clone` below a root.
        write(root, "src/deep/.git");
        let mut p = changed(&[root.join("src/deep/.git")]);
        w.update(&mut p, &roots, &ig, 100);
        assert!(!rel(root, &w).iter().any(|d| d.starts_with("src/deep")));
        // The root's own `.git` changes nothing.
        let adds = w.os.adds;
        let mut p = changed(&[root.join(".git")]);
        w.update(&mut p, &roots, &ig, 100);
        assert_eq!(w.os.adds, adds);
    }

    #[test]
    fn follows_gitignore_edits_and_overflow() {
        let tmp = tree();
        let root = tmp.path();
        let (mut w, roots, ig) = start(root);
        fs::write(root.join(".gitignore"), "src/deep/\n").unwrap();
        let mut p = changed(&[root.join(".gitignore")]);
        w.update(&mut p, &roots, &ig, 100);
        let now = rel(root, &w);
        assert!(now.contains(&"gen".to_owned()), "{now:?}");
        assert!(!now.iter().any(|d| d.starts_with("src/deep")), "{now:?}");
        assert!(p.paths.contains(&root.join("gen/out.rs")), "newly kept");

        // After lost events every watch is renewed.
        let (adds, count) = (w.os.adds, w.dirs.len());
        let mut p = Pending {
            overflow: true,
            ..Pending::default()
        };
        w.update(&mut p, &roots, &ig, 100);
        assert_eq!(w.os.adds, adds + count);
        assert_eq!(w.dirs.len(), count);
    }

    #[test]
    fn nested_roots_keep_their_own_watches() {
        let tmp = tree();
        let root = tmp.path();
        let (mut w, mut roots, ig) = start(root);
        // `vendor/dep` is a nested clone, ignored in the outer root, but
        // watched as a root of its own.
        let dep = root.join("vendor/dep");
        roots.push((RootId(2), dep.clone()));
        w.add_root(&roots, &dep, &ig).unwrap();
        assert!(rel(root, &w).contains(&"vendor/dep/src".to_owned()));
        // A change in the outer root doesn't drop them.
        let mut p = changed(&[root.join("vendor")]);
        p.gone.insert(root.join("vendor"));
        w.update(&mut p, &roots, &ig, 100);
        assert!(rel(root, &w).contains(&"vendor/dep/src".to_owned()));
        // Removing the inner root: the outer root's rules apply again.
        roots.pop();
        w.remove_root(&roots, &dep, &ig);
        assert!(!rel(root, &w).iter().any(|d| d.starts_with("vendor/dep")));
        // Removing the outer one unwatches everything.
        let outer = roots.pop().unwrap().1;
        w.remove_root(&roots, &outer, &ig);
        assert!(w.dirs.is_empty());
    }
}
