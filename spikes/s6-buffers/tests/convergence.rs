//! Concurrent edit convergence under random interleavings, for both models.
//!
//! Three UI clients (two windows plus a reconnecting one) and a daemon-side agent edit the
//! same buffer. Messages sit in per-link FIFO queues and are delivered in a random order.
//! Each client also keeps an "editor" string that only ever changes through the edits the
//! replication layer hands it, the way gpui-component's `InputState` would.
//!
//! Checked at quiescence: every replica, every editor mirror and the daemon are equal, and
//! in insert-only runs no inserted character is ever lost.

use std::collections::VecDeque;

use proptest::prelude::*;
use spike_s6_buffers::crdt::Replica;
use spike_s6_buffers::oplog::{Changed, Client, Server};
use spike_s6_buffers::ot::Edit;

const CLIENTS: usize = 3;
const PIECES: [&str; 6] = ["a", "é", "🙂", "\n", "xyz", "Q"];

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
    Up {
        who: usize,
    },
    Down {
        who: usize,
    },
    Undo {
        who: usize,
    },
}

fn act(allow_delete: bool) -> impl Strategy<Value = Act> {
    let del = if allow_delete { 0u8..6 } else { 0u8..1 };
    prop_oneof![
        4 => (0..CLIENTS, any::<u32>(), del.clone(), 0u8..6)
            .prop_map(|(who, pos, del, ins)| Act::Edit { who, pos, del, ins }),
        1 => (any::<u32>(), del, 0u8..6).prop_map(|(pos, del, ins)| Act::Agent { pos, del, ins }),
        3 => (0..CLIENTS).prop_map(|who| Act::Up { who }),
        3 => (0..CLIENTS).prop_map(|who| Act::Down { who }),
        if allow_delete { 1 } else { 0 } => (0..CLIENTS).prop_map(|who| Act::Undo { who }),
    ]
}

fn boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A random edit valid for `s`, at char boundaries.
fn make_edit(s: &str, pos: u32, del: u8, ins: u8) -> Edit {
    let start = boundary(s, pos as usize % (s.len() + 1));
    let mut end = start;
    for _ in 0..del {
        if end >= s.len() {
            break;
        }
        end += s[end..].chars().next().unwrap().len_utf8();
    }
    let text = if ins == 0 {
        String::new()
    } else {
        PIECES[ins as usize % PIECES.len()].repeat(ins as usize % 3 + 1)
    };
    Edit::new(start..end, text)
}

fn apply(s: &mut String, edits: &[Edit]) {
    for e in edits.iter().rev() {
        s.replace_range(e.start..e.end, &e.text);
    }
}

fn char_counts(s: &str) -> std::collections::BTreeMap<char, usize> {
    let mut m = std::collections::BTreeMap::new();
    for c in s.chars() {
        *m.entry(c).or_insert(0) += 1;
    }
    m
}

const INITIAL: &str = "fn main() {\n    println!(\"héllo\");\n}\n";

