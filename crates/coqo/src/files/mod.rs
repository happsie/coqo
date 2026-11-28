// SPDX-License-Identifier: Apache-2.0

//! Files (PLAN §6 `files`): the Files sidebar pane and the `file` tab kind.
//!
//! It lives in the app crate until the mod API carries UI contributions
//! (#147); it already plugs in only through the workbench registries
//! (`SidebarPanes`, `register_tab_kind`), so it moves out unchanged.
//!
//! Keyboard in the tree: Up/Down select, Right expands (or enters), Left
//! collapses (or goes to the parent), Enter opens. In a file tab: `Ctrl S`
//! (`Cmd S`) saves, `Ctrl F` finds, `Ctrl Z` / `Ctrl Y` undo and redo this
//! tab's own edits.

pub mod context;
pub mod editor;
pub mod glyph;
pub mod tree;

use coqo_ui::keys;
use coqo_ui::{
    dock::register_tab_kind,
    gpui::{App, AppContext as _},
    slots::{SidebarPane, SidebarPanes},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    file_tree,
    [
        /// Select the next row.
        SelectNext,
        /// Select the previous row.
        SelectPrev,
        /// Expand the selected folder, or step into it.
        Expand,
        /// Collapse the selected folder, or select its parent.
        Collapse,
        /// Open the selected file, or toggle the selected folder.
        Confirm,
    ]
);

/// Register the Files pane, the `file` tab kind and the tree's keys.
pub fn init(cx: &mut App) {
    let context = Some("FileTree");
    cx.bind_keys([
        keys::binding("down", SelectNext, context),
        keys::binding("up", SelectPrev, context),
        keys::binding("right", Expand, context),
        keys::binding("left", Collapse, context),
        keys::binding("enter", Confirm, context),
    ]);
    editor::init(cx);
    register_tab_kind(cx, editor::KIND, |state, window, cx| {
        editor::restore(state, window, cx)
    });
    SidebarPanes::register(
        cx,
        SidebarPane::new("files", "Files", 0, |dock, window, cx| {
            cx.new(|cx| tree::FileTree::new(dock, window, cx)).into()
        }),
    );
}

#[cfg(test)]
mod tests;
