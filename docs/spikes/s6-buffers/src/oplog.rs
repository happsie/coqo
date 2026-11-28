//! Model A: daemon-authoritative op-log with optimistic local apply (ot.js client/server).
//!
//! * The daemon (`Server`) owns the text and a revision counter. Every accepted change gets
//!   the next revision and is broadcast to every subscriber, the origin included (that copy is
//!   the origin's ack).
//! * A UI replica (`Client`) applies its own edits immediately, sends at most one
//!   unacknowledged change at a time and buffers the rest (composed into one op). Remote
//!   changes are transformed against what is in flight before they touch the editor.
//! * Agents edit on the daemon side, at the head revision, so they never need transforms.

use std::collections::VecDeque;

use ropey::Rope;

use crate::ot::{Edit, OtError, TextOp};

pub type ClientId = u32;
/// The daemon's own edits (agents via MCP / ACP `fs/write_text_file`, reload from disk).
pub const DAEMON: ClientId = 0;

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub rev: u64,
    pub origin: ClientId,
    pub seq: u64,
    pub op: TextOp,
    /// Kept so a single change can be reverted later (agent edit "reject").
    pub inverse: TextOp,
}

/// Broadcast for every accepted change: `buffer/changed`.
#[derive(Clone, Debug)]
pub struct Changed {
    pub rev: u64,
    pub origin: ClientId,
    pub seq: u64,
    pub edits: Vec<Edit>,
}

pub struct Server {
    pub doc: Rope,
    pub rev: u64,
    /// Changes after `log_base`; trimmed to the oldest revision a connected client may still
    /// send against (its last seen rev), so memory is bounded by in-flight work, not history.
    pub log: VecDeque<LogEntry>,
    pub log_base: u64,
    /// Daemon-side anchors (question anchors, walkthrough steps, agent cursors).
    pub anchors: Vec<Anchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub start: usize,
    pub end: usize,
}

impl Server {
    pub fn new(text: &str) -> Self {
        Self {
            doc: Rope::from_str(text),
            rev: 0,
            log: VecDeque::new(),
            log_base: 0,
            anchors: Vec::new(),
        }
    }

    /// Accept a client change made against `base_rev`.
    pub fn receive(
        &mut self,
        origin: ClientId,
        seq: u64,
        base_rev: u64,
        edits: &[Edit],
    ) -> Result<Changed, OtError> {
        if base_rev < self.log_base || base_rev > self.rev {
            return Err(OtError("base revision no longer in the log: resync"));
        }
        // The client's op was built against the document at `base_rev`.
        let base_len = self.len_at(base_rev);
        let mut op = TextOp::from_edits(base_len, edits)?;
        let skip = (base_rev - self.log_base) as usize;
        for e in self.log.iter().skip(skip) {
            op = TextOp::transform(&op, &e.op)?.0;
        }
        Ok(self.commit(origin, seq, op))
    }

    /// A daemon-side edit at the head revision (agent write, external reload).
    pub fn local_edit(&mut self, edits: &[Edit]) -> Result<Changed, OtError> {
        let op = TextOp::from_edits(self.doc.len(), edits)?;
        Ok(self.commit(DAEMON, 0, op))
    }

    fn commit(&mut self, origin: ClientId, seq: u64, op: TextOp) -> Changed {
        let inverse = op.invert(&self.doc);
        op.apply(&mut self.doc).expect("transformed op fits head");
        for a in &mut self.anchors {
            a.start = op.transform_offset(a.start, false);
            a.end = op.transform_offset(a.end, true).max(a.start);
        }
        self.rev += 1;
        let edits = op.to_edits();
        self.log.push_back(LogEntry {
            rev: self.rev,
            origin,
            seq,
            op,
            inverse,
        });
        Changed {
            rev: self.rev,
            origin,
            seq,
            edits,
        }
    }

    fn len_at(&self, rev: u64) -> usize {
        let skip = (rev - self.log_base) as usize;
        match self.log.get(skip) {
            Some(e) => e.op.base_len,
            None => self.doc.len(),
        }
    }

    /// Forget log entries every client has seen.
    pub fn trim_log(&mut self, min_client_rev: u64) {
        while self.log_base < min_client_rev && !self.log.is_empty() {
            self.log.pop_front();
            self.log_base += 1;
        }
    }

    /// Selective undo of one logged change ("reject this agent edit"): its inverse
    /// (stored at commit time), transformed through everything that happened since.
    pub fn revert(&mut self, rev: u64) -> Result<Changed, OtError> {
        let idx = (rev - self.log_base - 1) as usize;
        let mut undo = self.log[idx].inverse.clone();
        for e in self.log.iter().skip(idx + 1) {
            undo = TextOp::transform(&undo, &e.op)?.0;
        }
        Ok(self.commit(DAEMON, 0, undo))
    }
}

#[derive(Clone, Debug)]
enum State {
    Synced,
    Awaiting(TextOp),
    AwaitingWithBuffer(TextOp, TextOp),
}

/// A UI-side replica. In Coqo the document *is* the editor's rope (gpui-component's
/// `InputState` text); the client only keeps the in-flight ops, the revision and the undo
/// stack, so the UI holds no second copy of the text.
pub struct Client {
    pub id: ClientId,
    pub doc: Rope,
    pub rev: u64,
    state: State,
    next_seq: u64,
    in_flight_seq: u64,
    /// Inverse ops of this side's own edits, top = last; transformed by every remote change.
    undo: Vec<TextOp>,
    redo: Vec<TextOp>,
}

/// What the client wants sent: `buffer/edit`.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub base_rev: u64,
    pub seq: u64,
    pub edits: Vec<Edit>,
}

