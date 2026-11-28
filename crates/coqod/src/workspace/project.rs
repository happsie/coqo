// SPDX-License-Identifier: Apache-2.0

//! Which project (repository) a workspace belongs to (#92, ADR 0028): the
//! git common directory of its first root, the directory every worktree of
//! a repository shares, so worktrees made outside Coqo group with the rest.
//!
//! Found the way `git rev-parse --git-common-dir` finds it, from the
//! folder up: a `.git` folder is the common directory itself (unless it
//! names another in `commondir`); a `.git` file (a linked worktree, a
//! submodule) points at its git directory with `gitdir:`, whose
//! `commondir` file, if any, names the common directory. The tests check
//! the answer against the CLI. Only reads files, so it is cheap enough to
//! run whenever a workspace opens.

use std::path::{Path, PathBuf};

/// The canonical git common directory of the repository whose work tree
/// contains `dir`, or `None` outside any repository.
pub(crate) fn common_dir(dir: &Path) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    for folder in dir.ancestors() {
        let dot_git = folder.join(".git");
        let Ok(meta) = std::fs::metadata(&dot_git) else {
            continue;
        };
        let git_dir = if meta.is_dir() {
            dot_git
        } else {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
            folder.join(target)
        };
        let common = match std::fs::read_to_string(git_dir.join("commondir")) {
            Ok(text) => git_dir.join(text.trim()),
            Err(_) => git_dir,
        };
        return common.canonicalize().ok();
    }
    None
}

/// The worktree folders of the repository whose common directory is
/// `common`, as `git worktree list` names them: the main worktree (the
/// common directory's parent, unless the repository is bare), then each
/// linked worktree's, read from its admin folder's `gitdir` file. Only
/// reads files. A worktree whose folder is gone is still listed.
pub(crate) fn worktrees(common: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(main) = common.parent()
    {
        found.push(main.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return found;
    };
    let mut linked: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("gitdir")).ok()?;
            // `<worktree>/.git`, absolute (relative with
            // `worktree.useRelativePaths`: to the admin folder).
            let dot_git = e.path().join(text.trim());
            let folder = dot_git.parent()?;
            Some(normalize(folder))
        })
        .collect();
    linked.sort();
    found.extend(linked);
    found
}

/// `path` without `.` and `..` components, without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The project of a workspace whose first root is `root`, as stored:
/// the common directory, or `""` outside any repository. `None` when the
/// folder is not there to look at (keep what was known).
pub(crate) fn stored(root: &Path) -> Option<String> {
    if !root.is_dir() {
        return None;
    }
    Some(
        common_dir(root)
            .and_then(|p| p.to_str().map(str::to_owned))
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::common_dir;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// `git rev-parse --git-common-dir`, absolute and canonical.
    fn cli(dir: &Path) -> PathBuf {
        let out = PathBuf::from(git(dir, &["rev-parse", "--git-common-dir"]));
        dir.join(out).canonicalize().unwrap()
    }

    #[test]
    fn agrees_with_git_rev_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=a",
                "-c",
                "user.email=a@b",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "one",
            ],
        );
        let linked = tmp.path().join("elsewhere/linked");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "x", linked.to_str().unwrap()],
        );
        std::fs::create_dir_all(linked.join("sub")).unwrap();
        for dir in [
            repo.clone(),
            repo.join("src/deep"),
            linked.clone(),
            linked.join("sub"),
        ] {
            assert_eq!(common_dir(&dir), Some(cli(&dir)), "{}", dir.display());
        }
        assert_eq!(common_dir(&linked), common_dir(&repo));
        // The worktrees, as `git worktree list` names them.
        let common = common_dir(&repo).unwrap();
        let listed: Vec<PathBuf> = git(&repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect();
        assert_eq!(super::worktrees(&common), listed);
        assert_eq!(listed.len(), 2);
        assert_eq!(
            common_dir(&repo).unwrap(),
            repo.join(".git").canonicalize().unwrap()
        );

        // Outside any repository (unless the temporary folder is in one).
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let outside = Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(&plain)
            .output()
            .unwrap();
        if !outside.status.success() {
            assert_eq!(common_dir(&plain), None);
            assert_eq!(super::stored(&plain).as_deref(), Some(""));
        }
        assert_eq!(super::stored(&tmp.path().join("missing")), None);
    }
}
