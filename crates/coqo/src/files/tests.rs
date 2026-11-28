// SPDX-License-Identifier: Apache-2.0

//! The file tree, file tabs, workspaces and the daemon connection against a
//! real `coqod` served in-process on a temporary socket and data directory.

// `&[a..a]` is a list of one cursor here, not a mistaken range literal.
#![allow(clippy::single_range_in_vec_init)]

use std::{
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_client::connect;
use coqo_protocol::{
    ClientConfig,
    methods::{
        buffer::{self, BufferId, EditParams, OpenParams, OpenResult, ReplicaId, TextEdit},
        editor as edm,
        fs::EntryKind,
        workspace::{RootId, WorkspaceId},
    },
};
use coqo_ui::{
    ActiveTokens as _, Appearance,
    dock::{Edge, store::MemoryLayoutStore},
    editor::CodeEditor,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Keystroke, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::{
    context,
    editor::{Content, FileEditor, HIGHLIGHT_FOR, HoldOpens, KIND},
    tree::{FileTree, Row},
};
use crate::{
    daemon::{self, ConnectionState, Daemon, DaemonConfig},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{Fixture, TIMEOUT, write},
    workspace::{self, Workspaces},
};

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
}

fn open_ui(fixture: &Fixture, folder: Option<PathBuf>) -> Ui {
    let cx = TestAppContext::single();
    // The daemon connection wakes GPUI tasks from its own threads.
    cx.executor().allow_parking();
    let paths = fixture.daemon.paths.clone();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(folder, cx);
        crate::shell::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        coqo_ui::open_window(options, cx, |window, cx| {
            let store = Rc::new(MemoryLayoutStore::default());
            cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
        })
        .expect("open shell window")
    });
    Ui { cx, window, shell }
}

impl Ui {
    fn frame<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx
            .update_window(self.window, |_, window, cx| {
                window.render_frame(cx);
                f(window, cx)
            })
            .unwrap()
    }

    /// Run until `done` holds, letting real time pass for the daemon.
    fn wait_for(&mut self, what: &str, done: impl Fn(&mut Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.cx.run_until_parked();
            self.frame(|_, _| ());
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn tree(&mut self) -> Entity<FileTree> {
        // The Files pane is built on the first frame that shows it.
        self.frame(|_, _| ());
        self.cx
            .read(|cx| self.shell.read(cx).sidebar_pane("files"))
            .and_then(|view| view.downcast::<FileTree>().ok())
            .expect("the files pane is shown")
    }

    fn rows(&mut self) -> Vec<Row> {
        let tree = self.tree();
        self.cx.read(|cx| tree.read(cx).rows().to_vec())
    }

    fn names(&mut self) -> Vec<String> {
        self.rows()
            .iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth), r.name))
            .collect()
    }

    fn row_index(&mut self, name: &str) -> usize {
        self.rows()
            .iter()
            .position(|r| r.name.as_ref() == name)
            .unwrap_or_else(|| panic!("no row {name}"))
    }

    fn click_row(&mut self, name: &str) {
        let ix = self.row_index(name);
        self.frame(|window, cx| window.click(("file-row", ix), cx));
    }

    fn press(&mut self, keys: &'static str) {
        self.frame(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
        });
    }

    fn file_tabs(&mut self) -> Vec<Entity<FileEditor>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
                .collect()
        })
    }

    fn content(&mut self, tab: &Entity<FileEditor>) -> Content {
        self.cx.read(|cx| tab.read(cx).content().clone())
    }

    fn wait_for_tree(&mut self) {
        self.wait_for("the tree", |ui| !ui.rows().is_empty());
    }

    /// The tab's editor, once the file is open as text.
    fn code(&mut self, tab: &Entity<FileEditor>) -> Option<Entity<CodeEditor>> {
        self.cx.read(|cx| tab.read(cx).editor().cloned())
    }

    /// The tab's text, `None` until it's open.
    fn text(&mut self, tab: &Entity<FileEditor>) -> Option<String> {
        let editor = self.code(tab)?;
        Some(self.cx.read(|cx| editor.read(cx).text(cx)))
    }

    fn selections(&mut self, tab: &Entity<FileEditor>) -> Vec<Range<usize>> {
        let editor = self.code(tab).expect("open as text");
        self.cx.read(|cx| editor.read(cx).selections(cx))
    }

    fn dirty(&mut self, tab: &Entity<FileEditor>) -> bool {
        self.cx.read(|cx| tab.read(cx).is_dirty())
    }

    fn synced(&mut self, tab: &Entity<FileEditor>) -> bool {
        self.cx.read(|cx| tab.read(cx).is_synced())
    }

    /// Run `f` on the tab's editor (with its window), then render a frame.
    fn edit<R>(
        &mut self,
        tab: &Entity<FileEditor>,
        f: impl FnOnce(&mut CodeEditor, &mut Window, &mut Context<CodeEditor>) -> R,
    ) -> R {
        let editor = self.code(tab).expect("open as text");
        self.frame(|window, cx| editor.update(cx, |editor, cx| f(editor, window, cx)))
    }

    /// Focus the tab's editor, put the cursor at `at` and type `text`.
    fn type_at(&mut self, tab: &Entity<FileEditor>, at: usize, text: &str) {
        self.edit(tab, |editor, window, cx| {
            editor.focus(window, cx);
            editor.set_selections(&[at..at], cx);
            editor.type_text(text, window, cx);
        });
    }

    /// Open `path` (its folders already expanded) and wait for the editor.
    fn open_file(&mut self, name: &str) -> Entity<FileEditor> {
        let before = self.file_tabs().len();
        self.click_row(name);
        self.wait_for("the editor", |ui| {
            let tabs = ui.file_tabs();
            tabs.len() > before && ui.code(tabs.last().unwrap()).is_some()
        });
        self.file_tabs().pop().unwrap()
    }

    /// The current workspace and its first root.
    fn workspace_id(&mut self) -> Place {
        self.cx.read(|cx| {
            let workspaces = Workspaces::global(cx).unwrap();
            let current = workspaces.read(cx).current().unwrap();
            (current.id.clone(), current.roots[0].id)
        })
    }

    /// Let `duration` pass on GPUI's (simulated) clock.
    fn advance(&mut self, duration: Duration) {
        self.cx.executor().advance_clock(duration);
        self.cx.run_until_parked();
    }
}

/// A workspace and root.
type Place = (WorkspaceId, RootId);

/// Another window on the same daemon: a protocol client of its own.
struct OtherWindow {
    client: coqo_protocol::Client,
}

impl OtherWindow {
    fn connect(fixture: &Fixture) -> Self {
        let paths = fixture.daemon.paths.clone();
        let (client, _) = fixture
            .daemon
            .runtime
            .block_on(connect(&paths, ClientConfig::new("coqo-tests", "test")))
            .unwrap();
        Self { client }
    }

    /// `buffer/open` the file: its current text and the replica to edit with.
    fn open(&self, fixture: &Fixture, (workspace, root): &Place, path: &str) -> OpenResult {
        let params = OpenParams {
            workspace: workspace.clone(),
            root: *root,
            path: path.into(),
            replica: None,
        };
        let client = self.client.clone();
        fixture
            .daemon
            .runtime
            .block_on(async move { client.request::<buffer::Open>(params).await })
            .unwrap()
    }

    /// The daemon's text of the file.
    fn text(&self, fixture: &Fixture, workspace: &Place, path: &str) -> String {
        self.open(fixture, workspace, path).text
    }

    /// Edit the file as another window would.
    fn edit(&self, fixture: &Fixture, workspace: &Place, path: &str, edit: TextEdit) {
        let open = self.open(fixture, workspace, path);
        let params = EditParams {
            buffer: open.buffer,
            replica: open.replica,
            base_rev: open.rev,
            client_seq: open.last_seq + 1,
            edits: vec![edit],
        };
        let client = self.client.clone();
        fixture
            .daemon
            .runtime
            .block_on(async move { client.notify::<buffer::Edit>(params).await })
            .unwrap();
    }
}

#[cfg(target_os = "macos")]
const SAVE: &str = "cmd-s";
#[cfg(not(target_os = "macos"))]
const SAVE: &str = "ctrl-s";
#[cfg(target_os = "macos")]
const UNDO: &str = "cmd-z";
#[cfg(not(target_os = "macos"))]
const UNDO: &str = "ctrl-z";
#[cfg(target_os = "macos")]
const REDO: &str = "cmd-shift-z";
#[cfg(not(target_os = "macos"))]
const REDO: &str = "ctrl-y";

fn state(ui: &mut Ui) -> ConnectionState {
    ui.cx
        .read(|cx| Daemon::global(cx).unwrap().read(cx).state().clone())
}

fn current_name(ui: &mut Ui) -> Option<String> {
    ui.cx.read(|cx| {
        Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .map(|w| w.name.clone())
    })
}

#[cfg(target_os = "macos")]
const OPEN_FOLDER: &str = "cmd-o";
#[cfg(not(target_os = "macos"))]
const OPEN_FOLDER: &str = "ctrl-o";

