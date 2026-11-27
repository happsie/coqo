// SPDX-License-Identifier: Apache-2.0

//! What the workbench does with questions: open one in its file
//! ([`OpenQuestion`](super::OpenQuestion), F8 / Shift+F8), show a question an
//! agent starts waiting for in a file tab that is on screen, and open the
//! asking chat.

use coqo_protocol::methods::{
    questions::QuestionId,
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    dock::{AnyTab, Dock},
    gpui::{App, AppContext as _, Entity, Window},
};

use super::{NavFrom, primary_anchor, questions_for, step};
use crate::{
    chat,
    files::editor::{self as file_editor, FileEditor},
    workspace::Workspaces,
};

fn current(workspaces: &Entity<Workspaces>, cx: &App) -> Option<WorkspaceId> {
    workspaces.read(cx).current().map(|w| w.id.clone())
}

/// Open `root`/`path` in a file tab (or show its tab).
pub fn open_file(
    dock: &Entity<Dock>,
    workspace: &WorkspaceId,
    root: RootId,
    path: &str,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<FileEditor>> {
    let key = file_editor::key(workspace, root, path);
    if let Some(id) = dock.read(cx).find_tab(file_editor::KIND, &key, cx) {
        dock.update(cx, |dock, cx| dock.activate_tab(id, window, cx));
        return dock
            .read(cx)
            .tab(id)
            .and_then(AnyTab::downcast::<FileEditor>);
    }
    let (workspace, path) = (workspace.clone(), path.to_owned());
    let tab: Entity<FileEditor> = cx.new(|cx| FileEditor::new(workspace, root, path, window, cx));
    dock.update(cx, |dock, cx| {
        dock.open_tab(AnyTab::new(tab.clone()), window, cx);
    });
    Some(tab)
}

/// Show question `id` in its file, scrolled to and with the popover focused.
pub fn open_question(
    dock: &Entity<Dock>,
    workspaces: &Entity<Workspaces>,
    id: QuestionId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = current(workspaces, cx) else {
        return;
    };
    let Some(store) = questions_for(&workspace, cx) else {
        return;
    };
    let Some((root, path)) = store
        .read(cx)
        .get(id)
        .and_then(primary_anchor)
        .map(|a| (a.anchor.root, a.anchor.path.clone()))
    else {
        eprintln!("coqo: question {id} isn't in this workspace");
        return;
    };
    if let Some(file) = open_file(dock, &workspace, root, &path, window, cx) {
        file.update(cx, |file, cx| {
            file.show_question(id, true, true, window, cx)
        });
    }
}

/// F8 (`forward`) or Shift+F8: the next or previous open question, from the
/// active file tab's open popover or cursor.
pub fn step_question(
    dock: &Entity<Dock>,
    workspaces: &Entity<Workspaces>,
    forward: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = current(workspaces, cx) else {
        return;
    };
    let Some(store) = questions_for(&workspace, cx) else {
        return;
    };
    let items = store.read(cx).navigation();
    let from = {
        let dock = dock.read(cx);
        let file = dock
            .active_tab()
            .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
            .filter(|f| f.read(cx).workspace() == &workspace);
        match file {
            Some(file) => {
                let file = file.read(cx);
                match file.popover_question(cx) {
                    Some(id) => NavFrom::Question(id),
                    None => NavFrom::Place(
                        file.root(),
                        file.path().to_owned(),
                        file.cursor_line(cx).unwrap_or(0),
                    ),
                }
            }
            None => NavFrom::Nowhere,
        }
    };
    if let Some(id) = step(&items, &from, forward) {
        open_question(dock, workspaces, id, window, cx);
    }
}

/// An agent started waiting for question `id`: show its popover in the file
/// tabs on screen that show its code, without taking the keyboard.
pub fn show_waiting(
    dock: &Entity<Dock>,
    workspaces: &Entity<Workspaces>,
    id: QuestionId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = current(workspaces, cx) else {
        return;
    };
    let Some(store) = questions_for(&workspace, cx) else {
        return;
    };
    let Some(q) = store.read(cx).get(id).cloned() else {
        return;
    };
    let shown: Vec<Entity<FileEditor>> = {
        let dock = dock.read(cx);
        dock.tree()
            .groups()
            .into_iter()
            .filter_map(|group| group.active_tab())
            .filter_map(|tab| dock.tab(tab)?.downcast::<FileEditor>())
            .collect()
    };
    for file in shown {
        let (matches, free) = {
            let f = file.read(cx);
            let matches = f.workspace() == &q.workspace
                && q.anchors
                    .iter()
                    .any(|a| a.anchor.root == f.root() && a.anchor.path == f.path());
            (matches, f.popover_question(cx).is_none())
        };
        if matches && free {
            file.update(cx, |file, cx| {
                file.show_question(id, false, false, window, cx)
            });
        }
    }
}

/// Open the chat of the session that asked question `id`.
pub fn open_chat(
    dock: &Entity<Dock>,
    workspaces: &Entity<Workspaces>,
    id: QuestionId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = current(workspaces, cx) else {
        return;
    };
    let session = questions_for(&workspace, cx)
        .and_then(|store| store.read(cx).get(id).map(|q| q.session.clone()));
    if let Some(session) = session {
        chat::open_session(dock.downgrade(), session, window, cx);
    }
}
