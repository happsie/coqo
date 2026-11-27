// SPDX-License-Identifier: Apache-2.0

//! Writes, through the user's `git`: staging whole files, commits,
//! branches and the stash. (Line staging is in `patch.rs`.)

use std::ffi::OsStr;
use std::io::Write as _;
use std::process::{Command, Stdio};

use crate::{ErrorKind, GitError, Repo, Result, check_path};

/// Environment that would point `git` somewhere else than the work tree it
/// runs in (set when the daemon was started from a git hook, say).
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_NAMESPACE",
];

/// A created commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// Its id.
    pub oid: String,
    /// Its message's first line.
    pub summary: String,
}

impl Repo {
    /// Run `git args…` in the work tree, with `stdin`; its stdout, or a
    /// [`ErrorKind::Command`] error with what it printed.
    pub(crate) fn git<I, S>(&self, args: I, stdin: Option<&[u8]>) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.options.program);
        cmd.args(args)
            .current_dir(self.workdir())
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", ":")
            .env("GIT_SEQUENCE_EDITOR", ":")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in REPO_ENV {
            cmd.env_remove(var);
        }
        let mut child = cmd.spawn().map_err(|e| {
            GitError::new(
                ErrorKind::Command,
                format!("cannot run {}: {e}", self.options.program.display()),
            )
        })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A git that exits early closes the pipe; its status says why.
            let _ = pipe.write_all(input);
        }
        let out = child.wait_with_output().map_err(GitError::other)?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let said = String::from_utf8_lossy(if out.stderr.trim_ascii().is_empty() {
            &out.stdout
        } else {
            &out.stderr
        })
        .trim()
        .to_owned();
        Err(GitError::new(
            ErrorKind::Command,
            if said.is_empty() {
                format!("git failed ({})", out.status)
            } else {
                said
            },
        ))
    }

    /// Stage whole files: `git add -A -- paths` (new, changed and deleted
    /// files alike; marks a conflict resolved).
    pub fn stage(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        for p in paths {
            check_path(p.trim_end_matches('/'))?;
        }
        // Paths are paths, never globs.
        let mut args = vec!["--literal-pathspecs", "add", "-A", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.git(args, None).map(drop)
    }

    /// Unstage whole files: back to `HEAD`'s version in the index (`git
    /// restore --staged`), or out of it on a branch with no commits yet.
    pub fn unstage(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        for p in paths {
            check_path(p)?;
        }
        let unborn = self.gix().head_id().is_err();
        let mut args = if unborn {
            vec!["--literal-pathspecs", "rm", "--cached", "-r", "-q", "--"]
        } else {
            vec!["--literal-pathspecs", "restore", "--staged", "--"]
        };
        args.extend(paths.iter().map(String::as_str));
        self.git(args, None).map(drop)
    }

    /// Commit what is staged (`git commit -F -`), or amend the last commit.
    /// Hooks run.
    pub fn commit(&self, message: &str, amend: bool) -> Result<Commit> {
        if message.trim().is_empty() && !amend {
            return Err(GitError::new(
                ErrorKind::Invalid,
                "the commit message is empty",
            ));
        }
        let mut args = vec!["commit", "-q", "-F", "-"];
        if amend {
            args.push("--amend");
        }
        if message.trim().is_empty() {
            // Amend keeping the message.
            args = vec!["commit", "-q", "--amend", "--no-edit"];
            self.git(args, None)?;
        } else {
            self.git(args, Some(message.as_bytes()))?;
        }
        // A fresh handle: the one we hold may have cached the old HEAD.
        let repo = gix::open(self.gix().git_dir()).map_err(GitError::other)?;
        let commit = repo.head_commit().map_err(GitError::other)?;
        let summary = commit
            .message()
            .map(|m| m.summary().to_string())
            .unwrap_or_default();
        Ok(Commit {
            oid: commit.id.to_string(),
            summary,
        })
    }

    /// Create branch `name` at `start` (default `HEAD`), and switch to it
    /// if `switch`.
    pub fn create_branch(&self, name: &str, start: Option<&str>, switch: bool) -> Result<()> {
        check_ref_arg(name)?;
        if let Some(start) = start {
            check_ref_arg(start)?;
        }
        let mut args = if switch {
            vec!["switch", "-q", "-c", name]
        } else {
            vec!["branch", name]
        };
        args.extend(start);
        self.git(args, None).map(drop)
    }

    /// Switch to branch `name` (`git switch`; a remote branch's short name
    /// creates a tracking branch). Fails if changes would be overwritten.
    pub fn switch_branch(&self, name: &str) -> Result<()> {
        check_ref_arg(name)?;
        self.git(["switch", "-q", name], None).map(drop)
    }

    /// `git stash push`: put the changes away. Returns `false` if there was
    /// nothing to stash.
    pub fn stash_push(&self, message: Option<&str>, include_untracked: bool) -> Result<bool> {
        let before = self.stashes()?.first().map(|s| s.oid.clone());
        let mut args = vec!["stash", "push", "-q"];
        if include_untracked {
            args.push("--include-untracked");
        }
        if let Some(m) = message.filter(|m| !m.trim().is_empty()) {
            args.push("-m");
            args.push(m);
        }
        self.git(args, None)?;
        let after = self.stashes()?.first().map(|s| s.oid.clone());
        Ok(after.is_some() && after != before)
    }

    /// `git stash pop stash@{index}`: apply a stash and drop it.
    pub fn stash_pop(&self, index: usize) -> Result<()> {
        self.stash_op("pop", index)
    }

    /// `git stash apply stash@{index}`: apply a stash and keep it.
    pub fn stash_apply(&self, index: usize) -> Result<()> {
        self.stash_op("apply", index)
    }

    /// `git stash drop stash@{index}`.
    pub fn stash_drop(&self, index: usize) -> Result<()> {
        self.stash_op("drop", index)
    }

    fn stash_op(&self, op: &str, index: usize) -> Result<()> {
        let count = self.stashes()?.len();
        if index >= count {
            return Err(GitError::new(
                ErrorKind::NotFound,
                format!("there is no stash@{{{index}}}"),
            ));
        }
        let stash = format!("stash@{{{index}}}");
        self.git(["stash", op, "-q", stash.as_str()], None)
            .map(drop)
    }
}

/// A branch name or revision passed to `git`: not empty, not an option,
/// no whitespace or control characters. git checks the rest.
fn check_ref_arg(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(GitError::new(
            ErrorKind::Invalid,
            format!("not a valid branch name or revision: {name:?}"),
        ));
    }
    Ok(())
}
