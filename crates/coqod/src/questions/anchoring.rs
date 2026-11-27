// SPDX-License-Identifier: Apache-2.0

//! The anchors service's pure part: making anchors and resolving them in a
//! text with `coqo-anchors`, and their wire form.
//!
//! A file's anchors are resolved together against one [`Text`]: the open
//! buffer (with its id, revision and, when known, the changes since a
//! revision) or the file on disk. An anchor **tracked** in that buffer (made
//! or last rebased in it, so its revision means something there) is mapped
//! exactly through the changes since; any other is relocated by its
//! fingerprint (ADR 0013: a revision is only meaningful within one buffer
//! lifetime). Exact and moved anchors are rebased, so the next resolve is
//! the cheap exact path; a fuzzy match is never adopted.

use std::ops::RangeInclusive;

use coqo_anchors::{Anchor, AnchorError, Resolution, Snapshot, Span, Status};
use coqo_protocol::methods::buffer::{BufferId, TextEdit as WireEdit};
use coqo_protocol::methods::questions::{
    AnchorPoint, AnchorResolution, AnchorSpan, AnchorStatus, AnchorSymbol, CodeAnchor,
};
use coqo_protocol::methods::workspace::RootId;
use coqo_syntax::{Document, Language, TextEdit};

/// The text a file's anchors are made in or resolved against.
#[derive(Debug, Default)]
pub(crate) struct Text {
    /// The text.
    pub text: String,
    /// The open buffer it is, and its revision; `None` for the file on
    /// disk.
    pub buffer: Option<(BufferId, u64)>,
    /// The buffer's changes after `since`, if they were asked for and are
    /// still in its log.
    pub changes: Option<Vec<Vec<WireEdit>>>,
    /// The revision `changes` start after.
    pub since: Option<u64>,
}

impl Text {
    /// The changes from revision `from` to the text's, if known.
    fn history_from(&self, from: u64) -> Option<Vec<Vec<TextEdit>>> {
        let (since, changes) = (self.since?, self.changes.as_ref()?);
        let skip = usize::try_from(from.checked_sub(since)?).ok()?;
        let tail = changes.get(skip..)?;
        Some(
            tail.iter()
                .map(|change| {
                    change
                        .iter()
                        .map(|e| TextEdit::new(e.start..e.end, e.text.clone()))
                        .collect()
                })
                .collect(),
        )
    }

    fn document(&self, path: &str) -> Option<Document> {
        Language::detect(path, &self.text).map(|lang| Document::new(lang, self.text.clone()))
    }

    fn snapshot<'a>(&'a self, doc: Option<&'a Document>) -> Snapshot<'a> {
        let snap = match doc {
            Some(doc) => Snapshot::from_document(doc),
            None => Snapshot::new(&self.text),
        };
        match self.buffer {
            Some((_, rev)) => snap.at_rev(rev),
            None => snap,
        }
    }
}

/// An anchor whose revision is one of a buffer's: changes since it map it
/// exactly.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tracked {
    /// The buffer.
    pub buffer: BufferId,
    /// The anchor, at one of its revisions (`anchor.rev`).
    pub anchor: Anchor,
}

/// One anchor to resolve.
#[derive(Clone, Debug)]
pub(crate) struct Job {
    /// As asked.
    pub original: Anchor,
    /// As last rebased, if it moved.
    pub current: Option<Anchor>,
    /// As tracked in an open buffer.
    pub tracked: Option<Tracked>,
}

/// What resolving one anchor found.
#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    /// Where it is now.
    pub resolution: AnchorResolution,
    /// How to track it in the buffer from here on (`None` when the text is
    /// the file on disk, or the anchor was not found exactly).
    pub tracked: Option<Tracked>,
    /// A new `current` to store: the anchor rebased where it moved to.
    pub moved_to: Option<Anchor>,
}

/// Anchor whole `lines` (0-based) of every range in `lines` of `text`
/// (the file `path`, relative to its root), parsing it once.
pub(crate) fn make(
    path: &str,
    text: &Text,
    lines: &[RangeInclusive<usize>],
) -> Result<Vec<(Anchor, Option<Tracked>)>, AnchorError> {
    let doc = text.document(path);
    let snap = text.snapshot(doc.as_ref());
    lines
        .iter()
        .map(|range| {
            let anchor = Anchor::lines(path, &snap, range.clone())?;
            let tracked = text.buffer.map(|(buffer, _)| Tracked {
                buffer,
                anchor: anchor.clone(),
            });
            Ok((anchor, tracked))
        })
        .collect()
}

