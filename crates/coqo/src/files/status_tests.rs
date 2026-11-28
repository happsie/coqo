// SPDX-License-Identifier: Apache-2.0

//! The tree's row decorations from git (issue #163), auto-reveal, and the
//! 200k-entry scroll benchmark, against a real `coqod` (with the built-in
//! git mod) served in-process.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_mod_git::testing::Fixture as Repo;
use coqo_protocol::methods::{
    fs::EntryKind,
    workspace::{self as wsm, RootId},
};
use coqo_ui::{
    Appearance,
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::{
    decorations::{Badge, RowDecoration, Tone},
    editor::FileEditor,
    tree::{FileTree, Row},
};
use crate::{
    daemon::{self, DaemonConfig},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{TIMEOUT, TestDaemon},
    workspace,
};

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    daemon: TestDaemon,
}

fn open_ui(folder: PathBuf, daemon: TestDaemon) -> Ui {
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let paths = daemon.paths.clone();
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
    Ui {
        cx,
        window,
        shell,
        daemon,
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

    fn tree(&mut self) -> Entity<FileTree> {
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

    fn row_index(&mut self, path: &str) -> Option<usize> {
        self.rows().iter().position(|r| r.path == path)
    }

    /// What the row of `path` (in any root) shows; `None` without a row.
    fn decoration(&mut self, path: &str) -> Option<RowDecoration> {
        let ix = self.row_index(path)?;
        let tree = self.tree();
        Some(self.cx.read(|cx| tree.read(cx).decoration(ix, cx)))
    }

    fn letter(&mut self, path: &str) -> Option<char> {
        self.decoration(path)?.badge.map(|b| b.letter)
    }

    fn tint(&mut self, path: &str) -> Option<Tone> {
        self.decoration(path)?.tint
    }

    fn toggle(&mut self, root: RootId, path: &str) {
        let tree = self.tree();
        let path = path.to_owned();
        self.cx
            .update(|cx| tree.update(cx, |t, cx| t.toggle(root, &path, cx)));
    }

    fn root(&mut self) -> RootId {
        self.rows()[0].root
    }

    fn open(&mut self, root: RootId, path: &str) -> Entity<FileEditor> {
        let (path, window) = (path.to_owned(), self.window);
        let tree = self.tree();
        self.cx
            .update(|cx| tree.update(cx, |t, cx| t.reveal_entry(root, &path, cx)));
        self.wait_for("its row", |ui| ui.row_index(&path).is_some());
        let ix = self.row_index(&path).unwrap();
        self.cx
            .update_window(window, |_, window, cx| {
                tree.update(cx, |t, cx| t.activate(ix, window, cx))
            })
            .unwrap();
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        let active = move |cx: &App| {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
        };
        self.wait_for("the tab", |ui| {
            ui.cx
                .read(|cx| active(cx).is_some_and(|t| t.read(cx).path() == path))
        });
        self.cx.read(|cx| active(cx)).unwrap()
    }
}

fn start(repo_dir: &Path) -> (tempfile::TempDir, Repo, TestDaemon) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join(repo_dir));
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    (tmp, repo, daemon)
}

