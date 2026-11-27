// SPDX-License-Identifier: Apache-2.0

//! The daemon's authoritative buffer: [`ServerBuffer`].
//!
//! It owns the text, the revision `rev`, a log of recent changes (each with
//! its inverse), the file as it was last read or written (the **disk
//! baseline**), the unsaved difference between the two, and daemon-side
//! anchors. Everything is synchronous and in memory; `coqod` wraps it with
//! I/O, the journal and the protocol.
//!
//! **Changes.**
//! - A replica's change is made against some `base_rev`; it is transformed
//!   past every change committed since ([`apply_client`](ServerBuffer::apply_client)).
//! - Daemon-side changes (agents, formatters) are made at the head
//!   ([`apply_head`](ServerBuffer::apply_head), [`replace_text`](ServerBuffer::replace_text)).
//! - A new version of the file on disk is diffed against the **disk
//!   baseline**, not the live text, and merged around the unsaved edits
//!   ([`apply_disk`](ServerBuffer::apply_disk)).
//!
//! **Unsaved edits** are kept as one operation `unsaved` from the disk
//! baseline to the text. Every commit is composed into it; a save resets
//! it. The buffer is dirty exactly when `unsaved` changes something, so
//! typing a character and deleting it again leaves the buffer clean.
//!
//! A disk change `D` (baseline → new file) merges by transforming it
//! against `unsaved`: `(D', U') = transform(D, U)`. `D'` is committed at the
//! head and `U'` becomes the new `unsaved`, so the user's unsaved edits
//! survive and the next disk change is again diffed against the file.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;

use coqo_protocol::methods::buffer::{
    BufferChange, LineEnding, Origin, OriginKind, ReplicaId, TextEdit, TextEncoding,
};
use ropey::Rope;

use crate::diff::diff;
use crate::op::{OpError, TextOp};
use crate::text::Decoded;

/// One committed change, kept for transforming late client edits and for
/// [`revert`](ServerBuffer::revert).
#[derive(Clone, Debug)]
pub struct LogEntry {
    /// The revision it produced.
    pub rev: u64,
    /// Who made it.
    pub origin: Origin,
    /// The replica's seq (client changes), else 0.
    pub client_seq: u64,
    /// The change, against the text at `rev - 1`.
    pub op: TextOp,
    /// Its inverse, against the text at `rev`.
    pub inverse: TextOp,
}

/// Why a change was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BufferError {
    /// The revision is no longer in the log.
    #[error(
        "revision {rev} is no longer in the buffer's log (oldest is {oldest}); reopen the buffer"
    )]
    RevisionGone {
        /// The revision asked for.
        rev: u64,
        /// The oldest revision still usable.
        oldest: u64,
    },
    /// The revision is newer than the buffer.
    #[error("revision {rev} is ahead of the buffer (at {head})")]
    FutureRevision {
        /// The revision asked for.
        rev: u64,
        /// The buffer's revision.
        head: u64,
    },
    /// The edits do not fit the text.
    #[error("invalid edit: {0}")]
    InvalidEdit(#[from] OpError),
    /// `client_seq` must start at 1.
    #[error("client_seq must be at least 1")]
    ZeroSeq,
    /// The change cannot be replayed from the journal.
    #[error("cannot replay a {0:?} change; the journal needs a snapshot for it")]
    NotReplayable(OriginKind),
}

/// How much history the log keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogLimits {
    /// Always keep at least this many entries (so recent agent changes stay
    /// revertible even with no replica open).
    pub keep: usize,
    /// Never keep more than this many; replicas whose base is older must
    /// reopen.
    pub max: usize,
}

impl Default for LogLimits {
    fn default() -> Self {
        Self {
            keep: 1024,
            max: 10_000,
        }
    }
}

/// What a commit does with `unsaved`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unsaved {
    /// Compose the change into it (every change except disk merges and saves).
    Compose,
    /// The caller has set it already.
    Keep,
}

