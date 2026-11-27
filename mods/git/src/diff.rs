// SPDX-License-Identifier: Apache-2.0

//! Diffs between two sides (work tree, index, any revision): one file's
//! hunks, and the list of files that differ.
//!
//! Line diffs are Myers with git's indent heuristic (gix's imara-diff), and
//! hunks are grouped with context the way `git diff` groups them, with the
//! same `@@` section headings (git's default function-name rule), so the
//! output matches `git diff` line for line.

use std::collections::{BTreeMap, BTreeSet};

use gix::bstr::{BStr, ByteSlice as _};
use gix::diff::blob::{Algorithm, Diff, InternedInput};

use crate::status::Change;
use crate::{ErrorKind, GitError, Repo, Result, check_path};

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
    /// Repository-relative.
    pub path: String,
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
    /// Repository-relative.
    pub path: String,
    /// [`Change::Added`], [`Modified`](Change::Modified),
    /// [`Deleted`](Change::Deleted) or [`TypeChanged`](Change::TypeChanged).
    pub change: Change,
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

pub(crate) fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF)].contains(&0)
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
    fn tree(&self, rev: &str) -> Result<gix::Tree<'_>> {
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

    fn index(&self) -> Result<gix::worktree::Index> {
        self.gix().index_or_empty().map_err(GitError::other)
    }

    /// A file's bytes and mode on `side`; `None` if the side has no such
    /// file.
    pub(crate) fn read_side(&self, side: &Side, path: &str) -> Result<Option<Blob>> {
        let repo = self.gix();
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
            Side::Worktree => read_worktree(&self.workdir().join(path)),
            Side::Index => {
                let index = self.index()?;
                match index.entry_by_path(BStr::new(path)) {
                    Some(e) if e.stage_raw() == 0 => blob(e.id, e.mode.bits()),
                    _ => Ok(None),
                }
            }
            Side::Rev(rev) => {
                let tree = self.tree(rev)?;
                match tree.lookup_entry_by_path(path).map_err(GitError::other)? {
                    Some(e) if !e.mode().is_tree() => {
                        blob(e.object_id(), u32::from(e.mode().value()))
                    }
                    _ => Ok(None),
                }
            }
        }
    }

    /// A file's contents on `side` (repository-relative `path`); `None` if
    /// the side has no such file. For anchoring review comments to the
    /// lines a diff side shows.
    pub fn read_file(&self, side: &Side, path: &str) -> Result<Option<Vec<u8>>> {
        check_path(path)?;
        Ok(self.read_side(side, path)?.map(|b| b.data))
    }

    /// The diff of one file from `base` to `head`, with `context` lines
    /// around changes (`git diff -U<context>`).
    pub fn diff(&self, path: &str, base: &Side, head: &Side, context: u32) -> Result<FileDiff> {
        check_path(path)?;
        let old = self.read_side(base, path)?;
        let new = self.read_side(head, path)?;
        Ok(file_diff(path, old, new, context))
    }

    /// Files that differ from `base` to `head` (`git diff --numstat`),
    /// sorted by path. Untracked files are not on the work-tree side, as in
    /// `git diff`.
    pub fn changed_files(&self, base: &Side, head: &Side) -> Result<Vec<ChangedFile>> {
        let old = self.listing(base)?;
        let new = self.listing(head)?;
        let paths: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let mut out = Vec::new();
        for path in paths {
            let (o, n) = (old.get(path), new.get(path));
            if let (Some(o), Some(n)) = (o, n)
                && o.0.is_some()
                && o == n
            {
                continue;
            }
            let a = if o.is_some() {
                self.read_side(base, path)?
            } else {
                None
            };
            let b = if n.is_some() {
                self.read_side(head, path)?
            } else {
                None
            };
            let change = match (&a, &b) {
                (None, None) => continue,
                (None, Some(_)) => Change::Added,
                (Some(_), None) => Change::Deleted,
                (Some(a), Some(b)) => {
                    if a.mode == b.mode && a.data == b.data {
                        continue;
                    }
                    if (a.mode & 0o170000) != (b.mode & 0o170000) {
                        Change::TypeChanged
                    } else {
                        Change::Modified
                    }
                }
            };
            let d = file_diff(path, a, b, 0);
            out.push(ChangedFile {
                path: path.clone(),
                change,
                additions: d.additions,
                deletions: d.deletions,
                binary: d.binary,
            });
        }
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
                    .filter(|e| e.stage_raw() == 0)
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

pub(crate) fn file_diff(
    path: &str,
    old: Option<Blob>,
    new: Option<Blob>,
    context: u32,
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
    let binary = is_binary(a) || is_binary(b);
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
        old: o,
        new: n,
        binary,
        additions: count(LineKind::Added),
        deletions: count(LineKind::Removed),
        hunks,
    }
}
