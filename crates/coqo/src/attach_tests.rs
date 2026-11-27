// SPDX-License-Identifier: Apache-2.0

//! Attaching and reattaching (#27) and layouts kept by the daemon (#140),
//! against a real `coqod` served in-process: daemon restarts resume the
//! workspace events without repeating any, a `RESUME_GAP` takes a fresh
//! snapshot, windows show their own workspaces and stay in sync, and each
//! workspace's layout comes back after a UI restart.

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_protocol::methods::workspace::{
    self as ws, ListResult, OpenParams, RenameParams, WorkspaceEvent, WorkspaceId, WorkspaceParams,
};
use coqo_ui::{
    Appearance,
    dock::{
        AnyTab,
        store::{LAYOUT_VERSION, LayoutStore, MemoryLayoutStore, SidebarLayout, WorkbenchLayout},
        tree::PaneTree,
    },
    gpui::{AnyWindowHandle, App, AppContext as _, Entity, Keystroke, Window},
    test::{TestAppContext, TestWindowExt as _},
};

use crate::{
    daemon::{self, ConnectionState, Daemon, DaemonConfig},
    files::{
        preview::{FilePreview, KIND},
        tree::FileTree,
    },
    layout::{DaemonLayoutStore, LAYOUT_KEY, parse},
    resume::{Resumable, Update},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{Fixture, TIMEOUT, write},
    welcome, windows,
    workspace::{self, KnownWorkspaces, Workspaces},
};

/// What the app's workspace feed delivered, in order.
#[derive(Debug, Clone, PartialEq)]
enum Delivered {
    Snapshot,
    Event(u64),
}

type Log = Rc<RefCell<Vec<Delivered>>>;

struct Ui {
    cx: TestAppContext,
    windows: Vec<(AnyWindowHandle, Entity<Shell>)>,
    log: Log,
}

/// Start a UI against `fixture`'s daemon, with `fallback` as the offline
/// layout store, and open its first window.
fn start_ui(fixture: &Fixture, folder: Option<PathBuf>, fallback: MemoryLayoutStore) -> Ui {
    let cx = TestAppContext::single();
    // The daemon connection wakes GPUI tasks from its own threads.
    cx.executor().allow_parking();
    let paths = fixture.daemon.paths.clone();
    let log = Log::default();
    let recorded = log.clone();
    let first = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        let known = workspace::init(folder, cx);
        let feed = known.read(cx).feed().clone();
        cx.subscribe(
            &feed,
            move |_, update: &Update<ListResult, WorkspaceEvent>, _| {
                recorded.borrow_mut().push(match update {
                    Update::Snapshot(_) => Delivered::Snapshot,
                    Update::Event(e) => Delivered::Event(e.seq),
                });
            },
        )
        .detach();
        crate::shell::init(cx);
        let store = DaemonLayoutStore::install(Rc::new(fallback), DEFAULT_WORKSPACE, cx);
        windows::init_for_tests(store, cx);
        windows::open(cx).expect("open the first window")
    });
    Ui {
        cx,
        windows: vec![first],
        log,
    }
}

impl Ui {
    fn window(&self, ix: usize) -> AnyWindowHandle {
        self.windows[ix].0
    }

    fn shell(&self, ix: usize) -> Entity<Shell> {
        self.windows[ix].1.clone()
    }