/// Modified, staged-new, deleted and untracked files get their letters
/// and colours; folders with changes inside are tinted; a commit made
/// outside Coqo clears them live (`git/changes`).
#[test]
fn git_status_letters_and_folder_tints_are_live() {
    let (_tmp, repo, daemon) = start(Path::new("demo-repo"));
    repo.write("README.md", "# Demo\n");
    repo.write("src/lib.rs", "pub fn f() {}\n");
    repo.write("src/old.rs", "// old\n");
    repo.write("docs/guide.md", "Guide\n");
    repo.write("docs/faq.md", "FAQ\n");
    repo.write("assets/logo.svg", "<svg/>\n");
    repo.commit_all("Initial commit");
    repo.write("src/lib.rs", "pub fn g() {}\n");
    repo.write("src/new.rs", "pub fn new() {}\n");
    repo.git(&["add", "src/new.rs"]);
    std::fs::remove_file(repo.path().join("docs/guide.md")).unwrap();
    repo.write("notes.txt", "todo\n");

    let mut ui = open_ui(repo.path().to_owned(), daemon);
    ui.wait_for("the tree", |ui| ui.row_index("src").is_some());
    let root = ui.root();
    ui.toggle(root, "src");
    ui.toggle(root, "docs");
    ui.wait_for("the letters", |ui| {
        ui.letter("src/lib.rs") == Some('M') && ui.letter("src/new.rs").is_some()
    });

    let badge = |ui: &mut Ui, path: &str| ui.decoration(path).and_then(|d| d.badge);
    assert_eq!(
        badge(&mut ui, "src/lib.rs"),
        Some(Badge {
            letter: 'M',
            tone: Tone::Sand
        })
    );
    assert_eq!(
        badge(&mut ui, "src/new.rs"),
        Some(Badge {
            letter: 'A',
            tone: Tone::Green
        })
    );
    assert_eq!(
        badge(&mut ui, "notes.txt"),
        Some(Badge {
            letter: '?',
            tone: Tone::Subtle
        })
    );
    assert_eq!(badge(&mut ui, "README.md"), None);
    assert_eq!(badge(&mut ui, "docs/faq.md"), None);
    // A deleted file has no row: its folder's tint shows it.
    assert_eq!(ui.row_index("docs/guide.md"), None);
    assert_eq!(ui.tint("docs"), Some(Tone::Rose));
    // A modification is the strongest change in `src`.
    assert_eq!(ui.tint("src"), Some(Tone::Sand));
    assert_eq!(ui.tint("assets"), None);
    // Files show letters, never tints.
    assert_eq!(ui.tint("src/lib.rs"), None);
    // The letter is drawn at the end of the row.
    let lib = ui.row_index("src/lib.rs").unwrap();
    let readme = ui.row_index("README.md").unwrap();
    ui.frame(|window, _| {
        let row = window.find(("file-row", lib)).bounds();
        let letter = window.find(("file-row-badge", lib)).bounds();
        assert!(letter.right() <= row.right() && letter.right() > row.right() - px(16.));
        assert!(window.try_find(("file-row-badge", readme)).is_none());
    });

    // Committed outside Coqo: the letters and tints go by themselves.
    repo.commit_all("Change things");
    ui.wait_for("the letters to clear", |ui| {
        ui.letter("src/lib.rs").is_none() && ui.letter("notes.txt").is_none()
    });
    assert_eq!(ui.tint("src"), None);
    assert_eq!(ui.tint("docs"), None);
    // And come back with the next change.
    repo.write("assets/logo.svg", "<svg></svg>\n");
    ui.wait_for("the folder tint", |ui| {
        ui.tint("assets") == Some(Tone::Sand)
    });
}

/// Each root of a multi-root workspace has its own letters, and a root's
/// row is tinted when something in it changed.
#[test]
fn letters_in_a_multi_root_workspace() {
    let (tmp, repo, daemon) = start(Path::new("first"));
    repo.write("a.txt", "a\n");
    repo.commit_all("Initial commit");
    let other = Repo::init(&tmp.path().join("second"));
    other.write("b.txt", "b\n");
    other.write("c.txt", "c\n");
    other.commit_all("Initial commit");
    other.write("b.txt", "changed\n");

    let mut ui = open_ui(repo.path().to_owned(), daemon);
    ui.wait_for("the tree", |ui| ui.row_index("a.txt").is_some());
    let workspace = ui
        .cx
        .read(|cx| {
            crate::workspace::Workspaces::global(cx)?
                .read(cx)
                .current()
                .cloned()
        })
        .expect("a workspace")
        .id;
    let path = other.path().to_str().unwrap().to_owned();
    ui.daemon.with_client(async move |client| {
        client
            .request::<wsm::AddRoot>(wsm::AddRootParams {
                id: workspace,
                path,
                name: None,
            })
            .await
            .unwrap()
    });
    ui.wait_for("both roots", |ui| {
        ui.rows().iter().filter(|r| r.depth == 0).count() == 2 && ui.row_index("b.txt").is_some()
    });
    ui.wait_for("the second root's letter", |ui| {
        ui.letter("b.txt") == Some('M')
    });
    let roots: Vec<Row> = ui.rows().into_iter().filter(|r| r.depth == 0).collect();
    let tree = ui.tree();
    let root_tint = |ui: &mut Ui, name: &str| {
        let ix = ui
            .rows()
            .iter()
            .position(|r| r.depth == 0 && r.name.as_ref() == name)?;
        ui.cx.read(|cx| tree.read(cx).decoration(ix, cx).tint)
    };
    assert_eq!(root_tint(&mut ui, &roots[1].name), Some(Tone::Sand));
    assert_eq!(root_tint(&mut ui, &roots[0].name), None);
    assert_eq!(ui.letter("a.txt"), None);
    assert_eq!(ui.letter("c.txt"), None);
    // A change in the first root lands on its rows only.
    repo.write("a.txt", "changed\n");
    ui.wait_for("the first root's letter", |ui| {
        ui.letter("a.txt") == Some('M')
    });
    assert_eq!(root_tint(&mut ui, &roots[0].name), Some(Tone::Sand));
}

