// SPDX-License-Identifier: Apache-2.0

//! `questions/*` against the real router: the store across a restart,
//! anchors through live edits and after a restart, answers, the changes
//! feed, session state, and the bridge's `ask_question`, `await_answer` and
//! `annotate` end to end with a session token.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coqo_agents::model::{
    AgentState, ContentBlock, CreateSessionParams, PromptParams, SessionInfo, SessionParams,
    SessionUpdate, StopReason,
};
use coqo_agents::register_provider;
use coqo_agents::testing::{ScriptedProvider, Step};
use coqo_mcp::{BridgeError, BridgeOptions, Exit, ToolRegistry};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError};
use coqo_protocol::methods::agents as agm;
use coqo_protocol::methods::buffer::{self as bufm, EditParams, TextEdit};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::editor::LineRange;
use coqo_protocol::methods::questions::{
    self as qm, AnchorStatus, AnswerParams, ChangesParams, CreateParams, ListParams, NewAnchor,
    NewOption, Question, QuestionId, QuestionKind, QuestionParams, QuestionStatus, Severity,
    WatchParams,
};
use coqo_protocol::methods::workspace::{self as wsm, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig, ProtocolError, Server, ServerConfig, SessionToken};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{
    AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::daemon::DaemonContext;
use crate::mods::Mods;
use crate::mods::tests::test_builtins;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(10);

const MAIN_RS: &str = "struct Circle { r: f64 }\n\nimpl Circle {\n    fn area(&self) -> f64 {\n        3.14 * self.r * self.r\n    }\n}\n\nfn main() {}\n";

/// A provider mod whose sessions run turns the test scripts.
struct FakeAgent(ScriptedProvider);

impl Mod for FakeAgent {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        let provider = Arc::new(self.0.clone());
        Box::pin(async move {
            let info = AgentProviderInfo {
                id: "fake".parse()?,
                name: "Fake agent".into(),
                kind: AgentProviderKind::Native,
                description: None,
            };
            register_provider(&cx, info, provider)
        })
    }
}

const FAKE_MANIFEST: &str = r#"
    id = "test.fake-agent"
    name = "Fake agent"
    version = "0.1.0"
    engine = "^0.0"
    tier = "core"
    activation = ["startup"]
"#;

/// The daemon's router over data in `dir`, served in-process.
struct Daemon {
    ctx: DaemonContext,
    server: Server,
}

impl Daemon {
    async fn start(dir: &Path, agent: &ScriptedProvider) -> Self {
        let mut ctx = routes::tests::context(dir);
        ctx.mods = Mods::load(
            test_builtins(&dir.join("mods")).with(FAKE_MANIFEST, FakeAgent(agent.clone())),
        );
        ctx.mods.start().await;
        let mut config = ServerConfig::new("coqod", "test");
        config.authenticator = Some(Arc::new(ctx.auth.clone()));
        let server = Server::new(routes::router(&ctx).unwrap(), config);
        Self { ctx, server }
    }

    async fn connect(&self, token: SessionToken) -> Client {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(server_io).await });
        let mut config = ClientConfig::new("test", "test");
        config.token = Some(token);
        Client::connect(client_io, config).await.unwrap().0
    }

    async fn ui(&self) -> Client {
        self.connect(self.ctx.auth.daemon_token()).await
    }

    async fn agent(&self, ws: &WorkspaceInfo, session: &str) -> Client {
        let token = self
            .ctx
            .auth
            .mint_session(ws.id.0.clone(), session)
            .unwrap();
        self.connect(token).await
    }

    async fn stop(self) {
        self.ctx.shutdown.cancel();
        self.ctx.mods.shutdown().await;
    }
}

/// A project folder opened as a workspace.
async fn project(ui: &Client) -> (TempDir, WorkspaceInfo) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "one\ntwo\nthree\n").unwrap();
    let info = ui
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    (dir, info)
}

