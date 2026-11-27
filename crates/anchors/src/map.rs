// SPDX-License-Identifier: Apache-2.0

//! Exact re-anchoring: a byte range mapped through a buffer's changes.
//!
//! Each change is a list of edits in the coordinates of the text before it
//! (one `buffer/changes` entry, one [`Document::edit`] call), sorted and not
//! overlapping. Text inserted exactly at either edge of the range stays
//! **outside** it: a line typed above an anchored line does not join the
//! anchor. Text that replaces bytes of the range stays inside it.
//!
//! [`Document::edit`]: coqo_syntax::Document::edit

use std::ops::Range;

use coqo_syntax::TextEdit;

/// A range after [`map_range`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapped {
    /// Where the range is now.
    pub range: Range<usize>,
    /// Some change deleted bytes of the range or inserted text strictly
    /// inside it, so its text may differ.
    pub touched: bool,
    /// Every byte of a non-empty range was deleted (it is now empty).
    pub collapsed: bool,
    /// One edit replaced bytes on both sides of an edge of the range with
    /// text at least as long as the range (what a whole-file rewrite diffed
    /// by a prefix/suffix scan looks like): the anchored text may be
    /// anywhere in it, so the changes say nothing about where it went.
    pub swallowed: bool,
}

impl Mapped {
    /// Whether the range's text is exactly what it was.
    pub fn is_intact(&self) -> bool {
        !self.touched
    }
}

/// Map `range` through `changes`, oldest first.
pub fn map_range<C: AsRef<[TextEdit]>>(range: Range<usize>, changes: &[C]) -> Mapped {
    let mut out = Mapped {
        range,
        touched: false,
        collapsed: false,
        swallowed: false,
    };
    for change in changes {
        map_change(&mut out, change.as_ref());
    }
    out
}

/// Map through one change.
fn map_change(m: &mut Mapped, edits: &[TextEdit]) {
    let Range { start: a, end: b } = m.range;
    let empty = a == b;
    let mut start = None;
    let mut end = None;
    let mut delta: isize = 0;
    let mut deleted = 0;
    for e in edits {
        let (s, t, len) = (e.range.start, e.range.end, e.text.len());
        // Start: edits before it, or ending at it (an insert at the start
        // stays outside), move it; one that straddles it puts it after the
        // replacement.
        if start.is_none() {
            if t <= a {
                // moves it below
            } else if s < a {
                start = Some(shift(s, delta) + len);
            } else {
                start = Some(shift(a, delta));
            }
        }
        // End: edits before it, or replacing text that ends at it, move it;
        // an insert at the end stays outside; one that straddles it puts
        // it before the replacement.
        if end.is_none() {
            if t < b || (t == b && s < t) {
                // moves it below
            } else if s < b && b < t {
                end = Some(shift(s, delta));
            } else {
                end = Some(shift(b, delta));
            }
        }
        // Damage to the range itself.
        let overlap = t.min(b).saturating_sub(s.max(a));
        deleted += overlap;
        if overlap > 0 || (a < s && s < b) || (empty && s < a && a < t) {
            m.touched = true;
        }
        let straddles = (s < a && a < t) || (s < b && b < t);
        if straddles && overlap > 0 && len >= b - a {
            m.swallowed = true;
        }
        delta += len as isize - (t - s) as isize;
    }
    let start = start.unwrap_or_else(|| shift(a, delta));
    let end = if empty {
        start
    } else {
        end.unwrap_or_else(|| shift(b, delta)).max(start)
    };
    if !empty && deleted >= b - a {
        m.collapsed = true;
    }
    m.range = start..end;
}

fn shift(pos: usize, delta: isize) -> usize {
    pos.saturating_add_signed(delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(at: usize, text: &str) -> TextEdit {
        TextEdit::new(at..at, text)
    }

    fn del(range: Range<usize>) -> TextEdit {
        TextEdit::new(range, "")
    }

    fn map(range: Range<usize>, edits: Vec<TextEdit>) -> Mapped {
        map_range(range, &[edits])
    }

    #[test]
    fn edits_elsewhere_shift_the_range() {
        let m = map(10..20, vec![ins(0, "abc"), del(25..30)]);
        assert_eq!(m.range, 13..23);
        assert!(m.is_intact() && !m.collapsed);
    }

    #[test]
    fn inserts_at_the_edges_stay_outside() {
        let m = map(10..20, vec![ins(10, "above\n"), ins(20, " after")]);
        assert_eq!(m.range, 16..26);
        assert!(m.is_intact());
    }

    #[test]
    fn inserts_inside_touch() {
        let m = map(10..20, vec![ins(15, "xy")]);
        assert_eq!(m.range, 10..22);
        assert!(m.touched && !m.collapsed);
    }

    #[test]
    fn replacing_inside_keeps_the_replacement() {
        let m = map(10..20, vec![TextEdit::new(10..12, "abcd")]);
        assert_eq!(m.range, 10..22);
        let m = map(10..20, vec![TextEdit::new(18..20, "abcd")]);
        assert_eq!(m.range, 10..22);
        assert!(m.touched);
    }

    #[test]
    fn straddling_edits_cut_the_range() {
        // Deletes 5..12: the start lands after the replacement.
        let m = map(10..20, vec![TextEdit::new(5..12, "Z")]);
        assert_eq!(m.range, 6..14);
        // Deletes 18..25: the end lands before the replacement.
        let m = map(10..20, vec![TextEdit::new(18..25, "Z")]);
        assert_eq!(m.range, 10..18);
        assert!(m.touched && !m.collapsed && !m.swallowed);
    }

    #[test]
    fn deleting_everything_collapses() {
        let m = map(10..20, vec![del(10..20)]);
        assert_eq!(m.range, 10..10);
        assert!(m.collapsed && !m.swallowed);
        let m = map(10..20, vec![del(10..15), del(15..20)]);
        assert!(m.collapsed);
        let m = map(10..20, vec![TextEdit::new(0..30, "a whole new file")]);
        assert!(m.collapsed && m.swallowed);
        // Too short to hold the anchored text: just deleted.
        let m = map(10..20, vec![TextEdit::new(0..30, "short")]);
        assert!(m.collapsed && !m.swallowed);
        // Replaced exactly: rewritten in place, not swallowed.
        let m = map(10..20, vec![TextEdit::new(10..20, "a whole new line")]);
        assert!(m.collapsed && !m.swallowed);
        // Cut across its end by a long replacement: swallowed.
        let m = map(
            10..20,
            vec![TextEdit::new(15..40, "a long replacement text")],
        );
        assert!(m.touched && !m.collapsed && m.swallowed);
    }

    #[test]
    fn empty_ranges_follow_their_spot() {
        let m = map(10..10, vec![ins(10, "abc")]);
        assert_eq!(m.range, 13..13);
        assert!(!m.touched);
        let m = map(10..10, vec![del(5..15)]);
        assert_eq!(m.range, 5..5);
        assert!(m.touched && !m.collapsed);
    }

    #[test]
    fn several_changes_compose() {
        let changes = vec![vec![ins(0, "ab")], vec![del(0..1)], vec![ins(100, "x")]];
        let m = map_range(10..20, &changes);
        assert_eq!(m.range, 11..21);
        assert!(m.is_intact());
    }
}
