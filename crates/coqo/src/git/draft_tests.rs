// SPDX-License-Identifier: Apache-2.0

//! The Git pane's "Draft" button (#224) against the daemon's git, review and
//! session services, with a scripted agent, on a fixture repository.

use std::{
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use coqo_agents::{
    AgentUpdate, PermissionAsk,
    testing::{Call, ScriptedProvider, Step, test_config_options},
};
use coqo_client::DaemonPaths;
use coqo_mod_git::testing::Fixture as Repo;
use coqo_protocol::methods::agents::{
    self as agm, ConfigValue, ContentBlock, ListSessionsParams, PermissionOption,
    PermissionOptionKind, PermissionOutcome, ProviderStatus, StopReason,
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
use tokio::sync::Notify;

use super::{
    draft::{DraftState, SESSION_TITLE},
    pane::{GitPane, Group},
};
use crate::{
    daemon::{self, DaemonConfig},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{AgentDaemon, TIMEOUT},
    workspace,
};

const DRAFT: &str = "docs(readme): describe the demo\n\nSay what the demo shows.";

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    daemon: AgentDaemon,
    _tmp: tempfile::TempDir,
}

/// A repository with `README.md` changed and staged, and a daemon whose
/// "fake" agent is `provider`.
fn open(provider: &ScriptedProvider) -> (Ui, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    repo.write("README.md", "# Demo\n");
    repo.commit_all("feat: start the demo");
    repo.write("README.md", "# Demo\n\nWhat it shows.\n");
    repo.git(&["add", "README.md"]);
    let daemon = AgentDaemon::with_review(&tmp.path().join("d"), provider);
    let paths = daemon.paths.clone();
    let ui = open_ui(repo.path().to_owned(), daemon, paths, tmp);
    (ui, repo)
}

fn open_ui(folder: PathBuf, daemon: AgentDaemon, paths: DaemonPaths, tmp: tempfile::TempDir) -> Ui {
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
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

    fn pane(&mut self) -> Entity<GitPane> {
        self.frame(|_, _| ());
        self.cx
            .read(|cx| self.shell.read(cx).sidebar_pane("git"))
            .and_then(|view| view.downcast::<GitPane>().ok())
            .expect("the git pane is shown")
    }

    fn click(&mut self, id: &'static str) {
        self.frame(|window, cx| window.click(id, cx));
    }

    /// The Git pane, once README.md shows as staged.
    fn show_git(&mut self) -> Entity<GitPane> {
        self.click("sidebar-git");
        self.wait_for("README.md staged", |ui| {
            let pane = ui.pane();
            ui.cx
                .read(|cx| pane.read(cx).rows(Group::Staged, cx).len() == 1)
        });
        self.pane()
    }

    fn state(&mut self, pane: &Entity<GitPane>) -> DraftState {
        self.cx
            .read(|cx| pane.read(cx).drafter().read(cx).state().clone())
    }

    fn message(&mut self, pane: &Entity<GitPane>) -> String {
        self.cx
            .read(|cx| pane.read(cx).message_input().read(cx).text(cx))
    }

    fn unavailable(&mut self, pane: &Entity<GitPane>) -> Option<String> {
        self.cx.read(|cx| {
            let pane = pane.read(cx);
            let model = pane.model().read(cx);
            let staged = model
                .shown_root()
                .and_then(|r| model.root(r))
                .map_or(0, |r| r.staged().count());
            pane.drafter().read(cx).unavailable(staged, cx)
        })
    }

    /// Every session the daemon knows, closed ones too.
    fn sessions(&self) -> Vec<agm::SessionInfo> {
        self.daemon.with_client(async |client| {
            client
                .request::<agm::List>(ListSessionsParams {
                    workspace: None,
                    include_closed: true,
                })
                .await
                .unwrap()
                .sessions
        })
    }
}

fn reply(text: &str) -> Step {
    Step::Update(AgentUpdate::AgentMessageChunk(ContentBlock::text(text)))
}

fn prompt_text(calls: &[Call]) -> String {
    calls
        .iter()
        .find_map(|c| match c {
            Call::Prompt(content) => Some(
                content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .expect("a prompt was sent")
}

#[test]
fn a_draft_fills_the_commit_box_and_commits_nothing() {
    // The user's default mode is one that doesn't ask ("plan" stands in).
    let mut modes = coqo_agents::testing::test_modes();
    modes.current_mode_id = "plan".into();
    let provider = ScriptedProvider::new()
        .on_start([
            AgentUpdate::Modes(modes),
            AgentUpdate::ConfigOptions(test_config_options()),
        ])
        .turn([
            // A tool it asks for is turned down.
            Step::Permission(PermissionAsk {
                title: "Run git log".into(),
                tool_call: None,
                options: vec![
                    PermissionOption {
                        id: "yes".into(),
                        name: "Allow".into(),
                        kind: PermissionOptionKind::AllowOnce,
                    },
                    PermissionOption {
                        id: "no".into(),
                        name: "Reject".into(),
                        kind: PermissionOptionKind::RejectOnce,
                    },
                ],
                questions: Vec::new(),
            }),
            reply("```\ndocs(readme): describe the demo\n\n"),
            reply("Say what the demo shows.\n```"),
            Step::Stop(StopReason::EndTurn),
        ]);
    let (mut ui, repo) = open(&provider);
    let pane = ui.show_git();
    ui.wait_for("the agent", |ui| ui.unavailable(&pane).is_none());

    ui.click("git-draft");
    assert_eq!(ui.state(&pane), DraftState::Drafting);
    ui.wait_for("the draft", |ui| ui.message(&pane) == DRAFT);
    assert_eq!(ui.state(&pane), DraftState::Idle);

    // The agent got the staged diff and the repository's style, in a
    // session of its own, in the mode that asks before every change, then
    // on the cheapest settings.
    let calls = provider.calls();
    let prompt = prompt_text(&calls);
    assert!(prompt.contains("Conventional Commits"), "{prompt}");
    assert!(prompt.contains("- feat: start the demo\n"), "{prompt}");
    assert!(prompt.contains("- README.md (modified, +2 −0)"), "{prompt}");
    assert!(prompt.contains("+What it shows."), "{prompt}");
    assert!(calls.contains(&Call::SetConfigOption(
        "effort".into(),
        ConfigValue::Value("low".into())
    )));
    let mode = calls
        .iter()
        .position(|c| *c == Call::SetMode("default".into()));
    let effort = calls
        .iter()
        .position(|c| matches!(c, Call::SetConfigOption(id, _) if id == "effort"));
    assert!(
        mode.is_some() && mode < effort,
        "the mode first, then the settings: {calls:?}"
    );
    assert_eq!(
        provider.outcomes(),
        [PermissionOutcome::Selected {
            option_id: "no".into()
        }]
    );
    ui.wait_for("the session closed", |ui| {
        ui.sessions().iter().all(|s| s.closed)
    });
    let sessions = ui.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title.as_deref(), Some(SESSION_TITLE));
    assert_eq!(sessions[0].workspace, None, "not one of the user's chats");

    // Nothing was committed: the change is still staged.
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
    assert_eq!(
        repo.git(&["diff", "--cached", "--name-only"]),
        "README.md\n"
    );

    // The draft is the user's to edit, then commit.
    let input = ui.cx.read(|cx| pane.read(cx).message_input().clone());
    ui.frame(|window, cx| {
        input.update(cx, |i, cx| {
            i.set_text("docs(readme): say what the demo shows", window, cx);
        });
    });
    ui.click("git-commit");
    ui.wait_for("the commit", |_| {
        repo.git(&["rev-list", "--count", "HEAD"]).trim() == "2"
    });
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s"]).trim(),
        "docs(readme): say what the demo shows"
    );
}

#[test]
fn the_command_shows_the_git_pane_and_drafts() {
    let provider = ScriptedProvider::new().turn([
        reply("docs(readme): describe the demo"),
        Step::Stop(StopReason::EndTurn),
    ]);
    let (mut ui, _repo) = open(&provider);
    // The Files pane is shown; the badge's model sees what is staged.
    ui.wait_for("README.md staged", |ui| {
        let badge = ui.frame(|window, cx| {
            super::chrome::GitChrome::for_window(super::chrome::ChromePart::Badge, window, cx)
        });
        ui.cx.read(|cx| badge.read(cx).changed_files(cx) == Some(1))
    });
    assert!(
        ui.cx
            .read(|cx| ui.shell.read(cx).sidebar_pane("git"))
            .is_none()
    );
    ui.frame(|window, cx| {
        window.dispatch_action(Box::new(super::DraftCommitMessage), cx);
    });
    let pane = ui.pane();
    ui.wait_for("the draft", |ui| {
        ui.message(&pane) == "docs(readme): describe the demo"
    });
    let selected = ui.cx.read(|cx| ui.shell.read(cx).layout(cx).sidebar.pane);
    assert_eq!(selected.as_deref(), Some("git"));
}

#[test]
fn a_draft_can_be_cancelled() {
    let gate = Arc::new(Notify::new());
    let provider = ScriptedProvider::new().turn([
        Step::Wait(Arc::clone(&gate)),
        reply("feat: never shown"),
        Step::Stop(StopReason::EndTurn),
    ]);
    let (mut ui, repo) = open(&provider);
    let pane = ui.show_git();
    ui.wait_for("the agent", |ui| ui.unavailable(&pane).is_none());
    let input = ui.cx.read(|cx| pane.read(cx).message_input().clone());
    ui.frame(|window, cx| input.update(cx, |i, cx| i.set_text("wip", window, cx)));

    ui.click("git-draft");
    ui.wait_for("the turn", |_| {
        provider
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Prompt(_)))
    });
    // "Drafting…" stops it.
    ui.click("git-draft");
    assert_eq!(ui.state(&pane), DraftState::Idle);
    ui.wait_for("the session closed", |ui| {
        let sessions = ui.sessions();
        !sessions.is_empty() && sessions.iter().all(|s| s.closed)
    });
    gate.notify_one();
    ui.cx.run_until_parked();
    std::thread::sleep(Duration::from_millis(50));
    ui.cx.run_until_parked();
    assert_eq!(ui.message(&pane), "wip", "the commit box keeps what it had");
    assert_eq!(ui.state(&pane), DraftState::Idle);
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
}