fn create(
    ws: &WorkspaceInfo,
    session: &str,
    kind: QuestionKind,
    lines: (u32, u32),
) -> CreateParams {
    CreateParams {
        workspace: ws.id.clone(),
        session: session.into(),
        kind,
        body: "Why **3.14**?".into(),
        options: if kind == QuestionKind::Decision {
            vec![
                NewOption {
                    label: "Use PI".into(),
                    description: None,
                },
                NewOption {
                    label: "Keep it".into(),
                    description: Some("It is fine".into()),
                },
            ]
        } else {
            Vec::new()
        },
        severity: None,
        anchors: vec![NewAnchor {
            root: ws.roots[0].id,
            path: "src/main.rs".into(),
            lines: LineRange {
                start: lines.0,
                end: lines.1,
            },
        }],
    }
}

async fn list(ui: &Client, ws: &WorkspaceInfo) -> qm::QuestionList {
    ui.request::<qm::List>(ListParams {
        workspace: ws.id.clone(),
        root: None,
        path: None,
        session: None,
        include_closed: true,
    })
    .await
    .unwrap()
}

fn code<T: std::fmt::Debug>(r: Result<T, ProtocolError>) -> i64 {
    r.unwrap_err().rpc().map(|e| e.code).unwrap_or_default()
}

async fn until<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Some(t) = f().await {
            return t;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn questions_survive_a_restart_and_find_their_code_again() {
    let data = tempfile::tempdir().unwrap();
    let agent = ScriptedProvider::new();
    let daemon = Daemon::start(data.path(), &agent).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui).await;

    // `3.14 * self.r * self.r`, on disk (no buffer open).
    let q = ui
        .request::<qm::Create>(create(&ws, "s-1", QuestionKind::Clarification, (4, 4)))
        .await
        .unwrap();
    assert_eq!(q.status, QuestionStatus::Open);
    assert_eq!(q.anchors[0].anchor.text, "        3.14 * self.r * self.r\n");
    assert_eq!(
        q.anchors[0].anchor.symbol.as_ref().unwrap().path,
        ["Circle", "area"]
    );
    assert_eq!(q.anchors[0].resolution.status, AnchorStatus::Exact);
    assert_eq!(q.anchors[0].resolution.buffer, None);
    let note = ui
        .request::<qm::Create>(CreateParams {
            severity: Some(Severity::Warning),
            ..create(&ws, "s-1", QuestionKind::Annotation, (8, 8))
        })
        .await
        .unwrap();
    daemon.stop().await;

    // While the daemon is down, lines are added above both.
    let edited = MAIN_RS.replacen("struct", "// shapes\n\nstruct", 1);
    std::fs::write(dir.path().join("src/main.rs"), &edited).unwrap();

    let daemon = Daemon::start(data.path(), &agent).await;
    let ui = daemon.ui().await;
    let listed = list(&ui, &ws).await;
    assert_eq!(listed.questions.len(), 2, "{listed:?}");
    let again = &listed.questions[0];
    assert_eq!((again.id, again.kind), (q.id, QuestionKind::Clarification));
    assert_eq!(again.body, "Why **3.14**?");
    assert_eq!(again.anchors[0].resolution.status, AnchorStatus::Moved);
    assert_eq!(again.anchors[0].resolution.range.unwrap().start.line, 6);
    // The original snippet stays.
    assert_eq!(again.anchors[0].anchor.range.start.line, 4);
    let note_again = &listed.questions[1];
    assert_eq!(note_again.id, note.id);
    assert_eq!(note_again.severity, Some(Severity::Warning));
    assert_eq!(
        note_again.anchors[0].resolution.range.unwrap().start.line,
        10
    );
    assert!(listed.seq >= 2);

    // The anchored line is deleted: orphaned, with its original text.
    std::fs::write(
        dir.path().join("src/main.rs"),
        edited.replace("        3.14 * self.r * self.r\n", ""),
    )
    .unwrap();
    let q = ui
        .request::<qm::Get>(QuestionParams { id: q.id })
        .await
        .unwrap();
    assert_eq!(q.anchors[0].resolution.status, AnchorStatus::Orphaned);
    assert!(q.anchors[0].resolution.range.is_none());
    assert_eq!(q.anchors[0].anchor.text, "        3.14 * self.r * self.r\n");
    // Orphaned questions can still be answered.
    let answered = ui
        .request::<qm::Answer>(AnswerParams {
            id: q.id,
            option: None,
            text: Some("gone now".into()),
        })
        .await
        .unwrap();
    assert_eq!(answered.status, QuestionStatus::Answered);
    daemon.stop().await;
}

