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
//! row onto a folder to move it ([`ops`]). `Ctrl Z` / `Cmd Z` undoes the
//! last of those, `Ctrl Shift Z` / `Cmd Shift Z` (also `Ctrl Y` off macOS)
//! redoes it, while the tree itself has focus ([`history`]). The same
//! operations are file commands in the palette, on the selection or a file
//! tab's file ([`actions`]). In a file tab: `Ctrl S`
//! (`Cmd S`) saves, `Ctrl F` finds, `Ctrl Z` / `Ctrl Y` undo and redo this
//! tab's own edits. `Ctrl P` (`Cmd P`) finds a file by name ([`finder`]).
//!
//! Rows carry what other features say about them, such as git's status
//! letters, through [`decorations`]; with `files.auto_reveal` on, the tree
//! follows the active file tab.

pub mod actions;
pub mod context;
pub mod decorations;
pub mod editor;
pub mod finder;
pub mod glyph;
pub mod history;
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
        /// Copy the selected entry next to itself, then rename the copy.
        DuplicateEntry,
        /// Remember the selected file or folder, to paste it elsewhere.
        CopyEntry,
        /// Paste the copied file or folder into the selected folder (or the
        /// selected file's).
        PasteEntry,
        /// Close the context menu, the delete confirmation or an error.
        Dismiss,
        /// Undo the tree's last file operation.
        UndoFileOp,
        /// Redo the file operation undone last.
        RedoFileOp,
        /// Copy the selected entry's absolute path.
        CopyPath,
        /// Copy the selected entry's path relative to its root.
        CopyRelativePath,
        /// Show the selected entry in the OS file manager.
        RevealInFileManager,
        /// Show the active file tab's file in the tree.
        RevealInTree,
        /// Turn revealing the active file tab's file in the tree on or off,
        /// for the running app (`files.auto_reveal` in `settings.toml`).
        ToggleAutoReveal,
    ]
);

/// Where undo and redo of file operations work: the focused tree, never a
/// field in it (its name field has text undo).
const UNDO_CONTEXT: &str = "FileTree && !Input";
#[cfg(target_os = "macos")]
const UNDO_KEYS: &[&str] = &["cmd-z"];
#[cfg(not(target_os = "macos"))]
const UNDO_KEYS: &[&str] = &["ctrl-z"];
#[cfg(target_os = "macos")]
const REDO_KEYS: &[&str] = &["cmd-shift-z"];
#[cfg(not(target_os = "macos"))]
const REDO_KEYS: &[&str] = &["ctrl-shift-z", "ctrl-y"];

/// Register the Files pane, the `file` tab kind and the tree's keys. Its
/// file commands (and their keys, `F2` and `Delete`) are in [`actions`].
pub fn init(cx: &mut App) {
    let context = Some("FileTree");
    cx.bind_keys([
        keys::binding("down", SelectNext, context),
        keys::binding("up", SelectPrev, context),
        keys::binding("right", Expand, context),
        keys::binding("left", Collapse, context),
        keys::binding("enter", Confirm, context),
        keys::binding("escape", Dismiss, context),
    ]);
    use crate::commands::Command;
    let undo = UNDO_KEYS.iter().fold(
        Command::new("files.undo", "Undo file operation", "Files", UndoFileOp),
        |c, k| c.key(*k),
    );
    let redo = REDO_KEYS.iter().fold(
        Command::new("files.redo", "Redo file operation", "Files", RedoFileOp),
        |c, k| c.key(*k),
    );
    crate::commands::register(cx, [undo.context(UNDO_CONTEXT), redo.context(UNDO_CONTEXT)]);
    ops::FileEvents::init(cx);
    actions::init(cx);
    editor::init(cx);
    finder::init(cx);
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
mod actions_tests;
#[cfg(test)]
mod finder_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod tests;
