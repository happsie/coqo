// SPDX-License-Identifier: Apache-2.0

//! The workspace rail against the daemon's workspace service and session
//! manager, served in-process, with `ScriptedProvider` as the agent: the
//! rail lists every workspace, switching swaps the file tree and the
//! layout, background agents badge their workspace, and the context menu
//! renames, removes and opens in a new window.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use coqo_agents::{
    PermissionAsk,
    model::{
        ContentBlock, Create, CreateSessionParams, PermissionOption, PermissionOptionKind, Prompt,
        PromptParams,
    },
    testing::{ScriptedProvider, Step},
};
use coqo_protocol::methods::workspace::{self as ws, OpenParams, WorkspaceId};
use coqo_ui::{
    Appearance,
    dock::{
        store::{MemoryLayoutStore, WorkbenchLayout},
        tree::PaneTree,
    },
    gpui::{AnyWindowHandle, App, AppContext as _, ElementId, Entity, Keystroke, Window},
    test::{TestAppContext, TestWindowExt as _},
};
use tempfile::TempDir;
use tokio::sync::Notify;

use super::{Badge, Dot, Rail};
use crate::{
    activity::AgentActivity,
    daemon::{self, DaemonConfig},
    files::tree::FileTree,
    layout::DaemonLayoutStore,
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{ScriptedDaemon, TIMEOUT, write},
    welcome, windows,
    workspace::{self, KnownWorkspaces, Workspaces},
};

#[cfg(target_os = "macos")]
const MOD: &str = "cmd";
#[cfg(not(target_os = "macos"))]
const MOD: &str = "ctrl";

struct Fixture {
    tmp: TempDir,
    project: PathBuf,
    daemon: ScriptedDaemon,
}

impl Fixture {
    fn new(provider: &ScriptedProvider) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo-project");
        write(&project, "README.md", "# Demo\n");
        write(&project, "src/main.rs", "fn main() {}\n");
        let project = project.canonicalize().unwrap();
        let daemon = ScriptedDaemon::start(tmp.path(), &[("fake", "Fake agent", provider)]);
        Self {
            tmp,
            project,
            daemon,
        }
    }

    /// Another project folder, `name`, with a `README.md`.
    fn folder(&self, name: &str) -> PathBuf {
        let path = self.tmp.path().join(name);
        write(&path, "README.md", format!("# {name}\n"));
        path.canonicalize().unwrap()
    }

    /// Open `path` from another client, as the CLI would.
    fn open_in_daemon(&self, path: &Path) -> WorkspaceId {
        let path = path.to_string_lossy().into_owned();
        let id = self.daemon.with_client(async move |client| {
            client
                .request::<ws::Open>(OpenParams {
                    path: Some(path),
                    ..OpenParams::default()
                })
                .await
                .unwrap()
                .id
        });
        // Distinct creation times, so the rail's order is the order here.
        std::thread::sleep(Duration::from_millis(3));
        id
    }

    /// Start a session in `workspace` and send it a prompt.
    fn start_turn(&self, workspace: &WorkspaceId, cwd: &Path) {
        let (workspace, cwd) = (workspace.0.clone(), cwd.to_string_lossy().into_owned());
        self.daemon.with_client(async move |client| {
            let info = client
                .request::<Create>(CreateSessionParams {
                    provider: "fake".into(),
                    cwd,
                    workspace: Some(workspace),
                    title: None,
                    mode: None,
                })
                .await
                .unwrap();
            client
                .request::<Prompt>(PromptParams {
                    session: info.id,
                    content: vec![ContentBlock::text("Run the tests")],
                })
                .await
                .unwrap();
        });
    }
}

struct Ui {
    cx: TestAppContext,
    windows: Vec<(AnyWindowHandle, Entity<Shell>)>,
}

