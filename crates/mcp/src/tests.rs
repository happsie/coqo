// SPDX-License-Identifier: Apache-2.0

//! MCP conformance over in-process pipes: what an MCP client (Claude Code,
//! the MCP inspector) sends, as raw JSON-RPC lines, against the bridge and a
//! fake daemon. The real daemon and its route policy are tested in `coqod`.

use std::time::Duration;

use coqo_protocol::methods::editor::{self as edm, EditorContext, RevealResult};
use coqo_protocol::methods::workspace::{
    self as wsm, RootId, WorkspaceId, WorkspaceInfo, WorkspaceRoot,
};
use coqo_protocol::{Client, ClientConfig, ErrorObject, Router, Server, ServerConfig, codes};
use serde_json::{Map, Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, ReadHalf, WriteHalf,
};
use tokio::task::JoinHandle;

use crate::registry::{
    BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput, ToolRegistry,
};
use crate::{BridgeError, BridgeOptions, Exit, serve, tools};

const TIMEOUT: Duration = Duration::from_secs(10);

fn ws_info() -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId("ws-1".into()),
        name: "demo".into(),
        roots: vec![WorkspaceRoot {
            id: RootId(0),
            path: "/work/demo".into(),
            name: "demo".into(),
        }],
        created_at: 0,
        last_opened_at: 0,
        open: true,
    }
}

/// A daemon with `workspace/get`, `editor/getContext` (no window) and
/// `editor/reveal` for one workspace.
async fn fake_daemon() -> (Client, tokio::sync::oneshot::Sender<()>) {
    let router = Router::new()
        .method::<wsm::Get, _, _>(|_cx, p| async move {
            if p.id.0 == "ws-1" {
                Ok(ws_info())
            } else {
                Err(ErrorObject::new(codes::FORBIDDEN, "other workspace"))
            }
        })
        .method::<edm::GetContext, _, _>(|_cx, p| async move {
            Ok(EditorContext {
                workspace: p.workspace,
                windows: Vec::new(),
            })
        })
        .method::<edm::Reveal, _, _>(|_cx, p| async move {
            match (p.workspace.0.as_str(), p.root, p.path.as_str()) {
                ("ws-1", RootId(0), "src/a.txt") => Ok(RevealResult { delivered: false }),
                _ => Err(ErrorObject::new(codes::FORBIDDEN, "outside")),
            }
        });
    let server = Server::new(router, ServerConfig::new("fake-coqod", "test"));
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::select! {
            _ = server.serve(server_io) => {}
            _ = stop_rx => {}
        }
    });
    let (client, _) = Client::connect(client_io, ClientConfig::new("test", "test"))
        .await
        .unwrap();
    (client, stop_tx)
}

/// The agent's end of the bridge's stdio.
struct Agent {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: Option<WriteHalf<DuplexStream>>,
    /// Notifications received while waiting for responses.
    notifications: Vec<Value>,
    next_id: u64,
    bridge: JoinHandle<Result<Exit, BridgeError>>,
}

impl Agent {
    async fn start(registry: ToolRegistry) -> (Self, tokio::sync::oneshot::Sender<()>) {
        let (daemon, stop) = fake_daemon().await;
        let (agent_io, bridge_io) = tokio::io::duplex(1 << 16);
        let (bridge_read, bridge_write) = tokio::io::split(bridge_io);
        let options = BridgeOptions {
            workspace: WorkspaceId("ws-1".into()),
            session: "s-1".into(),
            build: "test".into(),
        };
        let bridge = tokio::spawn(async move {
            serve(daemon, &options, registry, bridge_read, bridge_write).await
        });
        let (read, write) = tokio::io::split(agent_io);
        let agent = Self {
            lines: BufReader::new(read).lines(),
            writer: Some(write),
            notifications: Vec::new(),
            next_id: 1,
            bridge,
        };
        (agent, stop)
    }

    async fn send(&mut self, message: Value) {
        let writer = self.writer.as_mut().expect("stdin open");
        let mut line = message.to_string();
        line.push('\n');
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(TIMEOUT, self.lines.next_line())
            .await
            .expect("a message in time")
            .unwrap()
            .expect("stdout open");
        serde_json::from_str(&line).unwrap()
    }

