// SPDX-License-Identifier: Apache-2.0

//! The diff facade on GPUI's headless test platform.

use gpui_kit::{
    AnyWindowHandle, Bounds, Context, IntoElement, ParentElement as _, Point, Render,
    TestAppContext, WindowBounds, WindowOptions, div, px, size,
};

use super::*;
use crate::Appearance;

const PATCH: &str = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,9 +1,9 @@
 fn one() {}
 fn two() {}
 fn three() {}
 fn four() {}
-fn five() { let total = 1; }
+fn five() { let total = 2; }
 fn six() {}
 fn seven() {}
 fn eight() {}
 fn nine() {}
";

struct Host {
    viewer: DiffViewer,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(DiffBody::new(&self.viewer).size_full())
    }
}

fn open() -> (TestAppContext, AnyWindowHandle, DiffViewer) {
    let cx = TestAppContext::single();
    let (window, host) = cx.update(|cx| {
        crate::init(Appearance::Dark, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(900.), px(600.)),
            })),
            ..Default::default()
        };
        crate::open_window(options, cx, |_, cx| {
            let viewer = DiffViewer::new(DiffMode::Split, cx);
            cx.new(|_| Host { viewer })
        })
        .expect("open window")
    });
    let viewer = cx.read(|cx| host.read(cx).viewer.clone());
    (cx, window, viewer)
}

fn frame(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
}

#[test]
fn modes_round_trip_through_their_names() {
    for mode in [DiffMode::Split, DiffMode::Unified] {
        assert_eq!(DiffMode::parse(mode.as_str()), Some(mode));
        assert_eq!(mode.toggled().toggled(), mode);
    }
    assert_eq!(DiffMode::parse("sideways"), None);
}

#[test]
fn a_patch_shows_and_folds_its_unchanged_lines() {
    let (mut cx, window, viewer) = open();
    cx.update(|cx| {
        viewer
            .set_patches(
                &[Patch {
                    text: PATCH.into(),
                    language: None,
                }],
                cx,
            )
            .unwrap()
    });
    frame(&mut cx, window);
    cx.read(|cx| {
        assert_eq!(viewer.paths(cx), [SharedString::from("src/lib.rs")]);
        assert_eq!(viewer.mode(cx), DiffMode::Split);
        // Lines 1 and 9 are folded away, but still in the patch.
        assert!(viewer.has_line(&LineRef::new("src/lib.rs", Side::Old, 1), cx));
        assert!(viewer.has_line(&LineRef::new("src/lib.rs", Side::New, 9), cx));
        assert!(!viewer.has_line(&LineRef::new("src/lib.rs", Side::New, 10), cx));
    });

    cx.update(|cx| viewer.set_mode(DiffMode::Unified, cx));
    frame(&mut cx, window);
    assert_eq!(cx.read(|cx| viewer.mode(cx)), DiffMode::Unified);
}

#[test]
fn selections_use_coqo_line_addresses() {
    let (mut cx, window, viewer) = open();
    cx.update(|cx| {
        viewer
            .set_patches(
                &[Patch {
                    text: PATCH.into(),
                    language: Some("rust".into()),
                }],
                cx,
            )
            .unwrap()
    });
    frame(&mut cx, window);
    let range = LineRange {
        path: "src/lib.rs".into(),
        start: (Side::Old, 5),
        end: (Side::New, 5),
    };
    cx.update(|cx| viewer.set_selection(Some(&range), cx));
    assert_eq!(cx.read(|cx| viewer.selection(cx)), Some(range.clone()));
    assert_eq!(range.first(), LineRef::new("src/lib.rs", Side::Old, 5));
    assert_eq!(range.last(), LineRef::new("src/lib.rs", Side::New, 5));
    cx.update(|cx| viewer.set_selection(None, cx));
    assert_eq!(cx.read(|cx| viewer.selection(cx)), None);
}

#[test]
fn a_malformed_patch_is_an_error() {
    let (cx, _, viewer) = open();
    let err = cx.update(|cx| {
        viewer.set_patches(
            &[Patch {
                text: "--- a/x\n+++ b/x\n@@ nonsense @@\n".into(),
                language: None,
            }],
            cx,
        )
    });
    assert!(err.is_err());
}

#[test]
fn word_level_emphasis_is_on() {
    let (cx, _, viewer) = open();
    cx.read(|cx| {
        assert_eq!(
            viewer.state.read(cx).inline_unit(),
            Some(DiffInlineUnit::Word)
        );
        assert_eq!(viewer.state.read(cx).context_lines(), Some(CONTEXT_LINES));
    });
}
