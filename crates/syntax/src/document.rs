// SPDX-License-Identifier: Apache-2.0

//! A parsed document that re-parses incrementally after edits.

use std::fmt;
use std::ops::Range;

use tree_sitter::{InputEdit, Parser, Point, Tree};

use crate::folding::{self, FoldingRange};
use crate::language::Language;
use crate::symbols::{self, OutlineItem, Symbol};

/// One edit: replace the bytes `range` with `text`.
///
/// Offsets are UTF-8 byte offsets, as in the buffer protocol.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextEdit {
    /// The bytes replaced; empty for a pure insert.
    pub range: Range<usize>,
    /// The replacement.
    pub text: String,
}

impl TextEdit {
    /// Replace `range` with `text`.
    pub fn new(range: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
        }
    }
}

/// Edits that don't fit the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditError(String);

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid edit: {}", self.0)
    }
}

impl std::error::Error for EditError {}

/// A document in a bundled language: its text and syntax tree.
///
/// [`edit`](Self::edit) applies edits and re-parses incrementally, reusing
/// the unchanged parts of the old tree, so a keystroke in a large file costs
/// far less than a full parse. Symbols, the outline and folding ranges are
/// computed from the current tree on demand.
pub struct Document {
    language: &'static Language,
    text: String,
    parser: Parser,
    tree: Tree,
    incremental_parses: u64,
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("language", &self.language.id())
            .field("len", &self.text.len())
            .finish_non_exhaustive()
    }
}

impl Document {
    /// Parse `text` as `language`.
    pub fn new(language: &'static Language, text: impl Into<String>) -> Self {
        let text = text.into();
        let mut parser = Parser::new();
        parser
            .set_language(&language.grammar())
            .expect("bundled grammars match the tree-sitter version");
        let tree = parse(&mut parser, &text, None);
        Self {
            language,
            text,
            parser,
            tree,
            incremental_parses: 0,
        }
    }

    /// The language.
    pub fn language(&self) -> &'static Language {
        self.language
    }

    /// The text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The current syntax tree.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// How many times [`edit`](Self::edit) re-parsed by reusing the previous
    /// tree.
    pub fn incremental_parses(&self) -> u64 {
        self.incremental_parses
    }

    /// Apply `edits` and re-parse incrementally.
    ///
    /// The edits are sorted, non-overlapping and all in the coordinates of
    /// the text before any of them (like an LSP `TextEdit[]` or one buffer
    /// change), on character boundaries. Nothing changes if any edit doesn't
    /// fit.
    pub fn edit(&mut self, edits: &[TextEdit]) -> Result<(), EditError> {
        let mut end = 0;
        for e in edits {
            let r = &e.range;
            if r.start > r.end || r.start < end {
                return Err(EditError(format!(
                    "edits must be sorted and must not overlap ({}..{})",
                    r.start, r.end
                )));
            }
            if r.end > self.text.len()
                || !self.text.is_char_boundary(r.start)
                || !self.text.is_char_boundary(r.end)
            {
                return Err(EditError(format!(
                    "{}..{} is not a character range of the {}-byte text",
                    r.start,
                    r.end,
                    self.text.len()
                )));
            }
            end = r.end;
        }
        if edits.is_empty() {
            return Ok(());
        }
        // Last edit first: each one's offsets stay valid in the text the
        // later ones left.
        for e in edits.iter().rev() {
            let start_position = point_at(&self.text, e.range.start);
            let old_end_position = point_at(&self.text, e.range.end);
            let new_end_position = advance(start_position, &e.text);
            self.tree.edit(&InputEdit {
                start_byte: e.range.start,
                old_end_byte: e.range.end,
                new_end_byte: e.range.start + e.text.len(),
                start_position,
                old_end_position,
                new_end_position,
            });
            self.text.replace_range(e.range.clone(), &e.text);
        }
        self.tree = parse(&mut self.parser, &self.text, Some(&self.tree));
        self.incremental_parses += 1;
        Ok(())
    }

    /// Replace the whole text and parse it from scratch.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.tree = parse(&mut self.parser, &self.text, None);
    }

    /// Every symbol the language's tags query finds (functions, methods,
    /// types, modules, …), in document order, each with the names of the
    /// symbols that contain it.
    pub fn symbols(&self) -> Vec<Symbol> {
        symbols::symbols(self)
    }

    /// The symbols as a tree: each item holds the symbols inside it.
    pub fn outline(&self) -> Vec<OutlineItem> {
        symbols::outline(self.symbols())
    }

    /// The symbols that contain `offset`, outermost first. The last one is
    /// the innermost: what an anchor at `offset` belongs to.
    pub fn symbols_at(&self, offset: usize) -> Vec<Symbol> {
        self.symbols()
            .into_iter()
            .filter(|s| s.range.start <= offset && offset < s.range.end.max(s.range.start + 1))
            .collect()
    }

    /// The foldable line ranges, sorted by start line, at most one per line.
    pub fn folding_ranges(&self) -> Vec<FoldingRange> {
        folding::folding_ranges(self)
    }
}

fn parse(parser: &mut Parser, text: &str, old: Option<&Tree>) -> Tree {
    parser
        .parse(text, old)
        .expect("a parser with a language and no timeout always returns a tree")
}

/// The row and byte column of `offset`.
pub(crate) fn point_at(text: &str, offset: usize) -> Point {
    let before = &text.as_bytes()[..offset];
    let row = before.iter().filter(|&&b| b == b'\n').count();
    let line_start = before
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    Point::new(row, offset - line_start)
}

/// Line numbers of byte offsets, by binary search over the line starts.
pub(crate) struct LineIndex {
    /// Offset of each `\n`.
    breaks: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            breaks: text
                .bytes()
                .enumerate()
                .filter_map(|(i, b)| (b == b'\n').then_some(i))
                .collect(),
        }
    }

    /// The 0-based line `offset` is on (a `\n` belongs to the line it ends).
    pub(crate) fn line(&self, offset: usize) -> usize {
        self.breaks.partition_point(|&b| b < offset)
    }
}

/// Where `start` ends up after inserting `text` there.
fn advance(start: Point, text: &str) -> Point {
    match text.rfind('\n') {
        None => Point::new(start.row, start.column + text.len()),
        Some(last) => {
            let rows = text.bytes().filter(|&b| b == b'\n').count();
            Point::new(start.row + rows, text.len() - last - 1)
        }
    }
}