#[test]
fn opens_the_folder_and_lists_it() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for("a connection", |ui| {
        matches!(state(ui), ConnectionState::Connected { .. })
    });
    ui.wait_for_tree();
    assert_eq!(current_name(&mut ui).as_deref(), Some("demo-project"));
    assert_eq!(
        ui.names(),
        ["src", "target", ".gitignore", "logo.png", "README.md"]
    );
    let rows = ui.rows();
    assert_eq!(rows[0].kind, EntryKind::Dir);
    assert!(!rows[0].ignored);
    assert!(rows[1].ignored, "target/ is ignored by .gitignore");
    assert!(!rows[3].ignored);
    ui.frame(|window, _| {
        for id in ["workspace-name", "daemon-status", "sidebar-files-pane"] {
            assert!(window.find(id).visible(), "{id} is not visible");
        }
        // The rail shows the workspace, lit as current.
        assert!(window.find(("workspace", 0usize)).visible());
        assert!(window.try_find("daemon-retry").is_none());
    });
}

#[test]
fn expands_collapses_and_navigates_with_keys() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();

    ui.click_row("src");
    ui.wait_for("src to expand", |ui| ui.names().len() == 7);
    assert_eq!(
        ui.names(),
        [
            "src",
            "  lib.rs",
            "  main.rs",
            "target",
            ".gitignore",
            "logo.png",
            "README.md"
        ]
    );
    // Clicking a folder again collapses it.
    ui.click_row("src");
    assert_eq!(ui.names().len(), 5);

    // The click focused the tree and selected src: drive it by keyboard.
    ui.press("right");
    assert_eq!(ui.names().len(), 7, "right expands");
    ui.press("right");
    let tree = ui.tree();
    let selected = |ui: &mut Ui| {
        ui.cx
            .read(|cx| tree.read(cx).selected().map(|(_, p)| p.to_owned()))
    };
    assert_eq!(
        selected(&mut ui).as_deref(),
        Some("src/lib.rs"),
        "right enters"
    );
    ui.press("down");
    assert_eq!(selected(&mut ui).as_deref(), Some("src/main.rs"));
    ui.press("left");
    assert_eq!(
        selected(&mut ui).as_deref(),
        Some("src"),
        "left selects the parent"
    );
    ui.press("left");
    assert_eq!(ui.names().len(), 5, "left collapses");
    ui.press("up");
    assert_eq!(
        selected(&mut ui).as_deref(),
        Some("src"),
        "stays at the top"
    );
    for _ in 0..4 {
        ui.press("down");
    }
    assert_eq!(selected(&mut ui).as_deref(), Some("README.md"));
    ui.press("down");
    assert_eq!(
        selected(&mut ui).as_deref(),
        Some("README.md"),
        "stays at the bottom"
    );
    ui.press("enter");
    ui.wait_for("the README tab", |ui| ui.file_tabs().len() == 1);
}

const MAIN_RS: &str = "fn main() {\n\tprintln!(\"hi\");\n}\n";

#[test]
fn opens_files_in_editor_tabs() {
    let fixture = Fixture::new();
    write(&fixture.project, "huge.txt", "a".repeat(5 * 1024 * 1024));
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    ui.click_row("src");
    ui.wait_for("src to expand", |ui| ui.names().len() == 8);

    let tab = ui.open_file("main.rs");
    assert_eq!(ui.content(&tab), Content::Text);
    assert_eq!(ui.text(&tab).as_deref(), Some(MAIN_RS));
    let editor = ui.code(&tab).unwrap();
    assert_eq!(ui.cx.read(|cx| editor.read(cx).language(cx)), "rust");
    assert!(!ui.dirty(&tab));
    ui.frame(|window, _| {
        assert!(window.find("file-editor-text").visible());
        assert!(window.try_find("file-tab-dirty").is_none());
    });

    // Opening it again shows the same tab.
    ui.click_row("main.rs");
    assert_eq!(ui.file_tabs().len(), 1);

    ui.click_row("logo.png");
    ui.wait_for("the binary tab", |ui| {
        let tabs = ui.file_tabs();
        tabs.len() == 2 && matches!(ui.content(&tabs[1]), Content::Binary { size } if size == 8)
    });
    ui.frame(|window, _| assert!(window.find("file-message").visible()));

    ui.click_row("huge.txt");
    ui.wait_for("the too-large tab", |ui| {
        let tabs = ui.file_tabs();
        tabs.len() == 3
            && matches!(ui.content(&tabs[2]), Content::TooLarge { size, .. } if size == 5 * 1024 * 1024)
    });
}

#[test]
fn follows_changes_on_disk() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    ui.click_row("src");
    ui.wait_for("src to expand", |ui| ui.names().len() == 7);
    ui.open_file("main.rs");

    write(&fixture.project, "src/new.rs", "// new\n");
    ui.wait_for("the new file", |ui| {
        ui.names().iter().any(|n| n == "  new.rs")
    });
    std::fs::remove_file(fixture.project.join("README.md")).unwrap();
    ui.wait_for("README.md to go", |ui| {
        !ui.names().iter().any(|n| n == "README.md")
    });
    std::fs::create_dir(fixture.project.join("docs")).unwrap();
    ui.wait_for("the new folder", |ui| {
        ui.names().iter().any(|n| n == "docs")
    });

    // An open file tab follows edits.
    write(&fixture.project, "src/main.rs", "fn main() {}\n");
    let tab = ui.file_tabs()[0].clone();
    ui.wait_for("the tab to reload", |ui| {
        ui.text(&tab).as_deref() == Some("fn main() {}\n")
    });
    assert!(!ui.dirty(&tab), "a clean buffer reloads clean");
}

/// Open the fixture with `src` expanded and `src/main.rs` in a tab.
fn open_main(fixture: &Fixture) -> (Ui, Entity<FileEditor>) {
    let mut ui = open_ui(fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    ui.click_row("src");
    ui.wait_for("src to expand", |ui| ui.names().len() == 7);
    let tab = ui.open_file("main.rs");
    (ui, tab)
}

#[test]
fn typing_reaches_the_daemon_and_save_writes_the_file() {
    let fixture = Fixture::new();
    let (mut ui, tab) = open_main(&fixture);
    let ws = ui.workspace_id();
    let other = OtherWindow::connect(&fixture);

    ui.type_at(&tab, 0, "// hello\n");
    let expected = format!("// hello\n{MAIN_RS}");
    assert_eq!(ui.text(&tab).as_deref(), Some(expected.as_str()));
    ui.wait_for("the edit to reach the daemon", |ui| ui.synced(&tab));
    assert_eq!(other.text(&fixture, &ws, "src/main.rs"), expected);
    assert!(ui.dirty(&tab));
    ui.frame(|window, _| assert!(window.find("file-tab-dirty").visible()));
    assert_eq!(
        std::fs::read_to_string(fixture.project.join("src/main.rs")).unwrap(),
        MAIN_RS,
        "nothing is written before a save"
    );

    // Type more and save at once: the save waits for the edit.
    ui.type_at(&tab, expected.len(), "// bye\n");
    ui.press(SAVE);
    let saved = format!("{expected}// bye\n");
    ui.wait_for("the save", |ui| {
        !ui.dirty(&tab)
            && std::fs::read_to_string(fixture.project.join("src/main.rs")).unwrap() == saved
    });
    ui.frame(|window, _| assert!(window.try_find("file-tab-dirty").is_none()));
    assert!(ui.cx.read(|cx| tab.read(cx).notice().is_none()));
}

#[test]
fn disk_changes_keep_the_cursor_and_undo_keeps_them() {
    let fixture = Fixture::new();
    let (mut ui, tab) = open_main(&fixture);

    // The user types at the end: an unsaved edit.
    ui.type_at(&tab, MAIN_RS.len(), "// mine\n");
    ui.wait_for("the edit to reach the daemon", |ui| ui.synced(&tab));
    // The cursor sits in the middle of `println`.
    let cursor = MAIN_RS.find("println").unwrap() + 3;
    ui.edit(&tab, |editor, _, cx| {
        editor.set_selections(&[cursor..cursor], cx)
    });

    // An agent (or anything else) rewrites the file on disk.
    let header = "// written by an agent\n";
    write(
        &fixture.project,
        "src/main.rs",
        format!("{header}{MAIN_RS}"),
    );
    let merged = format!("{header}{MAIN_RS}// mine\n");
    ui.wait_for("the disk change", |ui| {
        ui.text(&tab).as_deref() == Some(merged.as_str())
    });

    // The cursor stayed on its text, and the change is tinted.
    let moved = cursor + header.len();
    assert_eq!(ui.selections(&tab), vec![moved..moved]);
    let editor = ui.code(&tab).unwrap();
    let tinted = ui.cx.read(|cx| editor.read(cx).change_highlights(cx));
    assert_eq!(tinted, vec![0..header.len()]);
    assert!(ui.dirty(&tab), "the user's edit is still unsaved");

    // Undo reverts the user's edit only, not the disk change; redo brings it back.
    ui.press(UNDO);
    let undone = format!("{header}{MAIN_RS}");
    assert_eq!(ui.text(&tab).as_deref(), Some(undone.as_str()));
    ui.press(UNDO);
    assert_eq!(
        ui.text(&tab).as_deref(),
        Some(undone.as_str()),
        "nothing else to undo"
    );
    ui.wait_for("the undo to be clean", |ui| {
        ui.synced(&tab) && !ui.dirty(&tab)
    });
    ui.press(REDO);
    assert_eq!(ui.text(&tab).as_deref(), Some(merged.as_str()));

    // The tint fades.
    ui.advance(HIGHLIGHT_FOR);
    assert!(
        ui.cx
            .read(|cx| editor.read(cx).change_highlights(cx))
            .is_empty()
    );
}

#[test]
fn edits_from_another_window_keep_the_cursor() {
    let fixture = Fixture::new();
    let (mut ui, tab) = open_main(&fixture);
    let ws = ui.workspace_id();
    let cursor = MAIN_RS.find("hi").unwrap();
    ui.edit(&tab, |editor, window, cx| {
        editor.focus(window, cx);
        editor.set_selections(&[cursor..cursor], cx);
    });

    let other = OtherWindow::connect(&fixture);
    other.edit(
        &fixture,
        &ws,
        "src/main.rs",
        TextEdit::insert(0, "use std::io;\n"),
    );
    let expected = format!("use std::io;\n{MAIN_RS}");
    ui.wait_for("the other window's edit", |ui| {
        ui.text(&tab).as_deref() == Some(expected.as_str())
    });
    let moved = cursor + "use std::io;\n".len();
    assert_eq!(ui.selections(&tab), vec![moved..moved]);
    let editor = ui.code(&tab).unwrap();
    assert!(
        ui.cx
            .read(|cx| editor.read(cx).change_highlights(cx))
            .is_empty(),
        "another window's typing isn't tinted"
    );

    // Typing continues at the cursor, and undo has nothing of ours to revert.
    ui.press(UNDO);
    assert_eq!(ui.text(&tab).as_deref(), Some(expected.as_str()));
    ui.edit(&tab, |editor, window, cx| editor.type_text("!", window, cx));
    ui.wait_for("the edit to reach the daemon", |ui| ui.synced(&tab));
    assert_eq!(
        other.text(&fixture, &ws, "src/main.rs"),
        "use std::io;\nfn main() {\n\tprintln!(\"!hi\");\n}\n"
    );
}

#[test]
fn remote_edits_wait_for_an_ime_composition() {
    let fixture = Fixture::new();
    let (mut ui, tab) = open_main(&fixture);
    let ws = ui.workspace_id();
    let end = MAIN_RS.len();
    ui.edit(&tab, |editor, window, cx| {
        editor.focus(window, cx);
        editor.set_selections(&[end..end], cx);
        editor.compose("ni", window, cx);
    });
    let editor = ui.code(&tab).unwrap();
    assert!(ui.cx.read(|cx| editor.read(cx).is_composing(cx)));

    let other = OtherWindow::connect(&fixture);
    other.edit(
        &fixture,
        &ws,
        "src/main.rs",
        TextEdit::insert(0, "// top\n"),
    );
    // Give the change time to arrive; it is held while composing.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        ui.text(&tab).as_deref(),
        Some(format!("{MAIN_RS}ni").as_str()),
        "nothing lands in the middle of a composition"
    );

    // Committing the composition lets the held change in.
    ui.edit(&tab, |editor, window, cx| {
        editor.type_text("你", window, cx)
    });
    assert!(!ui.cx.read(|cx| editor.read(cx).is_composing(cx)));
    let expected = format!("// top\n{MAIN_RS}你");
    ui.wait_for("the held change", |ui| {
        ui.text(&tab).as_deref() == Some(expected.as_str())
    });
    ui.wait_for("the commit to reach the daemon", |ui| ui.synced(&tab));
    assert_eq!(other.text(&fixture, &ws, "src/main.rs"), expected);
}

