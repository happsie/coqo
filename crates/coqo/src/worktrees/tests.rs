// SPDX-License-Identifier: Apache-2.0

//! The worktree switcher and the project rail (#92) against the daemon's
//! workspace, git and session services, with scripted agents, on a
//! fixture repository: two agents work in two worktrees of one repository
//! without touching each other's files, a worktree made outside Coqo shows
//! up, and removal never forces and never takes the main worktree.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use coqo_agents::{
    model::{ContentBlock, Create, CreateSessionParams, Prompt, PromptParams},
    testing::{ScriptedProvider, Step},
};
use coqo_mod_git::testing::Fixture as Repo;
use coqo_protocol::methods::{
    agents::{AgentState, StopReason},
    workspace::{WorkspaceId, WorkspaceInfo},
};
use coqo_ui::{
    Appearance,
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Keystroke, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use tokio::sync::Notify;

use super::WorktreeSwitcher;
use crate::{
    activity::AgentActivity,
    daemon::{self, DaemonConfig},
    shell::{
        DEFAULT_WORKSPACE, Shell,
        rail::{Badge, Dot},
    },
    test_support::{AgentDaemon, TIMEOUT},
    workspace::{self, KnownWorkspaces},
};

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    daemon: AgentDaemon,
    repo: Repo,
    tmp: tempfile::TempDir,
}

/// A repository with one commit, a daemon whose "fake" agent is
/// `provider`, and a UI showing the repository.
fn open(provider: &ScriptedProvider) -> Ui {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("shop"));
    repo.write("README.md", "# Shop\n");
    repo.write("src/lib.rs", "pub fn price() -> u32 { 100 }\n");
    repo.commit_all("feat: start the shop");
    let daemon = AgentDaemon::with_review(&tmp.path().join("d"), provider);
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let (paths, folder) = (daemon.paths.clone(), repo.path().to_owned());
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
    let mut ui = Ui {
        cx,
        window,
        shell,
        daemon,
        repo,
        tmp,
    };
    ui.wait_for("the repository's workspace", |ui| {
        ui.current().is_some_and(|w| w.project.is_some()) && ui.shows("worktree-switcher")
    });
    ui
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

    fn shows(&mut self, id: impl Into<ElementId>) -> bool {
        let id = id.into();
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn click(&mut self, id: impl Into<ElementId>) {
        let id = id.into();
        self.frame(|window, cx| window.click(id, cx));
    }

    fn press(&mut self, keys: &'static str) {
        self.frame(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
        });
    }

    fn switcher(&self) -> Entity<WorktreeSwitcher> {
        self.cx.read(|cx| self.shell.read(cx).switcher().clone())
    }

    fn current(&self) -> Option<WorkspaceInfo> {
        let workspaces = self.cx.read(|cx| self.shell.read(cx).workspaces().clone());
        self.cx.read(|cx| workspaces.read(cx).current().cloned())
    }

    fn current_folder(&self) -> Option<PathBuf> {
        self.current()
            .and_then(|w| w.roots.first().map(|r| PathBuf::from(&r.path)))
    }

    /// The rail's buttons, top to bottom, by name.
    fn rail_names(&self) -> Vec<String> {
        let rail = self.cx.read(|cx| self.shell.read(cx).rail().clone());
        self.cx
            .read(|cx| rail.read(cx).list(cx).into_iter().map(|p| p.name).collect())
    }

    /// The listed worktrees as `(folder name, branch, changes)`.
    fn listed(&self) -> Vec<(String, Option<String>, Option<u32>)> {
        let switcher = self.switcher();
        self.cx.read(|cx| {
            switcher
                .read(cx)
                .worktrees()
                .iter()
                .map(|w| {
                    let name = Path::new(&w.path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    (name, w.branch.clone(), w.changes)
                })
                .collect()
        })
    }

    /// Open the list and wait until it shows `count` worktrees.
    fn open_list(&mut self, count: usize) {
        if !self.cx.read(|cx| self.switcher().read(cx).is_open()) {
            self.click("worktree-switcher");
        }
        self.wait_for("the worktree list", |ui| {
            ui.shows("worktree-popover")
                && ui.cx.read(|cx| !ui.switcher().read(cx).is_busy())
                && ui.listed().len() == count
        });
    }

    /// The row of the worktree in folder `name`.
    fn row_of(&self, name: &str) -> usize {
        self.listed()
            .iter()
            .position(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("{name} is not listed: {:?}", self.listed()))
    }

    /// Add a worktree for `branch` through the form.
    fn add_worktree(&mut self, branch: &str, count_before: usize) {
        self.open_list(count_before);
        self.click("worktree-new");
        assert!(self.shows("worktree-new-form"));
        let (input, base) = self
            .cx
            .read(|cx| self.switcher().read(cx).form_inputs())
            .unwrap();
        // The base starts as the current branch.
        assert_eq!(self.cx.read(|cx| base.read(cx).text(cx)), "main");
        self.cx
            .update_window(self.window, |_, window, cx| {
                input.update(cx, |i, cx| i.set_text(branch.to_owned(), window, cx));
            })
            .unwrap();
        self.press("enter");
        let folder = branch.replace('/', "-");
        self.wait_for("the new worktree's workspace", |ui| {
            ui.current_folder()
                .is_some_and(|f| f.file_name().unwrap().to_string_lossy() == folder)
        });
        assert!(!self.shows("worktree-popover"));
    }

    /// Start a session in `workspace` (folder `cwd`) and prompt it.
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
                    from: None,
                })
                .await
                .unwrap();
            client
                .request::<Prompt>(PromptParams {
                    session: info.id,
                    content: vec![ContentBlock::text("Change the price")],
                })
                .await
                .unwrap();
        });
    }

    /// The states of the open sessions of `workspace`.
    fn states(&self, workspace: &WorkspaceId) -> Vec<AgentState> {
        self.cx.read(|cx| {
            AgentActivity::global(cx)
                .unwrap()
                .read(cx)
                .sessions()
                .filter(|s| s.workspace.as_deref() == Some(workspace.0.as_str()))
                .map(|s| s.state)
                .collect()
        })
    }

    /// What the rail's one project button shows.
    fn project_badge(&self) -> (Option<Badge>, Option<Dot>) {
        let rail = self.cx.read(|cx| self.shell.read(cx).rail().clone());
        self.cx.read(|cx| {
            let project = rail.read(cx).list(cx).remove(0);
            let activity = AgentActivity::global(cx).unwrap().read(cx);
            let sum = project
                .workspaces
                .iter()
                .map(|w| activity.of(&w.id.0))
                .fold(Default::default(), crate::activity::WorkspaceActivity::sum);
            (Badge::of(sum), Dot::of(sum))
        })
    }
}