#[tokio::test]
async fn anchors_follow_edits_of_an_open_buffer() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path(), &ScriptedProvider::new()).await;
    let ui = daemon.ui().await;
    let (_dir, ws) = project(&ui).await;
    let open = ui
        .request::<bufm::Open>(bufm::OpenParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: "src/main.rs".into(),
            replica: None,
        })
        .await
        .unwrap();
    let q = ui
        .request::<qm::Create>(create(&ws, "s-1", QuestionKind::Clarification, (4, 4)))
        .await
        .unwrap();
    let r = q.anchors[0].resolution;
    assert_eq!((r.buffer, r.rev), (Some(open.buffer), Some(open.rev)));
    let listed = list(&ui, &ws).await;
    let mut changes = ui
        .subscribe::<qm::Changes>(
            ChangesParams {
                workspace: Some(ws.id.clone()),
            },
            Some(listed.seq),
        )
        .await
        .unwrap();

    // Two lines typed at the top: the question moves down with them.
    ui.notify::<bufm::Edit>(EditParams {
        buffer: open.buffer,
        replica: open.replica.clone(),
        base_rev: open.rev,
        client_seq: 1,
        edits: vec![TextEdit::insert(0, "// a\n// b\n")],
    })
    .await
    .unwrap();
    let moved = tokio::time::timeout(TIMEOUT, changes.next())
        .await
        .expect("a change in time")
        .unwrap()
        .unwrap()
        .event;
    assert_eq!(moved.id, q.id);
    let r = moved.anchors[0].resolution;
    assert_eq!(r.status, AnchorStatus::Moved);
    assert_eq!(r.range.unwrap().start.line, 6);
    assert_eq!((r.buffer, r.rev), (Some(open.buffer), Some(open.rev + 1)));

    // The anchored line edited in place: still found, now fuzzy.
    let line = "// a\n// b\n".len() + MAIN_RS.find("3.14").unwrap();
    ui.notify::<bufm::Edit>(EditParams {
        buffer: open.buffer,
        replica: open.replica.clone(),
        base_rev: open.rev + 1,
        client_seq: 2,
        edits: vec![TextEdit::new(line..line + 4, "std::f64::consts::PI")],
    })
    .await
    .unwrap();
    let edited = tokio::time::timeout(TIMEOUT, changes.next())
        .await
        .expect("a change in time")
        .unwrap()
        .unwrap()
        .event;
    let r = edited.anchors[0].resolution;
    assert!(
        matches!(r.status, AnchorStatus::Fuzzy { .. }),
        "{:?}",
        r.status
    );
    assert_eq!(r.range.unwrap().start.line, 6);
    assert_eq!(r.rev, Some(open.rev + 2));
    // A list sees the same.
    let listed = list(&ui, &ws).await;
    assert_eq!(listed.questions[0].anchors[0].resolution, r);
    daemon.stop().await;
}

