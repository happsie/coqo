//! Coqo patch (see `PATCHES.md` at the crate root): apply edits that someone
//! else made, such as an agent or another window sharing the buffer.
//!
//! A local edit moves the cursors to the edit, records undo history and emits
//! an [`InputEditsEvent`](super::InputEditsEvent). A remote edit must do none of
//! that: every cursor and selection stays on the text it was on, nothing enters
//! this editor's undo history, and no `InputEditsEvent` echoes it back to the
//! replication layer that sent it. Folds, decorations, diagnostics and
//! highlighting follow the edit exactly as they do for a local one, because the
//! text change itself goes through the same `replace_text_in_ranges` path.

use std::ops::Range;

use gpui::{Context, EntityInputHandler as _, Window};
use sum_tree::Bias;

use super::{RopeExt as _, cursor::CursorSelection, kind::InputModeKind, state::InputBaseState};

impl<M: InputModeKind> InputBaseState<M> {
    /// Every selection as a UTF-8 byte range, the active one first.
    ///
    /// A collapsed selection (`start == end`) is a cursor.
    pub fn selected_ranges(&self) -> Vec<Range<usize>> {
        self.selections.iter().map(|s| s.start..s.end).collect()
    }

    /// Whether an IME composition is in progress, so text is marked or has
    /// been composed but not reported in an `InputEditsEvent` yet.
    pub fn is_composing(&self) -> bool {
        self.ime_marked_range.is_some()
    }

    /// Apply edits made by someone else without disturbing the user.
    ///
    /// `edits` are `(UTF-8 byte range, replacement)` pairs in the coordinates of
    /// the current [`text`](Self::text), sorted or not, and disjoint, applied
    /// together like an LSP `TextEdit[]`. Ranges are clamped to the text and to
    /// character boundaries.
    ///
    /// - Every selection and cursor is mapped through the edits. A cursor, and
    ///   the start of a selection, at an insert point stays left of the inserted
    ///   text; the end of a selection sticks right. A position inside replaced
    ///   text collapses to the start (or, for a selection end, the end) of the
    ///   replacement.
    /// - Nothing is recorded in the undo history. The history holds absolute
    ///   offsets that cannot be rebased through someone else's edit, so it is
    ///   cleared instead: the built-in undo can never revert a remote edit or
    ///   replay a local one at the wrong place. Hosts that keep their own
    ///   per-side undo intercept `Undo` and `Redo` before they reach the input.
    /// - No `InputEditsEvent` or `InputEvent::Change` is emitted.
    /// - It applies to read-only and disabled inputs too, like other
    ///   programmatic edits.
    /// - The view does not scroll to follow the mapped cursor.
    ///
    /// A composition in progress is committed first, as if the IME had
    /// unmarked its text, which reports it as a local edit. Callers that track
    /// the text through `InputEditsEvent` should check
    /// [`is_composing`](Self::is_composing) and wait for the composition to end,
    /// because their coordinates do not include the composed text yet.
    pub fn apply_remote_edits(
        &mut self,
        edits: &[(Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if edits.is_empty() {
            return;
        }
        if self.is_composing() {
            self.unmark_text(window, cx);
        }

        let len = self.text.len();
        let mut sorted: Vec<(Range<usize>, String)> = edits
            .iter()
            .map(|(range, text)| {
                let start = self.text.clip_offset(range.start.min(len), Bias::Left);
                let end = self.text.clip_offset(range.end.min(len), Bias::Right);
                (start..end.max(start), text.clone())
            })
            .collect();
        sorted.sort_by_key(|(range, _)| (range.start, range.end));

        let selections: Vec<CursorSelection> = self.selections.iter().copied().collect();
        let painted_active = self.last_selected_range == Some(selections[0]);

        let (emit_events, ignoring) = (self.emit_events, self.undo_manager.is_ignoring());
        self.emit_events = false;
        self.undo_manager.set_ignoring(true);
        self.with_edits_allowed(|this| this.replace_text_in_ranges(&sorted, window, cx));
        self.undo_manager.set_ignoring(ignoring);
        self.emit_events = emit_events;
        self.last_input_edits.clear();
        self.undo_manager.clear();

        let text_len = self.text.len();
        let mapped: Vec<CursorSelection> = selections
            .into_iter()
            .map(|mut selection| {
                if selection.is_empty() {
                    let offset = map_offset(selection.start, &sorted, Bias::Left);
                    selection.start = offset.min(text_len);
                    selection.end = selection.start;
                } else {
                    selection.start =
                        map_offset(selection.start, &sorted, Bias::Left).min(text_len);
                    selection.end = map_offset(selection.end, &sorted, Bias::Right).min(text_len);
                }
                selection
            })
            .collect();
        self.selections.replace_all(mapped);
        self.selections.merge_overlapping();

        // The view follows the caret on the frame the selection changes. The
        // user did not move it, so the frame must not see a change.
        if painted_active {
            self.last_selected_range = Some(*self.selections.active());
        }
        cx.notify();
    }
}

/// Where `offset` ends up after `edits` (pre-edit coordinates, sorted by start,
/// disjoint). `Bias::Left` keeps an offset before text inserted exactly at it,
/// `Bias::Right` moves it past.
fn map_offset(offset: usize, edits: &[(Range<usize>, String)], bias: Bias) -> usize {
    let mut delta = 0isize;
    for (range, text) in edits {
        let new_end = (range.start as isize + delta) as usize + text.len();
        if range.end < offset || (range.end == offset && !range.is_empty()) {
            // Entirely before the offset.
        } else if range.start > offset || (range.start == offset && !range.is_empty()) {
            // Entirely after it: later edits are too.
            break;
        } else if range.is_empty() {
            // An insert exactly at the offset.
            if bias == Bias::Left {
                break;
            }
        } else {
            // The offset is inside replaced text.
            return match bias {
                Bias::Left => (range.start as isize + delta) as usize,
                Bias::Right => new_end,
            };
        }
        delta += text.len() as isize - range.len() as isize;
    }
    (offset as isize + delta) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(list: &[(Range<usize>, &str)]) -> Vec<(Range<usize>, String)> {
        list.iter()
            .map(|(r, t)| (r.clone(), t.to_string()))
            .collect()
    }

    #[test]
    fn map_offset_follows_inserts_and_deletes() {
        let e = edits(&[(0..0, "ab"), (5..8, "x"), (10..10, "yy")]);
        assert_eq!(map_offset(0, &e, Bias::Left), 0);
        assert_eq!(map_offset(0, &e, Bias::Right), 2);
        assert_eq!(map_offset(3, &e, Bias::Left), 5);
        assert_eq!(map_offset(5, &e, Bias::Right), 7);
        assert_eq!(map_offset(6, &e, Bias::Left), 7);
        assert_eq!(map_offset(6, &e, Bias::Right), 8);
        assert_eq!(map_offset(8, &e, Bias::Left), 8);
        assert_eq!(map_offset(10, &e, Bias::Left), 10);
        assert_eq!(map_offset(10, &e, Bias::Right), 12);
        assert_eq!(map_offset(20, &e, Bias::Left), 22);
    }
}
