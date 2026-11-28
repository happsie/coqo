// SPDX-License-Identifier: Apache-2.0

//! The per-workspace file index behind `fs/find` (issue #162).
//!
//! It holds the root-relative paths of every file in a workspace's roots
//! that the ignore rules keep, by the same rules the file tree shows
//! (`.gitignore`, `.git/info/exclude`, the global excludes, never `.git`;
//! [`IgnoreCache`]). Symlinks to files are files; symlinked folders are not
//! followed.
//!
//! The service starts one when a workspace opens. It walks the roots on
//! blocking threads (in parallel), then follows the workspace's watcher:
//! each `fs/changes` batch is re-checked on disk. A file that appeared is
//! added, one that went is dropped, a folder that changed is walked again,
//! and so is the folder of a `.gitignore` that changed. A `rescan` batch,
//! or falling behind the watcher, walks the root again.
//!
//! Queries match against a snapshot: an immutable list per root, rebuilt
//! on the first query after a change, so matching never holds the lock.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use coqo_protocol::methods::fs::FsChanges;
use coqo_protocol::methods::workspace::RootId;

use super::find::RootSnapshot;
use super::ignores::{IgnoreCache, IgnoreChain};

/// At most this many threads walk one root.
const MAX_WALK_THREADS: usize = 8;

#[derive(Debug)]
struct RootFiles {
    id: RootId,
    path: PathBuf,
    files: HashSet<Arc<str>>,
    /// The files as a list, until they change.
    snapshot: Option<Arc<[Arc<str>]>>,
}

/// One workspace's files. See the module docs.
#[derive(Debug)]
pub(crate) struct FileIndex {
    roots: Mutex<Vec<RootFiles>>,
    /// Whether every root has been walked once.
    ready: tokio::sync::watch::Sender<bool>,
    /// The task that builds and updates it.
    task: Mutex<Option<tokio::task::AbortHandle>>,
}

impl Drop for FileIndex {
    fn drop(&mut self) {
        self.stop();
    }
}

impl FileIndex {
    /// An empty index of `roots` (canonical), not ready yet.
    pub fn new(roots: Vec<(RootId, PathBuf)>) -> Self {
        Self {
            roots: Mutex::new(
                roots
                    .into_iter()
                    .map(|(id, path)| RootFiles {
                        id,
                        path,
                        files: HashSet::new(),
                        snapshot: None,
                    })
                    .collect(),
            ),
            ready: tokio::sync::watch::channel(false).0,
            task: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<RootFiles>> {
        self.roots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The roots it indexes.
    pub fn roots(&self) -> Vec<(RootId, PathBuf)> {
        self.lock().iter().map(|r| (r.id, r.path.clone())).collect()
    }

    /// Keep `task` as the one maintaining it, stopped with [`Self::stop`].
    pub fn set_task(&self, task: tokio::task::AbortHandle) {
        let old = self
            .task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(task);
        if let Some(old) = old {
            old.abort();
        }
    }

    /// Stop maintaining it.
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            task.abort();
        }
    }

    /// Whether every root has been walked once.
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// Wait until [`Self::is_ready`], at most `timeout`. Returns whether it
    /// is.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        let mut rx = self.ready.subscribe();
        tokio::time::timeout(timeout, rx.wait_for(|r| *r))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// How many files it holds.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().iter().map(|r| r.files.len()).sum()
    }

    /// The files of each root, as they are now.
    pub fn snapshot(&self) -> Vec<RootSnapshot> {
        self.lock()
            .iter_mut()
            .map(|r| {
                let files = r
                    .snapshot
                    .get_or_insert_with(|| r.files.iter().cloned().collect());
                (r.id, Arc::clone(files))
            })
            .collect()
    }

    /// Walk every root again and mark the index ready. Blocking.
    pub fn rebuild(&self, ignores: &IgnoreCache) {
        for (id, path) in self.roots() {
            let files = walk(&path, "", ignores);
            self.replace(id, files);
        }
        self.ready.send_replace(true);
    }

    fn replace(&self, root: RootId, files: Vec<String>) {
        let mut roots = self.lock();
        if let Some(r) = roots.iter_mut().find(|r| r.id == root) {
            r.files = files.into_iter().map(Arc::from).collect();
            r.snapshot = None;
        }
    }

