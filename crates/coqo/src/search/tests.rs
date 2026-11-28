// SPDX-License-Identifier: Apache-2.0

//! The Search pane against a real `coqod` (with the built-in search mod)
//! served in-process, on a folder of files.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_protocol::methods::{
    search::{SearchMatch, SearchSummary},
    workspace::RootId,
};
use coqo_ui::{
    Appearance,
    dock::store::{LayoutStore as _, MemoryLayoutStore, SidebarLayout},
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Window, WindowBounds,
        WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::{
    NextMatch, PANE, ShowSearch,
    pane::{FileResult, SearchPane, Status},
};
use crate::{
    daemon::{self, DaemonConfig},
    files::editor::FileEditor,
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{TIMEOUT, TestDaemon},
    workspace,
};

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    folder: PathBuf,
    _daemon: TestDaemon,
    _tmp: tempfile::TempDir,
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn open_ui(files: &[(&str, &str)]) -> Ui {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("project");
    for (rel, text) in files {
        write(&folder, rel, text);
    }
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let paths = daemon.paths.clone();
    let open = folder.clone();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(Some(open), cx);
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
    Ui {
        cx,
        window,
        shell,
        folder,
        _daemon: daemon,
        _tmp: tmp,
    }
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

    fn pane(&mut self) -> Entity<SearchPane> {
        self.frame(|_, _| ());
        self.cx
            .read(|cx| self.shell.read(cx).sidebar_pane(PANE))
            .and_then(|view| view.downcast::<SearchPane>().ok())
            .expect("the search pane is shown")
    }

    fn click(&mut self, id: impl Into<coqo_ui::gpui::ElementId>) {
        let id = id.into();
        self.frame(|window, cx| window.click(id, cx));
    }

    fn dispatch(&mut self, action: Box<dyn coqo_ui::gpui::Action>) {
        self.frame(|window, cx| window.dispatch_action(action, cx));
        self.cx.run_until_parked();
    }

    /// Type `query` and search at once.
    fn search(&mut self, query: &str) {
        let pane = self.pane();
        let query = query.to_owned();
        self.frame(|window, cx| {
            pane.update(cx, |pane, cx| {
                pane.query_input()
                    .update(cx, |q, cx| q.set_text(query, window, cx));
                pane.run(cx);
            })
        });
    }

    fn results(&mut self) -> Vec<(String, Vec<(u32, u32, u32)>)> {
        let pane = self.pane();
        self.cx.read(|cx| {
            pane.read(cx)
                .results()
                .iter()
                .map(|f| {
                    (
                        f.path.clone(),
                        f.matches.iter().map(|m| (m.line, m.start, m.end)).collect(),
                    )
                })
                .collect()
        })
    }

    fn done(&mut self) -> bool {
        let pane = self.pane();
        self.cx
            .read(|cx| matches!(pane.read(cx).status(), Status::Done(_)))
    }

    fn file_tab(&mut self, path: &str) -> Option<Entity<FileEditor>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
                .find(|f| f.read(cx).path() == path)
        })
    }

    fn tab_text(&mut self, tab: &Entity<FileEditor>) -> Option<String> {
        self.cx.read(|cx| {
            tab.read(cx)
                .text_and_editor()
                .map(|(text, _)| text.to_string())
        })
    }
}