#[derive(Clone, Copy, Debug, Default)]
struct ReplicaInfo {
    /// Highest committed `client_seq`.
    last_seq: u64,
    /// Lowest revision its next change can be based on.
    last_base: u64,
}

/// Identifies a daemon-side anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnchorId(pub u64);

/// A byte range that follows edits: the start sticks left, the end sticks
/// right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// Start offset.
    pub start: usize,
    /// End offset.
    pub end: usize,
    /// The revision the offsets are for.
    pub rev: u64,
    /// A non-empty range whose text was deleted entirely; it is now empty
    /// at the deletion point (`coqo-anchors` marks it orphaned).
    pub collapsed: bool,
}

/// Everything needed to rebuild a [`ServerBuffer`] (minus its log): what
/// the daemon's journal stores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The revision.
    pub rev: u64,
    /// The revision of the last save or reload.
    pub saved_rev: u64,
    /// The disk baseline.
    pub disk: String,
    /// The unsaved edits, against `disk`.
    pub unsaved: Vec<TextEdit>,
    /// Encoding on disk.
    pub encoding: TextEncoding,
    /// Line-ending convention.
    pub line_ending: LineEnding,
    /// Highest committed `client_seq` per replica.
    pub replicas: Vec<(ReplicaId, u64)>,
}

/// The authoritative state of one open buffer.
#[derive(Debug)]
pub struct ServerBuffer {
    text: Rope,
    rev: u64,
    log: VecDeque<LogEntry>,
    /// The revision before the first log entry.
    log_base: u64,
    disk: Rope,
    unsaved: TextOp,
    saved_rev: u64,
    encoding: TextEncoding,
    line_ending: LineEnding,
    replicas: HashMap<ReplicaId, ReplicaInfo>,
    anchors: BTreeMap<AnchorId, Anchor>,
    next_anchor: u64,
    limits: LogLimits,
}

impl ServerBuffer {
    /// A clean buffer for a file's contents, at revision 0.
    pub fn new(file: Decoded) -> Self {
        Self::at_rev(file, 0)
    }

    /// A clean buffer for a file's contents, at revision `rev`.
    pub fn at_rev(file: Decoded, rev: u64) -> Self {
        let text = Rope::from_str(&file.text);
        Self {
            unsaved: TextOp::identity(text.len()),
            disk: text.clone(),
            text,
            rev,
            log: VecDeque::new(),
            log_base: rev,
            saved_rev: rev,
            encoding: file.encoding,
            line_ending: file.line_ending,
            replicas: HashMap::new(),
            anchors: BTreeMap::new(),
            next_anchor: 1,
            limits: LogLimits::default(),
        }
    }

    /// Rebuild from a journal snapshot.
    pub fn restore(s: Snapshot) -> Result<Self, BufferError> {
        let disk = Rope::from_str(&s.disk);
        let unsaved = TextOp::from_edits(disk.len(), &s.unsaved)?;
        let mut text = disk.clone();
        unsaved.apply(&mut text)?;
        Ok(Self {
            text,
            rev: s.rev,
            log: VecDeque::new(),
            log_base: s.rev,
            disk,
            unsaved,
            saved_rev: s.saved_rev,
            encoding: s.encoding,
            line_ending: s.line_ending,
            replicas: s
                .replicas
                .into_iter()
                .map(|(r, last_seq)| {
                    (
                        r,
                        ReplicaInfo {
                            last_seq,
                            last_base: s.rev,
                        },
                    )
                })
                .collect(),
            anchors: BTreeMap::new(),
            next_anchor: 1,
            limits: LogLimits::default(),
        })
    }

    /// The journal snapshot of the current state.
    pub fn snapshot(&self) -> Snapshot {
        let mut replicas: Vec<_> = self
            .replicas
            .iter()
            .filter(|(_, i)| i.last_seq > 0)
            .map(|(r, i)| (r.clone(), i.last_seq))
            .collect();
        replicas.sort();
        Snapshot {
            rev: self.rev,
            saved_rev: self.saved_rev,
            disk: self.disk.to_string(),
            unsaved: self.unsaved.to_edits(),
            encoding: self.encoding,
            line_ending: self.line_ending,
            replicas,
        }
    }

