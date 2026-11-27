// SPDX-License-Identifier: Apache-2.0

//! Anchors: code locations that survive edits (PLAN §7.5, issue #72).
//!
//! One model for anchored questions, walkthrough steps, review comments and
//! trace points:
//!
//! - [`Anchor`]: a workspace-relative path, a range ([`Span`]: line, column
//!   and byte offset of each end) at a buffer revision, and a fingerprint:
//!   the anchored text and its [`text_hash`], the rest of its lines, up to
//!   [`CONTEXT_LINES`] lines of context on each side, and the innermost
//!   enclosing symbol from `coqo-syntax` ([`SymbolPath`]). It is serde
//!   serialisable; the JSON form is what the daemon stores.
//! - [`Snapshot`]: the text to anchor in or resolve against (a buffer at a
//!   revision, or a file), with its parsed [`Document`](coqo_syntax::Document)
//!   for symbols. Its indexes are shared by every anchor resolved with it.
//! - [`Snapshot::resolve`]: where an anchor is now, as a [`Resolution`] with
//!   a [`Status`]: [`Exact`](Status::Exact), [`Moved`](Status::Moved),
//!   [`Fuzzy`](Status::Fuzzy) with a confidence, or
//!   [`Orphaned`](Status::Orphaned). With the buffer's changes since the
//!   anchor's revision the range is mapped exactly ([`map_range`]);
//!   without them it is relocated by its fingerprint (exact text, then
//!   whitespace-insensitive, then fuzzy by lines, context and symbol). A
//!   match below [`MIN_CONFIDENCE`], or one tied with another place, leaves
//!   it orphaned rather than pointing at the wrong code.
//!
//! Several anchors (one question about two places) are just several
//! [`Anchor`]s; [`Snapshot::resolve_all`] resolves them together.
//!
//! ```
//! use coqo_anchors::{Anchor, Snapshot, Status};
//! use coqo_syntax::TextEdit;
//!
//! let before = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
//! let snap = Snapshot::new(before).at_rev(4);
//! let anchor = Anchor::lines("src/main.rs", &snap, 1..=1)?;
//! assert_eq!(anchor.text, "    let x = 1;\n");
//!
//! // A comment is added above it (revision 5).
//! let change = vec![TextEdit::new(12..12, "    // the answer\n")];
//! let after = "fn main() {\n    // the answer\n    let x = 1;\n    println!(\"{x}\");\n}\n";
//! let snap = Snapshot::new(after).at_rev(5);
//!
//! // With the change, the range is mapped exactly…
//! let r = snap.resolve(&anchor, Some(&[change]));
//! assert_eq!(r.status, Status::Moved);
//! assert_eq!(r.range.unwrap().start.line, 2);
//!
//! // …and without it, the anchor is found by its fingerprint.
//! let r = snap.resolve(&anchor, None);
//! assert_eq!(r.status, Status::Moved);
//! assert_eq!(&after[r.bytes().unwrap()], anchor.text);
//! # Ok::<(), coqo_anchors::AnchorError>(())
//! ```

mod map;
mod model;
mod resolve;
mod snapshot;
mod text;

pub use map::{Mapped, map_range};
pub use model::{
    Anchor, AnchorError, CONTEXT_LINES, Position, Resolution, Span, Status, SymbolPath,
};
pub use resolve::MIN_CONFIDENCE;
pub use snapshot::Snapshot;
pub use text::text_hash;
