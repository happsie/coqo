//! Model A building block: an ot.js-style text operation over UTF-8 **byte** offsets.
//!
//! An operation is a sequence of retain / insert / delete components that spans the whole
//! base document (`base_len` bytes) and produces `target_len` bytes. With a central server
//! (the daemon serialises every change) only TP1 is required, which this `transform`
//! satisfies; that is the well-known ot.js / Google Wave "client-server" setting.
//!
//! Applying an operation touches only the edited spots: a retain is just an offset jump,
//! so a keystroke in a 100k-line file costs `O(components * log n)` on the rope.

use std::ops::Range;

use ropey::Rope;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Comp {
    Retain(usize),
    Insert(String),
    Delete(usize),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextOp {
    pub comps: Vec<Comp>,
    pub base_len: usize,
    pub target_len: usize,
}

/// A ranged replacement in the coordinates of the document *before* the whole edit set,
/// like an LSP `TextEdit[]`: edits are sorted and non-overlapping. This is the wire form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Edit {
    pub fn new(range: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            start: range.start,
            end: range.end,
            text: text.into(),
        }
    }
}

#[derive(Debug)]
pub struct OtError(pub &'static str);

impl TextOp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_noop(&self) -> bool {
        self.comps.iter().all(|c| matches!(c, Comp::Retain(_)))
    }

    pub fn retain(&mut self, n: usize) -> &mut Self {
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

    pub fn insert(&mut self, s: &str) -> &mut Self {
        if s.is_empty() {
            return self;
        }
        self.target_len += s.len();
        // Keep the canonical order "insert before delete" so equal ops compare equal.
        let len = self.comps.len();
        match self.comps.as_mut_slice() {
            [.., Comp::Insert(prev)] => prev.push_str(s),
            [.., Comp::Insert(prev), Comp::Delete(_)] => prev.push_str(s),
            [.., Comp::Delete(_)] => {
                self.comps.insert(len - 1, Comp::Insert(s.to_string()));
            }
            _ => self.comps.push(Comp::Insert(s.to_string())),
        }
        self
    }

    pub fn delete(&mut self, n: usize) -> &mut Self {
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

    /// Build an operation from ranged edits (sorted, disjoint, pre-edit coordinates).
    pub fn from_edits(base_len: usize, edits: &[Edit]) -> Result<Self, OtError> {
        let mut op = TextOp::new();
        let mut pos = 0;
        for e in edits {
            if e.start < pos || e.end < e.start || e.end > base_len {
                return Err(OtError("edits must be sorted, disjoint and in range"));
            }
            op.retain(e.start - pos);
            op.delete(e.end - e.start);
            op.insert(&e.text);
            pos = e.end;
        }
        op.retain(base_len - pos);
        Ok(op)
    }

    /// The same change as ranged edits in pre-edit coordinates (what an editor applies).
    pub fn to_edits(&self) -> Vec<Edit> {
        let mut out: Vec<Edit> = Vec::new();
        let mut pos = 0;
        for c in &self.comps {
            match c {
                Comp::Retain(n) => pos += n,
                Comp::Delete(n) => {
                    match out.last_mut() {
                        Some(e) if e.end == pos => e.end += n,
                        _ => out.push(Edit::new(pos..pos + n, "")),
                    }
                    pos += n;
                }
                Comp::Insert(s) => match out.last_mut() {
                    Some(e) if e.end == pos => e.text.push_str(s),
                    _ => out.push(Edit::new(pos..pos, s.clone())),
                },
            }
        }
        out
    }

    pub fn apply(&self, doc: &mut Rope) -> Result<(), OtError> {
        if doc.len() != self.base_len {
            return Err(OtError("base length mismatch"));
        }
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

    /// The operation that undoes `self`, given the document `self` was applied to.
    pub fn invert(&self, before: &Rope) -> TextOp {
        let mut inv = TextOp::new();
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
    pub fn compose(a: &TextOp, b: &TextOp) -> Result<TextOp, OtError> {
        if a.target_len != b.base_len {
            return Err(OtError("compose: length mismatch"));
        }
        let mut out = TextOp::new();
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
                (None, _) | (_, None) => return Err(OtError("compose: op too short")),
                (Some(Comp::Retain(x)), Some(Comp::Retain(y))) => {
                    let m = x.min(y);
                    out.retain(m);
                    ca = if x > m {
                        Some(Comp::Retain(x - m))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Retain(y - m))
                    } else {
                        ib.next()
                    };
                }
                (Some(Comp::Insert(s)), Some(Comp::Delete(y))) => {
                    let m = s.len().min(y);
                    ca = if s.len() > m {
                        Some(Comp::Insert(s[m..].to_string()))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Delete(y - m))
                    } else {
                        ib.next()
                    };
                }
                (Some(Comp::Insert(s)), Some(Comp::Retain(y))) => {
                    let m = s.len().min(y);
                    out.insert(&s[..m]);
                    ca = if s.len() > m {
                        Some(Comp::Insert(s[m..].to_string()))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Retain(y - m))
                    } else {
                        ib.next()
                    };
                }
                (Some(Comp::Retain(x)), Some(Comp::Delete(y))) => {
                    let m = x.min(y);
                    out.delete(m);
                    ca = if x > m {
                        Some(Comp::Retain(x - m))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Delete(y - m))
                    } else {
                        ib.next()
                    };
                }
            }
        }
        Ok(out)
    }

    /// Transform concurrent `a` and `b` (same base) into `(a', b')` so that
    /// `apply(apply(d, a), b') == apply(apply(d, b), a')`. On a tie `a`'s insert goes first.
    pub fn transform(a: &TextOp, b: &TextOp) -> Result<(TextOp, TextOp), OtError> {
        if a.base_len != b.base_len {
            return Err(OtError("transform: base length mismatch"));
        }
        let (mut ap, mut bp) = (TextOp::new(), TextOp::new());
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
                (None, _) | (_, None) => return Err(OtError("transform: op too short")),
                (Some(Comp::Retain(x)), Some(Comp::Retain(y))) => {
                    let m = x.min(y);
                    ap.retain(m);
                    bp.retain(m);
                    ca = if x > m {
                        Some(Comp::Retain(x - m))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Retain(y - m))
                    } else {
                        ib.next()
                    };
                }
                (Some(Comp::Delete(x)), Some(Comp::Delete(y))) => {
                    let m = x.min(y);
                    ca = if x > m {
                        Some(Comp::Delete(x - m))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Delete(y - m))
                    } else {
                        ib.next()
                    };
                }
                (Some(Comp::Delete(x)), Some(Comp::Retain(y))) => {
                    let m = x.min(y);
                    ap.delete(m);
                    ca = if x > m {
                        Some(Comp::Delete(x - m))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Retain(y - m))
                    } else {
                        ib.next()
                    };
                }
                (Some(Comp::Retain(x)), Some(Comp::Delete(y))) => {
                    let m = x.min(y);
                    bp.delete(m);
                    ca = if x > m {
                        Some(Comp::Retain(x - m))
                    } else {
                        ia.next()
                    };
                    cb = if y > m {
                        Some(Comp::Delete(y - m))
                    } else {
                        ib.next()
                    };
                }
            }
        }
        Ok((ap, bp))
    }

    /// Where offset `pos` ends up after `self`. `stick_right`: an insert exactly at `pos`
    /// pushes it right (a cursor after typed text, the end of an anchor range); otherwise
    /// it stays left (the start of an anchor range).
    pub fn transform_offset(&self, pos: usize, stick_right: bool) -> usize {
        let mut old = 0; // position in the base document
        let mut new = 0; // position in the target document
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
                        return new; // inside deleted text: collapse to the deletion point
                    }
                    old += n;
                }
            }
        }
        new + pos.saturating_sub(old)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(base: &str, edits: &[(Range<usize>, &str)]) -> TextOp {
        let edits: Vec<Edit> = edits
            .iter()
            .map(|(r, t)| Edit::new(r.clone(), *t))
            .collect();
        TextOp::from_edits(base.len(), &edits).unwrap()
    }

    fn run(doc: &str, o: &TextOp) -> String {
        let mut r = Rope::from_str(doc);
        o.apply(&mut r).unwrap();
        r.to_string()
    }

    #[test]
    fn transform_converges_on_same_spot() {
        let d = "hello world";
        let a = op(d, &[(5..5, ",")]);
        let b = op(d, &[(5..11, "!")]);
        let (ap, bp) = TextOp::transform(&a, &b).unwrap();
        assert_eq!(run(&run(d, &a), &bp), run(&run(d, &b), &ap));
    }

    #[test]
    fn invert_and_compose() {
        let d = "abcdef";
        let a = op(d, &[(1..3, "XY"), (4..4, "Z")]);
        let after = run(d, &a);
        let inv = a.invert(&Rope::from_str(d));
        assert_eq!(run(&after, &inv), d);
        let both = TextOp::compose(&a, &inv).unwrap();
        assert_eq!(run(d, &both), d);
    }

    #[test]
    fn offsets_follow_edits() {
        let d = "0123456789";
        let a = op(d, &[(2..2, "ab"), (5..7, "")]);
        assert_eq!(a.transform_offset(1, false), 1);
        assert_eq!(a.transform_offset(2, false), 2);
        assert_eq!(a.transform_offset(2, true), 4);
        assert_eq!(a.transform_offset(6, false), 7); // inside deletion -> deletion point
        assert_eq!(a.transform_offset(9, false), 9);
    }
}
