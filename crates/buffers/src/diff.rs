// SPDX-License-Identifier: Apache-2.0

//! Turning a whole new text into an operation.
//!
//! Agents and other programs deliver whole files (Claude Code writes to disk
//! itself, ACP `fs/write_text_file` sends the entire content), and replacing
//! a buffer wholesale would wipe concurrent edits, cursors and anchors. So
//! the new text is diffed against the old one first.
//!
//! [`diff`] is a common prefix / suffix scan (ADR 0006: 0.8 ms on 3 MB). It
//! yields at most one replacement, which is exact when one region changed,
//! and is always a correct operation from `old` to `new`.

use ropey::Rope;

use crate::op::TextOp;

/// Length of the common prefix of two chunk streams, in bytes.
fn common_prefix<'a>(
    mut a: impl Iterator<Item = &'a str>,
    mut b: impl Iterator<Item = &'a str>,
) -> usize {
    let (mut ca, mut cb): (&[u8], &[u8]) = (&[], &[]);
    let mut n = 0;
    loop {
        if ca.is_empty() {
            match a.next() {
                Some(c) => ca = c.as_bytes(),
                None => return n,
            }
            continue;
        }
        if cb.is_empty() {
            match b.next() {
                Some(c) => cb = c.as_bytes(),
                None => return n,
            }
            continue;
        }
        let m = ca.len().min(cb.len());
        if ca[..m] == cb[..m] {
            n += m;
            ca = &ca[m..];
            cb = &cb[m..];
        } else {
            return n + ca.iter().zip(cb).take_while(|(x, y)| x == y).count();
        }
    }
}

/// Length of the common suffix of two reversed chunk streams, in bytes.
fn common_suffix<'a>(
    mut a: impl Iterator<Item = &'a str>,
    mut b: impl Iterator<Item = &'a str>,
) -> usize {
    let (mut ca, mut cb): (&[u8], &[u8]) = (&[], &[]);
    let mut n = 0;
    loop {
        if ca.is_empty() {
            match a.next() {
                Some(c) => ca = c.as_bytes(),
                None => return n,
            }
            continue;
        }
        if cb.is_empty() {
            match b.next() {
                Some(c) => cb = c.as_bytes(),
                None => return n,
            }
            continue;
        }
        let m = ca.len().min(cb.len());
        let (ta, tb) = (&ca[ca.len() - m..], &cb[cb.len() - m..]);
        if ta == tb {
            n += m;
            ca = &ca[..ca.len() - m];
            cb = &cb[..cb.len() - m];
        } else {
            return n + ta
                .iter()
                .rev()
                .zip(tb.iter().rev())
                .take_while(|(x, y)| x == y)
                .count();
        }
    }
}

/// An operation turning `old` into `new`: keep the common prefix and
/// suffix, replace the middle. Offsets are snapped to character
/// boundaries.
pub fn diff(old: &Rope, new: &Rope) -> TextOp {
    let (lo, ln) = (old.len(), new.len());
    let mut prefix = common_prefix(old.chunks(), new.chunks()).min(lo.min(ln));
    while !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let max_suffix = lo.min(ln) - prefix;
    let mut suffix = common_suffix(
        old.chunks_at(lo).0.reversed(),
        new.chunks_at(ln).0.reversed(),
    )
    .min(max_suffix);
    while !(old.is_char_boundary(lo - suffix) && new.is_char_boundary(ln - suffix)) {
        suffix -= 1;
    }
    let edit = coqo_protocol::methods::buffer::TextEdit::new(
        prefix..lo - suffix,
        new.slice(prefix..ln - suffix).to_string(),
    );
    if edit.start == edit.end && edit.text.is_empty() {
        return TextOp::identity(lo);
    }
    TextOp::from_edits(lo, std::slice::from_ref(&edit)).expect("the range is within `old`")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn check(old: &str, new: &str) -> TextOp {
        let op = diff(&Rope::from_str(old), &Rope::from_str(new));
        assert_eq!(op.apply_str(old).unwrap(), new, "{old:?} -> {new:?}");
        op
    }

    #[test]
    fn one_region() {
        let op = check("fn main() {}\n", "fn main() { run(); }\n");
        assert_eq!(op.to_edits().len(), 1);
        assert_eq!(op.to_edits()[0].text, " run(); ");
        assert!(check("same", "same").is_noop());
        check("", "new");
        check("old", "");
        check("aaaa", "aa");
        check("abcabc", "abc");
    }

    #[test]
    fn snaps_to_character_boundaries() {
        // é = C3 A9, è = C3 A8: they share a first byte.
        let op = check("café", "cafè");
        assert_eq!(op.to_edits()[0].start, 3);
        // ü = C3 BC, ö = C3 B6, and the same last byte trick: ä = C3 A4, ¤ = C2 A4.
        check("xä", "x¤");
        check("🙂🙂", "🙃🙂");
    }

    #[test]
    fn large_text_spanning_many_chunks() {
        let old: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        let mut new = old.clone();
        new.insert_str(old.len() / 2, "inserted\n");
        let op = check(&old, &new);
        assert_eq!(op.to_edits().len(), 1);
    }

    proptest! {
        #[test]
        fn diff_always_maps_old_to_new(
            old in "[aé🙂\n]{0,40}",
            new in "[aé🙂\n]{0,40}",
        ) {
            check(&old, &new);
        }
    }
}
