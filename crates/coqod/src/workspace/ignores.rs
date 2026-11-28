// SPDX-License-Identifier: Apache-2.0

//! Git ignore rules for the file tree, using the `ignore` crate's matcher.
//!
//! Precedence follows git: a directory's `.gitignore` beats its parents',
//! which beat `.git/info/exclude`, which beats the user's global excludes. A
//! path inside an ignored directory is ignored. `.git` is always ignored.
//!
//! A folder below a root that [stands apart](stands_apart) is ignored too,
//! whatever the ignore files say: a nested git repository or worktree (it
//! holds a `.git` file or folder) is its own project, a cache folder marked
//! with `CACHEDIR.TAG` (Cargo's `target/`, for one) is build output, and so
//! is `node_modules`. The tree shows them dimmed; the watcher puts no OS
//! watches inside them (ADR 0027), and the file index (`fs/find`) leaves
//! them out. The rule itself is `coqo_mod_api::workspace::stands_apart`,
//! which the search mod uses too.
//!
//! Matchers are cached per directory and revalidated against the
//! `.gitignore` file's size and mtime on each use, so edits are picked up
//! without a watcher.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Cached matchers are dropped wholesale beyond this many directories.
const MAX_CACHED_DIRS: usize = 50_000;

type Stamp = Option<(SystemTime, u64)>;

#[derive(Debug)]
struct Cached {
    stamp: Stamp,
    matcher: Option<Arc<Gitignore>>,
}

/// Ignore matchers shared by every workspace (keyed by absolute directory).
#[derive(Debug, Default)]
pub(crate) struct IgnoreCache {
    dirs: Mutex<HashMap<PathBuf, Cached>>,
    global: OnceLock<Option<Arc<Gitignore>>>,
}

/// The rules that apply inside one directory.
#[derive(Debug)]
pub(crate) struct IgnoreChain {
    /// Lowest precedence first.
    matchers: Vec<Arc<Gitignore>>,
    /// Whether the directory itself is ignored (then so is everything in it).
    pub dir_ignored: bool,
}

impl IgnoreChain {
    /// Whether the entry `path` (directly inside the chain's directory) is
    /// ignored.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.dir_ignored
            || path.file_name().is_some_and(|n| n == ".git")
            || eval(&self.matchers, path, is_dir)
            || (is_dir && stands_apart(path))
    }
}

// Whether a folder below a root is a world of its own: a nested repository
// or worktree, a `CACHEDIR.TAG` cache or `node_modules`. The rule lives in
// `coqo-mod-api` so mods that walk a root (search) share the one copy (ADR
// 0027); callers ask only about folders the ignore files keep.
pub(crate) use coqo_mod_api::workspace::stands_apart;

