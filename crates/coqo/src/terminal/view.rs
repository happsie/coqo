// SPDX-License-Identifier: Apache-2.0

//! The terminal tab: draws a mirrored [`Screen`] with GPUI text in JetBrains
//! Mono, and turns keys, the wheel, selection and resizes into backend calls.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use coqo_protocol::methods::terminal::{
    Color, CursorShape, Row, Screen, ScrollTo, Span, Style, TermSize, TerminalEvent, TerminalId,
    TerminalStatus, flags, span_width,
};
use coqo_ui::{
    ActiveTokens as _, Rgb, TerminalPalette,
    dock::TabItem,
    gpui::{
        AnyElement, App, Bounds, ClipboardItem, Context, FocusHandle, FontWeight, Hsla,
        InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
        MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, ScrollEvent,
        SharedString, Styled as _, Task, Window, canvas, div, font, px,
    },
    serde_json::{Value, json},
    widgets::{Icon, TestSupportExt as _},
};
use futures::StreamExt as _;

use super::backend::{TerminalBackend, Update};
use super::{CopyText, KIND, PasteText, keys};

/// Space between the panel edge and the grid.
const PADDING: Pixels = px(10.);

/// Open views per terminal, so closing one of two views of a terminal (after
/// a split) leaves the terminal running.
pub type OpenViews = Rc<RefCell<HashMap<TerminalId, usize>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewState {
    Attaching,
    Live,
    Exited(Option<i32>),
    Lost(SharedString),
}

/// A selection in screen cells, `(row, col)`, anchor where the drag began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Selection {
    /// Start and end (inclusive), in reading order.
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// The view of one daemon terminal.
pub struct TerminalView {
    id: TerminalId,
    backend: Rc<dyn TerminalBackend>,
    views: OpenViews,
    screen: Screen,
    title: SharedString,
    state: ViewState,
    focus: FocusHandle,
    selection: Option<Selection>,
    selecting: bool,
    /// Top-left of the grid in window coordinates, from the last layout.
    origin: Rc<Cell<Point<Pixels>>>,
    cell: Rc<Cell<(Pixels, Pixels)>>,
    sent_size: Option<TermSize>,
    scroll_px: f32,
    _updates: Task<()>,
}

impl std::fmt::Debug for TerminalView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalView")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        let mut views = self.views.borrow_mut();
        if let Some(n) = views.get_mut(&self.id) {
            *n -= 1;
            if *n == 0 {
                views.remove(&self.id);
            }
        }
    }
}

