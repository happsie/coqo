// SPDX-License-Identifier: Apache-2.0

//! Folding ranges, from each language's `queries/<lang>/folds.scm`.
//!
//! A folds query captures foldable nodes as `@fold` (blocks, bodies,
//! literals), `@fold.comment` (comments) or `@fold.imports` (import
//! statements). Comments and imports on consecutive lines fold together, so
//! a run of `//` lines or `use` items is one fold.

use std::ops::Range;

use tree_sitter::{QueryCursor, StreamingIterator as _};

use crate::document::{Document, LineIndex};
use crate::symbols::trim_end;

/// What a fold holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FoldKind {
    /// A block, body or literal.
    Region,
    /// A comment, or a run of line comments.
    Comment,
    /// A run of imports.
    Imports,
}

impl FoldKind {
    /// The name, as LSP's `FoldingRangeKind` spells it (`region`, `comment`,
    /// `imports`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Comment => "comment",
            Self::Imports => "imports",
        }
    }
}

/// Lines that can be folded: `start_line` stays visible and the lines after
/// it, through `end_line`, are hidden.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FoldingRange {
    /// The first line (0-based); it stays visible.
    pub start_line: usize,
    /// The last line folded (0-based, inclusive), after `start_line`.
    pub end_line: usize,
    /// What the fold holds.
    pub kind: FoldKind,
    /// The folded nodes, in UTF-8 bytes.
    pub range: Range<usize>,
}

pub(crate) fn folding_ranges(doc: &Document) -> Vec<FoldingRange> {
    let query = &doc.language().queries().folds;
    let names = query.capture_names();
    let mut found: Vec<FoldingRange> = Vec::new();
    let mut cursor = QueryCursor::new();
    let text = doc.text();
    let lines = LineIndex::new(text);
    let mut captures = cursor.captures(query, doc.tree().root_node(), text.as_bytes());
    while let Some((m, i)) = captures.next() {
        let c = m.captures[*i];
        let kind = match names[c.index as usize] {
            "fold" => FoldKind::Region,
            "fold.comment" => FoldKind::Comment,
            "fold.imports" => FoldKind::Imports,
            other => panic!("unknown fold capture @{other}"),
        };
        let range = trim_end(text, c.node.byte_range());
        let start_line = lines.line(range.start);
        let end_line = lines
            .line(range.end.max(range.start + 1) - 1)
            .max(start_line);
        found.push(FoldingRange {
            start_line,
            end_line,
            kind,
            range,
        });
    }
    found.sort_by_key(|f| (f.start_line, std::cmp::Reverse(f.end_line)));

    // Merge consecutive comments and imports.
    let mut merged: Vec<FoldingRange> = Vec::with_capacity(found.len());
    let mut runs: Vec<FoldingRange> = Vec::new();
    for f in found {
        if f.kind == FoldKind::Region {
            merged.push(f);
            continue;
        }
        match runs.iter_mut().find(|r| r.kind == f.kind) {
            Some(run) if f.start_line <= run.end_line + 1 => {
                run.end_line = run.end_line.max(f.end_line);
                run.range.end = run.range.end.max(f.range.end);
            }
            Some(run) => merged.push(std::mem::replace(run, f)),
            None => runs.push(f),
        }
    }
    merged.extend(runs);

    merged.retain(|f| f.end_line > f.start_line);
    merged.sort_by_key(|f| (f.start_line, std::cmp::Reverse(f.end_line)));
    merged.dedup_by_key(|f| f.start_line);
    merged
}
