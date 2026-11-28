// SPDX-License-Identifier: Apache-2.0

//! The agents overview and "welcome back" against a daemon served
//! in-process: the real workspace, session, buffer, question, walkthrough,
//! provenance and alert services, with `ScriptedProvider` as the agent
//! ([`AgentDaemon`]); the whole `coqod` for its status. Sessions in two
//! workspaces are started by another client (as if the UI had been
//! closed).

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use coqo_agents::{
    AgentUpdate, PermissionAsk,
    model::{
        ContentBlock, CreateSessionParams, PermissionOption, PermissionOptionKind, PromptParams,
        SessionId, StopReason, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus,
        ToolCallUpdate, ToolKind,
    },
    testing::{ScriptedProvider, Step},
};
use coqo_client::DaemonPaths;
use coqo_protocol::methods::{
    agents::{self as agm, AgentState, ChangedSinceParams, ListSessionsParams, SessionInfo},
    editor::LineRange,
    questions::{self as qm, CreateParams, NewAnchor, QuestionId, QuestionKind},
    walkthroughs::{self as wtm, WalkthroughId},
    workspace::{self as wsm, OpenParams, WorkspaceInfo},
};
use coqo_ui::{
    Appearance,
    dock::{TabItem as _, store::MemoryLayoutStore},
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Keystroke, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    serde_json::json,
    test::{TestAppContext, TestWindowExt as _},
};
use tempfile::TempDir;
use tokio::sync::Notify;

use super::{
    KIND,
    away::{self, Away, MemoryLastSeen, Presence},
    view::AgentsTab,
};
use crate::{
    alerts::now_ms,
    chat::{self, tab::ChatTab},
    daemon::{self, DaemonConfig},
    files::editor::{self, FileEditor},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::AgentDaemon,
    walkthrough::player::{self, WalkthroughPlayer},
    workspace::{self, Workspaces},
};

const TIMEOUT: Duration = Duration::from_secs(10);
const MIN: i64 = 60_000;

/// Two project folders (workspaces A and B) and a daemon.
struct Fixture {
    _tmp: TempDir,
    a: PathBuf,
    b: PathBuf,
    daemon: AgentDaemon,
}

fn project(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
    dir.canonicalize().unwrap()
}

