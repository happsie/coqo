// SPDX-License-Identifier: Apache-2.0

//! End to end in-process: an agent session gets the `coqo-editor` server
//! spec with a one-time token off `argv`; the bridge launched from that spec
//! reaches its own workspace through the real router and route policy, and
//! nothing else; closing the session cuts the bridge off.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coqo_agents::model::ContentBlock;
use coqo_agents::model::{CreateSessionParams, ProviderInfo};
use coqo_agents::testing::{Call, ScriptedProvider};
use coqo_agents::{
    AgentProvider, ManagerOptions, McpServer, McpServerSource, ProviderError, ProviderRegistry,
    SessionManager, SessionStore,
};
use coqo_client::{RUNTIME_DIR_ENV, SESSION_TOKEN_ENV};
use coqo_mcp::{BridgeError, BridgeOptions, Exit, ToolRegistry};
use coqo_protocol::methods::fs::{ReadFile, ReadParams};
use coqo_protocol::methods::workspace::{self as wsm, WorkspaceInfo};
use coqo_protocol::{
    Client, ClientConfig, ProtocolError, Server, ServerConfig, SessionToken, codes,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{
    AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::task::JoinHandle;

use super::{EditorMcp, revoke_on_close};
use crate::auth::TICKET_TTL;
use crate::cli::{self, Mode};
use crate::daemon::DaemonContext;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(10);
const PROGRAM: &str = "/opt/coqo/bin/coqod";

struct Fake(ScriptedProvider);

impl ProviderRegistry for Fake {
    fn providers(&self) -> Vec<ProviderInfo> {
        Vec::new()
    }

    fn provider(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        (id == "fake").then(|| Arc::new(self.0.clone()) as Arc<dyn AgentProvider>)
    }
}

/// The daemon's real router and authentication in-process, and a session
/// manager whose MCP server source records what it hands out.
struct Harness {
    _dir: TempDir,
    ctx: DaemonContext,
    server: Server,
    manager: Option<SessionManager>,
    specs: Arc<Mutex<Vec<McpServer>>>,
}

impl Harness {
    fn start() -> Self {
        Self::open(
            SessionStore::open_in_memory().unwrap(),
            ScriptedProvider::new(),
        )
    }

    fn open(store: SessionStore, provider: ScriptedProvider) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = routes::tests::context(dir.path());
        let router = routes::router(&ctx).unwrap();
        let mut config = ServerConfig::new("coqod", "test");
        config.authenticator = Some(Arc::new(ctx.auth.clone()));
        let server = Server::new(router, config);
        let specs: Arc<Mutex<Vec<McpServer>>> = Arc::default();
        let mut h = Self {
            _dir: dir,
            ctx,
            server,
            manager: None,
            specs,
        };
        h.manager = Some(h.manager_on(store, provider));
        h
    }

    /// A session manager on `store` with `provider` as `fake`, as after a
    /// daemon restart, whose MCP server source records what it hands out.
    fn manager_on(&self, store: SessionStore, provider: ScriptedProvider) -> SessionManager {
        let editor = EditorMcp {
            program: PROGRAM.into(),
            paths: self.ctx.paths.clone(),
            auth: self.ctx.auth.clone(),
            ttl: TICKET_TTL,
        };
        let recorded = Arc::clone(&self.specs);
        let source: McpServerSource = Arc::new(move |info| {
            let servers = editor.servers(info);
            recorded.lock().unwrap().extend(servers.clone());
            servers
        });
        let manager = SessionManager::open(
            store,
            Arc::new(Fake(provider)),
            ManagerOptions {
                keep_alive: None,
                mcp_servers: Some(source),
                observer: None,
            },
        )
        .unwrap();
        revoke_on_close(&manager, self.ctx.auth.clone(), self.ctx.shutdown.clone());
        manager
    }

    fn manager(&self) -> &SessionManager {
        self.manager.as_ref().unwrap()
    }

    async fn connect(&self, token: SessionToken) -> Result<Client, ProtocolError> {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(server_io).await });
        let mut config = ClientConfig::new("test", "test");
        config.token = Some(token);
        Client::connect(client_io, config).await.map(|(c, _)| c)
    }

    fn last_spec(&self) -> McpServer {
        self.specs
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a server spec")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.ctx.shutdown.cancel();
    }
}

/// A folder with one file, opened as a workspace with the daemon token.
async fn workspace(admin: &Client, file: &str) -> (TempDir, WorkspaceInfo) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(file), "text\n").unwrap();
    let info = admin
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    (dir, info)
}

