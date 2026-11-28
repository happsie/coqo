// SPDX-License-Identifier: Apache-2.0

//! The file commands run from the command palette (#287): on the tree's
//! selection, and on a file tab's file.

use coqo_protocol::methods::{fs::EntryKind, workspace::RootId};
use coqo_ui::{
    dock::TabItem as _,
    gpui::{Entity, Focusable as _},
};

use super::{
    finder::FIND_FILE_KEY,
    finder_tests::{Ui, open_ui},
    tree::{EditKind, FileTree},
};
use crate::{
    test_support::{Fixture, write},
    workspace,
};

#[cfg(target_os = "macos")]
const COMMANDS_KEY: &str = "cmd-shift-p";
#[cfg(not(target_os = "macos"))]
const COMMANDS_KEY: &str = "ctrl-shift-p";

impl Ui {
    fn tree(&mut self) -> Entity<FileTree> {
        self.settle();
        self.cx
            .read(|cx| self.shell.read(cx).sidebar_pane("files"))
            .and_then(|view| view.downcast::<FileTree>().ok())
            .expect("the Files pane")
    }

    fn root(&mut self) -> RootId {
        self.cx.read(|cx| {
            workspace::Workspaces::global(cx)
                .and_then(|w| w.read(cx).current().map(|info| info.roots[0].id))
                .expect("a workspace")
        })
    }

    fn clipboard(&mut self) -> Option<String> {
        self.cx.read_from_clipboard().and_then(|c| c.text())
    }

    /// Run the command titled `title` from the palette, found by typing
    /// `query`. It's the only one with that title where the focus is.
    fn run_command(&mut self, query: &str, title: &str) {
        self.press(COMMANDS_KEY);
        assert!(self.palette_open());
        self.type_text(query);
        self.wait_for(&format!("{title} in the palette"), |ui| {
            ui.titles().first().map(String::as_str) == Some(title)
        });
        let titles = self.titles();
        assert_eq!(
            titles.iter().filter(|t| *t == title).count(),
            1,
            "{titles:?}"
        );
        self.press("enter");
        assert!(!self.palette_open());
    }

    fn tree_focused(&mut self, tree: &Entity<FileTree>) -> bool {
        self.frame(|window, cx| tree.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Give the keyboard to the active file tab.
    fn focus_file_tab(&mut self) {
        let tab = self.active_file().expect("a file tab");
        self.frame(|window, cx| {
            let focus = tab.read(cx).focus_handle(cx).unwrap();
            window.focus(&focus, cx);
        });
        self.settle();
    }

    /// Type `name` in the tree's name field and press Enter.
    fn enter_name(&mut self, tree: &Entity<FileTree>, name: &str) {
        let input = self
            .cx
            .read(|cx| tree.read(cx).name_field().map(|(_, input)| input.clone()))
            .expect("a name field");
        let name = name.to_owned();
        self.frame(|window, cx| input.update(cx, |input, cx| input.set_text(name, window, cx)));
        self.press("enter");
    }
}

#[test]
fn file_commands_act_on_the_tree_selection() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture);
    let (tree, root) = (ui.tree(), ui.root());
    ui.wait_for("the rows", |ui| {
        ui.cx
            .read(|cx| tree.read(cx).rows().iter().any(|r| r.path == "README.md"))
    });
    // Select README.md, with the keyboard in the tree.
    ui.frame(|window, cx| {
        tree.update(cx, |tree, cx| {
            assert!(tree.reveal_entry(root, "README.md", cx));
            window.focus(&tree.focus_handle(cx), cx);
        })
    });
    ui.settle();
    assert!(ui.tree_focused(&tree));

    ui.run_command("copy relative", "Copy relative path");
    assert_eq!(ui.clipboard().as_deref(), Some("README.md"));

