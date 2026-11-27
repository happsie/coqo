// SPDX-License-Identifier: Apache-2.0

//! Markers on code and a view anchored to it (issue #74, design board 2).
//!
//! - **Markers** ([`CodeEditor::set_markers`]): a bar in the gutter strip, a
//!   band behind the marked lines and a diamond in the gap between the line
//!   numbers and the code, on the first line. The diamond is a button: a click
//!   emits [`MarkerClicked`]. A marker with `pulse` set (an agent waits for
//!   it) has a stronger band and a pulsing diamond. Marker ranges follow
//!   edits like the editor's decorations ([`CodeEditor::marker_range`]).
//! - **The ask button** ([`CodeEditor::set_ask_enabled`], #229): a speech
//!   bubble in the same gap, on the line under the mouse and on the first
//!   line of the selection while the editor has the keyboard. A click emits
//!   [`AskClicked`] with the lines to ask about.
//! - **The overlay** ([`CodeEditor::set_overlay`]): one view (the question
//!   popover) drawn over the code, below the lines of a marker with a caret
//!   pointing at them, or above them when there is more room there; or at the
//!   top of the editor. It moves with scrolling and edits, and blocks the
//!   mouse from the code under it.
//!
//! Positions come from the editor's last layout and the current scroll
//! offset (the `overlay_geometry` patch of gpui-base, ADR 0008).

use std::{cell::Cell, ops::Range, rc::Rc};

use gpui_kit::component::input::{
    GutterDecoration, GutterDecorationCollection, Rope, RopeExt as _,
};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, AnyView, App, Bounds, Context, ElementId, Hsla,
    InteractiveElement as _, IntoElement as _, ParentElement as _, Pixels, Size,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, pulsating_between, px,
};

use super::{CODE_LINE_HEIGHT, CodeEditor, GUTTER_GAP, REVEAL_FRAMES};
use crate::{
    theme::ActiveTokens as _,
    widgets::{Icon, TestSupportExt as _, icon},
};

/// Opacity of the band behind marked lines (board 2: sand at 13%).
const BAND: f32 = 0.13;
/// …and while an agent waits for the marker's question.
const BAND_PULSE: f32 = 0.2;
/// The diamond's size.
const DIAMOND: f32 = 9.;
/// Space between the marked lines and the overlay, where its caret sits.
const OVERLAY_GAP: f32 = 8.;
/// The overlay keeps this far from the editor's edges.
const OVERLAY_MARGIN: f32 = 12.;
/// The least height of an overlay squeezed by the lines it is about.
const OVERLAY_MIN_HEIGHT: f32 = 120.;
/// Widest overlay (board 2: 620 px).
const OVERLAY_MAX_WIDTH: f32 = 620.;
/// The caret's size and where it sits from the overlay's left edge.
const CARET: f32 = 16.;
const CARET_LEFT: f32 = 40.;

/// A marker on code. See the module docs.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeMarker {
    /// The owner's id for it, reported by [`MarkerClicked`].
    pub id: u64,
    /// UTF-8 byte range; every line it touches is marked.
    pub range: Range<usize>,
    pub color: Hsla,
    /// Draw attention: a stronger band and a pulsing diamond.
    pub pulse: bool,
}

/// A marker's diamond was clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerClicked(pub u64);

/// The ask button was clicked: ask about these lines (0-based, `end`
/// exclusive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskClicked {
    pub lines: Range<usize>,
}

/// Where [`CodeEditor::set_overlay`] puts its view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayAnchor {
    /// At the lines of this marker. If the marker is gone, at the top.
    Marker(u64),
    /// At these lines (0-based, `end` exclusive), as they are now.
    Lines { start: usize, end: usize },
    /// At the top of the editor.
    Top,
}

struct MarkerState {
    marker: CodeMarker,
    gutter: GutterDecorationCollection,
}

struct Overlay {
    anchor: OverlayAnchor,
    view: AnyView,
    /// The overlay's size in the last frame.
    size: Rc<Cell<Option<Size<Pixels>>>>,
}