impl Fixture {
    fn new(provider: &ScriptedProvider) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let a = project(tmp.path(), "checkout-service");
        let b = project(tmp.path(), "web-storefront");
        let daemon = AgentDaemon::start(tmp.path(), provider);
        Self {
            _tmp: tmp,
            a,
            b,
            daemon,
        }
    }

    /// Open (or create) the workspace of `folder` in the daemon.
    fn open_workspace(&self, folder: &Path) -> WorkspaceInfo {
        let path = folder.to_string_lossy().into_owned();
        self.daemon.with_client(async move |client| {
            client
                .request::<wsm::Open>(OpenParams {
                    path: Some(path),
                    ..OpenParams::default()
                })
                .await
                .unwrap()
        })
    }

    /// Start a session titled `title` in `ws` and prompt it, from another
    /// client.
    fn start_session(&self, ws: &WorkspaceInfo, title: &str) -> SessionId {
        let cwd = ws.roots[0].path.clone();
        let (ws, title) = (ws.id.0.clone(), title.to_owned());
        self.daemon.with_client(async move |client| {
            let id = client
                .request::<agm::Create>(CreateSessionParams {
                    provider: "fake".into(),
                    cwd,
                    workspace: Some(ws),
                    title: Some(title),
                    mode: None,
                    from: None,
                })
                .await
                .unwrap()
                .id;
            client
                .request::<agm::Prompt>(PromptParams {
                    session: id.clone(),
                    content: vec![ContentBlock::text("go")],
                })
                .await
                .unwrap();
            id
        })
    }

    /// The agent of `session` asks about line 2 of `src/main.rs` in `ws`.
    fn ask_question(&self, ws: &WorkspaceInfo, session: &SessionId) -> QuestionId {
        let (wsid, root, session) = (ws.id.clone(), ws.roots[0].id, session.0.clone());
        self.daemon.with_client(async move |client| {
            client
                .request::<qm::Create>(CreateParams {
                    workspace: wsid,
                    session,
                    kind: QuestionKind::Clarification,
                    body: "Loyalty bonus before or after the **cap**?".into(),
                    options: vec![],
                    severity: None,
                    anchors: vec![NewAnchor {
                        root,
                        path: "src/main.rs".into(),
                        lines: LineRange { start: 1, end: 1 },
                    }],
                })
                .await
                .unwrap()
                .id
        })
    }

    /// The agent of `session` publishes a one-step walkthrough of
    /// `src/main.rs` in `ws`, as its bridge's `publish_walkthrough` would.
    fn publish_walkthrough(&self, ws: &WorkspaceInfo, session: &SessionId) -> WalkthroughId {
        let params = wtm::CreateParams {
            workspace: ws.id.clone(),
            session: session.0.clone(),
            walkthrough: json!({
                "title": "Retry policy for 3DS callbacks",
                "summary": "Callbacks are retried with a backoff.",
                "scope": { "kind": "worktree" },
                "steps": [{
                    "id": "entry",
                    "title": "Where the retry starts",
                    "kind": "entry_point",
                    "anchors": [{ "path": "src/main.rs", "start_line": 1, "end_line": 2 }],
                    "what": "Retries the callback.",
                    "why": "Banks time out.",
                }],
            }),
        };
        self.daemon.with_client(async move |client| {
            client.request::<wtm::Create>(params).await.unwrap().id
        })
    }

    fn sessions(&self) -> Vec<SessionInfo> {
        self.daemon.with_client(async move |client| {
            client
                .request::<agm::List>(ListSessionsParams::default())
                .await
                .unwrap()
                .sessions
        })
    }

    /// Wait until `session` is in `state`, from the daemon's side.
    fn wait_state(&self, session: &SessionId, state: AgentState) {
        let deadline = Instant::now() + TIMEOUT;
        while !self
            .sessions()
            .iter()
            .any(|s| &s.id == session && s.state == state)
        {
            assert!(Instant::now() < deadline, "{session:?} never got {state:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
}

/// Open the UI on workspace A. `last_seen` says when it was last attached.
fn open_ui(fixture: &Fixture, last_seen: Rc<MemoryLastSeen>) -> Ui {
    open_ui_at(fixture.daemon.paths.clone(), fixture.a.clone(), last_seen)
}

/// Open the UI on `folder`, with the daemon at `paths`.
fn open_ui_at(paths: DaemonPaths, folder: PathBuf, last_seen: Rc<MemoryLastSeen>) -> Ui {
    let mut cx = TestAppContext::single();
    cx.executor().allow_parking();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        away::init(last_seen, cx);
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
    cx.update_window(window, |_, window, _| window.activate_window())
        .unwrap();
    let mut ui = Ui { cx, window, shell };
    ui.wait_for("the workspace", |ui| ui.workspace().is_some());
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

    fn workspace(&mut self) -> Option<WorkspaceInfo> {
        self.cx
            .read(|cx| Workspaces::global(cx)?.read(cx).current().cloned())
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

    /// The window's agents overview tab, if open.
    fn overview(&mut self) -> Option<Entity<AgentsTab>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == KIND)
                .find_map(|t| dock.tab(t.id)?.downcast::<AgentsTab>())
        })
    }

    /// The overview's rows, top to bottom.
    fn rows(&mut self) -> Vec<SessionId> {
        let Some(tab) = self.overview() else {
            return Vec::new();
        };
        self.cx
            .read(|cx| tab.read(cx).rows(cx).into_iter().map(|s| s.id).collect())
    }

    fn chat_sessions(&mut self) -> Vec<SessionId> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == chat::KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<ChatTab>())
                .map(|t| t.read(cx).session().clone())
                .collect()
        })
    }

    fn file_tabs(&mut self) -> Vec<String> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == editor::KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
                .map(|t| t.read(cx).path().to_owned())
                .collect()
        })
    }

    fn players(&mut self) -> Vec<Entity<WalkthroughPlayer>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == player::KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<WalkthroughPlayer>())
                .collect()
        })
    }

    fn welcome(&mut self) -> Option<away::WelcomeBack> {
        self.cx
            .read(|cx| Presence::global(cx).unwrap().read(cx).welcome().cloned())
    }
}

