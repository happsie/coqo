// SPDX-License-Identifier: Apache-2.0

//! Text inputs, wrapped so views never touch gpui-component's input directly.
//!
//! [`PromptInput`] is the multi-line field of a chat composer: it grows with
//! its text, `Enter` submits, `Shift+Enter` inserts a newline, and `Escape`
//! (when the field has nothing of its own to dismiss) asks to cancel.
//!
//! It also carries what a composer needs for completions:
//!
//! - **Mentions:** atomic inline chips ([`Mention`]) that edit as one unit and
//!   keep an id (a resource URI, say) next to their text.
//! - **Captured keys:** while [`PromptInput::set_capture`] is on (a popover is
//!   open), `Up`, `Down`, `Enter`, `Tab` and `Escape` are reported as
//!   [`PromptInputEvent::Key`] instead of editing the text.
//! - **History keys:** `Up` in an empty field, and `Up`/`Down` while the field
//!   still holds a [`recalled`](PromptInput::recall) prompt, are reported as
//!   keys too, so the owner can step through earlier prompts.

use std::ops::Range;

use gpui_kit::component::input::{
    Enter, Escape, IndentInline, InlineToken, InputContent, InputEvent, MoveDown, MoveUp, Textarea,
    TextareaState,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, div, px,
};

use crate::ActiveTokens as _;

/// The fields' `Enter` action (`secondary` is `Ctrl/Cmd+Enter`), for a view
/// that handles it before a field does, with `capture_action`.
pub use gpui_kit::component::input::Enter as InputEnter;

/// Emitted by a [`PromptInput`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptInputEvent {
    /// `Enter` (or `Ctrl/Cmd+Enter`) was pressed: send the text.
    Submit,
    /// `Escape` was pressed and the field had nothing to dismiss.
    Cancel,
    /// The text changed.
    Changed,
    /// A key the owner asked for (see the module docs). The field did nothing
    /// with it.
    Key(PromptKey),
}

/// A key reported by [`PromptInputEvent::Key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKey {
    /// `Up`.
    Up,
    /// `Down`.
    Down,
    /// `Enter` (only while capturing).
    Accept,
    /// `Tab` (only while capturing).
    Complete,
    /// `Escape` (only while capturing).
    Dismiss,
}

/// An atomic chip in the text, such as `@src/main.rs`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Mention {
    /// What it refers to, such as a `file://` URI. Not shown.
    pub id: SharedString,
    /// The text it stands for in [`PromptInput::text`], such as
    /// `@src/main.rs`. Single line.
    pub text: SharedString,
    /// What the chip shows, such as `@main.rs`.
    pub label: SharedString,
}

/// A mention and its byte range in the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionSpan {
    /// UTF-8 byte range of [`Mention::text`] in the text.
    pub range: Range<usize>,
    /// The mention.
    pub mention: Mention,
}

/// A growing multi-line text field for prompts.
pub struct PromptInput {
    state: Entity<TextareaState>,
    /// Popover keys go to the owner.
    capture: bool,
    /// The text last set by [`Self::recall`], while it is unchanged.
    recalled: Option<SharedString>,
    _events: Subscription,
}

impl std::fmt::Debug for PromptInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptInput")
            .field("capture", &self.capture)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<PromptInputEvent> for PromptInput {}

fn token(mention: &Mention) -> InlineToken {
    InlineToken::new(mention.id.clone(), mention.text.clone()).with_label(mention.label.clone())
}

