// SPDX-License-Identifier: Apache-2.0

//! Text operations over UTF-8 **byte** offsets, ot.js-style.
//!
//! An operation is a sequence of retain / insert / delete components that
//! spans the whole base text (`base_len` bytes) and produces `target_len`
//! bytes. With a central server (the daemon serialises every change) only
//! TP1 is needed, which [`TextOp::transform`] satisfies: the ot.js / Google
//! Wave client-server setting.
//!
//! Applying an operation touches only the edited spots; a retain is just an
//! offset jump, so a keystroke in a 100k-line file costs
//! `O(components · log n)` on the rope.
//!
//! Ported from the S6 prototype (`spikes/s6-buffers/src/ot.rs`), with
//! validation so a malformed operation is an error instead of a panic.

use coqo_protocol::methods::buffer::TextEdit;
use ropey::Rope;

/// One component of a [`TextOp`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Comp {
    /// Keep this many bytes.
    Retain(usize),
    /// Insert this text.
    Insert(String),
    /// Delete this many bytes.
    Delete(usize),
}

/// A change to a whole text: retains, inserts and deletes covering every
/// byte of the base text.
///
/// Components are kept canonical (adjacent components of one kind merged,
/// an insert before a delete at the same spot), so equal changes compare
/// equal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextOp {
    comps: Vec<Comp>,
    base_len: usize,
    target_len: usize,
}

/// Why an operation could not be built, applied or combined.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    /// Edits that are unsorted, overlapping or out of range.
    #[error("edits must be sorted, non-overlapping and within the text ({len} bytes)")]
    BadEdits {
        /// Length of the text they were checked against.
        len: usize,
    },
    /// An edit boundary that splits a UTF-8 character.
    #[error("offset {0} is not on a character boundary")]
    NotCharBoundary(usize),
    /// The operation was made for a text of another length.
    #[error("operation expects a {expected}-byte text, got {actual} bytes")]
    LengthMismatch {
        /// The operation's base length.
        expected: usize,
        /// The text's length.
        actual: usize,
    },
}

impl TextOp {
    /// The identity on a text of `len` bytes.
    pub fn identity(len: usize) -> Self {
        let mut op = Self::default();
        op.retain(len);
        op
    }

    /// The components.
    pub fn comps(&self) -> &[Comp] {
        &self.comps
    }

    /// Length of the text this applies to.
    pub fn base_len(&self) -> usize {
        self.base_len
    }

    /// Length of the text this produces.
    pub fn target_len(&self) -> usize {
        self.target_len
    }

    /// Whether this changes nothing.
    pub fn is_noop(&self) -> bool {
        self.comps.iter().all(|c| matches!(c, Comp::Retain(_)))
    }

    fn retain(&mut self, n: usize) -> &mut Self {
        if n == 0 {
            return self;
        }
        self.base_len += n;
        self.target_len += n;
        if let Some(Comp::Retain(last)) = self.comps.last_mut() {
            *last += n;
        } else {
            self.comps.push(Comp::Retain(n));
        }
        self
    }

    fn insert(&mut self, s: &str) -> &mut Self {
        if s.is_empty() {
            return self;
        }
        self.target_len += s.len();
        // Keep the canonical order "insert before delete".
        let len = self.comps.len();
        match self.comps.as_mut_slice() {
            [.., Comp::Insert(prev)] | [.., Comp::Insert(prev), Comp::Delete(_)] => {
                prev.push_str(s);
            }
            [.., Comp::Delete(_)] => self.comps.insert(len - 1, Comp::Insert(s.to_owned())),
            _ => self.comps.push(Comp::Insert(s.to_owned())),
        }
        self
    }

    fn delete(&mut self, n: usize) -> &mut Self {
        if n == 0 {
            return self;
        }
        self.base_len += n;
        if let Some(Comp::Delete(last)) = self.comps.last_mut() {
            *last += n;
        } else {
            self.comps.push(Comp::Delete(n));
        }
        self
    }

