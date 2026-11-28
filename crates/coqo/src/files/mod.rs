// SPDX-License-Identifier: Apache-2.0

//! Files (PLAN §6 `files`): the Files sidebar pane and the `file` tab kind.
//!
//! It lives in the app crate until the mod API carries UI contributions
//! (#147); it already plugs in only through the workbench registries
//! (`SidebarPanes`, `register_tab_kind`), so it moves out unchanged.
//!
//! Keyboard in the tree: Up/Down select, Right expands (or enters), Left
//! collapses (or goes to the parent), Enter opens, `F2` renames in place,
//! `Delete` (`Cmd Backspace` on macOS) deletes after a confirmation, Escape
//! dismisses a menu or a confirmation. Right-click a row for New File, New
//! Folder, Rename, Delete, Copy Path, Copy Relative Path and Reveal; drag a
//! row onto a folder to move it ([`ops`]). In a file tab: `Ctrl S`
//! (`Cmd S`) saves, `Ctrl F` finds, `Ctrl Z` / `Ctrl Y` undo and redo this
//! tab's own edits.

pub mod context;
pub mod editor;
pub mod glyph;
pub mod ops;
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
        /// Open the selected file, or toggle the selected folder (or delete,
        /// while a delete waits for confirmation).
        Confirm,
        /// Create a file next to the selection (or in the selected folder).
        NewFile,
        /// Create a folder next to the selection (or in the selected folder).
        NewFolder,
        /// Rename the selected entry in place.
        RenameEntry,
        /// Delete the selected entry, after a confirmation.
        DeleteEntry,
        /// Close the context menu, the delete confirmation or an error.
        Dismiss,
    ]
);

#[cfg(target_os = "macos")]
const DELETE_KEYS: &[&str] = &["delete", "cmd-backspace"];
#[cfg(not(target_os = "macos"))]
const DELETE_KEYS: &[&str] = &["delete"];

/// Register the Files pane, the `file` tab kind and the tree's keys.
pub fn init(cx: &mut App) {
    let context = Some("FileTree");
    cx.bind_keys([
        keys::binding("down", SelectNext, context),
        keys::binding("up", SelectPrev, context),
        keys::binding("right", Expand, context),
        keys::binding("left", Collapse, context),
        keys::binding("enter", Confirm, context),
        keys::binding("f2", RenameEntry, context),
        keys::binding("escape", Dismiss, context),
    ]);
    cx.bind_keys(
        DELETE_KEYS
            .iter()
            .map(|&stroke| keys::binding(stroke, DeleteEntry, context)),
    );
    ops::FileEvents::init(cx);
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
