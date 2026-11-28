// SPDX-License-Identifier: Apache-2.0

//! Diffs between two sides (work tree, index, any revision): one file's
//! hunks, and the list of files that differ.
//!
//! Line diffs are Myers with git's indent heuristic (gix's imara-diff), and
//! hunks are grouped with context the way `git diff` groups them, with the
//! same `@@` section headings (git's default function-name rule), so the
//! output matches `git diff` line for line.
//!
//! Files are read as `git diff` reads them ([`Reader`]): a work-tree file is
//! cleaned the way `git add` would store it (`.gitattributes` `text`/`eol`,
//! `core.autocrlf`, `ident`, `filter` drivers, `working-tree-encoding`),
//! and a file is binary when its `diff` attribute is unset (`-diff`, or the
//! `binary` macro), when its diff driver says `binary = true`, or else when
//! it has a NUL in its first 8000 bytes (a set `diff` attribute makes it
//! text). Text conversion (`textconv`) is not run. Deleted and added files
//! that are similar enough pair up as renames ([`crate::rename`]).

use std::collections::{BTreeMap, BTreeSet};

use gix::bstr::{BStr, ByteSlice as _};
use gix::diff::blob::{Algorithm, Diff, InternedInput};

use crate::status::Change;
use crate::{ErrorKind, GitError, Repo, Result, check_path, rename};

/// Lines of context around changes, as `git diff`.
pub const DEFAULT_CONTEXT: u32 = 3;

/// Bytes git looks at to decide that a file is binary.
const BINARY_SNIFF: usize = 8000;

/// Longest `@@` section heading, in bytes (git's buffer).
const SECTION_MAX: usize = 80;

/// One side of a comparison.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// The files on disk.
    Worktree,
    /// The index (what the next commit gets).
    Index,
    /// A revision: `HEAD`, a branch, a commit id, `main~2`, …
    Rev(String),
}

/// A file as one side has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSide {
    /// Git's mode: `0o100644`, `0o100755`, `0o120000` (symlink) or
    /// `0o160000` (submodule).
    pub mode: u32,
    /// The blob id; `None` for a work-tree file.
    pub oid: Option<String>,
}

/// What a diff line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// On both sides.
    Context,
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Removed,
}

/// One line of a hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    /// What it is.
    pub kind: LineKind,
    /// 1-based line number on the old side (context and removed lines).
    pub old_line: Option<u32>,
    /// 1-based line number on the new side (context and added lines).
    pub new_line: Option<u32>,
    /// The text, without its line break (lossy UTF-8).
    pub text: String,
    /// Whether the line ends in a line break (only the last line of a file
    /// may not: git's "\ No newline at end of file").
    pub newline: bool,
}

/// A run of changes with context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// `@@ -old_start,old_lines …`: the first old line (the line before,
    /// when `old_lines` is 0).
    pub old_start: u32,
    /// Old lines in the hunk (context and removed).
    pub old_lines: u32,
    /// `… +new_start,new_lines @@`.
    pub new_start: u32,
    /// New lines in the hunk (context and added).
    pub new_lines: u32,
    /// The heading after the `@@` (the enclosing function, roughly).
    pub section: Option<String>,
    /// The lines.
    pub lines: Vec<DiffLine>,
}

/// One file's diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// Repository-relative: the new side's path.
    pub path: String,
    /// The old side's path, when it differs (a rename).
    pub orig_path: Option<String>,
    /// The old side's file; `None` if it has none (an added file).
    pub old: Option<FileSide>,
    /// The new side's file; `None` if it has none (a deleted file).
    pub new: Option<FileSide>,
    /// Either side is binary: no hunks.
    pub binary: bool,
    /// The changes; empty when the contents are equal (only the mode
    /// changed, say).
    pub hunks: Vec<Hunk>,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
}

