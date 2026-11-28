// SPDX-License-Identifier: Apache-2.0

//! Concurrent edits converge and never lose characters (issue #40's
//! acceptance test, ported from `docs/spikes/s6-buffers/tests/convergence.rs`).
//!
//! Three replicas, an agent editing at the head, and the file changing on
//! disk under the buffer all edit the same buffer. Messages sit in per-link
//! FIFO queues and are delivered in a random order. Every replica also has
//! an *editor mirror*: a string changed only by the edits the replica hands
//! to the editor, the way the UI would.
//!
//! At quiescence the daemon, every replica and every mirror must be equal,
//! and in insert-only runs every inserted character must be present.
//! `coqod`'s tests run the same scenario against the real daemon over the
//! protocol.

use std::collections::{BTreeMap, VecDeque};

use coqo_buffers::{ClientReplica, Rope, ServerBuffer, UndoGrouping, text};
use coqo_protocol::methods::buffer::{
    BufferChange, BufferId, EditParams, OpenResult, Origin, OriginKind, ReplicaId, TextEdit,
};
use proptest::prelude::*;

const CLIENTS: usize = 3;
const PIECES: [&str; 6] = ["a", "é", "🙂", "\n", "xyz", "Q"];
const INITIAL: &str = "fn main() {\n    println!(\"héllo\");\n}\n";

#[derive(Clone, Debug)]
enum Act {
    Edit {
        who: usize,
        pos: u32,
        del: u8,
        ins: u8,
    },
    Agent {
        pos: u32,
        del: u8,
        ins: u8,
    },
    Disk {
        pos: u32,
        del: u8,
        ins: u8,
    },
    Save,
    Up {
        who: usize,
    },
    Down {
        who: usize,
    },
    Undo {
        who: usize,
    },
    Redo {
        who: usize,
    },
    Revert {
        back: u8,
    },
}

fn act(deletes: bool) -> impl Strategy<Value = Act> {
    let del = if deletes { 0u8..6 } else { 0u8..1 };
    let undo = if deletes { 1 } else { 0 };
    prop_oneof![
        5 => (0..CLIENTS, any::<u32>(), del.clone(), 0u8..6)
            .prop_map(|(who, pos, del, ins)| Act::Edit { who, pos, del, ins }),
        1 => (any::<u32>(), del.clone(), 0u8..6).prop_map(|(pos, del, ins)| Act::Agent { pos, del, ins }),
        1 => (any::<u32>(), del, 0u8..6).prop_map(|(pos, del, ins)| Act::Disk { pos, del, ins }),
        1 => Just(Act::Save),
        4 => (0..CLIENTS).prop_map(|who| Act::Up { who }),
        4 => (0..CLIENTS).prop_map(|who| Act::Down { who }),
        undo => (0..CLIENTS).prop_map(|who| Act::Undo { who }),
        undo => (0..CLIENTS).prop_map(|who| Act::Redo { who }),
        undo => any::<u8>().prop_map(|back| Act::Revert { back }),
    ]
}

fn boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A random edit valid for `s`, on character boundaries.
fn make_edit(s: &str, pos: u32, del: u8, ins: u8) -> TextEdit {
    let start = boundary(s, pos as usize % (s.len() + 1));
    let mut end = start;
    for _ in 0..del {
        match s[end..].chars().next() {
            Some(c) => end += c.len_utf8(),
            None => break,
        }
    }
    let text = if ins == 0 {
        String::new()
    } else {
        PIECES[ins as usize % PIECES.len()].repeat(ins as usize % 3 + 1)
    };
    TextEdit::new(start..end, text)
}

fn apply(s: &mut String, edits: &[TextEdit]) {
    for e in edits.iter().rev() {
        s.replace_range(e.start..e.end, &e.text);
    }
}

fn char_counts(s: &str) -> BTreeMap<char, usize> {
    let mut m = BTreeMap::new();
    for c in s.chars() {
        *m.entry(c).or_insert(0) += 1;
    }
    m
}

struct World {
    daemon: ServerBuffer,
    /// The file on disk.
    file: String,
    clients: Vec<ClientReplica>,
    editors: Vec<String>,
    up: Vec<VecDeque<EditParams>>,
    down: Vec<VecDeque<BufferChange>>,
    inserted: String,
}

fn open_result(daemon: &ServerBuffer, i: usize) -> OpenResult {
    OpenResult {
        buffer: BufferId(1),
        replica: ReplicaId(format!("r-{i}")),
        rev: daemon.rev(),
        text: daemon.text().to_string(),
        line_ending: daemon.line_ending(),
        encoding: daemon.encoding(),
        saved_rev: daemon.saved_rev(),
        dirty: daemon.is_dirty(),
        last_seq: 0,
    }
}

