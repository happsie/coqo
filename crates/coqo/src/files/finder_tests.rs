// SPDX-License-Identifier: Apache-2.0

//! The file finder in a workbench window against a real `coqod`: `Ctrl P`,
//! typing, Enter, `path:line`, recently opened files and `Ctrl Enter` into a
//! split (#162). The harness serves the file commands' tests too.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_ui::{
    Appearance,
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Window, WindowBounds,
        WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::{
    editor::{FileEditor, KIND},
    finder::FIND_FILE_KEY,
};
use crate::{
    commands::PaletteItem,
    daemon::{self, DaemonConfig},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{Fixture, TIMEOUT, write},
    workspace,
};

pub(super) struct Ui {
    pub(super) cx: TestAppContext,
    pub(super) window: AnyWindowHandle,
    pub(super) shell: Entity<Shell>,
}

pub(super) fn open_ui(fixture: &Fixture) -> Ui {
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let paths = fixture.daemon.paths.clone();
    let folder = fixture.project.clone();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(Some(folder), cx);
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
    let mut ui = Ui { cx, window, shell };
    ui.wait_for("the workspace", |ui| {
        ui.cx.read(|cx| {
            workspace::Workspaces::global(cx).is_some_and(|w| w.read(cx).current().is_some())
        })
    });
    ui
}

impl Ui {
    pub(super) fn frame<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx
            .update_window(self.window, |_, window, cx| {
                window.render_frame(cx);
                f(window, cx)
            })
            .unwrap()
    }

    pub(super) fn settle(&mut self) {
        for _ in 0..3 {
            self.cx.run_until_parked();
            self.frame(|_, _| ());
        }
    }

    pub(super) fn wait_for(&mut self, what: &str, done: impl Fn(&mut Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.settle();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn press(&mut self, keys: &str) {
        for k in keys.split_whitespace() {
            self.frame(|window, cx| window.press(k, cx));
            self.cx.run_until_parked();
        }
        self.settle();
    }

    pub(super) fn type_text(&mut self, text: &str) {
        self.cx.simulate_input(self.window, text);
        self.settle();
    }

    pub(super) fn palette_open(&mut self) -> bool {
        self.cx.read(|cx| self.shell.read(cx).palette().is_some())
    }

    pub(super) fn items(&mut self) -> Vec<PaletteItem> {
        self.cx.read(|cx| {
            self.shell
                .read(cx)
                .palette()
                .map(|p| p.read(cx).items().to_vec())
                .unwrap_or_default()
        })
    }

    pub(super) fn titles(&mut self) -> Vec<String> {
        self.items().iter().map(|i| i.title.to_string()).collect()
    }

    /// Wait until the first row is `title`.
    pub(super) fn wait_for_first(&mut self, title: &str) -> PaletteItem {
        self.wait_for(&format!("{title} first"), |ui| {
            ui.items().first().is_some_and(|i| i.title == title)
        });
        self.items().remove(0)
    }

    /// The active tab, if it's a file tab.
    pub(super) fn active_file(&mut self) -> Option<Entity<FileEditor>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
        })
    }

    pub(super) fn active_path(&mut self) -> Option<String> {
        let tab = self.active_file()?;
        Some(self.cx.read(|cx| tab.read(cx).path().to_owned()))
    }

    pub(super) fn file_tabs(&mut self) -> usize {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            dock.read(cx)
                .tree()
                .tabs()
                .iter()
                .filter(|t| t.kind == KIND)
                .count()
        })
    }

    pub(super) fn groups(&mut self) -> usize {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| dock.read(cx).tree().groups().len())
    }
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("// line {i}\n")).collect()
}

#[cfg(target_os = "macos")]
const SPLIT_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
const SPLIT_KEY: &str = "ctrl-enter";