fn ask() -> PermissionAsk {
    PermissionAsk {
        title: "`cargo test`".into(),
        tool_call: None,
        options: vec![
            PermissionOption {
                id: "allow-once".into(),
                name: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                id: "reject-once".into(),
                name: "Reject".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
    }
}

#[test]
fn the_overview_groups_sessions_of_every_workspace_and_leads_to_them() {
    let (release_ask, release_work) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let provider = ScriptedProvider::new()
        // Needs permission (A), then done.
        .turn([Step::Permission(ask()), Step::Stop(StopReason::EndTurn)])
        // Waits on a question (B).
        .turn([
            Step::Wait(Arc::clone(&release_ask)),
            Step::Stop(StopReason::EndTurn),
        ])
        // Working (B).
        .turn([
            Step::Wait(Arc::clone(&release_work)),
            Step::Stop(StopReason::EndTurn),
        ])
        // Failed (A).
        .turn([Step::Fail("npm ERR! ERESOLVE".into())])
        // Done (A).
        .turn([Step::Stop(StopReason::EndTurn)]);
    let fixture = Fixture::new(&provider);
    let a = fixture.open_workspace(&fixture.a);
    let b = fixture.open_workspace(&fixture.b);
    let perm = fixture.start_session(&a, "Fix flaky OrderControllerIT");
    fixture.wait_state(&perm, AgentState::NeedsPermission);
    let asks = fixture.start_session(&b, "Volume discount");
    let question = fixture.ask_question(&b, &asks);
    fixture.wait_state(&asks, AgentState::NeedsInput);
    let work = fixture.start_session(&b, "Cart discount line");
    let fail = fixture.start_session(&a, "Storybook upgrade");
    fixture.wait_state(&fail, AgentState::Error);
    let done = fixture.start_session(&a, "Retry policy");
    fixture.wait_state(&done, AgentState::Done);

    let mut ui = open_ui(&fixture, Rc::new(MemoryLastSeen::at(None)));
    assert_eq!(ui.workspace().unwrap().id, a.id);
    assert!(
        ui.welcome().is_none(),
        "the first start ever has nothing to say"
    );

    // The rail's button opens the overview: what needs you first (longest
    // waiting first), then errors, working, done.
    ui.click(ElementId::Name("rail-agents".into()));
    ui.wait_for("the overview", |ui| ui.overview().is_some());
    let expected = [&perm, &asks, &fail, &work, &done].map(Clone::clone);
    ui.wait_for("every session in order", |ui| ui.rows() == expected);
    assert!(ui.shows(ElementId::Name("agents-overview".into())));
    // The question is counted, and the permission request offers Allow.
    let tab = ui.overview().unwrap();
    ui.wait_for("the permission request", |ui| {
        ui.cx.read(|cx| tab.read(cx).ask(&perm).is_some())
    });
    assert!(ui.shows(("agents-deny", 0usize)));

    // Allowing it from the overview: the session finishes and moves down,
    // live.
    ui.click(("agents-primary", 0usize));
    fixture.wait_state(&perm, AgentState::Done);
    let expected = [&asks, &fail, &work, &perm, &done].map(Clone::clone);
    ui.wait_for("the allowed session to move down", |ui| {
        ui.rows() == expected
    });

    // Keyboard: Down, Down selects the failed one; Enter opens its chat.
    let focus = ui.cx.read(|cx| tab.read(cx).focus_handle(cx)).unwrap();
    ui.frame(|window, cx| window.focus(&focus, cx));
    ui.press("down");
    ui.press("down");
    let selected = ui.cx.read(|cx| tab.read(cx).selected().cloned());
    assert_eq!(selected.as_ref(), Some(&fail));
    ui.press("enter");
    ui.wait_for("the failed session's chat", |ui| {
        ui.chat_sessions() == [fail.clone()]
    });

    // Answer, from workspace A, for the question in workspace B: the window
    // switches to B and opens the question in its file.
    ui.cx.update(|cx| {
        cx.update_window(ui.window, |_, window, cx| {
            window.dispatch_action(Box::new(super::ShowAgents), cx);
        })
        .unwrap();
    });
    ui.wait_for("the overview again", |ui| {
        ui.shows(ElementId::Name("agents-overview".into()))
    });
    assert_eq!(ui.rows().first(), Some(&asks));
    ui.click(("agents-primary", 0usize));
    ui.wait_for("workspace B", |ui| {
        ui.workspace().is_some_and(|w| w.id == b.id)
    });
    ui.wait_for("the question's file", |ui| {
        ui.file_tabs() == ["src/main.rs"]
    });
    ui.wait_for("its popover", |ui| {
        let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
        ui.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
                .any(|f| f.read(cx).popover_question(cx) == Some(question))
        })
    });

    release_ask.notify_one();
    release_work.notify_one();
}