#[test]
fn split_tabs_on_one_file_stay_in_sync() {
    let fixture = Fixture::new();
    let (mut ui, left) = open_main(&fixture);
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    ui.frame(|window, cx| dock.update(cx, |dock, cx| dock.split(Edge::Right, window, cx)));
    ui.wait_for("the split's editor", |ui| {
        let tabs = ui.file_tabs();
        tabs.len() == 2 && tabs.iter().all(|t| ui.code(t).is_some())
    });
    let right = ui
        .file_tabs()
        .into_iter()
        .find(|t| *t != left)
        .expect("the duplicate tab");

    ui.type_at(&left, 0, "// left\n");
    let both = format!("// left\n{MAIN_RS}");
    ui.wait_for("the right tab to follow", |ui| {
        ui.text(&right).as_deref() == Some(both.as_str())
    });
    ui.type_at(&right, both.len(), "// right\n");
    let all = format!("{both}// right\n");
    ui.wait_for("the left tab to follow", |ui| {
        ui.text(&left).as_deref() == Some(all.as_str())
    });
    assert!(ui.dirty(&left) && ui.dirty(&right));

    // Undo is per tab: the left tab undoes its own edit only.
    ui.edit(&left, |editor, window, cx| editor.focus(window, cx));
    ui.press(UNDO);
    let without_left = format!("{MAIN_RS}// right\n");
    assert_eq!(ui.text(&left).as_deref(), Some(without_left.as_str()));
    ui.wait_for("the right tab to follow the undo", |ui| {
        ui.text(&right).as_deref() == Some(without_left.as_str())
    });

    // Saving in one tab cleans both.
    ui.press(SAVE);
    ui.wait_for("both tabs to be clean", |ui| {
        !ui.dirty(&left) && !ui.dirty(&right)
    });
    assert_eq!(
        std::fs::read_to_string(fixture.project.join("src/main.rs")).unwrap(),
        without_left
    );

    // Closing a tab leaves the other one working.
    let right_id = ui.cx.read(|cx| {
        let dock = dock.read(cx);
        dock.tree()
            .tabs()
            .into_iter()
            .map(|t| t.id)
            .find(|id| {
                dock.tab(*id).and_then(|t| t.downcast::<FileEditor>()) == Some(right.clone())
            })
            .unwrap()
    });
    ui.frame(|_, cx| dock.update(cx, |dock, cx| dock.close_tab(right_id, cx)));
    drop(right);
    ui.type_at(&left, 0, "// still\n");
    ui.wait_for("the edit after closing the split", |ui| ui.synced(&left));
}

#[test]
fn save_conflicts_offer_overwrite_and_reload() {
    let fixture = Fixture::new();
    let (mut ui, tab) = open_main(&fixture);
    let file = fixture.project.join("src/main.rs");

    // An unsaved edit, then the file changes on disk right before saving.
    let conflict = |ui: &mut Ui, disk: &str| {
        for attempt in 0.. {
            ui.wait_for("the edits to reach the daemon", |ui| ui.synced(&tab));
            write(
                &fixture.project,
                "src/main.rs",
                format!("{disk}{attempt}\n"),
            );
            ui.press(SAVE);
            ui.wait_for("the save to finish", |ui| {
                ui.cx.read(|cx| tab.read(cx).notice().is_some()) || !ui.dirty(&tab)
            });
            if ui.cx.read(|cx| tab.read(cx).notice().is_some()) {
                return format!("{disk}{attempt}\n");
            }
            // The watcher was faster and the save went through; try again.
            assert!(attempt < 5, "no save conflict after {attempt} attempts");
            ui.type_at(&tab, 0, "x");
        }
        unreachable!()
    };

    ui.type_at(&tab, 0, "// mine\n");
    let on_disk = conflict(&mut ui, "// theirs ");
    ui.frame(|window, _| {
        assert!(window.find("file-notice").visible());
        assert!(window.find("file-conflict-overwrite").visible());
    });
    // The disk change was merged into the buffer.
    ui.wait_for("the merge", |ui| {
        ui.text(&tab).is_some_and(|t| t.contains("// theirs"))
    });

    // Overwrite: the buffer (with the merge) is written.
    ui.frame(|window, cx| window.click("file-conflict-overwrite", cx));
    ui.wait_for("the overwrite", |ui| {
        !ui.dirty(&tab) && std::fs::read_to_string(&file).unwrap() == ui.text(&tab).unwrap()
    });
    assert!(ui.cx.read(|cx| tab.read(cx).notice().is_none()));
    assert!(std::fs::read_to_string(&file).unwrap().contains("// mine"));
    let _ = on_disk;

    // Reload: the unsaved edit is dropped for the file on disk.
    ui.type_at(&tab, 0, "// again\n");
    let on_disk = conflict(&mut ui, "// replaced ");
    ui.frame(|window, cx| window.click("file-conflict-reload", cx));
    ui.wait_for("the reload", |ui| {
        ui.text(&tab).as_deref() == Some(on_disk.as_str()) && ui.synced(&tab) && !ui.dirty(&tab)
    });
    assert!(ui.cx.read(|cx| tab.read(cx).notice().is_none()));
}

#[test]
fn reconnects_and_resyncs_unsaved_edits() {
    let mut fixture = Fixture::new();
    let (mut ui, tab) = open_main(&fixture);
    let ws = ui.workspace_id();

    ui.type_at(&tab, 0, "// before\n");
    ui.wait_for("the edit to reach the daemon", |ui| ui.synced(&tab));

    fixture.daemon.stop();
    ui.wait_for("the drop", |ui| {
        matches!(state(ui), ConnectionState::Disconnected { .. })
    });
    // Typing goes on while the daemon is away.
    let before = format!("// before\n{MAIN_RS}");
    ui.type_at(&tab, before.len(), "// offline\n");
    let all = format!("{before}// offline\n");
    assert_eq!(ui.text(&tab).as_deref(), Some(all.as_str()));
    assert!(!ui.synced(&tab));

    fixture.daemon.serve();
    ui.wait_for("the resync", |ui| {
        if ui.frame(|window, _| window.try_find("daemon-retry").is_some()) {
            ui.frame(|window, cx| window.click("daemon-retry", cx));
        }
        matches!(state(ui), ConnectionState::Connected { .. }) && ui.synced(&tab)
    });
    assert_eq!(ui.text(&tab).as_deref(), Some(all.as_str()));
    let other = OtherWindow::connect(&fixture);
    assert_eq!(other.text(&fixture, &ws, "src/main.rs"), all);

    // And the tab keeps working.
    other.edit(
        &fixture,
        &ws,
        "src/main.rs",
        TextEdit::insert(0, "// other\n"),
    );
    let expected = format!("// other\n{all}");
    ui.wait_for("a change after the resync", |ui| {
        ui.text(&tab).as_deref() == Some(expected.as_str())
    });
}

