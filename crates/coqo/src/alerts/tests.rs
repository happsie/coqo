// SPDX-License-Identifier: Apache-2.0

//! The bell, the alert center and the toast against a daemon served
//! in-process: the real workspace, session, buffer, question and alert
//! services, with `ScriptedProvider` as the agent. Sessions are started by
//! another client (as if the UI had been closed), so their alerts are news
//! to this window.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use coqo_agents::{
    AgentUpdate, ManagerOptions, PermissionAsk, SessionManager, SessionStore, StaticProviders,
    model::{
        ContentBlock, CreateSessionParams, PermissionOption, PermissionOptionKind, PromptParams,
        SessionId, StopReason,
    },
    testing::{ScriptedProvider, Step},
};
use coqo_client::{DaemonPaths, RuntimeDir, connect};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::methods::{
    agents as agm,
    alerts::{Alert, AlertKind, AlertTarget},
    editor::LineRange,
    questions::{self as qm, AnswerParams, CreateParams, NewAnchor, QuestionKind},
    workspace::WorkspaceInfo,
};
use coqo_protocol::{Client, ClientConfig, Router, Server, ServerConfig};
use coqo_storage::DataDirs;
use coqo_ui::{
    Appearance,
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use tempfile::TempDir;
use tokio::{runtime::Runtime, sync::Notify};
use tokio_util::sync::CancellationToken;

use super::AlertCenter;
use crate::{
    chat::{self, tab::ChatTab},
    daemon::{self, DaemonConfig},
    files::editor::{self, FileEditor},
    shell::{DEFAULT_WORKSPACE, Shell},
    workspace::{self, Workspaces},
};

const TIMEOUT: Duration = Duration::from_secs(10);

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

struct Fixture {
    _tmp: TempDir,
    project: PathBuf,
    daemon: TestDaemon,
}

impl Fixture {
    fn new(provider: &ScriptedProvider) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo-project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            project.join("src/main.rs"),
            "fn main() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let project = project.canonicalize().unwrap();
        let daemon = TestDaemon::start(tmp.path(), provider);
        Self {
            _tmp: tmp,
            project,
            daemon,
        }
    }

    /// Start a session in `ws` and prompt it, from another client.
    fn start_session(&self, ws: &WorkspaceInfo) -> SessionId {
        let cwd = self.project.to_string_lossy().into_owned();
        let ws = ws.id.0.clone();
        self.daemon.with_client(async move |client| {
            let id = client
                .request::<agm::Create>(CreateSessionParams {
                    provider: "fake".into(),
                    cwd,
                    workspace: Some(ws),
                    title: Some("Run the tests".into()),
                    mode: None,
                })
                .await
                .unwrap()
                .id;
            prompt(&client, &id).await;
            id
        })
    }

    fn prompt(&self, id: &SessionId) {
        let id = id.clone();
        self.daemon
            .with_client(async move |client| prompt(&client, &id).await);
    }
}

async fn prompt(client: &Client, id: &SessionId) {
    client
        .request::<agm::Prompt>(PromptParams {
            session: id.clone(),
            content: vec![ContentBlock::text("go")],
        })
        .await
        .unwrap();
}

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
}

