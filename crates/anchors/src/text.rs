// SPDX-License-Identifier: Apache-2.0

//! Text helpers: hashing, whitespace normalisation and similarity.

use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp};

/// The stable hash of an anchored text, as stored in
/// [`Anchor::text_hash`](crate::Anchor::text_hash): 64-bit FNV-1a in hex.
///
/// It never changes between versions or platforms (unlike `std`'s hasher),
/// so stored anchors stay comparable across restarts.
pub fn text_hash(text: &str) -> String {
    format!("fnv1a64:{:016x}", fnv1a64(text.as_bytes()))
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `s` with leading and trailing whitespace removed and every inner run of
/// whitespace collapsed to one space: reindenting or realigning code leaves
/// it unchanged.
pub(crate) fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// How many non-whitespace characters `s` has: how distinctive it is.
pub(crate) fn non_ws(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

/// How long one similarity diff may take before it settles for an
/// approximate answer.
const DIFF_DEADLINE: Duration = Duration::from_millis(20);

/// A byte diff of `old` → `new`.
pub(crate) fn diff(old: &[u8], new: &[u8]) -> Vec<DiffOp> {
    similar::capture_diff_slices_deadline(
        Algorithm::Myers,
        old,
        new,
        Some(Instant::now() + DIFF_DEADLINE),
    )
}

/// Similarity of two texts in `[0, 1]`: twice the matched bytes over the
/// total length (1 for equal texts, 0 when nothing matches).
pub(crate) fn ratio(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    similar::get_diff_ratio(&diff(a.as_bytes(), b.as_bytes()), a.len(), b.len())
}

/// The best [`ratio`] two texts of these lengths can have.
pub(crate) fn ratio_bound(a: usize, b: usize) -> f32 {
    if a + b == 0 {
        1.0
    } else {
        2.0 * a.min(b) as f32 / (a + b) as f32
    }
}

/// Where byte `pos` of the old text of `ops` lands in the new text.
/// `stick_right`: a position at the edge of an insertion moves after it.
pub(crate) fn map_through_diff(ops: &[DiffOp], pos: usize, stick_right: bool) -> usize {
    for op in ops {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                if pos < old_index + len || (pos == old_index + len && !stick_right) {
                    return new_index + pos.saturating_sub(old_index);
                }
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                if pos < old_index + old_len {
                    return new_index;
                }
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                if pos == old_index && !stick_right {
                    return new_index;
                }
                if pos == old_index {
                    return new_index + new_len;
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                if pos < old_index + old_len {
                    // Inside replaced text: the matching edge of the new text.
                    return if pos == old_index && !stick_right {
                        new_index
                    } else {
                        new_index + new_len
                    };
                }
            }
        }
    }
    // At or past the end of the old text.
    ops.last().map_or(pos, |op| {
        op.new_range().end + pos.saturating_sub(op.old_range().end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        // Changing this value breaks every stored anchor.
        assert_eq!(text_hash(""), "fnv1a64:cbf29ce484222325");
        assert_eq!(text_hash("fn main() {}"), text_hash("fn main() {}"));
        assert_ne!(text_hash("a"), text_hash("b"));
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize("  let  x =\t1;\n"), "let x = 1;");
        assert_eq!(normalize("   "), "");
    }

    #[test]
    fn ratio_bounds() {
        assert_eq!(ratio("abc", "abc"), 1.0);
        assert_eq!(ratio("abc", ""), 0.0);
        assert!(ratio("let x = 1;", "let x = 2;") > 0.8);
        assert!(ratio("abc", "xyz") < 0.01);
        assert!(ratio_bound(3, 9) < 0.51);
    }

    #[test]
    fn maps_offsets_through_a_diff() {
        let ops = diff(b"fn foo(x)", b"fn bar_baz(x)");
        assert_eq!(map_through_diff(&ops, 0, false), 0);
        assert_eq!(map_through_diff(&ops, 3, false), 3);
        assert_eq!(map_through_diff(&ops, 6, true), 10);
        assert_eq!(map_through_diff(&ops, 9, true), 13);
    }
}