#[test]
fn restores_the_last_workspace() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    drop(ui);

    let mut ui = open_ui(&fixture, None);
    ui.wait_for("the last workspace", |ui| {
        current_name(ui).as_deref() == Some("demo-project")
    });
    ui.wait_for_tree();
}

#[test]
fn opens_a_folder_from_the_picker() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, None);
    ui.wait_for("a connection", |ui| {
        matches!(state(ui), ConnectionState::Connected { .. })
    });
    ui.cx.run_until_parked();
    assert_eq!(current_name(&mut ui), None, "nothing to restore");
    // The empty state appears once the pane's restore check (a daemon
    // request on the connection's own threads) has finished, which
    // `run_until_parked` doesn't wait for (#173).
    ui.wait_for("the open-folder button", |ui| {
        ui.frame(|window, _| window.try_find("files-open-folder").is_some())
    });
    ui.frame(|window, _| assert!(window.find("files-open-folder").visible()));

    ui.press(OPEN_FOLDER);
    ui.cx.run_until_parked();
    assert!(ui.cx.did_prompt_for_paths());
    let project = fixture.project.clone();
    ui.cx.simulate_path_prompt_response(move |options| {
        assert!(options.directories && !options.files);
        Some(vec![project])
    });
    ui.wait_for("the picked folder", |ui| {
        current_name(ui).as_deref() == Some("demo-project")
    });
    ui.wait_for_tree();

    // The rail's + opens the picker too.
    ui.frame(|window, cx| window.click("add-workspace", cx));
    ui.cx.run_until_parked();
    assert!(ui.cx.did_prompt_for_paths());
    ui.cx.simulate_path_prompt_response(|_| None);
}

#[test]
fn reconnects_when_the_daemon_comes_back() {
    let mut fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();

    fixture.daemon.stop();
    ui.wait_for("the drop to show", |ui| {
        matches!(state(ui), ConnectionState::Disconnected { .. })
            && ui.frame(|window, _| window.try_find("daemon-retry").is_some())
    });

    fixture.daemon.serve();
    ui.wait_for("a new connection", |ui| {
        if ui.frame(|window, _| window.try_find("daemon-retry").is_some()) {
            ui.frame(|window, cx| window.click("daemon-retry", cx));
        }
        matches!(state(ui), ConnectionState::Connected { .. })
    });
    // The workspace is reopened and the tree listed again.
    ui.wait_for("the tree again", |ui| ui.names().len() == 5);
    write(&fixture.project, "after.txt", "x");
    ui.wait_for("changes after the reconnect", |ui| {
        ui.names().iter().any(|n| n == "after.txt")
    });
}

/// The editor context of `ws` as an agent reads it from the daemon.
fn editor_context(fixture: &Fixture, ws: &WorkspaceId) -> edm::EditorContext {
    let workspace = ws.clone();
    fixture.daemon.with_client(async move |client| {
        client
            .request::<edm::GetContext>(edm::GetContextParams { workspace })
            .await
            .unwrap()
    })
}

/// Call coqo-editor tool `tool` as an agent of workspace `ws` would (the
/// bridge's tool, on a connection of its own).
fn agent_tool(
    fixture: &Fixture,
    ws: &WorkspaceId,
    tool: impl coqo_mcp::Tool,
    args: coqo_ui::serde_json::Value,
) -> coqo_mcp::ToolOutput {
    let workspace = ws.clone();
    fixture.daemon.with_client(async move |client| {
        let cx = coqo_mcp::ToolContext::new(client, workspace, "s-test");
        let args = args.as_object().cloned().unwrap_or_default();
        tool.call(&cx, args).await.unwrap()
    })
}

/// The one window's tab of `path`, if the context has it.
fn context_tab<'a>(cx: &'a edm::EditorContext, path: &str) -> Option<&'a edm::EditorTab> {
    cx.windows
        .first()?
        .context
        .tabs
        .iter()
        .find(|t| t.path == path)
}

