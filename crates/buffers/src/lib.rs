// SPDX-License-Identifier: Apache-2.0

//! Replicated text buffers (ADR 0006, issue #40): a daemon-authoritative
//! op-log with optimistic local apply, ot.js-style, over UTF-8 byte offsets.
//!
//! - [`op`]: [`TextOp`], retain / insert / delete operations with apply,
//!   compose, transform and invert.
//! - [`diff`](mod@diff): whole-text changes (files written by agents) as
//!   operations.
//! - [`text`]: file bytes ↔ buffer text (UTF-8, byte-order mark, line
//!   endings).
//! - [`server`]: [`ServerBuffer`], the daemon's authoritative buffer: log,
//!   transforms, disk baseline and merges, save state, revert, anchors.
//! - [`replica`]: [`ClientReplica`], what a UI editor drives: local edits,
//!   incoming changes, per-side undo, reconnect and resync. No GPUI
//!   dependency.
//!
//! The wire types (`buffer/*`) are in [`coqo_protocol::methods::buffer`];
//! `coqod` serves them.
//!
//! ```
//! use coqo_buffers::{ClientReplica, ServerBuffer, UndoGrouping, text};
//! use coqo_protocol::methods::buffer::{BufferId, OpenResult, Origin, ReplicaId, TextEdit};
//!
//! let mut daemon = ServerBuffer::new(text::decode(b"hello\n".to_vec()).unwrap());
//! let open = OpenResult {
//!     buffer: BufferId(1),
//!     replica: ReplicaId("r-1".into()),
//!     rev: daemon.rev(),
//!     text: daemon.text().to_string(),
//!     line_ending: daemon.line_ending(),
//!     encoding: daemon.encoding(),
//!     saved_rev: daemon.saved_rev(),
//!     dirty: false,
//!     last_seq: 0,
//! };
//! let mut window = ClientReplica::new(&open);
//!
//! // Typing applies locally at once and yields a `buffer/edit` to send.
//! let edit = window
//!     .local_edit(&[TextEdit::insert(5, ", world")], UndoGrouping::NewGroup)?
//!     .unwrap();
//! // Meanwhile an agent edits in the daemon, at the head.
//! let agent = daemon.apply_head(Origin::agent("s1"), &[TextEdit::insert(0, "// hi\n")])?;
//! // The daemon transforms the window's edit past the agent's.
//! let ack = daemon
//!     .apply_client(&edit.replica, edit.client_seq, edit.base_rev, &edit.edits)?
//!     .unwrap();
//!
//! // The window transforms the agent's change past its unacknowledged edit.
//! let incoming = window.receive(&agent)?;
//! assert_eq!(incoming.edits, vec![TextEdit::insert(0, "// hi\n")]);
//! assert!(window.receive(&ack)?.acknowledged);
//! assert_eq!(window.text().to_string(), "// hi\nhello, world\n");
//! assert_eq!(daemon.text().to_string(), "// hi\nhello, world\n");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod diff;
pub mod op;
pub mod replica;
pub mod server;
pub mod text;

pub use diff::diff;
pub use op::{Comp, OpError, TextOp};
pub use replica::{Applied, ClientReplica, Incoming, ReplicaError, UndoGrouping};
pub use ropey::Rope;
pub use server::{Anchor, AnchorId, BufferError, LogEntry, LogLimits, ServerBuffer, Snapshot};
