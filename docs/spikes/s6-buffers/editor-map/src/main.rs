//! Spike S6, editor mapping (headless, offscreen: no real window, no screen capture).
//!
//! Question: can a replication layer apply *remote* edits to gpui-component 0.11's
//! `EditorState` without losing the user's cursors, selections and undo history?
//!
//! Each check prints `OK` / `FINDING` lines; run with `cargo run --release -p s6-editor-map`.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::input::{Editor, EditorState, InputEditsEvent, Redo, Undo};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::*;

struct Host {
    editor: Entity<EditorState>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(Editor::new(&self.editor).h_full())
    }
}

/// Byte range -> UTF-16 range, as `EntityInputHandler` wants.
fn utf16(text: &str, r: Range<usize>) -> Range<usize> {
    let u = |b: usize| text[..b].encode_utf16().count();
    u(r.start)..u(r.end)
}

fn main() {
    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        std::sync::Arc::new(gpui_kit::assets::Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(gpui_kit::init);
    let initial = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
    let editor_slot: Rc<RefCell<Option<Entity<EditorState>>>> = Rc::default();
    let slot = editor_slot.clone();
    let handle = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Default::default(),
                        size: size(px(800.), px(600.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    let editor = cx.new(|cx| {
                        EditorState::new(window, cx)
                            .line_number(true)
                            .default_value(initial)
                    });
                    *slot.borrow_mut() = Some(editor.clone());
                    cx.new(|_| Host { editor })
                },
            )
        })
        .expect("offscreen window");
    let handle: AnyWindowHandle = handle.0.into();
    let editor = editor_slot.borrow().clone().unwrap();
    let frame = |cx: &mut HeadlessAppContext| {
        cx.update_window(handle, |_, w, cx| w.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    };
    frame(&mut cx);

    // Record every InputEditsEvent: this is how the replica learns about local edits.
    let events: Rc<RefCell<Vec<(usize, usize, usize)>>> = Rc::default();
    let sink = events.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&editor, move |_, ev: &InputEditsEvent, _| {
            for (e, _) in ev.edits() {
                sink.borrow_mut()
                    .push((e.start_byte, e.old_end_byte, e.new_end_byte));
            }
        })
    });

    // 1. Local edit: focus, put the cursor on line 2 and type through the input handler.
    let cursor_at = initial.find("let a").unwrap();
    cx.update_window(handle, |_, window, cx| {
        editor.update(cx, |s, cx| {
            s.focus(window, cx);
            s.set_selected_range(cursor_at..cursor_at, cx);
            s.replace_text_in_range(None, "X", window, cx); // what a keystroke does
        });
    })
    .unwrap();
    frame(&mut cx);
    let local = events.borrow().clone();
    println!(
        "{} local keystroke emits InputEditsEvent with byte ranges: {:?}",
        if local == vec![(cursor_at, cursor_at, cursor_at + 1)] {
            "OK     "
        } else {
            "FINDING"
        },
        local
    );
    events.borrow_mut().clear();

    // 2. A remote edit above the cursor, through the only public ranged-edit entry point
    //    (EntityInputHandler::replace_text_in_range), with a second cursor present.
    let second = initial.find("let b").unwrap() + 1;
    cx.update_window(handle, |_, _, cx| {
        editor.update(cx, |s, cx| s.add_selection(second..second, cx));
    })
    .unwrap();
    let (before_sel, before_text) = cx.update(|cx| {
        (
            editor.read(cx).selected_range(),
            editor.read(cx).value().to_string(),
        )
    });
    let remote = "// agent: added a header\n";
    cx.update_window(handle, |_, window, cx| {
        editor.update(cx, |s, cx| {
            s.replace_text_in_range(Some(utf16(&before_text, 0..0)), remote, window, cx);
        });
    })
    .unwrap();
    frame(&mut cx);
    let (after_sel, _after_text) = cx.update(|cx| {
        (
            editor.read(cx).selected_range(),
            editor.read(cx).value().to_string(),
        )
    });
    let expected = before_sel.start + remote.len()..before_sel.end + remote.len();
    println!(
        "{} remote edit keeps the primary cursor where the user was: before {:?}, after {:?}, expected {:?}",
        if after_sel == expected {
            "OK     "
        } else {
            "FINDING"
        },
        before_sel,
        after_sel,
        expected
    );
    println!(
        "{} remote edit echoes back as an InputEditsEvent (must be filtered): {:?}",
        if events.borrow().is_empty() {
            "OK     "
        } else {
            "FINDING"
        },
        events.borrow()
    );
    events.borrow_mut().clear();

    // Multi-cursor survival: type once more and count insertions.
    cx.update_window(handle, |_, window, cx| {
        editor.update(cx, |s, cx| s.replace_text_in_range(None, "Y", window, cx));
    })
    .unwrap();
    let ys = cx.update(|cx| editor.read(cx).value().matches('Y').count());
    println!(
        "{} second cursor survives a remote edit (typing 'Y' inserted {ys} time(s), 2 expected)",
        if ys == 2 { "OK     " } else { "FINDING" }
    );

    // 3. Undo after a remote edit: whose edit does Ctrl+Z revert?
    cx.update_window(handle, |_, window, cx| {
        editor.update(cx, |s, cx| s.focus(window, cx));
        window.dispatch_action(Box::new(Undo), cx); // undo 'Y'
        window.dispatch_action(Box::new(Undo), cx); // next: the user's 'X'... or the agent's header?
    })
    .unwrap();
    frame(&mut cx);
    let after_undo = cx.update(|cx| editor.read(cx).value().to_string());
    let header_gone = !after_undo.contains("agent: added a header");
    println!(
        "{} undo is per side: after undoing twice the agent header is {} and the user's X is {}",
        if !header_gone && !after_undo.contains('X') {
            "OK     "
        } else {
            "FINDING"
        },
        if header_gone {
            "GONE (undo reverted the agent's edit)"
        } else {
            "kept"
        },
        if after_undo.contains('X') {
            "still there"
        } else {
            "gone"
        },
    );
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_action(Box::new(Redo), cx)
    })
    .unwrap();

    // 4. Workaround with today's public API: save selection, apply, restore shifted.
    let text = cx.update(|cx| editor.read(cx).value().to_string());
    let sel = cx.update(|cx| editor.read(cx).selected_range());
    let ins = "/* remote */";
    cx.update_window(handle, |_, window, cx| {
        editor.update(cx, |s, cx| {
            s.replace_text_in_range(Some(utf16(&text, 0..0)), ins, window, cx);
            s.set_selected_range(sel.start + ins.len()..sel.end + ins.len(), cx);
        });
    })
    .unwrap();
    let restored = cx.update(|cx| editor.read(cx).selected_range());
    println!(
        "{} save/restore workaround restores the primary selection ({:?}); extra cursors can't be read back (no public getter)",
        if restored == (sel.start + ins.len()..sel.end + ins.len()) {
            "OK     "
        } else {
            "FINDING"
        },
        restored
    );
}