impl Ui {
    /// Let the publish throttle fire until the daemon's context of `ws`
    /// satisfies `ok`.
    fn wait_for_context(
        &mut self,
        fixture: &Fixture,
        ws: &WorkspaceId,
        what: &str,
        ok: impl Fn(&edm::EditorContext) -> bool,
    ) -> edm::EditorContext {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.advance(context::PUBLISH_EVERY);
            self.frame(|_, _| ());
            let cx = editor_context(fixture, ws);
            if ok(&cx) {
                return cx;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {cx:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn close_file_tab(&mut self, ws: &WorkspaceId, root: RootId, path: &str) {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        let key = super::editor::key(ws, root, path);
        self.cx.update(|cx| {
            let id = dock.read(cx).find_tab(KIND, &key, cx).expect("the tab");
            dock.update(cx, |dock, cx| dock.close_tab(id, cx));
        });
    }

    /// The open tab of `path`, once it shows the file as text.
    fn loaded_tab(&mut self, path: &str) -> Option<Entity<FileEditor>> {
        let tab = self
            .file_tabs()
            .into_iter()
            .find(|t| self.cx.read(|cx| t.read(cx).path() == path))?;
        self.code(&tab).is_some().then_some(tab)
    }
}

#[test]
fn publishes_the_editor_context_for_agents() {
    let fixture = Fixture::new();
    let (mut ui, main) = open_main(&fixture);
    let (ws, root) = ui.workspace_id();

    // Select `main` on the first line.
    ui.edit(&main, |editor, _, cx| editor.set_selections(&[3..7], cx));
    let cx = ui.wait_for_context(&fixture, &ws, "the selection", |cx| {
        context_tab(cx, "src/main.rs")
            .and_then(|t| t.selections.first())
            .is_some_and(|s| s.text.as_deref() == Some("main"))
    });
    assert_eq!(cx.windows.len(), 1, "{cx:?}");
    let tab = context_tab(&cx, "src/main.rs").unwrap();
    assert_eq!(tab.root, root);
    assert!(tab.active && !tab.dirty, "{tab:?}");
    assert_eq!(
        tab.selections[0].range,
        edm::Range::new(edm::Position::new(0, 3), edm::Position::new(0, 7))
    );
    assert_eq!(tab.visible.map(|v| v.start), Some(0), "{tab:?}");
    // What an agent's `editor_context` tool makes of it: 1-based.
    let out = agent_tool(
        &fixture,
        &ws,
        coqo_mcp::tools::EditorContextTool,
        coqo_ui::serde_json::json!({}),
    );
    let s = out.structured.unwrap();
    // The daemon canonicalizes roots (`/var` is `/private/var` on macOS).
    let main_path = fixture.project.canonicalize().unwrap().join("src/main.rs");
    assert_eq!(s["activeFile"]["path"], main_path.to_str().unwrap(), "{s}");
    assert_eq!(
        s["activeFile"]["selections"][0],
        coqo_ui::serde_json::json!({
            "start": { "line": 1, "column": 4 },
            "end": { "line": 1, "column": 8 },
            "text": "main",
        })
    );
    assert!(
        out.text.contains("Selection: 1:4-1:8\n```\nmain\n```"),
        "{}",
        out.text
    );

    // A cursor in the second line, after the tab and the typed `x`; typing
    // makes it dirty.
    ui.type_at(&main, 13, "x");
    let cx = ui.wait_for_context(&fixture, &ws, "the cursor and the edit", |cx| {
        context_tab(cx, "src/main.rs").is_some_and(|t| t.dirty)
    });
    let tab = context_tab(&cx, "src/main.rs").unwrap();
    assert_eq!(
        tab.selections[0].range,
        edm::Range::new(edm::Position::new(1, 2), edm::Position::new(1, 2))
    );
    assert_eq!(tab.selections[0].text, None);

    // Another tab becomes the active one.
    ui.open_file("lib.rs");
    let cx = ui.wait_for_context(&fixture, &ws, "lib.rs active", |cx| {
        context_tab(cx, "src/lib.rs").is_some_and(|t| t.active)
    });
    let names: Vec<_> = cx.windows[0]
        .context
        .tabs
        .iter()
        .map(|t| t.path.as_str())
        .collect();
    assert_eq!(names, ["src/main.rs", "src/lib.rs"]);
    assert!(!context_tab(&cx, "src/main.rs").unwrap().active);

    // Closing it leaves main.rs.
    ui.close_file_tab(&ws, root, "src/lib.rs");
    let cx = ui.wait_for_context(&fixture, &ws, "lib.rs closed", |cx| {
        context_tab(cx, "src/lib.rs").is_none()
    });
    assert!(context_tab(&cx, "src/main.rs").unwrap().active, "{cx:?}");

    // A closed window takes its context with it.
    let Ui {
        mut cx,
        window,
        shell,
    } = ui;
    drop((main, shell));
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let deadline = Instant::now() + TIMEOUT;
    while !editor_context(&fixture, &ws).windows.is_empty() {
        assert!(Instant::now() < deadline, "the context outlived its window");
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
}

#[test]
fn reveals_open_the_file_and_select_the_range() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    let (ws, root) = ui.workspace_id();
    ui.wait_for_context(&fixture, &ws, "the window", |cx| !cx.windows.is_empty());

    let reveal = |range: Option<edm::Range>, path: &str| {
        let params = edm::RevealParams {
            workspace: ws.clone(),
            root,
            path: path.into(),
            range,
        };
        fixture
            .daemon
            .with_client(async move |client| client.request::<edm::Reveal>(params).await.unwrap())
    };
    // `f` in `pub fn f() {}`.
    let f = edm::Range::new(edm::Position::new(0, 7), edm::Position::new(0, 8));
    // The UI follows reveals from when it connected: ask until it shows.
    let deadline = Instant::now() + TIMEOUT;
    let tab = 'shown: loop {
        assert!(reveal(Some(f), "src/lib.rs").delivered);
        let retry = Instant::now() + Duration::from_millis(300);
        while Instant::now() < retry {
            ui.cx.run_until_parked();
            ui.frame(|_, _| ());
            if let Some(tab) = ui.loaded_tab("src/lib.rs") {
                break 'shown tab;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Instant::now() < deadline,
            "the reveal did not open the file"
        );
    };
    ui.wait_for("the range selected", |ui| ui.selections(&tab) == [7..8]);
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    let active = ui.cx.read(|cx| {
        let dock = dock.read(cx);
        dock.active_tab()
            .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
    });
    assert_eq!(active, Some(tab.clone()));

    // The agent's `editor_reveal` tool (1-based) lands in the same tab.
    let out = agent_tool(
        &fixture,
        &ws,
        coqo_mcp::tools::EditorRevealTool,
        coqo_ui::serde_json::json!({ "path": "src/lib.rs", "line": 1, "column": 1, "end_column": 4 }),
    );
    assert_eq!(out.structured.unwrap()["delivered"], true);
    ui.wait_for("the tool's range selected", |ui| {
        ui.selections(&tab) == [0..3]
    });

    // Revealing it again reuses the tab; positions past the text clamp to
    // its end, and no range is the top of the file.
    let past = edm::Range::new(edm::Position::new(0, 4), edm::Position::new(9, 0));
    reveal(Some(past), "src/lib.rs");
    let end = "pub fn f() {}\n".len();
    ui.wait_for("the clamped range", |ui| ui.selections(&tab) == [4..end]);
    reveal(None, "src/lib.rs");
    ui.wait_for("the top", |ui| ui.selections(&tab) == [0..0]);
    assert_eq!(ui.file_tabs().len(), 1);
}

#[test]
fn renders_only_the_visible_rows() {
    let fixture = Fixture::new();
    let many = fixture.project.join("many");
    std::fs::create_dir(&many).unwrap();
    for i in 0..10_000 {
        std::fs::write(many.join(format!("file-{i:05}.txt")), "").unwrap();
    }
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    ui.click_row("many");
    ui.wait_for("10k rows", |ui| ui.rows().len() > 10_000);
    ui.frame(|window, _| {
        assert!(window.try_find(("file-row", 1usize)).is_some());
        assert!(window.try_find(("file-row", 5_000usize)).is_none());
        assert!(window.try_find(("file-row", 10_004usize)).is_none());
    });
    // Moving down the list scrolls it.
    for _ in 0..40 {
        ui.press("down");
    }
    ui.frame(|window, _| assert!(window.try_find(("file-row", 40usize)).is_some()));
}

impl Ui {
    fn shows(&mut self, id: &'static str) -> bool {
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn click(&mut self, id: &'static str) {
        self.frame(|window, cx| window.click(id, cx));
    }

    fn right_click_row(&mut self, name: &str) {
        let ix = self.row_index(name);
        self.frame(|window, cx| window.right_click(("file-row", ix), cx));
    }

    fn selected_path(&mut self) -> Option<String> {
        let tree = self.tree();
        self.cx
            .read(|cx| tree.read(cx).selected().map(|(_, p)| p.to_owned()))
    }

    /// Type `name` in the tree's name field and press Enter.
    fn enter_name(&mut self, name: &str) {
        let tree = self.tree();
        let input = self
            .cx
            .read(|cx| tree.read(cx).name_field().map(|(_, input)| input.clone()))
            .expect("a name field");
        let name = name.to_owned();
        self.cx
            .update_window(self.window, |_, window, cx| {
                input.update(cx, |input, cx| input.set_text(name, window, cx));
            })
            .unwrap();
        self.press("enter");
    }

    fn tab_at(&mut self, path: &str) -> Option<Entity<FileEditor>> {
        let tabs = self.file_tabs();
        tabs.into_iter()
            .find(|t| self.cx.read(|cx| t.read(cx).path() == path))
    }
}

/// The tree's file operations against a real daemon: create from the
/// context menu, rename in place with F2 (the open tab follows, unsaved
/// edits and all), a name clash, delete with a confirmation that names the
/// file (to the test daemon's trash folder), a delete that keeps a tab's
/// unsaved edits, and a move by drag and drop.
#[test]
fn creates_renames_moves_and_deletes_from_the_tree() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    let tree = ui.tree();

    // New File from the menu of `src`: the field opens inside it.
    ui.right_click_row("src");
    assert!(ui.shows("file-menu"));
    assert_eq!(
        ui.cx
            .read(|cx| tree.read(cx).menu_target().map(|t| t.path.clone())),
        Some("src".to_owned())
    );
    ui.click("file-menu-new-file");
    assert!(!ui.shows("file-menu"));
    ui.wait_for("the name field in src", |ui| {
        ui.rows().iter().any(|r| r.editing && r.depth == 1)
    });
    assert!(ui.shows("file-name-field"));
    ui.enter_name("notes.md");
    ui.wait_for("notes.md in the tree, selected and open", |ui| {
        ui.names().contains(&"  notes.md".to_owned())
            && ui.selected_path().as_deref() == Some("src/notes.md")
            && ui.tab_at("src/notes.md").is_some()
    });
    assert!(fixture.project.join("src/notes.md").is_file());
    assert!(!ui.rows().iter().any(|r| r.editing));

    // Rename in place with F2: the open tab follows, with its unsaved edit.
    let main = ui.open_file("main.rs");
    ui.type_at(&main, 0, "// edited\n");
    ui.wait_for("the edit to reach the daemon", |ui| ui.synced(&main));
    // Opening a file puts the keyboard in its tab; select it from the tree.
    ui.click_row("src");
    ui.press("right");
    ui.press("right");
    ui.press("down");
    assert_eq!(ui.selected_path().as_deref(), Some("src/main.rs"));
    ui.press("f2");
    let field_text = ui.cx.read(|cx| {
        tree.read(cx)
            .name_field()
            .map(|(_, input)| input.read(cx).text(cx))
    });
    assert_eq!(field_text.as_deref(), Some("main.rs"));
    ui.enter_name("app.rs");
    ui.wait_for("app.rs in the tree and the tab", |ui| {
        let names = ui.names();
        names.contains(&"  app.rs".to_owned())
            && !names.contains(&"  main.rs".to_owned())
            && ui.cx.read(|cx| main.read(cx).path() == "src/app.rs")
    });
    assert_eq!(ui.selected_path().as_deref(), Some("src/app.rs"));
    assert!(fixture.project.join("src/app.rs").is_file());
    assert!(!fixture.project.join("src/main.rs").exists());
    assert!(ui.dirty(&main), "the unsaved edit is still there");
    assert_eq!(
        ui.text(&main).as_deref(),
        Some(format!("// edited\n{MAIN_RS}").as_str())
    );
    let (title, key) = ui.cx.read(|cx| {
        use coqo_ui::dock::TabItem as _;
        let tab = main.read(cx);
        (tab.title(cx), tab.key(cx))
    });
    assert_eq!(title, "app.rs");
    assert!(key.unwrap().ends_with("/src/app.rs"));

    // A clash keeps the field open with the reason; Escape gives up.
    ui.press("f2");
    ui.enter_name("lib.rs");
    ui.wait_for("the clash", |ui| {
        let tree = ui.tree();
        ui.cx.read(|cx| tree.read(cx).op_error().is_some())
    });
    assert!(ui.shows("file-op-error"));
    assert!(ui.rows().iter().any(|r| r.editing));
    ui.press("escape");
    assert!(!ui.rows().iter().any(|r| r.editing));
    assert_eq!(
        std::fs::read_to_string(fixture.project.join("src/lib.rs")).unwrap(),
        "pub fn f() {}\n"
    );

    // Delete asks first, naming the file; Escape keeps it.
    ui.right_click_row("README.md");
    ui.click("file-menu-delete");
    assert!(ui.shows("file-delete-confirm"));
    let pending = ui.cx.read(|cx| tree.read(cx).pending_delete().cloned());
    assert_eq!(pending.unwrap().target.path, "README.md");
    ui.press("escape");
    assert!(!ui.shows("file-delete-confirm"));
    assert!(fixture.project.join("README.md").exists());
    // The Delete key asks too; confirmed, it goes to the (test daemon's)
    // trash.
    ui.press("delete");
    assert!(ui.shows("file-delete-confirm"));
    ui.click("file-delete-ok");
    ui.wait_for("README.md to go", |ui| {
        !ui.names().contains(&"README.md".to_owned())
    });
    assert!(!fixture.project.join("README.md").exists());
    assert!(fixture.daemon.trash().join("README.md").is_file());

    // Deleting a file with unsaved edits says so, and the tab keeps them.
    ui.right_click_row("app.rs");
    ui.click("file-menu-delete");
    let pending = ui.cx.read(|cx| tree.read(cx).pending_delete().cloned());
    assert_eq!(pending.unwrap().dirty, ["app.rs"]);
    assert!(ui.shows("file-delete-unsaved"));
    ui.press("enter");
    ui.wait_for("app.rs to go", |ui| {
        !ui.names().contains(&"  app.rs".to_owned())
    });
    assert!(ui.dirty(&main));
    assert_eq!(
        ui.cx.read(|cx| main.read(cx).notice().cloned()),
        Some(super::editor::Notice::Deleted)
    );
    // A clean tab of a deleted file says it's gone.
    let notes = ui.tab_at("src/notes.md").unwrap();
    ui.right_click_row("notes.md");
    ui.click("file-menu-delete");
    ui.press("enter");
    ui.wait_for("notes.md's tab to say it's gone", |ui| {
        ui.content(&notes) == Content::Missing
    });

    // Drag a row onto a folder to move it there.
    let from = ui.row_index("logo.png");
    let to = ui.row_index("src");
    ui.frame(|window, cx| window.drag_to(("file-row", from), ("file-row", to), cx));
    ui.wait_for("logo.png in src", |ui| {
        ui.names().contains(&"  logo.png".to_owned())
    });
    assert!(fixture.project.join("src/logo.png").is_file());
    assert!(!fixture.project.join("logo.png").exists());

    // New Folder from the empty space below the rows, then Copy Path.
    ui.right_click_row("lib.rs");
    ui.click("file-menu-copy-relative-path");
    assert_eq!(
        ui.cx
            .read_from_clipboard()
            .and_then(|c| c.text())
            .as_deref(),
        Some("src/lib.rs")
    );
    ui.right_click_row("lib.rs");
    ui.click("file-menu-copy-path");
    let copied = ui.cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(
        std::fs::canonicalize(&copied).unwrap(),
        std::fs::canonicalize(fixture.project.join("src/lib.rs")).unwrap()
    );
    ui.right_click_row("src");
    ui.click("file-menu-new-folder");
    ui.enter_name("docs/guides");
    ui.wait_for("the new folders, open", |ui| {
        let names = ui.names();
        names.contains(&"    docs".to_owned()) || names.contains(&"  docs".to_owned())
    });
    assert!(fixture.project.join("src/docs/guides").is_dir());
    assert_eq!(ui.selected_path().as_deref(), Some("src/docs/guides"));
}

/// An agent's question on a file stays on it when the tree renames the
/// file (issue #288): the daemon moves the question, and the renamed tab
/// shows its marker on the same line.
#[test]
fn a_question_follows_its_file_through_a_rename() {
    use coqo_protocol::methods::questions::{self as qm, CreateParams, NewAnchor, QuestionKind};

    let fixture = Fixture::new();
    let (mut ui, main) = open_main(&fixture);
    let (ws, root) = ui.workspace_id();
    let agent = OtherWindow::connect(&fixture).client;
    let params = CreateParams {
        workspace: ws.clone(),
        session: "s-test".into(),
        kind: QuestionKind::Clarification,
        body: "Why print?".into(),
        options: Vec::new(),
        severity: None,
        anchors: vec![NewAnchor {
            root,
            path: "src/main.rs".into(),
            lines: edm::LineRange { start: 1, end: 1 },
        }],
    };
    let q = fixture
        .daemon
        .runtime
        .block_on(async move { agent.request::<qm::Create>(params).await })
        .unwrap();
    let markers = |ui: &mut Ui, tab: &Entity<FileEditor>| {
        let code = ui.code(tab).unwrap();
        ui.cx.read(|cx| code.read(cx).marker_lines(cx))
    };
    ui.wait_for("the marker on main.rs", |ui| {
        markers(ui, &main).iter().any(|(_, lines)| *lines == (1..2))
    });

    // Rename it in the tree: the tab follows, and so does the question.
    ui.click_row("src");
    ui.press("right");
    ui.press("right");
    ui.press("down");
    assert_eq!(ui.selected_path().as_deref(), Some("src/main.rs"));
    ui.press("f2");
    ui.enter_name("app.rs");
    ui.wait_for("the tab and the question on app.rs", |ui| {
        ui.cx.read(|cx| main.read(cx).path() == "src/app.rs")
            && ui.cx.update(|cx| {
                crate::questions::questions_for(&ws, cx).is_some_and(|store| {
                    store
                        .read(cx)
                        .get(q.id)
                        .is_some_and(|q| q.anchors[0].anchor.path == "src/app.rs")
                })
            })
    });
    ui.wait_for("the marker on app.rs", |ui| {
        markers(ui, &main).iter().any(|(_, lines)| *lines == (1..2))
    });
}

impl Ui {
    fn toast(&mut self) -> Option<String> {
        let tree = self.tree();
        self.cx.read(|cx| tree.read(cx).toast().map(str::to_owned))
    }

    /// Wait until the undo toast says `text` (the step is done in the UI,
    /// not only on disk).
    fn wait_for_toast(&mut self, text: &str) {
        self.wait_for(text, |ui| ui.toast().as_deref() == Some(text));
    }

    fn next_undo(&mut self) -> Option<super::history::FileOp> {
        let tree = self.tree();
        self.cx
            .read(|cx| tree.read(cx).history().next_undo().cloned())
    }

    /// Put the keyboard in the tree with `path` selected: click `src`
    /// (twice, to leave it open), then move down to it.
    fn select_in_tree(&mut self, path: &str) {
        self.click_row("src");
        self.click_row("src");
        for _ in 0..20 {
            if self.selected_path().as_deref() == Some(path) {
                return;
            }
            self.press("down");
        }
        panic!("cannot select {path}: {:?}", self.selected_path());
    }
}

#[cfg(target_os = "macos")]
const REDO_SHIFT: &str = "cmd-shift-z";
#[cfg(not(target_os = "macos"))]
const REDO_SHIFT: &str = "ctrl-shift-z";

/// Undo and redo in the focused tree (issue #291): a rename (the tab
/// follows back), a move, a delete restored from the (test daemon's)
/// trash, a new empty file removed, a changed one only after asking, and
/// an operation that can't be undone greyed with its reason. `Ctrl Z` in
/// the file's editor or the tree's name field is text undo, never a file
/// operation.
#[test]
fn undoes_and_redoes_file_operations_in_the_tree() {
    use super::history::FileOp;

    let fixture = Fixture::new();
    let (mut ui, main) = open_main(&fixture);
    let tree = ui.tree();

    // Rename main.rs to app.rs, then undo: back, with the tab.
    ui.click_row("src");
    ui.press("right");
    ui.press("right");
    ui.press("down");
    assert_eq!(ui.selected_path().as_deref(), Some("src/main.rs"));
    ui.press("f2");
    ui.enter_name("app.rs");
    ui.wait_for("app.rs", |ui| {
        ui.cx.read(|cx| main.read(cx).path() == "src/app.rs")
    });
    // In the name field, Ctrl Z is the field's own.
    ui.press("f2");
    assert!(ui.rows().iter().any(|r| r.editing));
    ui.press(UNDO);
    assert!(ui.rows().iter().any(|r| r.editing), "still naming");
    assert!(
        fixture.project.join("src/app.rs").is_file(),
        "nothing undone"
    );
    ui.press("escape");
    ui.press(UNDO);
    ui.wait_for("main.rs back", |ui| {
        ui.cx.read(|cx| main.read(cx).path() == "src/main.rs")
            && ui.names().contains(&"  main.rs".to_owned())
    });
    assert!(fixture.project.join("src/main.rs").is_file());
    assert!(!fixture.project.join("src/app.rs").exists());
    assert_eq!(
        ui.toast().as_deref(),
        Some("Undid rename of “main.rs” → “app.rs”")
    );
    assert!(ui.shows("file-undo-toast"));
    // Redo with Ctrl Shift Z, undo again, redo with the other key.
    ui.press(REDO_SHIFT);
    ui.wait_for("app.rs again", |ui| {
        ui.cx.read(|cx| main.read(cx).path() == "src/app.rs")
    });
    assert_eq!(
        ui.toast().as_deref(),
        Some("Redid rename of “main.rs” → “app.rs”")
    );
    ui.press(UNDO);
    ui.wait_for("main.rs again", |ui| {
        ui.cx.read(|cx| main.read(cx).path() == "src/main.rs")
    });
    ui.press(REDO);
    ui.wait_for("app.rs once more", |ui| {
        ui.cx.read(|cx| main.read(cx).path() == "src/app.rs")
    });
    ui.press(UNDO);
    ui.wait_for("main.rs once more", |ui| {
        ui.cx.read(|cx| main.read(cx).path() == "src/main.rs")
    });

    // Ctrl Z in the editor undoes typing, not the rename.
    ui.type_at(&main, 0, "// x\n");
    ui.wait_for("the edit", |ui| ui.dirty(&main));
    let before = ui.next_undo();
    ui.edit(&main, |editor, window, cx| editor.focus(window, cx));
    ui.press(UNDO);
    ui.wait_for("the typing undone", |ui| {
        ui.text(&main).as_deref() == Some(MAIN_RS)
    });
    assert_eq!(ui.next_undo(), before, "the tree's history is untouched");
    assert!(fixture.project.join("src/main.rs").is_file());

    // A move by drag and drop, undone.
    let from = ui.row_index("logo.png");
    let to = ui.row_index("src");
    ui.frame(|window, cx| window.drag_to(("file-row", from), ("file-row", to), cx));
    ui.wait_for("logo.png in src", |_| {
        fixture.project.join("src/logo.png").is_file()
    });
    ui.wait_for("the move in the tree", |ui| {
        ui.names().contains(&"  logo.png".to_owned())
    });
    ui.press(UNDO);
    ui.wait_for_toast("Undid move of “logo.png” to “src”");
    assert!(fixture.project.join("logo.png").is_file());

    // A delete, restored from the trash; redo deletes it again.
    ui.right_click_row("README.md");
    ui.click("file-menu-delete");
    ui.press("enter");
    ui.wait_for("README.md in the trash", |_| {
        fixture.daemon.trash().join("README.md").is_file()
    });
    ui.wait_for("README.md out of the tree", |ui| {
        !ui.names().contains(&"README.md".to_owned())
    });
    ui.press(UNDO);
    ui.wait_for("README.md restored", |ui| {
        ui.names().contains(&"README.md".to_owned())
    });
    assert_eq!(
        std::fs::read_to_string(fixture.project.join("README.md")).unwrap(),
        "# Demo\n"
    );
    // Out of the trash; on macOS, Coqo's copy came back instead and the
    // trashed one stays.
    assert_eq!(
        fixture.daemon.trash().join("README.md").exists(),
        cfg!(target_os = "macos")
    );
    assert_eq!(ui.toast().as_deref(), Some("Undid delete of “README.md”"));
    ui.press(REDO);
    ui.wait_for_toast("Redid delete of “README.md”");
    ui.wait_for("README.md deleted again", |_| {
        !fixture.project.join("README.md").exists()
    });
    ui.press(UNDO);
    ui.wait_for_toast("Undid delete of “README.md”");
    ui.wait_for("README.md restored again", |_| {
        fixture.project.join("README.md").is_file()
    });

    // A new, empty file: undo removes it (its tab says it's gone); redo
    // makes it again.
    ui.right_click_row("src");
    ui.click("file-menu-new-file");
    ui.wait_for("the name field", |ui| ui.rows().iter().any(|r| r.editing));
    ui.enter_name("notes.md");
    ui.wait_for("notes.md open", |ui| ui.tab_at("src/notes.md").is_some());
    // Maybe still loading (#312): it ends as "File not found" all the same.
    let notes = ui.tab_at("src/notes.md").unwrap();
    ui.select_in_tree("src/notes.md");
    ui.press(UNDO);
    ui.wait_for("notes.md gone", |ui| {
        !fixture.project.join("src/notes.md").exists()
            && ui.content(&notes) == Content::Missing
            && !ui.cx.read(|cx| notes.read(cx).is_opening())
    });
    assert!(ui.code(&notes).is_none(), "no buffer behind the message");
    assert!(
        !fixture.daemon.trash().join("notes.md").exists(),
        "an empty file is just removed"
    );
    assert_eq!(ui.toast().as_deref(), Some("Undid creation of “notes.md”"));
    ui.press(REDO);
    ui.wait_for_toast("Redid creation of “notes.md”");
    ui.wait_for("notes.md again", |_| {
        fixture.project.join("src/notes.md").is_file()
    });

    // Changed since: undo asks first, and Escape keeps it (still undoable).
    write(&fixture.project, "src/notes.md", "# Notes\n");
    ui.press(UNDO);
    ui.wait_for("the question", |ui| ui.shows("file-delete-confirm"));
    let pending = ui
        .cx
        .read(|cx| tree.read(cx).pending_delete().cloned())
        .unwrap();
    assert!(pending.undo.is_some());
    ui.press("escape");
    assert!(fixture.project.join("src/notes.md").is_file());
    assert!(matches!(ui.next_undo(), Some(FileOp::Create { .. })));
    ui.press(UNDO);
    ui.wait_for("the question again", |ui| ui.shows("file-delete-confirm"));
    ui.press("enter");
    ui.wait_for_toast("Undid creation of “notes.md”");
    ui.wait_for("notes.md in the trash", |_| {
        !fixture.project.join("src/notes.md").exists()
            && fixture.daemon.trash().join("notes.md").is_file()
    });

    // What can't be undone (deleted for good) shows greyed in the menu,
    // with why; the key says so and drops it.
    let root = ui.rows()[0].root;
    ui.cx.update(|cx| {
        tree.update(cx, |tree, _| {
            tree.record_for_tests(FileOp::Delete {
                root,
                path: "gone.txt".into(),
                kind: EntryKind::File,
                restore: Err("“gone.txt” was deleted permanently.".into()),
            })
        })
    });
    ui.right_click_row("README.md");
    assert!(ui.shows("file-menu-undo"));
    assert!(ui.shows("file-menu-undo-why"));
    ui.press("escape");
    ui.select_in_tree("README.md");
    ui.press(UNDO);
    assert_eq!(
        ui.toast().as_deref(),
        Some("Can't undo the delete of “gone.txt”. “gone.txt” was deleted permanently.")
    );
    assert_eq!(ui.next_undo(), None, "dropped");
}

impl Ui {
    /// Hold every tab's open back (the tabs stay loading) until
    /// [`release_open`](Self::release_open).
    fn hold_opens(&mut self, hold: bool) {
        self.cx.update(|cx| {
            if hold {
                cx.set_global(HoldOpens);
            } else {
                cx.remove_global::<HoldOpens>();
            }
        });
    }

    /// Let the tab's held open finish.
    fn release_open(&mut self, tab: &Entity<FileEditor>) {
        self.frame(|window, cx| tab.update(cx, |tab, cx| tab.release_open(window, cx)));
    }

    /// Open `name`'s row (its folder expanded) with opens held back, and
    /// wait until the daemon opened its buffer: the tab is still loading.
    fn open_held(&mut self, name: &str, path: &str) -> (Entity<FileEditor>, BufferId, ReplicaId) {
        self.hold_opens(true);
        self.click_row(name);
        self.wait_for("the open held", |ui| {
            ui.tab_at(path)
                .is_some_and(|tab| ui.cx.read(|cx| tab.read(cx).holds_open()))
        });
        let tab = self.tab_at(path).unwrap();
        assert_eq!(self.content(&tab), Content::Loading);
        let (buffer, replica) = self
            .cx
            .read(|cx| tab.read(cx).held_buffer())
            .expect("the daemon opened the buffer");
        (tab, buffer, replica)
    }

    /// Delete `name` from the tree (a file in an expanded folder).
    fn delete_row(&mut self, fixture: &Fixture, name: &str) {
        self.right_click_row(name);
        self.click("file-menu-delete");
        self.press("enter");
        self.wait_for("the file in the trash", |_| {
            fixture.daemon.trash().join(name).is_file()
        });
    }
}

/// Whether `replica` is still in the daemon's buffer of `path`: it made
/// edits (see [`edit_as`]), and the daemon forgets a replica's edits when it
/// closes. The file must be on disk.
fn replica_joined(
    fixture: &Fixture,
    other: &OtherWindow,
    place: &Place,
    path: &str,
    replica: &ReplicaId,
) -> bool {
    let params = OpenParams {
        workspace: place.0.clone(),
        root: place.1,
        path: path.to_owned(),
        replica: Some(replica.clone()),
    };
    let client = other.client.clone();
    let open = fixture
        .daemon
        .runtime
        .block_on(async move { client.request::<buffer::Open>(params).await })
        .unwrap();
    open.last_seq > 0
}

/// Two edits by `replica` that cancel out, so the daemon knows the replica
/// (its last seq) and the buffer stays clean.
fn edit_as(
    fixture: &Fixture,
    other: &OtherWindow,
    buffer: BufferId,
    replica: &ReplicaId,
    rev: u64,
) {
    let client = other.client.clone();
    let edits = [
        (rev, 1, TextEdit::insert(0, "x")),
        (rev + 1, 2, TextEdit::delete(0..1)),
    ]
    .map(|(base_rev, client_seq, edit)| EditParams {
        buffer,
        replica: replica.clone(),
        base_rev,
        client_seq,
        edits: vec![edit],
    });
    fixture.daemon.runtime.block_on(async move {
        for params in edits {
            client.notify::<buffer::Edit>(params).await.unwrap();
        }
    });
}

/// A tab whose file is deleted while it loads (#312): the open in flight
/// already got the buffer, and the delete comes before its outcome. The tab
/// ends as "File not found", like a clean loaded tab, with the buffer it got
/// closed again and nothing reopened. Undoing the delete opens it cleanly.
#[test]
fn a_tab_deleted_while_it_loads_says_the_file_is_gone() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    let place = ui.workspace_id();
    ui.click_row("src");
    ui.wait_for("src open", |ui| ui.names().contains(&"  lib.rs".to_owned()));
    let (tab, buffer, replica) = ui.open_held("lib.rs", "src/lib.rs");
    let other = OtherWindow::connect(&fixture);
    let rev = other.open(&fixture, &place, "src/lib.rs").rev;
    edit_as(&fixture, &other, buffer, &replica, rev);
    ui.wait_for("the replica's edits", |_| {
        replica_joined(&fixture, &other, &place, "src/lib.rs", &replica)
    });

    ui.delete_row(&fixture, "lib.rs");
    ui.wait_for("the tab told", |ui| ui.content(&tab) == Content::Missing);
    ui.hold_opens(false);
    ui.release_open(&tab);
    ui.advance(Duration::from_millis(500));
    ui.frame(|_, _| ());
    assert_eq!(ui.content(&tab), Content::Missing);
    ui.cx.read(|cx| {
        let tab = tab.read(cx);
        assert!(tab.editor().is_none(), "no buffer behind the message");
        assert!(!tab.is_opening(), "nothing reopens");
        assert!(!tab.is_subscribed());
    });

    // Undone: the file is back, and so is the tab.
    ui.press(UNDO);
    ui.wait_for_toast("Undid delete of “lib.rs”");
    ui.wait_for("the tab open again", |ui| {
        ui.text(&tab).as_deref() == Some("pub fn f() {}\n")
    });
    assert_eq!(ui.content(&tab), Content::Text);
    assert!(ui.cx.read(|cx| tab.read(cx).is_subscribed()));
    assert!(!ui.dirty(&tab));
    // The stale open's replica left the buffer (the file can be opened by
    // path again to ask).
    ui.wait_for("the stale replica closed in the daemon", |_| {
        !replica_joined(&fixture, &other, &place, "src/lib.rs", &replica)
    });
    assert_ne!(ui.cx.read(|cx| tab.read(cx).replica()), Some(replica));
}

/// A delete and its undo, both while the tab loads (#312): the stale open's
/// buffer is closed and the tab opens the restored file afresh.
#[test]
fn a_tab_deleted_and_restored_while_it_loads_opens_the_file() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    let (tab, _, replica) = ui.open_held("README.md", "README.md");

    ui.delete_row(&fixture, "README.md");
    ui.wait_for("the tab told", |ui| ui.content(&tab) == Content::Missing);
    ui.press(UNDO);
    ui.wait_for_toast("Undid delete of “README.md”");
    assert_eq!(ui.content(&tab), Content::Loading, "it reopens");
    ui.hold_opens(false);
    ui.release_open(&tab);
    ui.wait_for("the tab open", |ui| {
        ui.text(&tab).as_deref() == Some("# Demo\n")
    });
    assert_eq!(ui.content(&tab), Content::Text);
    let fresh = ui.cx.read(|cx| tab.read(cx).replica()).expect("a session");
    assert_ne!(fresh, replica, "a fresh replica, not the stale open's");
    assert!(ui.cx.read(|cx| tab.read(cx).is_subscribed()));
    assert!(!ui.dirty(&tab));
}

/// Renders the workbench with this repository open, `crates/coqo/src/files`
/// expanded and a file tab open, offscreen to PNGs. Needs a GPU:
/// `cargo test -p coqo -- --ignored files_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn files_screenshots() {
    use std::sync::Arc;

