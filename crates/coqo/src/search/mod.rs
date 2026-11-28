// SPDX-License-Identifier: Apache-2.0

//! Project search (PLAN §6 `search`, issue #45): the Search sidebar pane over
//! the daemon's `search/run` and `search/replace`.
//!
//! Like Files and Git, it lives in the app crate until the mod API carries
//! UI contributions (#147), and plugs in only through the workbench
//! registries. The visual language is in `docs/design/search.md`.
//!
//! Keys: `Ctrl/Cmd+Shift+F` shows the pane with the query focused (and the
//! editor's selection as the query); `F4` / `Shift+F4` open the next and
//! previous match. In the pane, `Alt+C`, `Alt+W` and `Alt+R` toggle match
//! case, whole word and regular expression.

pub mod pane;

use coqo_ui::{
    gpui::{App, AppContext as _},
    slots::{SidebarPane, SidebarPanes},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    search,
    [
        /// Show the Search pane with the query focused.
        ShowSearch,
        /// Open the next match (F4).
        NextMatch,
        /// Open the previous match (Shift+F4).
        PreviousMatch,
        /// Replace every match, after a confirmation.
        ReplaceAll,
        /// Toggle match case.
        ToggleCase,
        /// Toggle whole word.
        ToggleWord,
        /// Toggle regular expression.
        ToggleRegex,
    ]
);

/// The sidebar pane's id.
pub const PANE: &str = "search";
/// The pane's key context.
pub const KEY_CONTEXT: &str = "SearchPane";

#[cfg(target_os = "macos")]
const SHOW_KEY: &str = "cmd-shift-f";
#[cfg(not(target_os = "macos"))]
const SHOW_KEY: &str = "ctrl-shift-f";

/// Register the Search pane and its commands.
pub fn init(cx: &mut App) {
    use crate::commands::Command;
    crate::commands::register(
        cx,
        [
            Command::new("search.show", "Find in files", "Search", ShowSearch).key(SHOW_KEY),
            Command::new("search.next_match", "Next match", "Search", NextMatch).key("f4"),
            Command::new(
                "search.previous_match",
                "Previous match",
                "Search",
                PreviousMatch,
            )
            .key("shift-f4"),
            Command::new("search.replace_all", "Replace all", "Search", ReplaceAll)
                .context(KEY_CONTEXT),
            Command::new(
                "search.toggle_case",
                "Toggle match case",
                "Search",
                ToggleCase,
            )
            .key("alt-c")
            .context(KEY_CONTEXT),
            Command::new(
                "search.toggle_word",
                "Toggle whole word",
                "Search",
                ToggleWord,
            )
            .key("alt-w")
            .context(KEY_CONTEXT),
            Command::new(
                "search.toggle_regex",
                "Toggle regular expression",
                "Search",
                ToggleRegex,
            )
            .key("alt-r")
            .context(KEY_CONTEXT),
        ],
    );
    // The code editor binds `Cmd+Shift+F` to its own replace on macOS; in
    // Coqo it finds in files everywhere, as in other editors.
    #[cfg(target_os = "macos")]
    cx.bind_keys([coqo_ui::keys::binding(SHOW_KEY, ShowSearch, Some("Input"))]);
    SidebarPanes::register(
        cx,
        SidebarPane::new(PANE, "Search", 2, |dock, window, cx| {
            cx.new(|cx| pane::SearchPane::new(dock, window, cx)).into()
        }),
    );
}

#[cfg(test)]
mod tests;