    /// Build an operation from edits (sorted, non-overlapping, pre-edit
    /// coordinates) on a text of `base_len` bytes. Character boundaries are
    /// not checked here; see [`check`](Self::check).
    pub fn from_edits(base_len: usize, edits: &[TextEdit]) -> Result<Self, OpError> {
        let mut op = Self::default();
        let mut pos = 0;
        for e in edits {
            if e.start < pos || e.end < e.start || e.end > base_len {
                return Err(OpError::BadEdits { len: base_len });
            }
            op.retain(e.start - pos);
            op.delete(e.end - e.start);
            op.insert(&e.text);
            pos = e.end;
        }
        op.retain(base_len - pos);
        Ok(op)
    }

    /// The same change as edits in pre-edit coordinates (what an editor
    /// applies). Adjacent delete and insert become one replacement.
    pub fn to_edits(&self) -> Vec<TextEdit> {
        let mut out: Vec<TextEdit> = Vec::new();
        let mut pos = 0;
        for c in &self.comps {
            match c {
                Comp::Retain(n) => pos += n,
                Comp::Delete(n) => {
                    match out.last_mut() {
                        Some(e) if e.end == pos => e.end += n,
                        _ => out.push(TextEdit::delete(pos..pos + n)),
                    }
                    pos += n;
                }
                Comp::Insert(s) => match out.last_mut() {
                    Some(e) if e.end == pos => e.text.push_str(s),
                    _ => out.push(TextEdit::insert(pos, s.clone())),
                },
            }
        }
        out
    }

