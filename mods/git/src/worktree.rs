// SPDX-License-Identifier: Apache-2.0

//! Worktrees (#92, ADR 0028): `git worktree list`, `add` and `remove`,
//! through the user's `git`.
//!
//! - [`Repo::worktrees`] lists every worktree of the repository, the main
//!   one first, as `git worktree list --porcelain -z` does (a bare
//!   repository's own entry is left out: it has no work tree).
//! - [`Repo::add_worktree`] checks a branch out in a new worktree: a new
//!   branch from a base, or an existing branch as it is.
//! - [`Repo::remove_worktree`] removes one, never the main worktree and
//!   **never with `--force`**: uncommitted changes always stop it, and
//!   commits only its `HEAD` has stop it until confirmed.
//! - New worktrees go to `<parent>/<repo>-worktrees/<branch>` beside the
//!   main worktree ([`Repo::default_worktree_path`]).

use std::path::{Path, PathBuf};

use crate::cli::check_ref_arg;
use crate::{ErrorKind, GitError, Repo, Result};

/// One worktree of a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// Its top folder (canonical when it exists).
    pub path: PathBuf,
    /// The commit checked out; `None` on a branch with no commits yet.
    pub head: Option<String>,
    /// The branch's short name; `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The main worktree (the one the repository was cloned or made in).
    pub main: bool,
    /// `git worktree lock`ed: git refuses to remove it.
    pub locked: bool,
    /// Its folder is gone; `git worktree prune` forgets it.
    pub prunable: bool,
}

/// Work a worktree's removal would put at risk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorktreeWork {
    /// Changed files, staged or not, untracked ones included.
    pub changes: usize,
    /// Commits only this worktree's `HEAD` has: on no other branch, no
    /// remote-tracking branch and no tag.
    pub unpushed: usize,
}

impl WorktreeWork {
    /// Whether there is nothing at risk.
    pub fn is_clean(&self) -> bool {
        self.changes == 0 && self.unpushed == 0
    }
}

/// What [`Repo::remove_worktree`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The worktree is gone (its branch stays).
    Removed,
    /// Not removed: it has work. Uncommitted changes always stop removal
    /// (Coqo never forces); unpushed commits only until confirmed.
    HasWork(WorktreeWork),
}

/// The folder a branch's worktree gets: the branch name with anything but
/// letters, digits, `.`, `_` and `-` as `-` (`feature/login` →
/// `feature-login`).
fn slug(branch: &str) -> String {
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(|c| c == '-' || c == '.');
    if slug.is_empty() {
        "worktree".to_owned()
    } else {
        slug.to_owned()
    }
}

/// Parse `git worktree list --porcelain -z`.
fn parse_list(out: &[u8]) -> Vec<Worktree> {
    let out = String::from_utf8_lossy(out);
    let mut list = Vec::new();
    let mut current: Option<(Worktree, bool)> = None;
    let flush = |current: &mut Option<(Worktree, bool)>, list: &mut Vec<Worktree>| {
        if let Some((wt, bare)) = current.take()
            && !bare
        {
            list.push(wt);
        }
    };
    for line in out.split('\0') {
        if line.is_empty() {
            flush(&mut current, &mut list);
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            flush(&mut current, &mut list);
            let path = PathBuf::from(value);
            let path = path.canonicalize().unwrap_or(path);
            current = Some((
                Worktree {
                    path,
                    head: None,
                    branch: None,
                    main: false,
                    locked: false,
                    prunable: false,
                },
                false,
            ));
            continue;
        }
        let Some((wt, bare)) = current.as_mut() else {
            continue;
        };
        match key {
            "HEAD" => {
                wt.head = Some(value.to_owned()).filter(|h| h.bytes().any(|b| b != b'0'));
            }
            "branch" => {
                wt.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_owned(),
                );
            }
            "bare" => *bare = true,
            "locked" => wt.locked = true,
            "prunable" => wt.prunable = true,
            _ => {}
        }
    }
    flush(&mut current, &mut list);
    list
}