impl PromptInput {
    /// A field showing `placeholder` while empty, between `min_rows` and
    /// `max_rows` lines tall.
    pub fn new(
        placeholder: impl Into<SharedString>,
        min_rows: usize,
        max_rows: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = placeholder.into();
        let state = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(placeholder)
                .auto_grow(min_rows, max_rows)
                .submit_on_enter(true)
        });
        let events = cx.subscribe(&state, |this, state, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { shift: false, .. } => cx.emit(PromptInputEvent::Submit),
            InputEvent::Change => {
                if this
                    .recalled
                    .as_ref()
                    .is_some_and(|r| *r != state.read(cx).value())
                {
                    this.recalled = None;
                }
                cx.emit(PromptInputEvent::Changed);
            }
            _ => {}
        });
        Self {
            state,
            capture: false,
            recalled: None,
            _events: events,
        }
    }

    /// The text. Mentions appear as their [`Mention::text`].
    pub fn text(&self, cx: &App) -> String {
        self.state.read(cx).value().to_string()
    }

    /// The cursor's byte offset in the text.
    pub fn cursor(&self, cx: &App) -> usize {
        self.state.read(cx).cursor()
    }

    /// The mentions, in text order.
    pub fn mentions(&self, cx: &App) -> Vec<MentionSpan> {
        self.state
            .read(cx)
            .tokens()
            .iter()
            .map(|span| MentionSpan {
                range: span.range(),
                mention: Mention {
                    id: span.token().id().clone(),
                    text: span.token().text().clone(),
                    label: span.token().label().clone(),
                },
            })
            .collect()
    }

    /// Replace the text (not recorded in the undo history) and put the
    /// cursor at its end.
    pub fn set_text(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_content(text, Vec::new(), window, cx);
    }

    /// Replace the text and its mentions (each span's range must hold its
    /// mention's text; others are dropped) and put the cursor at the end.
    pub fn set_content(
        &mut self,
        text: impl Into<SharedString>,
        mentions: Vec<MentionSpan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        let mut content = InputContent::new(text.clone());
        for span in mentions {
            let with = content.clone().with_token(span.range, token(&span.mention));
            if let Ok(with) = with {
                content = with;
            }
        }
        self.recalled = None;
        let end = text.len();
        self.state.update(cx, |state, cx| {
            state.set_value(content, window, cx);
            state.set_selected_range(end..end, cx);
        });
        cx.emit(PromptInputEvent::Changed);
    }

    /// Show an earlier prompt. While it stays unchanged, `Up` and `Down` are
    /// reported as [`PromptKey`]s, so the owner can step on.
    pub fn recall(
        &mut self,
        text: impl Into<SharedString>,
        mentions: Vec<MentionSpan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.set_content(text.clone(), mentions, window, cx);
        self.recalled = Some(text);
    }

    /// Whether the field holds a [`recalled`](Self::recall) prompt, unchanged.
    pub fn is_recalled(&self, cx: &App) -> bool {
        self.recalled
            .as_ref()
            .is_some_and(|r| *r == self.state.read(cx).value())
    }

    /// Replace the byte `range` with a mention chip followed by a space, and
    /// put the cursor after it. Returns whether it was inserted.
    pub fn insert_mention(
        &mut self,
        range: Range<usize>,
        mention: Mention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let inserted = self.state.update(cx, |state, cx| {
            let ok = state
                .replace_range_with_token(range, token(&mention), window, cx)
                .is_ok();
            if ok {
                state.insert(" ", window, cx);
            }
            ok
        });
        cx.emit(PromptInputEvent::Changed);
        inserted
    }

    /// Replace the byte `range` with `text` and put the cursor after it.
    pub fn replace_range(
        &mut self,
        range: Range<usize>,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.state.update(cx, |state, cx| {
            state.set_selected_range(range, cx);
            state.replace(text, window, cx);
        });
        cx.emit(PromptInputEvent::Changed);
    }

    /// While on, `Up`, `Down`, `Enter`, `Tab` and `Escape` are reported as
    /// [`PromptInputEvent::Key`] instead of editing (a popover is open).
    pub fn set_capture(&mut self, capture: bool) {
        self.capture = capture;
    }

    /// Whether keys are captured.
    pub fn is_capturing(&self) -> bool {
        self.capture
    }

    fn key(&self, key: PromptKey, cx: &App) -> bool {
        if self.capture {
            return true;
        }
        match key {
            PromptKey::Up => self.state.read(cx).value().is_empty() || self.is_recalled(cx),
            PromptKey::Down => self.is_recalled(cx),
            _ => false,
        }
    }

    fn capture_key(&mut self, key: PromptKey, cx: &mut Context<Self>) {
        if self.key(key, cx) {
            cx.emit(PromptInputEvent::Key(key));
            cx.stop_propagation();
        }
    }
}

impl Focusable for PromptInput {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.read(cx).focus_handle(cx)
    }
}