#[tokio::test]
async fn answers_fit_the_kind_and_changes_resume() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path(), &ScriptedProvider::new()).await;
    let ui = daemon.ui().await;
    let (_dir, ws) = project(&ui).await;

    // Bad questions.
    let bad = |f: fn(&mut CreateParams)| {
        let mut p = create(&ws, "s-1", QuestionKind::Decision, (0, 0));
        f(&mut p);
        p
    };
    for p in [
        bad(|p| p.body = "  ".into()),
        bad(|p| p.options.truncate(1)),
        bad(|p| p.anchors.clear()),
        bad(|p| p.anchors[0].lines = LineRange { start: 3, end: 1 }),
        bad(|p| p.anchors[0].lines = LineRange { start: 0, end: 99 }),
        bad(|p| p.kind = QuestionKind::Confirmation),
    ] {
        let r = ui.request::<qm::Create>(p.clone()).await;
        assert_eq!(code(r), coqo_protocol::codes::INVALID_PARAMS, "{p:?}");
    }
    let mut missing = create(&ws, "s-1", QuestionKind::Info, (0, 0));
    missing.anchors[0].path = "nope.rs".into();
    assert_eq!(
        code(ui.request::<qm::Create>(missing).await),
        codes::NOT_FOUND
    );

    let decision = ui
        .request::<qm::Create>(create(&ws, "s-1", QuestionKind::Decision, (0, 0)))
        .await
        .unwrap();
    assert_eq!(
        decision
            .options
            .iter()
            .map(|o| (o.id.as_str(), o.label.as_str()))
            .collect::<Vec<_>>(),
        [("1", "Use PI"), ("2", "Keep it")]
    );
    let confirm = ui
        .request::<qm::Create>(create(&ws, "s-1", QuestionKind::Confirmation, (2, 3)))
        .await
        .unwrap();
    assert_eq!(confirm.options.len(), 2);
    let seq = list(&ui, &ws).await.seq;

    let answer = |id, option: Option<&str>, text: Option<&str>| AnswerParams {
        id,
        option: option.map(Into::into),
        text: text.map(Into::into),
    };
    for p in [
        answer(decision.id, Some("3"), None),
        answer(decision.id, None, None),
        answer(confirm.id, None, Some("sure")),
        answer(confirm.id, Some("1"), None),
    ] {
        let r = ui.request::<qm::Answer>(p.clone()).await;
        assert_eq!(code(r), coqo_protocol::codes::INVALID_PARAMS, "{p:?}");
    }
    assert_eq!(
        code(
            ui.request::<qm::Answer>(answer(QuestionId(999), Some("1"), None))
                .await
        ),
        codes::UNKNOWN_QUESTION
    );
    let done = ui
        .request::<qm::Answer>(answer(decision.id, Some("2"), Some("for now")))
        .await
        .unwrap();
    assert_eq!(done.status, QuestionStatus::Answered);
    assert_eq!(done.answer.as_ref().unwrap().option.as_deref(), Some("2"));
    assert!(done.closed_at.is_some());
    assert_eq!(
        code(
            ui.request::<qm::Answer>(answer(decision.id, Some("1"), None))
                .await
        ),
        codes::QUESTION_CLOSED
    );
    let dismissed = ui
        .request::<qm::Dismiss>(QuestionParams { id: confirm.id })
        .await
        .unwrap();
    assert_eq!(dismissed.status, QuestionStatus::Dismissed);

    // Resuming from before both: each question's newest state, once.
    let mut changes = ui
        .subscribe::<qm::Changes>(ChangesParams::default(), Some(seq))
        .await
        .unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let e = tokio::time::timeout(TIMEOUT, changes.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seen.push((e.event.id, e.event.status, e.seq == e.event.seq));
    }
    assert_eq!(
        seen,
        [
            (decision.id, QuestionStatus::Answered, true),
            (confirm.id, QuestionStatus::Dismissed, true)
        ]
    );
    // Ahead of the source: take a fresh snapshot.
    let r = ui
        .subscribe::<qm::Changes>(ChangesParams::default(), Some(seq + 100))
        .await;
    assert_eq!(code(r), coqo_protocol::codes::RESUME_GAP);
    // Open ones only by default; one file only.
    let open = ui
        .request::<qm::List>(ListParams {
            workspace: ws.id.clone(),
            root: None,
            path: Some("notes.txt".into()),
            session: None,
            include_closed: true,
        })
        .await
        .unwrap();
    assert!(open.questions.is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn questions_are_bound_to_their_session_and_workspace() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path(), &ScriptedProvider::new()).await;
    let ui = daemon.ui().await;
    let (_a_dir, a) = project(&ui).await;
    let (_b_dir, b) = project(&ui).await;
    let agent = daemon.agent(&a, "s-1").await;

    let mine = agent
        .request::<qm::Create>(create(&a, "s-1", QuestionKind::Clarification, (0, 0)))
        .await
        .unwrap();
    let theirs = ui
        .request::<qm::Create>(create(&a, "s-2", QuestionKind::Clarification, (0, 0)))
        .await
        .unwrap();
    let forbidden = coqo_protocol::codes::FORBIDDEN;
    // Another workspace, another session: refused by the route policy.
    for p in [
        create(&b, "s-1", QuestionKind::Clarification, (0, 0)),
        create(&a, "s-2", QuestionKind::Clarification, (0, 0)),
    ] {
        assert_eq!(code(agent.request::<qm::Create>(p).await), forbidden);
    }
    let watch = |ws: &WorkspaceInfo, session: &str, id| WatchParams {
        workspace: ws.id.clone(),
        session: session.into(),
        id,
    };
    assert_eq!(
        code(
            agent
                .subscribe::<qm::Watch>(watch(&a, "s-2", theirs.id), None)
                .await
        ),
        forbidden
    );
    assert_eq!(
        code(
            agent
                .subscribe::<qm::Watch>(watch(&b, "s-1", mine.id), None)
                .await
        ),
        forbidden
    );
    // Another session's question in its own workspace: as if unknown.
    assert_eq!(
        code(
            agent
                .subscribe::<qm::Watch>(watch(&a, "s-1", theirs.id), None)
                .await
        ),
        codes::UNKNOWN_QUESTION
    );
    // The UI's routes need the daemon token.
    let r = agent
        .request::<qm::Answer>(AnswerParams {
            id: mine.id,
            option: None,
            text: Some("self-answer".into()),
        })
        .await;
    assert_eq!(code(r), forbidden);
    let mut own = agent
        .subscribe::<qm::Watch>(watch(&a, "s-1", mine.id), None)
        .await
        .unwrap();
    let first = own.next().await.unwrap().unwrap().event;
    assert_eq!((first.id, first.waiting), (mine.id, true));
    daemon.stop().await;
}