    /// Send a request and wait for its response, keeping notifications.
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
            assert!(message.get("method").is_some(), "unexpected: {message}");
            self.notifications.push(message);
        }
    }

    async fn initialize(&mut self) -> Value {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test-agent", "version": "0.0.1" },
                }),
            )
            .await;
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        response
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
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

    async fn close_stdin(&mut self) -> Result<Exit, BridgeError> {
        // EOF on the bridge's stdin (dropping one half of a split duplex
        // does not close it).
        if let Some(mut writer) = self.writer.take() {
            writer.shutdown().await.unwrap();
        }
        tokio::time::timeout(TIMEOUT, &mut self.bridge)
            .await
            .expect("the bridge exits")
            .unwrap()
    }
}

/// The core tools, in the order `tools/list` gives them.
const CORE_TOOLS: [&str; 7] = [
    "annotate",
    "ask_question",
    "await_answer",
    "editor_context",
    "editor_reveal",
    "set_acceptance_criteria",
    "set_status",
];

#[test]
fn the_protocols_core_tool_list_matches_the_registry() {
    // Tool-use stats (`coqo-agents`) group calls by this list.
    let mut listed: Vec<&str> = coqo_protocol::methods::mcp::CORE_TOOLS.to_vec();
    listed.sort_unstable();
    assert_eq!(listed, CORE_TOOLS);
    assert_eq!(
        crate::SERVER_NAME,
        coqo_protocol::methods::mcp::EDITOR_SERVER
    );
}

fn core_registry() -> ToolRegistry {
    let registry = ToolRegistry::new();
    tools::core(&registry);
    registry
}

fn text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

#[tokio::test]
async fn initialize_list_and_call() {
    let (mut agent, _stop) = Agent::start(core_registry()).await;

    let init = agent.initialize().await;
    let result = &init["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18", "{init}");
    assert_eq!(result["serverInfo"]["name"], "coqo-editor");
    assert_eq!(
        result["capabilities"]["tools"]["listChanged"], true,
        "{init}"
    );

    let instructions = result["instructions"].as_str().unwrap();
    assert_eq!(instructions, crate::server::INSTRUCTIONS);
    for tool in ["editor_context", "ask_question", "set_status"] {
        assert!(instructions.contains(tool), "{tool} in {instructions}");
    }

    // Only the editor-only tools (ADR 0014): nothing the agent's own file
    // tools already do.
    let list = agent.request("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, CORE_TOOLS);
    for tool in tools {
        let description = tool["description"].as_str().unwrap();
        assert!(
            description.contains("Use it") || description.contains("Call it"),
            "{} says when to use it: {description}",
            tool["name"]
        );
    }
    let context = tools
        .iter()
        .find(|t| t["name"] == "editor_context")
        .unwrap();
    assert_eq!(context["inputSchema"]["type"], "object");
    assert_eq!(context["annotations"]["readOnlyHint"], true);
    let reveal = tools.iter().find(|t| t["name"] == "editor_reveal").unwrap();
    assert_eq!(reveal["inputSchema"]["required"], json!(["path"]));
    assert!(
        reveal["inputSchema"]["properties"]["path"]["description"]
            .as_str()
            .unwrap()
            .contains("relative")
    );

    // editor_context without a window still gives the roots and session.
    let context = agent.call("editor_context", json!({})).await;
    assert_eq!(context["result"]["isError"], false, "{context}");
    let structured = &context["result"]["structuredContent"];
    assert_eq!(structured["attached"], false);
    assert_eq!(structured["workspace"]["id"], "ws-1");
    assert_eq!(structured["workspace"]["name"], "demo");
    assert_eq!(structured["workspace"]["session"], "s-1");
    assert_eq!(
        structured["workspace"]["roots"],
        json!([{ "id": 0, "name": "demo", "path": "/work/demo" }])
    );
    let summary = text(&context);
    assert!(
        summary.starts_with("Workspace: demo (session s-1)\nRoot: /work/demo\nNo Coqo window"),
        "{summary}"
    );

    // Relative to the first root, or absolute inside it.
    for path in ["src/a.txt", "/work/demo/src/a.txt"] {
        let r = agent.call("editor_reveal", json!({ "path": path })).await;
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert_eq!(
            r["result"]["structuredContent"]["path"],
            "/work/demo/src/a.txt"
        );
    }

    // Tool failures are results the model can read, not protocol errors.
    for (args, expect) in [
        (json!({}), "`path`"),
        (json!({ "path": "/etc/passwd" }), "outside the workspace"),
        (json!({ "path": "../b/x" }), "outside"),
        (
            json!({ "path": "x", "line": 0 }),
            "`line` must be at least 1",
        ),
        (
            json!({ "path": "x", "root": "zero" }),
            "`root` must be an integer",
        ),
    ] {
        let r = agent.call("editor_reveal", args.clone()).await;
        assert_eq!(r["result"]["isError"], true, "{args}: {r}");
        assert!(text(&r).contains(expect), "{args}: {r}");
    }

    // An unknown tool is a protocol error.
    let r = agent.call("nope", json!({})).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");

    assert_eq!(agent.close_stdin().await.unwrap(), Exit::StdinClosed);
}

struct Slow;

impl Tool for Slow {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("slow", "Reports progress, then finishes.")
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        _args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            cx.progress(1.0, Some(2.0), Some("half way".into())).await;
            Ok(ToolOutput::text(format!("done in {}", cx.workspace())))
        })
    }
}

