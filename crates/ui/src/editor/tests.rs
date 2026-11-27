// SPDX-License-Identifier: Apache-2.0

//! Headless tests (GPUI's test platform: no window on screen, no GPU) for
//! remote edits, in the spirit of `spikes/s6-buffers/editor-map`.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::{
    AnyWindowHandle, Bounds, EntityInputHandler as _, Keystroke, Point, TestAppContext,
    WindowBounds, WindowOptions, px, size,
};

use super::*;
use crate::Appearance;
use gpui_kit::test::TestWindowExt as _;

#[cfg(target_os = "macos")]
const UNDO: &str = "cmd-z";
#[cfg(not(target_os = "macos"))]
const UNDO: &str = "ctrl-z";
#[cfg(target_os = "macos")]
const REDO: &str = "cmd-shift-z";
#[cfg(not(target_os = "macos"))]
const REDO: &str = "ctrl-y";

const TEXT: &str = "fn main() {\n    let alpha = 1;\n    let beta = 2;\n    let gamma = 3;\n}\n";

struct Harness {
    cx: TestAppContext,
    window: AnyWindowHandle,
    editor: Entity<CodeEditor>,
    /// Every `EditorEdits` the wrapper emitted.
    events: Rc<RefCell<Vec<EditorEdits>>>,
    /// Every raw `InputEditsEvent` gpui-component emitted, as byte ranges.
    raw: Rc<RefCell<Vec<Range<usize>>>>,
    _subscriptions: Vec<Subscription>,
}

fn open(text: &str) -> Harness {
    let cx = TestAppContext::single();
    let text = text.to_owned();
    let (window, editor) = cx.update(|cx| {
        crate::init(Appearance::Dark, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(800.), px(600.)),
            })),
            ..Default::default()
        };
        crate::open_window(options, cx, |window, cx| {
            cx.new(|cx| CodeEditor::new(text, window, cx))
        })
        .expect("open editor window")
    });
    let events: Rc<RefCell<Vec<EditorEdits>>> = Rc::default();
    let raw: Rc<RefCell<Vec<Range<usize>>>> = Rc::default();
    let subscriptions = cx.update(|cx| {
        let sink = events.clone();
        let wrapper = cx.subscribe(&editor, move |_, event: &EditorEdits, _| {
            sink.borrow_mut().push(event.clone());
        });
        let sink = raw.clone();
        let state = editor.read(cx).state.clone();
        let raw_sub = cx.subscribe(&state, move |_, event: &InputEditsEvent, _| {
            for (edit, _) in event.edits() {
                sink.borrow_mut().push(edit.start_byte..edit.old_end_byte);
            }
        });
        vec![wrapper, raw_sub]
    });
    let mut h = Harness {
        cx,
        window,
        editor,
        events,
        raw,
        _subscriptions: subscriptions,
    };
    h.with(|editor, window, cx| {
        editor.state.update(cx, |state, cx| state.focus(window, cx));
    });
    h
}

impl Harness {
    /// Run `f` against the editor, then render a frame and settle.
    fn with<R>(
        &mut self,
        f: impl FnOnce(&mut CodeEditor, &mut Window, &mut Context<CodeEditor>) -> R,
    ) -> R {
        let editor = self.editor.clone();
        let result = self
            .cx
            .update_window(self.window, |_, window, cx| {
                editor.update(cx, |editor, cx| f(editor, window, cx))
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
        self.cx
            .update_window(self.window, |_, window, cx| {
                window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
            })
            .unwrap();
        self.frame();
    }

    /// Type `text` at every cursor, as a keystroke does.
    fn type_text(&mut self, text: &str) {
        self.with(|editor, window, cx| {
            editor.state.update(cx, |state, cx| {
                state.replace_text_in_range(None, text, window, cx)
            });
        });
    }

    fn text(&mut self) -> String {
        self.cx.read(|cx| self.editor.read(cx).text(cx))
    }

    fn selections(&mut self) -> Vec<Range<usize>> {
        self.cx.read(|cx| self.editor.read(cx).selections(cx))
    }

    fn remote(&mut self, edits: &[(Range<usize>, &str)]) {
        let edits: Vec<TextEdit> = edits
            .iter()
            .map(|(range, text)| (range.clone(), (*text).to_owned()))
            .collect();
        self.with(|editor, window, cx| editor.apply_remote_edits(&edits, window, cx));
    }

    fn take_events(&mut self) -> (Vec<EditorEdits>, Vec<Range<usize>>) {
        (
            std::mem::take(&mut *self.events.borrow_mut()),
            std::mem::take(&mut *self.raw.borrow_mut()),
        )
    }
}

fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} in {text:?}"))
}

fn span(text: &str, needle: &str) -> Range<usize> {
    let start = at(text, needle);
    start..start + needle.len()
}