    /// Bring a root up to date with one watcher batch. Blocking: it checks
    /// the reported paths on disk.
    pub fn apply(&self, changes: &FsChanges, ignores: &IgnoreCache) {
        let Some(root) = self
            .lock()
            .iter()
            .find(|r| r.id == changes.root)
            .map(|r| r.path.clone())
        else {
            return;
        };
        if changes.rescan {
            self.replace(changes.root, walk(&root, "", ignores));
            return;
        }
        // Paths that are no files (any more), folders whose contents are
        // replaced, files to add, and folders to walk.
        let mut remove: HashSet<&str> = HashSet::new();
        let mut purge: HashSet<String> = HashSet::new();
        let mut add: Vec<String> = Vec::new();
        let mut rewalk: BTreeSet<String> = BTreeSet::new();
        for path in &changes.paths {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            if name == ".gitignore" {
                rewalk.insert(parent.to_owned());
            }
            let comps: Vec<&str> = path.split('/').collect();
            let abs = join(&root, &comps);
            match fs::symlink_metadata(&abs) {
                Err(_) => {
                    remove.insert(path);
                    purge.insert(path.clone());
                }
                Ok(meta) if meta.is_dir() => {
                    remove.insert(path);
                    rewalk.insert(path.clone());
                }
                Ok(meta) => {
                    let file = meta.is_file()
                        || (meta.file_type().is_symlink()
                            && fs::metadata(&abs).is_ok_and(|m| m.is_file()));
                    if file && !ignores.is_ignored(&root, &comps, false) {
                        add.push(path.clone());
                    } else {
                        remove.insert(path);
                    }
                }
            }
        }
        if rewalk.contains("") {
            self.replace(changes.root, walk(&root, "", ignores));
            return;
        }
        // Folders inside another folder that is walked anyway.
        let nested: Vec<String> = rewalk
            .iter()
            .filter(|d| ancestors(d).any(|a| rewalk.contains(a)))
            .cloned()
            .collect();
        for dir in nested {
            rewalk.remove(&dir);
        }
        for dir in rewalk {
            add.extend(walk(&root, &dir, ignores));
            purge.insert(dir);
        }
        let mut roots = self.lock();
        let Some(r) = roots.iter_mut().find(|r| r.id == changes.root) else {
            return;
        };
        if purge.is_empty() {
            for path in remove {
                r.files.remove(path);
            }
        } else {
            r.files
                .retain(|f| !remove.contains(&**f) && !ancestors(f).any(|a| purge.contains(a)));
        }
        r.files.extend(add.into_iter().map(Arc::from));
        r.snapshot = None;
    }
}

fn join(root: &Path, comps: &[&str]) -> PathBuf {
    let mut p = root.to_path_buf();
    p.extend(comps.iter().filter(|c| !c.is_empty()));
    p
}

/// The folders `path` is in, innermost first (`a/b/c` → `a/b`, `a`).
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.rmatch_indices('/').map(move |(i, _)| &path[..i])
}

/// A folder still to be read.
struct Job {
    abs: PathBuf,
    rel: String,
    chain: IgnoreChain,
}

