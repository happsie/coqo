// SPDX-License-Identifier: Apache-2.0

//! `buffer/*`: replicated text buffers (ADR 0006, issue #40).
//!
//! The daemon owns one authoritative buffer per open file: its text, a
//! revision counter `rev`, and a log of recent changes. UI windows hold
//! **replicas** that apply their own edits at once and send them to the
//! daemon tagged with the revision they were made against; the daemon
//! transforms them past anything committed since (ot.js-style operational
//! transformation) and broadcasts the result to every subscriber.
//!
//! **Offsets** are UTF-8 byte offsets into the buffer text. [`TextEdit`]s
//! come in lists that are sorted, non-overlapping and in the coordinates of
//! the text *before* the whole list, like an LSP `TextEdit[]`.
//!
//! **Flow.**
//!
//! 1. [`Open`] a file: get the text, `rev`, a [`ReplicaId`] and the buffer id.
//! 2. Subscribe to [`Changes`] with `since: rev`. Each event is one committed
//!    change; its `seq` **is** its `rev`.
//! 3. Send local edits with the [`Edit`] notification, at most one
//!    unacknowledged change per replica. The replica's own change comes back
//!    on [`Changes`] (same `replica` in `origin.id`, same `client_seq`): that
//!    is the acknowledgement. Edits the daemon cannot accept are answered with
//!    [`EditRejected`]; reopen the buffer then.
//! 4. Reconnect: subscribe with `since: <last rev>` and resend the change
//!    still in flight (same `client_seq`, the daemon drops duplicates). On
//!    [`RESUME_GAP`](crate::codes::RESUME_GAP) or
//!    [`UNKNOWN_BUFFER`](super::codes::UNKNOWN_BUFFER), [`Open`] again with the
//!    same `replica` and rebase the pending change onto the new snapshot.
//!
//! `coqo-buffers` implements the replica side (`ClientReplica`), so UI code
//! never deals with transforms itself.
//!
//! Unsaved edits are journaled by the daemon and survive UI and daemon
//! restarts. Changes made on disk by other programs (agents such as Claude
//! Code write files themselves, ADR 0005) are merged in as `disk` or
//! `agent` changes.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::workspace::{RootId, WorkspaceId};
use crate::message::ErrorObject;
use crate::method::{Method, NotificationMethod, Topic};

/// Identifies an open buffer in the running daemon. Not stable across
/// daemon restarts or after a buffer is unloaded: reopen by path then.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct BufferId(pub u64);

impl fmt::Display for BufferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies one replica of a buffer (one editor in one window), e.g.
/// `r-3f2a9c1d0b7e4a65`. Assigned by [`Open`]; a replica keeps it across
/// reconnects and reopens so the daemon can recognise its changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct ReplicaId(pub String);

impl fmt::Display for ReplicaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ReplicaId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Replace the bytes `start..end` with `text`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TextEdit {
    /// First byte replaced (UTF-8 offset, on a character boundary).
    pub start: usize,
    /// One past the last byte replaced; `start` for a pure insert.
    pub end: usize,
    /// The replacement.
    pub text: String,
}

impl TextEdit {
    /// Replace `range` with `text`.
    pub fn new(range: std::ops::Range<usize>, text: impl Into<String>) -> Self {
        Self {
            start: range.start,
            end: range.end,
            text: text.into(),
        }
    }

    /// Insert `text` at `at`.
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self::new(at..at, text)
    }

    /// Delete `range`.
    pub fn delete(range: std::ops::Range<usize>) -> Self {
        Self::new(range, "")
    }
}

/// The file's line-ending convention, detected from its first line break.
///
/// The buffer text keeps line breaks exactly as they are on disk (mixed
/// files round-trip unchanged); this tells the editor which break to insert.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum LineEnding {
    /// `\n` (also the default for files without line breaks).
    #[default]
    Lf,
    /// `\r\n`.
    Crlf,
}

/// The file's text encoding. Only UTF-8 is supported for now; other files
/// fail to open with [`UNSUPPORTED_ENCODING`](super::codes::UNSUPPORTED_ENCODING).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum TextEncoding {
    /// UTF-8 without a byte-order mark.
    #[default]
    Utf8,
    /// UTF-8 with a byte-order mark. The mark is not part of the buffer text
    /// and is written back on save.
    Utf8Bom,
}