impl Render for PromptInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            // Capture runs before the field's own handlers.
            .capture_action(
                cx.listener(|this, _: &MoveUp, _, cx| this.capture_key(PromptKey::Up, cx)),
            )
            .capture_action(
                cx.listener(|this, _: &MoveDown, _, cx| this.capture_key(PromptKey::Down, cx)),
            )
            .capture_action(cx.listener(|this, enter: &Enter, _, cx| {
                if !enter.shift && !enter.secondary {
                    this.capture_key(PromptKey::Accept, cx)
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, _, cx| {
                this.capture_key(PromptKey::Complete, cx)
            }))
            .capture_action(
                cx.listener(|this, _: &Escape, _, cx| this.capture_key(PromptKey::Dismiss, cx)),
            )
            // The field propagates `Escape` when it has nothing to dismiss.
            .on_action(cx.listener(|_, _: &Escape, _, cx| cx.emit(PromptInputEvent::Cancel)))
            .child(
                Textarea::new(&self.state)
                    .appearance(false)
                    .token(|token, _, cx| {
                        let t = cx.tokens();
                        let p = &t.palette;
                        div()
                            .flex()
                            .items_center()
                            .h(token.line_height())
                            .max_w(token.available_width())
                            .px(px(6.))
                            .rounded(px(6.))
                            .bg(if token.is_selected() {
                                p.blue.alpha(0.3)
                            } else {
                                p.raised_top.hsla()
                            })
                            .text_color(p.text_muted.hsla())
                            .text_size(t.metrics.text_sm)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(token.token().label().clone())
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui_kit::{
        AnyWindowHandle, Bounds, Keystroke, Point, TestAppContext, WindowBounds, WindowOptions,
        size,
    };

    use super::*;
    use gpui_kit::test::TestWindowExt as _;

    struct Harness {
        cx: TestAppContext,
        window: AnyWindowHandle,
        input: Entity<PromptInput>,
        events: Rc<RefCell<Vec<PromptInputEvent>>>,
        _subscription: Subscription,
    }

    fn open() -> Harness {
        let cx = TestAppContext::single();
        let (window, input) = cx.update(|cx| {
            crate::init(crate::Appearance::Dark, cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(600.), px(300.)),
                })),
                ..Default::default()
            };
            crate::open_window(options, cx, |window, cx| {
                cx.new(|cx| PromptInput::new("Message", 2, 8, window, cx))
            })
            .expect("open window")
        });
        let events: Rc<RefCell<Vec<PromptInputEvent>>> = Rc::default();
        let sink = events.clone();
        let subscription = cx.update(|cx| {
            cx.subscribe(&input, move |_, e: &PromptInputEvent, _| {
                sink.borrow_mut().push(*e)
            })
        });
        let mut h = Harness {
            cx,
            window,
            input,
            events,
            _subscription: subscription,
        };
        let input = h.input.clone();
        h.cx.update_window(h.window, |_, window, cx| {
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        })
        .unwrap();
        h
    }

    impl Harness {
        fn with<R>(
            &mut self,
            f: impl FnOnce(&mut PromptInput, &mut Window, &mut Context<PromptInput>) -> R,
        ) -> R {
            let input = self.input.clone();
            let result = self
                .cx
                .update_window(self.window, |_, window, cx| {
                    input.update(cx, |input, cx| f(input, window, cx))
                })
                .unwrap();
            self.frame();
            result
        }

        fn frame(&mut self) {
            self.cx
                .update_window(self.window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            self.cx.run_until_parked();
        }

        fn press(&mut self, keys: &'static str) {
            self.frame();
            self.cx
                .update_window(self.window, |_, window, cx| {
                    window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
                })
                .unwrap();
            self.frame();
        }

        fn keys(&self) -> Vec<PromptKey> {
            self.events
                .borrow()
                .iter()
                .filter_map(|e| match e {
                    PromptInputEvent::Key(k) => Some(*k),
                    _ => None,
                })
                .collect()
        }

        fn text(&mut self) -> String {
            self.cx.read(|cx| self.input.read(cx).text(cx))
        }
    }

    #[test]
    fn captured_keys_are_reported_and_not_applied() {
        let mut h = open();
        h.with(|i, window, cx| i.set_text("ab", window, cx));
        h.with(|i, _, _| i.set_capture(true));
        for key in ["up", "down", "enter", "tab", "escape"] {
            h.press(key);
        }
        use PromptKey::*;
        assert_eq!(h.keys(), [Up, Down, Accept, Complete, Dismiss]);
        assert!(!h.events.borrow().contains(&PromptInputEvent::Submit));
        assert!(!h.events.borrow().contains(&PromptInputEvent::Cancel));
        assert_eq!(h.text(), "ab");
        // Without capture, Enter submits again.
        h.with(|i, _, _| i.set_capture(false));
        h.press("enter");
        assert!(h.events.borrow().contains(&PromptInputEvent::Submit));
    }

    #[test]
    fn up_in_an_empty_field_and_on_a_recalled_prompt_is_reported() {
        let mut h = open();
        h.press("up");
        assert_eq!(h.keys(), [PromptKey::Up]);
        h.with(|i, window, cx| i.recall("earlier", Vec::new(), window, cx));
        h.events.borrow_mut().clear();
        h.press("down");
        assert_eq!(h.keys(), [PromptKey::Down]);
        // An edit ends the recall: Up moves the cursor again.
        h.with(|i, window, cx| i.replace_range(7..7, "!", window, cx));
        h.events.borrow_mut().clear();
        h.press("up");
        assert_eq!(h.keys(), []);
        assert_eq!(h.text(), "earlier!");
    }

    #[test]
    fn mentions_are_atomic_chips_with_their_id() {
        let mut h = open();
        h.with(|i, window, cx| i.set_text("see @ma", window, cx));
        let mention = Mention {
            id: "file:///p/src/main.rs".into(),
            text: "@src/main.rs".into(),
            label: "@main.rs".into(),
        };
        let m = mention.clone();
        assert!(h.with(|i, window, cx| i.insert_mention(4..7, m, window, cx)));
        let (text, cursor, mentions) = h.cx.read(|cx| {
            let i = h.input.read(cx);
            (i.text(cx), i.cursor(cx), i.mentions(cx))
        });
        assert_eq!(text, "see @src/main.rs ");
        assert_eq!(cursor, text.len());
        assert_eq!(
            mentions,
            [MentionSpan {
                range: 4..16,
                mention: mention.clone(),
            }]
        );
        // Content round-trips with its mentions.
        let spans = mentions.clone();
        h.with(|i, window, cx| i.recall("see @src/main.rs ", spans, window, cx));
        assert_eq!(h.cx.read(|cx| h.input.read(cx).mentions(cx)), mentions);
    }
}
