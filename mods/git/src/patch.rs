// SPDX-License-Identifier: Apache-2.0

//! Staging, unstaging and discarding chosen lines: a patch with only those
//! lines, applied to the index with `git apply --cached` (or to the work
//! tree, for a discard).
//!
//! **Staging** diffs the index against the work tree and keeps the chosen
//! lines: an unchosen removed line becomes context (it stays in the index),
//! an unchosen added line is left out. **Unstaging** diffs `HEAD` against
//! the index and applies the chosen lines in reverse: an unchosen added
//! line becomes context (it stays), an unchosen removed line is left out.
//! **Discarding** builds the patch as unstaging does, from the unstaged
//! diff, and applies it in reverse to the work tree (`git apply --reverse`,
//! which reads and writes the files through the same filters as `git
//! add` and checkout). Hunk headers are recounted, and hunks with nothing
//! chosen are dropped.

use std::collections::BTreeSet;

use crate::diff::{self, DEFAULT_CONTEXT, LineKind, RawHunk, RawLine, Side};
use crate::status::Change;
use crate::{ErrorKind, GitError, Repo, Result, check_path};

/// Lines of a diff, by their line numbers in it: removed lines by their old
/// number, added lines by their new number. Choosing a whole hunk is
/// choosing all of its removed and added lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineSelection {
    /// Old-side numbers of removed lines.
    pub removed: Vec<u32>,
    /// New-side numbers of added lines.
    pub added: Vec<u32>,
}

impl LineSelection {
    /// Every changed line of `hunk`, for staging it whole.
    pub fn hunk(hunk: &diff::Hunk) -> Self {
        let mut s = Self::default();
        for l in &hunk.lines {
            match l.kind {
                LineKind::Removed => s.removed.extend(l.old_line),
                LineKind::Added => s.added.extend(l.new_line),
                LineKind::Context => {}
            }
        }
        s
    }

    fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Stage,
    Unstage,
    /// Throw the chosen lines of the unstaged diff away: built as an
    /// unstage (the new side keeps the unchosen changes) and applied in
    /// reverse to the work tree.
    Discard,
}

impl Direction {
    /// The patch keeps the new side's unchosen changes (applied in
    /// reverse), not the old side's.
    fn reverse(self) -> bool {
        matches!(self, Direction::Unstage | Direction::Discard)
    }
}

impl Repo {
    /// Stage the chosen lines of `path`'s unstaged diff (index → work
    /// tree), as shown by [`Repo::diff`] with [`Side::Index`] and
    /// [`Side::Worktree`].
    pub fn stage_lines(&self, path: &str, selection: &LineSelection) -> Result<()> {
        self.apply_lines(path, selection, Direction::Stage)
    }

    /// Unstage the chosen lines of `path`'s staged diff (`HEAD` → index).
    /// For a staged rename the diff is from the `HEAD` path
    /// ([`StatusEntry::orig_path`](crate::StatusEntry::orig_path)), and the
    /// rename stays staged.
    pub fn unstage_lines(&self, path: &str, selection: &LineSelection) -> Result<()> {
        self.apply_lines(path, selection, Direction::Unstage)
    }

    /// Discard the chosen lines of `path`'s unstaged diff (index → work
    /// tree): the work tree gets the index's version of those lines back,
    /// and keeps its other changes. The lost lines are gone for good.
    pub fn discard_lines(&self, path: &str, selection: &LineSelection) -> Result<()> {
        self.apply_lines(path, selection, Direction::Discard)
    }

