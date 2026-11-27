// SPDX-License-Identifier: Apache-2.0

//! The anchor model: what is stored, and what resolving returns.

use std::fmt;
use std::ops::{Range, RangeInclusive};

use serde::{Deserialize, Serialize};

use crate::snapshot::Snapshot;
use crate::text::text_hash;

/// How many lines of context an anchor keeps on each side.
pub const CONTEXT_LINES: usize = 3;

/// A spot in a text: 0-based line, byte column in that line, and byte
/// offset from the start (UTF-8, as in the buffer protocol).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Position {
    /// 0-based line.
    pub line: usize,
    /// Byte column within the line.
    pub col: usize,
    /// Byte offset from the start of the text.
    pub offset: usize,
}

/// A range of a text, from `start` to `end` (exclusive).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    /// Where it starts.
    pub start: Position,
    /// Where it ends (exclusive).
    pub end: Position,
}

impl Span {
    /// The byte range.
    pub fn bytes(&self) -> Range<usize> {
        self.start.offset..self.end.offset
    }

    /// The span of `range` in `snapshot`.
    pub fn of(snapshot: &Snapshot<'_>, range: Range<usize>) -> Self {
        Self {
            start: snapshot.position(range.start),
            end: snapshot.position(range.end),
        }
    }
}

/// The innermost symbol (function, method, class, …) an anchor was inside,
/// from `coqo-syntax`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolPath {
    /// The container path and the symbol's name, outermost first:
    /// `["shapes", "Circle", "area"]`.
    pub path: Vec<String>,
    /// Its kind, in snake case (`coqo_syntax::SymbolKind::as_str`).
    pub kind: String,
    /// How many lines below the symbol's first line the anchor started.
    #[serde(default)]
    pub line_offset: usize,
}

impl SymbolPath {
    /// The symbol's own name.
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    /// The path joined with `separator`: `shapes::Circle::area`.
    pub fn qualified_name(&self, separator: &str) -> String {
        self.path.join(separator)
    }
}

/// Code pinned by a question, walkthrough step, review comment or trace
/// point, with enough of a fingerprint to find it again after edits.
///
/// The JSON form (serde) is what the daemon stores; it stays readable by
/// later versions (new fields get defaults).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Anchor {
    /// The file, relative to its workspace root, with `/` separators.
    pub path: String,
    /// The buffer revision `range` is for; `None` for a file read from
    /// disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
    /// Where the anchored text was.
    pub range: Span,
    /// [`text_hash`] of `text`.
    pub text_hash: String,
    /// The anchored text, as it was (shown for an orphaned anchor).
    pub text: String,
    /// The start of `range`'s first line, up to the range.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// The rest of `range`'s last line after the range (without the line
    /// break).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub suffix: String,
    /// Up to [`CONTEXT_LINES`] lines before the first line, in order.
    #[serde(default)]
    pub context_before: Vec<String>,
    /// Up to [`CONTEXT_LINES`] lines after the last line, in order.
    #[serde(default)]
    pub context_after: Vec<String>,
    /// The innermost enclosing symbol, if the text was parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolPath>,
}

/// Why an anchor could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorError {
    /// The range is reversed, past the end, or splits a character.
    BadRange {
        /// The range asked for.
        range: Range<usize>,
        /// The text's length.
        len: usize,
    },
    /// A line past the end of the text.
    BadLine {
        /// The line asked for.
        line: usize,
        /// How many lines the text has.
        lines: usize,
    },
}

impl fmt::Display for AnchorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRange { range, len } => write!(
                f,
                "{}..{} is not a character range of the {len}-byte text",
                range.start, range.end
            ),
            Self::BadLine { line, lines } => {
                write!(f, "line {line} is past the end ({lines} lines)")
            }
        }
    }
}

impl std::error::Error for AnchorError {}

