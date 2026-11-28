// SPDX-License-Identifier: Apache-2.0

//! Workbench windows (PLAN §4.3 "several clients").
//!
//! Every window is a [`Shell`] with its own workspace and dock; they share
//! the app's one daemon connection and layout store. **New Window**
//! (`Ctrl Alt N`, `Cmd Alt N` on macOS) opens an empty one. Closing the last
//! window quits the UI (the app runs with `QuitMode::LastWindowClosed`);
//! `coqod` keeps running under its idle policy.

use std::rc::Rc;

use coqo_ui::{
    dock::store::LayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Global, Point, Size,
        TitlebarOptions, WindowBounds, WindowOptions, px, size,
    },
};

use crate::shell::{DEFAULT_WORKSPACE, Shell};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    window,
    [
        /// Open another workbench window.
        NewWindow,
    ]
);

#[cfg(target_os = "macos")]
const NEW_WINDOW_KEY: &str = "cmd-alt-n";
#[cfg(not(target_os = "macos"))]
const NEW_WINDOW_KEY: &str = "ctrl-alt-n";

/// How far each new window is moved from the last, so they don't stack
/// exactly.
const CASCADE: f32 = 28.;

struct WindowsConfig {
    store: Rc<dyn LayoutStore>,
    /// Open windows at the origin, not centred and cascaded (tests).
    fixed_bounds: bool,
}

impl Global for WindowsConfig {}

/// Bind **New Window** and remember the layout store windows use. Call once,
/// after [`workspace::init`](crate::workspace::init) and
/// [`shell::init`](crate::shell::init).
pub fn init(store: Rc<dyn LayoutStore>, cx: &mut App) {
    cx.set_global(WindowsConfig {
        store,
        fixed_bounds: false,
    });
    crate::commands::register(
        cx,
        [
            crate::commands::Command::new("window.new", "New window", "Window", NewWindow)
                .key(NEW_WINDOW_KEY),
        ],
    );
    cx.on_action(|_: &NewWindow, cx| {
        if let Err(err) = open(cx) {
            eprintln!("coqo: failed to open a window: {err:#}");
        }
    });
}

/// Like [`init`], for tests: windows open at the origin.
#[cfg(test)]
pub fn init_for_tests(store: Rc<dyn LayoutStore>, cx: &mut App) {
    init(store, cx);
    cx.global_mut::<WindowsConfig>().fixed_bounds = true;
}

/// Open a workbench window. The app's first window opens what
/// [`workspace::init`](crate::workspace::init) was given; later ones start
/// with no workspace.
pub fn open(cx: &mut App) -> gpui::Result<(AnyWindowHandle, Entity<Shell>)> {
    let config = cx.global::<WindowsConfig>();
    let store = config.store.clone();
    let fixed_bounds = config.fixed_bounds;
    let offset = px(CASCADE * cx.windows().len() as f32);
    let window_size = size(px(1440.), px(900.));
    let bounds = if fixed_bounds {
        Bounds {
            origin: Point::default(),
            size: window_size,
        }
    } else {
        let mut bounds = Bounds::centered(None, window_size, cx);
        bounds.origin.x += offset;
        bounds.origin.y += offset;
        bounds
    };
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Coqo".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(Size {
            width: px(900.),
            height: px(560.),
        }),
        app_id: Some("dev.coqo.Coqo".into()),
        ..Default::default()
    };
    coqo_ui::open_window(options, cx, |window, cx| {
        cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
    })
}