#[test]
fn two_agents_work_in_two_worktrees_without_interference() {
    let (a_done, b_done) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let provider = ScriptedProvider::new()
        .turn([Step::Wait(a_done.clone()), Step::Stop(StopReason::EndTurn)])
        .turn([Step::Wait(b_done.clone()), Step::Stop(StopReason::EndTurn)]);
    let mut ui = open(&provider);
    let main = ui.current().unwrap();
    assert_eq!(ui.rail_names(), ["shop"]);
    // The header names the project and the worktree's branch.
    ui.frame(|window, _| assert!(window.find("worktree-header").visible()));
    ui.open_list(1);
    assert_eq!(
        ui.listed(),
        [("shop".to_owned(), Some("main".to_owned()), Some(0))]
    );
    assert!(
        !ui.shows(("worktree-remove", 0usize)),
        "the main worktree has no remove"
    );

    // A worktree from the switcher: its own workspace, the same project.
    ui.add_worktree("feature/discount", 1);
    let a = ui.current().unwrap();
    let a_dir = ui.current_folder().unwrap();
    assert_eq!(a.project, main.project);
    assert_eq!(
        a_dir,
        ui.tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("shop-worktrees/feature-discount")
    );

    // One made outside Coqo shows up in the list; a click opens it.
    let b_dir = ui.tmp.path().join("shop-hotfix");
    let b_str = b_dir.to_str().unwrap().to_owned();
    ui.repo
        .git(&["worktree", "add", "-q", "-b", "hotfix", &b_str]);
    ui.open_list(3);
    let row = ui.row_of("shop-hotfix");
    ui.click(("worktree-row", row));
    ui.wait_for("the hand-made worktree", |ui| {
        ui.current_folder()
            .is_some_and(|f| f.ends_with("shop-hotfix"))
    });
    let b = ui.current().unwrap();
    let b_dir = b_dir.canonicalize().unwrap();
    assert_eq!(b.project, main.project);
    // Three workspaces, one project on the rail.
    ui.wait_for("the rail to group them", |ui| {
        ui.cx
            .read(|cx| KnownWorkspaces::global(cx).unwrap().read(cx).all().len() == 3)
    });
    assert_eq!(ui.rail_names(), ["shop"]);

    // An agent in each; each edits its own copy while both work.
    ui.start_turn(&a.id, &a_dir);
    ui.start_turn(&b.id, &b_dir);
    ui.wait_for("both agents working", |ui| {
        ui.states(&a.id) == [AgentState::Working] && ui.states(&b.id) == [AgentState::Working]
    });
    std::fs::write(a_dir.join("src/lib.rs"), "pub fn price() -> u32 { 90 }\n").unwrap();
    std::fs::write(b_dir.join("src/lib.rs"), "pub fn price() -> u32 { 101 }\n").unwrap();
    std::fs::write(b_dir.join("CHANGELOG.md"), "Fixed rounding\n").unwrap();
    // The project's button sums both worktrees.
    ui.wait_for("the working dot", |ui| {
        ui.shows(("workspace-dot-working", 0usize))
    });
    assert_eq!(ui.project_badge(), (None, Some(Dot::Working)));

    // Switching back and forth keeps both running, and their files apart.
    for (name, dir, mine) in [
        ("feature-discount", &a_dir, "90"),
        ("shop-hotfix", &b_dir, "101"),
        ("feature-discount", &a_dir, "90"),
    ] {
        ui.open_list(3);
        let row = ui.row_of(name);
        ui.click(("worktree-row", row));
        ui.wait_for("the switch", |ui| ui.current_folder().as_ref() == Some(dir));
        assert_eq!(ui.states(&a.id), [AgentState::Working]);
        assert_eq!(ui.states(&b.id), [AgentState::Working]);
        let text = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(text.contains(mine), "{name} has its own copy: {text}");
    }
    ui.open_list(3);
    ui.wait_for("the change counts", |ui| {
        let listed = ui.listed();
        let changes = |n: &str| listed.iter().find(|(name, ..)| name == n).unwrap().2;
        changes("shop") == Some(0)
            && changes("feature-discount") == Some(1)
            && changes("shop-hotfix") == Some(2)
    });
    // Each row says what its agent does.
    let row = ui.row_of("shop-hotfix");
    assert!(ui.shows(("worktree-agent", row)));
    assert!(!ui.shows(("worktree-agent", ui.row_of("shop"))));
    assert_eq!(
        ui.repo.read("src/lib.rs"),
        "pub fn price() -> u32 { 100 }\n"
    );

    // Both finish on their own.
    a_done.notify_one();
    b_done.notify_one();
    ui.wait_for("both done", |ui| {
        ui.states(&a.id) == [AgentState::Done] && ui.states(&b.id) == [AgentState::Done]
    });
    assert_eq!(ui.project_badge(), (None, Some(Dot::Done)));
}