/// Every kept file under folder `start` (root-relative, `""` for the root)
/// of `root`, root-relative. Reads folders on a few threads.
pub(crate) fn walk(root: &Path, start: &str, ignores: &IgnoreCache) -> Vec<String> {
    let comps: Vec<&str> = start.split('/').filter(|c| !c.is_empty()).collect();
    let chain = ignores.chain(root, &comps);
    if chain.dir_ignored {
        return Vec::new();
    }
    let first = Job {
        abs: join(root, &comps),
        rel: comps.join("/"),
        chain,
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_WALK_THREADS);
    // The folders waiting, and how many are being read.
    let queue = Mutex::new((vec![first], 0usize));
    let wake = Condvar::new();
    let worker = || {
        let mut files = Vec::new();
        loop {
            let job = {
                let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(job) = q.0.pop() {
                        q.1 += 1;
                        break Some(job);
                    }
                    if q.1 == 0 {
                        break None;
                    }
                    q = wake.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            let Some(job) = job else {
                wake.notify_all();
                return files;
            };
            let dirs = read_dir(&job, ignores, &mut files);
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            q.0.extend(dirs);
            q.1 -= 1;
            drop(q);
            wake.notify_all();
        }
    };
    if threads <= 1 {
        return worker();
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(worker)).collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Read one folder: its kept files go to `files`, its kept folders come
/// back to be read.
fn read_dir(job: &Job, ignores: &IgnoreCache, files: &mut Vec<String>) -> Vec<Job> {
    let Ok(read) = fs::read_dir(&job.abs) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in read.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue; // not addressable in the protocol
        };
        if name == ".git" {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let abs = job.abs.join(&name);
        let rel = if job.rel.is_empty() {
            name
        } else {
            format!("{}/{name}", job.rel)
        };
        if file_type.is_dir() {
            if !job.chain.is_ignored(&abs, true) {
                let chain = ignores.descend(&job.chain, &abs);
                dirs.push(Job { abs, rel, chain });
            }
        } else if (file_type.is_file()
            || (file_type.is_symlink() && fs::metadata(&abs).is_ok_and(|m| m.is_file())))
            && !job.chain.is_ignored(&abs, false)
        {
            files.push(rel);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, rel).unwrap();
    }

    fn sorted(index: &FileIndex) -> Vec<String> {
        let mut files: Vec<String> = index.snapshot()[0]
            .1
            .iter()
            .map(|f| f.to_string())
            .collect();
        files.sort();
        files
    }

    fn changes(paths: &[&str]) -> FsChanges {
        FsChanges {
            root: RootId(1),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            rescan: false,
        }
    }

    #[test]
    fn walks_the_kept_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for f in [
            "src/main.rs",
            "src/deep/er/x.rs",
            "target/debug/out",
            "notes.log",
            "keep.log",
            ".github/ci.yml",
            ".git/HEAD",
            "sub/local.txt",
            "sub/other.txt",
        ] {
            write(root, f);
        }
        write(root, ".gitignore");
        fs::write(root.join(".gitignore"), "target/\n*.log\n!keep.log\n").unwrap();
        fs::write(root.join("sub/.gitignore"), "local.txt\n").unwrap();
        let index = FileIndex::new(vec![(RootId(1), root.to_path_buf())]);
        assert!(!index.is_ready());
        let ignores = IgnoreCache::default();
        index.rebuild(&ignores);
        assert!(index.is_ready());
        assert_eq!(
            sorted(&index),
            [
                ".github/ci.yml",
                ".gitignore",
                "keep.log",
                "src/deep/er/x.rs",
                "src/main.rs",
                "sub/.gitignore",
                "sub/other.txt",
            ]
        );
        assert_eq!(walk(root, "src/deep", &ignores), ["src/deep/er/x.rs"]);
        assert!(walk(root, "target", &ignores).is_empty());
    }

    #[test]
    fn follows_creates_renames_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "a.rs");
        write(root, "src/b.rs");
        write(root, "src/inner/c.rs");
        fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();
        let ignores = IgnoreCache::default();
        let index = FileIndex::new(vec![(RootId(1), root.to_path_buf())]);
        index.rebuild(&ignores);
        let before = index.snapshot();

        // A new file, a new ignored file, a new folder with files in it.
        write(root, "new.rs");
        write(root, "junk.tmp");
        write(root, "pkg/lib/d.rs");
        write(root, "pkg/e.rs");
        index.apply(&changes(&["junk.tmp", "new.rs", "pkg"]), &ignores);
        assert_eq!(
            sorted(&index),
            [
                ".gitignore",
                "a.rs",
                "new.rs",
                "pkg/e.rs",
                "pkg/lib/d.rs",
                "src/b.rs",
                "src/inner/c.rs"
            ]
        );
        // Snapshots taken earlier don't change.
        assert_eq!(before[0].1.len(), 4);

        // A rename of a file and of a folder: both ends are reported.
        fs::rename(root.join("a.rs"), root.join("z.rs")).unwrap();
        fs::rename(root.join("src"), root.join("lib")).unwrap();
        index.apply(&changes(&["a.rs", "lib", "src", "z.rs"]), &ignores);
        assert_eq!(
            sorted(&index),
            [
                ".gitignore",
                "lib/b.rs",
                "lib/inner/c.rs",
                "new.rs",
                "pkg/e.rs",
                "pkg/lib/d.rs",
                "z.rs"
            ]
        );

        // Deletes: a file and a whole folder.
        fs::remove_file(root.join("z.rs")).unwrap();
        fs::remove_dir_all(root.join("pkg")).unwrap();
        index.apply(&changes(&["pkg", "z.rs"]), &ignores);
        assert_eq!(
            sorted(&index),
            [".gitignore", "lib/b.rs", "lib/inner/c.rs", "new.rs"]
        );

        // A `.gitignore` change re-checks its folder.
        fs::write(root.join(".gitignore"), "*.tmp\nlib/\n").unwrap();
        index.apply(&changes(&[".gitignore"]), &ignores);
        assert_eq!(sorted(&index), [".gitignore", "new.rs"]);
        fs::write(root.join("lib/.gitignore"), "x\n").unwrap();
        fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();
        index.apply(&changes(&[".gitignore"]), &ignores);
        assert_eq!(index.len(), 5, "{:?}", sorted(&index));

        // A rescan walks it all again.
        write(root, "missed.rs");
        index.apply(
            &FsChanges {
                root: RootId(1),
                paths: Vec::new(),
                rescan: true,
            },
            &ignores,
        );
        assert!(sorted(&index).contains(&"missed.rs".to_owned()));
        // Batches for other roots are none of its business.
        index.apply(
            &FsChanges {
                root: RootId(9),
                paths: vec!["new.rs".into()],
                rescan: true,
            },
            &ignores,
        );
        assert_eq!(index.len(), 6);
    }

    #[test]
    fn ancestors_innermost_first() {
        assert_eq!(ancestors("a/b/c").collect::<Vec<_>>(), ["a/b", "a"]);
        assert_eq!(ancestors("a").count(), 0);
    }
}