#[tokio::test]
async fn registry_changes_and_progress_reach_the_agent() {
    let registry = core_registry();
    let (mut agent, _stop) = Agent::start(registry.clone()).await;
    agent.initialize().await;
    // Wait for the server to have processed `initialized`.
    agent.request("ping", json!({})).await;

    registry.register(Slow).unwrap();
    agent.notification("notifications/tools/list_changed").await;
    let list = agent.request("tools/list", json!({})).await;
    assert_eq!(
        list["result"]["tools"].as_array().unwrap().len(),
        CORE_TOOLS.len() + 1
    );

    let r = agent
        .request(
            "tools/call",
            json!({ "name": "slow", "arguments": {}, "_meta": { "progressToken": "p-1" } }),
        )
        .await;
    assert_eq!(text(&r), "done in ws-1", "{r}");
    let progress = agent.notification("notifications/progress").await;
    assert_eq!(progress["params"]["progressToken"], "p-1");
    assert_eq!(progress["params"]["progress"], 1.0);
    assert_eq!(progress["params"]["total"], 2.0);
    assert_eq!(progress["params"]["message"], "half way");

    assert!(registry.remove("slow"));
    agent.notification("notifications/tools/list_changed").await;
    assert_eq!(agent.close_stdin().await.unwrap(), Exit::StdinClosed);
}

#[tokio::test]
async fn the_bridge_exits_when_the_daemon_goes_away() {
    let (mut agent, stop) = Agent::start(core_registry()).await;
    agent.initialize().await;
    drop(stop);
    let exit = tokio::time::timeout(TIMEOUT, &mut agent.bridge)
        .await
        .expect("the bridge exits")
        .unwrap();
    assert_eq!(exit.unwrap(), Exit::DaemonGone);
}

#[tokio::test]
async fn stdin_closed_before_initialize() {
    let (mut agent, _stop) = Agent::start(core_registry()).await;
    assert_eq!(agent.close_stdin().await.unwrap(), Exit::StdinClosed);
}

#[test]
fn registry_rejects_bad_and_duplicate_names() {
    struct Named(&'static str);
    impl Tool for Named {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(self.0, "")
        }
        fn call<'a>(
            &'a self,
            _cx: &'a ToolContext,
            _args: Map<String, Value>,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async { Ok(ToolOutput::text("")) })
        }
    }
    let registry = ToolRegistry::new();
    let changes = registry.subscribe();
    registry.register(Named("ok_name-1")).unwrap();
    assert_eq!(
        registry.register(Named("ok_name-1")),
        Err(crate::RegistryError::Duplicate("ok_name-1".into()))
    );
    for bad in ["", "has space", "dots.no", "slash/no", &"x".repeat(65)] {
        let bad: &'static str = Box::leak(bad.to_owned().into_boxed_str());
        assert!(
            matches!(
                registry.register(Named(bad)),
                Err(crate::RegistryError::InvalidName(_))
            ),
            "{bad}"
        );
    }
    assert_eq!(*changes.borrow(), 1);
    assert!(!registry.remove("missing"));
    assert!(registry.remove("ok_name-1"));
    assert_eq!(*changes.borrow(), 2);
}