/// Returns the converged text, plus everything ever inserted.
fn run_oplog(acts: &[Act]) -> (String, String) {
    let mut server = Server::new(INITIAL);
    let mut clients: Vec<Client> = (0..CLIENTS)
        .map(|i| Client::new(i as u32 + 1, INITIAL, 0))
        .collect();
    let mut editors: Vec<String> = vec![INITIAL.to_string(); CLIENTS];
    let mut up: Vec<VecDeque<(u64, u64, Vec<Edit>)>> = vec![VecDeque::new(); CLIENTS];
    let mut down: Vec<VecDeque<Changed>> = vec![VecDeque::new(); CLIENTS];
    let mut inserted = String::new();

    let broadcast = |c: Changed, down: &mut Vec<VecDeque<Changed>>| {
        for q in down.iter_mut() {
            q.push_back(c.clone());
        }
    };
    let step = |a: &Act,
                server: &mut Server,
                clients: &mut Vec<Client>,
                editors: &mut Vec<String>,
                up: &mut Vec<VecDeque<(u64, u64, Vec<Edit>)>>,
                down: &mut Vec<VecDeque<Changed>>,
                inserted: &mut String| match *a {
        Act::Edit { who, pos, del, ins } => {
            let e = make_edit(&editors[who], pos, del, ins);
            inserted.push_str(&e.text);
            apply(&mut editors[who], std::slice::from_ref(&e));
            if let Some(out) = clients[who].local_edit(&[e]).unwrap() {
                up[who].push_back((out.base_rev, out.seq, out.edits));
            }
        }
        Act::Agent { pos, del, ins } => {
            let e = make_edit(&server.doc.to_string(), pos, del, ins);
            inserted.push_str(&e.text);
            let c = server.local_edit(&[e]).unwrap();
            broadcast(c, down);
        }
        Act::Up { who } => {
            if let Some((base, seq, edits)) = up[who].pop_front() {
                let c = server.receive(who as u32 + 1, seq, base, &edits).unwrap();
                broadcast(c, down);
            }
        }
        Act::Down { who } => {
            if let Some(msg) = down[who].pop_front() {
                let (edits, out) = clients[who].receive(&msg).unwrap();
                apply(&mut editors[who], &edits);
                if let Some(out) = out {
                    up[who].push_back((out.base_rev, out.seq, out.edits));
                }
            }
        }
        Act::Undo { who } => {
            if let Some((edits, out)) = clients[who].undo().unwrap() {
                apply(&mut editors[who], &edits);
                if let Some(out) = out {
                    up[who].push_back((out.base_rev, out.seq, out.edits));
                }
            }
        }
    };

    for a in acts {
        step(
            a,
            &mut server,
            &mut clients,
            &mut editors,
            &mut up,
            &mut down,
            &mut inserted,
        );
    }
    // Drain to quiescence.
    loop {
        let mut moved = false;
        for who in 0..CLIENTS {
            if !up[who].is_empty() {
                step(
                    &Act::Up { who },
                    &mut server,
                    &mut clients,
                    &mut editors,
                    &mut up,
                    &mut down,
                    &mut inserted,
                );
                moved = true;
            }
            if !down[who].is_empty() {
                step(
                    &Act::Down { who },
                    &mut server,
                    &mut clients,
                    &mut editors,
                    &mut up,
                    &mut down,
                    &mut inserted,
                );
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    let text = server.doc.to_string();
    for (c, e) in clients.iter().zip(&editors) {
        assert!(c.is_synced());
        assert_eq!(c.doc.to_string(), text, "replica diverged");
        assert_eq!(e, &text, "editor mirror diverged");
    }
    (text, inserted)
}

fn run_crdt(acts: &[Act]) -> (String, String) {
    let mut daemon = Replica::from_text(1, INITIAL);
    let snap = daemon.full_state();
    let mut clients: Vec<Replica> = (0..CLIENTS)
        .map(|i| {
            let mut r = Replica::new(i as u64 + 2);
            r.apply_remote(&snap);
            r
        })
        .collect();
    let mut editors: Vec<String> = vec![INITIAL.to_string(); CLIENTS];
    let mut up: Vec<VecDeque<Vec<u8>>> = vec![VecDeque::new(); CLIENTS];
    let mut down: Vec<VecDeque<Vec<u8>>> = vec![VecDeque::new(); CLIENTS];
    let mut inserted = String::new();

    let step = |a: &Act,
                daemon: &mut Replica,
                clients: &mut Vec<Replica>,
                editors: &mut Vec<String>,
                up: &mut Vec<VecDeque<Vec<u8>>>,
                down: &mut Vec<VecDeque<Vec<u8>>>,
                inserted: &mut String| match *a {
        Act::Edit { who, pos, del, ins } => {
            let e = make_edit(&editors[who], pos, del, ins);
            inserted.push_str(&e.text);
            apply(&mut editors[who], std::slice::from_ref(&e));
            let u = clients[who].local_edit(&[e]);
            up[who].push_back(u);
        }
        Act::Agent { pos, del, ins } => {
            let e = make_edit(&daemon.string(), pos, del, ins);
            inserted.push_str(&e.text);
            let u = daemon.agent_edit("agent", &[e]);
            for q in down.iter_mut() {
                q.push_back(u.clone());
            }
        }
        Act::Up { who } => {
            if let Some(u) = up[who].pop_front() {
                daemon.apply_remote(&u);
                for (i, q) in down.iter_mut().enumerate() {
                    if i != who {
                        q.push_back(u.clone());
                    }
                }
            }
        }
        Act::Down { who } => {
            if let Some(u) = down[who].pop_front() {
                let edits = clients[who].apply_remote(&u);
                apply(&mut editors[who], &edits);
            }
        }
        Act::Undo { who } => {
            if let Some((edits, u)) = clients[who].undo() {
                apply(&mut editors[who], &edits);
                up[who].push_back(u);
            }
        }
    };

    for a in acts {
        step(
            a,
            &mut daemon,
            &mut clients,
            &mut editors,
            &mut up,
            &mut down,
            &mut inserted,
        );
    }
    loop {
        let mut moved = false;
        for who in 0..CLIENTS {
            if !up[who].is_empty() {
                step(
                    &Act::Up { who },
                    &mut daemon,
                    &mut clients,
                    &mut editors,
                    &mut up,
                    &mut down,
                    &mut inserted,
                );
                moved = true;
            }
            if !down[who].is_empty() {
                step(
                    &Act::Down { who },
                    &mut daemon,
                    &mut clients,
                    &mut editors,
                    &mut up,
                    &mut down,
                    &mut inserted,
                );
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    let text = daemon.string();
    for (c, e) in clients.iter().zip(&editors) {
        assert_eq!(c.string(), text, "replica diverged");
        assert_eq!(e, &text, "editor mirror diverged");
    }
    (text, inserted)
}

fn assert_no_lost_chars(text: &str, inserted: &str) {
    let mut expected = char_counts(INITIAL);
    for (c, n) in char_counts(inserted) {
        *expected.entry(c).or_insert(0) += n;
    }
    assert_eq!(char_counts(text), expected, "characters lost or duplicated");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn oplog_converges(acts in prop::collection::vec(act(true), 1..120)) {
        run_oplog(&acts);
    }

    #[test]
    fn oplog_never_loses_inserts(acts in prop::collection::vec(act(false), 1..120)) {
        let (text, inserted) = run_oplog(&acts);
        assert_no_lost_chars(&text, &inserted);
    }

    #[test]
    fn crdt_converges(acts in prop::collection::vec(act(true), 1..120)) {
        run_crdt(&acts);
    }

    #[test]
    fn crdt_never_loses_inserts(acts in prop::collection::vec(act(false), 1..120)) {
        let (text, inserted) = run_crdt(&acts);
        assert_no_lost_chars(&text, &inserted);
    }
}

/// Reconnect with unsaved, unacknowledged edits: the client keeps typing offline while an
/// agent edits; on reconnect the daemon replays what the client missed and the client
/// resends its pending change (op-log), or both sides exchange state vectors (CRDT).
#[test]
fn reconnect_with_unsaved_edits_oplog() {
    let mut server = Server::new(INITIAL);
    let mut c = Client::new(1, INITIAL, 0);
    let mut editor = INITIAL.to_string();
    // First edit goes out but the connection drops before the ack.
    let e = make_edit(&editor, 3, 0, 1);
    apply(&mut editor, std::slice::from_ref(&e));
    let lost = c.local_edit(&[e]).unwrap().unwrap();
    let _ = lost; // never delivered
    for i in 0..200u32 {
        let e = make_edit(&editor, i * 7, (i % 3) as u8, (i % 5) as u8);
        apply(&mut editor, std::slice::from_ref(&e));
        assert!(c.local_edit(&[e]).unwrap().is_none());
    }
    let mut missed = Vec::new();
    for i in 0..50u32 {
        let e = make_edit(&server.doc.to_string(), i * 13, 1, 2);
        missed.push(server.local_edit(&[e]).unwrap());
    }
    // Reconnect: subscribe from c.rev; daemon replays the missed changes.
    for m in &missed {
        let (edits, _) = c.receive(m).unwrap();
        apply(&mut editor, &edits);
    }
    let out = c.resend().unwrap();
    let ack = server
        .receive(1, out.seq, out.base_rev, &out.edits)
        .unwrap();
    c.receive(&ack).unwrap();
    assert!(c.is_synced());
    assert_eq!(c.doc.to_string(), server.doc.to_string());
    assert_eq!(editor, server.doc.to_string());
}

#[test]
fn reconnect_with_unsaved_edits_crdt() {
    let mut daemon = Replica::from_text(1, INITIAL);
    let mut c = Replica::new(2);
    c.apply_remote(&daemon.full_state());
    let mut editor = INITIAL.to_string();
    for i in 0..200u32 {
        let e = make_edit(&editor, i * 7, (i % 3) as u8, (i % 5) as u8);
        apply(&mut editor, std::slice::from_ref(&e));
        c.local_edit(&[e]); // never delivered
    }
    for i in 0..50u32 {
        let e = make_edit(&daemon.string(), i * 13, 1, 2);
        daemon.agent_edit("agent", &[e]);
    }
    let to_client = daemon.diff_since(&c.state_vector());
    let to_daemon = c.diff_since(&daemon.state_vector());
    daemon.apply_remote(&to_daemon);
    let edits = c.apply_remote(&to_client);
    apply(&mut editor, &edits);
    assert_eq!(c.string(), daemon.string());
    assert_eq!(editor, daemon.string());
}

/// Selective revert of one agent change after the user kept editing around it.
#[test]
fn revert_agent_edit_keeps_user_edits() {
    let mut server = Server::new("line one\nline two\nline three\n");
    let agent = server
        .local_edit(&[Edit::new(9..17, "LINE TWO (agent)")])
        .unwrap();
    // User edits before and after the agent's change.
    let len = server.doc.len();
    server
        .local_edit(&[Edit::new(0..0, "// top\n"), Edit::new(len..len, "tail\n")])
        .unwrap();
    server.revert(agent.rev).unwrap();
    assert_eq!(
        server.doc.to_string(),
        "// top\nline one\nline two\nline three\ntail\n"
    );
}
