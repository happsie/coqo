// SPDX-License-Identifier: Apache-2.0

//! Line diffs of a tool call's proposed edits, for the inline preview and the
//! `+n −m` counts.

use similar::{ChangeTag, TextDiff};

/// Lines shown around each change.
const CONTEXT: usize = 2;
/// Lines kept for the inline preview; the rest is summarised.
const MAX_LINES: usize = 80;

/// What a diff line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Added,
    Removed,
    Context,
    /// A gap between hunks.
    Gap,
}

/// One line of the preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

/// A file edit, ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffPreview {
    /// Absolute path of the file.
    pub path: String,
    /// The file is new.
    pub created: bool,
    pub added: usize,
    pub removed: usize,
    /// Up to [`MAX_LINES`] lines of hunks with context.
    pub lines: Vec<DiffLine>,
    /// Lines left out of `lines`.
    pub truncated: usize,
}

impl DiffPreview {
    /// Diff `old` (`None` for a new file) against `new`.
    pub fn new(path: &str, old: Option<&str>, new: &str) -> Self {
        let diff = TextDiff::from_lines(old.unwrap_or(""), new);
        let (mut added, mut removed) = (0, 0);
        let mut lines = Vec::new();
        let mut total = 0;
        for (ix, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
            if ix > 0 {
                total += 1;
                if lines.len() < MAX_LINES {
                    lines.push(DiffLine {
                        kind: LineKind::Gap,
                        text: String::new(),
                    });
                }
            }
            for op in group {
                for change in diff.iter_changes(op) {
                    let kind = match change.tag() {
                        ChangeTag::Insert => {
                            added += 1;
                            LineKind::Added
                        }
                        ChangeTag::Delete => {
                            removed += 1;
                            LineKind::Removed
                        }
                        ChangeTag::Equal => LineKind::Context,
                    };
                    total += 1;
                    if lines.len() < MAX_LINES {
                        let text = change.value().trim_end_matches(['\n', '\r']);
                        lines.push(DiffLine {
                            kind,
                            text: text.replace('\t', "    "),
                        });
                    }
                }
            }
        }
        Self {
            path: path.to_owned(),
            created: old.is_none(),
            added,
            removed,
            truncated: total - lines.len(),
            lines,
        }
    }

    /// The file name, for compact labels.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn file_name(&self) -> &str {
        file_name(&self.path)
    }
}

/// The last component of `path`.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let d = DiffPreview::new("/w/src/x.txt", Some(old), new);
        assert_eq!((d.added, d.removed), (2, 1));
        assert!(!d.created);
        assert_eq!(d.file_name(), "x.txt");
        let kinds: Vec<_> = d.lines.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LineKind::Gap), "{kinds:?}");
        assert_eq!(d.lines[1].kind, LineKind::Removed);
        assert_eq!(d.lines[1].text, "b");
        assert_eq!(d.lines[2].kind, LineKind::Added);
        assert_eq!(d.truncated, 0);
    }

    #[test]
    fn new_files_are_all_added_and_long_diffs_are_cut() {
        let new: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let d = DiffPreview::new("/w/new.rs", None, &new);
        assert!(d.created);
        assert_eq!((d.added, d.removed), (200, 0));
        assert_eq!(d.lines.len(), MAX_LINES);
        assert_eq!(d.truncated, 200 - MAX_LINES);
    }
}