/// With `files.auto_reveal` on, switching tabs reveals the file in the
/// tree (its collapsed folders expanded) and leaves the focus in the tab;
/// off (the default), the tree stays as it is.
#[test]
fn auto_reveal_follows_the_active_tab() {
    let (_tmp, repo, daemon) = start(Path::new("demo-repo"));
    repo.write("src/deep/inner/lib.rs", "pub fn f() {}\n");
    repo.write("docs/guide.md", "Guide\n");
    repo.commit_all("Initial commit");
    let mut ui = open_ui(repo.path().to_owned(), daemon);
    ui.wait_for("the tree", |ui| ui.row_index("src").is_some());
    let root = ui.root();
    let lib = ui.open(root, "src/deep/inner/lib.rs");
    let guide = ui.open(root, "docs/guide.md");
    ui.toggle(root, "src");
    ui.toggle(root, "docs");
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    let id_of = |ui: &mut Ui, tab: &Entity<FileEditor>| {
        ui.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .map(|t| t.id)
                .find(|id| {
                    dock.tab(*id)
                        .and_then(|t| t.downcast::<FileEditor>())
                        .is_some_and(|t| t == *tab)
                })
                .unwrap()
        })
    };
    let (lib_id, guide_id) = (id_of(&mut ui, &lib), id_of(&mut ui, &guide));
    let activate = |ui: &mut Ui, id| {
        ui.frame(|window, cx| dock.update(cx, |d, cx| d.activate_tab(id, window, cx)));
        ui.cx.run_until_parked();
    };
    let selected = |ui: &mut Ui| {
        let tree = ui.tree();
        ui.cx
            .read(|cx| tree.read(cx).selected().map(|(_, p)| p.to_owned()))
    };

    // Off: nothing moves.
    activate(&mut ui, lib_id);
    ui.wait_for("a frame", |_| true);
    assert_eq!(ui.row_index("src/deep"), None, "src stays collapsed");

    // On (the palette command): the switch reveals the file.
    ui.frame(|window, cx| window.dispatch_action(Box::new(super::ToggleAutoReveal), cx));
    assert!(
        ui.cx
            .read(|cx| crate::settings::get(cx).bool(crate::settings::AUTO_REVEAL, None))
    );
    activate(&mut ui, guide_id);
    ui.wait_for("guide.md revealed", |ui| {
        selected(ui).as_deref() == Some("docs/guide.md")
    });
    activate(&mut ui, lib_id);
    ui.wait_for("lib.rs revealed", |ui| {
        selected(ui).as_deref() == Some("src/deep/inner/lib.rs")
    });
    assert!(ui.row_index("src/deep/inner").is_some());
    // The focus stays in the tab.
    let tree = ui.tree();
    let focus = ui
        .cx
        .read(|cx| coqo_ui::dock::TabItem::focus_handle(lib.read(cx), cx))
        .expect("a file tab takes the keyboard");
    let tree_focus = ui.cx.read(|cx| tree.read(cx).focus_handle(cx));
    ui.frame(|window, cx| {
        assert!(focus.contains_focused(window, cx));
        assert!(!tree_focus.contains_focused(window, cx));
    });

    // Off again: the tree keeps what it shows.
    ui.frame(|window, cx| window.dispatch_action(Box::new(super::ToggleAutoReveal), cx));
    ui.toggle(root, "docs");
    activate(&mut ui, guide_id);
    ui.wait_for("a frame", |_| true);
    assert_eq!(selected(&mut ui).as_deref(), Some("src/deep/inner/lib.rs"));
    assert_eq!(ui.row_index("docs/guide.md"), None);
}

