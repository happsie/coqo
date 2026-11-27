// SPDX-License-Identifier: Apache-2.0

//! The coverage guard (PLAN §7.6, issue #99, ADR 0020): every changed hunk
//! of a walkthrough's scope is covered by a step, listed as trivial, or
//! flagged.
//!
//! Coqo computes the diff itself, with the git mod's API (`coqo.git@1`, in
//! the daemon), without context lines so each run of changed lines is its
//! own hunk:
//!
//! - **worktree** scope: `base` (its commit when published) against the
//!   files on disk, untracked files included (an agent's new files are
//!   untracked until the user stages them);
//! - **commits**: `base` against `head`;
//! - **pr**: the merge base of `base` and `head` against `head`, as the PR
//!   shows it.
//!
//! Only the scope's root counts, and only the part of its repository under
//! it. Committed walkthrough files (`.coqo/walkthroughs/`) are not part of
//! the change.
//!
//! **Overlap.** A hunk is covered when an anchor of a step in the same root
//! and file has a line in the hunk's lines of the new file. Lines that were
//! only removed have no new lines: an anchor on the line before or after
//! them covers them. A whole new file is one hunk. A deleted or binary file
//! has nothing to anchor, so only `trivial_hunks` covers it. A file moved
//! with its contents unchanged (git's rename) is one hunk on the new path,
//! covered by any anchor in it. `trivial_hunks` match the same way, or a
//! whole file when they give no lines (by its old path too, for a rename).
//! A step's anchor wins over a trivial entry.

use coqo_mod_git::{self as mg, GitError, Repo, Side};
use coqo_protocol::methods::editor::LineRange;
use coqo_protocol::methods::walkthroughs::{
    COMMITTED_DIR, MAX_UNCOVERED, TrivialHunk, UncoveredHunk, WalkthroughCoverage, WholeFile,
};
use coqo_protocol::methods::workspace::RootId;

/// Most changed files checked; a bigger diff is not checked.
pub(crate) const MAX_FILES: usize = 2000;
/// Changed lines shown in an excerpt.
const EXCERPT_LINES: usize = 3;
/// Characters of an excerpt line.
const EXCERPT_WIDTH: usize = 100;

/// A changed hunk of the scope, its path relative to the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Changed {
    pub path: String,
    /// The first new line, 0-based. For lines only removed (`new_lines`
    /// 0), the line after them.
    pub new_start: u32,
    pub new_lines: u32,
    /// The first old line, 0-based (meaningful when `old_lines` > 0).
    pub old_start: u32,
    pub old_lines: u32,
    pub whole_file: Option<WholeFile>,
    pub renamed_from: Option<String>,
    pub excerpt: String,
}

impl Changed {
    /// The new-file lines an anchor must touch to cover it, 0-based,
    /// inclusive; `None` when no anchor can (a deleted or binary file).
    fn touch(&self) -> Option<(u32, u32)> {
        match self.whole_file {
            Some(WholeFile::Deleted | WholeFile::Binary) => None,
            _ if self.new_lines == 0 => Some((self.new_start.saturating_sub(1), self.new_start)),
            _ => Some((self.new_start, self.new_start + self.new_lines - 1)),
        }
    }

    fn uncovered(&self, root: RootId) -> UncoveredHunk {
        let new_range = match self.whole_file {
            Some(WholeFile::Deleted) => LineRange { start: 0, end: 0 },
            _ if self.new_lines == 0 => {
                let at = self.new_start.saturating_sub(1);
                LineRange { start: at, end: at }
            }
            _ => LineRange {
                start: self.new_start,
                end: self.new_start + self.new_lines - 1,
            },
        };
        UncoveredHunk {
            root,
            path: self.path.clone(),
            new_range,
            old_range: (self.old_lines > 0).then(|| LineRange {
                start: self.old_start,
                end: self.old_start + self.old_lines - 1,
            }),
            added: self.new_lines,
            removed: self.old_lines,
            whole_file: self.whole_file,
            renamed_from: self.renamed_from.clone(),
            excerpt: self.excerpt.clone(),
        }
    }
}

/// A step's anchor, for coverage: its root, path and lines (0-based,
/// inclusive) in the file as it is now.
pub(crate) type AnchorLines = (RootId, String, LineRange);

fn overlaps((a, b): (u32, u32), r: LineRange) -> bool {
    r.start <= b && a <= r.end
}