    use coqo_ui::{
        CoqoAssets,
        gpui::{HeadlessAppContext, platform},
    };

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf();
    let fixture = Fixture::new();
    // Name the workspace `coqo`, whatever the checkout's folder is called.
    let (paths, folder) = (fixture.daemon.paths.clone(), repo.clone());
    fixture.daemon.runtime.block_on(async move {
        let (client, _) = connect(&paths, ClientConfig::new("coqo-tests", "test"))
            .await
            .unwrap();
        client
            .request::<coqo_protocol::methods::workspace::Open>(
                coqo_protocol::methods::workspace::OpenParams {
                    path: Some(folder.to_string_lossy().into_owned()),
                    name: Some("coqo".into()),
                    id: None,
                },
            )
            .await
            .unwrap();
    });
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let mut cx = HeadlessAppContext::with_platform(
            platform::current_platform(true).text_system(),
            Arc::new(CoqoAssets),
            platform::current_headless_renderer,
        );
        cx.allow_parking();
        let paths = fixture.daemon.paths.clone();
        let repo = repo.clone();
        let (handle, shell) = cx
            .update(|cx| {
                coqo_ui::init(appearance, cx);
                let mut config = DaemonConfig::at(paths);
                config.spawn = false;
                daemon::init(config, cx);
                workspace::init(Some(repo), cx);
                crate::shell::init(cx);
                coqo_ui::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Point::default(),
                            size: size(px(1440.), px(900.)),
                        })),
                        focus: false,
                        show: false,
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        let store = Rc::new(MemoryLayoutStore::default());
                        cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
                    },
                )
            })
            .expect("open window");
        let settle = |cx: &mut HeadlessAppContext, done: &dyn Fn(&App) -> bool| {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                    .unwrap();
                cx.run_until_parked();
                if cx.update(|cx| done(cx)) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let tree = |cx: &App| {
            shell
                .read(cx)
                .sidebar_pane("files")
                .and_then(|v| v.downcast::<FileTree>().ok())
        };
        settle(&mut cx, &|cx| {
            tree(cx).is_some_and(|t| !t.read(cx).rows().is_empty())
        });
        // Expand crates → coqo → src → files, then open glyph.rs.
        for (step, target) in ["crates", "coqo", "src", "files", "glyph.rs"]
            .into_iter()
            .enumerate()
        {
            let tree = cx.update(|cx| tree(cx)).unwrap();
            let ix = cx.update(|cx| {
                tree.read(cx)
                    .rows()
                    .iter()
                    .position(|r| r.name.as_ref() == target && r.depth == step)
                    .unwrap_or_else(|| panic!("no row {target}"))
            });
            cx.update_window(handle, |_, window, cx| {
                tree.update(cx, |t, cx| t.activate(ix, window, cx))
            })
            .unwrap();
            settle(&mut cx, &|cx| {
                let rows = tree.read(cx).rows();
                rows.get(ix)
                    .is_some_and(|r| r.expanded || r.kind == EntryKind::File)
                    && rows
                        .get(ix + 1)
                        .is_some_and(|r| r.depth > step || target == "glyph.rs")
            });
        }
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        settle(&mut cx, &|cx| {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
                .is_some_and(|tab| matches!(tab.read(cx).content(), Content::Text))
        });
        // Let the background highlighter finish.
        for _ in 0..20 {
            settle(&mut cx, &|_| true);
            std::thread::sleep(Duration::from_millis(20));
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-files-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());

        // The same editor with the `Accent` enum tinted as an agent's change
        // shows (`buffer/changes` with an agent origin).
        let editor = cx.update(|cx| {
            let dock = dock.read(cx);
            let tab = dock
                .active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
                .unwrap();
            tab.read(cx).editor().cloned().unwrap()
        });
        cx.update(|cx| {
            let text = editor.read(cx).text(cx);
            let start = text.find("/// An accent").expect("Accent in glyph.rs");
            let end = start + text[start..].find("\n}\n").expect("end of Accent") + 3;
            let sage = cx.tokens().palette.sage.hsla();
            editor.update(cx, |editor, cx| {
                editor.highlight_changes(&[start..end], sage, cx)
            });
        });
        for _ in 0..5 {
            settle(&mut cx, &|_| true);
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-editor-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());

        // The tree's context menu on glyph.rs, then a rename in place and a
        // delete waiting for confirmation (#161). Nothing is sent: the
        // folder is this repository.
        let tree = cx.update(|cx| tree(cx)).unwrap();
        let row_of = |cx: &mut HeadlessAppContext, name: &str| {
            cx.update(|cx| {
                tree.read(cx)
                    .rows()
                    .iter()
                    .position(|r| r.name.as_ref() == name && r.depth == 4)
                    .unwrap_or_else(|| panic!("no row {name}"))
            })
        };
        let files = "crates/coqo/src/files";
        let root = cx.update(|cx| tree.read(cx).rows()[0].root);
        let glyph = row_of(&mut cx, "glyph.rs");
        let at = cx
            .update_window(handle, |_, window, _| {
                let bounds = window.find(("file-row", glyph)).bounds();
                coqo_ui::gpui::point(bounds.left() + px(64.), bounds.center().y)
            })
            .unwrap();
        cx.update(|cx| {
            tree.update(cx, |t, cx| {
                let target = super::tree::Target {
                    root,
                    path: format!("{files}/glyph.rs"),
                    kind: EntryKind::File,
                };
                t.open_menu(target, at, cx)
            })
        });
        for _ in 0..5 {
            settle(&mut cx, &|_| true);
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-files-menu-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());

        cx.update_window(handle, |_, window, cx| {
            tree.update(cx, |t, cx| {
                t.start_rename(root, &format!("{files}/context.rs"), window, cx);
                let target = super::tree::Target {
                    root,
                    path: format!("{files}/tests.rs"),
                    kind: EntryKind::File,
                };
                t.request_delete(target, window, cx);
            })
        })
        .unwrap();
        for _ in 0..5 {
            settle(&mut cx, &|_| true);
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-files-ops-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