impl FileDiff {
    /// The hunks as `git diff` prints them (no file headers).
    pub fn to_unified(&self) -> String {
        let mut out = String::new();
        for h in &self.hunks {
            out.push_str(&hunk_header(h));
            out.push('\n');
            for l in &h.lines {
                out.push(match l.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                });
                out.push_str(&l.text);
                out.push('\n');
                if !l.newline {
                    out.push_str("\\ No newline at end of file\n");
                }
            }
        }
        out
    }
}

/// `@@ -a,b +c,d @@ section`, with git's `,1` elision.
pub(crate) fn hunk_header(h: &Hunk) -> String {
    let range = |start: u32, len: u32| {
        if len == 1 {
            start.to_string()
        } else {
            format!("{start},{len}")
        }
    };
    let mut s = format!(
        "@@ -{} +{} @@",
        range(h.old_start, h.old_lines),
        range(h.new_start, h.new_lines)
    );
    if let Some(section) = &h.section {
        s.push(' ');
        s.push_str(section);
    }
    s
}

/// A file that differs between two sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    /// Repository-relative: the new side's path.
    pub path: String,
    /// [`Change::Added`], [`Modified`](Change::Modified),
    /// [`Deleted`](Change::Deleted), [`TypeChanged`](Change::TypeChanged)
    /// or [`Renamed`](Change::Renamed).
    pub change: Change,
    /// The old side's path of a rename.
    pub orig_path: Option<String>,
    /// Added lines (0 for a binary file).
    pub additions: u32,
    /// Removed lines (0 for a binary file).
    pub deletions: u32,
    /// Either side is binary.
    pub binary: bool,
}

/// A file's bytes and mode on one side.
#[derive(Debug)]
pub(crate) struct Blob {
    pub data: Vec<u8>,
    pub mode: u32,
    pub oid: Option<gix::ObjectId>,
}

/// A hunk over raw bytes, for building patches.
#[derive(Clone, Debug)]
pub(crate) struct RawHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub section: Option<Vec<u8>>,
    /// `(kind, old line, new line, bytes without the line break, newline)`.
    pub lines: Vec<RawLine>,
}

#[derive(Clone, Debug)]
pub(crate) struct RawLine {
    pub kind: LineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: Vec<u8>,
    pub newline: bool,
}

/// git's content check: a NUL in the first 8000 bytes.
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF)].contains(&0)
}

/// What `.gitattributes` says about diffing a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffAttr {
    /// Decide from the contents.
    Auto,
    /// `diff` is set: always text.
    Text,
    /// `-diff`, `binary`, or a driver with `binary = true`.
    Binary,
}

/// Reads files from the sides of a comparison as `git diff` does: work-tree
/// files through the clean filters, the index without intent-to-add
/// entries, and binary-ness from `.gitattributes`. One per operation: it
/// keeps the index, the filter pipeline and the attribute stack it loads.
pub(crate) struct Reader<'r> {
    repo: &'r Repo,
    index: Option<gix::worktree::Index>,
    filter: Option<(
        gix::filter::Pipeline<'r>,
        gix::worktree::IndexPersistedOrInMemory,
    )>,
    attrs: Option<(gix::AttributeStack<'r>, gix::attrs::search::Outcome)>,
}

impl<'r> Reader<'r> {
    pub(crate) fn new(repo: &'r Repo) -> Self {
        Self {
            repo,
            index: None,
            filter: None,
            attrs: None,
        }
    }

    fn index(&mut self) -> Result<&gix::worktree::Index> {
        if self.index.is_none() {
            self.index = Some(self.repo.index()?);
        }
        Ok(self.index.as_ref().expect("loaded above"))
    }

