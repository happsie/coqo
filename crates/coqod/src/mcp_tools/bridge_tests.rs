// SPDX-License-Identifier: Apache-2.0

//! Every `coqo-editor` tool end to end: an agent speaking MCP to the bridge
//! (`coqo_mcp::serve`), which holds a session token for workspace A and
//! calls the real router in-process. Results are right, the editor context
//! a UI publishes reaches `editor_context`, `editor_reveal` reaches a UI
//! following reveals, `set_status` and `set_acceptance_criteria` reach the
//! session and the UI's `sessions/changes`, a bridge pointed at another workspace or session gets
//! nothing, and a mod's tool appears with `tools/list_changed` and is
//! callable.

use std::sync::Arc;
use std::time::Duration;

use coqo_agents::model::{
    self as agm, AgentState, ChangesParams, CreateSessionParams, ListSessionsParams, PromptParams,
    SessionId, SessionInfo, SessionParams, StopReason,
};
use coqo_agents::testing::{ScriptedProvider, Step};
use coqo_kernel::ProviderKind;
use coqo_mcp::{BridgeError, BridgeOptions, Exit, ToolRegistry};
use coqo_mod_api::Manifest;
use coqo_protocol::methods::editor::{
    self as edm, EditorTab, LineRange, Position, PublishContextParams, Range, RevealsParams,
    Selection, WindowContext,
};
use coqo_protocol::methods::workspace::{self as wsm, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig, Server, ServerConfig, SessionToken};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{
    AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::tests::{ACME_MANIFEST, Acme, PROBE_TOOL};
use crate::daemon::DaemonContext;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    _dir: TempDir,
    ctx: DaemonContext,
    server: Server,
}

impl Harness {
    fn start() -> Self {
        Self::start_with(ScriptedProvider::new())
    }

    /// With `agent` as the `fake` provider, once the mods start.
    fn start_with(agent: ScriptedProvider) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = routes::tests::context_with(dir.path(), agent);
        let mut config = ServerConfig::new("coqod", "test");
        config.authenticator = Some(Arc::new(ctx.auth.clone()));
        let server = Server::new(routes::router(&ctx).unwrap(), config);
        Self {
            _dir: dir,
            ctx,
            server,
        }
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.ctx.shutdown.cancel();
    }
}

/// A project folder opened as a workspace.
async fn project(ui: &Client) -> (TempDir, WorkspaceInfo) {
    let dir = tempfile::tempdir().unwrap();
    let write = |path: &str, text: &str| {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("README.md", "# Demo\n");
    write(
        "src/main.rs",
        "struct Circle { r: f64 }\n\nimpl Circle {\n    fn area(&self) -> f64 {\n        3.14 * self.r * self.r\n    }\n}\n\nfn main() {}\n",
    );
    let info = ui
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    (dir, info)
}

/// The agent's end of a bridge.
struct Agent {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    notifications: Vec<Value>,
    next_id: u64,
    _bridge: JoinHandle<Result<Exit, BridgeError>>,
}

impl Agent {
    /// A bridge for `workspace` on `daemon`, initialized.
    async fn start(daemon: Client, workspace: &WorkspaceInfo) -> Self {
        Self::start_as(daemon, workspace, "s-1").await
    }

    /// A bridge for `workspace` and agent session `session` on `daemon`,
    /// initialized.
    async fn start_as(daemon: Client, workspace: &WorkspaceInfo, session: &str) -> Self {
        let registry = ToolRegistry::new();
        coqo_mcp::tools::core(&registry);
        let options = BridgeOptions {
            workspace: workspace.id.clone(),
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
            notifications: Vec::new(),
            next_id: 1,
            _bridge: bridge,
        };
        agent
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test-agent", "version": "0" },
                }),
            )
            .await;
        agent
            .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        agent.request("ping", json!({})).await;
        agent
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(TIMEOUT, self.lines.next_line())
            .await
            .expect("a message in time")
            .unwrap()
            .expect("stdout open");
        serde_json::from_str(&line).unwrap()
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        loop {
            let message = self.next().await;
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
            self.notifications.push(message);
        }
    }

    async fn notification(&mut self, method: &str) -> Value {
        if let Some(i) = self
            .notifications
            .iter()
            .position(|n| n["method"] == method)
        {
            return self.notifications.remove(i);
        }
        loop {
            let message = self.next().await;
            if message["method"] == method {
                return message;
            }
            self.notifications.push(message);
        }
    }

    async fn names(&mut self) -> Vec<String> {
        let list = self.request("tools/list", json!({})).await;
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Call `name`; the result, which must not be an error.
    async fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let r = self.call(name, arguments.clone()).await;
        assert_eq!(r["isError"], false, "{name} {arguments}: {r}");
        r
    }

    /// Call `name`; the error text, which it must be.
    async fn error(&mut self, name: &str, arguments: Value) -> String {
        let r = self.call(name, arguments.clone()).await;
        assert_eq!(r["isError"], true, "{name} {arguments}: {r}");
        text(&r).to_owned()
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let r = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        r["result"].clone()
    }
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