/// Params of [`Open`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct OpenParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// `/`-separated path of a file relative to the root.
    pub path: String,
    /// Reuse this replica id (when reopening after a reconnect or a resume
    /// gap), so the daemon reports which of its changes it already has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica: Option<ReplicaId>,
}

/// Result of [`Open`]: a snapshot of the buffer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OpenResult {
    /// The buffer. The same file opened twice (from two windows, or through
    /// a symlink) is the same buffer.
    pub buffer: BufferId,
    /// This replica's id: the one asked for, or a new one.
    pub replica: ReplicaId,
    /// The revision `text` reflects. Subscribe to [`Changes`] with `since`
    /// set to it.
    pub rev: u64,
    /// The buffer text, including unsaved edits.
    pub text: String,
    /// Line-ending convention.
    pub line_ending: LineEnding,
    /// Encoding on disk.
    pub encoding: TextEncoding,
    /// Revision of the last save or reload from disk.
    pub saved_rev: u64,
    /// Whether the text differs from the file on disk.
    pub dirty: bool,
    /// The highest `client_seq` of `replica` the daemon has committed (0 if
    /// none): a pending change with a seq at or below it is already in
    /// `text`.
    pub last_seq: u64,
}

/// Params of the [`Edit`] notification: one change made by a replica.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct EditParams {
    /// The buffer.
    pub buffer: BufferId,
    /// The replica that made the change.
    pub replica: ReplicaId,
    /// The revision the edits were made against.
    pub base_rev: u64,
    /// Per-replica sequence number, strictly increasing from 1. Resending a
    /// change keeps its seq; the daemon ignores seqs it has committed.
    pub client_seq: u64,
    /// The edits, sorted and non-overlapping, against the text at `base_rev`.
    pub edits: Vec<TextEdit>,
}

/// Who made a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum OriginKind {
    /// A replica; `id` is its [`ReplicaId`].
    Client,
    /// An agent; `id` is its session id. Agents' disk writes are attributed
    /// when they match a tool call the daemon was told about.
    Agent,
    /// The file changed on disk (another program, or an unattributed agent).
    Disk,
    /// A formatter run by the daemon.
    Format,
    /// [`RevertChange`]; `id` is the reverted revision.
    Revert,
    /// The buffer was saved: no edits, `dirty` becomes false.
    Save,
    /// Project search and replace (`search/replace`) by the user.
    Replace,
}

/// Who made a change, with an id whose meaning depends on the kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Origin {
    /// What kind of source.
    pub kind: OriginKind,
    /// The source's id, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl Origin {
    /// A change made by `replica`.
    pub fn client(replica: &ReplicaId) -> Self {
        Self {
            kind: OriginKind::Client,
            id: Some(replica.0.clone()),
        }
    }

    /// A change made by agent session `session`.
    pub fn agent(session: impl Into<String>) -> Self {
        Self {
            kind: OriginKind::Agent,
            id: Some(session.into()),
        }
    }

    /// A change with no id (`disk`, `format`, `save`).
    pub fn of(kind: OriginKind) -> Self {
        Self { kind, id: None }
    }

    /// Whether this change was made by `replica`.
    pub fn is_replica(&self, replica: &ReplicaId) -> bool {
        self.kind == OriginKind::Client && self.id.as_deref() == Some(replica.0.as_str())
    }
}

/// Params of the [`Changes`] topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// The buffer.
    pub buffer: BufferId,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One event of the [`Changes`] topic: a committed change. The event's
/// subscription `seq` equals `rev`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BufferChange {
    /// The revision this change produced (the previous one plus 1).
    pub rev: u64,
    /// Who made it.
    pub origin: Origin,
    /// The replica's `client_seq` for client changes, else 0.
    #[serde(default)]
    pub client_seq: u64,
    /// The edits, sorted and non-overlapping, against the text at `rev - 1`.
    pub edits: Vec<TextEdit>,
    /// Whether the text differs from the file on disk after this change.
    pub dirty: bool,
    /// A disk or agent change that overlapped unsaved edits; both were kept,
    /// and the user should look at the result.
    #[serde(default, skip_serializing_if = "is_false")]
    pub conflict: bool,
}

