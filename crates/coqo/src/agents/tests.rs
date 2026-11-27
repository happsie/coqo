// SPDX-License-Identifier: Apache-2.0

//! The agents overview and "welcome back" against a daemon served
//! in-process: the real workspace, session, buffer, question and alert
//! services, with `ScriptedProvider` as the agent. Sessions in two
//! workspaces are started by another client (as if the UI had been
//! closed).

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use coqo_agents::{
    ManagerOptions, PermissionAsk, SessionManager, SessionStore, StaticProviders,
    model::{
        ContentBlock, CreateSessionParams, PermissionOption, PermissionOptionKind, PromptParams,
        SessionId, StopReason,
    },
    testing::{ScriptedProvider, Step},
};
use coqo_client::{DaemonPaths, RuntimeDir, connect};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::methods::{
    agents::{self as agm, AgentState, ListSessionsParams, SessionInfo},
    editor::LineRange,
    questions::{self as qm, CreateParams, NewAnchor, QuestionId, QuestionKind},
    workspace::{self as wsm, OpenParams, WorkspaceInfo},
};
use coqo_protocol::{Client, ClientConfig, Router, Server, ServerConfig};
use coqo_storage::DataDirs;
use coqo_ui::{
    Appearance,
    dock::{TabItem as _, store::MemoryLayoutStore},
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Keystroke, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use tempfile::TempDir;
use tokio::{runtime::Runtime, sync::Notify};
use tokio_util::sync::CancellationToken;

use super::{
    KIND,
    away::{self, MemoryLastSeen, Presence},
    view::AgentsTab,
};
use crate::{
    alerts::now_ms,
    chat::{self, tab::ChatTab},
    daemon::{self, DaemonConfig},
    files::editor::{self, FileEditor},
    shell::{DEFAULT_WORKSPACE, Shell},
    workspace::{self, Workspaces},
};

const TIMEOUT: Duration = Duration::from_secs(10);
const MIN: i64 = 60_000;

/// The daemon's workspace, session, buffer, question and alert services on
/// a socket.
struct TestDaemon {
    runtime: Runtime,
    paths: DaemonPaths,
    _stop: CancellationToken,
}

impl TestDaemon {
    fn start(dir: &Path, provider: &ScriptedProvider) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let paths = DaemonPaths::at(dir.join("run"));
        let data = DataDirs::at(dir.join("data"));
        std::fs::create_dir_all(data.root()).unwrap();
        let stop = CancellationToken::new();
        let entered = runtime.enter();
        let registry = StaticProviders::new().with(
            AgentProviderInfo {
                id: "fake".parse().unwrap(),
                name: "Fake agent".into(),
                kind: AgentProviderKind::Acp,
                description: None,
            },
            Arc::new(provider.clone()),
        );
        let db = data.global_db();
        let store = SessionStore::open(&db).unwrap();
        let manager =
            SessionManager::open(store, Arc::new(registry), ManagerOptions::default()).unwrap();
        let workspaces = Arc::new(coqod::workspace::WorkspaceService::open(data.clone()).unwrap());
        let buffers = coqod::buffers::BufferService::new(
            Arc::clone(&workspaces),
            data,
            coqod::buffers::BufferConfig::default(),
        );
        let questions = coqod::questions::QuestionService::open(
            &db,
            Arc::clone(&workspaces),
            Arc::clone(&buffers),
            Some(manager.clone()),
        )
        .unwrap();
        let alerts = coqod::alerts::AlertService::open(&db, None).unwrap();
        coqod::alerts::agents::AgentSource::new(
            Arc::clone(&alerts),
            manager.clone(),
            Some(questions.clone()),
        )
        .spawn(stop.clone());
        let router = coqod::workspace::register(Router::new(), workspaces);
        let router = coqo_agents::rpc::register(router, manager);
        let router = coqod::questions::register(router, questions);
        let router = coqod::buffers::register(router, buffers);
        let router = coqod::alerts::register(router, alerts);
        let server = Server::new(router, ServerConfig::new("coqod", "test"));
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        runtime.spawn(coqo_protocol::unix::serve_unix(server, listener));
        drop(entered);
        Self {
            runtime,
            paths,
            _stop: stop,
        }
    }

    /// Run `f` with a client of its own (another UI, the CLI, an agent's
    /// bridge).
    fn with_client<R>(&self, f: impl AsyncFnOnce(Client) -> R) -> R {
        let paths = self.paths.clone();
        self.runtime.block_on(async move {
            let (client, _) = connect(&paths, ClientConfig::new("coqo-tests", "test"))
                .await
                .unwrap();
            f(client).await
        })
    }
}

/// Two project folders (workspaces A and B) and a daemon.
struct Fixture {
    _tmp: TempDir,
    a: PathBuf,
    b: PathBuf,
    daemon: TestDaemon,
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
        let daemon = TestDaemon::start(tmp.path(), provider);
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
    let mut cx = TestAppContext::single();
    cx.executor().allow_parking();
    let paths = fixture.daemon.paths.clone();
    let folder = fixture.a.clone();
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
    assert!(welcome.was_closed);
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

/// Renders the agents overview with a welcome back offscreen to PNGs, for
/// visual review against board 6. Needs a GPU:
/// `cargo test -p coqo -- --ignored agents_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn agents_screenshots() {
    use coqo_protocol::methods::{
        agents::{
            AcceptanceCriterion, AgentStatus, CriterionAuthor, CriterionStatus, PermissionRequest,
            PermissionRequestId, StatusProgress,
        },
        workspace::{RootId, WorkspaceId, WorkspaceRoot},
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
        text: "Walkthrough ready · 3 trace scenarios passed".into(),
        state: None,
        progress: None,
        turn: None,
        updated_at: now,
    });
    let sessions = vec![asks, perm.clone(), failed, working, done];
    let request = PermissionRequest {
        id: PermissionRequestId(1),
        title: "`docker compose up -d postgres`".into(),
        tool_call: None,
        options: ask().options,
        suggested_rule: None,
    };
    let welcome = away::WelcomeBack::build(now - 134 * MIN, now, true, &sessions, &[]).unwrap();
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
                        tab.set_for_tests(&providers, vec![(perm, request)], cx)
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