impl Anchor {
    /// Anchor the bytes `range` of `snapshot`'s text in the file `path`
    /// (workspace-relative). The revision is the snapshot's.
    pub fn new(
        path: impl Into<String>,
        snapshot: &Snapshot<'_>,
        range: Range<usize>,
    ) -> Result<Self, AnchorError> {
        let text = snapshot.text();
        if range.start > range.end
            || range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            return Err(AnchorError::BadRange {
                range,
                len: text.len(),
            });
        }
        let (first, last) = snapshot.lines_of(&range);
        let first_line = snapshot.line_range(first);
        let last_line = snapshot.line_range(last);
        let prefix = &text[first_line.start..range.start];
        let suffix = if range.end <= last_line.end {
            &text[range.end..last_line.end]
        } else {
            ""
        };
        let context_before = (first.saturating_sub(CONTEXT_LINES)..first)
            .map(|l| snapshot.line_text(l).to_owned())
            .collect();
        let context_after = (last + 1..(last + 1 + CONTEXT_LINES).min(snapshot.line_count()))
            .map(|l| snapshot.line_text(l).to_owned())
            .collect();
        let symbol = snapshot.enclosing_symbol(&range).map(|s| {
            let mut path = s.container.clone();
            path.push(s.name.clone());
            SymbolPath {
                path,
                kind: s.kind.as_str().to_owned(),
                line_offset: first.saturating_sub(s.start_line),
            }
        });
        let anchored = &text[range.clone()];
        Ok(Self {
            path: path.into(),
            rev: snapshot.rev(),
            range: Span::of(snapshot, range),
            text_hash: text_hash(anchored),
            text: anchored.to_owned(),
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
            context_before,
            context_after,
            symbol,
        })
    }

    /// Anchor whole lines (0-based, inclusive), with the last line's line
    /// break: what an agent means by "lines 10–12".
    pub fn lines(
        path: impl Into<String>,
        snapshot: &Snapshot<'_>,
        lines: RangeInclusive<usize>,
    ) -> Result<Self, AnchorError> {
        let count = snapshot.line_count();
        for &line in [lines.start(), lines.end()] {
            if line >= count {
                return Err(AnchorError::BadLine { line, lines: count });
            }
        }
        let start = snapshot.line_start(*lines.start());
        let end = snapshot.line_start(lines.end() + 1).max(start);
        Self::new(path, snapshot, start..end)
    }

    /// The anchored byte range, as it was.
    pub fn byte_range(&self) -> Range<usize> {
        self.range.bytes()
    }

    /// The whole lines the anchor was on: `prefix + text + suffix`.
    pub fn block(&self) -> String {
        let mut s = String::with_capacity(self.prefix.len() + self.text.len() + self.suffix.len());
        s.push_str(&self.prefix);
        s.push_str(&self.text);
        s.push_str(&self.suffix);
        s
    }

    /// The same anchor made afresh at `resolution`'s range of `snapshot`
    /// (new range, revision and fingerprints), or `None` for an orphan.
    ///
    /// Rebase after an [`Exact`](Status::Exact) or [`Moved`](Status::Moved)
    /// resolution to keep later resolves fast. After a
    /// [`Fuzzy`](Status::Fuzzy) one it adopts the guess, so it is up to the
    /// caller (for example, only once the user has seen it).
    pub fn rebased(&self, snapshot: &Snapshot<'_>, resolution: &Resolution) -> Option<Self> {
        let span = resolution.range?;
        Self::new(self.path.clone(), snapshot, span.bytes()).ok()
    }
}

/// How well an anchor was found again.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    /// The anchored text, unchanged, at the same range.
    Exact,
    /// The anchored text, unchanged, at another range (lines were added or
    /// removed above it, or it was moved).
    Moved,
    /// Text that is probably the anchored code after an edit (changed in
    /// place, renamed, reformatted). `confidence` is in
    /// [`MIN_CONFIDENCE`](crate::MIN_CONFIDENCE)`..1`.
    Fuzzy {
        /// How sure the match is.
        confidence: f32,
    },
    /// The anchored code is gone, or no match was good enough. The anchor's
    /// own text is all there is to show.
    Orphaned,
}

impl Status {
    /// 1 for exact and moved, the score for fuzzy, 0 for orphaned.
    pub fn confidence(&self) -> f32 {
        match *self {
            Self::Exact | Self::Moved => 1.0,
            Self::Fuzzy { confidence } => confidence,
            Self::Orphaned => 0.0,
        }
    }

    /// Whether the anchor points at code.
    pub fn is_found(&self) -> bool {
        !matches!(self, Self::Orphaned)
    }
}

/// Where an anchor is in a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    /// How well it was found.
    pub status: Status,
    /// Its range in the snapshot; `None` when orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Span>,
    /// The snapshot's revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
}

impl Resolution {
    /// The byte range, unless orphaned.
    pub fn bytes(&self) -> Option<Range<usize>> {
        self.range.map(|s| s.bytes())
    }
}