/// Sort `changed` (the scope's hunks in `root`) into covered, trivial and
/// uncovered.
pub(crate) fn classify(
    root: RootId,
    changed: &[Changed],
    anchors: &[AnchorLines],
    trivial: &[TrivialHunk],
) -> WalkthroughCoverage {
    let mut c = WalkthroughCoverage {
        total: changed.len() as u32,
        ..WalkthroughCoverage::default()
    };
    for h in changed {
        let touch = h.touch();
        let by_step = touch.is_some_and(|t| {
            anchors
                .iter()
                .any(|(r, p, lines)| *r == root && *p == h.path && overlaps(t, *lines))
        });
        if by_step {
            c.covered += 1;
            continue;
        }
        let listed = trivial.iter().any(|t| {
            let same_file = t.path == h.path || h.renamed_from.as_ref() == Some(&t.path);
            t.root == root
                && same_file
                && match (t.lines, touch) {
                    (None, _) | (_, None) => true,
                    // A rename has no changed lines: any lines of it.
                    (Some(_), Some(_)) if h.whole_file == Some(WholeFile::Renamed) => true,
                    (Some(lines), Some(t)) => overlaps(t, lines),
                }
        });
        if listed {
            c.trivial += 1;
        } else if c.uncovered.len() < MAX_UNCOVERED {
            c.uncovered.push(h.uncovered(root));
        }
    }
    c
}

fn excerpt<'a>(lines: impl Iterator<Item = &'a mg::DiffLine>) -> String {
    let mut out = Vec::new();
    for l in lines {
        let mark = match l.kind {
            mg::LineKind::Added => '+',
            mg::LineKind::Removed => '-',
            mg::LineKind::Context => continue,
        };
        let text: String = l.text.trim_end().chars().take(EXCERPT_WIDTH).collect();
        if text.trim().is_empty() {
            // Says nothing.
            continue;
        }
        out.push(format!("{mark}{text}"));
        if out.len() == EXCERPT_LINES {
            break;
        }
    }
    out.join("\n")
}

/// A changed file's hunks, with the texts a rename is matched by.
struct FileHunks {
    hunks: Vec<Changed>,
    /// Removed lines of a deleted file, added lines of an added one.
    text: Option<Vec<String>>,
    added: bool,
    deleted: bool,
}

fn file_hunks(path: String, d: mg::FileDiff) -> Option<FileHunks> {
    let whole = |kind, new_lines, old_lines, excerpt| Changed {
        path: path.clone(),
        new_start: 0,
        new_lines,
        old_start: 0,
        old_lines,
        whole_file: Some(kind),
        renamed_from: None,
        excerpt,
    };
    let lines_of = |d: &mg::FileDiff| {
        d.hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.text.clone()))
            .collect::<Vec<_>>()
    };
    let all = |d: &mg::FileDiff| excerpt(d.hunks.iter().flat_map(|h| h.lines.iter()));
    if d.binary {
        return Some(FileHunks {
            hunks: vec![whole(WholeFile::Binary, 0, 0, "binary file".into())],
            text: None,
            added: false,
            deleted: false,
        });
    }
    if d.new.is_none() {
        return Some(FileHunks {
            hunks: vec![whole(WholeFile::Deleted, 0, d.deletions, all(&d))],
            text: Some(lines_of(&d)),
            added: false,
            deleted: true,
        });
    }
    if d.old.is_none() {
        return Some(FileHunks {
            hunks: vec![whole(WholeFile::Added, d.additions, 0, all(&d))],
            text: Some(lines_of(&d)),
            added: true,
            deleted: false,
        });
    }
    if d.hunks.is_empty() {
        // Only the mode changed: no lines to explain.
        return None;
    }
    let hunks = d
        .hunks
        .iter()
        .map(|h| Changed {
            path: path.clone(),
            // git names the line before when there are no new lines.
            new_start: if h.new_lines == 0 {
                h.new_start
            } else {
                h.new_start - 1
            },
            new_lines: h.new_lines,
            old_start: h.old_start.saturating_sub(1),
            old_lines: h.old_lines,
            whole_file: None,
            renamed_from: None,
            excerpt: excerpt(h.lines.iter()),
        })
        .collect();
    Some(FileHunks {
        hunks,
        text: None,
        added: false,
        deleted: false,
    })
}