    /// A file's bytes and mode on `side`; `None` if the side has no such
    /// file (an intent-to-add entry is not in the index yet).
    pub(crate) fn read(&mut self, side: &Side, path: &str) -> Result<Option<Blob>> {
        let repo = self.repo.gix();
        let blob = |oid: gix::ObjectId, mode: u32| -> Result<Option<Blob>> {
            if mode & 0o170000 == 0o160000 {
                // A submodule: its "contents" are the commit id, as git
                // shows it.
                return Ok(Some(Blob {
                    data: format!("Subproject commit {oid}\n").into_bytes(),
                    mode,
                    oid: Some(oid),
                }));
            }
            let data = repo.find_blob(oid).map_err(GitError::other)?.take_data();
            Ok(Some(Blob {
                data,
                mode,
                oid: Some(oid),
            }))
        };
        match side {
            Side::Worktree => {
                let Some(mut file) = read_worktree(&self.repo.workdir().join(path))? else {
                    return Ok(None);
                };
                if file.mode != 0o120000 {
                    file.data = self.clean(path, file.data)?;
                }
                Ok(Some(file))
            }
            Side::Index => {
                let found = self.index()?.entry_by_path(BStr::new(path)).and_then(|e| {
                    (e.stage_raw() == 0 && !is_intent_to_add(e)).then(|| (e.id, e.mode.bits()))
                });
                match found {
                    Some((id, mode)) => blob(id, mode),
                    None => Ok(None),
                }
            }
            Side::Rev(rev) => {
                let tree = self.repo.tree(rev)?;
                match tree.lookup_entry_by_path(path).map_err(GitError::other)? {
                    Some(e) if !e.mode().is_tree() => {
                        blob(e.object_id(), u32::from(e.mode().value()))
                    }
                    _ => Ok(None),
                }
            }
        }
    }

    /// `data` (the work-tree file at `path`) as `git add` would store it.
    fn clean(&mut self, path: &str, data: Vec<u8>) -> Result<Vec<u8>> {
        use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
        use std::io::Read as _;
        if self.filter.is_none() {
            self.filter = Some(
                self.repo
                    .gix()
                    .filter_pipeline(None)
                    .map_err(GitError::other)?,
            );
        }
        let (pipeline, index) = self.filter.as_mut().expect("made above");
        let outcome = pipeline
            .convert_to_git(data.as_slice(), std::path::Path::new(path), index)
            .map_err(|e| GitError::other(format!("{path}: {e}")))?;
        let cleaned = match outcome {
            ToGitOutcome::Unchanged(_) => None,
            ToGitOutcome::Buffer(buf) => Some(buf.to_vec()),
            ToGitOutcome::Process(mut read) => {
                let mut buf = Vec::new();
                read.read_to_end(&mut buf)
                    .map_err(|e| GitError::other(format!("{path}: {e}")))?;
                Some(buf)
            }
        };
        Ok(cleaned.unwrap_or(data))
    }

    /// What the `diff` attribute (and the driver it names) says of `path`.
    fn diff_attr(&mut self, path: &str) -> Result<DiffAttr> {
        if self.attrs.is_none() {
            let index = self.repo.index()?;
            let stack = self
                .repo
                .gix()
                .attributes_only(
                    &index,
                    gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
                )
                .map_err(GitError::other)?;
            let outcome = stack.selected_attribute_matches(["diff"]);
            self.attrs = Some((stack, outcome));
        }
        let (stack, outcome) = self.attrs.as_mut().expect("made above");
        outcome.reset();
        stack
            .at_path(std::path::Path::new(path), None)
            .map_err(GitError::other)?
            .matching_attributes(outcome);
        let state = outcome
            .iter_selected()
            .next()
            .map(|m| m.assignment.state.to_owned());
        Ok(match state {
            Some(gix::attrs::State::Set) => DiffAttr::Text,
            Some(gix::attrs::State::Unset) => DiffAttr::Binary,
            Some(gix::attrs::State::Value(driver)) => {
                let key = format!("diff.{}.binary", driver.as_ref().as_bstr());
                match self.repo.gix().config_snapshot().boolean(key.as_str()) {
                    Some(true) => DiffAttr::Binary,
                    Some(false) => DiffAttr::Text,
                    None => DiffAttr::Auto,
                }
            }
            Some(gix::attrs::State::Unspecified) | None => DiffAttr::Auto,
        })
    }

