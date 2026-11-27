// SPDX-License-Identifier: Apache-2.0

//! Git (PLAN §7.7, issue #91): the Git sidebar pane and the `git-diff` tab
//! kind, over the daemon's `git/*` methods and `git/changes`.
//!
//! Like Files, it lives in the app crate until the mod API carries UI
//! contributions (#147), and plugs in only through the workbench
//! registries. [`model::GitModel`] holds a window's git state for every
//! view that needs it.

pub mod diff;
pub mod model;
pub mod pane;

use coqo_ui::{
    dock::register_tab_kind,
    gpui::{App, AppContext as _},
    slots::{SidebarPane, SidebarPanes},
};

/// Register the Git pane and the `git-diff` tab kind.
pub fn init(cx: &mut App) {
    register_tab_kind(cx, diff::KIND, |state, window, cx| {
        diff::restore(state, window, cx)
    });
    SidebarPanes::register(
        cx,
        SidebarPane::new("git", "Git", 10, |dock, window, cx| {
            cx.new(|cx| pane::GitPane::new(dock, window, cx)).into()
        }),
    );
}

#[cfg(test)]
mod tests;