    // Rename… shows its key, F2, and names the entry in its row.
    ui.press(COMMANDS_KEY);
    ui.type_text("rename");
    ui.wait_for("Rename…", |ui| {
        ui.items().first().is_some_and(|i| i.title == "Rename…")
    });
    assert_eq!(ui.items()[0].keys.as_deref(), Some("F2"));
    ui.press("escape");
    ui.frame(|window, cx| window.focus(&tree.read(cx).focus_handle(cx), cx));
    ui.run_command("rename", "Rename…");
    let editing = ui
        .cx
        .read(|cx| tree.read(cx).name_field().map(|(k, _)| k.clone()));
    assert_eq!(
        editing,
        Some(EditKind::Rename {
            root,
            path: "README.md".into()
        })
    );
    ui.enter_name(&tree, "GUIDE.md");
    ui.wait_for("the rename", |_| fixture.project.join("GUIDE.md").exists());
    assert!(!fixture.project.join("README.md").exists());
}

#[test]
fn file_commands_act_on_the_active_file() {
    let fixture = Fixture::new();
    write(&fixture.project, "src/deep/nested.rs", "// nested\n");
    let mut ui = open_ui(&fixture);
    let (tree, root) = (ui.tree(), ui.root());

    // Open a file the tree hasn't listed the folder of.
    ui.press(FIND_FILE_KEY);
    ui.type_text("nested.rs");
    ui.wait_for("nested.rs", |ui| {
        ui.items().first().is_some_and(|i| i.title == "nested.rs")
    });
    ui.press("enter");
    ui.wait_for("the file tab", |ui| {
        ui.active_path().as_deref() == Some("src/deep/nested.rs")
    });
    ui.focus_file_tab();

    // Reveal active file in tree: its folders expand, it's selected, and the
    // keyboard goes to the tree.
    ui.run_command("reveal active", "Reveal active file in tree");
    ui.wait_for("the file in the tree", |ui| {
        ui.cx
            .read(|cx| tree.read(cx).selected() == Some((root, "src/deep/nested.rs")))
    });
    assert!(ui.tree_focused(&tree));

    // Copy path: the file's absolute path.
    ui.focus_file_tab();
    ui.run_command("copy path", "Copy path");
    let copied = ui.clipboard().expect("a path");
    assert_eq!(
        std::fs::canonicalize(copied).unwrap(),
        std::fs::canonicalize(fixture.project.join("src/deep/nested.rs")).unwrap()
    );

    // Delete… asks in the tree, about this file.
    ui.focus_file_tab();
    ui.run_command("delete", "Delete…");
    let pending = ui.cx.read(|cx| {
        tree.read(cx)
            .pending_delete()
            .map(|p| (p.target.path.clone(), p.target.kind))
    });
    assert_eq!(
        pending,
        Some(("src/deep/nested.rs".into(), EntryKind::File))
    );
    assert!(ui.tree_focused(&tree));
    ui.press("escape");
    assert!(ui.cx.read(|cx| tree.read(cx).pending_delete().is_none()));
    assert!(fixture.project.join("src/deep/nested.rs").exists());

    // New file…: in the file's folder.
    ui.focus_file_tab();
    ui.run_command("new file", "New file…");
    let editing = ui
        .cx
        .read(|cx| tree.read(cx).name_field().map(|(k, _)| k.clone()));
    assert_eq!(
        editing,
        Some(EditKind::Create {
            root,
            dir: "src/deep".into(),
            kind: EntryKind::File
        })
    );
    ui.enter_name(&tree, "sibling.rs");
    ui.wait_for("the new file", |_| {
        fixture.project.join("src/deep/sibling.rs").exists()
    });
    ui.wait_for("the new file's tab", |ui| {
        ui.active_path().as_deref() == Some("src/deep/sibling.rs")
    });

    // Rename… from the first file's tab: the tab follows.
    ui.press(FIND_FILE_KEY);
    ui.type_text("nested.rs");
    ui.wait_for("nested.rs", |ui| {
        ui.items().first().is_some_and(|i| i.title == "nested.rs")
    });
    ui.press("enter");
    ui.wait_for("the first tab", |ui| {
        ui.active_path().as_deref() == Some("src/deep/nested.rs")
    });
    ui.focus_file_tab();
    ui.run_command("rename", "Rename…");
    ui.enter_name(&tree, "moved.rs");
    ui.wait_for("the tab to follow", |ui| {
        ui.active_path().as_deref() == Some("src/deep/moved.rs")
    });
    assert!(fixture.project.join("src/deep/moved.rs").exists());
}