    /// Whether `data`, the contents of `path`, diffs as binary.
    pub(crate) fn is_binary(&mut self, path: &str, data: &[u8]) -> Result<bool> {
        Ok(match self.diff_attr(path)? {
            DiffAttr::Text => false,
            DiffAttr::Binary => true,
            DiffAttr::Auto => is_binary(data),
        })
    }

    /// Whether the diff of `old` (at `old_path`) and `new` (at `path`) is
    /// binary: either side is.
    pub(crate) fn either_binary(
        &mut self,
        old_path: &str,
        old: Option<&Blob>,
        path: &str,
        new: Option<&Blob>,
    ) -> Result<bool> {
        if let Some(b) = old
            && self.is_binary(old_path, &b.data)?
        {
            return Ok(true);
        }
        match new {
            Some(b) => self.is_binary(path, &b.data),
            None => Ok(false),
        }
    }
}

fn is_intent_to_add(e: &gix::index::Entry) -> bool {
    e.flags.contains(gix::index::entry::Flags::INTENT_TO_ADD)
}

/// Lines with their line breaks.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|&b| b == b'\n').collect()
}

fn strip_newline(line: &[u8]) -> (&[u8], bool) {
    match line.strip_suffix(b"\n") {
        Some(l) => (l, true),
        None => (line, false),
    }
}

/// git's default section heading: the nearest line at or before `from`
/// (0-based, in the old file) that starts with a letter, `_` or `$`.
fn section(old: &[&[u8]], from: Option<usize>) -> Option<Vec<u8>> {
    let from = from?;
    old.iter().take(from + 1).rev().find_map(|line| {
        let first = *line.first()?;
        if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
            return None;
        }
        let mut l = &line[..line.len().min(SECTION_MAX)];
        while let Some((last, rest)) = l.split_last() {
            if last.is_ascii_whitespace() {
                l = rest;
            } else {
                break;
            }
        }
        Some(l.to_vec())
    })
}

/// Diff `old` against `new` with `context` lines around each change.
pub(crate) fn compute(old: &[u8], new: &[u8], context: u32) -> Vec<RawHunk> {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_lines(&input);
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let changes: Vec<_> = diff.hunks().collect();
    let ctx = context;
    let mut groups: Vec<Vec<gix::diff::blob::Hunk>> = Vec::new();
    for h in changes {
        match groups.last_mut() {
            Some(g) if h.before.start - g.last().map_or(0, |l| l.before.end) <= 2 * ctx => {
                g.push(h);
            }
            _ => groups.push(vec![h]),
        }
    }
    let n_old = old_lines.len() as u32;
    groups
        .into_iter()
        .map(|g| {
            let (first, last) = (&g[0], &g[g.len() - 1]);
            let old_from = first.before.start.saturating_sub(ctx);
            let old_to = (last.before.end + ctx).min(n_old);
            let new_from = first.after.start - (first.before.start - old_from);
            let mut lines = Vec::new();
            let (mut o, mut n) = (old_from, new_from);
            let push_ctx = |o: &mut u32, n: &mut u32, until: u32, lines: &mut Vec<RawLine>| {
                while *o < until {
                    let (text, newline) = strip_newline(old_lines[*o as usize]);
                    lines.push(RawLine {
                        kind: LineKind::Context,
                        old_line: Some(*o + 1),
                        new_line: Some(*n + 1),
                        text: text.to_vec(),
                        newline,
                    });
                    *o += 1;
                    *n += 1;
                }
            };
            for h in &g {
                push_ctx(&mut o, &mut n, h.before.start, &mut lines);
                for i in h.before.clone() {
                    let (text, newline) = strip_newline(old_lines[i as usize]);
                    lines.push(RawLine {
                        kind: LineKind::Removed,
                        old_line: Some(i + 1),
                        new_line: None,
                        text: text.to_vec(),
                        newline,
                    });
                }
                for i in h.after.clone() {
                    let (text, newline) = strip_newline(new_lines[i as usize]);
                    lines.push(RawLine {
                        kind: LineKind::Added,
                        old_line: None,
                        new_line: Some(i + 1),
                        text: text.to_vec(),
                        newline,
                    });
                }
                o = h.before.end;
                n = h.after.end;
            }
            push_ctx(&mut o, &mut n, old_to, &mut lines);
            let old_count = old_to - old_from;
            let new_count = n - new_from;
            RawHunk {
                old_start: if old_count == 0 {
                    old_from
                } else {
                    old_from + 1
                },
                old_lines: old_count,
                new_start: if new_count == 0 {
                    new_from
                } else {
                    new_from + 1
                },
                new_lines: new_count,
                section: section(&old_lines, old_from.checked_sub(1).map(|i| i as usize)),
                lines,
            }
        })
        .collect()
}