#[tokio::test]
async fn a_session_needs_input_while_its_question_is_open() {
    let data = tempfile::tempdir().unwrap();
    let release = Arc::new(Notify::new());
    let provider = ScriptedProvider::new().turn([
        Step::Wait(Arc::clone(&release)),
        Step::Stop(StopReason::EndTurn),
    ]);
    let daemon = Daemon::start(data.path(), &provider).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui).await;
    let session = ui
        .request::<agm::Create>(CreateSessionParams {
            provider: "fake".into(),
            cwd: dir.path().to_str().unwrap().into(),
            workspace: Some(ws.id.0.clone()),
            title: None,
            mode: None,
        })
        .await
        .unwrap();
    let id = session.id.clone();
    ui.request::<agm::Prompt>(PromptParams {
        session: id.clone(),
        content: vec![ContentBlock::text("go")],
    })
    .await
    .unwrap();
    let state = async |ui: &Client| -> SessionInfo {
        ui.request::<agm::Get>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap()
        .info
    };
    assert_eq!(state(&ui).await.state, AgentState::Working);

    let agent = daemon.agent(&ws, &id.0).await;
    let note = agent
        .request::<qm::Create>(create(&ws, &id.0, QuestionKind::Annotation, (0, 0)))
        .await
        .unwrap();
    assert_eq!(
        state(&ui).await.state,
        AgentState::Working,
        "a note waits for nothing"
    );
    let q = agent
        .request::<qm::Create>(create(&ws, &id.0, QuestionKind::Clarification, (4, 4)))
        .await
        .unwrap();
    let info = state(&ui).await;
    assert_eq!(info.state, AgentState::NeedsInput);
    assert_eq!(
        info.state_reason.as_deref(),
        Some("Waiting for your answer: Why **3.14**?")
    );
    ui.request::<qm::Answer>(AnswerParams {
        id: q.id,
        option: None,
        text: Some("It is π".into()),
    })
    .await
    .unwrap();
    assert_eq!(state(&ui).await.state, AgentState::Working);

    // The transcript has the note, the question, and its answer.
    let mut updates = ui
        .subscribe::<agm::Updates>(
            SessionParams {
                session: id.clone(),
            },
            Some(0),
        )
        .await
        .unwrap();
    let mut questions = Vec::new();
    while questions.len() < 3 {
        let u = tokio::time::timeout(TIMEOUT, updates.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let SessionUpdate::Question(q) = u.event {
            questions.push((q.id, q.status));
        }
    }
    assert_eq!(
        questions,
        [
            (note.id, QuestionStatus::Open),
            (q.id, QuestionStatus::Open),
            (q.id, QuestionStatus::Answered)
        ]
    );

    // After the turn ends, an open question keeps it waiting; the answer
    // makes it done.
    let q = agent
        .request::<qm::Create>(create(&ws, &id.0, QuestionKind::Confirmation, (4, 4)))
        .await
        .unwrap();
    release.notify_one();
    until("the turn to end", async || {
        let info = state(&ui).await;
        (info.state == AgentState::NeedsInput
            && !ui
                .request::<agm::Get>(SessionParams {
                    session: id.clone(),
                })
                .await
                .unwrap()
                .running)
            .then_some(())
    })
    .await;
    ui.request::<qm::Dismiss>(QuestionParams { id: q.id })
        .await
        .unwrap();
    assert_eq!(state(&ui).await.state, AgentState::Done);
    daemon.stop().await;
}