    fn apply_lines(&self, path: &str, sel: &LineSelection, dir: Direction) -> Result<()> {
        check_path(path)?;
        if sel.is_empty() {
            return Err(GitError::new(ErrorKind::Invalid, "no lines chosen"));
        }
        let unborn = self.gix().head_id().is_err();
        let mut reader = self.reader();
        let mut orig_path = path.to_owned();
        let (old, new) = match dir {
            Direction::Stage | Direction::Discard => (
                reader.read(&Side::Index, path)?,
                reader.read(&Side::Worktree, path)?,
            ),
            Direction::Unstage => {
                let mut old = if unborn {
                    None
                } else {
                    reader.read(&Side::Rev("HEAD".into()), path)?
                };
                if old.is_none() && !unborn {
                    // A staged rename: its diff is from where it was.
                    let from = self.status()?.entries.into_iter().find_map(|e| {
                        (e.path == path && e.staged == Some(Change::Renamed))
                            .then_some(e.orig_path)
                            .flatten()
                    });
                    if let Some(from) = from {
                        old = reader.read(&Side::Rev("HEAD".into()), &from)?;
                        orig_path = from;
                    }
                }
                (old, reader.read(&Side::Index, path)?)
            }
        };
        if reader.either_binary(&orig_path, old.as_ref(), path, new.as_ref())? {
            return Err(GitError::new(
                ErrorKind::Unsupported,
                match dir {
                    Direction::Discard => format!("{path} is binary: discard the whole file"),
                    _ => format!("{path} is binary: stage or unstage the whole file"),
                },
            ));
        }
        match (dir, &old, &new) {
            (Direction::Unstage, _, None) => {
                return Err(GitError::new(
                    ErrorKind::Unsupported,
                    format!("{path} is deleted in the index: unstage the whole file"),
                ));
            }
            (Direction::Discard, None, _) => {
                return Err(GitError::new(
                    ErrorKind::Unsupported,
                    format!("{path} is not in the index: discard the whole file"),
                ));
            }
            (Direction::Discard, _, None) => {
                return Err(GitError::new(
                    ErrorKind::Unsupported,
                    format!("{path} is deleted: discard the whole file to bring it back"),
                ));
            }
            _ => {}
        }
        let empty: &[u8] = &[];
        let (a, b) = (
            old.as_ref().map_or(empty, |b| b.data.as_slice()),
            new.as_ref().map_or(empty, |b| b.data.as_slice()),
        );
        let hunks = diff::compute(a, b, DEFAULT_CONTEXT);
        check_selection(&hunks, sel)?;
        let removed: BTreeSet<u32> = sel.removed.iter().copied().collect();
        let added: BTreeSet<u32> = sel.added.iter().copied().collect();
        let header = header(
            path,
            match (&old, dir) {
                (None, Direction::Stage) => new.as_ref().map(|b| b.mode),
                _ => None,
            },
        );
        let patch = build(header, &hunks, &removed, &added, dir);
        let mut args = vec!["apply", "--whitespace=nowarn"];
        if dir != Direction::Discard {
            // The index; a discard patches the work tree.
            args.push("--cached");
        }
        if dir.reverse() {
            args.push("--reverse");
        }
        args.push("-");
        self.git(args, Some(&patch)).map(drop).map_err(|e| {
            GitError::new(
                e.kind,
                format!("cannot apply the chosen lines of {path}: {}", e.message),
            )
        })
    }
}

/// Every chosen line must be a changed line of the current diff.
fn check_selection(hunks: &[RawHunk], sel: &LineSelection) -> Result<()> {
    let lines = || hunks.iter().flat_map(|h| &h.lines);
    let removed: BTreeSet<u32> = lines()
        .filter(|l| l.kind == LineKind::Removed)
        .filter_map(|l| l.old_line)
        .collect();
    let added: BTreeSet<u32> = lines()
        .filter(|l| l.kind == LineKind::Added)
        .filter_map(|l| l.new_line)
        .collect();
    let stale = sel.removed.iter().any(|n| !removed.contains(n))
        || sel.added.iter().any(|n| !added.contains(n));
    if stale {
        return Err(GitError::new(
            ErrorKind::StaleSelection,
            "the chosen lines are not in the current diff: the file changed, show it again",
        ));
    }
    Ok(())
}