fn public_hunk(h: RawHunk) -> Hunk {
    Hunk {
        old_start: h.old_start,
        old_lines: h.old_lines,
        new_start: h.new_start,
        new_lines: h.new_lines,
        section: h.section.map(|s| s.to_str_lossy().into_owned()),
        lines: h
            .lines
            .into_iter()
            .map(|l| DiffLine {
                kind: l.kind,
                old_line: l.old_line,
                new_line: l.new_line,
                text: l.text.to_str_lossy().into_owned(),
                newline: l.newline,
            })
            .collect(),
    }
}

/// `(oid, mode)` of every file on a side; `oid` is `None` for a work-tree
/// file that differs from the index (its contents decide).
type Listing = BTreeMap<String, (Option<gix::ObjectId>, u32)>;

impl Repo {
    pub(crate) fn tree(&self, rev: &str) -> Result<gix::Tree<'_>> {
        let repo = self.gix();
        let id = repo.rev_parse_single(rev).map_err(|e| {
            GitError::new(
                ErrorKind::NotFound,
                format!("unknown revision {rev:?}: {e}"),
            )
        })?;
        id.object()
            .and_then(|o| o.peel_to_tree())
            .map_err(|e| GitError::new(ErrorKind::NotFound, format!("{rev:?} has no tree: {e}")))
    }

    pub(crate) fn index(&self) -> Result<gix::worktree::Index> {
        self.gix().index_or_empty().map_err(GitError::other)
    }

    /// A reader of the sides, for one operation.
    pub(crate) fn reader(&self) -> Reader<'_> {
        Reader::new(self)
    }

    /// A file's contents on `side` (repository-relative `path`), as its diff
    /// shows them (a work-tree file cleaned as `git add` would store it);
    /// `None` if the side has no such file. For anchoring review comments to
    /// the lines a diff side shows.
    pub fn read_file(&self, side: &Side, path: &str) -> Result<Option<Vec<u8>>> {
        check_path(path)?;
        Ok(self.reader().read(side, path)?.map(|b| b.data))
    }

    /// The diff of one file from `base` to `head`, with `context` lines
    /// around changes (`git diff -U<context>`).
    pub fn diff(&self, path: &str, base: &Side, head: &Side, context: u32) -> Result<FileDiff> {
        self.diff_paths(path, path, base, head, context)
    }

    /// The diff of a file that is at `orig_path` on `base` and at `path` on
    /// `head` (a rename, as [`ChangedFile::orig_path`] or
    /// [`StatusEntry::orig_path`](crate::StatusEntry::orig_path) give it),
    /// with `context` lines around changes.
    pub fn diff_paths(
        &self,
        orig_path: &str,
        path: &str,
        base: &Side,
        head: &Side,
        context: u32,
    ) -> Result<FileDiff> {
        check_path(orig_path)?;
        check_path(path)?;
        let mut reader = self.reader();
        let old = reader.read(base, orig_path)?;
        let new = reader.read(head, path)?;
        let binary = reader.either_binary(orig_path, old.as_ref(), path, new.as_ref())?;
        let mut d = file_diff(path, old, new, context, binary);
        if orig_path != path {
            d.orig_path = Some(orig_path.to_owned());
        }
        Ok(d)
    }

    /// Files that differ from `base` to `head` (`git diff -M --numstat`),
    /// sorted by path, with renames paired. Untracked files are not on the
    /// work-tree side, as in `git diff`.
    pub fn changed_files(&self, base: &Side, head: &Side) -> Result<Vec<ChangedFile>> {
        let old = self.listing(base)?;
        let new = self.listing(head)?;
        let paths: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let mut reader = self.reader();
        let mut out = Vec::new();
        let mut deleted: Vec<(String, Blob)> = Vec::new();
        let mut added: Vec<(String, Blob)> = Vec::new();
        for path in paths {
            let (o, n) = (old.get(path), new.get(path));
            if let (Some(o), Some(n)) = (o, n)
                && o.0.is_some()
                && o == n
            {
                continue;
            }
            let a = if o.is_some() {
                reader.read(base, path)?
            } else {
                None
            };
            let b = if n.is_some() {
                reader.read(head, path)?
            } else {
                None
            };
            match (a, b) {
                (None, None) => {}
                (None, Some(b)) => added.push((path.clone(), b)),
                (Some(a), None) => deleted.push((path.clone(), a)),
                (Some(a), Some(b)) => {
                    if a.mode == b.mode && a.data == b.data {
                        continue;
                    }
                    let change = if (a.mode & 0o170000) != (b.mode & 0o170000) {
                        Change::TypeChanged
                    } else {
                        Change::Modified
                    };
                    out.push(changed(&mut reader, path, path, change, Some(a), Some(b))?);
                }
            }
        }
        let pairs = renames(&mut reader, &deleted, &added)?;
        let mut deleted: Vec<Option<(String, Blob)>> = deleted.into_iter().map(Some).collect();
        let mut added: Vec<Option<(String, Blob)>> = added.into_iter().map(Some).collect();
        for p in pairs {
            if let (Some((from, a)), Some((to, b))) =
                (deleted[p.source].take(), added[p.dest].take())
            {
                out.push(changed(
                    &mut reader,
                    &from,
                    &to,
                    Change::Renamed,
                    Some(a),
                    Some(b),
                )?);
            }
        }
        for (path, a) in deleted.into_iter().flatten() {
            out.push(changed(
                &mut reader,
                &path,
                &path,
                Change::Deleted,
                Some(a),
                None,
            )?);
        }
        for (path, b) in added.into_iter().flatten() {
            out.push(changed(
                &mut reader,
                &path,
                &path,
                Change::Added,
                None,
                Some(b),
            )?);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn listing(&self, side: &Side) -> Result<Listing> {
        match side {
            Side::Rev(rev) => {
                let tree = self.tree(rev)?;
                let files = tree
                    .traverse()
                    .breadthfirst
                    .files()
                    .map_err(GitError::other)?;
                Ok(files
                    .into_iter()
                    .filter(|e| !e.mode.is_tree())
                    .map(|e| {
                        (
                            e.filepath.to_str_lossy().into_owned(),
                            (Some(e.oid), u32::from(e.mode.value())),
                        )
                    })
                    .collect())
            }
            Side::Index => {
                let index = self.index()?;
                Ok(index
                    .entries()
                    .iter()
                    .filter(|e| e.stage_raw() == 0 && !is_intent_to_add(e))
                    .map(|e| {
                        (
                            e.path(&index).to_str_lossy().into_owned(),
                            (Some(e.id), e.mode.bits()),
                        )
                    })
                    .collect())
            }
            Side::Worktree => {
                let mut listing = self.listing(&Side::Index)?;
                for e in self.status()?.entries {
                    if e.conflict.is_some() {
                        listing.insert(e.path, (None, 0));
                        continue;
                    }
                    match e.unstaged {
                        Some(Change::Deleted) => {
                            listing.remove(&e.path);
                        }
                        Some(Change::Renamed) => {
                            if let Some(orig) = &e.orig_path {
                                listing.remove(orig);
                            }
                            listing.insert(e.path, (None, 0));
                        }
                        Some(Change::Modified | Change::TypeChanged | Change::Added) => {
                            listing.insert(e.path, (None, 0));
                        }
                        Some(Change::Untracked) | None => {}
                    }
                }
                Ok(listing)
            }
        }
    }
}

/// A changed file, its lines counted.
fn changed(
    reader: &mut Reader<'_>,
    orig_path: &str,
    path: &str,
    change: Change,
    a: Option<Blob>,
    b: Option<Blob>,
) -> Result<ChangedFile> {
    let binary = reader.either_binary(orig_path, a.as_ref(), path, b.as_ref())?;
    let d = file_diff(path, a, b, 0, binary);
    Ok(ChangedFile {
        path: path.to_owned(),
        change,
        orig_path: (orig_path != path).then(|| orig_path.to_owned()),
        additions: d.additions,
        deletions: d.deletions,
        binary: d.binary,
    })
}

/// The renames among `deleted` and `added` files.
pub(crate) fn renames(
    reader: &mut Reader<'_>,
    deleted: &[(String, Blob)],
    added: &[(String, Blob)],
) -> Result<Vec<rename::Pair>> {
    if deleted.is_empty() || added.is_empty() {
        return Ok(Vec::new());
    }
    let mut sources = Vec::with_capacity(deleted.len());
    for (path, blob) in deleted {
        sources.push(candidate(reader, path, blob)?);
    }
    let mut dests = Vec::with_capacity(added.len());
    for (path, blob) in added {
        dests.push(candidate(reader, path, blob)?);
    }
    Ok(rename::find(&sources, &dests))
}

fn candidate<'a>(
    reader: &mut Reader<'_>,
    path: &'a str,
    blob: &'a Blob,
) -> Result<rename::Candidate<'a>> {
    Ok(rename::Candidate {
        path,
        data: &blob.data,
        mode: blob.mode,
        text: !reader.is_binary(path, &blob.data)?,
    })
}