/// The tree with git's letters and tinted folders, dark and light, in
/// `$COQO_SHOT_DIR` as `coqo-files-status-<appearance>.png` (offscreen,
/// needs a GPU).
#[test]
#[ignore = "needs a GPU; run manually"]
fn files_status_screenshots() {
    use std::sync::Arc;

    use coqo_ui::{
        CoqoAssets,
        gpui::{HeadlessAppContext, platform},
    };

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let (_tmp, repo, daemon) = start(Path::new("billing"));
    for (path, text) in [
        ("Cargo.toml", "[workspace]\n"),
        ("README.md", "# Billing\n"),
        ("crates/invoice/src/lib.rs", "pub mod tax;\n"),
        ("crates/invoice/src/tax.rs", "pub fn rate() -> u32 { 20 }\n"),
        ("crates/invoice/src/totals.rs", "pub fn total() {}\n"),
        ("crates/invoice/Cargo.toml", "[package]\n"),
        ("crates/ledger/src/lib.rs", "pub fn post() {}\n"),
        ("docs/guide.md", "Guide\n"),
        ("docs/setup.md", "Setup\n"),
    ] {
        repo.write(path, text);
    }
    repo.commit_all("Initial commit");
    repo.write("crates/invoice/src/tax.rs", "pub fn rate() -> u32 { 21 }\n");
    repo.write(
        "crates/invoice/src/totals.rs",
        "pub fn total() -> u32 { 0 }\n",
    );
    repo.write("crates/invoice/src/discount.rs", "pub fn off() {}\n");
    repo.git(&["add", "crates/invoice/src/discount.rs"]);
    repo.write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    repo.write("NOTES.md", "todo\n");
    std::fs::remove_file(repo.path().join("docs/setup.md")).unwrap();

    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let mut cx = HeadlessAppContext::with_platform(
            platform::current_platform(true).text_system(),
            Arc::new(CoqoAssets),
            platform::current_headless_renderer,
        );
        cx.allow_parking();
        let (paths, folder) = (daemon.paths.clone(), repo.path().to_owned());
        let (handle, shell) = cx
            .update(|cx| {
                coqo_ui::init(appearance, cx);
                let mut config = DaemonConfig::at(paths);
                config.spawn = false;
                daemon::init(config, cx);
                workspace::init(Some(folder), cx);
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
            tree(cx).is_some_and(|t| t.read(cx).rows().iter().any(|r| r.path == "docs"))
        });
        let tree = cx.update(|cx| tree(cx)).unwrap();
        let root = cx.update(|cx| tree.read(cx).rows()[0].root);
        for folder in ["crates", "crates/invoice", "crates/invoice/src", "docs"] {
            cx.update(|cx| tree.update(cx, |t, cx| t.toggle(root, folder, cx)));
            settle(&mut cx, &|cx| {
                tree.read(cx)
                    .rows()
                    .iter()
                    .any(|r| r.path.starts_with(&format!("{folder}/")))
            });
        }
        // Open tax.rs, revealed and selected in the tree.
        cx.update_window(handle, |_, window, cx| {
            tree.update(cx, |t, cx| {
                let ix = t
                    .rows()
                    .iter()
                    .position(|r| r.path == "crates/invoice/src/tax.rs")
                    .unwrap();
                t.activate(ix, window, cx);
            })
        })
        .unwrap();
        settle(&mut cx, &|cx| {
            let t = tree.read(cx);
            let ix = t.rows().iter().position(|r| r.path == "NOTES.md");
            ix.is_some_and(|ix| t.decoration(ix, cx).badge.is_some())
        });
        for _ in 0..20 {
            settle(&mut cx, &|_| true);
            std::thread::sleep(Duration::from_millis(20));
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-files-status-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}

/// A folder of `count` empty files.
fn fill(dir: &Path, count: usize) {
    std::fs::create_dir_all(dir).unwrap();
    for i in 0..count {
        std::fs::write(dir.join(format!("file-{i:04}.txt")), "").unwrap();
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let ix = ((sorted.len() as f64 - 1.) * p).round() as usize;
    sorted[ix]
}

/// Print the median, 95th percentile and worst of `frames`; the first two.
fn report(what: &str, mut frames: Vec<Duration>) -> (Duration, Duration) {
    frames.sort();
    let (p50, p95, max) = (
        percentile(&frames, 0.5),
        percentile(&frames, 0.95),
        *frames.last().unwrap(),
    );
    println!(
        "{what}: {} frames, p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms",
        frames.len(),
        p50.as_secs_f64() * 1e3,
        p95.as_secs_f64() * 1e3,
        max.as_secs_f64() * 1e3,
    );
    (p50, p95)
}

/// 200 folders of 1,000 files, all expanded (200,200 rows), scrolled a
/// few rows a frame (a wheel) and by jumps across the whole list (the
/// scrollbar). Prints the frame times; `docs/design/files.md` has the
/// numbers. The tree is made under the build's `target/` (or
/// `$COQO_BENCH_DIR`) and deleted afterwards.
///
/// `cargo test -p coqo --release -- --ignored files_scroll_benchmark --nocapture`
#[test]
#[ignore = "a benchmark: makes 200k files; run manually"]
fn files_scroll_benchmark() {
    const FOLDERS: usize = 200;
    const FILES: usize = 1_000;
    let base = std::env::var_os("COQO_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .join("target")
        });
    std::fs::create_dir_all(&base).unwrap();
    let bench = tempfile::Builder::new()
        .prefix("files-scroll-bench-")
        .tempdir_in(&base)
        .unwrap();
    let project = bench.path().join("big");
    let made = Instant::now();
    for d in 0..FOLDERS {
        fill(&project.join(format!("dir-{d:03}")), FILES);
    }
    println!(
        "made {} files in {:.1} s",
        FOLDERS * FILES,
        made.elapsed().as_secs_f64()
    );
    // The daemon's socket stays in the system's temporary folder (socket
    // paths are short); its data goes with the tree.
    let run = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start(run.path().join("run"), bench.path().join("data"));
    let mut ui = open_ui(project.clone(), daemon);
    ui.wait_for("the tree", |ui| ui.rows().len() == FOLDERS);
    let root = ui.root();
    let tree = ui.tree();
    // Let the daemon finish indexing the folder (the file finder's index)
    // before timing anything.
    settle(&mut ui, Duration::from_secs(5));

    // The same window with the folders collapsed (200 rows), for scale.
    // The first frames lay out text and glyphs for the first time.
    for ix in 0..30 {
        frame_at(&mut ui, &tree, ix * 3);
    }
    let small: Vec<Duration> = (0..600)
        .map(|i| frame_at(&mut ui, &tree, (i * 3) % 170))
        .collect();
    let small_p50 = report("200 rows, wheel scroll (3 rows/frame)", small).0;

    let expanding = Instant::now();
    for d in 0..FOLDERS {
        ui.toggle(root, &format!("dir-{d:03}"));
    }
    let total = FOLDERS * (FILES + 1);
    ui.wait_for("every folder listed", |ui| ui.rows().len() == total);
    println!(
        "expanded {FOLDERS} folders ({total} rows) in {:.2} s",
        expanding.elapsed().as_secs_f64()
    );
    settle(&mut ui, Duration::from_secs(3));
    // One rebuild of the flat row list: a collapse and an expand.
    let mut rebuilds = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        ui.toggle(root, "dir-100");
        ui.toggle(root, "dir-100");
        rebuilds.push(t.elapsed() / 2);
    }
    assert_eq!(ui.rows().len(), total);
    report("rebuild of the 200k-row list", rebuilds);

    for ix in 0..30 {
        frame_at(&mut ui, &tree, ix * 3);
    }
    // A wheel: three rows a frame, in the middle of the list.
    let start = total / 2;
    let wheel: Vec<Duration> = (0..600)
        .map(|i| frame_at(&mut ui, &tree, start + i * 3))
        .collect();
    let (wheel_p50, wheel_p95) = report("200k rows, wheel scroll (3 rows/frame)", wheel);
    // The scrollbar: jumps across the whole list.
    let mut seed = 0x2545_f491_u64;
    let jumps: Vec<Duration> = (0..300)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            frame_at(&mut ui, &tree, (seed % total as u64) as usize)
        })
        .collect();
    let jump_p95 = report("200k rows, jumps across the list", jumps).1;
    // Only the rows on screen are drawn.
    ui.frame(|window, _| {
        assert!(window.try_find(("file-row", 0usize)).is_none());
    });
    let rows = ui.rows();
    assert_eq!(rows.len(), total);
    assert!(
        rows.iter()
            .all(|r| r.kind == EntryKind::Dir || r.depth == 1)
    );
    drop(ui);
    drop(bench);
    assert!(!project.exists(), "the generated tree is deleted");
    // Virtualised: a frame costs about the same with 1,000 times the rows.
    assert!(
        wheel_p50 < small_p50 * 3 / 2 + Duration::from_millis(1),
        "a 200k-row frame costs more than a 200-row one"
    );
    // A frame is 16.7 ms at 60 Hz (in an optimised build; a debug build is
    // several times slower everywhere).
    if !cfg!(debug_assertions) {
        let budget = Duration::from_micros(16_700);
        assert!(
            wheel_p95 < budget && jump_p95 < budget,
            "scrolling 200k rows is slower than 60 fps"
        );
    }
}

/// Run the app's tasks for `time`, so background work is done.
fn settle(ui: &mut Ui, time: Duration) {
    let until = Instant::now() + time;
    while Instant::now() < until {
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Scroll `tree` to put row `ix` at the top, and time the next frame.
fn frame_at(ui: &mut Ui, tree: &Entity<FileTree>, ix: usize) -> Duration {
    ui.cx
        .update(|cx| tree.update(cx, |t, cx| t.scroll_to_row(ix, cx)));
    let t = Instant::now();
    ui.cx
        .update_window(ui.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    t.elapsed()
}
