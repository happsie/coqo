// SPDX-License-Identifier: Apache-2.0

//! The file commands in the command palette (issue #287): New file, New
//! folder, Rename, Delete, Duplicate, Copy and Paste (#290), Copy path, Copy relative path, Reveal in the file
//! manager, Reveal active file in tree, and Toggle auto-reveal (the tree
//! then follows the active file tab, `files.auto_reveal`).
//!
//! With the focus in the tree they act on its selection: the tree handles
//! the actions itself, as its context menu and keys do. From a file tab
//! they act on that tab's file: the workbench handles them
//! ([`on_active_file`]), shows the Files pane and the file in it, and runs
//! the same tree operation there (a rename in the file's row, a delete
//! confirmation above the rows, a new entry in the file's folder).

use std::path::PathBuf;

use coqo_protocol::methods::{
    fs::EntryKind,
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::gpui::{
    App, ClipboardItem, Context, Div, Entity, Focusable as _, InteractiveElement as _, Stateful,
    Window,
};

use super::{
    CopyEntry, CopyPath, CopyRelativePath, DeleteEntry, DuplicateEntry, NewFile, NewFolder,
    PasteEntry, RenameEntry, RevealInFileManager, RevealInTree, ToggleAutoReveal,
    editor::FileEditor,
    tree::{FileTree, REVEAL_LABEL, Target},
};
use crate::{
    commands::{self, Command},
    shell::Shell,
    workspace::Workspaces,
};

/// The tree's key context.
const TREE: &str = "FileTree";
/// A file tab's key context.
const EDITOR: &str = "FileEditor";
/// Either.
const TREE_OR_EDITOR: &str = "FileTree || FileEditor";
/// The palette group.
const CATEGORY: &str = "File";

#[cfg(target_os = "macos")]
const DELETE_KEYS: &[&str] = &["delete", "cmd-backspace"];
#[cfg(not(target_os = "macos"))]
const DELETE_KEYS: &[&str] = &["delete"];

/// Register the file commands. Rename and Delete are two commands each:
/// the tree's carries the plain key (`F2`, `Delete`), which a file tab must
/// keep for its text.
pub fn init(cx: &mut App) {
    let delete = DELETE_KEYS.iter().fold(
        Command::new("files.delete", "Delete…", CATEGORY, DeleteEntry).context(TREE),
        |command, &keys| command.key(keys),
    );
    commands::register(
        cx,
        [
            Command::new("files.new_file", "New file…", CATEGORY, NewFile).context(TREE_OR_EDITOR),
            Command::new("files.new_folder", "New folder…", CATEGORY, NewFolder)
                .context(TREE_OR_EDITOR),
            Command::new("files.rename", "Rename…", CATEGORY, RenameEntry)
                .key("f2")
                .context(TREE),
            Command::new("files.rename_active", "Rename…", CATEGORY, RenameEntry).context(EDITOR),
            delete,
            Command::new("files.delete_active", "Delete…", CATEGORY, DeleteEntry).context(EDITOR),
            Command::new("files.duplicate", "Duplicate", CATEGORY, DuplicateEntry)
                .context(TREE_OR_EDITOR),
            Command::new("files.copy", "Copy file or folder", CATEGORY, CopyEntry)
                .context(TREE_OR_EDITOR),
            Command::new("files.paste", "Paste file or folder", CATEGORY, PasteEntry)
                .context(TREE_OR_EDITOR),
            Command::new("files.copy_path", "Copy path", CATEGORY, CopyPath)
                .context(TREE_OR_EDITOR),
            Command::new(
                "files.copy_relative_path",
                "Copy relative path",
                CATEGORY,
                CopyRelativePath,
            )
            .context(TREE_OR_EDITOR),
            Command::new("files.reveal", REVEAL_LABEL, CATEGORY, RevealInFileManager)
                .context(TREE_OR_EDITOR),
            Command::new(
                "files.reveal_in_tree",
                "Reveal active file in tree",
                CATEGORY,
                RevealInTree,
            )
            .context(EDITOR),
            Command::new(
                "files.toggle_auto_reveal",
                "Toggle auto-reveal active file in tree",
                CATEGORY,
                ToggleAutoReveal,
            ),
        ],
    );
    cx.on_action(|_: &ToggleAutoReveal, cx| {
        crate::settings::toggle(cx, crate::settings::AUTO_REVEAL);
    });
}

/// What a file command does to the active file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    NewFile,
    NewFolder,
    Rename,
    Delete,
    Duplicate,
    Copy,
    Paste,
    CopyPath { relative: bool },
    Reveal,
    RevealInTree,
}

