// SPDX-License-Identifier: Apache-2.0

//! Git (PLAN §7.7, issue #91): the Git sidebar pane and the `git-diff` tab
//! kind, over the daemon's `git/*` methods and `git/changes`.
//!
//! Like Files, it lives in the app crate until the mod API carries UI
//! contributions (#147), and plugs in only through the workbench
//! registries. [`model::GitModel`] holds a window's git state for every
//! view that needs it.

pub mod chrome;
pub mod decorations;
pub mod diff;
pub mod draft;
pub mod model;
pub mod pane;

use coqo_ui::{
    dock::register_tab_kind,
    gpui::{App, AppContext as _},
    slots::{SidebarPane, SidebarPanes, Slot, Slots},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    git,
    [
        /// Show the Git pane and ask an agent to draft the commit message
        /// for what is staged (#224).
        DraftCommitMessage,
    ]
);

/// The Git pane's id in the sidebar.
pub const PANE: &str = "git";

/// Register the Git pane (with its tab's badge), the status bar's branch,
/// the `git-diff` tab kind, the git commands and the file tree's status
/// letters.
pub fn init(cx: &mut App) {
    decorations::init(cx);
    register_tab_kind(cx, diff::KIND, |state, window, cx| {
        diff::restore(state, window, cx)
    });
    SidebarPanes::register(
        cx,
        SidebarPane::new(PANE, "Git", 10, |dock, window, cx| {
            cx.new(|cx| pane::GitPane::new(dock, window, cx)).into()
        })
        .with_badge(chrome::badge),
    );
    Slots::register(cx, Slot::StatusBarStart, "branch", 0, chrome::status_branch);
    crate::commands::register(
        cx,
        [crate::commands::Command::new(
            "git.draft_message",
            "Draft commit message",
            "Git",
            DraftCommitMessage,
        )],
    );
}

#[cfg(test)]
mod draft_tests;
#[cfg(test)]
mod tests;
