// SPDX-License-Identifier: Apache-2.0

//! [`Snapshot`]: a text anchors are made in or resolved against.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::OnceLock;

use coqo_syntax::{Document, Symbol};

use crate::model::Position;
use crate::text::{fnv1a64, normalize};

/// A text at one moment (a buffer at a revision, or a file on disk), with
/// its symbols when it was parsed.
///
/// Indexes (line starts, symbols, line hashes, whitespace-free text) are
/// built on first use and shared by every anchor resolved against the same
/// snapshot, so resolve many anchors with one snapshot
/// ([`resolve_all`](Self::resolve_all)).
pub struct Snapshot<'a> {
    text: &'a str,
    rev: Option<u64>,
    doc: Option<&'a Document>,
    line_starts: OnceLock<Vec<usize>>,
    symbols: OnceLock<Vec<Symbol>>,
    lines: OnceLock<LineHashes>,
    squashed: OnceLock<Squashed>,
}

impl fmt::Debug for Snapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("len", &self.text.len())
            .field("rev", &self.rev)
            .field("parsed", &self.doc.is_some())
            .finish_non_exhaustive()
    }
}

/// Normalised line hash → the lines with it, in order. Blank lines are left
/// out.
#[derive(Debug, Default)]
pub(crate) struct LineHashes {
    pub(crate) by_hash: HashMap<u64, Vec<usize>>,
    /// Each line's normalised hash (0 for a blank line).
    pub(crate) hashes: Vec<u64>,
}

/// The text without whitespace, and where each of its bytes came from.
#[derive(Debug, Default)]
pub(crate) struct Squashed {
    pub(crate) text: String,
    pub(crate) origin: Vec<usize>,
}

