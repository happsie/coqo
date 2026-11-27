// SPDX-License-Identifier: Apache-2.0

//! Unit tests of [`ServerBuffer`](super::ServerBuffer).

use super::*;
use crate::text::decode;

fn buffer(text: &str) -> ServerBuffer {
    ServerBuffer::new(decode(text.as_bytes().to_vec()).unwrap())
}

fn r(id: &str) -> ReplicaId {
    ReplicaId(id.to_owned())
}

fn client(b: &mut ServerBuffer, who: &str, seq: u64, base: u64, e: TextEdit) -> BufferChange {
    b.apply_client(&r(who), seq, base, &[e]).unwrap().unwrap()
}

#[test]
fn dirty_tracks_the_difference_to_disk() {
    let mut b = buffer("abc");
    assert!(!b.is_dirty());
    assert!(client(&mut b, "a", 1, 0, TextEdit::insert(1, "x")).dirty);
    let c = client(&mut b, "a", 2, 1, TextEdit::delete(1..2));
    assert!(!c.dirty, "typed and deleted: clean again");
    // A replacement and its undo.
    let format = Origin::of(OriginKind::Format);
    b.apply_head(format.clone(), &[TextEdit::new(0..2, "AB")])
        .unwrap();
    assert!(b.is_dirty());
    let c = b.apply_head(format, &[TextEdit::new(0..2, "ab")]).unwrap();
    assert!(!c.dirty);
}

#[test]
fn client_edits_are_transformed_and_deduplicated() {
    let mut b = buffer("hello");
    b.join(&r("a"));
    b.join(&r("b"));
    client(&mut b, "a", 1, 0, TextEdit::insert(0, ">"));
    // b edited against rev 0, not knowing about a's change.
    let c = client(&mut b, "b", 1, 0, TextEdit::insert(5, "!"));
    assert_eq!(c.edits, vec![TextEdit::insert(6, "!")]);
    assert_eq!(b.text().to_string(), ">hello!");
    // A resend of a committed seq is dropped.
    let again = b.apply_client(&r("b"), 1, 1, &[TextEdit::insert(0, "?")]);
    assert_eq!(again.unwrap(), None);
    assert_eq!(b.last_seq(&r("b")), 1);
    assert_eq!(
        b.apply_client(&r("b"), 0, 2, &[]),
        Err(BufferError::ZeroSeq)
    );
    assert!(matches!(
        b.apply_client(&r("b"), 2, 9, &[]),
        Err(BufferError::FutureRevision { rev: 9, head: 2 })
    ));
    // Edits that split "é" or run past the end.
    let mut b = buffer("é");
    assert!(matches!(
        b.apply_client(&r("a"), 1, 0, &[TextEdit::insert(1, "x")]),
        Err(BufferError::InvalidEdit(OpError::NotCharBoundary(1)))
    ));
    assert!(matches!(
        b.apply_client(&r("a"), 1, 0, &[TextEdit::insert(3, "x")]),
        Err(BufferError::InvalidEdit(OpError::BadEdits { .. }))
    ));
    assert_eq!(b.rev(), 0);
}

#[test]
fn log_is_trimmed_to_what_replicas_need() {
    let mut b = buffer("");
    b.set_limits(LogLimits { keep: 2, max: 5 });
    b.join(&r("slow"));
    let agent = |b: &mut ServerBuffer, t: &str| {
        b.apply_head(Origin::agent("s"), &[TextEdit::insert(0, t)])
            .unwrap()
    };
    for i in 0..4 {
        agent(&mut b, &i.to_string());
    }
    assert_eq!(b.oldest_rev(), 0, "kept for the slow replica");
    client(&mut b, "slow", 1, 3, TextEdit::insert(0, "x"));
    assert_eq!(b.oldest_rev(), 3);
    b.leave(&r("slow"));
    assert_eq!(b.oldest_rev(), 3, "but always the last `keep`");
    for _ in 0..10 {
        agent(&mut b, "y");
    }
    assert_eq!(b.oldest_rev(), b.rev() - 2);
    b.join(&r("late"));
    for _ in 0..10 {
        agent(&mut b, "z");
    }
    assert_eq!(b.oldest_rev(), b.rev() - 5, "capped at `max`");
    assert!(matches!(
        b.apply_client(&r("late"), 1, 15, &[]),
        Err(BufferError::RevisionGone { rev: 15, .. })
    ));
    assert!(matches!(b.revert(1), Err(BufferError::RevisionGone { .. })));
}

