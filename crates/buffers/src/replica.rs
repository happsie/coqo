// SPDX-License-Identifier: Apache-2.0

//! The UI side of a buffer: [`ClientReplica`].
//!
//! A replica applies its own edits at once and keeps **at most one change in
//! flight**: the first local edit is sent as a `buffer/edit`, later ones are
//! composed into a buffered change and sent when the daemon acknowledges
//! the first (its own `buffer/changes` event). Changes from elsewhere (other
//! windows, agents, the disk) are transformed past whatever is still
//! unacknowledged before they reach the editor, so every replica converges
//! on the daemon's text without ever blocking typing.
//!
//! The replica has no GPUI dependency. An editor drives it like this:
//!
//! - **Local edit** (from the editor's edit event, with the edits in
//!   pre-edit byte offsets): [`local_edit`](ClientReplica::local_edit); send
//!   the returned [`EditParams`] as `buffer/edit`.
//! - **Incoming change** (a `buffer/changes` event):
//!   [`receive`](ClientReplica::receive); apply [`Incoming::edits`] to the
//!   editor *as remote edits* (no undo entry, cursors mapped, no echo), and
//!   send [`Incoming::outgoing`] if any.
//! - **Undo / redo**: [`undo`](ClientReplica::undo) and
//!   [`redo`](ClientReplica::redo) revert only this replica's own edits,
//!   never an agent's or another window's: the undo stack holds inverse
//!   operations that are transformed through every remote change.
//! - **Reconnect**: subscribe again with `since: rev()`, feed the replayed
//!   events to `receive`, then send [`resend`](ClientReplica::resend). If the
//!   daemon cannot replay (`RESUME_GAP`), forgot the buffer
//!   (`UNKNOWN_BUFFER`) or rejected an edit (`buffer/editRejected`), call
//!   `buffer/open` with `replica: Some(replica())` and pass the result to
//!   [`resync`](ClientReplica::resync).
//!
//! The replica keeps its own copy of the text as a [`Rope`]: inverses for
//! undo need the deleted text, which editors report only after the fact,
//! and a resync needs the last acknowledged text. Ropes share unchanged
//! chunks, so the acknowledged copy costs only the edited chunks.

use coqo_protocol::methods::buffer::{
    BufferChange, BufferId, EditParams, LineEnding, OpenResult, ReplicaId, TextEdit, TextEncoding,
};
use ropey::Rope;

use crate::diff::diff;
use crate::op::{OpError, TextOp};

/// Why the replica could not take an edit or a change.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplicaError {
    /// Edits that do not fit the text.
    #[error(transparent)]
    Op(#[from] OpError),
    /// A change was missed: resubscribe from [`ClientReplica::rev`] (or
    /// reopen and [`ClientReplica::resync`]).
    #[error("expected revision {expected}, got {got}: resubscribe from the last revision")]
    Gap {
        /// The revision expected next.
        expected: u64,
        /// The revision received.
        got: u64,
    },
}

/// How a local edit joins the undo history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoGrouping {
    /// Start a new undo step.
    NewGroup,
    /// Extend the last undo step (typing within a word, say).
    Extend,
    /// Not undoable (e.g. a format-on-type the editor drives itself).
    Skip,
}

/// What [`ClientReplica::receive`] produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Incoming {
    /// Edits to apply to the editor (pre-edit byte offsets of the editor's
    /// current text), as remote edits. Empty for our own acknowledgement.
    pub edits: Vec<TextEdit>,
    /// A buffered change to send now (after an acknowledgement).
    pub outgoing: Option<EditParams>,
    /// Whether this was the acknowledgement of our change in flight.
    pub acknowledged: bool,
}

/// What [`ClientReplica::undo`], [`redo`](ClientReplica::redo) and
/// [`resync`](ClientReplica::resync) produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Edits to apply to the editor (pre-edit byte offsets). For undo and
    /// redo they are the user's own action (move the cursor there); for a
    /// resync they are remote edits.
    pub edits: Vec<TextEdit>,
    /// A change to send now.
    pub outgoing: Option<EditParams>,
}