impl<'a> Snapshot<'a> {
    /// A plain text (no symbols).
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            rev: None,
            doc: None,
            line_starts: OnceLock::new(),
            symbols: OnceLock::new(),
            lines: OnceLock::new(),
            squashed: OnceLock::new(),
        }
    }

    /// A parsed document: its text, with symbols for fingerprints and
    /// relocation.
    pub fn from_document(doc: &'a Document) -> Self {
        Self {
            doc: Some(doc),
            ..Self::new(doc.text())
        }
    }

    /// The same text, at buffer revision `rev`.
    #[must_use]
    pub fn at_rev(mut self, rev: u64) -> Self {
        self.rev = Some(rev);
        self
    }

    /// The text.
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// The buffer revision, if it is a buffer's text.
    pub fn rev(&self) -> Option<u64> {
        self.rev
    }

    /// The parsed document, if any.
    pub fn document(&self) -> Option<&'a Document> {
        self.doc
    }

    // ---- lines --------------------------------------------------------------

    fn line_starts(&self) -> &[usize] {
        self.line_starts.get_or_init(|| {
            std::iter::once(0)
                .chain(
                    self.text
                        .bytes()
                        .enumerate()
                        .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
                )
                .collect()
        })
    }

    /// How many lines the text has (a final `\n` starts an empty last line).
    pub fn line_count(&self) -> usize {
        self.line_starts().len()
    }

    /// The 0-based line `offset` is on.
    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts().partition_point(|&s| s <= offset) - 1
    }

    /// The byte range of `line` without its `\n` (and `\r`).
    pub fn line_range(&self, line: usize) -> Range<usize> {
        let starts = self.line_starts();
        let Some(&start) = starts.get(line) else {
            return self.text.len()..self.text.len();
        };
        let mut end = starts.get(line + 1).map_or(self.text.len(), |&s| s - 1);
        if end > start && self.text.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        start..end
    }

    /// Where `line` starts, or the end of the text past the last line.
    pub fn line_start(&self, line: usize) -> usize {
        self.line_starts()
            .get(line)
            .copied()
            .unwrap_or(self.text.len())
    }

    /// The text of `line`, without its line break.
    pub fn line_text(&self, line: usize) -> &'a str {
        &self.text[self.line_range(line)]
    }

    /// The line and byte column of `offset`.
    pub fn position(&self, offset: usize) -> Position {
        let offset = offset.min(self.text.len());
        let line = self.line_of(offset);
        Position {
            line,
            col: offset - self.line_starts()[line],
            offset,
        }
    }

    /// The byte offset of `line`, `col` (columns past the end of the line
    /// are clamped to it). `None` past the last line.
    pub fn offset(&self, line: usize, col: usize) -> Option<usize> {
        let starts = self.line_starts();
        let start = *starts.get(line)?;
        let end = starts.get(line + 1).map_or(self.text.len(), |&s| s - 1);
        let mut at = (start + col).min(end);
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        Some(at)
    }

    /// The first and last line a range covers. A non-empty range that ends
    /// at the start of a line (it includes the line break before) does not
    /// cover that line.
    pub(crate) fn lines_of(&self, range: &Range<usize>) -> (usize, usize) {
        let first = self.line_of(range.start);
        let mut last = self.line_of(range.end);
        if range.end > range.start && last > first && self.line_start(last) == range.end {
            last -= 1;
        }
        (first, last)
    }

    // ---- indexes ------------------------------------------------------------

    /// Every symbol (empty for a plain text).
    pub fn symbols(&self) -> &[Symbol] {
        self.symbols
            .get_or_init(|| self.doc.map(Document::symbols).unwrap_or_default())
    }

    /// The innermost symbol that contains all of `range`, leading and
    /// trailing whitespace aside (whole lines of a function are inside it).
    pub fn enclosing_symbol(&self, range: &Range<usize>) -> Option<&Symbol> {
        let range = match self.text.get(range.clone()) {
            Some(s) if !s.trim().is_empty() => {
                let start = range.start + (s.len() - s.trim_start().len());
                start..start + s.trim().len()
            }
            _ => range.clone(),
        };
        self.symbols()
            .iter()
            .filter(|s| {
                s.range.start <= range.start
                    && (range.end < s.range.end
                        || (range.end == s.range.end && range.start < range.end))
            })
            .min_by_key(|s| s.range.len())
    }

    pub(crate) fn line_hashes(&self) -> &LineHashes {
        self.lines.get_or_init(|| {
            let mut out = LineHashes::default();
            for line in 0..self.line_count() {
                let h = line_hash(self.line_text(line));
                out.hashes.push(h);
                if h != 0 {
                    out.by_hash.entry(h).or_default().push(line);
                }
            }
            out
        })
    }

    pub(crate) fn squashed(&self) -> &Squashed {
        self.squashed.get_or_init(|| {
            let mut out = Squashed::default();
            for (i, c) in self.text.char_indices() {
                if !c.is_whitespace() {
                    let mut buf = [0; 4];
                    let s = c.encode_utf8(&mut buf);
                    out.text.push_str(s);
                    out.origin.extend(i..i + s.len());
                }
            }
            out
        })
    }
}

/// The hash of a line, normalised; 0 for a blank one.
pub(crate) fn line_hash(line: &str) -> u64 {
    let n = normalize(line);
    if n.is_empty() {
        0
    } else {
        fnv1a64(n.as_bytes()).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_positions() {
        let s = Snapshot::new("ab\r\ncd\n\nef");
        assert_eq!(s.line_count(), 4);
        assert_eq!(s.line_text(0), "ab");
        assert_eq!(s.line_text(2), "");
        assert_eq!(s.line_text(3), "ef");
        assert_eq!(s.position(5).line, 1);
        assert_eq!(s.position(5).col, 1);
        assert_eq!(s.offset(1, 99), Some(6));
        assert_eq!(s.offset(9, 0), None);
        assert_eq!(s.lines_of(&(0..4)), (0, 0));
        assert_eq!(s.lines_of(&(0..5)), (0, 1));
        assert_eq!(s.lines_of(&(4..4)), (1, 1));
    }

    #[test]
    fn squashed_maps_back() {
        let s = Snapshot::new("a b\n  cé");
        let q = s.squashed();
        assert_eq!(q.text, "abcé");
        assert_eq!(q.origin, vec![0, 2, 6, 7, 8]);
    }
}