impl TerminalView {
    pub fn new(
        id: TerminalId,
        backend: Rc<dyn TerminalBackend>,
        views: OpenViews,
        cx: &mut Context<Self>,
    ) -> Self {
        *views.borrow_mut().entry(id).or_default() += 1;
        let mut updates = backend.attach(id);
        let task = cx.spawn(async move |this, cx| {
            while let Some(update) = updates.next().await {
                if this.update(cx, |view, cx| view.apply(update, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            id,
            backend,
            views,
            screen: Screen::blank(TermSize::default()),
            title: "Terminal".into(),
            state: ViewState::Attaching,
            focus: cx.focus_handle(),
            selection: None,
            selecting: false,
            origin: Rc::default(),
            cell: Rc::new(Cell::new((px(8.), px(18.)))),
            sent_size: None,
            scroll_px: 0.,
            _updates: task,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn state(&self) -> &ViewState {
        &self.state
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Snapshot(snap) => {
                self.screen = snap.screen;
                self.title = snap.info.title.into();
                self.state = match snap.info.status {
                    TerminalStatus::Running => ViewState::Live,
                    TerminalStatus::Exited { code } => ViewState::Exited(code),
                };
                // The daemon's size may differ from ours: send ours again.
                if self.sent_size.is_some_and(|s| s != self.screen.size) {
                    self.sent_size = None;
                }
            }
            Update::Event(TerminalEvent::Frame(frame)) => self.screen.apply(&frame),
            Update::Event(TerminalEvent::Title { title }) => self.title = title.into(),
            Update::Event(TerminalEvent::Exited { code }) => self.state = ViewState::Exited(code),
            Update::Event(_) => return,
            Update::Lost(reason) => {
                if !matches!(self.state, ViewState::Exited(_)) {
                    self.state = ViewState::Lost(reason.into());
                }
            }
        }
        cx.notify();
    }

    fn live(&self) -> bool {
        self.state == ViewState::Live
    }

    /// Fit the grid to `bounds` (the panel), resizing the PTY if it changed.
    fn fit(&mut self, bounds: Bounds<Pixels>) {
        self.origin
            .set(bounds.origin + Point::new(PADDING, PADDING));
        if !matches!(self.state, ViewState::Live | ViewState::Attaching) {
            return;
        }
        let (cw, lh) = self.cell.get();
        let cols = ((bounds.size.width - PADDING * 2.) / cw).floor().max(1.);
        let rows = ((bounds.size.height - PADDING * 2.) / lh).floor().max(1.);
        let size = TermSize::new(cols as u16, rows as u16);
        if self.sent_size != Some(size) {
            self.sent_size = Some(size);
            self.backend.resize(self.id, size);
        }
    }

    /// The cell under a window position.
    fn cell_at(&self, position: Point<Pixels>) -> (u16, u16) {
        let (cw, lh) = self.cell.get();
        let rel = position - self.origin.get();
        let max_row = self.screen.size.rows.saturating_sub(1);
        let max_col = self.screen.size.cols.saturating_sub(1);
        let row = (rel.y / lh).floor().clamp(0., f32::from(max_row));
        let col = (rel.x / cw).floor().clamp(0., f32::from(max_col));
        (row as u16, col as u16)
    }

    /// The selected text, rows joined with newlines.
    pub fn selected_text(&self) -> Option<String> {
        let ((r0, c0), (r1, c1)) = self.selection?.ordered();
        let cols = self.screen.size.cols;
        let lines: Vec<String> = (r0..=r1)
            .filter_map(|r| {
                let row = self.screen.rows.get(usize::from(r))?;
                let start = if r == r0 { c0 } else { 0 };
                let end = if r == r1 { c1 + 1 } else { cols };
                Some(row.text_in(start..end))
            })
            .collect();
        Some(lines.join("\n"))
    }

    fn send(&mut self, data: String, cx: &mut Context<Self>) {
        if self.live() && !data.is_empty() {
            self.backend.input(self.id, data);
            if self.selection.take().is_some() {
                cx.notify();
            }
        }
    }

    fn key_down(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(data) = keys::encode(&e.keystroke, &self.screen.modes) {
            self.send(data, cx);
            cx.stop_propagation();
        }
    }

    fn copy(&mut self, _: &CopyText, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text().filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &PasteText, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let data = keys::paste(&text, &self.screen.modes);
            self.send(data, cx);
        }
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let cell = self.cell_at(e.position);
        self.selection = Some(Selection {
            anchor: cell,
            head: cell,
        });
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting && e.pressed_button == Some(MouseButton::Left) {
            let head = self.cell_at(e.position);
            if let Some(sel) = self.selection.as_mut()
                && sel.head != head
            {
                sel.head = head;
                cx.notify();
            }
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        if self.selection.is_some_and(|s| s.anchor == s.head) {
            self.selection = None;
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, e: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (_, lh) = self.cell.get();
        self.scroll_px += e.delta.pixel_delta(lh).y.as_f32();
        let lines = (self.scroll_px / lh.as_f32()).trunc();
        if lines == 0. || !self.live() {
            return;
        }
        self.scroll_px -= lines * lh.as_f32();
        let lines = lines as i32;
        if self.screen.modes.alt_screen {
            // No scrollback on the alternate screen: scroll the program.
            let key = if lines > 0 { "up" } else { "down" };
            let keystroke = coqo_ui::gpui::Keystroke::parse(key).expect("valid key");
            let data = keys::encode(&keystroke, &self.screen.modes).unwrap_or_default();
            self.send(data.repeat(lines.unsigned_abs() as usize), cx);
        } else {
            self.backend.scroll(self.id, ScrollTo::Lines(lines));
        }
    }
}

/// Foreground and (non-default) background of a style.
fn colors(style: &Style, t: &TerminalPalette) -> (Hsla, Option<Hsla>) {
    let resolve = |c: Color, default: Rgb| match c {
        Color::Default => default,
        Color::Indexed(i) => t.indexed(i),
        Color::Rgb(r, g, b) => Rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)),
    };
    let mut fg = resolve(style.fg, t.foreground);
    let mut bg = (style.bg != Color::Default).then(|| resolve(style.bg, t.background));
    if style.has(flags::BOLD)
        && let Color::Indexed(i @ 0..=7) = style.fg
    {
        fg = t.ansi[usize::from(i) + 8];
    }
    if style.has(flags::INVERSE) {
        let old_bg = bg.unwrap_or(t.background);
        bg = Some(fg);
        fg = old_bg;
    }
    let mut fg = fg.hsla();
    if style.has(flags::DIM) {
        fg = fg.opacity(0.6);
    }
    if style.has(flags::HIDDEN) {
        fg = fg.opacity(0.);
    }
    (fg, bg.map(Rgb::hsla))
}

fn span_element(span: &Span, t: &TerminalPalette, cw: Pixels, lh: Pixels) -> AnyElement {
    let (fg, bg) = colors(&span.style, t);
    let mut el = div()
        .absolute()
        .left(cw * f32::from(span.col))
        .top(px(0.))
        .h(lh)
        .w(cw * span_width(span) as f32)
        .whitespace_nowrap()
        .text_color(fg)
        .child(span.text.clone());
    if let Some(bg) = bg {
        el = el.bg(bg);
    }
    let s = &span.style;
    if s.has(flags::BOLD) {
        el = el.font_weight(FontWeight::BOLD);
    }
    if s.has(flags::ITALIC) {
        el = el.italic();
    }
    if s.has(flags::UNDERLINE) {
        el = el.underline();
    }
    if s.has(flags::STRIKEOUT) {
        el = el.line_through();
    }
    el.into_any_element()
}

/// The character in a row at `col`, for drawing it over a block cursor.
fn char_at(row: &Row, col: u16) -> Option<(char, Style)> {
    row.spans.iter().find_map(|span| {
        let width = if span.wide { 2 } else { 1 };
        let offset = col.checked_sub(span.col)? / width;
        span.text
            .chars()
            .nth(usize::from(offset))
            .map(|c| (c, span.style))
    })
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = cx.tokens();
        let (p, m) = (tokens.palette.clone(), tokens.metrics.clone());
        let t = tokens.terminal();
        let font_size = m.text_md;
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font(m.font_mono.clone()));
        let cw = text_system
            .advance(font_id, font_size, 'm')
            .map_or(px(7.8), |s| s.width);
        let lh = px((font_size.as_f32() * 1.4).round());
        self.cell.set((cw, lh));
        let focused = self.focus.is_focused(window);

        let mut grid = div()
            .absolute()
            .top(PADDING)
            .left(PADDING)
            .size_full()
            .font_family(m.font_mono.clone())
            .text_size(font_size)
            .line_height(lh)
            .text_color(t.foreground.hsla());

        // Selection under the text.
        if let Some(sel) = self.selection {
            let ((r0, c0), (r1, c1)) = sel.ordered();
            for r in r0..=r1 {
                let start = if r == r0 { c0 } else { 0 };
                let end = if r == r1 {
                    c1 + 1
                } else {
                    self.screen.size.cols
                };
                grid = grid.child(
                    div()
                        .absolute()
                        .top(lh * f32::from(r))
                        .left(cw * f32::from(start))
                        .w(cw * f32::from(end.saturating_sub(start)))
                        .h(lh)
                        .bg(t.selection.alpha(TerminalPalette::SELECTION_ALPHA)),
                );
            }
        }

        for (r, row) in self.screen.rows.iter().enumerate() {
            if row.spans.is_empty() {
                continue;
            }
            grid = grid.child(
                div()
                    .absolute()
                    .top(lh * r as f32)
                    .left(px(0.))
                    .h(lh)
                    .w_full()
                    .children(row.spans.iter().map(|s| span_element(s, &t, cw, lh))),
            );
        }

        if self.live()
            && let Some(cursor) = self.screen.cursor
        {
            let at = div()
                .id("terminal-cursor")
                .test_support()
                .absolute()
                .top(lh * f32::from(cursor.row))
                .left(cw * f32::from(cursor.col));
            let color = t.cursor.hsla();
            let el = match (focused, cursor.shape) {
                (true, CursorShape::Block) => {
                    let glyph = self
                        .screen
                        .rows
                        .get(usize::from(cursor.row))
                        .and_then(|row| char_at(row, cursor.col));
                    at.w(cw)
                        .h(lh)
                        .bg(color)
                        .text_color(t.background.hsla())
                        .children(glyph.map(|(c, style)| {
                            let el = div().child(c.to_string());
                            if style.has(flags::BOLD) {
                                el.font_weight(FontWeight::BOLD)
                            } else {
                                el
                            }
                        }))
                }
                (true, CursorShape::Beam) => at.w(px(2.)).h(lh).bg(color),
                (true, CursorShape::Underline) => at.mt(lh - px(2.)).w(cw).h(px(2.)).bg(color),
                _ => at.w(cw).h(lh).border_1().border_color(color),
            };
            grid = grid.child(el);
        }

        let banner = match &self.state {
            ViewState::Live | ViewState::Attaching => None,
            ViewState::Exited(code) => Some(match code {
                Some(code) => format!("Process exited with code {code}"),
                None => "Process ended by a signal".to_owned(),
            }),
            ViewState::Lost(reason) => Some(format!("Disconnected: {reason}")),
        };

        let this = cx.entity().downgrade();
        div()
            .id(("terminal", self.id.0 as usize))
            .test_support()
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .overflow_hidden()
            // No fill: the background is the dock panel's own (rounded) colour.
            .cursor_text()
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll(cx.listener(Self::scroll_wheel))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        this.update(cx, |view, _| view.fit(bounds)).ok();
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(grid)
            .children(banner.map(|text| {
                div()
                    .id("terminal-banner")
                    .test_support()
                    .absolute()
                    .bottom(PADDING)
                    .right(PADDING)
                    .px_3()
                    .py_1()
                    .rounded(m.radius_control)
                    .bg(p.surface.hsla())
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .child(text)
            }))
    }
}

impl TabItem for TerminalView {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        self.title.clone()
    }

    fn icon(&self, _: &App) -> Option<Icon> {
        Some(Icon::Terminal)
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(format!("terminal:{}", self.id).into())
    }

    fn save(&self, _: &App) -> Value {
        json!({ "id": self.id.0 })
    }

    fn focus_handle(&self, _: &App) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }

    /// Closing the last tab of a terminal ends it; quitting the app doesn't.
    fn on_close(&mut self, _: &mut Context<Self>) {
        let others = self
            .views
            .borrow()
            .get(&self.id)
            .copied()
            .unwrap_or(0)
            .saturating_sub(1);
        if others == 0 {
            self.backend.close(self.id);
        }
    }
}
