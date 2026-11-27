// SPDX-License-Identifier: Apache-2.0

//! The file tree, file tabs, workspaces and the daemon connection against a
//! real `coqod` served in-process on a temporary socket and data directory.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_client::{DaemonPaths, connect};
use coqo_protocol::{
    ClientConfig,
    daemon::{Shutdown, ShutdownParams},
    methods::fs::EntryKind,
};
use coqo_storage::DataDirs;
use coqo_ui::{
    Appearance,
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Keystroke, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use coqod::IdlePolicy;
use tempfile::TempDir;
use tokio::{runtime::Runtime, task::JoinHandle};

use super::{
    preview::{Content, FilePreview, KIND},
    tree::{FileTree, Row},
};
use crate::{
    daemon::{self, ConnectionState, Daemon, DaemonConfig},
    shell::{DEFAULT_WORKSPACE, Shell},
    workspace::{self, Workspaces},
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A `coqod` served on a background runtime.
struct TestDaemon {
    runtime: Runtime,
    paths: DaemonPaths,
    data: PathBuf,
    serving: Option<JoinHandle<()>>,
}

impl TestDaemon {
    fn start(run: PathBuf, data: PathBuf) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut daemon = Self {
            runtime,
            paths: DaemonPaths::at(run),
            data,
            serving: None,
        };
        daemon.serve();
        daemon
    }

    fn serve(&mut self) {
        let (paths, data) = (self.paths.clone(), DataDirs::at(&self.data));
        // Binding needs the runtime's reactor.
        let _entered = self.runtime.enter();
        let lock = coqod::daemon::acquire(&paths).unwrap();
        let listener = coqod::daemon::bind(&paths).unwrap();
        let policy = IdlePolicy {
            stop_services_after: None,
            exit_after: None,
        };
        self.serving = Some(self.runtime.spawn(async move {
            coqod::daemon::serve(paths, data, policy, lock, listener)
                .await
                .unwrap();
        }));
    }

    /// `daemon/shutdown`, and wait until it stopped.
    fn stop(&mut self) {
        let paths = self.paths.clone();
        let serving = self.serving.take().expect("serving");
        self.runtime.block_on(async move {
            let (client, _) = connect(&paths, ClientConfig::new("coqo-tests", "test"))
                .await
                .unwrap();
            client
                .request::<Shutdown>(ShutdownParams { force: true })
                .await
                .unwrap();
            serving.await.unwrap();
        });
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            serving.abort();
        }
    }
}

/// A project folder and a daemon.
struct Fixture {
    _tmp: TempDir,
    project: PathBuf,
    daemon: TestDaemon,
}

fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo-project");
        write(&project, ".gitignore", "target/\n");
        write(&project, "README.md", "# Demo\n");
        write(
            &project,
            "src/main.rs",
            "fn main() {\n\tprintln!(\"hi\");\n}\n",
        );
        write(&project, "src/lib.rs", "pub fn f() {}\n");
        write(&project, "target/out.txt", "build output\n");
        write(
            &project,
            "logo.png",
            [0x89, b'P', b'N', b'G', 0x00, 0xff, 0xfe, 0x00],
        );
        let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
        Self {
            _tmp: tmp,
            project,
            daemon,
        }
    }
}

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

    fn file_tabs(&mut self) -> Vec<Entity<FilePreview>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<FilePreview>())
                .collect()
        })
    }

    fn content(&mut self, tab: &Entity<FilePreview>) -> Content {
        self.cx.read(|cx| tab.read(cx).content().clone())
    }

    fn wait_for_tree(&mut self) {
        self.wait_for("the tree", |ui| !ui.rows().is_empty());
    }
}

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

#[test]
fn opens_files_in_preview_tabs() {
    let fixture = Fixture::new();
    write(&fixture.project, "huge.txt", "a".repeat(5 * 1024 * 1024));
    let mut ui = open_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for_tree();
    ui.click_row("src");
    ui.wait_for("src to expand", |ui| ui.names().len() == 8);

    ui.click_row("main.rs");
    ui.wait_for("the text", |ui| {
        let tabs = ui.file_tabs();
        tabs.len() == 1 && matches!(ui.content(&tabs[0]), Content::Text(_))
    });
    let tab = ui.file_tabs()[0].clone();
    let Content::Text(lines) = ui.content(&tab) else {
        unreachable!()
    };
    let lines: Vec<&str> = lines.iter().map(|l| l.as_ref()).collect();
    assert_eq!(lines, ["fn main() {", "    println!(\"hi\");", "}"]);
    ui.frame(|window, _| assert!(window.find("file-preview").visible()));

    // Opening it again shows the same tab.
    ui.click_row("main.rs");
    assert_eq!(ui.file_tabs().len(), 1);

    ui.click_row("logo.png");
    ui.wait_for("the binary tab", |ui| {
        let tabs = ui.file_tabs();
        tabs.len() == 2 && matches!(ui.content(&tabs[1]), Content::Binary { .. })
    });
    ui.frame(|window, _| assert!(window.find("file-preview-message").visible()));

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
    ui.click_row("main.rs");
    ui.wait_for("the text", |ui| {
        let tabs = ui.file_tabs();
        tabs.len() == 1 && matches!(ui.content(&tabs[0]), Content::Text(_))
    });

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
    ui.wait_for(
        "the tab to reload",
        |ui| matches!(ui.content(&tab), Content::Text(lines) if lines.len() == 1),
    );
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
                .and_then(|id| dock.tab(id)?.downcast::<FilePreview>())
                .is_some_and(|tab| matches!(tab.read(cx).content(), Content::Text(_)))
        });
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-files-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