    /// Check that this applies to `doc`: the length matches and every
    /// boundary is on a character boundary.
    pub fn check(&self, doc: &Rope) -> Result<(), OpError> {
        if doc.len() != self.base_len {
            return Err(OpError::LengthMismatch {
                expected: self.base_len,
                actual: doc.len(),
            });
        }
        let mut pos = 0;
        for c in &self.comps {
            match c {
                Comp::Retain(n) => pos += n,
                Comp::Insert(_) => {
                    if !doc.is_char_boundary(pos) {
                        return Err(OpError::NotCharBoundary(pos));
                    }
                }
                Comp::Delete(n) => {
                    if !doc.is_char_boundary(pos) {
                        return Err(OpError::NotCharBoundary(pos));
                    }
                    pos += n;
                    if !doc.is_char_boundary(pos) {
                        return Err(OpError::NotCharBoundary(pos));
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply to `doc` after [`check`](Self::check)ing it; `doc` is unchanged
    /// on error.
    pub fn apply(&self, doc: &mut Rope) -> Result<(), OpError> {
        self.check(doc)?;
        let mut pos = 0;
        for c in &self.comps {
            match c {
                Comp::Retain(n) => pos += n,
                Comp::Insert(s) => {
                    doc.insert(pos, s);
                    pos += s.len();
                }
                Comp::Delete(n) => doc.remove(pos..pos + n),
            }
        }
        Ok(())
    }

    /// Apply to a string (tests, small texts).
    pub fn apply_str(&self, doc: &str) -> Result<String, OpError> {
        let mut rope = Rope::from_str(doc);
        self.apply(&mut rope)?;
        Ok(rope.to_string())
    }

    /// The operation that undoes `self`, given the text `self` applies to.
    pub fn invert(&self, before: &Rope) -> TextOp {
        let mut inv = TextOp::default();
        let mut pos = 0;
        for c in &self.comps {
            match c {
                Comp::Retain(n) => {
                    inv.retain(*n);
                    pos += n;
                }
                Comp::Insert(s) => {
                    inv.delete(s.len());
                }
                Comp::Delete(n) => {
                    inv.insert(&before.slice(pos..pos + n).to_string());
                    pos += n;
                }
            }
        }
        inv
    }

    /// `a` then `b` as one operation.
    pub fn compose(a: &TextOp, b: &TextOp) -> Result<TextOp, OpError> {
        if a.target_len != b.base_len {
            return Err(OpError::LengthMismatch {
                expected: b.base_len,
                actual: a.target_len,
            });
        }
        let mut out = TextOp::default();
        let mut ia = a.comps.iter().cloned();
        let mut ib = b.comps.iter().cloned();
        let (mut ca, mut cb) = (ia.next(), ib.next());
        loop {
            match (ca.take(), cb.take()) {
                (None, None) => break,
                (Some(Comp::Delete(n)), y) => {
                    out.delete(n);
                    ca = ia.next();
                    cb = y;
                }
                (x, Some(Comp::Insert(s))) => {
                    out.insert(&s);
                    ca = x;
                    cb = ib.next();
                }
                (None, _) | (_, None) => unreachable!("lengths were checked"),
                (Some(Comp::Retain(x)), Some(Comp::Retain(y))) => {
                    let m = x.min(y);
                    out.retain(m);
                    ca = rest(Comp::Retain, x, m).or_else(|| ia.next());
                    cb = rest(Comp::Retain, y, m).or_else(|| ib.next());
                }
                (Some(Comp::Insert(s)), Some(Comp::Delete(y))) => {
                    let m = s.len().min(y);
                    ca = (s.len() > m)
                        .then(|| Comp::Insert(s[m..].to_owned()))
                        .or_else(|| ia.next());
                    cb = rest(Comp::Delete, y, m).or_else(|| ib.next());
                }
                (Some(Comp::Insert(s)), Some(Comp::Retain(y))) => {
                    let m = s.len().min(y);
                    out.insert(&s[..m]);
                    ca = (s.len() > m)
                        .then(|| Comp::Insert(s[m..].to_owned()))
                        .or_else(|| ia.next());
                    cb = rest(Comp::Retain, y, m).or_else(|| ib.next());
                }
                (Some(Comp::Retain(x)), Some(Comp::Delete(y))) => {
                    let m = x.min(y);
                    out.delete(m);
                    ca = rest(Comp::Retain, x, m).or_else(|| ia.next());
                    cb = rest(Comp::Delete, y, m).or_else(|| ib.next());
                }
            }
        }
        Ok(out)
    }

    /// Transform concurrent `a` and `b` (same base) into `(a', b')` so that
    /// `b'` applied after `a` equals `a'` applied after `b`. On a tie
    /// (inserts at the same spot) `a`'s insert goes first.
    pub fn transform(a: &TextOp, b: &TextOp) -> Result<(TextOp, TextOp), OpError> {
        if a.base_len != b.base_len {
            return Err(OpError::LengthMismatch {
                expected: a.base_len,
                actual: b.base_len,
            });
        }
        let (mut ap, mut bp) = (TextOp::default(), TextOp::default());
        let mut ia = a.comps.iter().cloned();
        let mut ib = b.comps.iter().cloned();
        let (mut ca, mut cb) = (ia.next(), ib.next());
        loop {
            match (ca.take(), cb.take()) {
                (None, None) => break,
                (Some(Comp::Insert(s)), y) => {
                    ap.insert(&s);
                    bp.retain(s.len());
                    ca = ia.next();
                    cb = y;
                }
                (x, Some(Comp::Insert(s))) => {
                    ap.retain(s.len());
                    bp.insert(&s);
                    ca = x;
                    cb = ib.next();
                }
                (None, _) | (_, None) => unreachable!("lengths were checked"),
                (Some(Comp::Retain(x)), Some(Comp::Retain(y))) => {
                    let m = x.min(y);
                    ap.retain(m);
                    bp.retain(m);
                    ca = rest(Comp::Retain, x, m).or_else(|| ia.next());
                    cb = rest(Comp::Retain, y, m).or_else(|| ib.next());
                }
                (Some(Comp::Delete(x)), Some(Comp::Delete(y))) => {
                    let m = x.min(y);
                    ca = rest(Comp::Delete, x, m).or_else(|| ia.next());
                    cb = rest(Comp::Delete, y, m).or_else(|| ib.next());
                }
                (Some(Comp::Delete(x)), Some(Comp::Retain(y))) => {
                    let m = x.min(y);
                    ap.delete(m);
                    ca = rest(Comp::Delete, x, m).or_else(|| ia.next());
                    cb = rest(Comp::Retain, y, m).or_else(|| ib.next());
                }
                (Some(Comp::Retain(x)), Some(Comp::Delete(y))) => {
                    let m = x.min(y);
                    bp.delete(m);
                    ca = rest(Comp::Retain, x, m).or_else(|| ia.next());
                    cb = rest(Comp::Delete, y, m).or_else(|| ib.next());
                }
            }
        }
        Ok((ap, bp))
    }

    /// Where offset `pos` of the base text ends up after `self`.
    ///
    /// `stick_right`: an insert exactly at `pos` pushes it right (a cursor
    /// after typed text, the end of an anchor range); otherwise it stays left
    /// (the start of an anchor range). An offset inside deleted text
    /// collapses to the deletion point.
    pub fn transform_offset(&self, pos: usize, stick_right: bool) -> usize {
        let mut old = 0; // position in the base text
        let mut new = 0; // position in the target text
        for c in &self.comps {
            match c {
                Comp::Retain(n) => {
                    if pos < old + n || (pos == old + n && !stick_right) {
                        return new + (pos - old);
                    }
                    old += n;
                    new += n;
                }
                Comp::Insert(s) => {
                    if old == pos && !stick_right {
                        return new;
                    }
                    new += s.len();
                }
                Comp::Delete(n) => {
                    if pos < old + n {
                        return new;
                    }
                    old += n;
                }
            }
        }
        new + pos.saturating_sub(old)
    }

    /// The same change without replacements that put back exactly the text
    /// they remove (an insert and a delete of equal text, as left behind by
    /// typing a replacement and undoing it). `base` is the text `self`
    /// applies to.
    pub fn simplify(&self, base: &Rope) -> TextOp {
        let mut out = TextOp::default();
        let mut pos = 0;
        let mut comps = self.comps.iter().peekable();
        while let Some(c) = comps.next() {
            match c {
                Comp::Retain(n) => {
                    out.retain(*n);
                    pos += n;
                }
                Comp::Insert(s) => match comps.peek() {
                    Some(Comp::Delete(n))
                        if *n == s.len()
                            && pos + n <= base.len()
                            && base.slice(pos..pos + n) == s.as_str() =>
                    {
                        out.retain(*n);
                        pos += n;
                        comps.next();
                    }
                    _ => {
                        out.insert(s);
                    }
                },
                Comp::Delete(n) => {
                    out.delete(*n);
                    pos += n;
                }
            }
        }
        out
    }

    /// The base-text ranges this changes: each delete's range, and an empty
    /// range at each insert point.
    pub fn changed_ranges(&self) -> Vec<std::ops::Range<usize>> {
        self.to_edits()
            .into_iter()
            .map(|e| e.start..e.end)
            .collect()
    }

    /// Whether `a` and `b` (same base) change overlapping or touching parts
    /// of the text, e.g. an agent's write and unsaved user edits.
    pub fn overlaps(a: &TextOp, b: &TextOp) -> bool {
        let (ra, rb) = (a.changed_ranges(), b.changed_ranges());
        let (mut i, mut j) = (0, 0);
        while i < ra.len() && j < rb.len() {
            let (x, y) = (&ra[i], &rb[j]);
            if x.start <= y.end && y.start <= x.end {
                return true;
            }
            if x.end < y.end {
                i += 1;
            } else {
                j += 1;
            }
        }
        false
    }
}

fn rest(make: fn(usize) -> Comp, total: usize, used: usize) -> Option<Comp> {
    (total > used).then(|| make(total - used))
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;

    fn op(base: &str, edits: &[(Range<usize>, &str)]) -> TextOp {
        let edits: Vec<TextEdit> = edits
            .iter()
            .map(|(r, t)| TextEdit::new(r.clone(), *t))
            .collect();
        TextOp::from_edits(base.len(), &edits).unwrap()
    }

    #[test]
    fn transform_converges_on_same_spot() {
        let d = "hello world";
        let a = op(d, &[(5..5, ",")]);
        let b = op(d, &[(5..11, "!")]);
        let (ap, bp) = TextOp::transform(&a, &b).unwrap();
        let left = bp.apply_str(&a.apply_str(d).unwrap()).unwrap();
        let right = ap.apply_str(&b.apply_str(d).unwrap()).unwrap();
        assert_eq!(left, right);
        assert_eq!(left, "hello,!");
    }

    #[test]
    fn invert_and_compose() {
        let d = "abcdef";
        let a = op(d, &[(1..3, "XY"), (4..4, "Z")]);
        let after = a.apply_str(d).unwrap();
        let inv = a.invert(&Rope::from_str(d));
        assert_eq!(inv.apply_str(&after).unwrap(), d);
        let both = TextOp::compose(&a, &inv).unwrap();
        assert_eq!(both.apply_str(d).unwrap(), d);
        // Composition is syntactic: "bc" is deleted and inserted again.
        assert!(!both.is_noop());
        assert!(both.simplify(&Rope::from_str(d)).is_noop());
        // A real replacement stays.
        assert_eq!(a.simplify(&Rope::from_str(d)), a);
    }

    #[test]
    fn edits_round_trip() {
        let d = "0123456789";
        let a = op(d, &[(1..3, "ab"), (5..5, "x"), (7..9, "")]);
        assert_eq!(
            a.to_edits(),
            vec![
                TextEdit::new(1..3, "ab"),
                TextEdit::insert(5, "x"),
                TextEdit::delete(7..9)
            ]
        );
        assert_eq!(TextOp::from_edits(10, &a.to_edits()).unwrap(), a);
    }

    #[test]
    fn bad_edits_are_errors_not_panics() {
        assert!(TextOp::from_edits(3, &[TextEdit::insert(4, "x")]).is_err());
        assert!(
            TextOp::from_edits(5, &[TextEdit::insert(3, "x"), TextEdit::insert(1, "y")]).is_err()
        );
        let backwards = TextEdit {
            start: 3,
            end: 2,
            text: String::new(),
        };
        assert!(TextOp::from_edits(5, &[backwards]).is_err());
        // "é" is two bytes: offset 1 splits it.
        let mut doc = Rope::from_str("é!");
        let split = TextOp::from_edits(3, &[TextEdit::insert(1, "x")]).unwrap();
        assert_eq!(split.apply(&mut doc), Err(OpError::NotCharBoundary(1)));
        let del = TextOp::from_edits(3, &[TextEdit::delete(0..1)]).unwrap();
        assert_eq!(del.apply(&mut doc), Err(OpError::NotCharBoundary(1)));
        assert_eq!(doc.to_string(), "é!", "unchanged on error");
        let wrong_len = TextOp::identity(7);
        assert!(matches!(
            wrong_len.apply(&mut doc),
            Err(OpError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn offsets_follow_edits() {
        let d = "0123456789";
        let a = op(d, &[(2..2, "ab"), (5..7, "")]);
        assert_eq!(a.transform_offset(1, false), 1);
        assert_eq!(a.transform_offset(2, false), 2);
        assert_eq!(a.transform_offset(2, true), 4);
        assert_eq!(a.transform_offset(6, false), 7); // inside the deletion
        assert_eq!(a.transform_offset(9, false), 9);
    }

    #[test]
    fn overlap_detection() {
        let d = "0123456789";
        let a = op(d, &[(2..4, "x")]);
        assert!(TextOp::overlaps(&a, &op(d, &[(3..3, "y")])));
        assert!(TextOp::overlaps(&a, &op(d, &[(4..6, "")])), "touching");
        assert!(!TextOp::overlaps(&a, &op(d, &[(6..7, "z")])));
        assert!(!TextOp::overlaps(&a, &TextOp::identity(10)));
        let many = op(d, &[(0..1, "a"), (8..9, "b")]);
        assert!(!TextOp::overlaps(&a, &many));
        assert!(TextOp::overlaps(&many, &op(d, &[(9..10, "")])));
    }
}
