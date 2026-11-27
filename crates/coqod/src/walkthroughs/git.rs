// SPDX-License-Identifier: Apache-2.0

//! Resolving a walkthrough scope's revisions: a minimal, read-only `git
//! rev-parse`.
//!
//! Stand-in until the git mod (#91) offers its `git/*` API: then the scope
//! resolves through it, and this module goes. It only ever runs `git
//! rev-parse --verify` with a revision that cannot be an option, with
//! prompts and optional locks off, so it reads and never writes.

use std::path::Path;
use std::process::{Command, Stdio};

/// Longest revision name accepted.
const MAX_REV: usize = 256;

/// Why a revision did not resolve.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RevError {
    /// The root is not inside a git repository.
    NotARepo,
    /// No commit has that name.
    Unknown,
    /// The name is not a revision name.
    Invalid,
    /// `git` could not be run.
    NoGit(String),
}

fn git(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Whether `root` is inside a git work tree.
pub(crate) fn is_repo(root: &Path) -> Result<bool, RevError> {
    let out = git(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|e| RevError::NoGit(e.to_string()))?;
    Ok(out.status.success() && out.stdout.starts_with(b"true"))
}

/// The commit `rev` names in the repository at `root`, as a full hash.
pub(crate) fn rev_parse(root: &Path, rev: &str) -> Result<String, RevError> {
    if rev.is_empty()
        || rev.len() > MAX_REV
        || rev.starts_with('-')
        || rev.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(RevError::Invalid);
    }
    if !is_repo(root)? {
        return Err(RevError::NotARepo);
    }
    let out = git(root)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .map_err(|e| RevError::NoGit(e.to_string()))?;
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() && !hash.is_empty() {
        Ok(hash)
    } else {
        Err(RevError::Unknown)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A git repository in `dir` with one commit of `files`; its hash.
    pub(crate) fn repo(dir: &Path, files: &[(&str, &str)]) -> String {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        run(&["init", "-q", "-b", "main"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "first"]);
        run(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    #[test]
    fn revisions_resolve_in_a_repository_only() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(rev_parse(dir.path(), "HEAD"), Err(RevError::NotARepo));
        let head = repo(dir.path(), &[("a.txt", "a\n")]);
        assert_eq!(rev_parse(dir.path(), "HEAD").unwrap(), head);
        assert_eq!(rev_parse(dir.path(), "main").unwrap(), head);
        assert_eq!(rev_parse(dir.path(), "nope"), Err(RevError::Unknown));
        assert_eq!(rev_parse(dir.path(), "--all"), Err(RevError::Invalid));
        assert_eq!(rev_parse(dir.path(), "a b"), Err(RevError::Invalid));
    }
}