fn root(ws: &WorkspaceInfo) -> &str {
    &ws.roots[0].path
}

#[tokio::test]
async fn core_tools_end_to_end() {
    let h = Harness::start();
    let ui = h.ui().await;
    let (a_dir, a) = project(&ui).await;
    let token = h.ctx.auth.mint_session(a.id.0.clone(), "s-1").unwrap();
    let mut agent = Agent::start(h.connect(token).await, &a).await;

    // Exactly the editor-only tools, plus mod tools (ADR 0014).
    let mut names = agent.names().await;
    names.sort();
    assert_eq!(
        names,
        [
            "annotate",
            "ask_question",
            "await_answer",
            "editor_context",
            "editor_reveal",
            PROBE_TOOL,
            "publish_walkthrough",
            "set_acceptance_criteria",
            "set_status",
        ]
    );

    // editor_context: the workspace's roots and the session, and no
    // window until a UI publishes.
    let cx = agent.ok("editor_context", json!({})).await;
    assert_eq!(cx["structuredContent"]["attached"], false);
    assert_eq!(cx["structuredContent"]["workspace"]["id"], json!(a.id));
    assert_eq!(cx["structuredContent"]["workspace"]["session"], "s-1");
    assert_eq!(
        cx["structuredContent"]["workspace"]["roots"][0]["path"],
        json!(root(&a))
    );
    ui.notify::<edm::PublishContext>(PublishContextParams {
        workspace: a.id.clone(),
        window: "w-1".into(),
        context: Some(WindowContext {
            focused: true,
            tabs: vec![
                EditorTab {
                    root: a.roots[0].id,
                    path: "src/main.rs".into(),
                    active: true,
                    dirty: true,
                    selections: vec![Selection {
                        range: Range::new(Position::new(3, 7), Position::new(3, 11)),
                        text: Some("area".into()),
                        truncated: false,
                    }],
                    visible: Some(LineRange { start: 0, end: 8 }),
                },
                EditorTab {
                    root: a.roots[0].id,
                    path: "README.md".into(),
                    active: false,
                    dirty: false,
                    selections: Vec::new(),
                    visible: None,
                },
            ],
        }),
    })
    .await
    .unwrap();
    let cx = tokio::time::timeout(TIMEOUT, async {
        loop {
            let cx = agent.ok("editor_context", json!({})).await;
            if cx["structuredContent"]["attached"] == true {
                return cx;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the context in time");
    let main = format!("{}/src/main.rs", root(&a));
    let s = &cx["structuredContent"];
    assert_eq!(s["activeFile"]["path"], json!(main));
    assert_eq!(s["activeFile"]["unsaved"], true);
    assert_eq!(
        s["activeFile"]["selections"][0],
        json!({ "start": { "line": 4, "column": 8 }, "end": { "line": 4, "column": 12 }, "text": "area" })
    );
    assert_eq!(
        s["activeFile"]["visibleLines"],
        json!({ "start": 1, "end": 9 })
    );
    assert_eq!(s["openTabs"].as_array().unwrap().len(), 2);
    let summary = text(&cx);
    assert!(
        summary.contains(&format!("Active file: {main} (unsaved changes)")),
        "{summary}"
    );
    assert!(
        summary.contains("Selection: 4:8-4:12\n```\narea\n```"),
        "{summary}"
    );
    assert!(
        summary.contains(&format!("- {}/README.md", root(&a))),
        "{summary}"
    );

    // editor_reveal reaches a UI following reveals.
    let mut reveals = ui
        .subscribe::<edm::Reveals>(RevealsParams::default(), None)
        .await
        .unwrap();
    let r = agent
        .ok(
            "editor_reveal",
            json!({ "path": main, "line": 4, "column": 8, "end_column": 12 }),
        )
        .await;
    assert_eq!(r["structuredContent"]["delivered"], true, "{r}");
    let event = tokio::time::timeout(TIMEOUT, reveals.next())
        .await
        .expect("a reveal in time")
        .unwrap()
        .unwrap()
        .event;
    assert_eq!(event.path, "src/main.rs");
    assert_eq!(event.session.as_deref(), Some("s-1"));
    assert_eq!(
        event.range,
        Some(Range::new(Position::new(3, 7), Position::new(3, 11)))
    );

    // Through a symlink to the root (an agent whose cwd is one), and as
    // the temp folder was given (not canonical on macOS).
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("project");
    std::os::unix::fs::symlink(a_dir.path(), &link).unwrap();
    for path in [link.join("README.md"), a_dir.path().join("README.md")] {
        let r = agent.ok("editor_reveal", json!({ "path": path })).await;
        assert_eq!(
            r["structuredContent"]["path"],
            json!(format!("{}/README.md", root(&a)))
        );
        let event = tokio::time::timeout(TIMEOUT, reveals.next())
            .await
            .expect("a reveal in time")
            .unwrap()
            .unwrap()
            .event;
        assert_eq!(event.path, "README.md");
    }

    // Bad arguments are tool errors the model can read.
    assert!(
        agent
            .error("editor_reveal", json!({ "path": main, "line": 0 }))
            .await
            .contains("`line` must be at least 1")
    );
    assert!(
        agent
            .error("editor_reveal", json!({ "path": "nope.rs" }))
            .await
            .contains("nope.rs")
    );
    assert!(
        agent
            .error("editor_reveal", json!({ "path": "/etc/passwd" }))
            .await
            .contains("outside the workspace")
    );
}

#[tokio::test]
async fn other_workspaces_are_forbidden() {
    let h = Harness::start();
    let ui = h.ui().await;
    let (_a_dir, a) = project(&ui).await;
    let (_b_dir, b) = project(&ui).await;
    // A token for A, in a bridge told to work on B.
    let token = h.ctx.auth.mint_session(a.id.0.clone(), "s-1").unwrap();
    let mut agent = Agent::start(h.connect(token).await, &b).await;
    for (name, args) in [
        ("editor_context", json!({})),
        ("editor_reveal", json!({ "path": "src/main.rs" })),
    ] {
        let error = agent.error(name, args).await;
        assert!(
            error.contains("outside this connection's scope"),
            "{name}: {error}"
        );
    }
    // Listing B's mod tools was refused too, so there are none.
    assert!(!agent.names().await.iter().any(|n| n == PROBE_TOOL));
}

#[tokio::test]
async fn mod_tools_come_and_go() {
    let h = Harness::start();
    let ui = h.ui().await;
    let (_a_dir, a) = project(&ui).await;
    let token = h.ctx.auth.mint_session(a.id.0.clone(), "s-1").unwrap();
    let mut agent = Agent::start(h.connect(token).await, &a).await;

    // A built-in mod's tool, under its own name, activating its mod.
    let r = agent.ok(PROBE_TOOL, json!({ "text": "hi" })).await;
    assert_eq!(
        r["structuredContent"],
        json!({ "workspace": a.id, "session": "s-1", "text": "hi" })
    );
    assert!(
        agent
            .error(PROBE_TOOL, json!({}))
            .await
            .contains("`text` is required")
    );

    // An installed mod's tool appears, prefixed, with list_changed.
    let manager = &h.ctx.mods.manager;
    manager
        .load(
            Manifest::from_toml_str(ACME_MANIFEST).unwrap(),
            Box::new(Acme),
            ProviderKind::Mod,
        )
        .unwrap();
    manager
        .activate(&"acme.build".parse().unwrap())
        .await
        .unwrap();
    agent.notification("notifications/tools/list_changed").await;
    let list = agent.request("tools/list", json!({})).await;
    let acme = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "acme_build_run_tests")
        .cloned()
        .expect("acme's tool");
    assert_eq!(acme["title"], "Run tests");
    assert!(
        agent
            .error("acme_build_run_tests", json!({}))
            .await
            .contains("3 tests failed")
    );

    // Disabled: gone again.
    manager
        .disable(&"acme.build".parse().unwrap())
        .await
        .unwrap();
    agent.notification("notifications/tools/list_changed").await;
    let names = agent.names().await;
    assert!(
        !names.iter().any(|n| n == "acme_build_run_tests"),
        "{names:?}"
    );
    assert!(names.iter().any(|n| n == PROBE_TOOL), "{names:?}");
}

/// The next `sessions/changes` event of `session`.
async fn next_change(
    changes: &mut coqo_protocol::Subscription<agm::Changes>,
    session: &SessionId,
) -> SessionInfo {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let change = changes.next().await.expect("open").expect("ok").event;
            if &change.id == session {
                return change;
            }
        }
    })
    .await
    .expect("a change in time")
}