/// Start a finished and a waiting session, as if while the UI was closed.
fn away_fixture() -> (Fixture, SessionId, SessionId) {
    let provider = ScriptedProvider::new()
        .turn([Step::Stop(StopReason::EndTurn)])
        .turn([Step::Permission(ask()), Step::Stop(StopReason::EndTurn)]);
    let fixture = Fixture::new(&provider);
    let a = fixture.open_workspace(&fixture.a);
    let b = fixture.open_workspace(&fixture.b);
    let done = fixture.start_session(&b, "Retry policy for 3DS callbacks");
    fixture.wait_state(&done, AgentState::Done);
    let waits = fixture.start_session(&a, "Fix flaky OrderControllerIT");
    fixture.wait_state(&waits, AgentState::NeedsPermission);
    (fixture, done, waits)
}

#[test]
fn welcome_back_sums_up_a_long_absence() {
    let (fixture, done, waits) = away_fixture();
    let last_seen = Rc::new(MemoryLastSeen::at(Some(now_ms() - 134 * MIN)));
    let mut ui = open_ui(&fixture, last_seen);

    ui.wait_for("the welcome back", |ui| {
        ui.shows(ElementId::Name("welcome-back".into()))
    });
    let welcome = ui.welcome().unwrap();
    assert_eq!(welcome.away, Away::Closed);
    let ids: Vec<_> = welcome.sessions.iter().map(|s| s.id.clone()).collect();
    assert_eq!(ids, [waits.clone(), done.clone()], "what needs you first");
    let sentence = welcome.sentence();
    assert!(
        sentence.starts_with(
            "In the last 2 h 14 m the daemon ran 2 sessions: 1 finished and 1 needs permission."
        ),
        "{sentence}"
    );

    // **Go to the first one** opens the waiting session's chat, and the
    // banner goes.
    ui.click(ElementId::Name("welcome-back-go".into()));
    ui.wait_for("its chat", |ui| ui.chat_sessions() == [waits.clone()]);
    ui.wait_for("the banner to go", |ui| {
        !ui.shows(ElementId::Name("welcome-back".into()))
    });
    assert!(ui.welcome().is_none());
}

#[test]
fn no_welcome_back_after_a_quick_reconnect() {
    use away::LastSeenStore as _;

    let (fixture, _, _) = away_fixture();
    let before = now_ms() - 2 * MIN;
    let store = Rc::new(MemoryLastSeen::at(Some(before)));
    let mut ui = open_ui(&fixture, Rc::clone(&store));
    ui.wait_for("the attach to be noted", |_| {
        store.load().is_some_and(|at| at > before)
    });
    ui.wait_for("the sessions", |ui| {
        ui.cx.read(|cx| {
            crate::activity::AgentActivity::global(cx)
                .is_some_and(|a| a.read(cx).sessions().count() == 2)
        })
    });
    assert!(ui.welcome().is_none());
    assert!(!ui.shows(ElementId::Name("welcome-back".into())));
}