    /// Change how much history is kept.
    pub fn set_limits(&mut self, limits: LogLimits) {
        self.limits = limits;
        self.trim();
    }

    /// The text.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// The current revision.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// The oldest revision a change can still be based on.
    pub fn oldest_rev(&self) -> u64 {
        self.log_base
    }

    /// The revision of the last save or reload.
    pub fn saved_rev(&self) -> u64 {
        self.saved_rev
    }

    /// Whether the text differs from the disk baseline.
    pub fn is_dirty(&self) -> bool {
        !self.unsaved.is_noop()
    }

    /// The file as last read or written.
    pub fn disk(&self) -> &Rope {
        &self.disk
    }

    /// The unsaved edits, against [`disk`](Self::disk).
    pub fn unsaved(&self) -> &TextOp {
        &self.unsaved
    }

    /// Encoding on disk.
    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    /// Line-ending convention.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// The highest `client_seq` of `replica` committed (0 if none).
    pub fn last_seq(&self, replica: &ReplicaId) -> u64 {
        self.replicas.get(replica).map_or(0, |i| i.last_seq)
    }

    /// The log entry that produced `rev`, if still kept.
    pub fn entry(&self, rev: u64) -> Option<&LogEntry> {
        let idx = rev.checked_sub(self.log_base + 1)?;
        self.log.get(usize::try_from(idx).ok()?)
    }

    /// The kept log, oldest first.
    pub fn log(&self) -> impl Iterator<Item = &LogEntry> {
        self.log.iter()
    }

    /// A replica (re)joined at the current revision: keep the log from here
    /// for it.
    pub fn join(&mut self, replica: &ReplicaId) {
        let rev = self.rev;
        let info = self.replicas.entry(replica.clone()).or_default();
        info.last_base = rev;
    }

    /// A replica left; the log no longer needs to be kept for it. Its last
    /// seq is forgotten too, so only call this when it is gone for good.
    pub fn leave(&mut self, replica: &ReplicaId) {
        self.replicas.remove(replica);
        self.trim();
    }

    /// A replica's change, made against `base_rev`. Returns `None` for a
    /// change already committed (a resend with a known seq).
    pub fn apply_client(
        &mut self,
        replica: &ReplicaId,
        client_seq: u64,
        base_rev: u64,
        edits: &[TextEdit],
    ) -> Result<Option<BufferChange>, BufferError> {
        if client_seq == 0 {
            return Err(BufferError::ZeroSeq);
        }
        if client_seq <= self.last_seq(replica) {
            return Ok(None);
        }
        if base_rev > self.rev {
            return Err(BufferError::FutureRevision {
                rev: base_rev,
                head: self.rev,
            });
        }
        if base_rev < self.log_base {
            return Err(BufferError::RevisionGone {
                rev: base_rev,
                oldest: self.log_base,
            });
        }
        let mut op = TextOp::from_edits(self.len_at(base_rev), edits)?;
        for e in self.log.iter().skip(self.index(base_rev)) {
            op = TextOp::transform(&op, &e.op)?.0;
        }
        op.check(&self.text)?;
        let info = self.replicas.entry(replica.clone()).or_default();
        info.last_seq = client_seq;
        info.last_base = info.last_base.max(base_rev);
        Ok(Some(self.commit(
            Origin::client(replica),
            client_seq,
            op,
            false,
            Unsaved::Compose,
        )))
    }

    /// A daemon-side change at the head revision (an agent, a formatter).
    pub fn apply_head(
        &mut self,
        origin: Origin,
        edits: &[TextEdit],
    ) -> Result<BufferChange, BufferError> {
        let op = TextOp::from_edits(self.text.len(), edits)?;
        op.check(&self.text)?;
        Ok(self.commit(origin, 0, op, false, Unsaved::Compose))
    }