fn env<'a>(spec: &'a McpServer, name: &str) -> &'a str {
    spec.env
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("{name} in {spec:?}"))
}

/// What an agent does with a server spec, minus the process: parse its
/// argv as `coqod mcp` would, connect with the token from its environment,
/// and serve MCP on a pipe.
struct Bridge {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    stdin: WriteHalf<DuplexStream>,
    next_id: u64,
    task: JoinHandle<Result<Exit, BridgeError>>,
    /// The bridge's own daemon connection.
    daemon: Client,
}

impl Bridge {
    async fn launch(h: &Harness, spec: &McpServer, token: Option<&str>) -> Self {
        let args = cli::parse(spec.args.iter().map(OsString::from)).unwrap();
        assert_eq!(args.mode, Mode::Mcp);
        let mcp = args.mcp.unwrap();
        let token = token.unwrap_or_else(|| env(spec, SESSION_TOKEN_ENV));
        let daemon = h.connect(SessionToken::new(token)).await.unwrap();
        let options = BridgeOptions {
            workspace: mcp.workspace.as_str().into(),
            session: mcp.session,
            build: "test".into(),
        };
        let registry = ToolRegistry::new();
        coqo_mcp::tools::core(&registry);
        let (agent_io, bridge_io) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(bridge_io);
        let client = daemon.clone();
        let task =
            tokio::spawn(
                async move { coqo_mcp::serve(client, &options, registry, read, write).await },
            );
        let (read, stdin) = tokio::io::split(agent_io);
        let mut bridge = Self {
            lines: BufReader::new(read).lines(),
            stdin,
            next_id: 1,
            task,
            daemon,
        };
        let init = bridge
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test-agent", "version": "0" },
                }),
            )
            .await;
        assert_eq!(
            init["result"]["serverInfo"]["name"], "coqo-editor",
            "{init}"
        );
        bridge
            .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        bridge
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        loop {
            let line = tokio::time::timeout(TIMEOUT, self.lines.next_line())
                .await
                .expect("an answer in time")
                .unwrap()
                .expect("stdout open");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    /// A tool's result: (is_error, text, structured content).
    async fn call(&mut self, name: &str, arguments: Value) -> (bool, String, Value) {
        let r = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        let result = &r["result"];
        assert!(result.is_object(), "{r}");
        (
            result["isError"] == true,
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            result["structuredContent"].clone(),
        )
    }

    async fn exit(self) -> Exit {
        tokio::time::timeout(TIMEOUT, self.task)
            .await
            .expect("the bridge exits")
            .unwrap()
            .unwrap()
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, ProtocolError>) -> i64 {
    result
        .unwrap_err()
        .rpc()
        .map(|e| e.code)
        .unwrap_or_default()
}

fn unauthorized(result: Result<Client, ProtocolError>) -> bool {
    matches!(result, Err(ProtocolError::Unauthorized(_)))
}

#[tokio::test]
async fn a_session_bridge_reaches_its_own_workspace_until_the_session_closes() {
    let h = Harness::start();
    let daemon_token = h.ctx.auth.daemon_token();
    let admin = h.connect(daemon_token.clone()).await.unwrap();
    let (a_dir, a) = workspace(&admin, "a.txt").await;
    let (b_dir, b) = workspace(&admin, "b.txt").await;

    let session = h
        .manager()
        .create(CreateSessionParams {
            provider: "fake".into(),
            cwd: a_dir.path().to_str().unwrap().to_owned(),
            workspace: Some(a.id.0.clone()),
            title: None,
            mode: None,
            from: None,
        })
        .await
        .unwrap();

    // The spec: our own binary in `mcp` mode, routing ids on argv, the
    // one-time token and runtime directory in the environment.
    let spec = h.last_spec();
    assert_eq!(spec.name, "coqo-editor");
    assert_eq!(spec.command.to_str(), Some(PROGRAM));
    assert_eq!(
        spec.args,
        ["mcp", "--workspace", &a.id.0, "--session", &session.id.0]
    );
    let token = env(&spec, SESSION_TOKEN_ENV).to_owned();
    assert_eq!(token.len(), 64);
    assert_eq!(
        env(&spec, RUNTIME_DIR_ENV),
        h.ctx.paths.runtime_dir().to_str().unwrap()
    );
    for arg in &spec.args {
        assert!(!arg.contains(&token), "token on argv: {arg}");
        assert!(
            !arg.contains(daemon_token.expose()),
            "daemon token on argv: {arg}"
        );
    }

    let mut bridge = Bridge::launch(&h, &spec, None).await;
    // Single use: the token is spent now.
    let reused = h.connect(SessionToken::new(token.clone())).await;
    assert!(unauthorized(reused));

    let list = bridge.request("tools/list", json!({})).await;
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    for name in ["editor_context", "editor_reveal", "ask_question"] {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }

    let (is_error, _, context) = bridge.call("editor_context", json!({})).await;
    assert!(!is_error, "{context}");
    let info = &context["workspace"];
    assert_eq!(info["id"], a.id.0);
    assert_eq!(info["session"], session.id.0);
    assert_eq!(info["roots"][0]["path"], a.roots[0].path);

    // No UI here, so nobody sees a reveal, but the path is the workspace's.
    let (is_error, text, _) = bridge
        .call("editor_reveal", json!({ "path": "a.txt" }))
        .await;
    assert!(!is_error, "{text}");
    let absolute = format!("{}/a.txt", a.roots[0].path);
    let (is_error, text, _) = bridge
        .call("editor_reveal", json!({ "path": absolute }))
        .await;
    assert!(!is_error, "{text}");

    // Outside the workspace: refused by the tool, the daemon's path checks,
    // and the route policy.
    let other = format!("{}/b.txt", b.roots[0].path);
    let (is_error, text, _) = bridge.call("editor_reveal", json!({ "path": other })).await;
    assert!(is_error && text.contains("outside"), "{text}");
    let (is_error, text, _) = bridge
        .call("editor_reveal", json!({ "path": "../b.txt" }))
        .await;
    assert!(is_error, "{text}");
    let read_b = bridge
        .daemon
        .request::<ReadFile>(ReadParams {
            workspace: b.id.clone(),
            root: b.roots[0].id,
            path: "b.txt".into(),
            max_bytes: None,
        })
        .await;
    assert_eq!(code(read_b), codes::FORBIDDEN);
    let status = bridge
        .daemon
        .request::<coqo_protocol::daemon::Status>(())
        .await;
    assert_eq!(code(status), codes::FORBIDDEN);

    // argv only routes: a bridge told it works in B, with a token for A,
    // still reaches only A.
    let ticket = h
        .ctx
        .auth
        .issue_ticket(a.id.0.clone(), session.id.0.clone(), TICKET_TTL)
        .unwrap();
    let mut lying = spec.clone();
    lying.args[2] = b.id.0.clone();
    let mut misrouted = Bridge::launch(&h, &lying, Some(ticket.expose())).await;
    let (is_error, text, _) = misrouted.call("editor_context", json!({})).await;
    assert!(is_error, "{text}");
    assert!(text.contains("outside this connection's scope"), "{text}");

    // Closing the session revokes its scope: unused tokens stop working and
    // both bridges are cut off and exit.
    let unused = h
        .ctx
        .auth
        .issue_ticket(a.id.0.clone(), session.id.0.clone(), TICKET_TTL)
        .unwrap();
    assert!(h.manager().close(&session.id).await.unwrap());
    assert_eq!(bridge.exit().await, Exit::DaemonGone);
    assert_eq!(misrouted.exit().await, Exit::DaemonGone);
    assert!(unauthorized(h.connect(unused).await));
    // The daemon token is not affected.
    admin.request::<wsm::List>(()).await.unwrap();
    drop((a_dir, b_dir));
}

#[tokio::test]
async fn sessions_without_a_workspace_get_no_server() {
    let h = Harness::start();
    let dir = tempfile::tempdir().unwrap();
    h.manager()
        .create(CreateSessionParams {
            provider: "fake".into(),
            cwd: dir.path().to_str().unwrap().to_owned(),
            workspace: None,
            title: None,
            mode: None,
            from: None,
        })
        .await
        .unwrap();
    assert!(h.specs.lock().unwrap().is_empty());
}

/// What the provider was handed for each start and resume.
fn editor_specs(provider: &ScriptedProvider) -> Vec<McpServer> {
    provider
        .contexts()
        .into_iter()
        .map(|cx| {
            assert_eq!(cx.mcp_servers.len(), 1, "{cx:?}");
            cx.mcp_servers.into_iter().next().unwrap()
        })
        .collect()
}

#[tokio::test]
async fn every_start_and_resume_gets_the_server_with_its_own_single_use_token() {
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("coqo.db");
    let first = ScriptedProvider::new();
    let mut h = Harness::open(SessionStore::open(&db).unwrap(), first.clone());
    let admin = h.connect(h.ctx.auth.daemon_token()).await.unwrap();
    let (dir, ws) = workspace(&admin, "a.txt").await;
    let session = h
        .manager()
        .create(CreateSessionParams {
            provider: "fake".into(),
            cwd: dir.path().to_str().unwrap().to_owned(),
            workspace: Some(ws.id.0.clone()),
            title: None,
            mode: None,
            from: None,
        })
        .await
        .unwrap();
    assert_eq!(first.calls().len(), 1, "{:?}", first.calls());

    // A daemon restart: the next prompt resumes the provider session.
    let resumed = ScriptedProvider::new();
    h.manager().shutdown().await;
    h.manager = Some(h.manager_on(SessionStore::open(&db).unwrap(), resumed.clone()));
    h.manager()
        .prompt(&session.id, vec![ContentBlock::text("hi")])
        .await
        .unwrap();
    assert_eq!(resumed.calls()[0], Call::Resume("fake-1".into()));
    // A second session, not prompted yet.
    let fresh = h
        .manager()
        .create(CreateSessionParams {
            provider: "fake".into(),
            cwd: dir.path().to_str().unwrap().to_owned(),
            workspace: Some(ws.id.0.clone()),
            title: None,
            mode: None,
            from: None,
        })
        .await
        .unwrap();

    // Another restart, and its resume fails: having no turns, it starts
    // afresh, with a token of its own, not the failed resume's.
    let failing = ScriptedProvider::new().fail_resume(ProviderError::Failed("gone".into()));
    h.manager().shutdown().await;
    h.manager = Some(h.manager_on(SessionStore::open(&db).unwrap(), failing.clone()));
    h.manager()
        .prompt(&fresh.id, vec![ContentBlock::text("again")])
        .await
        .unwrap();
    assert!(
        matches!(failing.calls()[..2], [Call::Resume(_), Call::Start { .. }]),
        "{:?}",
        failing.calls()
    );

    let specs: Vec<McpServer> = [&first, &resumed, &failing]
        .into_iter()
        .flat_map(editor_specs)
        .collect();
    assert_eq!(specs.len(), 5, "start, resume, start, failed resume, start");
    assert_eq!(specs, *h.specs.lock().unwrap());
    let mut tokens = Vec::new();
    for spec in &specs {
        assert_eq!(spec.name, "coqo-editor");
        assert_eq!(spec.command.to_str(), Some(PROGRAM));
        assert_eq!(
            spec.args[..4],
            ["mcp", "--workspace", &ws.id.0, "--session"]
        );
        assert!([&session.id.0, &fresh.id.0].contains(&&spec.args[4]));
        let token = env(spec, SESSION_TOKEN_ENV).to_owned();
        assert!(!tokens.contains(&token), "a token handed out twice");
        tokens.push(token);
    }
    // Each works exactly once.
    for token in tokens {
        h.connect(SessionToken::new(token.clone())).await.unwrap();
        assert!(unauthorized(h.connect(SessionToken::new(token)).await));
    }
    drop(dir);
}

#[test]
fn the_switch_only_turns_the_server_off() {
    for off in ["0", "false", "off", "no", "OFF", " 0 "] {
        assert!(super::switched_off(Some(off)), "{off}");
    }
    for on in [None, Some("1"), Some("true"), Some("on"), Some("")] {
        assert!(!super::switched_off(on), "{on:?}");
    }
}

#[tokio::test]
async fn the_daemon_injects_the_server_by_default() {
    if std::env::var_os(super::SWITCH_ENV).is_some() {
        eprintln!("skipped: {} is set", super::SWITCH_ENV);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ctx = routes::tests::context(dir.path());
    let source = super::source(&ctx).unwrap().expect("on by default");
    let info = coqo_agents::model::SessionInfo {
        id: coqo_agents::model::SessionId("s-1".into()),
        provider: "fake".into(),
        workspace: Some("ws-1".into()),
        cwd: "/work".into(),
        title: None,
        state: coqo_agents::model::AgentState::Idle,
        state_reason: None,
        state_since: None,
        status: None,
        criteria: Vec::new(),
        tool_use: Vec::new(),
        worktrees: Vec::new(),
        mode: None,
        closed: false,
        created_at: 0,
        updated_at: 0,
    };
    let servers = source(&info);
    assert_eq!(servers.len(), 1);
    // The running binary, absolute and resolved.
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    assert_eq!(servers[0].command, exe);
    assert!(servers[0].command.is_absolute());
    assert_eq!(super::coqod_binary().unwrap(), exe);
    ctx.shutdown.cancel();
}