#[derive(Clone, Debug)]
enum State {
    Synced,
    /// One change sent, not yet acknowledged.
    Awaiting(TextOp),
    /// One change sent, and later local edits composed behind it.
    AwaitingWithBuffer(TextOp, TextOp),
}

/// One editor's replica of a daemon buffer. See the [module docs](self).
#[derive(Debug)]
pub struct ClientReplica {
    buffer: BufferId,
    replica: ReplicaId,
    /// The editor's text, including unacknowledged edits.
    text: Rope,
    /// The daemon's text at `rev`.
    confirmed: Rope,
    rev: u64,
    state: State,
    next_seq: u64,
    in_flight_seq: u64,
    dirty: bool,
    line_ending: LineEnding,
    encoding: TextEncoding,
    /// Inverses of this replica's own edits, last on top. Each applies to
    /// the text after the ones above it were undone.
    undo: Vec<TextOp>,
    redo: Vec<TextOp>,
}

impl ClientReplica {
    /// A replica for a `buffer/open` result.
    pub fn new(open: &OpenResult) -> Self {
        let text = Rope::from_str(&open.text);
        Self {
            buffer: open.buffer,
            replica: open.replica.clone(),
            confirmed: text.clone(),
            text,
            rev: open.rev,
            state: State::Synced,
            next_seq: open.last_seq + 1,
            in_flight_seq: 0,
            dirty: open.dirty,
            line_ending: open.line_ending,
            encoding: open.encoding,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// The buffer.
    pub fn buffer(&self) -> BufferId {
        self.buffer
    }

    /// This replica's id (pass it to `buffer/open` when reopening).
    pub fn replica(&self) -> &ReplicaId {
        &self.replica
    }

    /// The text, including local edits not yet acknowledged. It always
    /// equals the editor's text.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// The last revision received: `since` for resubscribing.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Whether every local edit has been acknowledged.
    pub fn is_synced(&self) -> bool {
        matches!(self.state, State::Synced)
    }

    /// Whether the buffer has unsaved changes, as of the last change
    /// received. Local edits not yet acknowledged make it dirty too.
    pub fn is_dirty(&self) -> bool {
        self.dirty || !self.is_synced()
    }

    /// The file's line-ending convention.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// The file's encoding.
    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    /// Whether [`undo`](Self::undo) would do something.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether [`redo`](Self::redo) would do something.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// A local edit the editor already shows (sorted, non-overlapping edits
    /// in the coordinates of the text before it). Returns the change to send
    /// now, if nothing else is in flight.
    pub fn local_edit(
        &mut self,
        edits: &[TextEdit],
        grouping: UndoGrouping,
    ) -> Result<Option<EditParams>, ReplicaError> {
        let op = TextOp::from_edits(self.text.len(), edits)?;
        op.check(&self.text)?;
        if op.is_noop() {
            return Ok(None);
        }
        match grouping {
            UndoGrouping::NewGroup => {
                self.undo.push(op.invert(&self.text));
                self.redo.clear();
            }
            UndoGrouping::Extend => {
                let inv = op.invert(&self.text);
                let step = match self.undo.pop() {
                    Some(top) => TextOp::compose(&inv, &top)?,
                    None => inv,
                };
                self.undo.push(step);
                self.redo.clear();
            }
            UndoGrouping::Skip => {
                transform_stack(&mut self.undo, &op)?;
                transform_stack(&mut self.redo, &op)?;
            }
        }
        self.apply_local(op)
    }

    /// A `buffer/changes` event. Events at or below [`rev`](Self::rev) are
    /// ignored (a replay overlap); a skipped revision is
    /// [`ReplicaError::Gap`].
    pub fn receive(&mut self, change: &BufferChange) -> Result<Incoming, ReplicaError> {
        if change.rev <= self.rev {
            return Ok(Incoming::default());
        }
        if change.rev != self.rev + 1 {
            return Err(ReplicaError::Gap {
                expected: self.rev + 1,
                got: change.rev,
            });
        }
        let own = change.origin.is_replica(&self.replica)
            && change.client_seq == self.in_flight_seq
            && !self.is_synced();
        if own {
            let (state, outgoing) = match std::mem::replace(&mut self.state, State::Synced) {
                State::Awaiting(inflight) => {
                    inflight.apply(&mut self.confirmed)?;
                    (State::Synced, None)
                }
                State::AwaitingWithBuffer(inflight, buffered) => {
                    inflight.apply(&mut self.confirmed)?;
                    self.rev = change.rev;
                    let out = self.outgoing(&buffered);
                    (State::Awaiting(buffered), Some(out))
                }
                State::Synced => unreachable!("checked above"),
            };
            self.state = state;
            self.rev = change.rev;
            self.dirty = change.dirty;
            return Ok(Incoming {
                edits: Vec::new(),
                outgoing,
                acknowledged: true,
            });
        }

        // A remote change, against the daemon text at `rev`. Work out
        // everything before touching any state.
        let remote = TextOp::from_edits(self.confirmed.len(), &change.edits)?;
        remote.check(&self.confirmed)?;
        let (state, local_remote) = match &self.state {
            State::Synced => (State::Synced, remote.clone()),
            State::Awaiting(a) => {
                let (a2, r2) = TextOp::transform(a, &remote)?;
                (State::Awaiting(a2), r2)
            }
            State::AwaitingWithBuffer(a, b) => {
                let (a2, r1) = TextOp::transform(a, &remote)?;
                let (b2, r2) = TextOp::transform(b, &r1)?;
                (State::AwaitingWithBuffer(a2, b2), r2)
            }
        };
        local_remote.check(&self.text)?;
        remote.apply(&mut self.confirmed)?;
        local_remote.apply(&mut self.text)?;
        transform_stack(&mut self.undo, &local_remote)?;
        transform_stack(&mut self.redo, &local_remote)?;
        self.state = state;
        self.rev = change.rev;
        self.dirty = change.dirty;
        Ok(Incoming {
            edits: local_remote.to_edits(),
            outgoing: None,
            acknowledged: false,
        })
    }

    /// Undo this replica's last undo step. Agent changes and other windows'
    /// edits are never undone here (agents' can be rejected with
    /// `buffer/revertChange`).
    pub fn undo(&mut self) -> Result<Option<Applied>, ReplicaError> {
        let Some(inv) = self.undo.pop() else {
            return Ok(None);
        };
        self.redo.push(inv.invert(&self.text));
        let edits = inv.to_edits();
        let outgoing = self.apply_local(inv)?;
        Ok(Some(Applied { edits, outgoing }))
    }

    /// Redo the last undone step.
    pub fn redo(&mut self) -> Result<Option<Applied>, ReplicaError> {
        let Some(op) = self.redo.pop() else {
            return Ok(None);
        };
        self.undo.push(op.invert(&self.text));
        let edits = op.to_edits();
        let outgoing = self.apply_local(op)?;
        Ok(Some(Applied { edits, outgoing }))
    }

    /// After resubscribing and receiving the replayed events: the change
    /// still in flight, to send again (same `client_seq`; the daemon drops
    /// it if the first copy did arrive).
    pub fn resend(&self) -> Option<EditParams> {
        let inflight = match &self.state {
            State::Synced => return None,
            State::Awaiting(a) | State::AwaitingWithBuffer(a, _) => a,
        };
        Some(EditParams {
            buffer: self.buffer,
            replica: self.replica.clone(),
            base_rev: self.rev,
            client_seq: self.in_flight_seq,
            edits: inflight.to_edits(),
        })
    }

    /// Catch up from a fresh `buffer/open` (opened with this replica's id)
    /// when the missed changes cannot be replayed. Whatever changed in the
    /// daemon is diffed against the last acknowledged text and merged
    /// around the local edits still pending, which are sent again.
    pub fn resync(&mut self, open: &OpenResult) -> Result<Applied, ReplicaError> {
        let committed = open.replica == self.replica && open.last_seq >= self.in_flight_seq;
        let (base, pending) = match &self.state {
            State::Synced => (self.confirmed.clone(), None),
            State::Awaiting(a) if committed => {
                let mut base = self.confirmed.clone();
                a.apply(&mut base)?;
                (base, None)
            }
            State::Awaiting(a) => (self.confirmed.clone(), Some(a.clone())),
            State::AwaitingWithBuffer(a, b) if committed => {
                let mut base = self.confirmed.clone();
                a.apply(&mut base)?;
                (base, Some(b.clone()))
            }
            State::AwaitingWithBuffer(a, b) => {
                (self.confirmed.clone(), Some(TextOp::compose(a, b)?))
            }
        };
        let new_text = Rope::from_str(&open.text);
        let remote = diff(&base, &new_text);
        let (pending, local_remote) = match pending {
            Some(p) => {
                let (p2, r2) = TextOp::transform(&p, &remote)?;
                (Some(p2), r2)
            }
            None => (None, remote),
        };
        local_remote.check(&self.text)?;
        local_remote.apply(&mut self.text)?;
        transform_stack(&mut self.undo, &local_remote)?;
        transform_stack(&mut self.redo, &local_remote)?;
        self.buffer = open.buffer;
        self.replica = open.replica.clone();
        self.confirmed = new_text;
        self.rev = open.rev;
        self.dirty = open.dirty;
        self.line_ending = open.line_ending;
        self.encoding = open.encoding;
        self.next_seq = self.next_seq.max(open.last_seq + 1);
        self.state = State::Synced;
        let outgoing = match pending {
            Some(p) if !p.is_noop() => {
                let out = self.outgoing(&p);
                self.state = State::Awaiting(p);
                Some(out)
            }
            _ => None,
        };
        Ok(Applied {
            edits: local_remote.to_edits(),
            outgoing,
        })
    }

    /// Map an offset of the daemon's text at [`rev`](Self::rev) (a
    /// daemon-side anchor resolved at that revision) into the editor's text,
    /// through the local edits not yet acknowledged.
    pub fn daemon_to_local(&self, offset: usize, stick_right: bool) -> usize {
        match &self.state {
            State::Synced => offset,
            State::Awaiting(a) => a.transform_offset(offset, stick_right),
            State::AwaitingWithBuffer(a, b) => {
                b.transform_offset(a.transform_offset(offset, stick_right), stick_right)
            }
        }
    }

    /// Where `offset` of a text of `base_len` bytes ends up after `edits`
    /// (for cursors and selections the editor maps itself).
    pub fn map_offset(
        edits: &[TextEdit],
        base_len: usize,
        offset: usize,
        stick_right: bool,
    ) -> usize {
        TextOp::from_edits(base_len, edits)
            .map(|op| op.transform_offset(offset, stick_right))
            .unwrap_or(offset)
    }

    fn apply_local(&mut self, op: TextOp) -> Result<Option<EditParams>, ReplicaError> {
        op.apply(&mut self.text)?;
        let (state, out) = match std::mem::replace(&mut self.state, State::Synced) {
            State::Synced => {
                let out = self.outgoing(&op);
                (State::Awaiting(op), Some(out))
            }
            State::Awaiting(inflight) => (State::AwaitingWithBuffer(inflight, op), None),
            State::AwaitingWithBuffer(inflight, buffered) => (
                State::AwaitingWithBuffer(inflight, TextOp::compose(&buffered, &op)?),
                None,
            ),
        };
        self.state = state;
        Ok(out)
    }

    fn outgoing(&mut self, op: &TextOp) -> EditParams {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.in_flight_seq = seq;
        EditParams {
            buffer: self.buffer,
            replica: self.replica.clone(),
            base_rev: self.rev,
            client_seq: seq,
            edits: op.to_edits(),
        }
    }
}

/// Transform an undo stack (top = last; each entry applies to the text
/// after the ones above it were undone) past a change to the current text.
fn transform_stack(stack: &mut [TextOp], op: &TextOp) -> Result<(), OpError> {
    let mut op = op.clone();
    for entry in stack.iter_mut().rev() {
        let (e2, o2) = TextOp::transform(entry, &op)?;
        *entry = e2;
        op = o2;
    }
    Ok(())
}
