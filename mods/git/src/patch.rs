// SPDX-License-Identifier: Apache-2.0

//! Staging and unstaging chosen lines: a patch with only those lines,
//! applied to the index with `git apply --cached`.
//!
//! **Staging** diffs the index against the work tree and keeps the chosen
//! lines: an unchosen removed line becomes context (it stays in the index),
//! an unchosen added line is left out. **Unstaging** diffs `HEAD` against
//! the index and applies the chosen lines in reverse: an unchosen added
//! line becomes context (it stays), an unchosen removed line is left out.
//! Hunk headers are recounted, and hunks with nothing chosen are dropped.

use std::collections::BTreeSet;

use crate::diff::{self, DEFAULT_CONTEXT, LineKind, RawHunk, RawLine, Side};
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
}

impl Repo {
    /// Stage the chosen lines of `path`'s unstaged diff (index → work
    /// tree), as shown by [`Repo::diff`] with [`Side::Index`] and
    /// [`Side::Worktree`].
    pub fn stage_lines(&self, path: &str, selection: &LineSelection) -> Result<()> {
        self.apply_lines(path, selection, Direction::Stage)
    }

    /// Unstage the chosen lines of `path`'s staged diff (`HEAD` → index).
    pub fn unstage_lines(&self, path: &str, selection: &LineSelection) -> Result<()> {
        self.apply_lines(path, selection, Direction::Unstage)
    }

    fn apply_lines(&self, path: &str, sel: &LineSelection, dir: Direction) -> Result<()> {
        check_path(path)?;
        if sel.is_empty() {
            return Err(GitError::new(ErrorKind::Invalid, "no lines chosen"));
        }
        let unborn = self.gix().head_id().is_err();
        let (old, new) = match dir {
            Direction::Stage => (
                self.read_side(&Side::Index, path)?,
                self.read_side(&Side::Worktree, path)?,
            ),
            Direction::Unstage => (
                if unborn {
                    None
                } else {
                    self.read_side(&Side::Rev("HEAD".into()), path)?
                },
                self.read_side(&Side::Index, path)?,
            ),
        };
        let empty: &[u8] = &[];
        let (a, b) = (
            old.as_ref().map_or(empty, |b| b.data.as_slice()),
            new.as_ref().map_or(empty, |b| b.data.as_slice()),
        );
        if diff::is_binary(a) || diff::is_binary(b) {
            return Err(GitError::new(
                ErrorKind::Unsupported,
                format!("{path} is binary: stage or unstage the whole file"),
            ));
        }
        if new.is_none() && dir == Direction::Unstage {
            return Err(GitError::new(
                ErrorKind::Unsupported,
                format!("{path} is deleted in the index: unstage the whole file"),
            ));
        }
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
        let mut args = vec!["apply", "--cached", "--whitespace=nowarn"];
        if dir == Direction::Unstage {
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
                    match (on, dir) {
                        (true, _) => Some(b'-'),
                        (false, Direction::Stage) => Some(b' '),
                        (false, Direction::Unstage) => None,
                    }
                }
                LineKind::Added => {
                    let on = l.new_line.is_some_and(|n| added.contains(&n));
                    chosen |= on;
                    match (on, dir) {
                        (true, _) => Some(b'+'),
                        (false, Direction::Stage) => None,
                        (false, Direction::Unstage) => Some(b' '),
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
            Direction::Unstage => {
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