fn open_ui(fixture: &Fixture) -> Ui {
    let mut cx = TestAppContext::single();
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

    fn shows(&mut self, id: &str) -> bool {
        let id = name(id);
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn click(&mut self, id: impl Into<ElementId>) {
        let id = id.into();
        self.frame(|window, cx| window.click(id, cx));
    }

    /// The alert center's alert of `session`, once `ok` holds for it.
    fn alert(&mut self, session: &SessionId, ok: impl Fn(&Alert) -> bool) -> Alert {
        let found = |ui: &mut Self| {
            ui.cx.read(|cx| {
                AlertCenter::global(cx)
                    .unwrap()
                    .read(cx)
                    .alerts()
                    .into_iter()
                    .find(|a| a.session.as_ref() == Some(session) && ok(a))
                    .cloned()
            })
        };
        self.wait_for("the alert", |ui| found(ui).is_some());
        found(self).unwrap()
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
}

fn name(id: &str) -> ElementId {
    ElementId::Name(id.to_owned().into())
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
fn a_permission_request_rings_the_bell_and_opening_it_leads_to_the_chat() {
    let provider = ScriptedProvider::new().turn([
        Step::Permission(ask()),
        Step::Update(AgentUpdate::AgentMessageChunk(ContentBlock::text(
            "Tests pass",
        ))),
        Step::Stop(StopReason::EndTurn),
    ]);
    let fixture = Fixture::new(&provider);
    let mut ui = open_ui(&fixture);
    let ws = ui.workspace().unwrap();
    assert!(!ui.shows("alerts-badge-needs-you"));
    let id = fixture.start_session(&ws);

    // The bell counts it in sand, and a toast shows it.
    let alert = ui.alert(&id, |a| a.is_unread() && a.kind == AlertKind::NeedsYou);
    assert_eq!(alert.title, "Permission requested: Run cargo test");
    ui.wait_for("the sand badge", |ui| ui.shows("alerts-badge-needs-you"));
    ui.wait_for("the toast", |ui| ui.shows("alerts-toast"));
    let toast = ui.cx.read(|cx| ui.shell.read(cx).toast().cloned());
    assert_eq!(toast.map(|t| t.id), Some(alert.id));

    // The bell opens the alert center; the alert opens the chat.
    ui.click(name("alerts"));
    ui.wait_for("the alert center", |ui| ui.shows("alerts-center"));
    ui.click(super::view::row_id(0));
    ui.wait_for("the chat tab", |ui| ui.chat_sessions() == [id.clone()]);
    assert!(!ui.shows("alerts-center"), "it closes");
    assert!(!ui.shows("alerts-toast"), "the toast goes too");
    ui.wait_for("the badge to go", |ui| !ui.shows("alerts-badge-needs-you"));
    let read = ui.alert(&id, |a| a.id == alert.id);
    assert!(read.read && read.is_active(), "read, still standing");

    // Answering the request in the chat resolves it.
    let allow = |ui: &mut Ui| {
        ui.frame(|window, _| {
            (0..16)
                .map(|ix| format!("chat-permission-{ix}-allow-once"))
                .find(|button| window.try_find(name(button)).is_some())
        })
    };
    ui.wait_for("the permission card", |ui| allow(ui).is_some());
    let button = allow(&mut ui).unwrap();
    ui.click(name(&button));
    let resolved = ui.alert(&id, |a| {
        a.source == "agents.permission" && a.resolved_at.is_some()
    });
    assert_eq!(resolved.id, alert.id);
    // The chat is on screen, so the turn's end is read at once.
    let done = ui.alert(&id, |a| a.kind == AlertKind::Done);
    ui.wait_for("done read", |ui| ui.alert(&id, |a| a.id == done.id).read);
}

#[test]
fn a_waiting_question_opens_its_code_and_an_error_turns_the_bell_rose() {
    let release = Arc::new(Notify::new());
    let provider = ScriptedProvider::new()
        .turn([
            Step::Wait(Arc::clone(&release)),
            Step::Stop(StopReason::EndTurn),
        ])
        .turn([Step::Fail("npm ERR! ERESOLVE".into())]);
    let fixture = Fixture::new(&provider);
    let mut ui = open_ui(&fixture);
    let ws = ui.workspace().unwrap();
    let id = fixture.start_session(&ws);

    // The agent asks about line 2 of src/main.rs.
    let (wsid, root, session) = (ws.id.clone(), ws.roots[0].id, id.0.clone());
    let question = fixture.daemon.with_client(async move |client| {
        client
            .request::<qm::Create>(CreateParams {
                workspace: wsid,
                session,
                kind: QuestionKind::Clarification,
                body: "Why **1**?".into(),
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
    });
    let alert = ui.alert(&id, |a| a.is_unread() && a.source == "agents.question");
    assert_eq!(alert.body.as_deref(), Some("Why 1?"));
    assert!(matches!(
        alert.target,
        Some(AlertTarget::Question { question: q, .. }) if q == question.id
    ));
    ui.wait_for("the sand badge", |ui| ui.shows("alerts-badge-needs-you"));

    // Opening it opens the code it is pinned to.
    ui.click(name("alerts"));
    ui.wait_for("the alert center", |ui| ui.shows("alerts-center"));
    ui.click(super::view::row_id(0));
    ui.wait_for("the file tab", |ui| ui.file_tabs() == ["src/main.rs"]);

    // Answering resolves it; the finished turn is done (sage).
    let qid = question.id;
    fixture.daemon.with_client(async move |client| {
        client
            .request::<qm::Answer>(AnswerParams {
                id: qid,
                option: None,
                text: Some("Because".into()),
            })
            .await
            .unwrap();
    });
    ui.alert(&id, |a| {
        a.source == "agents.question" && a.resolved_at.is_some()
    });
    release.notify_one();
    ui.alert(&id, |a| a.kind == AlertKind::Done && a.is_unread());
    ui.wait_for("the done badge", |ui| ui.shows("alerts-badge-done"));

    // The next turn fails: rose.
    fixture.prompt(&id);
    let failed = ui.alert(&id, |a| a.kind == AlertKind::Error && a.is_unread());
    assert_eq!(failed.body.as_deref(), Some("npm ERR! ERESOLVE"));
    ui.wait_for("the rose badge", |ui| ui.shows("alerts-badge-error"));
    // No toast for an error: only what needs you pops up.
    assert!(!ui.shows("alerts-toast"));

    // Dismissing it empties the center.
    ui.click(name("alerts"));
    ui.wait_for("the alert center", |ui| ui.shows("alerts-center"));
    ui.click(name("alerts-dismiss-all"));
    ui.wait_for("an empty center", |ui| ui.shows("alerts-empty"));
    assert!(!ui.shows("alerts-badge-error"));
}

/// Renders the alert center and a toast offscreen to PNGs, for visual
/// review against board 6. Needs a GPU:
/// `cargo test -p coqo -- --ignored alert_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn alert_screenshots() {
    use coqo_protocol::methods::alerts::AlertId;
    use coqo_protocol::methods::workspace::{RootId, WorkspaceId, WorkspaceRoot};
    use coqo_ui::test::render_offscreen;

    use crate::workspace::KnownWorkspaces;

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
    let now = super::now_ms();
    let alert = |id: u64, ws: i64, kind, title: &str, body: &str, mins: i64, read: bool| Alert {
        id: AlertId(id),
        workspace: Some(WorkspaceId(format!("ws-{ws}"))),
        session: Some(SessionId(format!("s-{id}"))),
        source: "agents.question".into(),
        key: format!("s-{id}"),
        kind,
        title: title.into(),
        body: (!body.is_empty()).then(|| body.into()),
        target: None,
        created_at: now - mins * 60_000,
        updated_at: now,
        read,
        dismissed: false,
        resolved_at: None,
        seq: id,
    };
    let alerts = vec![
        alert(
            1,
            1,
            AlertKind::NeedsYou,
            "Claude Code asked a question",
            "Loyalty bonus before or after the 500 EUR cap?",
            18,
            false,
        ),
        alert(
            2,
            1,
            AlertKind::NeedsYou,
            "Permission requested: docker compose up -d postgres",
            "Fix flaky OrderControllerIT",
            4,
            false,
        ),
        alert(
            3,
            2,
            AlertKind::Error,
            "Gemini CLI failed",
            "npm ERR! ERESOLVE peer react@19 conflict",
            41,
            false,
        ),
        alert(
            4,
            3,
            AlertKind::Done,
            "Claude Code finished",
            "Retry policy for 3DS callbacks",
            60,
            true,
        ),
    ];
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
            workspace::init(None, cx);
            crate::shell::init(cx);
            let list = vec![
                ws(1, "checkout-service"),
                ws(2, "web-storefront"),
                ws(3, "payments-gateway"),
            ];
            let known = KnownWorkspaces::global(cx).unwrap();
            known.update(cx, |k, cx| k.set_for_tests(list.clone(), cx));
            let center = AlertCenter::global(cx).unwrap();
            center.update(cx, |c, cx| c.set_for_tests(alerts.clone(), cx));
            let store = Rc::new(MemoryLayoutStore::default());
            let toast = alerts[0].clone();
            cx.new(|cx| {
                let mut shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                shell
                    .workspaces()
                    .update(cx, |w, cx| w.set_current_for_tests(list[0].clone(), cx));
                shell.rail().update(cx, |rail, cx| rail.toggle_alerts(cx));
                shell.show_toast_for_tests(toast);
                shell
            })
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-alerts-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