impl World {
    fn new() -> Self {
        let mut daemon = ServerBuffer::new(text::decode(INITIAL.as_bytes().to_vec()).unwrap());
        let clients = (0..CLIENTS)
            .map(|i| {
                let open = open_result(&daemon, i);
                daemon.join(&open.replica);
                ClientReplica::new(&open)
            })
            .collect();
        Self {
            daemon,
            file: INITIAL.to_owned(),
            clients,
            editors: vec![INITIAL.to_owned(); CLIENTS],
            up: vec![VecDeque::new(); CLIENTS],
            down: vec![VecDeque::new(); CLIENTS],
            inserted: String::new(),
        }
    }

    fn broadcast(&mut self, c: BufferChange) {
        for q in &mut self.down {
            q.push_back(c.clone());
        }
    }

    fn send(&mut self, who: usize, out: Option<EditParams>) {
        if let Some(out) = out {
            self.up[who].push_back(out);
        }
    }

    fn step(&mut self, a: &Act) {
        match *a {
            Act::Edit { who, pos, del, ins } => {
                let e = make_edit(&self.editors[who], pos, del, ins);
                self.inserted.push_str(&e.text);
                apply(&mut self.editors[who], std::slice::from_ref(&e));
                let grouping = if pos % 3 == 0 {
                    UndoGrouping::Extend
                } else {
                    UndoGrouping::NewGroup
                };
                let out = self.clients[who].local_edit(&[e], grouping).unwrap();
                self.send(who, out);
            }
            Act::Agent { pos, del, ins } => {
                let e = make_edit(&self.daemon.text().to_string(), pos, del, ins);
                self.inserted.push_str(&e.text);
                let c = self.daemon.apply_head(Origin::agent("s1"), &[e]).unwrap();
                self.broadcast(c);
            }
            Act::Disk { pos, del, ins } => {
                // Another program (or Claude Code) rewrites the file.
                let e = make_edit(&self.file, pos, del, ins);
                self.inserted.push_str(&e.text);
                apply(&mut self.file, std::slice::from_ref(&e));
                if let Some(c) = self
                    .daemon
                    .apply_disk(Rope::from_str(&self.file), Origin::of(OriginKind::Disk))
                {
                    self.broadcast(c);
                }
            }
            Act::Save => {
                let written = self.daemon.text().clone();
                let rev = self.daemon.rev();
                self.file = written.to_string();
                let c = self.daemon.mark_saved(written, rev);
                assert!(!c.dirty);
                self.broadcast(c);
            }
            Act::Up { who } => {
                if let Some(e) = self.up[who].pop_front() {
                    let c = self
                        .daemon
                        .apply_client(&e.replica, e.client_seq, e.base_rev, &e.edits)
                        .unwrap()
                        .expect("not a duplicate");
                    self.broadcast(c);
                }
            }
            Act::Down { who } => {
                if let Some(c) = self.down[who].pop_front() {
                    let inc = self.clients[who].receive(&c).unwrap();
                    apply(&mut self.editors[who], &inc.edits);
                    self.send(who, inc.outgoing);
                }
            }
            Act::Undo { who } => {
                if let Some(applied) = self.clients[who].undo().unwrap() {
                    apply(&mut self.editors[who], &applied.edits);
                    self.send(who, applied.outgoing);
                }
            }
            Act::Redo { who } => {
                if let Some(applied) = self.clients[who].redo().unwrap() {
                    apply(&mut self.editors[who], &applied.edits);
                    self.send(who, applied.outgoing);
                }
            }
            Act::Revert { back } => {
                let rev = self.daemon.rev().saturating_sub(u64::from(back % 8));
                if rev > self.daemon.oldest_rev()
                    && let Ok(c) = self.daemon.revert(rev)
                {
                    self.broadcast(c);
                }
            }
        }
    }