/// An agent's edit of `path`: a tool call with its diff, completed.
fn edit(id: &str, path: &Path) -> [Step; 2] {
    let path = path.to_string_lossy().into_owned();
    [
        Step::Update(AgentUpdate::ToolCall(ToolCall {
            id: ToolCallId(id.into()),
            title: format!("Edit {path}"),
            name: None,
            kind: ToolKind::Edit,
            status: ToolCallStatus::Pending,
            content: vec![ToolCallContent::Diff {
                path,
                old_text: None,
                new_text: "fn main() {}\n".into(),
            }],
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
        })),
        Step::Update(AgentUpdate::ToolCallUpdate(ToolCallUpdate {
            id: ToolCallId(id.into()),
            status: Some(ToolCallStatus::Completed),
            ..ToolCallUpdate::default()
        })),
    ]
}

/// #265: the welcome back says how many files the agents changed while the
/// UI was away (`agents/changedSince`, from the daemon's provenance).
#[test]
fn welcome_back_counts_the_files_agents_changed() {
    let provider = ScriptedProvider::new();
    let fixture = Fixture::new(&provider);
    let a = fixture.open_workspace(&fixture.a);
    let mut turn: Vec<Step> = Vec::new();
    turn.extend(edit("t1", &fixture.a.join("src/main.rs")));
    turn.extend(edit("t2", &fixture.a.join("src/lib.rs")));
    // The same file again counts once.
    turn.extend(edit("t3", &fixture.a.join("src/main.rs")));
    turn.push(Step::Stop(StopReason::EndTurn));
    provider.push_turn(turn);
    let before = now_ms() - 134 * MIN;
    let done = fixture.start_session(&a, "Volume discount");
    fixture.wait_state(&done, AgentState::Done);
    let deadline = Instant::now() + TIMEOUT;
    while fixture.daemon.with_client(async move |client| {
        client
            .request::<agm::ChangedSince>(ChangedSinceParams {
                since: before,
                workspace: None,
            })
            .await
            .unwrap()
            .files
    }) < 2
    {
        assert!(Instant::now() < deadline, "the edits were never recorded");
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut ui = open_ui(&fixture, Rc::new(MemoryLastSeen::at(Some(before))));
    ui.wait_for("the welcome back", |ui| ui.welcome().is_some());
    let welcome = ui.welcome().unwrap();
    assert_eq!(welcome.files_changed, 2);
    assert_eq!(
        welcome.sentence(),
        "In the last 2 h 14 m the daemon ran 1 session: 1 finished. 2 files changed. \
         1 alert is unread."
    );
}

/// #265: a UI that stayed attached while the machine slept sees its next
/// heartbeat come late, and welcomes you back all the same.
#[test]
fn welcome_back_after_the_machine_slept() {
    let (fixture, done, waits) = away_fixture();
    let mut ui = open_ui(
        &fixture,
        Rc::new(MemoryLastSeen::at(Some(now_ms() - 2 * MIN))),
    );
    ui.wait_for("the sessions", |ui| {
        ui.cx.read(|cx| {
            crate::activity::AgentActivity::global(cx)
                .is_some_and(|a| a.read(cx).sessions().count() == 2)
        })
    });
    assert!(ui.welcome().is_none(), "attached a moment ago");

    // A heartbeat on time: nothing to say.
    ui.cx.executor().advance_clock(away::HEARTBEAT);
    ui.cx.run_until_parked();
    assert!(ui.welcome().is_none());

    // The machine sleeps half an hour: the next heartbeat is that late.
    ui.cx.update(|cx| {
        let presence = Presence::global(cx).unwrap();
        presence.update(cx, |p, _| p.set_last_seen_for_tests(now_ms() - 30 * MIN));
    });
    ui.cx.executor().advance_clock(away::HEARTBEAT);
    ui.wait_for("the welcome back", |ui| {
        ui.shows(ElementId::Name("welcome-back".into()))
    });
    let welcome = ui.welcome().unwrap();
    assert_eq!(welcome.away, Away::Asleep);
    assert_eq!(
        welcome.headline(),
        "Welcome back. Your agents kept working while you were away."
    );
    let ids: Vec<_> = welcome.sessions.iter().map(|s| s.id.clone()).collect();
    assert_eq!(ids, [waits, done]);
    let sentence = welcome.sentence();
    assert!(
        sentence.starts_with("In the last 30 m the daemon ran 2 sessions:"),
        "{sentence}"
    );
}

/// #265: the header's memory, from `daemon/status` of the whole `coqod`:
/// it and its child processes.
#[test]
fn the_overview_header_shows_the_daemons_memory() {
    let fixture = crate::test_support::Fixture::new();
    let mut ui = open_ui_at(
        fixture.daemon.paths.clone(),
        fixture.project.clone(),
        Rc::new(MemoryLastSeen::at(None)),
    );
    ui.click(ElementId::Name("rail-agents".into()));
    ui.wait_for("the overview", |ui| ui.overview().is_some());
    let tab = ui.overview().unwrap();
    ui.wait_for("the daemon's memory", |ui| {
        ui.cx
            .read(|cx| tab.read(cx).status().is_some_and(|s| s.memory.is_some()))
    });
    let memory = ui
        .cx
        .read(|cx| tab.read(cx).status().and_then(|s| s.memory))
        .unwrap();
    assert!(memory.rss_bytes > 0 && memory.processes >= 1, "{memory:?}");
    let facts = ui.cx.read(|cx| {
        let tab = tab.read(cx);
        tab.facts(&tab.rows(cx), now_ms())
    });
    assert!(facts.starts_with("coqod up "), "{facts}");
    assert!(facts.ends_with(" incl. language servers"), "{facts}");
    assert!(ui.shows(ElementId::Name("agents-facts".into())));
}

/// #265: a done session that published a walkthrough offers
/// **Walkthrough**, which opens the player (in the session's workspace);
/// one without falls back to **Open chat**.
#[test]
fn a_done_session_that_published_a_walkthrough_offers_it() {
    let provider = ScriptedProvider::new()
        .turn([Step::Stop(StopReason::EndTurn)])
        .turn([Step::Stop(StopReason::EndTurn)]);
    let fixture = Fixture::new(&provider);
    let a = fixture.open_workspace(&fixture.a);
    let b = fixture.open_workspace(&fixture.b);
    let plain = fixture.start_session(&a, "Cart discount line");
    fixture.wait_state(&plain, AgentState::Done);
    let shown = fixture.start_session(&b, "Retry policy for 3DS callbacks");
    fixture.wait_state(&shown, AgentState::Done);
    let walkthrough = fixture.publish_walkthrough(&b, &shown);

    let mut ui = open_ui(&fixture, Rc::new(MemoryLastSeen::at(None)));
    ui.click(ElementId::Name("rail-agents".into()));
    ui.wait_for("the overview", |ui| ui.overview().is_some());
    let expected = [shown.clone(), plain.clone()];
    ui.wait_for("both done sessions", |ui| ui.rows() == expected);
    let tab = ui.overview().unwrap();
    let walkthrough_of = |ui: &mut Ui, id: &SessionId| {
        ui.cx
            .read(|cx| tab.read(cx).walkthrough_of(&info_of(cx, &tab, id), cx))
    };
    ui.wait_for("the walkthrough", |ui| {
        walkthrough_of(ui, &shown).is_some_and(|w| w.id == walkthrough)
    });
    assert!(
        walkthrough_of(&mut ui, &plain).is_none(),
        "it published none"
    );
    let summary = walkthrough_of(&mut ui, &shown).unwrap();
    assert_eq!(
        super::walkthrough_line(&summary),
        "Walkthrough ready · 1 step"
    );

    // Without a walkthrough: **Open chat**.
    ui.click(("agents-primary", 1usize));
    ui.wait_for("its chat", |ui| ui.chat_sessions() == [plain.clone()]);

    // **Walkthrough**: the window switches to B and opens the player.
    ui.cx.update(|cx| {
        cx.update_window(ui.window, |_, window, cx| {
            window.dispatch_action(Box::new(super::ShowAgents), cx);
        })
        .unwrap();
    });
    ui.wait_for("the overview again", |ui| {
        ui.shows(ElementId::Name("agents-overview".into()))
    });
    ui.click(("agents-primary", 0usize));
    ui.wait_for("workspace B", |ui| {
        ui.workspace().is_some_and(|w| w.id == b.id)
    });
    ui.wait_for("the player", |ui| {
        let players = ui.players();
        ui.cx.read(|cx| {
            players.iter().any(|p| {
                p.read(cx)
                    .walkthrough()
                    .is_some_and(|w| w.id == walkthrough)
            })
        })
    });
}

fn info_of(cx: &App, tab: &Entity<AgentsTab>, id: &SessionId) -> SessionInfo {
    tab.read(cx)
        .rows(cx)
        .into_iter()
        .find(|r| &r.id == id)
        .unwrap()
}

/// Renders the agents overview with a welcome back offscreen to PNGs, for
/// visual review against board 6. Needs a GPU:
/// `cargo test -p coqo -- --ignored agents_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn agents_screenshots() {
    use coqo_protocol::{
        PROTOCOL_VERSION,
        daemon::{DaemonStatus, IdlePhase, IdleStatus, MemoryUsage},
        methods::{
            agents::{
                AcceptanceCriterion, AgentStatus, CriterionAuthor, CriterionStatus,
                PermissionRequest, PermissionRequestId, StatusProgress,
            },
            walkthroughs::{ScopeKind, StorageMode, WalkthroughSummary},
            workspace::{RootId, WorkspaceId, WorkspaceRoot},
        },
    };
    use coqo_ui::test::render_offscreen;

    use crate::{activity::AgentActivity, workspace::KnownWorkspaces};

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let ws = |ix: i64, name: &str| WorkspaceInfo {
        id: WorkspaceId(format!("ws-{ix}")),
        name: name.into(),
        roots: vec![WorkspaceRoot {
            id: RootId(0),
            path: format!("/home/you/code/{name}"),
            name: name.into(),
        }],
        created_at: ix,
        last_opened_at: ix,
        open: true,
        project: None,
    };
    let now = now_ms();
    let session = |id: &str, ws: i64, provider: &str, title: &str, state, mins: i64| SessionInfo {
        id: SessionId(id.into()),
        provider: provider.into(),
        workspace: Some(format!("ws-{ws}")),
        cwd: "/".into(),
        title: Some(title.into()),
        state,
        state_reason: None,
        state_since: Some(now - mins * MIN),
        status: None,
        criteria: Vec::new(),
        tool_use: Vec::new(),
        mode: None,
        closed: false,
        created_at: 0,
        updated_at: now - mins * MIN,
    };
    let criterion = |id: &str, status| AcceptanceCriterion {
        id: id.into(),
        text: id.into(),
        status,
        evidence: Vec::new(),
        changed_by: CriterionAuthor::Agent,
        updated_at: now,
    };
    let mut asks = session(
        "s-1",
        1,
        "claude-code",
        "Volume discount",
        AgentState::NeedsInput,
        18,
    );
    asks.state_reason =
        Some("Loyalty bonus before or after the 500 EUR cap? · DiscountService.kt:14".into());
    asks.criteria = vec![
        criterion("AC1", CriterionStatus::Met),
        criterion("AC2", CriterionStatus::Met),
        criterion("AC3", CriterionStatus::Pending),
    ];
    let perm = session(
        "s-2",
        1,
        "claude-code",
        "Fix flaky OrderControllerIT",
        AgentState::NeedsPermission,
        4,
    );
    let mut failed = session(
        "s-3",
        2,
        "gemini",
        "Storybook upgrade",
        AgentState::Error,
        41,
    );
    failed.state_reason = Some("npm ERR! ERESOLVE peer react@19 conflict".into());
    let mut working = session(
        "s-4",
        2,
        "codex",
        "Cart discount line",
        AgentState::Working,
        7,
    );
    working.status = Some(AgentStatus {
        text: "Editing CartSummary.tsx".into(),
        state: None,
        progress: Some(StatusProgress {
            current: 3,
            total: 5,
        }),
        turn: Some(1),
        updated_at: now,
    });
    let mut done = session(
        "s-5",
        3,
        "claude-code",
        "Retry policy for 3DS callbacks",
        AgentState::Done,
        60,
    );
    done.status = Some(AgentStatus {
        text: "3 trace scenarios passed".into(),
        state: None,
        progress: None,
        turn: None,
        updated_at: now,
    });
    // It published a walkthrough: **Walkthrough** (#265).
    let walkthrough = WalkthroughSummary {
        id: WalkthroughId("w-1".into()),
        workspace: WorkspaceId("ws-3".into()),
        session: done.id.clone(),
        title: "Retry policy for 3DS callbacks".into(),
        scope: ScopeKind::Worktree,
        steps: 6,
        understood: 0,
        questions: 0,
        start: None,
        storage: StorageMode::Local,
        coverage: None,
        created_at: now - 60 * MIN,
        revision: 0,
    };
    // Up 3 days, 2.1 GB with its language servers (#265).
    const MB: u64 = 1024 * 1024;
    let status = DaemonStatus {
        version: "0.1.0".into(),
        build: "test".into(),
        protocol: PROTOCOL_VERSION,
        pid: 1,
        started_at_ms: u64::try_from(now - 3 * 1440 * MIN - 5 * MIN).unwrap(),
        uptime_ms: 0,
        connections: 1,
        busy: Vec::new(),
        idle: IdleStatus {
            phase: IdlePhase::Active,
            stop_services_after_secs: None,
            exit_after_secs: None,
        },
        memory: Some(MemoryUsage {
            rss_bytes: 2150 * MB,
            processes: 6,
        }),
    };
    let sessions = vec![asks, perm.clone(), failed, working, done];
    let request = PermissionRequest {
        id: PermissionRequestId(1),
        title: "`docker compose up -d postgres`".into(),
        tool_call: None,
        options: ask().options,
        suggested_rule: None,
    };
    let welcome =
        away::WelcomeBack::build(now - 134 * MIN, now, Away::Closed, &sessions, &[], 12).unwrap();
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
            workspace::init(None, cx);
            away::init(Rc::new(MemoryLastSeen::default()), cx);
            crate::shell::init(cx);
            let list = vec![
                ws(1, "checkout-service"),
                ws(2, "web-storefront"),
                ws(3, "payments-gateway"),
            ];
            let known = KnownWorkspaces::global(cx).unwrap();
            known.update(cx, |k, cx| k.set_for_tests(list.clone(), cx));
            let activity = AgentActivity::global(cx).unwrap();
            activity.update(cx, |a, cx| a.set_for_tests(sessions.clone(), cx));
            let presence = Presence::global(cx).unwrap();
            presence.update(cx, |p, cx| p.set_for_tests(Some(welcome.clone()), cx));
            let store = Rc::new(MemoryLayoutStore::default());
            let (perm, request) = (perm.id.clone(), request.clone());
            let (walkthrough, status) = (walkthrough.clone(), status.clone());
            cx.new(|cx| {
                let mut shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                shell
                    .workspaces()
                    .update(cx, |w, cx| w.set_current_for_tests(list[0].clone(), cx));
                shell.show_agents(window, cx);
                let dock = shell.dock().clone();
                let tab = {
                    let dock = dock.read(cx);
                    dock.tree()
                        .tabs()
                        .into_iter()
                        .find_map(|t| dock.tab(t.id)?.downcast::<AgentsTab>())
                };
                if let Some(tab) = tab {
                    tab.update(cx, |tab, cx| {
                        let providers = [
                            ("claude-code", "Claude Code"),
                            ("gemini", "Gemini CLI"),
                            ("codex", "Codex CLI"),
                        ];
                        tab.set_for_tests(
                            &providers,
                            vec![(perm, request)],
                            vec![walkthrough],
                            Some(status),
                            cx,
                        )
                    });
                }
                shell
            })
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-agents-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
