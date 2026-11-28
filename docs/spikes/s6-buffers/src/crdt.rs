//! Model B: a `yrs` (Yjs port, MIT) text CRDT on every side.
//!
//! The daemon and every UI replica hold a full `yrs::Doc`. Local edits run in a transaction
//! with a local origin and produce a binary update that is relayed through the daemon.
//! Remote updates are applied and the text delta is turned back into ranged edits for the
//! editor. Undo is yrs' `UndoManager` tracking the local origin only, anchors are
//! `StickyIndex`es.

use std::sync::{Arc, Mutex};

use yrs::types::Delta;
use yrs::types::text::TextEvent;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Assoc, Doc, GetString, IndexedSequence, Observable, OffsetKind, Options, Out, ReadTxn,
    StateVector, StickyIndex, Text, TextRef, Transact, UndoManager, Update,
};

use crate::ot::Edit;

pub const LOCAL: &str = "local";
pub const REMOTE: &str = "remote";

pub struct Replica {
    pub doc: Doc,
    pub text: TextRef,
    pub undo: UndoManager,
    /// Edits for the editor, collected by the text observer for remote transactions.
    pending: Arc<Mutex<Vec<Edit>>>,
}

fn options(client_id: u64) -> Options {
    let mut o = Options::with_client_id(yrs::block::ClientID::new(client_id));
    o.offset_kind = OffsetKind::Bytes; // ropey / gpui-component use UTF-8 byte offsets
    o
}

impl Replica {
    /// An empty replica; fill it with `apply_remote(snapshot)`.
    pub fn new(client_id: u64) -> Self {
        let doc = Doc::with_options(options(client_id));
        let text = doc.get_or_insert_text("buffer");
        let pending: Arc<Mutex<Vec<Edit>>> = Arc::default();
        let sink = pending.clone();
        text.observe("editor", move |txn, e: &TextEvent| {
            let remote = txn
                .origin()
                .map(|o| o.as_ref() == REMOTE.as_bytes())
                .unwrap_or(true);
            if remote {
                let mut out = sink.lock().unwrap();
                delta_to_edits(e.delta(txn), &mut out);
            }
        });
        let mut undo = UndoManager::with_options(yrs::undo::Options {
            capture_timeout_millis: 0,
            ..Default::default()
        });
        undo.expand_scope(&doc, &text);
        undo.include_origin(LOCAL);
        Self {
            doc,
            text,
            undo,
            pending,
        }
    }

    /// The daemon creates the document from the file on disk.
    pub fn from_text(client_id: u64, s: &str) -> Self {
        let r = Self::new(client_id);
        r.text.insert(&mut r.doc.transact_mut_with(REMOTE), 0, s);
        r.take_editor_edits();
        r
    }

    pub fn string(&self) -> String {
        self.text.get_string(&self.doc.transact())
    }

    /// Local edits (pre-edit coordinates, sorted): apply and return the update to send.
    pub fn local_edit(&mut self, edits: &[Edit]) -> Vec<u8> {
        let mut txn = self.doc.transact_mut_with(LOCAL);
        for e in edits.iter().rev() {
            if e.end > e.start {
                self.text
                    .remove_range(&mut txn, e.start as u32, (e.end - e.start) as u32);
            }
            if !e.text.is_empty() {
                self.text.insert(&mut txn, e.start as u32, &e.text);
            }
        }
        txn.encode_update_v1()
    }

    /// A daemon-side edit (agent): same as local, but outside the user's undo scope.
    pub fn agent_edit(&mut self, origin: &str, edits: &[Edit]) -> Vec<u8> {
        let mut txn = self.doc.transact_mut_with(origin);
        for e in edits.iter().rev() {
            if e.end > e.start {
                self.text
                    .remove_range(&mut txn, e.start as u32, (e.end - e.start) as u32);
            }
            if !e.text.is_empty() {
                self.text.insert(&mut txn, e.start as u32, &e.text);
            }
        }
        txn.encode_update_v1()
    }

    /// Apply an update from the daemon; returns the edits the editor must apply.
    pub fn apply_remote(&mut self, update: &[u8]) -> Vec<Edit> {
        {
            let mut txn = self.doc.transact_mut_with(REMOTE);
            txn.apply_update(Update::decode_v1(update).expect("valid update"))
                .expect("apply");
        }
        self.take_editor_edits()
    }