/// A UI on `fixture`'s daemon with its first window, opening `folder`.
fn start_ui(fixture: &Fixture, folder: Option<PathBuf>) -> Ui {
    let cx = TestAppContext::single();
    // The daemon connection wakes GPUI tasks from its own threads.
    cx.executor().allow_parking();
    let paths = fixture.daemon.paths.clone();
    let first = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(folder, cx);
        crate::shell::init(cx);
        let fallback = Rc::new(MemoryLayoutStore::default());
        let store = DaemonLayoutStore::install(fallback, DEFAULT_WORKSPACE, cx);
        windows::init_for_tests(store, cx);
        windows::open(cx).expect("open the first window")
    });
    Ui {
        cx,
        windows: vec![first],
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

    fn rail(&self, ix: usize) -> Entity<Rail> {
        self.cx.read(|cx| self.shell(ix).read(cx).rail().clone())
    }

    fn workspaces(&self, ix: usize) -> Entity<Workspaces> {
        self.cx
            .read(|cx| self.shell(ix).read(cx).workspaces().clone())
    }

    /// The rail's workspaces, top to bottom.
    fn rail_names(&self, ix: usize) -> Vec<String> {
        let rail = self.rail(ix);
        self.cx
            .read(|cx| rail.read(cx).list(cx).into_iter().map(|w| w.name).collect())
    }

    /// How many workspace buttons the rail draws.
    fn rail_buttons(&mut self, ix: usize) -> usize {
        self.frame(ix, |window, _| {
            (0..16usize)
                .take_while(|i| window.try_find(("workspace", *i)).is_some())
                .count()
        })
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
        PaneTree::from_root(self.layout(ix).dock)
            .tabs()
            .iter()
            .map(|t| t.kind.to_string())
            .collect()
    }

    fn open_welcome_tab(&mut self, ix: usize) {
        let shell = self.shell(ix);
        self.cx
            .update_window(self.window(ix), |_, window, cx| {
                let tab = welcome::new(cx);
                let dock = shell.read(cx).dock().clone();
                dock.update(cx, |dock, cx| dock.open_tab(tab, window, cx));
            })
            .unwrap();
        self.cx
            .update(|cx| shell.update(cx, |s, cx| s.save_now(cx)));
    }

    fn shows(&mut self, ix: usize, id: impl Into<ElementId>) -> bool {
        let id = id.into();
        self.frame(ix, |window, _| window.try_find(id).is_some())
    }

    fn click(&mut self, ix: usize, id: impl Into<ElementId>) {
        let id = id.into();
        self.frame(ix, |window, cx| window.click(id, cx));
    }

    fn right_click(&mut self, ix: usize, id: impl Into<ElementId>) {
        let id = id.into();
        self.frame(ix, |window, cx| window.right_click(id, cx));
    }

    fn press(&mut self, ix: usize, keys: &str) {
        // Parsing wants a static string.
        let keys: &'static str = Box::leak(keys.to_owned().into_boxed_str());
        self.frame(ix, |window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
        });
    }

    /// The rail button of the workspace called `name`.
    fn button_of(&self, ix: usize, name: &str) -> usize {
        self.rail_names(ix)
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("{name} is not on the rail"))
    }

    fn badge(&self, id: &WorkspaceId) -> (Option<Badge>, Option<Dot>) {
        self.cx.read(|cx| {
            let activity = AgentActivity::global(cx).unwrap().read(cx).of(&id.0);
            (Badge::of(activity), Dot::of(activity))
        })
    }
}