/// The work-tree file at `path`: a symlink's target, or a file's bytes.
fn read_worktree(path: &std::path::Path) -> Result<Option<Blob>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(20) => return Ok(None), // ENOTDIR
        Err(e) => return Err(GitError::other(format!("{}: {e}", path.display()))),
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(GitError::other)?;
        let data = gix::path::os_str_into_bstr(target.as_os_str())
            .map_err(GitError::other)?
            .to_vec();
        return Ok(Some(Blob {
            data,
            mode: 0o120000,
            oid: None,
        }));
    }
    if !meta.is_file() {
        return Ok(None);
    }
    let data = std::fs::read(path).map_err(GitError::other)?;
    Ok(Some(Blob {
        data,
        mode: if executable(&meta) {
            0o100755
        } else {
            0o100644
        },
        oid: None,
    }))
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

/// The diff of `old` to `new` (at `path`); `binary` as the [`Reader`]
/// decided.
pub(crate) fn file_diff(
    path: &str,
    old: Option<Blob>,
    new: Option<Blob>,
    context: u32,
    binary: bool,
) -> FileDiff {
    let side = |b: &Option<Blob>| {
        b.as_ref().map(|b| FileSide {
            mode: b.mode,
            oid: b.oid.map(|o| o.to_string()),
        })
    };
    let (o, n) = (side(&old), side(&new));
    let empty: &[u8] = &[];
    let a = old.as_ref().map_or(empty, |b| b.data.as_slice());
    let b = new.as_ref().map_or(empty, |b| b.data.as_slice());
    let hunks: Vec<Hunk> = if binary {
        Vec::new()
    } else {
        compute(a, b, context)
            .into_iter()
            .map(public_hunk)
            .collect()
    };
    let count = |k: LineKind| {
        hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == k)
            .count() as u32
    };
    FileDiff {
        path: path.to_owned(),
        orig_path: None,
        old: o,
        new: n,
        binary,
        additions: count(LineKind::Added),
        deletions: count(LineKind::Removed),
        hunks,
    }
}