/// The `diff --git` header; `new_file` is the mode of a file the index does
/// not have yet.
fn header(path: &str, new_file: Option<u32>) -> Vec<u8> {
    let a = quote(&format!("a/{path}"));
    let b = quote(&format!("b/{path}"));
    let mut h = format!("diff --git {a} {b}\n");
    match new_file {
        Some(mode) => h.push_str(&format!("new file mode {mode:o}\n--- /dev/null\n")),
        None => h.push_str(&format!("--- {a}\n")),
    }
    h.push_str(&format!("+++ {b}\n"));
    h.into_bytes()
}

/// git's C-style quoting, for names git would quote.
fn quote(name: &str) -> String {
    let plain = name
        .chars()
        .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ');
    if plain {
        return name.to_owned();
    }
    let mut q = String::from("\"");
    for b in name.bytes() {
        match b {
            b'"' => q.push_str("\\\""),
            b'\\' => q.push_str("\\\\"),
            b'\t' => q.push_str("\\t"),
            b'\n' => q.push_str("\\n"),
            0x20..=0x7e => q.push(char::from(b)),
            _ => q.push_str(&format!("\\{b:03o}")),
        }
    }
    q.push('"');
    q
}

fn build(
    mut out: Vec<u8>,
    hunks: &[RawHunk],
    removed: &BTreeSet<u32>,
    added: &BTreeSet<u32>,
    dir: Direction,
) -> Vec<u8> {
    // New minus old lines of the hunks written so far.
    let mut delta: i64 = 0;
    for h in hunks {
        let mut lines: Vec<(u8, &RawLine)> = Vec::new();
        let mut chosen = false;
        for l in &h.lines {
            let keep = match l.kind {
                LineKind::Context => Some(b' '),
                LineKind::Removed => {
                    let on = l.old_line.is_some_and(|n| removed.contains(&n));
                    chosen |= on;
                    match (on, dir.reverse()) {
                        (true, _) => Some(b'-'),
                        (false, false) => Some(b' '),
                        (false, true) => None,
                    }
                }
                LineKind::Added => {
                    let on = l.new_line.is_some_and(|n| added.contains(&n));
                    chosen |= on;
                    match (on, dir.reverse()) {
                        (true, _) => Some(b'+'),
                        (false, false) => None,
                        (false, true) => Some(b' '),
                    }
                }
            };
            if let Some(mark) = keep {
                lines.push((mark, l));
            }
        }
        if !chosen {
            continue;
        }
        let old_lines = lines.iter().filter(|(m, _)| *m != b'+').count() as i64;
        let new_lines = lines.iter().filter(|(m, _)| *m != b'-').count() as i64;
        // 0-based position of the hunk on the side that keeps its numbers,
        // then the other side's from the running offset.
        let (old_start, new_start) = match dir {
            Direction::Stage => {
                let first_old = i64::from(h.old_start) - i64::from(h.old_lines > 0);
                let first_new = first_old + delta;
                (i64::from(h.old_start), first_new + i64::from(new_lines > 0))
            }
            Direction::Unstage | Direction::Discard => {
                let first_new = i64::from(h.new_start) - i64::from(h.new_lines > 0);
                let first_old = first_new - delta;
                (first_old + i64::from(old_lines > 0), i64::from(h.new_start))
            }
        };
        delta += new_lines - old_lines;
        out.extend_from_slice(
            format!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n").as_bytes(),
        );
        for (mark, l) in lines {
            out.push(mark);
            out.extend_from_slice(&l.text);
            out.push(b'\n');
            if !l.newline {
                out.extend_from_slice(b"\\ No newline at end of file\n");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quotes_like_git() {
        assert_eq!(quote("a/src/main.rs"), "a/src/main.rs");
        assert_eq!(quote("a/with space.txt"), "a/with space.txt");
        assert_eq!(quote("a/tab\there"), "\"a/tab\\there\"");
        assert_eq!(quote("a/é"), "\"a/\\303\\251\"");
    }
}
