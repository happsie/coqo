//! Coqo patch (see `PATCHES.md` at the crate root): where lines and the gutter
//! are, for views drawn over the editor (Coqo's question markers and the
//! popover anchored to code).
//!
//! Upstream exposes the text area's bounds and the scroll offset, but not the
//! gutter's width (it depends on the digit count, folding and decorations), and
//! the y of a line takes folds into account only through the display map.

use gpui::{Pixels, px};

use super::{kind::InputModeKind, state::InputBaseState};

impl<M: InputModeKind> InputBaseState<M> {
    /// The width of the gutter in the last layout: the gutter decoration strip,
    /// the line numbers, the fold icons and the gap before the text. `None`
    /// before the first layout.
    pub fn gutter_width(&self) -> Option<Pixels> {
        self.last_layout
            .as_ref()
            .map(|layout| layout.line_number_width)
    }

    /// The top of buffer line `line` (0-based), relative to the top of the
    /// [`text_bounds`](Self::text_bounds), at the scroll offset the next frame
    /// lays out with (so a view placed with it doesn't lag a scroll by a
    /// frame). `None` before the first layout, or while the line is folded
    /// away.
    pub fn line_top(&self, line: usize) -> Option<Pixels> {
        let layout = self.last_layout.as_ref()?;
        let rows = self.display_map.buffer_line_to_display_row_range(line)?;
        let scroll_y = self
            .deferred_scroll_y_f64
            .unwrap_or_else(|| self.effective_scroll_y_f64());
        let line_height = f64::from(f32::from(layout.line_height));
        Some(px((rows.start as f64 * line_height + scroll_y) as f32))
    }
}