/// The markers and the overlay of one editor.
#[derive(Default)]
pub(super) struct Overlays {
    markers: Vec<MarkerState>,
    overlay: Option<Overlay>,
    /// The editor's bounds in the last frame.
    frame: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Whether the ask button shows.
    ask: bool,
    /// The line under the mouse.
    hovered: Option<usize>,
    /// A marker to scroll near the top once laid out, and the frames left
    /// to wait for that.
    scroll_to: Option<(u64, usize)>,
}

/// The lines `range` touches in `text`: the line of its start up to the
/// line of its last byte (an empty range is its line).
fn lines_of(text: &Rope, range: &Range<usize>) -> Range<usize> {
    let len = text.len();
    let first = text.offset_to_point(range.start.min(len)).row;
    let last = if range.end > range.start {
        text.offset_to_point((range.end - 1).min(len)).row
    } else {
        first
    };
    first..last.max(first) + 1
}

impl CodeEditor {
    /// Replace the markers. A marker whose id was already shown is moved to
    /// its new range (pass [`marker_range`](Self::marker_range) to keep it
    /// where edits took it).
    pub fn set_markers(&mut self, markers: Vec<CodeMarker>, cx: &mut Context<Self>) {
        let mut old = std::mem::take(&mut self.overlays.markers);
        for marker in markers {
            let marks = vec![GutterDecoration::new(marker.range.clone()).with_color(marker.color)];
            let gutter = match old.iter().position(|m| m.marker.id == marker.id) {
                Some(ix) => {
                    let kept = old.swap_remove(ix);
                    kept.gutter.set(marks, cx);
                    kept.gutter
                }
                None => self.state.update(cx, |state, cx| {
                    state.create_gutter_decorations_collection(marks, cx)
                }),
            };
            self.overlays.markers.push(MarkerState { marker, gutter });
        }
        for gone in old {
            gone.gutter.dispose(cx);
        }
        cx.notify();
    }

    /// The markers, in the order they were set.
    pub fn markers(&self) -> Vec<CodeMarker> {
        self.overlays
            .markers
            .iter()
            .map(|m| m.marker.clone())
            .collect()
    }

    /// Where marker `id` is now: its range after the edits since it was set.
    pub fn marker_range(&self, id: u64, cx: &App) -> Option<Range<usize>> {
        let marker = self.overlays.markers.iter().find(|m| m.marker.id == id)?;
        marker.gutter.get_ranges(cx).into_iter().next()
    }

    /// The lines (0-based, end exclusive) each marker is on now.
    pub fn marker_lines(&self, cx: &App) -> Vec<(u64, Range<usize>)> {
        let text = self.state.read(cx).text();
        self.overlays
            .markers
            .iter()
            .filter_map(|m| {
                let range = m.gutter.get_ranges(cx).into_iter().next()?;
                Some((m.marker.id, lines_of(text, &range)))
            })
            .collect()
    }

    /// The 0-based line of byte `offset`.
    pub fn line_of(&self, offset: usize, cx: &App) -> usize {
        let text = self.state.read(cx).text();
        text.offset_to_point(offset.min(text.len())).row
    }

    /// The byte offset where 0-based `line` starts (the end of the text past
    /// the last line).
    pub fn line_start(&self, line: usize, cx: &App) -> usize {
        let text = self.state.read(cx).text();
        if line >= text.lines_len() {
            text.len()
        } else {
            text.line_start_offset(line)
        }
    }

    /// Show `view` over the code at `anchor`, replacing any other.
    pub fn set_overlay(&mut self, anchor: OverlayAnchor, view: AnyView, cx: &mut Context<Self>) {
        let same = self
            .overlays
            .overlay
            .as_ref()
            .is_some_and(|o| o.view.entity_id() == view.entity_id());
        let size = match self.overlays.overlay.take() {
            Some(o) if same => o.size,
            _ => Rc::default(),
        };
        self.overlays.overlay = Some(Overlay { anchor, view, size });
        cx.notify();
    }

    /// Remove the overlay.
    pub fn clear_overlay(&mut self, cx: &mut Context<Self>) {
        if self.overlays.overlay.take().is_some() {
            cx.notify();
        }
    }