impl Repo {
    /// Every worktree of the repository, the main one first.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let out = self.git(["worktree", "list", "--porcelain", "-z"], None)?;
        let mut list = parse_list(&out);
        // git lists the main worktree first; a bare repository has none.
        let main = self.main_worktree_dir();
        for wt in &mut list {
            wt.main = main.as_deref() == Some(wt.path.as_path());
        }
        Ok(list)
    }

    /// The main worktree's folder: the one the common directory is the
    /// `.git` of. `None` for a bare repository.
    pub fn main_worktree_dir(&self) -> Option<PathBuf> {
        let common = self.common_dir().canonicalize().ok()?;
        if common.file_name()? != ".git" {
            return None;
        }
        common.parent().map(Path::to_path_buf)
    }

    /// Where [`add_worktree`](Self::add_worktree) puts `branch` by default:
    /// `<parent>/<repo>-worktrees/<branch>` beside the main worktree (beside
    /// the repository, for a bare one), with `-2`, `-3`, … added if that is
    /// taken.
    pub fn default_worktree_path(&self, branch: &str) -> Result<PathBuf> {
        let dir = self.worktrees_dir()?;
        let base = slug(branch);
        let mut path = dir.join(&base);
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("{base}-{n}"));
            n += 1;
        }
        Ok(path)
    }

    /// `<parent>/<repo>-worktrees`.
    pub fn worktrees_dir(&self) -> Result<PathBuf> {
        let top = match self.main_worktree_dir() {
            Some(main) => main,
            None => self.common_dir().canonicalize().map_err(GitError::other)?,
        };
        let name = top
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.strip_suffix(".git").unwrap_or(n).to_owned())
            .unwrap_or_else(|| "repo".to_owned());
        let parent = top.parent().unwrap_or(&top);
        Ok(parent.join(format!("{name}-worktrees")))
    }

    /// Check `branch` out in a new worktree at `path` (default:
    /// [`default_worktree_path`](Self::default_worktree_path)). A branch
    /// that does not exist is created from `base` (default `HEAD`); an
    /// existing one is checked out as it is, and `base` is ignored. git
    /// refuses a branch another worktree has checked out.
    pub fn add_worktree(
        &self,
        branch: &str,
        base: Option<&str>,
        path: Option<&Path>,
    ) -> Result<Worktree> {
        check_ref_arg(branch)?;
        if let Some(base) = base {
            check_ref_arg(base)?;
        }
        let path = match path {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => {
                return Err(GitError::new(
                    ErrorKind::Invalid,
                    format!("a worktree's path must be absolute: {}", p.display()),
                ));
            }
            None => self.default_worktree_path(branch)?,
        };
        if path.exists() {
            return Err(GitError::new(
                ErrorKind::Invalid,
                format!("{} already exists", path.display()),
            ));
        }
        let Some(path_str) = path.to_str() else {
            return Err(GitError::new(
                ErrorKind::Invalid,
                "a worktree's path must be UTF-8",
            ));
        };
        let exists = self
            .gix()
            .try_find_reference(format!("refs/heads/{branch}").as_str())
            .map_err(GitError::other)?
            .is_some();
        let mut args = vec!["worktree", "add", "-q"];
        if exists {
            args.extend([path_str, branch]);
        } else {
            args.extend(["-b", branch, path_str]);
            args.extend(base);
        }
        self.git(args, None)?;
        let path = path.canonicalize().map_err(GitError::other)?;
        self.worktrees()?
            .into_iter()
            .find(|w| w.path == path)
            .ok_or_else(|| {
                GitError::new(
                    ErrorKind::Other,
                    format!("git made no worktree at {}", path.display()),
                )
            })
    }

    /// The work removing the worktree at `path` would put at risk.
    pub fn worktree_work(&self, path: &Path) -> Result<WorktreeWork> {
        let repo = Repo::open(path, self.options.clone())?;
        let changes = repo.status()?.entries.len();
        let unpushed = repo.unpushed()?;
        Ok(WorktreeWork { changes, unpushed })
    }

    /// How many commits `HEAD` has that no other branch, remote-tracking
    /// branch or tag has.
    fn unpushed(&self) -> Result<usize> {
        if self.gix().head_id().is_err() {
            return Ok(0);
        }
        let branch = self
            .gix()
            .head_name()
            .map_err(GitError::other)?
            .map(|name| name.shorten().to_string());
        // `--branches` matches `--exclude` against the short name.
        let exclude = branch.map(|b| format!("--exclude={b}"));
        let mut args = vec!["rev-list", "--count", "HEAD", "--not"];
        args.extend(exclude.as_deref());
        args.extend(["--branches", "--remotes", "--tags"]);
        let out = self.git(args, None)?;
        String::from_utf8_lossy(&out)
            .trim()
            .parse()
            .map_err(GitError::other)
    }

    /// The changed files of the worktree at `path`, staged or not.
    pub fn worktree_changes(&self, path: &Path) -> Result<usize> {
        Ok(Repo::open(path, self.options.clone())?
            .status()?
            .entries
            .len())
    }

    /// Remove the worktree at `path`: `git worktree remove`, never forced.
    /// Uncommitted changes always stop it, and commits only its `HEAD` has
    /// stop it unless `confirm`ed; both come back as
    /// [`Removal::HasWork`]. The main worktree and a locked one are
    /// refused. A worktree whose folder is gone is pruned.
    pub fn remove_worktree(&self, path: &Path, confirm: bool) -> Result<Removal> {
        let wanted = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let list = self.worktrees()?;
        let Some(wt) = list.iter().find(|w| w.path == wanted) else {
            return Err(GitError::new(
                ErrorKind::NotFound,
                format!("{} is not a worktree of this repository", path.display()),
            ));
        };
        if wt.main {
            return Err(GitError::new(
                ErrorKind::Invalid,
                "the main worktree can't be removed",
            ));
        }
        if wt.locked {
            return Err(GitError::new(
                ErrorKind::Invalid,
                format!("{} is locked (git worktree unlock)", wt.path.display()),
            ));
        }
        // Run from a worktree that stays: the main one, or the common
        // directory of a bare repository.
        let from = self
            .main_worktree_dir()
            .or_else(|| self.common_dir().canonicalize().ok())
            .unwrap_or_else(|| self.workdir().to_path_buf());
        if wt.prunable {
            self.git_in(&from, ["worktree", "prune"])?;
            return Ok(Removal::Removed);
        }
        let work = self.worktree_work(&wt.path)?;
        if work.changes > 0 || (work.unpushed > 0 && !confirm) {
            return Ok(Removal::HasWork(work));
        }
        let Some(target) = wt.path.to_str() else {
            return Err(GitError::new(
                ErrorKind::Invalid,
                "a worktree's path must be UTF-8",
            ));
        };
        self.git_in(&from, ["worktree", "remove", target])?;
        Ok(Removal::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_list, slug};
    use std::path::PathBuf;

    #[test]
    fn slugs_of_branch_names() {
        assert_eq!(slug("feature/login"), "feature-login");
        assert_eq!(slug("fix-42"), "fix-42");
        assert_eq!(slug("v1.2"), "v1.2");
        assert_eq!(slug("/"), "worktree");
    }

    #[test]
    fn parses_the_porcelain_listing() {
        let out = b"worktree /nowhere/repo\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0\
worktree /nowhere/wt\0HEAD 2222222222222222222222222222222222222222\0detached\0locked\0\0\
worktree /nowhere/gone\0HEAD 3333333333333333333333333333333333333333\0branch refs/heads/feature/x\0prunable gitdir file points to non-existent location\0\0";
        let list = parse_list(out);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].path, PathBuf::from("/nowhere/repo"));
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert!(list[1].branch.is_none() && list[1].locked);
        assert_eq!(list[2].branch.as_deref(), Some("feature/x"));
        assert!(list[2].prunable);
        // A bare repository's own entry is left out.
        assert!(parse_list(b"worktree /nowhere/r.git\0bare\0\0").is_empty());
    }
}
