// SPDX-License-Identifier: Apache-2.0

//! Terminal tabs (issue #46). The PTY and the emulator live in `coqod`, so a
//! terminal outlives the window: quitting Coqo keeps it running, and the
//! saved layout reattaches to it on the next start. Closing the tab ends it.
//!
//! - [`backend`]: where terminals come from ([`DaemonBackend`] in the app).
//! - [`view`]: the tab ([`TerminalView`]).
//! - [`keys`]: keystrokes to terminal bytes.
//!
//! Keys: new terminal `` Ctrl ` ``; copy `Ctrl Shift C` / paste `Ctrl Shift V`
//! (`Cmd C` / `Cmd V` on macOS). In a terminal, `Ctrl W` and `Ctrl \` go to
//! the program, not the dock.

pub mod backend;
pub mod keys;
pub mod view;

#[cfg(test)]
mod tests;

use coqo_ui::keys::binding;
use std::rc::Rc;

use coqo_protocol::methods::terminal::{SpawnParams, TerminalId};
use coqo_ui::{
    dock::{AnyTab, Dock, register_tab_kind},
    gpui::{App, AppContext as _, Entity, Global, Window},
};

pub use backend::{DaemonBackend, TerminalBackend};
pub use view::{OpenViews, TerminalView};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

/// The tab kind, as saved in layouts.
pub const KIND: &str = "terminal";

coqo_ui::gpui::actions!(
    terminal,
    [
        /// Open a terminal in the active group.
        NewTerminal,
        /// Copy the selection.
        CopyText,
        /// Paste the clipboard into the terminal.
        PasteText,
    ]
);

#[cfg(target_os = "macos")]
pub(crate) const COPY_PASTE: [&str; 2] = ["cmd-c", "cmd-v"];
#[cfg(not(target_os = "macos"))]
pub(crate) const COPY_PASTE: [&str; 2] = ["ctrl-shift-c", "ctrl-shift-v"];

/// The shortcut that opens a terminal, as shown to the user.
pub const NEW_TERMINAL_KEYS: &str = "Ctrl `";

struct Terminals {
    backend: Rc<dyn TerminalBackend>,
    views: OpenViews,
}

impl Global for Terminals {}

/// Enable terminal tabs, backed by `backend`. Without this, saved terminal
/// tabs are kept as placeholders and `NewTerminal` does nothing.
pub fn init(backend: Rc<dyn TerminalBackend>, cx: &mut App) {
    cx.set_global(Terminals {
        backend,
        views: OpenViews::default(),
    });
    register_tab_kind(cx, KIND, |state, _, cx| {
        let id = state.get("id")?.as_u64()?;
        tab(TerminalId(id), cx)
    });
    let context = Some("Terminal");
    let mut bindings = vec![
        binding("ctrl-`", NewTerminal, None),
        binding(COPY_PASTE[0], CopyText, context),
        binding(COPY_PASTE[1], PasteText, context),
        // Not focus navigation: completion and vi need them.
        binding("tab", gpui::NoAction, context),
        binding("shift-tab", gpui::NoAction, context),
        binding("escape", gpui::NoAction, context),
    ];
    if cfg!(not(target_os = "macos")) {
        // These are the dock's on Linux, but a shell needs them (delete
        // word, SIGQUIT).
        bindings.push(binding("ctrl-w", gpui::NoAction, context));
        bindings.push(binding("ctrl-\\", gpui::NoAction, context));
    }
    cx.bind_keys(bindings);
}

/// A tab showing terminal `id`, or `None` if terminals aren't enabled.
pub fn tab(id: TerminalId, cx: &mut App) -> Option<AnyTab> {
    let t = cx.try_global::<Terminals>()?;
    let (backend, views) = (Rc::clone(&t.backend), Rc::clone(&t.views));
    Some(AnyTab::new(
        cx.new(|cx| TerminalView::new(id, backend, views, cx)),
    ))
}

/// Start a login shell and open it in the active group of `dock`.
pub fn open_new(dock: &Entity<Dock>, window: &mut Window, cx: &mut App) {
    open_with(dock, SpawnParams::default(), window, cx);
}

/// Start a terminal with `params` (a program such as `claude`, say) and
/// open it in the active group of `dock`.
pub fn open_with(dock: &Entity<Dock>, params: SpawnParams, window: &mut Window, cx: &mut App) {
    let Some(t) = cx.try_global::<Terminals>() else {
        return;
    };
    let started = t.backend.spawn(params);
    let dock = dock.downgrade();
    window
        .spawn(cx, async move |cx| {
            let info = match started.await {
                Ok(Ok(info)) => info,
                Ok(Err(error)) => {
                    eprintln!("coqo: couldn't start a terminal: {error}");
                    return;
                }
                Err(_) => return,
            };
            cx.update(|window, cx| {
                if let Some(tab) = tab(info.id, cx) {
                    dock.update(cx, |dock, cx| dock.open_tab(tab, window, cx))
                        .ok();
                }
            })
            .ok();
        })
        .detach();
}