impl Client {
    pub fn new(id: ClientId, text: &str, rev: u64) -> Self {
        Self {
            id,
            doc: Rope::from_str(text),
            rev,
            state: State::Synced,
            next_seq: 1,
            in_flight_seq: 0,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn is_synced(&self) -> bool {
        matches!(self.state, State::Synced)
    }

    /// A local edit (from `InputEditsEvent`): apply now, maybe send.
    pub fn local_edit(&mut self, edits: &[Edit]) -> Result<Option<Outgoing>, OtError> {
        let op = TextOp::from_edits(self.doc.len(), edits)?;
        self.undo.push(op.invert(&self.doc));
        self.redo.clear();
        self.apply_local(op)
    }

    fn apply_local(&mut self, op: TextOp) -> Result<Option<Outgoing>, OtError> {
        op.apply(&mut self.doc)?;
        let (state, out) = match std::mem::replace(&mut self.state, State::Synced) {
            State::Synced => {
                let out = self.outgoing(&op);
                (State::Awaiting(op), Some(out))
            }
            State::Awaiting(inflight) => (State::AwaitingWithBuffer(inflight, op), None),
            State::AwaitingWithBuffer(inflight, buf) => (
                State::AwaitingWithBuffer(inflight, TextOp::compose(&buf, &op)?),
                None,
            ),
        };
        self.state = state;
        Ok(out)
    }

    fn outgoing(&mut self, op: &TextOp) -> Outgoing {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.in_flight_seq = seq;
        Outgoing {
            base_rev: self.rev,
            seq,
            edits: op.to_edits(),
        }
    }

    /// A `buffer/changed` from the daemon. Returns the edits to apply to the editor (empty
    /// for our own ack) and maybe the buffered change to send next.
    pub fn receive(&mut self, msg: &Changed) -> Result<(Vec<Edit>, Option<Outgoing>), OtError> {
        if msg.rev != self.rev + 1 {
            return Err(OtError("gap in revisions: resubscribe from last rev"));
        }
        self.rev = msg.rev;
        if msg.origin == self.id && msg.seq == self.in_flight_seq {
            let (state, out) = match std::mem::replace(&mut self.state, State::Synced) {
                State::Awaiting(_) => (State::Synced, None),
                State::AwaitingWithBuffer(_, buf) => {
                    let out = self.outgoing(&buf);
                    (State::Awaiting(buf), Some(out))
                }
                State::Synced => return Err(OtError("ack without in-flight op")),
            };
            self.state = state;
            return Ok((Vec::new(), out));
        }
        // Remote change: built against the daemon doc at `rev - 1`, i.e. our doc minus
        // whatever we have in flight / buffered.
        let base_len = match &self.state {
            State::Synced => self.doc.len(),
            State::Awaiting(a) => a.base_len,
            State::AwaitingWithBuffer(a, _) => a.base_len,
        };
        let mut remote = TextOp::from_edits(base_len, &msg.edits)?;
        let state = match std::mem::replace(&mut self.state, State::Synced) {
            State::Synced => State::Synced,
            State::Awaiting(a) => {
                let (a2, r2) = TextOp::transform(&a, &remote)?;
                remote = r2;
                State::Awaiting(a2)
            }
            State::AwaitingWithBuffer(a, b) => {
                let (a2, r1) = TextOp::transform(&a, &remote)?;
                let (b2, r2) = TextOp::transform(&b, &r1)?;
                remote = r2;
                State::AwaitingWithBuffer(a2, b2)
            }
        };
        self.state = state;
        remote.apply(&mut self.doc)?;
        transform_stack(&mut self.undo, &remote)?;
        transform_stack(&mut self.redo, &remote)?;
        Ok((remote.to_edits(), None))
    }

    /// Undo this side's last edit only (agent and other-window edits are untouched).
    pub fn undo(&mut self) -> Result<Option<(Vec<Edit>, Option<Outgoing>)>, OtError> {
        let Some(inv) = self.undo.pop() else {
            return Ok(None);
        };
        self.redo.push(inv.invert(&self.doc));
        let edits = inv.to_edits();
        let out = self.apply_local(inv)?;
        Ok(Some((edits, out)))
    }

    /// Where a local offset (cursor, selection end) goes after `edits` were applied.
    pub fn map_offset(edits: &[Edit], base_len: usize, pos: usize, stick_right: bool) -> usize {
        TextOp::from_edits(base_len, edits)
            .map(|op| op.transform_offset(pos, stick_right))
            .unwrap_or(pos)
    }

    /// After a reconnect: everything not yet acknowledged, as one change against `rev`.
    pub fn pending(&self) -> Option<TextOp> {
        match &self.state {
            State::Synced => None,
            State::Awaiting(a) => Some(a.clone()),
            State::AwaitingWithBuffer(a, b) => TextOp::compose(a, b).ok(),
        }
    }

    /// Resend after a reconnect: the daemon replays `buffer/changed` after `self.rev`
    /// (the client transforms its pending ops against them as usual), then the client
    /// resends its in-flight op with a new seq.
    pub fn resend(&mut self) -> Option<Outgoing> {
        let pending = self.pending()?;
        let out = self.outgoing(&pending);
        self.state = State::Awaiting(pending);
        Some(out)
    }
}

/// Transform an undo stack (top = last, each entry applying to the doc after the ones above
/// it were undone) against a change applied to the current document.
fn transform_stack(stack: &mut [TextOp], op: &TextOp) -> Result<(), OtError> {
    let mut op = op.clone();
    for entry in stack.iter_mut().rev() {
        let (e2, o2) = TextOp::transform(entry, &op)?;
        *entry = e2;
        op = o2;
    }
    Ok(())
}
