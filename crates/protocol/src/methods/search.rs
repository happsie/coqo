// SPDX-License-Identifier: Apache-2.0

//! `search/*`: project search and replace (PLAN §7.1, issue #45), from the
//! `coqo.search` mod.
//!
//! - [`Run`] (`search/run`) is a topic: subscribing starts a search of the
//!   workspace's roots and streams [`SearchEvent::Files`] batches as files
//!   are searched (in no fixed order), then one [`SearchEvent::Done`], and
//!   the subscription ends. Unsubscribing cancels the search, so a client
//!   whose query changed drops the old subscription and starts a new one.
//!   A bad query (a regular expression or glob that does not parse) fails
//!   the subscribe with [`INVALID_QUERY`](super::codes::INVALID_QUERY).
//! - [`Replace`] (`search/replace`) replaces matches of the same query in
//!   chosen files: all of a file's matches, or only those named by place.
//!   Open buffers are edited (the change shows in every editor and stays
//!   unsaved); files that are not open are written atomically. Files a
//!   client holds itself are returned as edits instead, for it to apply in
//!   its own editor so they join its undo history.
//!
//! **What is searched:** every root (or the chosen ones), with
//! `.gitignore`, `.ignore`, `.git/info/exclude` and the user's global
//! excludes respected; `.git`, symlinks, binary files and files over 16 MiB
//! are skipped; hidden files are searched. A file open in a buffer is
//! searched in its current text, unsaved edits included.
//!
//! **Positions:** lines are 0-based and `start`/`end` count characters on
//! the line, as editor positions do. A match never spans lines.
//!
//! **Access:** both take the daemon token only. Agents search with their
//! own tools.

use serde::{Deserialize, Serialize};

use super::buffer::{BufferId, TextEdit};
use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, Topic};

/// Default [`RunParams::max_matches`].
pub const DEFAULT_MAX_MATCHES: u32 = 20_000;
/// Largest [`RunParams::max_matches`].
pub const MAX_MATCHES: u32 = 100_000;

/// What to search for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SearchQuery {
    /// The text, or a regular expression if `regex`. Must not be empty.
    pub pattern: String,
    /// `pattern` is a regular expression (Rust `regex` syntax, as ripgrep);
    /// otherwise it is matched literally.
    #[serde(default)]
    pub regex: bool,
    /// Match case; otherwise case is ignored.
    #[serde(default)]
    pub case_sensitive: bool,
    /// Only matches with a word boundary at both ends.
    #[serde(default)]
    pub whole_word: bool,
    /// Only files matching one of these globs (gitignore syntax, relative to
    /// the root: `*.rs`, `src/**`; a plain name such as `src` also covers
    /// what is under it). Empty: every file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Never files or folders matching these globs (`target`, `*.min.js`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// Params of [`Run`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RunParams {
    /// The workspace to search.
    pub workspace: WorkspaceId,
    /// What to search for.
    pub query: SearchQuery,
    /// Only these roots; omitted: every root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<RootId>>,
    /// Stop after this many matches (default [`DEFAULT_MAX_MATCHES`], at most
    /// [`MAX_MATCHES`]); [`SearchSummary::limit_hit`] then says there are
    /// more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<u32>,
}

/// One match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SearchMatch {
    /// 0-based line.
    pub line: u32,
    /// 0-based character where it starts on the line.
    pub start: u32,
    /// 0-based character where it ends (exclusive), on the same line.
    pub end: u32,
    /// The line without its line ending and leading whitespace; a window
    /// around the match, with `…` where cut, if the line is long.
    pub preview: String,
    /// Where the match starts in `preview` (bytes).
    pub preview_start: u32,
    /// Where it ends in `preview` (bytes, exclusive).
    pub preview_end: u32,
}

impl SearchMatch {
    /// Its place, to name it in [`ReplaceFile::matches`].
    pub fn at(&self) -> MatchAt {
        MatchAt {
            line: self.line,
            start: self.start,
            end: self.end,
        }
    }
}

/// The matches of one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileMatches {
    /// The file's root.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// In order.
    pub matches: Vec<SearchMatch>,
}