/// Handle the file commands on the workbench `el`: they act on the active
/// file tab's file. (In the tree, the tree handles them first.)
pub fn on_active_file(el: Stateful<Div>, cx: &mut Context<Shell>) -> Stateful<Div> {
    el.on_action(cx.listener(|s, _: &NewFile, w, cx| run(s, Op::NewFile, w, cx)))
        .on_action(cx.listener(|s, _: &NewFolder, w, cx| run(s, Op::NewFolder, w, cx)))
        .on_action(cx.listener(|s, _: &RenameEntry, w, cx| run(s, Op::Rename, w, cx)))
        .on_action(cx.listener(|s, _: &DeleteEntry, w, cx| run(s, Op::Delete, w, cx)))
        .on_action(cx.listener(|s, _: &DuplicateEntry, w, cx| run(s, Op::Duplicate, w, cx)))
        .on_action(cx.listener(|s, _: &CopyEntry, w, cx| run(s, Op::Copy, w, cx)))
        .on_action(cx.listener(|s, _: &PasteEntry, w, cx| run(s, Op::Paste, w, cx)))
        .on_action(
            cx.listener(|s, _: &CopyPath, w, cx| run(s, Op::CopyPath { relative: false }, w, cx)),
        )
        .on_action(cx.listener(|s, _: &CopyRelativePath, w, cx| {
            run(s, Op::CopyPath { relative: true }, w, cx)
        }))
        .on_action(cx.listener(|s, _: &RevealInFileManager, w, cx| run(s, Op::Reveal, w, cx)))
        .on_action(cx.listener(|s, _: &RevealInTree, w, cx| run(s, Op::RevealInTree, w, cx)))
}

/// The active tab's file, if it is a file tab.
fn active_file(shell: &Shell, cx: &App) -> Option<(WorkspaceId, RootId, String)> {
    let dock = shell.dock().read(cx);
    let tab = dock.tab(dock.active_tab()?)?.downcast::<FileEditor>()?;
    let tab = tab.read(cx);
    Some((tab.workspace().clone(), tab.root(), tab.path().to_owned()))
}

/// The absolute path of `path` under `root`, if `workspace` is the
/// window's.
fn absolute(
    workspace: &WorkspaceId,
    root: RootId,
    path: &str,
    window: &Window,
    cx: &App,
) -> Option<PathBuf> {
    let workspaces = Workspaces::for_window(window, cx)?;
    let info = workspaces.read(cx).current()?;
    if &info.id != workspace {
        return None;
    }
    let root = info.roots.iter().find(|r| r.id == root)?;
    let mut abs = PathBuf::from(&root.path);
    abs.extend(path.split('/').filter(|c| !c.is_empty()));
    Some(abs)
}

/// The window's file tree, its pane shown.
fn show_tree(
    shell: &mut Shell,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Option<Entity<FileTree>> {
    shell
        .show_pane("files", window, cx)?
        .downcast::<FileTree>()
        .ok()
}

fn run(shell: &mut Shell, op: Op, window: &mut Window, cx: &mut Context<Shell>) {
    let Some((workspace, root, path)) = active_file(shell, cx) else {
        return;
    };
    match op {
        Op::CopyPath { relative: true } => {
            cx.write_to_clipboard(ClipboardItem::new_string(path));
        }
        Op::CopyPath { relative: false } => {
            if let Some(abs) = absolute(&workspace, root, &path, window, cx) {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    abs.to_string_lossy().into_owned(),
                ));
            }
        }
        Op::Reveal => {
            if let Some(abs) = absolute(&workspace, root, &path, window, cx) {
                cx.reveal_path(&abs);
            }
        }
        Op::NewFile
        | Op::NewFolder
        | Op::Rename
        | Op::Delete
        | Op::Duplicate
        | Op::Copy
        | Op::Paste
        | Op::RevealInTree => {
            // The tree shows the window's workspace: a tab of another one
            // (left from before a switch) has no rows there.
            if absolute(&workspace, root, &path, window, cx).is_none() {
                return;
            }
            let Some(tree) = show_tree(shell, window, cx) else {
                return;
            };
            tree.update(cx, |tree, cx| {
                tree.reveal_entry(root, &path, cx);
                match op {
                    Op::NewFile | Op::NewFolder => {
                        let kind = if op == Op::NewFile {
                            EntryKind::File
                        } else {
                            EntryKind::Dir
                        };
                        let folder = path.rsplit_once('/').map_or("", |(dir, _)| dir);
                        tree.start_create((root, folder.to_owned()), kind, window, cx);
                    }
                    Op::Rename => tree.start_rename(root, &path, window, cx),
                    Op::Duplicate | Op::Copy => {
                        let target = Target {
                            root,
                            path,
                            kind: EntryKind::File,
                        };
                        if op == Op::Copy {
                            tree.copy_entry(target, cx);
                        } else {
                            tree.duplicate(target, window, cx);
                        }
                    }
                    Op::Paste => {
                        let folder = path.rsplit_once('/').map_or("", |(dir, _)| dir);
                        tree.paste((root, folder.to_owned()), window, cx);
                    }
                    Op::Delete => {
                        let target = Target {
                            root,
                            path,
                            kind: EntryKind::File,
                        };
                        tree.request_delete(target, window, cx);
                    }
                    _ => {
                        let focus = tree.focus_handle(cx);
                        window.focus(&focus, cx);
                    }
                }
            });
        }
    }
}