fn eval(matchers: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> bool {
    for m in matchers.iter().rev() {
        match m.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

fn stamp(file: &Path) -> Stamp {
    let meta = fs::metadata(file).ok()?;
    meta.is_file().then(|| {
        (
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
        )
    })
}

fn build(base: &Path, file: &Path) -> Option<Arc<Gitignore>> {
    let mut b = GitignoreBuilder::new(base);
    if let Some(e) = b.add(file) {
        tracing::debug!(file = %file.display(), error = %e, "ignoring part of an ignore file");
    }
    match b.build() {
        Ok(g) if !g.is_empty() => Some(Arc::new(g)),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!(file = %file.display(), error = %e, "unusable ignore file");
            None
        }
    }
}

impl IgnoreCache {
    fn cached(&self, key: PathBuf, base: &Path, file: &Path) -> Option<Arc<Gitignore>> {
        let stamp = stamp(file);
        let mut dirs = self.dirs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = dirs.get(&key)
            && c.stamp == stamp
        {
            return c.matcher.clone();
        }
        let matcher = stamp.and_then(|_| build(base, file));
        if dirs.len() >= MAX_CACHED_DIRS {
            dirs.clear();
        }
        dirs.insert(
            key,
            Cached {
                stamp,
                matcher: matcher.clone(),
            },
        );
        matcher
    }

    fn global(&self) -> Option<Arc<Gitignore>> {
        self.global
            .get_or_init(|| {
                let (g, err) = Gitignore::global();
                if let Some(e) = err {
                    tracing::debug!(error = %e, "global git excludes partly unreadable");
                }
                (!g.is_empty()).then(|| Arc::new(g))
            })
            .clone()
    }

    /// The rules for directory `root/comps…`.
    pub fn chain(&self, root: &Path, comps: &[impl AsRef<str>]) -> IgnoreChain {
        let mut matchers = Vec::with_capacity(comps.len() + 3);
        matchers.extend(self.global());
        let exclude = root.join(".git/info/exclude");
        matchers.extend(self.cached(exclude.clone(), root, &exclude));
        let mut dir = root.to_path_buf();
        matchers.extend(self.cached(dir.clone(), &dir, &dir.join(".gitignore")));
        let mut dir_ignored = false;
        for comp in comps {
            let comp = comp.as_ref();
            dir.push(comp);
            dir_ignored =
                dir_ignored || comp == ".git" || eval(&matchers, &dir, true) || stands_apart(&dir);
            if dir_ignored {
                // Nothing below an ignored directory can be re-included.
                break;
            }
            matchers.extend(self.cached(dir.clone(), &dir, &dir.join(".gitignore")));
        }
        IgnoreChain {
            matchers,
            dir_ignored,
        }
    }

    /// The rules inside `dir`, a directory directly inside the one `parent`
    /// is for: a walk carries the chain down instead of rebuilding it from
    /// the root for every directory.
    pub fn descend(&self, parent: &IgnoreChain, dir: &Path) -> IgnoreChain {
        let mut matchers = parent.matchers.clone();
        matchers.extend(self.cached(dir.to_path_buf(), dir, &dir.join(".gitignore")));
        IgnoreChain {
            matchers,
            dir_ignored: parent.dir_ignored,
        }
    }

    /// Whether `root/comps…` is ignored.
    pub fn is_ignored(&self, root: &Path, comps: &[impl AsRef<str>], is_dir: bool) -> bool {
        match comps.split_last() {
            None => false,
            Some((_, parent)) => {
                let chain = self.chain(root, parent);
                chain.is_ignored(&join(root, comps), is_dir)
            }
        }
    }

    /// Whether the directory containing `root/comps…` is ignored (or is
    /// `.git`), i.e. changes to the path are noise for the file tree.
    pub fn parent_ignored(&self, root: &Path, comps: &[impl AsRef<str>]) -> bool {
        match comps.split_last() {
            None => false,
            Some((_, parent)) => self.chain(root, parent).dir_ignored,
        }
    }
}

fn join(root: &Path, comps: &[impl AsRef<str>]) -> PathBuf {
    let mut p = root.to_path_buf();
    p.extend(comps.iter().map(AsRef::as_ref));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, contents: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    fn comps(rel: &str) -> Vec<&str> {
        rel.split('/').collect()
    }

    #[test]
    fn follows_git_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.tmp\nbuild/\n!keep.tmp\n");
        write(root, "sub/.gitignore", "!sub-keep.tmp\nlocal.txt\n");
        write(root, ".git/info/exclude", "secret.cfg\n");
        let cache = IgnoreCache::default();
        let ig = |rel: &str, dir: bool| cache.is_ignored(root, &comps(rel), dir);

        assert!(ig("a.tmp", false));
        assert!(!ig("keep.tmp", false));
        assert!(!ig("a.rs", false));
        assert!(ig("build", true));
        assert!(!ig("build", false), "`build/` only matches directories");
        assert!(ig("build/inner/x.rs", false), "inside an ignored dir");
        assert!(ig("sub/x.tmp", false));
        assert!(!ig("sub/sub-keep.tmp", false), "deeper whitelist wins");
        assert!(ig("sub/local.txt", false));
        assert!(!ig("local.txt", false), "sub's rules don't apply above it");
        assert!(ig("secret.cfg", false), ".git/info/exclude");
        assert!(ig(".git", true));
        assert!(ig(".git/config", false));
        assert!(cache.parent_ignored(root, &comps("build/x")));
        assert!(!cache.parent_ignored(root, &comps("build")));
        assert!(!cache.parent_ignored(root, &comps("sub/x")));
    }

    #[test]
    fn nested_repositories_and_caches_stand_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".git/HEAD", "ref: refs/heads/main\n");
        write(root, "src/lib.rs", "");
        // A worktree (a `.git` file), a nested clone, a tagged cache.
        write(
            root,
            ".claude/worktrees/agent/.git",
            "gitdir: ../../../.git/worktrees/agent\n",
        );
        write(root, ".claude/worktrees/agent/src/lib.rs", "");
        write(root, "vendor/dep/.git/HEAD", "ref: refs/heads/main\n");
        write(root, "vendor/dep/x.rs", "");
        write(
            root,
            "out/CACHEDIR.TAG",
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        write(root, "out/debug/x.o", "");
        write(root, "web/node_modules/left-pad/index.js", "");
        write(root, "docs/node_modules.md", "");
        let cache = IgnoreCache::default();
        let ig = |rel: &str, dir: bool| cache.is_ignored(root, &comps(rel), dir);

        assert!(!ig("src", true), "the root's own repository");
        assert!(!ig(".claude", true));
        assert!(ig(".claude/worktrees/agent", true), "a worktree");
        assert!(ig(".claude/worktrees/agent/src/lib.rs", false));
        assert!(!ig("vendor", true));
        assert!(ig("vendor/dep", true), "a nested clone");
        assert!(ig("vendor/dep/x.rs", false));
        assert!(ig("out", true), "a tagged cache");
        assert!(cache.parent_ignored(root, &comps("out/debug/x.o")));
        assert!(!ig("out", false), "only folders stand apart");
        assert!(!ig("web", true));
        assert!(
            ig("web/node_modules", true),
            "node_modules, without a .gitignore"
        );
        assert!(ig("web/node_modules/left-pad/index.js", false));
        assert!(!ig("docs/node_modules.md", false));
        // The chain a walk carries down agrees with one built from the root.
        let web = cache.chain(root, &comps("web"));
        assert!(web.is_ignored(&root.join("web/node_modules"), true));
        let src = cache.descend(&cache.chain(root, &[] as &[&str]), &root.join("src"));
        assert!(!src.dir_ignored);
        assert!(!src.is_ignored(&root.join("src/lib.rs"), false));
    }

    #[test]
    fn picks_up_edits_to_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cache = IgnoreCache::default();
        assert!(!cache.is_ignored(root, &["x.log"], false));
        write(root, ".gitignore", "*.log\n");
        assert!(cache.is_ignored(root, &["x.log"], false));
        // A different size, so the edit is seen even if the mtime is coarse.
        write(root, ".gitignore", "*.lo\n");
        assert!(!cache.is_ignored(root, &["x.log"], false));
        fs::remove_file(root.join(".gitignore")).unwrap();
        assert!(!cache.is_ignored(root, &["x.lo"], false));
    }
}