/// The acceptance test of #145: two cursors and a selection, a remote insert
/// above them, then Ctrl+Z.
#[test]
fn remote_insert_above_keeps_cursors_selection_and_undo() {
    let mut h = open(TEXT);

    // The user types first, so there is local history Ctrl+Z could revert.
    let end = TEXT.len();
    h.with(|editor, _, cx| editor.set_selections(&[end..end], cx));
    h.type_text("// user\n");
    let (events, raw) = h.take_events();
    assert_eq!(raw, vec![end..end], "the local edit reaches gpui-component");
    assert_eq!(
        events,
        vec![EditorEdits {
            origin: EditOrigin::Local,
            edits: vec![(end..end, "// user\n".to_owned())],
        }],
        "local edits are reported as local, in byte ranges"
    );

    // Two cursors and a selection.
    let text = h.text();
    let alpha = at(&text, "alpha");
    let beta = at(&text, "beta");
    let gamma = span(&text, "gamma");
    h.with(|editor, _, cx| editor.set_selections(&[alpha..alpha, beta..beta, gamma.clone()], cx));
    assert_eq!(
        h.selections(),
        vec![alpha..alpha, beta..beta, gamma.clone()]
    );

    // An agent adds a header above all of them.
    let header = "// agent: added a header\n";
    h.remote(&[(0..0, header)]);
    let text = h.text();
    assert!(text.starts_with(header));

    let selections = h.selections();
    assert_eq!(selections.len(), 3, "no cursor is dropped: {selections:?}");
    assert!(
        text[selections[0].start..].starts_with("alpha"),
        "{selections:?}"
    );
    assert!(selections[0].is_empty());
    assert!(
        text[selections[1].start..].starts_with("beta"),
        "{selections:?}"
    );
    assert!(selections[1].is_empty());
    assert_eq!(&text[selections[2].clone()], "gamma");

    // No echo: gpui-component emitted nothing, and the wrapper flagged it remote.
    let (events, raw) = h.take_events();
    assert!(
        raw.is_empty(),
        "remote edits emit no InputEditsEvent: {raw:?}"
    );
    assert_eq!(
        events,
        vec![EditorEdits {
            origin: EditOrigin::Remote,
            edits: vec![(0..0, header.to_owned())],
        }]
    );

    // Ctrl+Z (no Coqo undo installed yet) does not revert the agent's header.
    h.press(UNDO);
    h.press(UNDO);
    let text = h.text();
    assert!(
        text.starts_with(header),
        "undo reverted the remote edit: {text:?}"
    );
    assert!(
        text.ends_with("// user\n"),
        "undo replayed at a stale offset: {text:?}"
    );
    assert_eq!(h.take_events().1, Vec::<Range<usize>>::new());
}

#[test]
fn history_handler_replaces_the_built_in_undo() {
    let mut h = open(TEXT);
    let end = TEXT.len();
    h.with(|editor, _, cx| editor.set_selections(&[end..end], cx));
    h.type_text("x");

    let calls: Rc<RefCell<Vec<HistoryAction>>> = Rc::default();
    let sink = calls.clone();
    h.with(|editor, _, _| {
        editor.set_history_handler(move |_, action, _, _| sink.borrow_mut().push(action))
    });
    h.press(UNDO);
    h.press(REDO);
    assert_eq!(*calls.borrow(), [HistoryAction::Undo, HistoryAction::Redo]);
    assert!(
        h.text().ends_with("}\nx"),
        "the built-in history must not run while a handler is installed"
    );

    // Without the handler, the built-in history is back.
    h.with(|editor, _, _| editor.clear_history_handler());
    h.press(UNDO);
    assert_eq!(h.text(), TEXT);
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn insert_at_a_cursor_goes_after_it_and_selection_end_sticks_right() {
    let mut h = open(TEXT);
    let beta = at(TEXT, "beta");
    let gamma = span(TEXT, "gamma");
    h.with(|editor, _, cx| editor.set_selections(&[beta..beta, gamma.clone()], cx));

    // Others insert exactly at the cursor, at the selection's start and at its end.
    h.remote(&[
        (beta..beta, "AA"),
        (gamma.start..gamma.start, "<"),
        (gamma.end..gamma.end, ">"),
    ]);
    let text = h.text();
    assert!(text.contains("let AAbeta = 2;"));
    assert!(text.contains("let <gamma> = 3;"));

    let selections = h.selections();
    // The cursor stays left of the text inserted at it.
    assert_eq!(selections[0], beta..beta);
    assert!(text[selections[0].start..].starts_with("AAbeta"));
    // The selection's start stays left and its end sticks right.
    assert_eq!(&text[selections[1].clone()], "<gamma>");
}

#[test]
fn remote_edits_below_and_around_cursors() {
    let mut h = open(TEXT);
    let alpha = at(TEXT, "alpha");
    let beta = span(TEXT, "beta");
    let inside_beta = beta.start + 2;
    h.with(|editor, _, cx| editor.set_selections(&[alpha..alpha, inside_beta..inside_beta], cx));

    // Below the cursors: nothing moves. Replacing the word the second cursor is
    // in: that cursor collapses to the start of the replacement.
    h.remote(&[
        (beta.clone(), "delta"),
        (TEXT.len() - 2..TEXT.len() - 2, "// tail\n"),
    ]);
    let text = h.text();
    assert!(text.contains("let delta = 2;"));
    assert!(text.ends_with("// tail\n}\n"));
    assert_eq!(h.selections(), vec![alpha..alpha, beta.start..beta.start]);

    // A deletion before the cursors shifts them left, still on their text.
    h.remote(&[(0..3, "")]);
    let text = h.text();
    let selections = h.selections();
    assert!(text.starts_with("main()"));
    assert!(text[selections[0].start..].starts_with("alpha"));
    assert!(text[selections[1].start..].starts_with("delta"));
}

#[test]
fn remote_edits_apply_to_a_read_only_editor_too() {
    let mut h = open(TEXT);
    h.with(|editor, window, cx| {
        editor
            .state
            .update(cx, |state, cx| state.set_readonly(true, cx));
        editor.apply_remote_edits(&[(0..0, "// ro\n".to_owned())], window, cx);
    });
    assert!(h.text().starts_with("// ro\n"));
}

#[test]
fn disjoint_allows_touching_and_rejects_overlap() {
    let e = |r: Range<usize>| (r, String::new());
    assert!(disjoint(&[e(0..0), e(0..2), e(2..2), e(5..9)]));
    assert!(disjoint(&[e(5..9), e(0..2)]));
    assert!(!disjoint(&[e(0..3), e(2..4)]));
}