/// Why the scope's hunks could not be listed.
#[derive(Debug)]
pub(crate) enum HunksError {
    Git(GitError),
    TooMany(usize),
}

/// The hunks from `base` to `head` in `repo`, under its folder (`prefix`),
/// relative to it. `untracked`: also untracked files (the work tree as the
/// head).
pub(crate) fn scope_hunks(
    repo: &Repo,
    base: &Side,
    head: &Side,
    untracked: bool,
) -> Result<Vec<Changed>, HunksError> {
    let prefix = match repo.prefix() {
        "" => String::new(),
        p => format!("{}/", p.trim_end_matches('/')),
    };
    let mut paths: Vec<String> = repo
        .changed_files(base, head)
        .map_err(HunksError::Git)?
        .into_iter()
        .map(|f| f.path)
        .collect();
    if untracked {
        let status = repo.status().map_err(HunksError::Git)?;
        paths.extend(
            status
                .entries
                .into_iter()
                .filter(|e| e.unstaged == Some(mg::Change::Untracked) && !e.path.ends_with('/'))
                .map(|e| e.path),
        );
        paths.sort();
        paths.dedup();
    }
    let committed = format!("{COMMITTED_DIR}/");
    let paths: Vec<(String, String)> = paths
        .into_iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(&prefix)?.to_owned();
            (!rel.starts_with(&committed)).then_some((p, rel))
        })
        .collect();
    if paths.len() > MAX_FILES {
        return Err(HunksError::TooMany(paths.len()));
    }
    let mut files = Vec::new();
    for (path, rel) in paths {
        let d = repo.diff(&path, base, head, 0).map_err(HunksError::Git)?;
        files.extend(file_hunks(rel, d));
    }
    pair_renames(&mut files);
    Ok(files.into_iter().flat_map(|f| f.hunks).collect())
}