    /// Where the overlay is, if one is shown.
    pub fn overlay_anchor(&self) -> Option<OverlayAnchor> {
        self.overlays.overlay.as_ref().map(|o| o.anchor)
    }

    /// The top of 0-based `line` relative to the editor's top, as the next
    /// frame lays it out; `None` before the first layout or while it is
    /// folded away.
    pub fn line_top(&self, line: usize, cx: &App) -> Option<Pixels> {
        let frame = self.overlays.frame.get()?;
        let state = self.state.read(cx);
        let text = state.text_bounds()?;
        Some(text.origin.y - frame.origin.y + state.line_top(line)?)
    }

    /// Show the ask button (on the hovered line and the selection).
    pub fn set_ask_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.overlays.ask != enabled {
            self.overlays.ask = enabled;
            cx.notify();
        }
    }

    /// The lines of the primary selection (0-based, `end` exclusive): what
    /// the user asks about. A selection ending at the start of a line leaves
    /// that line out.
    pub fn selected_lines(&self, cx: &App) -> Range<usize> {
        let state = self.state.read(cx);
        let range = self.selections(cx).first().cloned().unwrap_or(0..0);
        lines_of(state.text(), &range)
    }

    /// The line at `y` in the window, if it is a line of the text on screen.
    pub(super) fn hover_at(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let line = self.line_at(y, cx);
        if line != self.overlays.hovered {
            self.overlays.hovered = line;
            if self.overlays.ask {
                cx.notify();
            }
        }
    }

    fn line_at(&self, y: Pixels, cx: &App) -> Option<usize> {
        let frame = self.overlays.frame.get()?;
        if y < frame.origin.y || y >= frame.origin.y + frame.size.height {
            return None;
        }
        let state = self.state.read(cx);
        let text = state.text_bounds()?;
        let rel = y - text.origin.y;
        let lines = self.visible_lines(cx)?;
        let count = state.text().lines_len();
        lines.filter(|l| *l < count).find(|&l| {
            state
                .line_top(l)
                .is_some_and(|top| top <= rel && rel < top + px(CODE_LINE_HEIGHT))
        })
    }

    /// Scroll so marker `id`'s first line sits near the top of the editor,
    /// leaving the room below it for an overlay. Waits for the first layout.
    pub fn scroll_marker_to_top(&mut self, id: u64, cx: &mut Context<Self>) {
        self.overlays.scroll_to = Some((id, REVEAL_FRAMES));
        cx.notify();
    }

    /// Apply [`Self::scroll_marker_to_top`] once the editor was laid out.
    fn scroll_pending_marker(&mut self, cx: &mut Context<Self>) {
        let Some((id, frames)) = self.overlays.scroll_to.take() else {
            return;
        };
        let Some(range) = self.marker_range(id, cx) else {
            return;
        };
        let line = self.line_of(range.start, cx);
        match self.state.read(cx).line_top(line) {
            Some(top) => {
                // Two lines of context above it.
                let delta = f64::from(2. * CODE_LINE_HEIGHT - f32::from(top));
                self.state.update(cx, |state, cx| {
                    let y = (state.scroll_y_f64() + delta).min(0.);
                    state.set_scroll_y_f64(y, cx);
                });
            }
            None if frames > 0 => {
                self.overlays.scroll_to = Some((id, frames - 1));
                cx.notify();
            }
            None => {}
        }
    }

    /// The measuring element and the markers' and overlay's elements, drawn
    /// over the editor.
    pub(super) fn render_overlays(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.scroll_pending_marker(cx);
        let frame = self.overlays.frame.clone();
        let mut elements = vec![
            canvas(move |bounds, _, _| frame.set(Some(bounds)), |_, _, _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .into_any_element(),
        ];
        if self.overlays.markers.is_empty() && self.overlays.overlay.is_none() && !self.overlays.ask
        {
            return elements;
        }
        let geometry = {
            let state = self.state.read(cx);
            match (
                self.overlays.frame.get(),
                state.text_bounds(),
                state.gutter_width(),
            ) {
                (Some(frame), Some(text), Some(gutter)) => Some((frame, text, gutter)),
                _ => None,
            }
        };
        let Some((frame, text_bounds, gutter)) = geometry else {
            // Not laid out yet: place them on the next frame.
            cx.on_next_frame(window, |_, _, cx| cx.notify());
            return elements;
        };
        let line_height = px(CODE_LINE_HEIGHT);
        let dx = text_bounds.origin.x - frame.origin.x;
        let dy = text_bounds.origin.y - frame.origin.y;
        let height = frame.size.height;
        let state = self.state.read(cx);
        let text = state.text();
        // Each marker's lines, as tops and bottoms in the editor.
        let placed: Vec<(CodeMarker, Pixels, Pixels)> = self
            .overlays
            .markers
            .iter()
            .filter_map(|m| {
                let range = m.gutter.get_ranges(cx).into_iter().next()?;
                let lines = lines_of(text, &range);
                let top = dy + state.line_top(lines.start)?;
                let bottom = state
                    .line_top(lines.end - 1)
                    .map_or(top + line_height, |t| dy + t + line_height)
                    .max(top + line_height);
                Some((m.marker.clone(), top, bottom))
            })
            .collect();

        for (marker, top, bottom) in &placed {
            if *bottom < px(0.) || *top > height {
                continue;
            }
            let band = if marker.pulse { BAND_PULSE } else { BAND };
            elements.push(
                div()
                    .absolute()
                    .left(dx)
                    .right(px(10.))
                    .top(*top)
                    .h(*bottom - *top)
                    .rounded_r(px(8.))
                    .bg(marker.color.opacity(band))
                    .into_any_element(),
            );
            let diamond = icon(Icon::MarkerDiamond, px(DIAMOND), marker.color);
            let diamond = if marker.pulse {
                diamond
                    .with_animation(
                        ElementId::NamedInteger("code-marker-pulse".into(), marker.id),
                        Animation::new(std::time::Duration::from_millis(1400))
                            .repeat()
                            .with_easing(pulsating_between(0.35, 1.)),
                        |diamond, delta| diamond.opacity(delta),
                    )
                    .into_any_element()
            } else {
                diamond.into_any_element()
            };
            let id = marker.id;
            elements.push(
                div()
                    .id(ElementId::NamedInteger("code-marker".into(), id))
                    .test_support()
                    .absolute()
                    .left(dx + gutter - px(GUTTER_GAP))
                    .top(*top)
                    .w(px(GUTTER_GAP))
                    .h(line_height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .occlude()
                    .child(diamond)
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(MarkerClicked(id))))
                    .into_any_element(),
            );
        }

        // The ask button: on the hovered line, else (with the keyboard in
        // the editor) the selection's first line; not over a diamond.
        let focused = gpui_kit::Focusable::focus_handle(self.state.read(cx), cx).is_focused(window);
        let selected = self.selected_lines(cx);
        let ask_line = self
            .overlays
            .hovered
            .or_else(|| focused.then_some(selected.start))
            .filter(|_| self.overlays.ask && self.overlays.overlay.is_none());
        if let Some(line) = ask_line {
            let state = self.state.read(cx);
            let marked = placed.iter().any(|(_, top, _)| {
                state
                    .line_top(line)
                    .is_some_and(|t| (dy + t - *top).abs() < px(1.))
            });
            if let Some(top) = state.line_top(line).map(|t| dy + t)
                && !marked
                && top >= px(0.)
                && top < height
            {
                let lines = if selected.contains(&line) {
                    selected.clone()
                } else {
                    line..line + 1
                };
                let p = &cx.tokens().palette;
                elements.push(
                    div()
                        .id("code-ask")
                        .test_support()
                        .absolute()
                        .left(dx + gutter - px(GUTTER_GAP))
                        .top(top)
                        .w(px(GUTTER_GAP))
                        .h(line_height)
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .occlude()
                        .child(icon(Icon::MessageCircle, px(13.), p.blue.hsla()))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(AskClicked {
                                lines: lines.clone(),
                            })
                        }))
                        .into_any_element(),
                );
            }
        }

        if let Some(overlay) = &self.overlays.overlay {
            let state = self.state.read(cx);
            let target = match overlay.anchor {
                OverlayAnchor::Marker(id) => placed
                    .iter()
                    .find(|(m, ..)| m.id == id)
                    .map(|(_, top, bottom)| (*top, *bottom)),
                OverlayAnchor::Lines { start, end } => state.line_top(start).map(|t| {
                    let top = dy + t;
                    let bottom = state
                        .line_top(end.max(start + 1) - 1)
                        .map_or(top + line_height, |t| dy + t + line_height);
                    (top, bottom.max(top + line_height))
                }),
                OverlayAnchor::Top => None,
            };
            let left = (dx + gutter).min(frame.size.width - px(OVERLAY_MARGIN + 200.));
            let left = left.max(px(OVERLAY_MARGIN));
            let width = (frame.size.width - left - px(OVERLAY_MARGIN)).min(px(OVERLAY_MAX_WIDTH));
            // Its height without a limit (the content's).
            let measured = overlay.size.get().map(|s| s.height).unwrap_or(px(0.));
            let full = (height - px(2. * OVERLAY_MARGIN)).max(px(OVERLAY_MIN_HEIGHT));
            // Below the lines if it fits there, else above if it fits there,
            // else on the roomier side with its content scrolling: never over
            // the lines it is about.
            let (top, max_height, caret) = match target {
                Some((top, bottom)) => {
                    let below = bottom + px(OVERLAY_GAP);
                    let room_below = height - below - px(OVERLAY_MARGIN);
                    let room_above = top - px(OVERLAY_GAP + OVERLAY_MARGIN);
                    let above = measured > room_below
                        && (measured <= room_above || room_above > room_below);
                    if above {
                        let max = room_above.max(px(OVERLAY_MIN_HEIGHT));
                        let at =
                            (top - px(OVERLAY_GAP) - measured.min(max)).max(px(OVERLAY_MARGIN));
                        (at, max, Some((false, top - px(OVERLAY_GAP))))
                    } else {
                        let max = room_below.max(px(OVERLAY_MIN_HEIGHT));
                        let at = below.min(height - px(OVERLAY_MARGIN) - measured.min(max));
                        (at.max(px(OVERLAY_MARGIN)), max, Some((true, below)))
                    }
                }
                None => (px(OVERLAY_MARGIN), full, None),
            };
            let shown = measured.min(max_height);
            let size = overlay.size.clone();
            elements.push(
                div()
                    .id("code-overlay")
                    .test_support()
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(width)
                    .max_h(max_height)
                    .occlude()
                    .overflow_y_scroll()
                    .child(
                        div().relative().child(overlay.view.clone()).child(
                            canvas(
                                move |bounds, _, _| size.set(Some(bounds.size)),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        ),
                    )
                    .into_any_element(),
            );
            // The caret points at the lines while they are on screen.
            if let Some((below, edge)) = caret
                && edge > px(0.)
                && edge < height
                && (if below {
                    edge <= top + px(1.)
                } else {
                    edge >= top + shown - px(1.)
                })
            {
                let p = &cx.tokens().palette;
                // The triangles fill a quarter to three quarters of the
                // icon's height; their base covers the overlay's border.
                let (glyph, y) = if below {
                    (Icon::CaretUp, edge + px(1. - CARET * 0.75))
                } else {
                    (Icon::CaretDown, edge - px(1. + CARET * 0.25))
                };
                elements.push(
                    div()
                        .absolute()
                        .left(left + px(CARET_LEFT))
                        .top(y)
                        .child(icon(glyph, px(CARET), p.surface.hsla()))
                        .into_any_element(),
                );
            }
        }
        elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_of_ranges() {
        let text = Rope::from("one\ntwo\nthree\n");
        assert_eq!(lines_of(&text, &(0..4)), 0..1, "a whole line");
        assert_eq!(lines_of(&text, &(4..14)), 1..3, "two lines");
        assert_eq!(lines_of(&text, &(5..5)), 1..2, "empty: its line");
        assert_eq!(lines_of(&text, &(14..14)), 3..4, "the end");
    }
}