    fn frame<R>(&mut self, ix: usize, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx
            .update_window(self.window(ix), |_, window, cx| {
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
            for ix in 0..self.windows.len() {
                self.frame(ix, |_, _| ());
            }
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn open_window(&mut self) -> usize {
        let opened = self.cx.update(windows::open).expect("open a window");
        self.windows.push(opened);
        self.windows.len() - 1
    }

    fn workspaces(&self, ix: usize) -> Entity<Workspaces> {
        self.cx
            .read(|cx| self.shell(ix).read(cx).workspaces().clone())
    }

    fn current(&self, ix: usize) -> Option<(WorkspaceId, String)> {
        let workspaces = self.workspaces(ix);
        self.cx.read(|cx| {
            workspaces
                .read(cx)
                .current()
                .map(|w| (w.id.clone(), w.name.clone()))
        })
    }

    fn current_name(&self, ix: usize) -> Option<String> {
        self.current(ix).map(|(_, name)| name)
    }

    fn connected(&self) -> bool {
        self.cx.read(|cx| {
            matches!(
                Daemon::global(cx).unwrap().read(cx).state(),
                ConnectionState::Connected { .. }
            )
        })
    }

    fn feed(&self) -> Entity<Resumable<ListResult, WorkspaceEvent>> {
        self.cx
            .read(|cx| KnownWorkspaces::global(cx).unwrap().read(cx).feed().clone())
    }

    fn snapshots(&self) -> usize {
        let feed = self.feed();
        self.cx.read(|cx| feed.read(cx).snapshots())
    }

    fn open_names(&self) -> Vec<String> {
        self.cx.read(|cx| {
            KnownWorkspaces::global(cx)
                .unwrap()
                .read(cx)
                .open_workspaces()
                .map(|w| w.name.clone())
                .collect()
        })
    }

    fn rail_buttons(&mut self, ix: usize) -> usize {
        self.frame(ix, |window, _| {
            (0..8usize)
                .take_while(|i| window.try_find(("workspace", *i)).is_some())
                .count()
        })
    }

    fn rows(&mut self, ix: usize) -> Vec<String> {
        self.frame(ix, |_, _| ());
        let shell = self.shell(ix);
        let tree = self
            .cx
            .read(|cx| shell.read(cx).sidebar_pane("files"))
            .and_then(|view| view.downcast::<FileTree>().ok());
        let Some(tree) = tree else {
            return Vec::new();
        };
        self.cx.read(|cx| {
            tree.read(cx)
                .rows()
                .iter()
                .map(|r| r.name.to_string())
                .collect()
        })
    }

    fn layout(&self, ix: usize) -> WorkbenchLayout {
        let shell = self.shell(ix);
        self.cx.read(|cx| shell.read(cx).layout(cx))
    }

    fn tab_kinds(&self, ix: usize) -> Vec<String> {
        tab_kinds(&self.layout(ix))
    }

    fn file_tabs(&self, ix: usize) -> Vec<Entity<FilePreview>> {
        let shell = self.shell(ix);
        self.cx.read(|cx| {
            let dock = shell.read(cx).dock().read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<FilePreview>())
                .collect()
        })
    }

    fn open_tab(&mut self, ix: usize, build: impl FnOnce(&mut App) -> AnyTab) {
        let shell = self.shell(ix);
        self.cx
            .update_window(self.window(ix), |_, window, cx| {
                let tab = build(cx);
                let dock = shell.read(cx).dock().clone();
                dock.update(cx, |dock, cx| dock.open_tab(tab, window, cx));
            })
            .unwrap();
    }

    fn open_folder(&mut self, ix: usize, path: PathBuf) {
        let workspaces = self.workspaces(ix);
        self.cx
            .update(|cx| workspaces.update(cx, |w, cx| w.open_path(path, cx)));
    }

    fn save_now(&mut self, ix: usize) {
        let shell = self.shell(ix);
        self.cx
            .update(|cx| shell.update(cx, |s, cx| s.save_now(cx)));
    }

    /// Wait until the daemon is gone and the UI's own reconnect gave up.
    fn wait_detached(&mut self) {
        self.wait_for("the drop to show", |ui| {
            !ui.connected() && ui.frame(0, |window, _| window.try_find("daemon-retry").is_some())
        });
    }

    fn retry(&mut self) {
        self.wait_for("a new connection", |ui| {
            if ui.frame(0, |window, _| window.try_find("daemon-retry").is_some()) {
                ui.frame(0, |window, cx| window.click("daemon-retry", cx));
            }
            ui.connected()
        });
    }

    /// Quit the UI as the user would: quit hooks save the layout.
    fn quit(self) {
        self.cx.quit();
    }
}

fn tab_kinds(layout: &WorkbenchLayout) -> Vec<String> {
    PaneTree::from_root(layout.dock.clone())
        .tabs()
        .iter()
        .map(|t| t.kind.to_string())
        .collect()
}

/// The events delivered so far: every seq once, in order.
fn assert_in_order_without_repeats(log: &Log) {
    let seqs: Vec<u64> = log
        .borrow()
        .iter()
        .filter_map(|d| match d {
            Delivered::Event(seq) => Some(*seq),
            Delivered::Snapshot => None,
        })
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "events repeated or out of order: {seqs:?}"
    );
}

fn open_in_daemon(fixture: &Fixture, path: &std::path::Path) -> WorkspaceId {
    let path = path.to_string_lossy().into_owned();
    fixture.daemon.with_client(async move |client| {
        client
            .request::<ws::Open>(OpenParams {
                path: Some(path),
                ..OpenParams::default()
            })
            .await
            .unwrap()
            .id
    })
}

#[test]
fn reattaches_after_a_daemon_restart_and_replays_what_it_missed() {
    let mut fixture = Fixture::new();
    let other = fixture.folder("other-service");
    let mut ui = start_ui(
        &fixture,
        Some(fixture.project.clone()),
        MemoryLayoutStore::default(),
    );
    ui.wait_for("the workspace and its tree", |ui| {
        ui.current_name(0).as_deref() == Some("demo-project") && !ui.rows(0).is_empty()
    });
    // A file tab, to check tabs survive the restart.
    ui.frame(0, |_, _| ());
    let shell = ui.shell(0);
    let tree = ui
        .cx
        .read(|cx| shell.read(cx).sidebar_pane("files"))
        .and_then(|v| v.downcast::<FileTree>().ok())
        .unwrap();
    let readme = ui.cx.read(|cx| {
        tree.read(cx)
            .rows()
            .iter()
            .position(|r| r.name.as_ref() == "README.md")
            .unwrap()
    });
    ui.frame(0, |window, cx| window.click(("file-row", readme), cx));
    ui.wait_for("the README tab", |ui| ui.file_tabs(0).len() == 1);
    let tab = ui.file_tabs(0)[0].clone();
    let dock = ui.cx.read(|cx| shell.read(cx).dock().clone());
    let (id, _) = ui.current(0).unwrap();
    let seq_before = ui.cx.read(|cx| ui.feed().read(cx).last_seq()).unwrap();

    fixture.daemon.stop();
    ui.wait_detached();
    fixture.daemon.serve();
    // While the UI is away, another client renames this workspace and
    // opens another one.
    let renamed = id.clone();
    fixture.daemon.with_client(async move |client| {
        client
            .request::<ws::Rename>(RenameParams {
                id: renamed,
                name: "renamed-project".into(),
            })
            .await
            .unwrap();
    });
    open_in_daemon(&fixture, &other);
    ui.retry();

    ui.wait_for("the missed events", |ui| {
        ui.current_name(0).as_deref() == Some("renamed-project")
            && ui.open_names().contains(&"other-service".to_owned())
    });
    ui.wait_for("the rail", |ui| ui.rail_buttons(0) == 2);
    assert_eq!(ui.snapshots(), 1, "resumed, without a new snapshot");
    let seq_after = ui.cx.read(|cx| ui.feed().read(cx).last_seq()).unwrap();
    assert!(seq_after > seq_before);
    assert_in_order_without_repeats(&ui.log);
    assert_eq!(ui.log.borrow()[0], Delivered::Snapshot);
    assert_eq!(
        ui.log
            .borrow()
            .iter()
            .filter(|d| **d == Delivered::Snapshot)
            .count(),
        1
    );
    // Same workspace, same tabs: nothing was rebuilt.
    assert_eq!(ui.current(0).unwrap().0, id);
    assert_eq!(ui.cx.read(|cx| shell.read(cx).dock().clone()), dock);
    assert_eq!(ui.file_tabs(0), [tab]);
    // And the file tree follows the disk again.
    write(&fixture.project, "after.txt", "x");
    ui.wait_for("changes after the reattach", |ui| {
        ui.rows(0).iter().any(|n| n == "after.txt")
    });
}

#[test]
fn a_resume_gap_takes_a_fresh_snapshot() {
    let mut fixture = Fixture::new();
    let other = fixture.folder("other-service");
    let mut ui = start_ui(
        &fixture,
        Some(fixture.project.clone()),
        MemoryLayoutStore::default(),
    );
    ui.wait_for("the workspace", |ui| {
        ui.current_name(0).as_deref() == Some("demo-project") && ui.snapshots() == 1
    });

    fixture.daemon.stop();
    ui.wait_detached();
    // Retention drops everything the UI hasn't seen yet.
    fixture.daemon.prune_event_log(20);
    fixture.daemon.serve();
    open_in_daemon(&fixture, &other);
    ui.retry();

    ui.wait_for("a fresh snapshot", |ui| ui.snapshots() == 2);
    ui.wait_for("the other workspace in the rail", |ui| {
        ui.open_names().contains(&"other-service".to_owned()) && ui.rail_buttons(0) == 2
    });
    let log = ui.log.borrow().clone();
    assert_eq!(
        log.iter().filter(|d| **d == Delivered::Snapshot).count(),
        2,
        "{log:?}"
    );
    assert_eq!(ui.current_name(0).as_deref(), Some("demo-project"));
    // Events keep flowing after the new snapshot.
    let id = ui.current(0).unwrap().0;
    fixture.daemon.with_client(async move |client| {
        client
            .request::<ws::Rename>(RenameParams {
                id,
                name: "after-the-gap".into(),
            })
            .await
            .unwrap();
    });
    ui.wait_for("the rename", |ui| {
        ui.current_name(0).as_deref() == Some("after-the-gap")
    });
}

#[cfg(target_os = "macos")]
const NEW_WINDOW: &str = "cmd-alt-n";
#[cfg(not(target_os = "macos"))]
const NEW_WINDOW: &str = "ctrl-alt-n";

#[test]
fn windows_show_their_own_workspaces_and_stay_in_sync() {
    let fixture = Fixture::new();
    let other = fixture.folder("other-service");
    let mut ui = start_ui(
        &fixture,
        Some(fixture.project.clone()),
        MemoryLayoutStore::default(),
    );
    ui.wait_for("the first workspace", |ui| {
        ui.current_name(0).as_deref() == Some("demo-project") && ui.rail_buttons(0) == 1
    });

    // New Window opens a second, empty window.
    ui.frame(0, |window, cx| {
        window.dispatch_keystroke(Keystroke::parse(NEW_WINDOW).unwrap(), cx);
    });
    ui.cx.run_until_parked();
    assert_eq!(ui.cx.windows().len(), 2);
    ui.cx.update(|cx| {
        let extra = cx.windows()[1];
        extra
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
    });
    ui.cx.run_until_parked();
    assert_eq!(ui.cx.windows().len(), 1);

    let second = ui.open_window();
    ui.wait_for("the second window", |ui| {
        ui.rail_buttons(second) == 1 && ui.current(second).is_none()
    });
    ui.open_folder(second, other.clone());
    // The first window's rail shows the second window's workspace as soon as
    // the second window shows it.
    let deadline = Instant::now() + TIMEOUT;
    let mut shown_at = None;
    let mut railed_at = None;
    while shown_at.is_none() || railed_at.is_none() {
        assert!(Instant::now() < deadline, "timed out");
        ui.cx.run_until_parked();
        if shown_at.is_none() && ui.current_name(second).as_deref() == Some("other-service") {
            shown_at = Some(Instant::now());
        }
        if railed_at.is_none() && ui.rail_buttons(0) == 2 {
            railed_at = Some(Instant::now());
        }
        std::thread::yield_now();
    }
    let (shown_at, railed_at) = (shown_at.unwrap(), railed_at.unwrap());
    let lag = railed_at.saturating_duration_since(shown_at);
    assert!(lag < Duration::from_millis(100), "the rail lagged {lag:?}");

    // Each window lists its own folder.
    assert_eq!(ui.current_name(0).as_deref(), Some("demo-project"));
    ui.wait_for("both trees", |ui| {
        ui.rows(0).contains(&"src".to_owned()) && ui.rows(second) == ["README.md"]
    });

    // Switching the first window to the workspace the second shows focuses
    // the second instead.
    let other_id = ui.current(second).unwrap().0;
    let first_workspaces = ui.workspaces(0);
    ui.cx.update(|cx| {
        first_workspaces.update(cx, |w, cx| w.switch_to(other_id.clone(), cx));
    });
    ui.cx.run_until_parked();
    assert_eq!(ui.cx.read(|cx| cx.active_window()), Some(ui.window(second)));
    assert_eq!(ui.current_name(0).as_deref(), Some("demo-project"));
    assert_eq!(
        ui.cx.read(|cx| Workspaces::window_showing(&other_id, cx)),
        Some(ui.window(second))
    );

    // So does opening its folder.
    ui.cx
        .update_window(ui.window(0), |_, window, _| window.activate_window())
        .unwrap();
    ui.open_folder(0, other);
    ui.wait_for("the second window to be focused", |ui| {
        ui.cx.read(|cx| cx.active_window()) == Some(ui.window(second))
            && !ui.cx.read(|cx| ui.workspaces(0).read(cx).is_opening())
    });
    assert_eq!(ui.current_name(0).as_deref(), Some("demo-project"));
}

fn layout_with_sidebar(width: f32) -> WorkbenchLayout {
    WorkbenchLayout {
        version: LAYOUT_VERSION,
        sidebar: SidebarLayout {
            width,
            pane: Some("files".into()),
        },
        dock: PaneTree::new().root().clone(),
    }
}

#[test]
fn layouts_live_with_their_workspace_in_the_daemon() {
    let fixture = Fixture::new();
    let other = fixture.folder("other-service");
    // A layout file from before the daemon kept layouts.
    let fallback = MemoryLayoutStore::default();
    fallback
        .save(DEFAULT_WORKSPACE, &layout_with_sidebar(333.))
        .unwrap();

    let mut ui = start_ui(&fixture, Some(fixture.project.clone()), fallback.clone());
    ui.wait_for("the workspace", |ui| {
        ui.current_name(0).as_deref() == Some("demo-project")
    });
    let (project, _) = ui.current(0).unwrap();
    // The old layout was imported into the workspace, once.
    assert_eq!(ui.layout(0).sidebar.width, 333.);
    assert_eq!(fallback.load(DEFAULT_WORKSPACE).unwrap(), None);
    ui.open_tab(0, welcome::new);
    ui.save_now(0);
    let saved = |fixture: &Fixture, id: &WorkspaceId| {
        let id = id.clone();
        fixture.daemon.with_client(async move |client| {
            client
                .request::<ws::GetState>(WorkspaceParams { id })
                .await
                .unwrap()
                .entries
                .get(LAYOUT_KEY)
                .and_then(parse)
        })
    };
    let deadline = Instant::now() + TIMEOUT;
    while saved(&fixture, &project).is_none_or(|l| tab_kinds(&l).is_empty()) {
        assert!(Instant::now() < deadline, "the layout never reached coqod");
        ui.cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(saved(&fixture, &project).unwrap().sidebar.width, 333.);
    assert_eq!(fallback.load(project.0.as_str()).unwrap(), None);

    // Another workspace in the same window starts from the default layout.
    ui.open_folder(0, other.clone());
    ui.wait_for("the other workspace", |ui| {
        ui.current_name(0).as_deref() == Some("other-service")
    });
    let (other_id, _) = ui.current(0).unwrap();
    assert!(ui.tab_kinds(0).is_empty());
    assert_ne!(ui.layout(0).sidebar.width, 333.);
    ui.open_tab(0, welcome::new);
    ui.open_tab(0, welcome::new);
    ui.save_now(0);

    // Back to the first: its layout comes back.
    ui.open_folder(0, fixture.project.clone());
    ui.wait_for("the first workspace again", |ui| {
        ui.current(0).map(|c| c.0) == Some(project.clone())
    });
    assert_eq!(ui.tab_kinds(0), ["welcome"]);
    assert_eq!(ui.layout(0).sidebar.width, 333.);
    ui.quit();

    // A new UI, with nothing on disk: the daemon's copy comes back.
    let mut ui = start_ui(&fixture, None, MemoryLayoutStore::default());
    ui.wait_for("the last workspace", |ui| {
        ui.current(0).map(|c| c.0) == Some(project.clone())
    });
    assert_eq!(ui.tab_kinds(0), ["welcome"]);
    assert_eq!(ui.layout(0).sidebar.width, 333.);
    // A second window on the other workspace opens with its layout.
    let second = ui.open_window();
    ui.open_folder(second, other);
    ui.wait_for("the other workspace", |ui| {
        ui.current(second).map(|c| c.0) == Some(other_id.clone())
    });
    assert_eq!(ui.tab_kinds(second), ["welcome", "welcome"]);
}

#[test]
fn layouts_saved_while_detached_reach_the_daemon_on_reattach() {
    let mut fixture = Fixture::new();
    let fallback = MemoryLayoutStore::default();
    let mut ui = start_ui(&fixture, Some(fixture.project.clone()), fallback.clone());
    ui.wait_for("the workspace", |ui| ui.current(0).is_some());
    let (project, _) = ui.current(0).unwrap();

    fixture.daemon.stop();
    ui.wait_detached();
    ui.open_tab(0, welcome::new);
    ui.save_now(0);
    // Kept on disk meanwhile.
    let offline = fallback.load(project.0.as_str()).unwrap().expect("saved");
    assert_eq!(tab_kinds(&offline), ["welcome"]);

    fixture.daemon.serve();
    ui.retry();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        // The shell saves shortly after the reattach (the test clock).
        ui.cx.executor().advance_clock(Duration::from_millis(100));
        ui.cx.run_until_parked();
        let id = project.clone();
        let tabs = fixture.daemon.with_client(async move |client| {
            client
                .request::<ws::GetState>(WorkspaceParams { id })
                .await
                .unwrap()
                .entries
                .get(LAYOUT_KEY)
                .and_then(parse)
                .map(|l| tab_kinds(&l).len())
        });
        if tabs == Some(1) {
            break;
        }
        assert!(Instant::now() < deadline, "the layout never reached coqod");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fallback.load(project.0.as_str()).unwrap(), None);
}