/// Params of the [`EditRejected`] notification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct EditRejectedParams {
    /// The buffer.
    pub buffer: BufferId,
    /// The replica whose change was rejected.
    pub replica: ReplicaId,
    /// The rejected change's seq.
    pub client_seq: u64,
    /// Why, e.g. [`REVISION_GONE`](super::codes::REVISION_GONE) or
    /// [`INVALID_EDIT`](super::codes::INVALID_EDIT).
    pub error: ErrorObject,
}

/// Params of [`RevertChange`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RevertParams {
    /// The buffer.
    pub buffer: BufferId,
    /// The revision whose change to undo.
    pub rev: u64,
}

/// Result of [`RevertChange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RevertResult {
    /// The revision of the reverting change.
    pub rev: u64,
}

/// Params of [`Save`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SaveParams {
    /// The buffer.
    pub buffer: BufferId,
    /// Overwrite the file even if it changed on disk since it was last read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
}

/// Result of [`Save`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    /// The revision of the save event; the file holds the text as of it.
    pub saved_rev: u64,
}

/// Params of [`Close`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CloseParams {
    /// The buffer.
    pub buffer: BufferId,
    /// The replica going away.
    pub replica: ReplicaId,
}

/// `buffer/open`: open (or join) the buffer of a file.
#[derive(Debug)]
pub enum Open {}
impl Method for Open {
    const NAME: &'static str = "buffer/open";
    type Params = OpenParams;
    type Result = OpenResult;
}

/// `buffer/edit` (client → server): a replica's change. Acknowledged by its
/// own event on [`Changes`], rejected with [`EditRejected`].
#[derive(Debug)]
pub enum Edit {}
impl NotificationMethod for Edit {
    const NAME: &'static str = "buffer/edit";
    type Params = EditParams;
}

/// `buffer/editRejected` (server → client): a [`Edit`] the daemon could not
/// apply. The replica must reopen the buffer.
#[derive(Debug)]
pub enum EditRejected {}
impl NotificationMethod for EditRejected {
    const NAME: &'static str = "buffer/editRejected";
    type Params = EditRejectedParams;
}

/// `buffer/changes`: every committed change of a buffer, `seq` = `rev`.
/// Subscribers keep the buffer loaded.
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "buffer/changes";
    type Params = ChangesParams;
    type Event = BufferChange;
}

/// `buffer/revertChange`: undo one change (typically an agent's) by
/// committing its inverse, transformed past every later change.
#[derive(Debug)]
pub enum RevertChange {}
impl Method for RevertChange {
    const NAME: &'static str = "buffer/revertChange";
    type Params = RevertParams;
    type Result = RevertResult;
}

/// `buffer/save`: write the buffer to its file (atomically).
#[derive(Debug)]
pub enum Save {}
impl Method for Save {
    const NAME: &'static str = "buffer/save";
    type Params = SaveParams;
    type Result = SaveResult;
}

/// `buffer/close`: a replica is done with the buffer. The daemon unloads it
/// once nobody subscribes to it; unsaved edits stay in the journal.
#[derive(Debug)]
pub enum Close {}
impl Method for Close {
    const NAME: &'static str = "buffer/close";
    type Params = CloseParams;
    type Result = ();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn change_wire_shape() {
        let c = BufferChange {
            rev: 7,
            origin: Origin::client(&"r-1".into()),
            client_seq: 3,
            edits: vec![TextEdit::insert(4, "é")],
            dirty: true,
            conflict: false,
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({
                "rev": 7,
                "origin": {"kind": "client", "id": "r-1"},
                "clientSeq": 3,
                "edits": [{"start": 4, "end": 4, "text": "é"}],
                "dirty": true,
            })
        );
        let back: BufferChange = serde_json::from_value(json!({
            "rev": 8, "origin": {"kind": "disk"}, "edits": [], "dirty": false, "conflict": true
        }))
        .unwrap();
        assert_eq!(back.client_seq, 0);
        assert!(back.conflict);
        assert_eq!(back.origin, Origin::of(OriginKind::Disk));
    }

    #[test]
    fn origin_matches_its_replica_only() {
        let r: ReplicaId = "r-a".into();
        assert!(Origin::client(&r).is_replica(&r));
        assert!(!Origin::client(&"r-b".into()).is_replica(&r));
        assert!(
            !Origin {
                kind: OriginKind::Agent,
                id: Some("r-a".into())
            }
            .is_replica(&r)
        );
    }
}