#[tokio::test]
async fn set_status_reaches_the_session_and_the_ui() {
    let go = Arc::new(Notify::new());
    let provider = ScriptedProvider::new()
        .turn([Step::Wait(Arc::clone(&go)), Step::Stop(StopReason::EndTurn)]);
    let h = Harness::start_with(provider);
    h.ctx.mods.start().await;
    let ui = h.ui().await;
    let (a_dir, a) = project(&ui).await;
    let create = || CreateSessionParams {
        provider: "fake".into(),
        cwd: a_dir.path().to_str().unwrap().to_owned(),
        workspace: Some(a.id.0.clone()),
        title: Some("Fix the tests".into()),
        mode: None,
    };
    let session = ui.request::<agm::Create>(create()).await.unwrap().id;
    let other = ui.request::<agm::Create>(create()).await.unwrap().id;
    let seq = ui
        .request::<agm::List>(ListSessionsParams::default())
        .await
        .unwrap()
        .seq;
    let mut changes = ui
        .subscribe::<agm::Changes>(ChangesParams::default(), Some(seq))
        .await
        .unwrap();
    ui.request::<agm::Prompt>(PromptParams {
        session: session.clone(),
        content: vec![agm::ContentBlock::text("fix the tests")],
    })
    .await
    .unwrap();
    assert_eq!(
        next_change(&mut changes, &session).await.state,
        AgentState::Working
    );

    let token = h
        .ctx
        .auth
        .mint_session(a.id.0.clone(), session.0.clone())
        .unwrap();
    let mut agent = Agent::start_as(h.connect(token.clone()).await, &a, &session.0).await;
    assert!(agent.names().await.iter().any(|n| n == "set_status"));
    let tool = agent.request("tools/list", json!({})).await["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "set_status")
        .cloned()
        .unwrap();
    assert_eq!(tool["inputSchema"]["required"], json!(["status"]));

    // Progress: the UI sees it on sessions/changes.
    let r = agent
        .ok(
            "set_status",
            json!({ "status": "Running the tests", "progress": { "current": 2, "total": 5 } }),
        )
        .await;
    assert!(text(&r).contains("Running the tests"), "{r}");
    assert!(text(&r).contains("2 of 5"), "{r}");
    assert_eq!(r["structuredContent"]["state"], "working");
    let seen = next_change(&mut changes, &session).await;
    let status = seen.status.clone().unwrap();
    assert_eq!(status.text, "Running the tests");
    assert_eq!(status.progress.map(|p| (p.current, p.total)), Some((2, 5)));

    // Blocked: the session needs the user.
    let r = agent
        .ok(
            "set_status",
            json!({ "status": "Need DATABASE_URL to run the integration tests", "state": "blocked" }),
        )
        .await;
    assert_eq!(r["structuredContent"]["state"], "blocked");
    let seen = next_change(&mut changes, &session).await;
    assert_eq!(seen.state, AgentState::Blocked);
    assert_eq!(
        seen.state_reason.as_deref(),
        Some("Need DATABASE_URL to run the integration tests")
    );

    // Bad arguments never reach the daemon.
    let e = agent
        .error(
            "set_status",
            json!({ "status": "x", "state": "needs_input" }),
        )
        .await;
    assert!(e.contains("invalid arguments"), "{e}");

    // A bridge told it works for another session, with this session's
    // token, reaches nothing: the route checks the token's session.
    let ticket = h
        .ctx
        .auth
        .mint_session(a.id.0.clone(), session.0.clone())
        .unwrap();
    let mut lying = Agent::start_as(h.connect(ticket).await, &a, &other.0).await;
    let e = lying
        .error("set_status", json!({ "status": "Not mine" }))
        .await;
    assert!(e.contains("outside this connection's scope"), "{e}");
    let snap = ui
        .request::<agm::Get>(SessionParams {
            session: other.clone(),
        })
        .await
        .unwrap();
    assert!(snap.info.status.is_none(), "{:?}", snap.info);

    // The turn ends; the agent said it is blocked, so that stays.
    go.notify_one();
    let ended = tokio::time::timeout(TIMEOUT, async {
        loop {
            let s = ui
                .request::<agm::Get>(SessionParams {
                    session: session.clone(),
                })
                .await
                .unwrap();
            if !s.running {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(ended.info.state, AgentState::Blocked);
    assert!(ended.info.status.is_some());
    drop(a_dir);
}

#[tokio::test]
async fn acceptance_criteria_reach_the_session_and_the_ui() {
    let h = Harness::start();
    h.ctx.mods.start().await;
    let ui = h.ui().await;
    let (a_dir, a) = project(&ui).await;
    let create = || CreateSessionParams {
        provider: "fake".into(),
        cwd: a_dir.path().to_str().unwrap().to_owned(),
        workspace: Some(a.id.0.clone()),
        title: Some("Volume discount".into()),
        mode: None,
    };
    let session = ui.request::<agm::Create>(create()).await.unwrap().id;
    let other = ui.request::<agm::Create>(create()).await.unwrap().id;
    let seq = ui
        .request::<agm::List>(ListSessionsParams::default())
        .await
        .unwrap()
        .seq;
    let mut changes = ui
        .subscribe::<agm::Changes>(ChangesParams::default(), Some(seq))
        .await
        .unwrap();

    let token = h
        .ctx
        .auth
        .mint_session(a.id.0.clone(), session.0.clone())
        .unwrap();
    let mut agent = Agent::start_as(h.connect(token.clone()).await, &a, &session.0).await;
    assert!(
        agent
            .names()
            .await
            .iter()
            .any(|n| n == "set_acceptance_criteria")
    );

    // The agent registers its criteria at the start of the task.
    let r = agent
        .ok(
            "set_acceptance_criteria",
            json!({
                "criteria": [
                    { "text": "100+ units get 10% off" },
                    { "text": "Discount capped at 500 EUR" },
                ],
                "replace": true,
            }),
        )
        .await;
    assert!(
        text(&r).contains("- AC1 [pending] 100+ units get 10% off"),
        "{r}"
    );
    assert_eq!(r["structuredContent"]["criteria"][1]["id"], "AC2");
    let seen = next_change(&mut changes, &session).await;
    assert_eq!(seen.criteria.len(), 2);
    assert_eq!(seen.criteria[0].changed_by, agm::CriterionAuthor::Agent);

    // It verifies one, with a test as evidence.
    let r = agent
        .ok(
            "set_acceptance_criteria",
            json!({ "criteria": [{
                "id": "AC1",
                "status": "met",
                "evidence": [{ "text": "test volume_discount passes", "path": "src/main.rs", "line": 4 }],
            }] }),
        )
        .await;
    assert!(text(&r).contains("(1 of 2 met)"), "{r}");
    let seen = next_change(&mut changes, &session).await;
    let ac1 = &seen.criteria[0];
    assert_eq!(ac1.status, agm::CriterionStatus::Met);
    let expected = a_dir.path().join("src/main.rs");
    assert_eq!(
        ac1.evidence[0].path.as_deref(),
        Some(expected.to_str().unwrap())
    );

    // The user marks the other one failed (the daemon token): the agent
    // sees it on its next call.
    ui.request::<agm::SetCriteria>(agm::SetCriteriaParams {
        session: session.clone(),
        criteria: vec![agm::CriterionInput {
            id: Some("AC2".into()),
            status: Some(agm::CriterionStatus::Failed),
            ..agm::CriterionInput::default()
        }],
        replace: false,
    })
    .await
    .unwrap();
    let r = agent
        .ok("set_acceptance_criteria", json!({ "criteria": [] }))
        .await;
    assert!(text(&r).contains("- AC2 [failed, set by the user]"), "{r}");

    // Bad arguments never reach the daemon; bad ids are the daemon's to
    // refuse.
    let e = agent
        .error(
            "set_acceptance_criteria",
            json!({ "criteria": [{ "status": "done" }] }),
        )
        .await;
    assert!(e.contains("invalid arguments"), "{e}");
    let e = agent
        .error(
            "set_acceptance_criteria",
            json!({ "criteria": [{ "id": "AC7", "status": "met" }] }),
        )
        .await;
    assert!(e.contains("needs `text`"), "{e}");

    // A bridge told it works for another session, with this session's
    // token, reaches nothing.
    let ticket = h
        .ctx
        .auth
        .mint_session(a.id.0.clone(), session.0.clone())
        .unwrap();
    let mut lying = Agent::start_as(h.connect(ticket).await, &a, &other.0).await;
    let e = lying
        .error(
            "set_acceptance_criteria",
            json!({ "criteria": [{ "text": "Not mine" }] }),
        )
        .await;
    assert!(e.contains("outside this connection's scope"), "{e}");
    let snap = ui
        .request::<agm::Get>(SessionParams {
            session: other.clone(),
        })
        .await
        .unwrap();
    assert!(snap.info.criteria.is_empty(), "{:?}", snap.info);
    drop(a_dir);
}

#[tokio::test]
async fn publish_walkthrough_end_to_end() {
    use coqo_protocol::methods::walkthroughs as wtm;

    let h = Harness::start();
    h.ctx.mods.start().await;
    let ui = h.ui().await;
    let (a_dir, a) = project(&ui).await;
    let create = || CreateSessionParams {
        provider: "fake".into(),
        cwd: a_dir.path().to_str().unwrap().to_owned(),
        workspace: Some(a.id.0.clone()),
        title: Some("Circle area".into()),
        mode: None,
    };
    let session = ui.request::<agm::Create>(create()).await.unwrap().id;
    let other = ui.request::<agm::Create>(create()).await.unwrap().id;
    let token = h
        .ctx
        .auth
        .mint_session(a.id.0.clone(), session.0.clone())
        .unwrap();
    let mut agent = Agent::start_as(h.connect(token).await, &a, &session.0).await;

    // The tool, and the schema as a resource.
    let list = agent.request("tools/list", json!({})).await;
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "publish_walkthrough")
        .expect("publish_walkthrough is offered")
        .clone();
    assert!(
        tool["inputSchema"]["$defs"]["StepDraft"].is_object(),
        "{tool}"
    );
    let resources = agent.request("resources/list", json!({})).await;
    assert_eq!(
        resources["result"]["resources"][0]["uri"],
        wtm::SCHEMA_URI,
        "{resources}"
    );
    let read = agent
        .request("resources/read", json!({ "uri": wtm::SCHEMA_URI }))
        .await;
    let text_json = read["result"]["contents"][0]["text"].as_str().unwrap();
    let schema: Value = serde_json::from_str(text_json).unwrap();
    assert_eq!(schema["$id"], wtm::SCHEMA_URI);
    let missing = agent
        .request("resources/read", json!({ "uri": "coqo://nope" }))
        .await;
    assert!(missing["error"].is_object(), "{missing}");

    let walkthrough = json!({
        "title": "Circle area",
        "summary": "Circles compute their area.",
        "scope": { "kind": "worktree" },
        "steps": [{
            "id": "area",
            "title": "The area",
            "kind": "logic",
            "anchors": [{ "path": "src/main.rs", "start_line": 4, "end_line": 6, "text": "fn area" }],
            "what": "pi r squared.",
            "why": "Geometry.",
        }],
    });

    // A bad draft: every problem at once, and nothing published. The
    // bridge does not cut the list short with its own schema check.
    let mut bad = walkthrough.clone();
    bad["title"] = json!("");
    bad["steps"][0]["kind"] = json!("maths");
    bad["steps"][0]["anchors"][0]["start_line"] = json!(1);
    bad["steps"][0]["anchors"][0]["end_line"] = json!(2);
    bad["steps"][0]["depends_on"] = json!(["base"]);
    let e = agent.error("publish_walkthrough", bad).await;
    assert!(
        e.starts_with("the walkthrough was not published: 4 problems."),
        "{e}"
    );
    for part in [
        "\n- title: is empty",
        "\n- steps[0].kind: `maths` is not a step kind",
        "\n- steps[0].depends_on[0]: no step has id `base`",
        "\n- steps[0].anchors[0].text: `fn area` is not in lines 1-2 of src/main.rs",
    ] {
        assert!(e.contains(part), "{part:?} in {e}");
    }

    // A good one.
    let r = agent.ok("publish_walkthrough", walkthrough.clone()).await;
    assert!(
        text(&r).starts_with("Published walkthrough `circle-area-"),
        "{r}"
    );
    let id = r["structuredContent"]["id"].as_str().unwrap().to_owned();
    let list = ui
        .request::<wtm::List>(wtm::ListParams {
            workspace: a.id.clone(),
            session: Some(session.clone()),
        })
        .await
        .unwrap();
    assert_eq!(list.walkthroughs.len(), 1);
    assert_eq!(list.walkthroughs[0].id.0, id);
    assert_eq!(list.walkthroughs[0].session, session);

    // A bridge claiming another session with this session's token is
    // refused.
    let ticket = h
        .ctx
        .auth
        .mint_session(a.id.0.clone(), session.0.clone())
        .unwrap();
    let mut lying = Agent::start_as(h.connect(ticket).await, &a, &other.0).await;
    let e = lying.error("publish_walkthrough", walkthrough).await;
    assert!(e.contains("outside this connection's scope"), "{e}");
    let list = ui
        .request::<wtm::List>(wtm::ListParams {
            workspace: a.id.clone(),
            session: Some(other.clone()),
        })
        .await
        .unwrap();
    assert!(list.walkthroughs.is_empty());
    drop(a_dir);
}
