// SPDX-License-Identifier: Apache-2.0

//! The code editor, wrapped for Coqo's replicated buffers (ADR 0006, ADR 0008).
//!
//! [`CodeEditor`] is gpui-component's editor plus what a shared buffer needs:
//!
//! - **Remote edits:** [`CodeEditor::apply_remote_edits`] applies an edit that
//!   an agent or another window made. The user's cursors and selections stay on
//!   the text they were on, nothing enters the user's undo history, and the edit
//!   is reported as [`EditOrigin::Remote`], never as a local edit.
//! - **Local edits:** every edit the user makes is reported as an [`EditorEdits`]
//!   event with [`EditOrigin::Local`], in UTF-8 byte ranges, ready for the
//!   buffer replica.
//! - **Coqo-owned undo:** [`CodeEditor::set_history_handler`] routes `Undo` and
//!   `Redo` (Ctrl+Z / Ctrl+Y, Cmd+Z / Cmd+Shift+Z) to Coqo's per-side undo. The
//!   actions are caught in the capture phase, before they reach the editor's
//!   own history. Without a handler the built-in history is used; a remote edit
//!   clears it, so it can never revert someone else's edit.
//!
//! Offsets are UTF-8 byte offsets, as in the buffer protocol.

use std::ops::Range;

use gpui_kit::component::input::{Editor, EditorState, InputEditsEvent, Redo, Undo};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, div,
};

/// One edit: a UTF-8 byte range and the text that replaces it.
pub type TextEdit = (Range<usize>, String);

/// Who made an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOrigin {
    /// The user, typing in this editor.
    Local,
    /// Someone else: applied through [`CodeEditor::apply_remote_edits`].
    Remote,
}

/// Emitted after the text changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorEdits {
    pub origin: EditOrigin,
    /// For [`EditOrigin::Local`], the edits in the order they were applied, each
    /// in the coordinates of the text left by the one before it. For
    /// [`EditOrigin::Remote`], the edits exactly as they were passed in (all in
    /// pre-edit coordinates).
    pub edits: Vec<TextEdit>,
}

/// An `Undo` or `Redo` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryAction {
    Undo,
    Redo,
}

/// Coqo's per-side undo. It gets the editor, so it can apply the inverse edit.
pub type HistoryHandler =
    Box<dyn FnMut(&mut CodeEditor, HistoryAction, &mut Window, &mut Context<CodeEditor>)>;

/// A code editor whose text can also be changed by others.
pub struct CodeEditor {
    state: Entity<EditorState>,
    history: Option<HistoryHandler>,
    _edits: Subscription,
}

impl std::fmt::Debug for CodeEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeEditor")
            .field("history_handler", &self.history.is_some())
            .finish_non_exhaustive()
    }
}

impl EventEmitter<EditorEdits> for CodeEditor {}

impl CodeEditor {
    /// An editor with line numbers, showing `text`.
    pub fn new(text: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = text.into();
        let state = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(true)
                .default_value(text)
        });
        let edits = cx.subscribe(&state, |_, _, event: &InputEditsEvent, cx| {
            let edits = event
                .edits()
                .iter()
                .map(|(edit, after)| {
                    let text = after.slice(edit.start_byte..edit.new_end_byte).to_string();
                    (edit.start_byte..edit.old_end_byte, text)
                })
                .collect();
            cx.emit(EditorEdits {
                origin: EditOrigin::Local,
                edits,
            });
        });
        Self {
            state,
            history: None,
            _edits: edits,
        }
    }

    /// The whole text.
    pub fn text(&self, cx: &App) -> String {
        self.state.read(cx).text().to_string()
    }

    /// Every selection as a byte range, the primary one first. An empty range
    /// is a cursor.
    pub fn selections(&self, cx: &App) -> Vec<Range<usize>> {
        self.state.read(cx).selected_ranges()
    }

    /// Replace the selections. The first range is the primary selection; an
    /// empty range is a cursor. Does nothing if `ranges` is empty.
    pub fn set_selections(&mut self, ranges: &[Range<usize>], cx: &mut Context<Self>) {
        let Some((first, rest)) = ranges.split_first() else {
            return;
        };
        self.state.update(cx, |state, cx| {
            state.set_selected_range(first.clone(), cx);
            for range in rest {
                state.add_selection(range.clone(), cx);
            }
        });
    }

    /// Whether an IME composition is in progress. Its text has not been
    /// reported as a local edit yet, so remote edits should wait until it ends.
    pub fn is_composing(&self, cx: &App) -> bool {
        self.state.read(cx).is_composing()
    }

    /// Apply edits made by someone else (an agent, another window).
    ///
    /// `edits` are UTF-8 byte ranges in the coordinates of the current text,
    /// disjoint, applied together like an LSP `TextEdit[]`. Every cursor and
    /// selection is mapped through them: a cursor at an insert point stays left
    /// of the inserted text, and a selection's end sticks right. Nothing is
    /// recorded in the undo history, and the change is reported as
    /// [`EditOrigin::Remote`]. Folds, decorations, diagnostics and highlighting
    /// follow the edits as they do for local edits.
    pub fn apply_remote_edits(
        &mut self,
        edits: &[TextEdit],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if edits.is_empty() {
            return;
        }
        debug_assert!(disjoint(edits), "remote edits must not overlap: {edits:?}");
        self.state
            .update(cx, |state, cx| state.apply_remote_edits(edits, window, cx));
        cx.emit(EditorEdits {
            origin: EditOrigin::Remote,
            edits: edits.to_vec(),
        });
    }

    /// Route `Undo` and `Redo` to `handler` instead of the editor's own history.
    pub fn set_history_handler(
        &mut self,
        handler: impl FnMut(&mut CodeEditor, HistoryAction, &mut Window, &mut Context<CodeEditor>)
        + 'static,
    ) {
        self.history = Some(Box::new(handler));
    }

    /// Go back to the editor's own undo history.
    pub fn clear_history_handler(&mut self) {
        self.history = None;
    }

    fn on_history(&mut self, action: HistoryAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut handler) = self.history.take() else {
            // No handler: let the action reach the editor's own history.
            return;
        };
        cx.stop_propagation();
        handler(self, action, window, cx);
        // The handler may have installed a new one; keep that.
        self.history.get_or_insert(handler);
    }
}

/// Whether the edits' ranges don't overlap (touching inserts are fine).
fn disjoint(edits: &[TextEdit]) -> bool {
    let mut ranges: Vec<&Range<usize>> = edits.iter().map(|(range, _)| range).collect();
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges.windows(2).all(|pair| pair[0].end <= pair[1].start)
}

impl Focusable for CodeEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.read(cx).focus_handle(cx)
    }
}

impl Render for CodeEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .capture_action(cx.listener(|this, _: &Undo, window, cx| {
                this.on_history(HistoryAction::Undo, window, cx)
            }))
            .capture_action(cx.listener(|this, _: &Redo, window, cx| {
                this.on_history(HistoryAction::Redo, window, cx)
            }))
            .child(Editor::new(&self.state).h_full())
    }
}

#[cfg(test)]
mod tests;