#[test]
fn removal_warns_never_forces_and_keeps_the_main_worktree() {
    let mut ui = open(&ScriptedProvider::new());
    // git's refusal shows in the list: `main` is checked out already.
    ui.open_list(1);
    ui.click("worktree-new");
    let (input, _) = ui
        .cx
        .read(|cx| ui.switcher().read(cx).form_inputs())
        .unwrap();
    ui.cx
        .update_window(ui.window, |_, window, cx| {
            input.update(cx, |i, cx| i.set_text("main", window, cx));
        })
        .unwrap();
    ui.click("worktree-create");
    ui.wait_for("git's refusal", |ui| ui.shows("worktree-error"));
    let error = ui
        .cx
        .read(|cx| ui.switcher().read(cx).error().map(str::to_owned))
        .unwrap();
    assert!(error.contains("main"), "{error}");
    assert!(ui.shows("worktree-new-form"), "the form stays to fix it");
    ui.press("escape");
    ui.wait_for("the form to close", |ui| !ui.shows("worktree-new-form"));

    ui.add_worktree("spike", 1);
    let spike = ui.current().unwrap();
    let spike_dir = ui.current_folder().unwrap();

    // Uncommitted work: refused, with no way to force it.
    std::fs::write(spike_dir.join("notes.txt"), "half done\n").unwrap();
    ui.open_list(2);
    let row = ui.row_of("spike");
    ui.click(("worktree-remove", row));
    ui.wait_for("the warning", |ui| ui.shows("worktree-removal"));
    let pending = ui
        .cx
        .read(|cx| ui.switcher().read(cx).pending_removal().cloned())
        .unwrap();
    assert_eq!((pending.changes, pending.unpushed), (1, 0));
    assert!(!ui.shows("worktree-remove-anyway"));
    ui.click("worktree-remove-cancel");
    assert!(!ui.shows("worktree-removal"));
    assert!(spike_dir.join("notes.txt").exists());

    // A commit no other branch has: removal waits for a confirmation.
    for args in [
        &["add", "-A"][..],
        &[
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@b",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "notes",
        ],
    ] {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&spike_dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    ui.open_list(2);
    let row = ui.row_of("spike");
    ui.click(("worktree-remove", row));
    ui.wait_for("the warning", |ui| ui.shows("worktree-remove-anyway"));
    let pending = ui
        .cx
        .read(|cx| ui.switcher().read(cx).pending_removal().cloned())
        .unwrap();
    assert_eq!((pending.changes, pending.unpushed), (0, 1));
    assert!(spike_dir.exists());

    // Confirmed: it goes, with its workspace, and the window shows the
    // main worktree.
    ui.click("worktree-remove-anyway");
    ui.wait_for("the main worktree", |ui| {
        ui.current_folder().as_deref() == Some(ui.repo.path())
    });
    assert!(!spike_dir.exists());
    ui.wait_for("the workspace to be forgotten", |ui| {
        ui.cx.read(|cx| {
            KnownWorkspaces::global(cx)
                .unwrap()
                .read(cx)
                .get(&spike.id)
                .is_none()
        })
    });
    assert!(
        ui.repo
            .git(&["branch", "--list", "spike"])
            .contains("spike")
    );
    ui.open_list(1);
    assert!(!ui.shows(("worktree-remove", 0usize)));
}
