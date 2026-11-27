// SPDX-License-Identifier: Apache-2.0

//! The tools mods contribute, which run in the daemon (`mcp/*`).
//!
//! [`sync`] lists them (`mcp/tools`) into the registry as [`DaemonTool`]
//! proxies, which forward calls with `mcp/call`, and follows
//! `mcp/toolsChanged` to list again: tools that went away are removed, new
//! and changed ones (re)registered, and each change reaches the agent as
//! `notifications/tools/list_changed`. A daemon tool never replaces a tool
//! the bridge registered itself (the core tools): it is skipped with a
//! warning.

use std::collections::BTreeMap;
use std::sync::Arc;

use coqo_protocol::methods::mcp::{self as mcpm, CallParams, ToolInfo, ToolsParams};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_protocol::{Client, ProtocolError, codes};
use futures_util::StreamExt as _;
use serde_json::{Map, Value};

use crate::registry::{
    BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput, ToolRegistry,
};

/// A mod-contributed tool, called through the daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonTool {
    info: ToolInfo,
}

impl DaemonTool {
    /// A proxy for `info`.
    pub fn new(info: ToolInfo) -> Self {
        Self { info }
    }
}

impl Tool for DaemonTool {
    fn definition(&self) -> ToolDefinition {
        let mut def = ToolDefinition::new(self.info.name.clone(), self.info.description.clone())
            .with_schema(Value::Object(self.info.input_schema.clone()));
        if let Some(title) = &self.info.title {
            def = def.with_title(title.clone());
        }
        if self.info.read_only {
            def = def.read_only();
        }
        def
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let result = cx
                .daemon()
                .request::<mcpm::Call>(CallParams {
                    workspace: cx.workspace().clone(),
                    session: cx.session().to_owned(),
                    name: self.info.name.clone(),
                    arguments: args,
                })
                .await?;
            if result.is_error {
                return Err(ToolError::Failed(result.text));
            }
            Ok(ToolOutput {
                text: result.text,
                structured: result.structured,
            })
        })
    }
}

/// The daemon tools currently in a registry, by name.
#[derive(Debug, Default)]
pub struct Synced {
    tools: BTreeMap<String, ToolInfo>,
}

impl Synced {
    /// Bring `registry` in line with `tools`, in one change.
    pub fn apply(&mut self, registry: &ToolRegistry, tools: Vec<ToolInfo>) {
        let wanted: BTreeMap<String, ToolInfo> =
            tools.into_iter().map(|t| (t.name.clone(), t)).collect();
        let remove: Vec<String> = self
            .tools
            .iter()
            .filter(|(name, info)| wanted.get(*name) != Some(info))
            .map(|(name, _)| name.clone())
            .collect();
        let mut add: Vec<Arc<dyn Tool>> = Vec::new();
        let mut kept = BTreeMap::new();
        for (name, info) in wanted {
            if self.tools.get(&name) == Some(&info) {
                kept.insert(name, info);
                continue;
            }
            if !self.tools.contains_key(&name) && registry.get(&name).is_some() {
                tracing::warn!(tool = %name, provider = %info.provider, "a mod tool has a core tool's name; skipped");
                continue;
            }
            add.push(Arc::new(DaemonTool::new(info.clone())));
            kept.insert(name, info);
        }
        for error in registry.update(&remove, add) {
            tracing::warn!(error = %error, "cannot register a mod tool");
            let (crate::RegistryError::InvalidName(name) | crate::RegistryError::Duplicate(name)) =
                &error;
            kept.remove(name);
        }
        self.tools = kept;
    }

    /// The names it registered.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
}

/// Whether the daemon has no `mcp/*` (an older build, or a test daemon).
fn unsupported(e: &ProtocolError) -> bool {
    e.rpc().is_some_and(|e| e.code == codes::METHOD_NOT_FOUND)
}

/// List the daemon's tools into `registry` once.
pub async fn load(
    daemon: &Client,
    workspace: &WorkspaceId,
    registry: &ToolRegistry,
    synced: &mut Synced,
) -> Result<(), ProtocolError> {
    let list = daemon
        .request::<mcpm::Tools>(ToolsParams {
            workspace: workspace.clone(),
        })
        .await?;
    synced.apply(registry, list.tools);
    Ok(())
}

/// Keep `registry` in line with the daemon's tools until the connection
/// closes. Lists once before it returns the follow-up future, so the
/// agent's first `tools/list` already has them.
pub async fn sync(
    daemon: Client,
    workspace: WorkspaceId,
    registry: ToolRegistry,
) -> BoxFuture<'static, ()> {
    let done: BoxFuture<'static, ()> = Box::pin(std::future::ready(()));
    // Subscribe first, so no change between the list and the
    // subscription is missed.
    let changes = match daemon
        .subscribe::<mcpm::ToolsChanged>(
            ToolsParams {
                workspace: workspace.clone(),
            },
            None,
        )
        .await
    {
        Ok(changes) => changes,
        Err(e) if unsupported(&e) => {
            tracing::debug!("the daemon has no mod tools");
            return done;
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot follow the mod tools");
            return done;
        }
    };
    let mut synced = Synced::default();
    if let Err(e) = load(&daemon, &workspace, &registry, &mut synced).await {
        tracing::warn!(error = %e, "cannot list the mod tools");
    }
    Box::pin(async move {
        let mut changes = changes;
        while let Some(event) = changes.next().await {
            if let Err(e) = event {
                tracing::debug!(error = %e, "mod tool changes ended");
                break;
            }
            if let Err(e) = load(&daemon, &workspace, &registry, &mut synced).await {
                tracing::warn!(error = %e, "cannot list the mod tools");
            }
        }
    })
}