/// How a search went: the last event of [`Run`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SearchSummary {
    /// Files with matches.
    pub files: u32,
    /// Matches.
    pub matches: u32,
    /// Files searched.
    pub searched: u32,
    /// It stopped at `max_matches`: there are more.
    pub limit_hit: bool,
    /// How long it took, in milliseconds.
    pub elapsed_ms: u64,
}

/// An event of [`Run`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SearchEvent {
    /// Files with matches, as they were searched.
    Files {
        /// Each with its matches.
        files: Vec<FileMatches>,
    },
    /// The search is over; the subscription ends after this.
    Done(SearchSummary),
}

/// `search/run`: search a workspace, streaming the results (see the module
/// docs). Unsubscribe to cancel.
#[derive(Debug)]
pub enum Run {}
impl Topic for Run {
    const NAME: &'static str = "search/run";
    type Params = RunParams;
    type Event = SearchEvent;
}

/// Where a match is: its line and character columns, as
/// [`SearchMatch`] gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct MatchAt {
    /// 0-based line.
    pub line: u32,
    /// 0-based start character.
    pub start: u32,
    /// 0-based end character (exclusive).
    pub end: u32,
}

/// A file to replace in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ReplaceFile {
    /// The file's root.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// Only the matches at these places; omitted: every match in the file.
    /// A place where the current text no longer has a match (it changed
    /// since the search) is skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<Vec<MatchAt>>,
}

/// Params of [`Replace`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ReplaceParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The search whose matches to replace.
    pub query: SearchQuery,
    /// What each match becomes. For a regular expression it may name
    /// capture groups (`$1`, `${name}`; `$$` is a `$`); otherwise it is
    /// literal.
    pub replacement: String,
    /// The files.
    pub files: Vec<ReplaceFile>,
    /// Open buffers the client edits itself (its editor tabs, so the
    /// replacement joins their undo history): their edits come back as
    /// [`ReplaceOutcome::Held`] instead of being applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hold: Vec<BufferId>,
}

/// What happened to one file of a [`Replace`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ReplaceOutcome {
    /// Its open buffer was edited (one change, unsaved).
    Buffer,
    /// It was not open; the file was written.
    Written,
    /// Its buffer is one the client holds: apply `edits` at `rev`.
    Held,
    /// Nothing to replace (no match at the places named, any more).
    Unchanged,
    /// It could not be done; `error` says why.
    Failed,
}

/// One file of a [`ReplaceResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileReplaced {
    /// The file's root.
    pub root: RootId,
    /// Its path, as given.
    pub path: String,
    /// What happened.
    pub outcome: ReplaceOutcome,
    /// Matches replaced (or to replace, when held).
    pub replaced: u32,
    /// Its open buffer, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer: Option<BufferId>,
    /// The buffer's revision after the change (`buffer`), or the one the
    /// edits apply to (`held`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
    /// For `held` only: the edits, in order, against the buffer at `rev`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<TextEdit>,
    /// For `failed`: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of [`Replace`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ReplaceResult {
    /// One entry per file asked for, in order.
    pub files: Vec<FileReplaced>,
    /// Matches replaced in all (held ones included).
    pub replaced: u32,
}

/// `search/replace`: replace a search's matches in chosen files (see the
/// module docs).
#[derive(Debug)]
pub enum Replace {}
impl Method for Replace {
    const NAME: &'static str = "search/replace";
    type Params = ReplaceParams;
    type Result = ReplaceResult;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_tagged() {
        let done = SearchEvent::Done(SearchSummary {
            files: 1,
            ..SearchSummary::default()
        });
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["kind"], "done");
        assert_eq!(v["files"], 1);
        let back: SearchEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, done);
        let files = SearchEvent::Files { files: Vec::new() };
        assert_eq!(serde_json::to_value(&files).unwrap()["kind"], "files");
    }

    #[test]
    fn a_bare_query_parses() {
        let q: SearchQuery = serde_json::from_value(serde_json::json!({ "pattern": "x" })).unwrap();
        assert_eq!(q.pattern, "x");
        assert!(!q.regex && !q.case_sensitive && !q.whole_word && q.include.is_empty());
    }
}