fn ask() -> PermissionAsk {
    PermissionAsk {
        title: "Run cargo test".into(),
        tool_call: None,
        options: vec![PermissionOption {
            id: "allow-once".into(),
            name: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
    }
}

#[test]
fn lists_every_workspace_and_switches_with_clicks_and_keys() {
    let fixture = Fixture::new(&ScriptedProvider::new());
    let alpha = fixture.folder("alpha");
    let beta = fixture.folder("beta-service");
    fixture.open_in_daemon(&alpha);
    fixture.open_in_daemon(&beta);
    let mut ui = start_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for("the workspace and the rail", |ui| {
        ui.current_name(0).as_deref() == Some("demo-project")
            && ui.rail_buttons(0) == 3
            && ui.rows(0).contains(&"src".to_owned())
    });
    // In the order they were added, with their initials.
    assert_eq!(ui.rail_names(0), ["alpha", "beta-service", "demo-project"]);
    ui.frame(0, |window, _| {
        for ix in 0..3usize {
            let button = window.find(("workspace", ix)).bounds();
            assert_eq!(button.size.width.as_f32(), 42.);
            assert_eq!(button.size.height.as_f32(), 42.);
        }
    });
    ui.open_welcome_tab(0);
    assert_eq!(ui.tab_kinds(0), ["welcome"]);

    // A click switches the window: the tree and the layout follow.
    // Timed from the click (after the harness's hover and press frames) to
    // the new layout's first frame, polling tightly.
    ui.click(0, ("workspace", 0usize));
    let started = Instant::now();
    while ui.current_name(0).as_deref() != Some("alpha") {
        assert!(started.elapsed() < TIMEOUT, "timed out switching");
        ui.cx.run_until_parked();
        std::thread::yield_now();
    }
    ui.frame(0, |_, _| ());
    let switched = started.elapsed();
    ui.wait_for("alpha's tree", |ui| ui.rows(0) == ["README.md"]);
    assert!(ui.tab_kinds(0).is_empty(), "alpha has a layout of its own");
    // The rail keeps its order while switching.
    assert_eq!(ui.rail_names(0), ["alpha", "beta-service", "demo-project"]);
    println!("switched workspaces in {switched:?}");
    // The budget is 100 ms, held in optimised builds
    // (`cargo test --release -p coqo -- rail::tests::lists`). An unoptimised
    // build on a busy CI runner draws frames many times slower, so it's only
    // reported there.
    if !cfg!(debug_assertions) {
        assert!(
            switched < Duration::from_millis(100),
            "switching took {switched:?}"
        );
    }

    // Ctrl/Cmd+2 and +3 switch too, and the first workspace's layout comes
    // back.
    ui.press(0, &format!("{MOD}-2"));
    ui.wait_for("beta", |ui| {
        ui.current_name(0).as_deref() == Some("beta-service") && ui.rows(0) == ["README.md"]
    });
    ui.press(0, &format!("{MOD}-3"));
    ui.wait_for("the project again", |ui| {
        ui.current_name(0).as_deref() == Some("demo-project")
            && ui.rows(0).contains(&"src".to_owned())
    });
    assert_eq!(ui.tab_kinds(0), ["welcome"]);
    // A key past the end does nothing.
    ui.press(0, &format!("{MOD}-9"));
    ui.cx.run_until_parked();
    assert_eq!(ui.current_name(0).as_deref(), Some("demo-project"));

    // A workspace another window shows is focused there, not taken over.
    let second = {
        let opened = ui.cx.update(windows::open).expect("open a window");
        ui.windows.push(opened);
        ui.windows.len() - 1
    };
    ui.click(second, ("workspace", 0usize));
    ui.wait_for("alpha in the second window", |ui| {
        ui.current_name(second).as_deref() == Some("alpha")
    });
    ui.cx
        .update_window(ui.window(0), |_, window, _| window.activate_window())
        .unwrap();
    ui.click(0, ("workspace", 0usize));
    ui.cx.run_until_parked();
    assert_eq!(ui.cx.read(|cx| cx.active_window()), Some(ui.window(second)));
    assert_eq!(ui.current_name(0).as_deref(), Some("demo-project"));
}

#[test]
fn background_agents_badge_their_workspace() {
    let release = Arc::new(Notify::new());
    let provider = ScriptedProvider::new()
        .turn([Step::Wait(release.clone()), Step::Permission(ask())])
        .turn([Step::Fail("npm ERR! ERESOLVE".into())]);
    let fixture = Fixture::new(&provider);
    let other = fixture.folder("other-service");
    let mut ui = start_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for("the workspace", |ui| ui.current(0).is_some());
    let (project, _) = ui.current(0).unwrap();
    let other_id = fixture.open_in_daemon(&other);
    ui.wait_for("both on the rail", |ui| ui.rail_buttons(0) == 2);
    assert_eq!(ui.badge(&project), (None, None));

    // A turn runs: the workspace's button shows a working dot.
    fixture.start_turn(&project, &fixture.project);
    let mine = ui.button_of(0, "demo-project");
    ui.wait_for("the working dot", |ui| {
        ui.shows(0, ("workspace-dot-working", mine))
    });
    assert_eq!(ui.badge(&project), (None, Some(Dot::Working)));

    // Switch away; the agent keeps going in the background and asks for
    // permission: the rail badges the workspace in sand.
    ui.click(0, ("workspace", ui.button_of(0, "other-service")));
    ui.wait_for("the other workspace", |ui| {
        ui.current_name(0).as_deref() == Some("other-service")
    });
    release.notify_one();
    ui.wait_for("the waiting badge", |ui| {
        ui.shows(0, ("workspace-badge-waiting", mine))
    });
    assert_eq!(ui.badge(&project), (Some(Badge::Waiting(1)), None));
    let theirs = ui.button_of(0, "other-service");
    assert!(!ui.shows(0, ("workspace-badge-waiting", theirs)));

    // A failed agent badges its workspace in rose.
    fixture.start_turn(&other_id, &other);
    ui.wait_for("the error badge", |ui| {
        ui.shows(0, ("workspace-badge-error", theirs))
    });
    assert_eq!(ui.badge(&other_id).0, Some(Badge::Error));
    // The waiting badge is still there.
    assert!(ui.shows(0, ("workspace-badge-waiting", mine)));
}

#[test]
fn the_context_menu_renames_removes_and_opens_in_a_new_window() {
    let fixture = Fixture::new(&ScriptedProvider::new());
    let alpha = fixture.folder("alpha");
    let beta = fixture.folder("beta-service");
    let alpha_id = fixture.open_in_daemon(&alpha);
    fixture.open_in_daemon(&beta);
    let mut ui = start_ui(&fixture, Some(fixture.project.clone()));
    ui.wait_for("the rail", |ui| {
        ui.current(0).is_some() && ui.rail_buttons(0) == 3
    });

    // Right-click opens the menu; a click outside closes it.
    ui.right_click(0, ("workspace", 0usize));
    assert!(ui.shows(0, "workspace-menu"));
    assert_eq!(
        ui.cx.read(|cx| ui.rail(0).read(cx).menu().cloned()),
        Some(alpha_id.clone())
    );
    ui.click(0, "main");
    assert!(!ui.shows(0, "workspace-menu"));

    // Rename: the field starts with the name; Enter sends the new one.
    ui.right_click(0, ("workspace", 0usize));
    ui.click(0, "workspace-menu-rename");
    assert!(ui.shows(0, "workspace-rename"));
    let rail = ui.rail(0);
    let input = ui
        .cx
        .read(|cx| rail.read(cx).rename_input().cloned())
        .unwrap();
    assert_eq!(ui.cx.read(|cx| input.read(cx).text(cx)), "alpha");
    ui.cx
        .update_window(ui.window(0), |_, window, cx| {
            input.update(cx, |input, cx| input.set_text("Alpha app", window, cx));
        })
        .unwrap();
    // The field has the focus, so the menu's click didn't reach the tree
    // behind it.
    let focused = ui.frame(0, |window, cx| {
        coqo_ui::gpui::Focusable::focus_handle(input.read(cx), cx).is_focused(window)
    });
    assert!(focused);
    ui.press(0, "enter");
    ui.wait_for("the new name", |ui| {
        ui.rail_names(0)[0] == "Alpha app" && !ui.shows(0, "workspace-rename")
    });

    // Esc cancels a rename.
    ui.right_click(0, ("workspace", 0usize));
    ui.click(0, "workspace-menu-rename");
    ui.press(0, "escape");
    assert!(!ui.shows(0, "workspace-rename"));
    assert_eq!(ui.rail_names(0)[0], "Alpha app");

    // Open in a new window: only for a workspace no window shows.
    ui.right_click(0, ("workspace", 2usize));
    ui.click(0, "workspace-menu-new-window");
    assert_eq!(ui.cx.windows().len(), 1, "demo-project is shown here");
    ui.click(0, "main");
    ui.right_click(0, ("workspace", 1usize));
    ui.click(0, "workspace-menu-new-window");
    ui.cx.run_until_parked();
    assert_eq!(ui.cx.windows().len(), 2);
    let second = ui.cx.update(|cx| {
        let handle = cx.windows()[1];
        let shell = handle
            .update(cx, |root, _, cx| {
                let root = root.downcast::<coqo_ui::gpui::base::Root>().unwrap();
                root.read(cx).view().clone().downcast::<Shell>().unwrap()
            })
            .unwrap();
        (handle, shell)
    });
    ui.windows.push(second);
    ui.wait_for("beta in the new window", |ui| {
        ui.current_name(1).as_deref() == Some("beta-service") && ui.rows(1) == ["README.md"]
    });

    // Remove from list: gone from every rail, the folder untouched.
    ui.right_click(0, ("workspace", 0usize));
    ui.click(0, "workspace-menu-remove");
    ui.wait_for("alpha to leave the rails", |ui| {
        ui.rail_names(0) == ["beta-service", "demo-project"]
            && ui.rail_names(1) == ["beta-service", "demo-project"]
    });
    assert!(alpha.join("README.md").exists());
    let known = fixture.daemon.with_client(async |client| {
        client
            .request::<ws::List>(())
            .await
            .unwrap()
            .workspaces
            .into_iter()
            .map(|w| w.id)
            .collect::<Vec<_>>()
    });
    assert!(!known.contains(&alpha_id));

    // Removing the window's own workspace leaves it with none.
    ui.open_welcome_tab(1);
    ui.right_click(1, ("workspace", 0usize));
    ui.click(1, "workspace-menu-remove");
    ui.wait_for("the second window to show no workspace", |ui| {
        ui.current(1).is_none() && ui.rail_names(1) == ["demo-project"]
    });
    assert!(beta.join("README.md").exists());
    assert!(ui.tab_kinds(1).is_empty());
    assert_eq!(
        ui.cx
            .read(|cx| ui.shell(1).read(cx).layout_key().to_string()),
        DEFAULT_WORKSPACE
    );
    ui.frame(1, |window, _| {
        assert!(window.find("workspace-name").visible());
    });
    // The first window still shows its own.
    assert_eq!(ui.current_name(0).as_deref(), Some("demo-project"));
    assert!(
        ui.cx
            .read(|cx| KnownWorkspaces::global(cx).unwrap().read(cx).all().len())
            == 1
    );
}