#[test]
fn search_open_a_match_and_replace_all() {
    let mut ui = open_ui(&[
        (".gitignore", "build/\n"),
        ("src/main.rs", "fn main() {\n    greet(\"world\");\n}\n"),
        (
            "src/greet.rs",
            "pub fn greet(who: &str) {\n    println!(\"hi {who}\");\n}\n",
        ),
        ("build/out.rs", "greet\n"),
        ("README.md", "Say hi.\n"),
    ]);
    // Ctrl/Cmd+Shift+F shows the pane.
    ui.dispatch(Box::new(ShowSearch));
    ui.wait_for("the workspace", |ui| {
        let pane = ui.pane();
        ui.cx.read(|cx| {
            crate::workspace::Workspaces::global(cx)
                .and_then(|w| w.read(cx).current().cloned())
                .is_some()
                && !matches!(pane.read(cx).status(), Status::Failed(_))
        })
    });

    // Typing a query streams in the results, sorted by path; the ignored
    // build output is not searched.
    ui.search("greet");
    ui.wait_for("the results", |ui| ui.done());
    assert_eq!(
        ui.results(),
        vec![
            ("src/greet.rs".to_owned(), vec![(0, 7, 12)]),
            ("src/main.rs".to_owned(), vec![(1, 4, 9)]),
        ]
    );

    // Clicking a match opens its file with the match selected. Rows: the
    // greet.rs file, its match, main.rs, its match.
    ui.click(("search-match", 3usize));
    ui.wait_for("the file tab", |ui| {
        ui.file_tab("src/main.rs")
            .and_then(|t| ui.tab_text(&t))
            .is_some()
    });
    let main = ui.file_tab("src/main.rs").unwrap();
    ui.wait_for("the selection", |ui| {
        let main = main.clone();
        ui.cx.read(|cx| {
            main.read(cx)
                .text_and_editor()
                .map(|(_, e)| e.read(cx).selections(cx))
                == Some(vec![16..21])
        })
    });

    // F4 goes on to the next match, wrapping to the first.
    ui.dispatch(Box::new(NextMatch));
    ui.wait_for("greet.rs", |ui| ui.file_tab("src/greet.rs").is_some());
    let pane = ui.pane();
    assert_eq!(ui.cx.read(|cx| pane.read(cx).selected()), Some((0, 0)));

    // Replace all asks first, with the counts.
    let replace = ui.cx.read(|cx| pane.read(cx).replace_input().clone());
    ui.frame(|window, cx| {
        replace.update(cx, |r, cx| r.set_text("welcome", window, cx));
    });
    ui.click("search-replace-all");
    let confirm = ui
        .cx
        .read(|cx| pane.read(cx).confirm())
        .expect("a confirmation");
    assert_eq!((confirm.files, confirm.matches), (2, 2));
    ui.click("search-confirm-replace");
    ui.wait_for("the replace", |ui| {
        let pane = ui.pane();
        ui.cx.read(|cx| {
            let p = pane.read(cx);
            !p.is_replacing() && p.note().is_some()
        }) && ui.done()
    });
    assert_eq!(
        ui.cx.read(|cx| pane.read(cx).note().map(str::to_owned)),
        Some("Replaced 2 matches in 2 files".to_owned())
    );
    // Both files are open in tabs: the tabs took the edits, unsaved and
    // undoable; the disk is unchanged until they are saved.
    let greet = ui.file_tab("src/greet.rs").unwrap();
    ui.wait_for("the edits", |ui| {
        ui.tab_text(&main).as_deref() == Some("fn main() {\n    welcome(\"world\");\n}\n")
            && ui.tab_text(&greet).as_deref()
                == Some("pub fn welcome(who: &str) {\n    println!(\"hi {who}\");\n}\n")
    });
    assert!(ui.cx.read(|cx| main.read(cx).is_dirty()));
    assert_eq!(
        std::fs::read_to_string(ui.folder.join("src/main.rs")).unwrap(),
        "fn main() {\n    greet(\"world\");\n}\n"
    );
    // The search ran again: nothing left.
    assert!(ui.results().is_empty());
}

#[test]
fn replace_in_a_file_that_is_not_open_writes_it() {
    let mut ui = open_ui(&[("a.txt", "one two one\n"), ("b.txt", "one\n")]);
    ui.click("sidebar-search");
    ui.wait_for("the workspace", |ui| {
        ui.cx.read(|cx| {
            crate::workspace::Workspaces::global(cx)
                .and_then(|w| w.read(cx).current().cloned())
                .is_some()
        })
    });
    ui.search("one");
    ui.wait_for("the results", |ui| ui.done() && ui.results().len() == 2);
    let pane = ui.pane();
    let replace = ui.cx.read(|cx| pane.read(cx).replace_input().clone());
    ui.frame(|window, cx| replace.update(cx, |r, cx| r.set_text("1", window, cx)));
    // The second match of a.txt only, then the whole of b.txt.
    ui.click(("search-replace-match", 2usize));
    ui.wait_for("a.txt", |ui| {
        std::fs::read_to_string(ui.folder.join("a.txt")).unwrap() == "one two 1\n"
    });
    ui.wait_for("the search again", |ui| {
        ui.done() && ui.results().len() == 2 && ui.results()[0].1.len() == 1
    });
    ui.click(("search-replace-file", 1usize));
    ui.wait_for("b.txt", |ui| {
        std::fs::read_to_string(ui.folder.join("b.txt")).unwrap() == "1\n"
    });
    ui.wait_for("the search again", |ui| {
        ui.done() && ui.results().len() == 1
    });

    // A bad regex says so.
    ui.click("search-regex");
    ui.search("(one");
    ui.wait_for("the error", |ui| {
        let pane = ui.pane();
        ui.cx
            .read(|cx| matches!(pane.read(cx).status(), Status::Failed(_)))
    });
}