/// How many lines `text` has, as anchors count them.
pub(crate) fn line_count(text: &str) -> usize {
    Snapshot::new(text).line_count()
}

/// Resolve `jobs` (anchors in the file `path`) against `text`.
pub(crate) fn resolve(path: &str, text: &Text, jobs: &[Job]) -> Vec<Outcome> {
    let doc = text.document(path);
    let snap = text.snapshot(doc.as_ref());
    jobs.iter()
        .map(|job| resolve_one(&snap, text, job))
        .collect()
}

fn resolve_one(snap: &Snapshot<'_>, text: &Text, job: &Job) -> Outcome {
    let buffer = text.buffer.map(|(id, _)| id);
    let tracked = job
        .tracked
        .as_ref()
        .filter(|t| Some(t.buffer) == buffer)
        .and_then(|t| Some((t, text.history_from(t.anchor.rev?)?)));
    let (anchor, resolution, with_history) = match tracked {
        Some((t, history)) => {
            let r = snap.resolve(&t.anchor, Some(&history));
            (t.anchor.clone(), r, true)
        }
        None => {
            let base = job.current.as_ref().unwrap_or(&job.original);
            let r = snap.resolve(base, None);
            if r.status == Status::Orphaned && job.current.is_some() {
                // The code it last moved to is gone (unsaved edits that
                // were discarded, say): look for it as asked.
                let original = snap.resolve(&job.original, None);
                (job.original.clone(), original, false)
            } else {
                (base.clone(), r, false)
            }
        }
    };
    let rebased = match resolution.status {
        Status::Exact | Status::Moved => anchor.rebased(snap, &resolution),
        Status::Fuzzy { .. } | Status::Orphaned => None,
    };
    let moved_to = rebased
        .as_ref()
        .filter(|_| resolution.status == Status::Moved)
        .cloned();
    let tracked = match (buffer, rebased) {
        (Some(buffer), Some(anchor)) => Some(Tracked { buffer, anchor }),
        // Mapped through the changes but edited or deleted: keep mapping
        // from the same revision, so an undo finds it again.
        (Some(buffer), None) if with_history => Some(Tracked { buffer, anchor }),
        _ => None,
    };
    Outcome {
        resolution: resolution_wire(&resolution, text.buffer),
        tracked,
        moved_to,
    }
}

/// An anchor that could not be looked for (the file is gone or binary).
pub(crate) fn orphaned() -> AnchorResolution {
    AnchorResolution {
        status: AnchorStatus::Orphaned,
        ..AnchorResolution::default()
    }
}

fn point(p: coqo_anchors::Position) -> AnchorPoint {
    AnchorPoint {
        line: u32::try_from(p.line).unwrap_or(u32::MAX),
        col: u32::try_from(p.col).unwrap_or(u32::MAX),
        offset: p.offset as u64,
    }
}

fn span(s: Span) -> AnchorSpan {
    AnchorSpan {
        start: point(s.start),
        end: point(s.end),
    }
}

fn status(s: Status) -> AnchorStatus {
    match s {
        Status::Exact => AnchorStatus::Exact,
        Status::Moved => AnchorStatus::Moved,
        Status::Fuzzy { confidence } => AnchorStatus::Fuzzy { confidence },
        Status::Orphaned => AnchorStatus::Orphaned,
    }
}

fn resolution_wire(r: &Resolution, buffer: Option<(BufferId, u64)>) -> AnchorResolution {
    AnchorResolution {
        status: status(r.status),
        range: r.range.map(span),
        buffer: buffer.map(|(id, _)| id),
        rev: buffer.map(|(_, rev)| rev),
    }
}

/// An anchor's wire form.
pub(crate) fn code_anchor(root: RootId, a: &Anchor) -> CodeAnchor {
    CodeAnchor {
        root,
        path: a.path.clone(),
        rev: a.rev,
        range: span(a.range),
        text_hash: a.text_hash.clone(),
        text: a.text.clone(),
        prefix: a.prefix.clone(),
        suffix: a.suffix.clone(),
        context_before: a.context_before.clone(),
        context_after: a.context_after.clone(),
        symbol: a.symbol.as_ref().map(|s| AnchorSymbol {
            path: s.path.clone(),
            kind: s.kind.clone(),
            line_offset: u32::try_from(s.line_offset).unwrap_or(u32::MAX),
        }),
    }
}