    /// Replace the whole text at the head (e.g. ACP `fs/write_text_file`,
    /// which sends a whole new file computed from the current buffer). The
    /// new text is diffed against the live text so cursors and anchors
    /// outside the changed region stay put.
    pub fn replace_text(&mut self, origin: Origin, new_text: &Rope) -> BufferChange {
        let op = diff(&self.text, new_text);
        self.commit(origin, 0, op, false, Unsaved::Compose)
    }

    /// The file changed on disk to `new_disk`. Returns `None` if it equals
    /// the disk baseline (for example, the watcher seeing our own save).
    pub fn apply_disk(&mut self, new_disk: Rope, origin: Origin) -> Option<BufferChange> {
        if new_disk == self.disk {
            return None;
        }
        let d = diff(&self.disk, &new_disk);
        let conflict = self.is_dirty() && TextOp::overlaps(&d, &self.unsaved);
        let (d2, u2) =
            TextOp::transform(&d, &self.unsaved).expect("both are based on the disk baseline");
        self.disk = new_disk;
        self.unsaved = u2;
        let change = self.commit(origin, 0, d2, conflict, Unsaved::Keep);
        if !self.is_dirty() {
            self.saved_rev = change.rev;
        }
        Some(change)
    }

    /// The text as of `written_rev` was written to disk. Commits a `save`
    /// event (no edits) and returns it.
    ///
    /// `written` must be the text at `written_rev`; changes committed since
    /// stay unsaved.
    pub fn mark_saved(&mut self, written: Rope, written_rev: u64) -> BufferChange {
        let mut unsaved = TextOp::identity(written.len());
        let mut composed = written_rev >= self.log_base;
        if composed {
            for e in self.log.iter().skip(self.index(written_rev)) {
                match TextOp::compose(&unsaved, &e.op) {
                    Ok(u) => unsaved = u,
                    Err(_) => {
                        composed = false;
                        break;
                    }
                }
            }
        }
        if !composed {
            unsaved = diff(&written, &self.text);
        }
        self.disk = written;
        self.unsaved = unsaved;
        let change = self.commit(
            Origin::of(OriginKind::Save),
            0,
            TextOp::identity(self.text.len()),
            false,
            Unsaved::Keep,
        );
        self.saved_rev = change.rev;
        change
    }

    /// Undo the change that produced `rev`: its inverse, transformed past
    /// every later change.
    pub fn revert(&mut self, rev: u64) -> Result<BufferChange, BufferError> {
        if rev > self.rev {
            return Err(BufferError::FutureRevision {
                rev,
                head: self.rev,
            });
        }
        let Some(entry) = self.entry(rev) else {
            return Err(BufferError::RevisionGone {
                rev,
                oldest: self.log_base + 1,
            });
        };
        let mut undo = entry.inverse.clone();
        for e in self.log.iter().skip(self.index(rev)) {
            undo = TextOp::transform(&undo, &e.op)?.0;
        }
        Ok(self.commit(
            Origin {
                kind: OriginKind::Revert,
                id: Some(rev.to_string()),
            },
            0,
            undo,
            false,
            Unsaved::Compose,
        ))
    }

    /// Re-apply a journaled change (recovery after a daemon restart).
    /// `change.rev` must be the next revision.
    pub fn replay(&mut self, change: &BufferChange) -> Result<(), BufferError> {
        if change.rev != self.rev + 1 {
            return Err(BufferError::FutureRevision {
                rev: change.rev,
                head: self.rev,
            });
        }
        match change.origin.kind {
            OriginKind::Disk => return Err(BufferError::NotReplayable(OriginKind::Disk)),
            OriginKind::Save => {
                let text = self.text.clone();
                let rev = self.rev;
                self.mark_saved(text, rev);
                return Ok(());
            }
            _ => {}
        }
        let op = TextOp::from_edits(self.text.len(), &change.edits)?;
        op.check(&self.text)?;
        if change.origin.kind == OriginKind::Client
            && let Some(id) = &change.origin.id
        {
            let info = self.replicas.entry(ReplicaId(id.clone())).or_default();
            info.last_seq = info.last_seq.max(change.client_seq);
        }
        self.commit(
            change.origin.clone(),
            change.client_seq,
            op,
            change.conflict,
            Unsaved::Compose,
        );
        Ok(())
    }