// ---- the bridge ---------------------------------------------------------------

/// The agent's end of a `coqo-editor` bridge.
struct Agent {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    next_id: u64,
    _bridge: JoinHandle<Result<Exit, BridgeError>>,
}

impl Agent {
    async fn start(daemon: Client, ws: &WorkspaceInfo, session: &str) -> Self {
        let registry = ToolRegistry::new();
        coqo_mcp::tools::core(&registry);
        let options = BridgeOptions {
            workspace: ws.id.clone(),
            session: session.into(),
            build: "test".into(),
        };
        let (agent_io, bridge_io) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(bridge_io);
        let bridge =
            tokio::spawn(
                async move { coqo_mcp::serve(daemon, &options, registry, read, write).await },
            );
        let (read, writer) = tokio::io::split(agent_io);
        let mut agent = Self {
            lines: BufReader::new(read).lines(),
            writer,
            next_id: 1,
            _bridge: bridge,
        };
        let id = agent
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test-agent", "version": "0" },
                }),
            )
            .await;
        agent.response(id).await;
        agent
            .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        agent
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send_request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        id
    }

    /// The response to request `id`, skipping notifications.
    async fn response(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(TIMEOUT, self.lines.next_line())
                .await
                .expect("a message in time")
                .unwrap()
                .expect("stdout open");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    /// Start a tool call; its request id.
    async fn start_call(&mut self, name: &str, arguments: Value) -> u64 {
        self.send_request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    /// Call `name`; the result, which must not be an error.
    async fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.start_call(name, arguments.clone()).await;
        let r = self.response(id).await["result"].clone();
        assert_eq!(r["isError"], false, "{name} {arguments}: {r}");
        r
    }
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

async fn get(ui: &Client, id: QuestionId) -> Question {
    ui.request::<qm::Get>(QuestionParams { id }).await.unwrap()
}

async fn until_waiting(ui: &Client, id: QuestionId, waiting: bool) {
    until("the wait to change", async || {
        (get(ui, id).await.waiting == waiting).then_some(())
    })
    .await;
}

#[tokio::test]
async fn ask_answer_and_await_through_the_bridge() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path(), &ScriptedProvider::new()).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui).await;
    let mut agent = Agent::start(daemon.agent(&ws, "s-1").await, &ws, "s-1").await;
    let main_rs = dir.path().join("src/main.rs");

    // Asked without waiting: it is listed for the UI.
    let r = agent
        .ok(
            "ask_question",
            json!({
                "path": main_rs,
                "start_line": 4,
                "end_line": 6,
                "question": "Should `area` use `std::f64::consts::PI`?",
                "options": ["Yes", "No"],
                "wait": false,
            }),
        )
        .await;
    let id = QuestionId(r["structuredContent"]["id"].as_u64().unwrap());
    assert!(text(&r).contains("await_answer"), "{r}");
    let listed = list(&ui, &ws).await;
    assert_eq!(listed.questions.len(), 1);
    let q = &listed.questions[0];
    assert_eq!(
        (q.id, q.kind, q.session.0.as_str()),
        (id, QuestionKind::Decision, "s-1")
    );
    assert_eq!(q.anchors[0].anchor.path, "src/main.rs");
    assert_eq!(
        q.anchors[0].anchor.text,
        "    fn area(&self) -> f64 {\n        3.14 * self.r * self.r\n    }\n"
    );
    ui.request::<qm::Answer>(AnswerParams {
        id,
        option: Some("1".into()),
        text: Some("and add a test".into()),
    })
    .await
    .unwrap();
    // Already answered: await_answer returns at once.
    let r = agent.ok("await_answer", json!({ "id": id.0 })).await;
    assert_eq!(r["structuredContent"]["status"], "answered", "{r}");
    assert_eq!(r["structuredContent"]["answer"]["label"], "Yes");
    assert!(
        text(&r).contains("Choice: Yes") && text(&r).contains("and add a test"),
        "{r}"
    );

    // Asked with a wait: the call returns when the user answers.
    let call = agent
        .start_call(
            "ask_question",
            json!({ "path": "src/main.rs", "line": 9, "question": "Is `main` meant to be empty?" }),
        )
        .await;
    let q = until("the question", async || {
        list(&ui, &ws)
            .await
            .questions
            .into_iter()
            .find(|q| q.status == QuestionStatus::Open)
    })
    .await;
    assert_eq!(q.kind, QuestionKind::Clarification);
    until_waiting(&ui, q.id, true).await;
    ui.request::<qm::Answer>(AnswerParams {
        id: q.id,
        option: None,
        text: Some("Yes, for now.".into()),
    })
    .await
    .unwrap();
    let r = agent.response(call).await["result"].clone();
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["answer"]["text"], "Yes, for now.");
    until_waiting(&ui, q.id, false).await;

    // A wait that runs out leaves the question open.
    let r = agent
        .ok(
            "ask_question",
            json!({
                "path": "src/main.rs",
                "line": 1,
                "question": "Rename `Circle`?",
                "kind": "confirmation",
                "timeout_s": 1,
            }),
        )
        .await;
    assert_eq!(r["structuredContent"]["status"], "open", "{r}");
    assert!(text(&r).contains("No answer yet"), "{r}");
    // Clients that show only the structured result still get the advice.
    let message = r["structuredContent"]["message"].as_str().unwrap();
    assert!(message.contains("await_answer"), "{r}");
    let late = QuestionId(r["structuredContent"]["id"].as_u64().unwrap());
    ui.request::<qm::Dismiss>(QuestionParams { id: late })
        .await
        .unwrap();
    let r = agent.ok("await_answer", json!({ "id": late.0 })).await;
    assert_eq!(r["structuredContent"]["status"], "dismissed", "{r}");

    // A note: non-blocking, listed with its severity.
    let r = agent
        .ok(
            "annotate",
            json!({ "path": "notes.txt", "line": 2, "note": "Typo?", "severity": "warning" }),
        )
        .await;
    let note = QuestionId(r["structuredContent"]["id"].as_u64().unwrap());
    let n = get(&ui, note).await;
    assert_eq!(
        (n.kind, n.severity),
        (QuestionKind::Annotation, Some(Severity::Warning))
    );
    assert_eq!(n.anchors[0].anchor.text, "two\n");

    // Bad arguments and other sessions' questions are tool errors.
    let call = agent
        .start_call(
            "ask_question",
            json!({ "path": "notes.txt", "line": 40, "question": "?" }),
        )
        .await;
    let r = agent.response(call).await["result"].clone();
    assert_eq!(r["isError"], true, "{r}");
    assert!(text(&r).contains("past its end"), "{r}");
    let other = ui
        .request::<qm::Create>(create(&ws, "s-2", QuestionKind::Clarification, (0, 0)))
        .await
        .unwrap();
    let call = agent
        .start_call("await_answer", json!({ "id": other.id.0 }))
        .await;
    let r = agent.response(call).await["result"].clone();
    assert_eq!(r["isError"], true, "{r}");
    daemon.stop().await;
}

#[tokio::test]
async fn cancelling_a_wait_releases_it() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path(), &ScriptedProvider::new()).await;
    let ui = daemon.ui().await;
    let (_dir, ws) = project(&ui).await;
    let mut agent = Agent::start(daemon.agent(&ws, "s-1").await, &ws, "s-1").await;

    let call = agent
        .start_call(
            "ask_question",
            json!({ "path": "src/main.rs", "line": 5, "question": "Why 3.14?" }),
        )
        .await;
    let q = until("the question", async || {
        list(&ui, &ws).await.questions.pop()
    })
    .await;
    until_waiting(&ui, q.id, true).await;
    agent
        .send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": call, "reason": "user pressed Esc" },
        }))
        .await;
    until_waiting(&ui, q.id, false).await;
    assert_eq!(get(&ui, q.id).await.status, QuestionStatus::Open);

    // The answer still reaches a later await_answer.
    ui.request::<qm::Answer>(AnswerParams {
        id: q.id,
        option: None,
        text: Some("Laziness".into()),
    })
    .await
    .unwrap();
    let r = agent.ok("await_answer", json!({ "id": q.id.0 })).await;
    assert_eq!(r["structuredContent"]["answer"]["text"], "Laziness");
    daemon.stop().await;
}