#[test]
fn disk_changes_merge_around_unsaved_edits() {
    let mut b = buffer("one\ntwo\nthree\n");
    // An unsaved user edit on line 1.
    client(&mut b, "a", 1, 0, TextEdit::insert(3, "!"));
    // An agent rewrites line 3 on disk; the file never had the "!".
    let c = b
        .apply_disk(Rope::from_str("one\ntwo\nTHREE\n"), Origin::agent("s1"))
        .unwrap();
    assert_eq!(c.edits, vec![TextEdit::new(9..14, "THREE")]);
    assert!(c.dirty);
    assert!(!c.conflict);
    assert_eq!(b.text().to_string(), "one!\ntwo\nTHREE\n");
    assert_eq!(b.disk().to_string(), "one\ntwo\nTHREE\n");
    // The same content again is no change (our own save, a touch).
    let same = Rope::from_str("one\ntwo\nTHREE\n");
    assert_eq!(b.apply_disk(same, Origin::of(OriginKind::Disk)), None);
    // A change touching the unsaved edit is flagged.
    let c = b
        .apply_disk(
            Rope::from_str("ONE\ntwo\nTHREE\n"),
            Origin::of(OriginKind::Disk),
        )
        .unwrap();
    assert!(c.conflict);
    assert!(
        b.text().to_string().contains('!'),
        "the user's edit survives"
    );
}

#[test]
fn clean_buffers_reload_from_disk() {
    let mut b = buffer("a\n");
    let c = b
        .apply_disk(Rope::from_str("b\n"), Origin::of(OriginKind::Disk))
        .unwrap();
    assert!(!c.dirty);
    assert_eq!(b.saved_rev(), c.rev);
    assert_eq!(b.text().to_string(), "b\n");
}

#[test]
fn save_keeps_later_edits_unsaved() {
    let mut b = buffer("abc");
    let me = Origin::client(&r("a"));
    b.apply_head(me.clone(), &[TextEdit::insert(0, "1")])
        .unwrap();
    let (written, at) = (b.text().clone(), b.rev());
    // An edit lands while the file is being written.
    b.apply_head(me, &[TextEdit::insert(4, "2")]).unwrap();
    let c = b.mark_saved(written, at);
    assert_eq!(c.origin.kind, OriginKind::Save);
    assert!(c.edits.is_empty());
    assert!(c.dirty, "the second edit is not on disk");
    assert_eq!(b.disk().to_string(), "1abc");
    assert_eq!(b.saved_rev(), c.rev);
    let c = b.mark_saved(b.text().clone(), b.rev());
    assert!(!c.dirty);
}

#[test]
fn snapshot_and_replay_restore_the_buffer() {
    let mut b = buffer("hello\n");
    client(&mut b, "a", 4, 0, TextEdit::insert(5, "!"));
    let snap = b.snapshot();
    let later = vec![
        b.apply_head(Origin::agent("s"), &[TextEdit::insert(0, "> ")])
            .unwrap(),
        client(&mut b, "b", 1, 2, TextEdit::delete(0..1)),
    ];
    let mut back = ServerBuffer::restore(snap).unwrap();
    for c in &later {
        back.replay(c).unwrap();
    }
    assert_eq!(back.text().to_string(), b.text().to_string());
    assert_eq!(back.rev(), b.rev());
    assert_eq!(back.disk().to_string(), "hello\n");
    assert!(back.is_dirty());
    assert_eq!(back.last_seq(&r("a")), 4);
    assert_eq!(back.last_seq(&r("b")), 1);
    // A duplicate from before the restart is still recognised.
    assert_eq!(back.apply_client(&r("a"), 4, 3, &[]).unwrap(), None);
    // Revisions must follow on.
    assert!(back.replay(&later[0]).is_err());
}

#[test]
fn anchors_follow_edits_and_collapse() {
    let mut b = buffer("0123456789");
    let a = b.add_anchor(2..5).unwrap();
    let gone = b.add_anchor(7..8).unwrap();
    let edits = [TextEdit::insert(0, "ab"), TextEdit::insert(5, "X")];
    b.apply_head(Origin::agent("s"), &edits).unwrap();
    let moved = b.anchor(a).unwrap();
    assert_eq!((moved.start, moved.end, moved.rev), (4, 8, 1));
    b.apply_head(Origin::agent("s"), &[TextEdit::delete(8..11)])
        .unwrap();
    let g = b.anchor(gone).unwrap();
    assert!(g.collapsed);
    assert_eq!(g.start, g.end);
    assert!(!b.anchor(a).unwrap().collapsed);
    assert!(b.remove_anchor(a));
    assert!(b.anchor(a).is_none());
    assert!(b.add_anchor(3..99).is_err());
}