    // ---- anchors ------------------------------------------------------------

    /// Track `range` (byte offsets into the current text) through every
    /// later change.
    pub fn add_anchor(&mut self, range: Range<usize>) -> Result<AnchorId, BufferError> {
        let len = self.text.len();
        if range.start > range.end || range.end > len {
            return Err(BufferError::InvalidEdit(OpError::BadEdits { len }));
        }
        let id = AnchorId(self.next_anchor);
        self.next_anchor += 1;
        self.anchors.insert(
            id,
            Anchor {
                start: range.start,
                end: range.end,
                rev: self.rev,
                collapsed: false,
            },
        );
        Ok(id)
    }

    /// An anchor's current range.
    pub fn anchor(&self, id: AnchorId) -> Option<Anchor> {
        self.anchors.get(&id).copied()
    }

    /// Stop tracking an anchor. Returns whether it existed.
    pub fn remove_anchor(&mut self, id: AnchorId) -> bool {
        self.anchors.remove(&id).is_some()
    }

    // ---- internals ----------------------------------------------------------

    /// Index of the first log entry after `rev` (`rev >= log_base`).
    fn index(&self, rev: u64) -> usize {
        usize::try_from(rev - self.log_base).unwrap_or(usize::MAX)
    }

    /// Length of the text at `rev` (`log_base <= rev <= self.rev`).
    fn len_at(&self, rev: u64) -> usize {
        match self.log.get(self.index(rev)) {
            Some(e) => e.op.base_len(),
            None => self.text.len(),
        }
    }

    /// Apply a checked `op` at the head. `unsaved` is updated by the caller
    /// for disk changes and composed here for everything else.
    fn commit(
        &mut self,
        origin: Origin,
        client_seq: u64,
        op: TextOp,
        conflict: bool,
        unsaved: Unsaved,
    ) -> BufferChange {
        let inverse = op.invert(&self.text);
        op.apply(&mut self.text)
            .expect("ops are checked against the head before commit");
        if unsaved == Unsaved::Compose {
            self.unsaved = TextOp::compose(&self.unsaved, &op).expect("lengths match");
            // Same length as the file is necessary for being clean again
            // (typed and deleted, or undone): only then is it worth looking.
            if !self.unsaved.is_noop() && self.unsaved.base_len() == self.unsaved.target_len() {
                self.unsaved = self.unsaved.simplify(&self.disk);
            }
        }
        self.rev += 1;
        for a in self.anchors.values_mut() {
            let was_empty = a.start == a.end;
            a.start = op.transform_offset(a.start, false);
            a.end = op.transform_offset(a.end, true).max(a.start);
            if !was_empty && a.start == a.end {
                a.collapsed = true;
            }
            a.rev = self.rev;
        }
        let change = BufferChange {
            rev: self.rev,
            origin: origin.clone(),
            client_seq,
            edits: op.to_edits(),
            dirty: !self.unsaved.is_noop(),
            conflict,
        };
        self.log.push_back(LogEntry {
            rev: self.rev,
            origin,
            client_seq,
            op,
            inverse,
        });
        self.trim();
        change
    }

    fn trim(&mut self) {
        let keep_from = self.rev.saturating_sub(self.limits.keep as u64);
        let needed = self
            .replicas
            .values()
            .map(|i| i.last_base)
            .min()
            .unwrap_or(self.rev);
        let trim_to = needed.min(keep_from);
        while self.log_base < trim_to && !self.log.is_empty() {
            self.log.pop_front();
            self.log_base += 1;
        }
        while self.log.len() > self.limits.max {
            self.log.pop_front();
            self.log_base += 1;
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
