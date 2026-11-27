// SPDX-License-Identifier: Apache-2.0

//! The MCP side: an `rmcp` server handler over a [`ToolRegistry`].

use std::sync::Arc;

use coqo_protocol::Client;
use coqo_protocol::methods::workspace::WorkspaceId;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ServerCapabilities,
    ServerConfig, Tool as McpTool, ToolAnnotations,
};
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};

use crate::SERVER_NAME;
use crate::registry::{ToolContext, ToolDefinition, ToolError, ToolRegistry};

const INSTRUCTIONS: &str = "Coqo editor tools. They act on the Coqo workspace this agent session \
     belongs to, through the Coqo daemon, and see what the editor sees.";

/// Serves the registry's tools to one agent.
#[derive(Clone)]
pub(crate) struct EditorServer {
    registry: ToolRegistry,
    daemon: Client,
    workspace: WorkspaceId,
    session: Arc<str>,
}

impl EditorServer {
    pub(crate) fn new(
        registry: ToolRegistry,
        daemon: Client,
        workspace: WorkspaceId,
        session: &str,
    ) -> Self {
        Self {
            registry,
            daemon,
            workspace,
            session: session.into(),
        }
    }
}

fn to_mcp(def: ToolDefinition) -> McpTool {
    let mut tool = McpTool::new(def.name, def.description, Arc::new(def.input_schema));
    if let Some(title) = def.title {
        tool = tool.with_title(title);
    }
    if def.read_only {
        tool = tool.with_annotations(ToolAnnotations::new().read_only(true));
    }
    tool
}

impl ServerHandler for EditorServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .registry
            .definitions()
            .into_iter()
            .map(to_mcp)
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        let Some(tool) = self.registry.get(name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        };
        let mut cx = ToolContext::new(
            self.daemon.clone(),
            self.workspace.clone(),
            self.session.as_ref(),
        );
        if let Some(token) = context.meta.get_progress_token() {
            let peer = context.peer.clone();
            cx = cx.with_progress(Arc::new(move |progress, total, message| {
                let mut param = ProgressNotificationParam::new(token.clone(), progress);
                if let Some(total) = total {
                    param = param.with_total(total);
                }
                if let Some(message) = message {
                    param = param.with_message(message);
                }
                let peer = peer.clone();
                Box::pin(async move {
                    if let Err(e) = peer.notify_progress(param).await {
                        tracing::debug!(error = %e, "cannot send progress");
                    }
                })
            }));
        }
        let args = request.arguments.unwrap_or_default();
        let definition = tool.definition();
        let call = async {
            crate::schema::validate_arguments(&definition.input_schema, &args)
                .map_err(ToolError::InvalidArguments)?;
            tool.call(&cx, args).await
        };
        let result = tokio::select! {
            result = call => result,
            () = context.ct.cancelled() => {
                return Err(ErrorData::internal_error("cancelled", None));
            }
        };
        let result = match result {
            Ok(output) => {
                let mut result = CallToolResult::success(vec![ContentBlock::text(output.text)]);
                result.structured_content = output.structured;
                result
            }
            Err(e) => {
                tracing::debug!(tool = name, error = %e, "tool call failed");
                CallToolResult::error(vec![ContentBlock::text(e.to_string())])
            }
        };
        Ok(result.into())
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        // Tell the agent whenever the registry changes from here on.
        let mut changes = self.registry.subscribe();
        changes.mark_unchanged();
        let peer = context.peer;
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                if peer.notify_tool_list_changed().await.is_err() {
                    break;
                }
            }
        });
    }
}