/// A deleted file and an added one with the same lines are a move: one
/// hunk on the new path.
fn pair_renames(files: &mut [FileHunks]) {
    let deleted: Vec<usize> = (0..files.len())
        .filter(|&i| files[i].deleted && files[i].text.as_ref().is_some_and(|t| !t.is_empty()))
        .collect();
    let mut gone = Vec::new();
    for i in 0..files.len() {
        if !files[i].added {
            continue;
        }
        let Some(d) = deleted
            .iter()
            .copied()
            .find(|&d| !gone.contains(&d) && files[d].text == files[i].text)
        else {
            continue;
        };
        gone.push(d);
        let from = files[d].hunks[0].path.clone();
        let h = &mut files[i].hunks[0];
        h.whole_file = Some(WholeFile::Renamed);
        h.renamed_from = Some(from);
        h.excerpt = String::new();
    }
    for d in gone {
        files[d].hunks.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(path: &str, new_start: u32, new_lines: u32, old_lines: u32) -> Changed {
        Changed {
            path: path.into(),
            new_start,
            new_lines,
            old_start: new_start,
            old_lines,
            whole_file: None,
            renamed_from: None,
            excerpt: String::new(),
        }
    }

    fn anchor(root: u32, path: &str, start: u32, end: u32) -> AnchorLines {
        (RootId(root), path.into(), LineRange { start, end })
    }

    fn trivial(path: &str, lines: Option<(u32, u32)>) -> TrivialHunk {
        TrivialHunk {
            root: RootId(0),
            path: path.into(),
            lines: lines.map(|(start, end)| LineRange { start, end }),
            reason: None,
        }
    }

    #[test]
    fn an_anchor_overlapping_any_line_covers_a_hunk() {
        // Lines 10-12 changed (0-based).
        let changed = [hunk("a.rs", 10, 3, 1)];
        let cov = |anchors: &[AnchorLines]| classify(RootId(0), &changed, anchors, &[]);
        assert_eq!(cov(&[anchor(0, "a.rs", 12, 20)]).covered, 1);
        assert_eq!(cov(&[anchor(0, "a.rs", 5, 10)]).covered, 1);
        assert_eq!(cov(&[anchor(0, "a.rs", 13, 20)]).covered, 0);
        assert_eq!(cov(&[anchor(0, "a.rs", 0, 9)]).covered, 0);
        // Another file, or the same path in another root, does not.
        assert_eq!(cov(&[anchor(0, "b.rs", 10, 12)]).covered, 0);
        assert_eq!(cov(&[anchor(1, "a.rs", 10, 12)]).covered, 0);
        let c = cov(&[]);
        assert_eq!((c.total, c.covered, c.trivial), (1, 0, 0));
        let u = &c.uncovered[0];
        assert_eq!(
            (u.new_range, u.old_range, u.added, u.removed),
            (
                LineRange { start: 10, end: 12 },
                Some(LineRange { start: 10, end: 10 }),
                3,
                1
            )
        );
        assert_eq!(u.location(), "a.rs:11-13");
    }

    #[test]
    fn removed_lines_are_covered_by_an_anchor_beside_them() {
        // Lines removed between new lines 4 and 5 (0-based): git's
        // new_start 5 (1-based, the line before) is 0-based 5 after.
        let changed = [hunk("a.rs", 5, 0, 2)];
        let cov = |a: AnchorLines| classify(RootId(0), &changed, &[a], &[]).covered;
        assert_eq!(cov(anchor(0, "a.rs", 4, 4)), 1, "the line before");
        assert_eq!(cov(anchor(0, "a.rs", 5, 8)), 1, "the line after");
        assert_eq!(cov(anchor(0, "a.rs", 6, 8)), 0);
        assert_eq!(cov(anchor(0, "a.rs", 0, 3)), 0);
        let u = classify(RootId(0), &changed, &[], &[]).uncovered.remove(0);
        assert_eq!(u.new_range, LineRange { start: 4, end: 4 });
        assert_eq!(u.location(), "a.rs:5");
        // At the top of the file.
        let top = [hunk("a.rs", 0, 0, 2)];
        assert_eq!(
            classify(RootId(0), &top, &[anchor(0, "a.rs", 0, 0)], &[]).covered,
            1
        );
    }

    #[test]
    fn trivial_hunks_count_unless_a_step_covers_them() {
        let changed = [
            hunk("a.rs", 0, 1, 0),
            hunk("a.rs", 20, 1, 0),
            hunk("b.rs", 3, 2, 2),
        ];
        let c = classify(
            RootId(0),
            &changed,
            &[anchor(0, "a.rs", 0, 0)],
            &[trivial("a.rs", Some((0, 0))), trivial("b.rs", None)],
        );
        assert_eq!((c.total, c.covered, c.trivial), (3, 1, 1));
        assert_eq!(c.uncovered_count(), 1);
        assert_eq!(c.uncovered[0].new_range.start, 20);
    }

    #[test]
    fn whole_files() {
        let mut deleted = hunk("gone.rs", 0, 0, 4);
        deleted.whole_file = Some(WholeFile::Deleted);
        let mut binary = hunk("logo.png", 0, 0, 0);
        binary.whole_file = Some(WholeFile::Binary);
        let mut moved = hunk("new.rs", 0, 9, 0);
        moved.whole_file = Some(WholeFile::Renamed);
        moved.renamed_from = Some("old.rs".into());
        let changed = [deleted, binary, moved];
        // No anchor covers a deleted or binary file.
        let anchors = [
            anchor(0, "gone.rs", 0, 3),
            anchor(0, "logo.png", 0, 0),
            anchor(0, "new.rs", 8, 8),
        ];
        let c = classify(RootId(0), &changed, &anchors, &[]);
        assert_eq!(c.covered, 1);
        assert_eq!(c.uncovered[0].location(), "gone.rs");
        assert_eq!(c.uncovered[0].change(), "file deleted");
        assert_eq!(c.uncovered[1].change(), "binary file");
        // Trivial entries cover them, a rename by its old path too.
        let c = classify(
            RootId(0),
            &changed,
            &[],
            &[
                trivial("gone.rs", Some((0, 0))),
                trivial("logo.png", None),
                trivial("old.rs", None),
            ],
        );
        assert_eq!((c.trivial, c.uncovered_count()), (3, 0));
        assert!(c.is_complete());
    }

    #[test]
    fn the_list_is_capped_but_the_counts_are_not() {
        let changed: Vec<_> = (0..MAX_UNCOVERED as u32 + 5)
            .map(|i| hunk("a.rs", i * 3, 1, 0))
            .collect();
        let c = classify(RootId(0), &changed, &[], &[]);
        assert_eq!(c.uncovered.len(), MAX_UNCOVERED);
        assert_eq!(c.uncovered_count(), MAX_UNCOVERED as u32 + 5);
    }
}