    /// Deliver everything, then check that all sides agree.
    fn settle(&mut self) -> String {
        loop {
            let mut moved = false;
            for who in 0..CLIENTS {
                if !self.up[who].is_empty() {
                    self.step(&Act::Up { who });
                    moved = true;
                }
                if !self.down[who].is_empty() {
                    self.step(&Act::Down { who });
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        let text = self.daemon.text().to_string();
        for (c, e) in self.clients.iter().zip(&self.editors) {
            assert!(c.is_synced());
            assert_eq!(c.rev(), self.daemon.rev());
            assert_eq!(c.text().to_string(), text, "replica diverged");
            assert_eq!(e, &text, "editor mirror diverged");
        }
        // The unsaved edits always map the file to the text.
        let mut from_disk = Rope::from_str(&self.file);
        self.daemon.unsaved().apply(&mut from_disk).unwrap();
        assert_eq!(from_disk.to_string(), text, "unsaved edits out of step");
        assert_eq!(self.daemon.disk().to_string(), self.file);
        text
    }
}

fn run(acts: &[Act]) -> (String, String) {
    let mut w = World::new();
    for a in acts {
        w.step(a);
    }
    let text = w.settle();
    (text, w.inserted)
}

fn assert_no_lost_chars(text: &str, inserted: &str) {
    let mut expected = char_counts(INITIAL);
    for (c, n) in char_counts(inserted) {
        *expected.entry(c).or_insert(0) += n;
    }
    assert_eq!(char_counts(text), expected, "characters lost or duplicated");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    #[test]
    fn replicas_converge(acts in prop::collection::vec(act(true), 1..150)) {
        run(&acts);
    }

    #[test]
    fn concurrent_inserts_are_never_lost(acts in prop::collection::vec(act(false), 1..150)) {
        let (text, inserted) = run(&acts);
        assert_no_lost_chars(&text, &inserted);
    }

    /// A replica that loses its connection keeps typing, then resubscribes
    /// (replaying what it missed) and resends its change in flight, which
    /// may or may not have reached the daemon.
    #[test]
    fn reconnect_resend_converges(
        acts in prop::collection::vec(act(true), 1..80),
        delivered in any::<bool>(),
        offline in prop::collection::vec((any::<u32>(), 0u8..3, 0u8..6), 1..30),
    ) {
        let mut w = World::new();
        for a in &acts {
            w.step(a);
        }
        // Client 0 drops: its queued edits may or may not have arrived.
        if delivered {
            while !w.up[0].is_empty() {
                w.step(&Act::Up { who: 0 });
            }
        }
        w.up[0].clear();
        w.down[0].clear();
        let seen = w.clients[0].rev();
        for (pos, del, ins) in offline {
            w.step(&Act::Edit { who: 0, pos, del, ins });
        }
        w.up[0].clear();
        // Others keep going.
        w.step(&Act::Agent { pos: 7, del: 1, ins: 2 });
        // Reconnect: replay from `seen`, then resend.
        let replay: Vec<BufferChange> = w
            .daemon
            .log()
            .filter(|e| e.rev > seen)
            .map(|e| BufferChange {
                rev: e.rev,
                origin: e.origin.clone(),
                client_seq: e.client_seq,
                edits: e.op.to_edits(),
                dirty: true,
                conflict: false,
            })
            .collect();
        w.down[0] = replay.into();
        while !w.down[0].is_empty() {
            w.step(&Act::Down { who: 0 });
        }
        w.up[0].clear(); // anything queued by acks is superseded by the resend
        if let Some(out) = w.clients[0].resend() {
            match w.daemon.apply_client(&out.replica, out.client_seq, out.base_rev, &out.edits).unwrap() {
                Some(c) => w.broadcast(c),
                None => {
                    // A duplicate: the first copy was committed, and its event
                    // is already in the replay. Nothing to do.
                }
            }
        }
        w.settle();
    }
}

/// A replica that missed too much reopens and resyncs; its pending edits are
/// rebased onto the new snapshot and nothing typed is lost.
#[test]
fn resync_rebases_pending_edits() {
    for committed in [false, true] {
        let mut w = World::new();
        w.step(&Act::Edit {
            who: 0,
            pos: 3,
            del: 0,
            ins: 1,
        });
        w.step(&Act::Edit {
            who: 0,
            pos: 9,
            del: 0,
            ins: 2,
        });
        if committed {
            w.step(&Act::Up { who: 0 });
        }
        w.up[0].clear();
        w.down[0].clear();
        for i in 0..20 {
            w.step(&Act::Agent {
                pos: i * 5,
                del: 0,
                ins: 3,
            });
        }
        let mut open = open_result(&w.daemon, 0);
        open.last_seq = w.daemon.last_seq(&open.replica);
        let applied = w.clients[0].resync(&open).unwrap();
        apply(&mut w.editors[0], &applied.edits);
        w.send(0, applied.outgoing);
        w.settle();
        let text = w.daemon.text().to_string();
        assert_eq!(text.matches('é').count(), 3, "{committed}: {text:?}");
        assert_eq!(text.matches('🙂').count(), 3, "{committed}: {text:?}");
    }
}

#[test]
fn revert_agent_change_keeps_user_edits() {
    let mut d =
        ServerBuffer::new(text::decode(b"line one\nline two\nline three\n".to_vec()).unwrap());
    let agent = d
        .apply_head(
            Origin::agent("s1"),
            &[TextEdit::new(9..17, "LINE TWO (agent)")],
        )
        .unwrap();
    let len = d.text().len();
    d.apply_head(
        Origin::client(&"r".into()),
        &[
            TextEdit::insert(0, "// top\n"),
            TextEdit::insert(len, "tail\n"),
        ],
    )
    .unwrap();
    let revert = d.revert(agent.rev).unwrap();
    assert_eq!(revert.origin.kind, OriginKind::Revert);
    assert_eq!(
        d.text().to_string(),
        "// top\nline one\nline two\nline three\ntail\n"
    );
}
