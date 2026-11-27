// SPDX-License-Identifier: Apache-2.0

//! `mcp/*` through the real router: mod tools are listed under the names
//! agents see, called with the session's workspace and session, checked
//! against their schemas, and changes are announced.

use std::sync::Arc;
use std::time::Duration;

use coqo_kernel::ProviderKind;
use coqo_mod_api::contrib::McpTool;
use coqo_mod_api::serde_json::json;
use coqo_mod_api::{BoxFuture, Manifest, McpToolOutput, Mod, ModContext, ModError};
use coqo_protocol::methods::mcp::{self as mcpm, CallParams, ToolsParams};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_protocol::{Client, ClientConfig, Server, ServerConfig, SessionToken, codes};
use futures_util::StreamExt as _;
use serde_json::{Map, Value};
use tempfile::TempDir;

use super::exposed_name;
use crate::daemon::DaemonContext;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(5);

/// A built-in test mod (in the test daemons' mod table) whose tool is
/// declared in the manifest and registered on its first call.
pub(crate) const PROBE_MANIFEST: &str = r#"
id = "coqo.probe"
name = "Probe"
version = "0.1.0"
description = "Test mod with an MCP tool."
engine = "^0.0"
tier = "core"
activation = ["mcp_tool:probe_echo"]

[[contributes.mcp_tools]]
name = "probe_echo"
description = "Echoes its arguments with the caller's workspace and session."
read_only = true

[contributes.mcp_tools.input_schema]
type = "object"
required = ["text"]
additionalProperties = false
properties.text = { type = "string" }
"#;

/// The probe tool's name.
pub(crate) const PROBE_TOOL: &str = "probe_echo";

/// The mod behind [`PROBE_MANIFEST`].
pub(crate) struct Probe;

fn probe_tool() -> Result<McpTool, ModError> {
    let manifest = Manifest::from_toml_str(PROBE_MANIFEST).map_err(ModError::failed)?;
    Ok(manifest.contributes.mcp_tools[0].clone())
}

impl Mod for Probe {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            cx.register_mcp_tool(
                probe_tool()?,
                |call: coqo_mod_api::McpToolCall| async move {
                    Ok(McpToolOutput::json(json!({
                        "workspace": call.workspace,
                        "session": call.session,
                        "text": call.arguments.get("text"),
                    })))
                },
            )
        })
    }
}

/// An installed mod that contributes its tool at activation.
pub(crate) struct Acme;

pub(crate) const ACME_MANIFEST: &str = r#"
id = "acme.build"
name = "Acme build"
version = "1.0.0"
engine = "^0.0"
tier = "core"
activation = ["startup"]
"#;

impl Mod for Acme {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            let tool =
                McpTool::new("run_tests".parse()?, "Runs the tests.").with_title("Run tests");
            cx.register_mcp_tool(tool, |_call| async {
                Err(ModError::failed("3 tests failed"))
            })
        })
    }
}

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
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.ctx.shutdown.cancel();
    }
}

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(o) => o,
        _ => Map::new(),
    }
}

#[test]
fn names_agents_see() {
    assert_eq!(exposed_name("coqo.maven", "maven_test", true), "maven_test");
    assert_eq!(
        exposed_name("acme.maven", "maven_test", false),
        "acme_maven_maven_test"
    );
    assert_eq!(exposed_name("acme", "a.b", false), "acme_a_b");
    let long = exposed_name(&"x".repeat(40), &"y".repeat(40), false);
    assert_eq!(long.len(), 64);
    assert_ne!(long, exposed_name(&"x".repeat(40), &"y".repeat(41), false));
    assert!(coqo_mcp::registry::valid_name(&long));
}

#[tokio::test]
async fn mod_tools_are_listed_called_and_announced() {
    let h = Harness::start();
    let ws = WorkspaceId("ws-a".into());
    let token = h.ctx.auth.mint_session(ws.0.clone(), "s-1").unwrap();
    let agent = h.connect(token).await;
    let params = ToolsParams {
        workspace: ws.clone(),
    };

    let list = agent.request::<mcpm::Tools>(params.clone()).await.unwrap();
    let probe = list
        .tools
        .iter()
        .find(|t| t.name == PROBE_TOOL)
        .expect("the probe tool");
    assert!(probe.read_only);
    assert_eq!(probe.provider, "coqo.probe");
    assert_eq!(probe.input_schema["required"], json!(["text"]));

    // The first call activates the mod, which registers the handler.
    let call = |name: &str, arguments: Value| CallParams {
        workspace: ws.clone(),
        session: "s-1".into(),
        name: name.into(),
        arguments: args(arguments),
    };
    let r = agent
        .request::<mcpm::Call>(call(PROBE_TOOL, json!({ "text": "hi" })))
        .await
        .unwrap();
    assert!(!r.is_error, "{r:?}");
    assert_eq!(
        r.structured,
        Some(json!({ "workspace": "ws-a", "session": "s-1", "text": "hi" }))
    );
    // Arguments are checked against the schema first.
    let r = agent
        .request::<mcpm::Call>(call(PROBE_TOOL, json!({ "text": 1, "x": 0 })))
        .await
        .unwrap();
    assert!(r.is_error);
    assert_eq!(
        r.text,
        "invalid arguments: `text` must be a string, not an integer; `x` is not a known argument"
    );
    // Unknown tools, and other sessions or workspaces.
    let err = agent
        .request::<mcpm::Call>(call("nope", json!({})))
        .await
        .unwrap_err();
    assert_eq!(
        err.rpc().map(|e| e.code),
        Some(coqo_protocol::methods::codes::UNKNOWN_TOOL)
    );
    for params in [
        CallParams {
            session: "s-2".into(),
            ..call(PROBE_TOOL, json!({ "text": "x" }))
        },
        CallParams {
            workspace: WorkspaceId("ws-b".into()),
            ..call(PROBE_TOOL, json!({ "text": "x" }))
        },
    ] {
        let err = agent.request::<mcpm::Call>(params).await.unwrap_err();
        assert_eq!(err.rpc().map(|e| e.code), Some(codes::FORBIDDEN), "{err}");
    }

    // An installed mod's tool appears, prefixed, and is announced.
    let mut changes = agent
        .subscribe::<mcpm::ToolsChanged>(params.clone(), None)
        .await
        .unwrap();
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
    let event = tokio::time::timeout(TIMEOUT, changes.next())
        .await
        .expect("a change in time")
        .unwrap()
        .unwrap();
    assert!(event.event.generation >= 1);
    let list = agent.request::<mcpm::Tools>(params.clone()).await.unwrap();
    let acme = list
        .tools
        .iter()
        .find(|t| t.provider == "acme.build")
        .expect("acme's tool");
    assert_eq!(acme.name, "acme_build_run_tests");
    assert_eq!(acme.title.as_deref(), Some("Run tests"));
    // A mod's error is a failed result the model can read.
    let r = agent
        .request::<mcpm::Call>(call("acme_build_run_tests", json!({})))
        .await
        .unwrap();
    assert!(r.is_error);
    assert!(r.text.contains("3 tests failed"), "{r:?}");

    // Disabling it takes the tool away, and says so.
    manager
        .disable(&"acme.build".parse().unwrap())
        .await
        .unwrap();
    tokio::time::timeout(TIMEOUT, changes.next())
        .await
        .expect("a change in time")
        .unwrap()
        .unwrap();
    let list = agent.request::<mcpm::Tools>(params).await.unwrap();
    assert!(list.tools.iter().all(|t| t.provider != "acme.build"));
}
