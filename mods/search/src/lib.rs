// SPDX-License-Identifier: Apache-2.0

//! `coqo.search`: project search and replace (PLAN §6 `search`, §7.1,
//! issue #45).
//!
//! The mod provides the [`Search`] service (`coqo.search@1`); the daemon's
//! `search/*` methods resolve it on every call, so disabling the mod turns
//! them off and another mod can replace it.
//!
//! - [`Search::compile`] checks a [`Query`] (pattern, regex or literal,
//!   case, whole word, include and exclude globs) once and gives a
//!   [`Matcher`].
//! - [`Matcher::search`] walks folders in parallel with ripgrep's crates:
//!   `.gitignore`, `.ignore` and git's excludes are respected (in and out of
//!   a repository), `.git` is skipped, hidden files are searched, binary
//!   files are not. Files whose text the caller already has (open buffers
//!   with unsaved edits) are searched in that text instead of on disk.
//!   Results go to a sink one file at a time, as soon as each file is done.
//! - [`Matcher::find`] and [`Matcher::replace`] work on one text, with the
//!   same matcher, so a replace changes exactly what the search showed.
//!
//! **Positions:** lines are 0-based and columns count characters (Unicode
//! scalar values), as editor positions do. A match never spans lines. Empty
//! matches (`^`, `x*`) are not reported. Everything here is blocking; the
//! daemon calls it on blocking threads.

mod matcher;
mod walk;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use coqo_mod_api::{BoxFuture, Manifest, Mod, ModContext, ModError, Service, ServiceVersion};

pub use matcher::{Match, MatchAt, Matcher, Replacement};
pub use walk::{FileMatches, Overlay, Root, Summary};

/// The mod's `coqo.mod.toml`.
pub const MANIFEST: &str = include_str!("../coqo.mod.toml");

/// The parsed, validated [`MANIFEST`].
pub fn manifest() -> Manifest {
    Manifest::from_toml_str(MANIFEST).expect("coqo.search's manifest is valid")
}

/// Longest preview of a matching line, in bytes. Longer lines show a window
/// around the match, with `…` where they were cut.
pub const MAX_PREVIEW: usize = 240;

/// Files larger than this are not searched.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// What to search for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// The text, or a regular expression if [`regex`](Self::regex).
    pub pattern: String,
    /// The pattern is a regular expression (Rust `regex` syntax, as
    /// ripgrep). Otherwise it is matched literally.
    pub regex: bool,
    /// Match case. Otherwise case is ignored (Unicode-aware).
    pub case_sensitive: bool,
    /// Only matches with a word boundary at both ends.
    pub whole_word: bool,
    /// Only files matching one of these globs (gitignore syntax, relative to
    /// each folder searched: `*.rs`, `src/**`). Empty: every file.
    pub include: Vec<String>,
    /// Never files or folders matching these globs (`target`, `*.min.js`).
    pub exclude: Vec<String>,
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The pattern or a glob does not parse, or the pattern is empty.
    InvalidQuery,
    /// A folder could not be searched (it does not exist, say).
    Io,
}

/// An error of a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchError {
    /// What went wrong.
    pub kind: ErrorKind,
    /// For people.
    pub message: String,
}

impl SearchError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidQuery, message)
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SearchError {}

/// The result of a search operation.
pub type Result<T, E = SearchError> = std::result::Result<T, E>;

/// The `coqo.search@1` service.
#[derive(Debug, Default)]
pub struct Search;

impl Service for Search {
    const ID: &'static str = "coqo.search";
    const VERSION: ServiceVersion = ServiceVersion::new(1, 0);
}

impl Search {
    /// Check `query` and build its matcher. Fails with
    /// [`ErrorKind::InvalidQuery`] for an empty pattern, a regular
    /// expression that does not parse (or can match a line break), or a
    /// glob that does not.
    pub fn compile(&self, query: &Query) -> Result<Matcher> {
        Matcher::new(query.clone())
    }
}

/// The mod: provides [`Search`] while active.
#[derive(Debug, Default)]
pub struct SearchMod;

impl SearchMod {
    /// The mod.
    pub fn new() -> Self {
        Self
    }
}

impl Mod for SearchMod {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move { cx.provide::<Search>(Arc::new(Search)) })
    }
}