#[test]
fn ctrl_p_finds_and_opens_files() {
    let fixture = Fixture::new();
    write(&fixture.project, "src/deep/finder_target.rs", lines(60));
    write(&fixture.project, "target/finder_target.rs", "ignored\n");
    let mut ui = open_ui(&fixture);

    ui.press(FIND_FILE_KEY);
    assert!(ui.palette_open());
    let mode = ui.cx.read(|cx| {
        let palette = ui.shell.read(cx).palette().unwrap().read(cx);
        (palette.text(cx), palette.mode())
    });
    assert_eq!(mode, (String::new(), Some("")), "the default mode");
    // No query, nothing opened yet: the files nearest the root.
    ui.wait_for("files", |ui| ui.titles().contains(&"README.md".to_owned()));

    // Typing narrows it; the ignored copy under target/ isn't there.
    ui.type_text("fndtgt");
    let first = ui.wait_for_first("finder_target.rs");
    assert_eq!(first.detail.as_deref(), Some("src/deep"));
    assert_eq!(first.glyph.as_deref(), Some("finder_target.rs"));
    assert_eq!(first.highlights, [0, 2, 3, 7, 10, 12]);
    assert_eq!(ui.items().len(), 1, "{:?}", ui.titles());

    // Enter opens it in a file tab and closes the palette.
    ui.press("enter");
    assert!(!ui.palette_open());
    ui.wait_for("the file tab", |ui| {
        ui.active_path().as_deref() == Some("src/deep/finder_target.rs")
    });

    // `path:line` lands on the line.
    ui.press(FIND_FILE_KEY);
    ui.type_text("finder_target:42");
    ui.wait_for_first("finder_target.rs:42");
    ui.press("enter");
    ui.wait_for("line 42", |ui| {
        let tab = ui.active_file().unwrap();
        ui.cx.read(|cx| tab.read(cx).cursor_line(cx)) == Some(41)
    });
    assert_eq!(ui.file_tabs(), 1, "the open tab is reused");

    // Next time, with no query, it comes first as recently opened.
    ui.press(FIND_FILE_KEY);
    let first = ui.wait_for_first("finder_target.rs");
    assert!(first.recent);
    ui.press("escape");
    assert!(!ui.palette_open());

    // `Ctrl Enter` opens a file in a split to the right.
    assert_eq!(ui.groups(), 1);
    ui.press(FIND_FILE_KEY);
    ui.type_text("main.rs");
    ui.wait_for_first("main.rs");
    ui.press(SPLIT_KEY);
    assert!(!ui.palette_open());
    ui.wait_for("main.rs in a split", |ui| {
        ui.active_path().as_deref() == Some("src/main.rs")
    });
    assert_eq!(ui.groups(), 2);

    // The same key again closes the finder.
    ui.press(FIND_FILE_KEY);
    assert!(ui.palette_open());
    ui.press(FIND_FILE_KEY);
    assert!(!ui.palette_open());
}

#[test]
fn the_finder_follows_new_files() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture);
    ui.press(FIND_FILE_KEY);
    ui.type_text("brand_new");
    ui.wait_for("no match", |ui| ui.items().is_empty());
    ui.press("escape");
    write(&fixture.project, "docs/brand_new.md", "# New\n");
    // The daemon's index follows the watcher a moment later.
    let deadline = Instant::now() + TIMEOUT;
    let shown = |ui: &mut Ui| ui.titles().first().map(String::as_str) == Some("brand_new.md");
    loop {
        ui.press(FIND_FILE_KEY);
        ui.type_text("brand_new");
        // Give this query's answer time to come back, even under load.
        let answered = Instant::now() + Duration::from_secs(1);
        while !shown(&mut ui) && Instant::now() < answered {
            ui.settle();
            std::thread::sleep(Duration::from_millis(20));
        }
        if shown(&mut ui) {
            break;
        }
        ui.press("escape");
        assert!(Instant::now() < deadline, "the new file never showed");
        std::thread::sleep(Duration::from_millis(50));
    }
    ui.press("enter");
    ui.wait_for("the new file", |ui| {
        ui.active_path().as_deref() == Some("docs/brand_new.md")
    });
}

/// Renders the finder over this repository offscreen to PNGs for
/// docs/design/palette.md: with no query (a recently opened file first)
/// and with one. Needs a GPU: `cargo test -p coqo -- --ignored
/// finder_screenshots`. Writes to `$COQO_SHOT_DIR` (default: the system
/// temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn finder_screenshots() {
    use std::{path::PathBuf, sync::Arc};

    use coqo_client::connect;
    use coqo_protocol::{
        ClientConfig,
        methods::workspace::{Open, OpenParams},
    };
    use coqo_ui::{
        CoqoAssets,
        gpui::{HeadlessAppContext, platform},
    };

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
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
            .request::<Open>(OpenParams {
                path: Some(folder.to_string_lossy().into_owned()),
                name: Some("coqo".into()),
                id: None,
            })
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
        let current = |cx: &App| {
            workspace::Workspaces::global(cx).and_then(|w| w.read(cx).current().cloned())
        };
        settle(&mut cx, &|cx| current(cx).is_some());
        // A file tab, so the finder has a recently opened file.
        let info = cx.update(|cx| current(cx)).unwrap();
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        cx.update_window(handle, |_, window, cx| {
            super::finder::open_file(
                &dock,
                &info.id,
                info.roots[0].id,
                "crates/coqo/src/commands/palette.rs",
                None,
                None,
                false,
                window,
                cx,
            );
        })
        .unwrap();
        settle(&mut cx, &|cx| {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
                .is_some_and(|tab| tab.read(cx).editor().is_some())
        });
        for (query, suffix) in [("", ""), ("tree", "-query")] {
            cx.update_window(handle, |_, window, cx| {
                shell.update(cx, |shell, cx| {
                    if shell.palette().is_none() {
                        shell.toggle_palette("", window, cx);
                    }
                    let palette = shell.palette().unwrap().clone();
                    palette.update(cx, |p, cx| p.set_text(query, window, cx));
                })
            })
            .unwrap();
            settle(&mut cx, &|cx| {
                shell.read(cx).palette().is_some_and(|p| {
                    let p = p.read(cx);
                    p.items().len() > 5 && (query.is_empty() || p.items()[0].title.contains(query))
                })
            });
            for _ in 0..10 {
                settle(&mut cx, &|_| true);
                std::thread::sleep(Duration::from_millis(20));
            }
            let image = cx.capture_screenshot(handle).expect("screenshot");
            let path = dir.join(format!("coqo-finder{suffix}-{name}.png"));
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        }
    }
}