/// The anchor a wire form describes (the inverse of [`code_anchor`]), for
/// anchors kept in their wire form, like walkthrough steps'.
pub(crate) fn from_wire(a: &CodeAnchor) -> Anchor {
    let pos = |p: AnchorPoint| coqo_anchors::Position {
        line: p.line as usize,
        col: p.col as usize,
        offset: usize::try_from(p.offset).unwrap_or(usize::MAX),
    };
    Anchor {
        path: a.path.clone(),
        rev: a.rev,
        range: Span {
            start: pos(a.range.start),
            end: pos(a.range.end),
        },
        text_hash: a.text_hash.clone(),
        text: a.text.clone(),
        prefix: a.prefix.clone(),
        suffix: a.suffix.clone(),
        context_before: a.context_before.clone(),
        context_after: a.context_after.clone(),
        symbol: a.symbol.as_ref().map(|s| coqo_anchors::SymbolPath {
            path: s.path.clone(),
            kind: s.kind.clone(),
            line_offset: s.line_offset as usize,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";

    fn buffer_text(text: &str, rev: u64) -> Text {
        Text {
            text: text.into(),
            buffer: Some((BufferId(1), rev)),
            ..Text::default()
        }
    }

    #[test]
    fn anchors_follow_buffer_changes_exactly_and_rebase() {
        let made = make(
            "src/main.rs",
            &buffer_text(SRC, 4),
            &[RangeInclusive::new(1, 1)],
        )
        .unwrap();
        let (anchor, tracked) = made.into_iter().next().unwrap();
        assert_eq!(anchor.text, "    let x = 1;\n");
        assert_eq!(anchor.symbol.as_ref().unwrap().path, ["main"]);
        let tracked = tracked.unwrap();
        assert_eq!(tracked.anchor.rev, Some(4));

        // A line typed above it (rev 5).
        let after = SRC.replacen("{\n", "{\n    // one\n", 1);
        let text = Text {
            changes: Some(vec![vec![WireEdit::insert(12, "    // one\n")]]),
            since: Some(4),
            ..buffer_text(&after, 5)
        };
        let job = Job {
            original: anchor.clone(),
            current: None,
            tracked: Some(tracked),
        };
        let out = resolve("src/main.rs", &text, std::slice::from_ref(&job)).remove(0);
        assert_eq!(out.resolution.status, AnchorStatus::Moved);
        assert_eq!(out.resolution.range.unwrap().start.line, 2);
        assert_eq!(
            (out.resolution.buffer, out.resolution.rev),
            (Some(BufferId(1)), Some(5))
        );
        let moved = out.moved_to.unwrap();
        assert_eq!((moved.rev, moved.range.start.line), (Some(5), 2));
        assert_eq!(out.tracked.unwrap().anchor, moved);

        // In another buffer lifetime the revisions mean nothing: the same
        // anchor is found by its fingerprint.
        let reopened = Text {
            buffer: Some((BufferId(2), 5)),
            ..buffer_text(&after, 5)
        };
        let out = resolve("src/main.rs", &reopened, &[job]).remove(0);
        assert_eq!(out.resolution.status, AnchorStatus::Moved);
        assert_eq!(out.resolution.range.unwrap().start.line, 2);
    }

    #[test]
    fn deleted_code_is_orphaned_and_the_original_is_kept() {
        let disk = Text {
            text: SRC.into(),
            ..Text::default()
        };
        let (anchor, tracked) = make("src/main.rs", &disk, &[RangeInclusive::new(1, 1)])
            .unwrap()
            .remove(0);
        assert!(tracked.is_none(), "a file on disk is not tracked");
        let gone = Text {
            text: "fn main() {\n    println!(\"hi\");\n}\n".into(),
            ..Text::default()
        };
        let job = Job {
            original: anchor,
            current: None,
            tracked: None,
        };
        let out = resolve("src/main.rs", &gone, &[job]).remove(0);
        assert_eq!(out.resolution.status, AnchorStatus::Orphaned);
        assert!(out.resolution.range.is_none() && out.moved_to.is_none());
    }

    #[test]
    fn the_wire_form_round_trips() {
        let disk = Text {
            text: SRC.into(),
            ..Text::default()
        };
        let (anchor, _) = make("src/main.rs", &disk, &[RangeInclusive::new(1, 2)])
            .unwrap()
            .remove(0);
        assert!(anchor.symbol.is_some());
        assert_eq!(from_wire(&code_anchor(RootId(0), &anchor)), anchor);
    }

    #[test]
    fn lines_past_the_end_are_refused() {
        let disk = Text {
            text: SRC.into(),
            ..Text::default()
        };
        assert!(make("a.rs", &disk, &[RangeInclusive::new(9, 12)]).is_err());
        assert_eq!(line_count(SRC), 5);
    }
}
