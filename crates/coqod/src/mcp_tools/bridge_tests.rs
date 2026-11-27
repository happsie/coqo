// SPDX-License-Identifier: Apache-2.0

//! Every `coqo-editor` tool end to end: an agent speaking MCP to the bridge
//! (`coqo_mcp::serve`), which holds a session token for workspace A and
//! calls the real router in-process. Results are right, the editor context
//! a UI publishes reaches `editor_context`, `editor_reveal` reaches a UI
//! following reveals, a bridge pointed at another workspace gets nothing,
//! and a mod's tool appears with `tools/list_changed` and is callable.

use std::sync::Arc;
use std::time::Duration;

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
        let dir = tempfile::tempdir().unwrap();
        let ctx = routes::tests::context(dir.path());
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
    write(".gitignore", "target/\n");
    write("README.md", "# Demo\n");
    write(
        "src/main.rs",
        "struct Circle { r: f64 }\n\nimpl Circle {\n    fn area(&self) -> f64 {\n        3.14 * self.r * self.r\n    }\n}\n\nfn main() {}\n",
    );
    write("src/files/tree.rs", "pub fn tree() {}\n");
    write("target/out.txt", "build output\n");
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
        let registry = ToolRegistry::new();
        coqo_mcp::tools::core(&registry);
        let options = BridgeOptions {
            workspace: workspace.id.clone(),
            session: "s-1".into(),
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
    let (_a_dir, a) = project(&ui).await;
    let token = h.ctx.auth.mint_session(a.id.0.clone(), "s-1").unwrap();
    let mut agent = Agent::start(h.connect(token).await, &a).await;

    let names = agent.names().await;
    for name in [
        "workspace_info",
        "read_file",
        "list_files",
        "find_files",
        "file_outline",
        "editor_context",
        "editor_reveal",
        PROBE_TOOL,
    ] {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }

    // workspace_info, read_file
    let info = agent.ok("workspace_info", json!({})).await;
    assert_eq!(info["structuredContent"]["id"], json!(a.id));
    let file = agent.ok("read_file", json!({ "path": "README.md" })).await;
    assert_eq!(text(&file), "# Demo\n");

    // list_files: ignored entries left out unless asked for.
    let list = agent.ok("list_files", json!({})).await;
    let listing = text(&list);
    assert!(
        listing.ends_with("\nsrc/") && listing.contains("\nREADME.md\n"),
        "{listing}"
    );
    assert!(!listing.contains("target"), "{listing}");
    let list = agent
        .ok("list_files", json!({ "depth": 3, "include_ignored": true }))
        .await;
    let paths: Vec<_> = list["structuredContent"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_owned())
        .collect();
    for path in ["src/files/tree.rs", "target/out.txt", "src/main.rs"] {
        assert!(paths.iter().any(|p| p == path), "{path} in {paths:?}");
    }
    let list = agent
        .ok("list_files", json!({ "path": "src", "limit": 1 }))
        .await;
    assert_eq!(list["structuredContent"]["truncated"], true, "{list}");

    // find_files: best matches first, absolute paths.
    let found = agent.ok("find_files", json!({ "query": "tree" })).await;
    let matches = found["structuredContent"]["matches"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        matches[0],
        json!(format!("{}/src/files/tree.rs", root(&a))),
        "{found}"
    );
    let found = agent.ok("find_files", json!({ "query": "out.txt" })).await;
    assert_eq!(found["structuredContent"]["total"], 0, "ignored: {found}");

    // file_outline: tree-sitter symbols with 1-based lines.
    let outline = agent
        .ok("file_outline", json!({ "path": "src/main.rs" }))
        .await;
    let outline_text = text(&outline);
    assert!(
        outline_text.contains("struct Circle (line 1)"),
        "{outline_text}"
    );
    assert!(
        outline_text.contains("\nimpl Circle (lines 3-7)\n  method area (lines 4-6)\n"),
        "{outline_text}"
    );
    assert!(
        outline_text.contains("function main (line 9)"),
        "{outline_text}"
    );
    assert_eq!(outline["structuredContent"]["language"], "rust");

    // editor_context: nothing until a UI window publishes.
    let cx = agent.ok("editor_context", json!({})).await;
    assert_eq!(cx["structuredContent"]["attached"], false);
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

    // Bad arguments are tool errors the model can read.
    assert!(
        agent
            .error("list_files", json!({ "depth": 0 }))
            .await
            .contains("`depth` must be at least 1")
    );
    assert!(
        agent
            .error("editor_reveal", json!({ "path": "nope.rs" }))
            .await
            .contains("nope.rs")
    );
    assert!(
        agent
            .error("file_outline", json!({ "path": "/etc/passwd" }))
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
        ("workspace_info", json!({})),
        ("read_file", json!({ "path": "README.md" })),
        ("list_files", json!({})),
        ("find_files", json!({ "query": "main" })),
        ("file_outline", json!({ "path": "src/main.rs" })),
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