#[test]
fn without_an_agent_the_button_is_off() {
    let provider = ScriptedProvider::new().status(ProviderStatus {
        ready: false,
        message: Some("Install Claude Code first".into()),
        actions: Vec::new(),
        sign_in: None,
    });
    let (mut ui, repo) = open(&provider);
    let pane = ui.show_git();
    ui.wait_for("the providers", |ui| {
        ui.unavailable(&pane)
            .is_some_and(|why| why.contains("Install Claude Code first"))
    });
    ui.click("git-draft");
    ui.cx.run_until_parked();
    assert_eq!(ui.state(&pane), DraftState::Idle);
    assert!(provider.calls().is_empty(), "no session was started");
    assert!(ui.sessions().is_empty());
    assert_eq!(ui.message(&pane), "");
    assert!(ui.frame(|window, _| window.find("git-draft").visible()));
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
}

/// Draft a message with the real Claude Code, through a `coqod` already
/// serving at `COQO_RUNTIME_DIR` (with the Claude ACP adapter in its
/// `COQO_DATA_DIR`), on a fixture repository. Run by hand:
/// `COQO_RUNTIME_DIR=… cargo test -p coqo -- --ignored real_claude_drafts`.
#[test]
#[ignore = "needs Claude Code and a running coqod; run manually"]
fn real_claude_drafts_a_message() {
    let runtime = std::env::var_os("COQO_RUNTIME_DIR").expect("COQO_RUNTIME_DIR");
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    repo.write(
        "src/cart.rs",
        "pub fn total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n",
    );
    repo.commit_all("feat(cart): add the cart total");
    repo.write(
        "src/cart.rs",
        "/// The total, with 10% off from 10 items.\npub fn total(items: &[u32]) -> u32 {\n    let sum: u32 = items.iter().sum();\n    if items.len() >= 10 { sum * 9 / 10 } else { sum }\n}\n",
    );
    repo.git(&["add", "src/cart.rs"]);
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let folder = repo.path().to_owned();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(DaemonPaths::at(PathBuf::from(&runtime)));
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(Some(folder), cx);
        crate::shell::init(cx);
        coqo_ui::open_window(WindowOptions::default(), cx, |window, cx| {
            let store = Rc::new(MemoryLayoutStore::default());
            cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
        })
        .expect("open shell window")
    });
    let mut ui = Ui {
        cx,
        window,
        shell,
        // Unused: the daemon is the real one.
        daemon: AgentDaemon::start(&tmp.path().join("unused"), &ScriptedProvider::new()),
        _tmp: tmp,
    };
    let pane = ui.show_git();
    ui.wait_for("Claude", |ui| ui.unavailable(&pane).is_none());
    ui.click("git-draft");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        ui.cx.run_until_parked();
        ui.frame(|_, _| ());
        match ui.state(&pane) {
            DraftState::Drafting => {}
            DraftState::Idle => break,
            DraftState::Failed(err) => panic!("the draft failed: {err}"),
        }
        assert!(Instant::now() < deadline, "no draft in time");
        std::thread::sleep(Duration::from_millis(100));
    }
    let message = ui.message(&pane);
    println!("--- drafted ---\n{message}\n---------------");
    assert!(!message.trim().is_empty());
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
}
