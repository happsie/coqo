// SPDX-License-Identifier: Apache-2.0

//! Workspace conventions the kernel and mods share, so a mod that walks a
//! root (search, say) skips what the file tree, the watcher and the file
//! index skip (ADR 0027).

use std::path::Path;

/// Whether the folder `dir`, somewhere **below** a workspace root, is a
/// world of its own and is left out whatever the ignore files say:
///
/// - it holds a `.git` file or folder: a nested clone, a submodule or a
///   worktree (`git worktree add`, `.claude/worktrees/*`);
/// - it holds a `CACHEDIR.TAG` file (<https://bford.info/cachedir/>), as
///   Cargo's `target/` does;
/// - it is named `node_modules`.
///
/// A root itself never stands apart (its own `.git` doesn't count), so
/// callers must not ask about the root. Costs up to two `stat`s; symbolic
/// links named `.git` count, a `CACHEDIR.TAG` must be a regular file.
pub fn stands_apart(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "node_modules")
        || dir.join(".git").symlink_metadata().is_ok()
        || dir
            .join("CACHEDIR.TAG")
            .symlink_metadata()
            .is_ok_and(|m| m.is_file())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::stands_apart;

    #[test]
    fn nested_repositories_caches_and_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = |rel: &str| {
            let p = root.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        };
        let worktree = dir("worktree");
        fs::write(worktree.join(".git"), "gitdir: ../.git/worktrees/w\n").unwrap();
        let clone = dir("clone/.git");
        let cache = dir("target");
        fs::write(
            cache.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        let not_a_tag = dir("odd/CACHEDIR.TAG");

        assert!(stands_apart(&worktree), "a worktree");
        assert!(stands_apart(clone.parent().unwrap()), "a nested clone");
        assert!(stands_apart(&cache), "a tagged cache");
        assert!(stands_apart(&dir("web/node_modules")));
        assert!(!stands_apart(&dir("src")));
        assert!(
            !stands_apart(not_a_tag.parent().unwrap()),
            "the tag is a file"
        );
        assert!(!stands_apart(&dir("node_modules_old")));
    }
}