fn sample_results() -> Vec<FileResult> {
    let m = |line: u32, start: u32, preview: &str, at: (u32, u32)| SearchMatch {
        line,
        start,
        end: start + (at.1 - at.0),
        preview: preview.into(),
        preview_start: at.0,
        preview_end: at.1,
    };
    vec![
        FileResult {
            root: RootId(0),
            path: "crates/coqod/src/routes.rs".into(),
            matches: vec![
                m(
                    24,
                    11,
                    "use crate::buffers::{self, BufferConfig, BufferService};",
                    (11, 18),
                ),
                m(52, 8, "let buffers = BufferService::new(", (4, 11)),
                m(
                    58,
                    4,
                    "buffers::flush_on_shutdown(&buffers, ctx.shutdown.clone());",
                    (0, 7),
                ),
            ],
            collapsed: false,
        },
        FileResult {
            root: RootId(0),
            path: "crates/coqod/src/search.rs".into(),
            matches: vec![m(60, 4, "buffers: Arc<BufferService>,", (0, 7))],
            collapsed: false,
        },
        FileResult {
            root: RootId(0),
            path: "docs/PLAN.md".into(),
            matches: vec![m(
                88,
                40,
                "…the authoritative buffers in the daemon (coqod) and local replicas",
                (21, 28),
            )],
            collapsed: false,
        },
    ]
}

/// Offscreen renders of the Search pane for `docs/design/search.md`. Needs
/// a GPU: `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored search_screenshots`.
#[test]
#[ignore = "needs a GPU; run manually"]
fn search_screenshots() {
    use coqo_ui::test::render_offscreen;

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let shots = [
        ("search", Appearance::Dark, ""),
        ("search", Appearance::Light, ""),
        ("search-replace", Appearance::Dark, "buf"),
        ("search-replace", Appearance::Light, "buf"),
    ];
    for (name, appearance, replacement) in shots {
        let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
            crate::shell::init(cx);
            let scratch = cx.new(|cx| {
                Shell::new(
                    Rc::new(MemoryLayoutStore::default()),
                    DEFAULT_WORKSPACE,
                    window,
                    cx,
                )
            });
            let mut layout = scratch.read(cx).layout(cx);
            drop(scratch);
            layout.sidebar = SidebarLayout {
                width: 320.,
                pane: Some(PANE.into()),
            };
            let store = Rc::new(MemoryLayoutStore::default());
            store.save(DEFAULT_WORKSPACE, &layout).unwrap();
            cx.new(|cx| {
                let mut shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                let view = shell.show_pane(PANE, window, cx).expect("the pane");
                let pane = view.downcast::<SearchPane>().expect("a search pane");
                pane.update(cx, |pane, cx| {
                    pane.set_for_screenshot(
                        "buffers",
                        replacement,
                        sample_results(),
                        Status::Done(SearchSummary {
                            files: 3,
                            matches: 5,
                            searched: 412,
                            limit_hit: false,
                            elapsed_ms: 14,
                        }),
                        window,
                        cx,
                    );
                });
                shell
            })
        })
        .expect("offscreen render");
        let suffix = match appearance {
            Appearance::Light => "light",
            _ => "dark",
        };
        let path = dir.join(format!("coqo-{name}-{suffix}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
