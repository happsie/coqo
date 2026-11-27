// SPDX-License-Identifier: Apache-2.0

//! `coqo.git`: everyday git (PLAN §7.7, issue #91).
//!
//! The mod provides the [`Git`] service (`coqo.git@1`); the daemon's `git/*`
//! methods resolve it on every call, so disabling the mod turns them off and
//! another mod can replace it.
//!
//! - **Reads** use [`gix`]: [`Repo::status`], [`Repo::diff`],
//!   [`Repo::changed_files`], [`Repo::blame`], [`Repo::log`],
//!   [`Repo::branches`] and [`Repo::stashes`].
//! - **Writes** run the user's `git` (hooks, config and credentials apply as
//!   in a terminal): [`Repo::stage`], [`Repo::unstage`] (whole files, or
//!   chosen lines through `git apply --cached`), [`Repo::commit`],
//!   [`Repo::create_branch`], [`Repo::switch_branch`] and the stash
//!   operations.
//!
//! **Paths** are relative to the repository's work tree and `/`-separated.
//! A repository opened from a subdirectory reports it as
//! [`Repo::prefix`]. Everything here is blocking; the daemon calls it on
//! blocking threads.
//!
//! **Fidelity:** results match the `git` CLI on the same repository (the
//! tests compare against `git status --porcelain=v2`, `git diff`, `git log`
//! and `git blame --porcelain`), with these choices: status lists untracked
//! files one by one (`-uall`) and does not pair renames (`--no-renames`);
//! diffs compare raw contents (no `.gitattributes` text conversion or
//! textconv); blame covers committed history only.

mod blame;
mod cli;
mod diff;
mod log;
mod patch;
mod refs;
mod status;
pub mod testing;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use coqo_mod_api::{BoxFuture, Manifest, Mod, ModContext, ModError, Service, ServiceVersion};

pub use blame::{Blame, BlameCommit, BlameHunk};
pub use cli::Commit;
pub use diff::{ChangedFile, DiffLine, FileDiff, FileSide, Hunk, LineKind, Side};
pub use log::{EdgeKind, GraphEdge, GraphRow, LogCommit, LogOptions, LogPage};
pub use patch::LineSelection;
pub use refs::{Branch, Stash};
pub use status::{Change, Conflict, Head, Status, StatusEntry};

/// The mod's `coqo.mod.toml`.
pub const MANIFEST: &str = include_str!("../coqo.mod.toml");

/// The parsed, validated [`MANIFEST`].
pub fn manifest() -> Manifest {
    Manifest::from_toml_str(MANIFEST).expect("coqo.git's manifest is valid")
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The folder is not in a git work tree (or the repository is bare).
    NotARepository,
    /// A path or revision that does not exist.
    NotFound,
    /// A line selection that does not match the current diff: the file
    /// changed since it was shown. Show the diff again.
    StaleSelection,
    /// What was asked cannot be done this way (a line selection in a binary
    /// file, say).
    Unsupported,
    /// Invalid input (an empty commit message, a bad branch name).
    Invalid,
    /// `git` ran and failed; the message carries what it said.
    Command,
    /// Anything else (I/O, a corrupt object).
    Other,
}

/// An error of a git operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitError {
    /// What went wrong.
    pub kind: ErrorKind,
    /// For people.
    pub message: String,
}

impl GitError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn other(e: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Other, e.to_string())
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GitError {}

/// The result of a git operation.
pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// How the mod runs `git`.
#[derive(Clone, Debug)]
pub struct GitOptions {
    /// The `git` executable (default `git`, found on `PATH`).
    pub program: PathBuf,
}

impl Default for GitOptions {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
        }
    }
}

/// The `coqo.git@1` service: opens repositories.
#[derive(Debug, Default)]
pub struct Git {
    options: GitOptions,
}

impl Service for Git {
    const ID: &'static str = "coqo.git";
    const VERSION: ServiceVersion = ServiceVersion::new(1, 0);
}

impl Git {
    /// The service, running `git` as `options` say.
    pub fn new(options: GitOptions) -> Self {
        Self { options }
    }

    /// The repository whose work tree contains `dir`.
    pub fn open(&self, dir: &Path) -> Result<Repo> {
        Repo::open(dir, self.options.clone())
    }
}

/// A repository with a work tree, opened from a folder in it.
pub struct Repo {
    repo: gix::Repository,
    workdir: PathBuf,
    prefix: String,
    options: GitOptions,
}

impl fmt::Debug for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Repo")
            .field("workdir", &self.workdir)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl Repo {
    fn open(dir: &Path, options: GitOptions) -> Result<Self> {
        let dir = dir
            .canonicalize()
            .map_err(|e| GitError::new(ErrorKind::NotFound, format!("{}: {e}", dir.display())))?;
        let repo = gix::discover(&dir).map_err(|e| {
            GitError::new(
                ErrorKind::NotARepository,
                format!("{} is not in a git repository: {e}", dir.display()),
            )
        })?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| {
                GitError::new(
                    ErrorKind::NotARepository,
                    format!("{} is in a bare repository", dir.display()),
                )
            })?
            .canonicalize()
            .map_err(GitError::other)?;
        let prefix = dir
            .strip_prefix(&workdir)
            .ok()
            .and_then(|p| p.to_str())
            .map(|p| p.replace(std::path::MAIN_SEPARATOR, "/"))
            .unwrap_or_default();
        Ok(Self {
            repo,
            workdir,
            prefix,
            options,
        })
    }

    /// The work tree's top folder (canonical).
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// The repository's git directory (`.git`, or the worktree's own under
    /// `.git/worktrees/` for a linked worktree).
    pub fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    /// The directory refs, objects and the stash live in, shared by linked
    /// worktrees (the same as [`git_dir`](Self::git_dir) otherwise).
    pub fn common_dir(&self) -> &Path {
        self.repo.common_dir()
    }

    /// Where the folder it was opened from is, relative to the work tree
    /// (`""` at the top).
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub(crate) fn gix(&self) -> &gix::Repository {
        &self.repo
    }
}

/// Check a repository-relative path: `/`-separated, relative, no `.` or
/// `..` components, no `.git`.
pub(crate) fn check_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c.eq_ignore_ascii_case(".git"));
    if bad {
        return Err(GitError::new(
            ErrorKind::Invalid,
            format!("not a repository-relative path: {path:?}"),
        ));
    }
    Ok(())
}

/// The mod: provides [`Git`] while active.
#[derive(Debug, Default)]
pub struct GitMod {
    options: GitOptions,
}

impl GitMod {
    /// The mod, running `git` as `options` say.
    pub fn new(options: GitOptions) -> Self {
        Self { options }
    }
}

impl Mod for GitMod {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move { cx.provide::<Git>(Arc::new(Git::new(self.options.clone()))) })
    }
}