    fn take_editor_edits(&self) -> Vec<Edit> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }

    /// Undo this side's last edit; returns the update to send (editor edits come from the
    /// observer too, but undo runs in a local transaction, so compute them here).
    pub fn undo(&mut self) -> Option<(Vec<Edit>, Vec<u8>)> {
        let before = self.doc.transact().state_vector();
        let before_text = self.string();
        if !self.undo.undo_blocking() {
            return None;
        }
        let update = self.doc.transact().encode_diff_v1(&before);
        let after = self.string();
        Some((naive_diff(&before_text, &after), update))
    }

    pub fn state_vector(&self) -> Vec<u8> {
        self.doc.transact().state_vector().encode_v1()
    }

    /// Everything the peer with state vector `sv` is missing (reconnect / snapshot).
    pub fn diff_since(&self, sv: &[u8]) -> Vec<u8> {
        self.doc
            .transact()
            .encode_diff_v1(&StateVector::decode_v1(sv).expect("sv"))
    }

    pub fn full_state(&self) -> Vec<u8> {
        self.doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    pub fn anchor(&self, offset: usize, assoc: Assoc) -> StickyIndex {
        let mut txn = self.doc.transact_mut();
        self.text
            .sticky_index(&mut txn, offset as u32, assoc)
            .expect("anchor in range")
    }

    pub fn resolve(&self, anchor: &StickyIndex) -> Option<usize> {
        anchor
            .get_offset(&self.doc.transact())
            .map(|o| o.index as usize)
    }
}

fn delta_to_edits(delta: &[Delta<Out>], out: &mut Vec<Edit>) {
    // Delta positions are in the document as it evolves; editor edits must be in pre-edit
    // coordinates, so track both cursors.
    let mut old = 0usize;
    let mut batch: Vec<Edit> = Vec::new();
    for d in delta {
        match d {
            Delta::Retain(n, _) => old += *n as usize,
            Delta::Deleted(n) => {
                let n = *n as usize;
                match batch.last_mut() {
                    Some(e) if e.end == old => e.end += n,
                    _ => batch.push(Edit::new(old..old + n, "")),
                }
                old += n;
            }
            Delta::Inserted(value, _) => {
                let s = match value {
                    Out::Any(yrs::Any::String(s)) => s.to_string(),
                    other => format!("{other:?}"),
                };
                match batch.last_mut() {
                    Some(e) if e.end == old => e.text.push_str(&s),
                    _ => batch.push(Edit::new(old..old, s)),
                }
            }
        }
    }
    // Several transactions may land before the editor drains; for the spike each
    // apply_remote drains once per update, so batches are applied in order.
    out.extend(batch);
}

/// Minimal single-hunk diff (common prefix/suffix) for the undo path.
pub fn naive_diff(a: &str, b: &str) -> Vec<Edit> {
    if a == b {
        return Vec::new();
    }
    let pre = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let mut pre = pre;
    while !a.is_char_boundary(pre) || !b.is_char_boundary(pre) {
        pre -= 1;
    }
    let max_suf = a.len().min(b.len()) - pre;
    let mut suf = a
        .bytes()
        .rev()
        .zip(b.bytes().rev())
        .take(max_suf)
        .take_while(|(x, y)| x == y)
        .count();
    while !a.is_char_boundary(a.len() - suf) || !b.is_char_boundary(b.len() - suf) {
        suf -= 1;
    }
    vec![Edit::new(pre..a.len() - suf, &b[pre..b.len() - suf])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(s: &str, edits: &[Edit]) -> String {
        let mut out = s.to_string();
        for e in edits.iter().rev() {
            out.replace_range(e.start..e.end, &e.text);
        }
        out
    }

    #[test]
    fn two_replicas_converge_and_report_byte_edits() {
        let daemon = Replica::from_text(1, "héllo wörld\n");
        let mut a = Replica::new(2);
        let mut b = Replica::new(3);
        let snap = daemon.full_state();
        a.apply_remote(&snap);
        b.apply_remote(&snap);
        let ua = a.local_edit(&[Edit::new(6..6, "big ")]);
        let ub = b.local_edit(&[Edit::new(0..6, "Hi")]);
        let (a_before, b_before) = (a.string(), b.string());
        let ea = a.apply_remote(&ub);
        let eb = b.apply_remote(&ua);
        assert_eq!(a.string(), b.string());
        // The editor edits, applied to the editor's text, reproduce the replica.
        assert_eq!(apply(&a_before, &ea), a.string());
        assert_eq!(apply(&b_before, &eb), b.string());
    }

    #[test]
    fn undo_is_per_side() {
        let daemon = Replica::from_text(1, "abc");
        let mut a = Replica::new(2);
        a.apply_remote(&daemon.full_state());
        a.local_edit(&[Edit::new(3..3, "X")]);
        let mut d = daemon;
        let u = d.agent_edit("agent", &[Edit::new(0..0, "AGENT ")]);
        a.apply_remote(&u);
        let (edits, _) = a.undo().unwrap();
        assert_eq!(a.string(), "AGENT abc");
        assert_eq!(edits, vec![Edit::new(9..10, "")]);
    }

    #[test]
    fn sticky_anchor_survives_edits() {
        let mut r = Replica::from_text(1, "fn main() {}\n");
        let start = r.anchor(3, Assoc::After);
        r.local_edit(&[Edit::new(0..0, "// hi\n")]);
        assert_eq!(r.resolve(&start), Some(9));
    }
}
