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
        buffer::{self, EditParams, OpenParams, OpenResult, TextEdit},
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
    editor::{Content, FileEditor, HIGHLIGHT_FOR, KIND},
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
    }
}
