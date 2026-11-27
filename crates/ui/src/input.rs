// SPDX-License-Identifier: Apache-2.0

//! Text inputs, wrapped so views never touch gpui-component's input directly.
//!
//! [`PromptInput`] is the multi-line field of a chat composer: it grows with
//! its text, `Enter` submits, `Shift+Enter` inserts a newline, and `Escape`
//! (when the field has nothing of its own to dismiss) asks to cancel.

use gpui_kit::component::input::{Escape, InputEvent, Textarea, TextareaState};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, div,
};

/// Emitted by a [`PromptInput`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptInputEvent {
    /// `Enter` (or `Ctrl/Cmd+Enter`) was pressed: send the text.
    Submit,
    /// `Escape` was pressed and the field had nothing to dismiss.
    Cancel,
    /// The text changed.
    Changed,
}

/// A growing multi-line text field for prompts.
pub struct PromptInput {
    state: Entity<TextareaState>,
    _events: Subscription,
}

impl std::fmt::Debug for PromptInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptInput").finish_non_exhaustive()
    }
}

impl EventEmitter<PromptInputEvent> for PromptInput {}

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
        let events = cx.subscribe(&state, |_, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { shift: false, .. } => cx.emit(PromptInputEvent::Submit),
            InputEvent::Change => cx.emit(PromptInputEvent::Changed),
            _ => {}
        });
        Self {
            state,
            _events: events,
        }
    }

    /// The text.
    pub fn text(&self, cx: &App) -> String {
        self.state.read(cx).value().to_string()
    }

    /// Replace the text (not recorded in the undo history).
    pub fn set_text(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.state
            .update(cx, |state, cx| state.set_value(text, window, cx));
        cx.emit(PromptInputEvent::Changed);
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
            // The field propagates `Escape` when it has nothing to dismiss.
            .on_action(cx.listener(|_, _: &Escape, _, cx| cx.emit(PromptInputEvent::Cancel)))
            .child(Textarea::new(&self.state).appearance(false))
    }
}
