// SPDX-License-Identifier: Apache-2.0

//! `mcp/*`: the MCP tools mods contribute (point `mcp_tools`, PLAN §7.4,
//! issue #70), for the `coqo-editor` bridge (`coqod mcp`).
//!
//! Mods run here, so their tools do too. `mcp/tools` lists them under the
//! names agents see, `mcp/call` checks a call's arguments against the
//! tool's schema and runs its handler (activating the mod first if it waits
//! for `mcp_tool:<name>`), and `mcp/toolsChanged` tells bridges when the set
//! changed, so they refresh and send `notifications/tools/list_changed`.
//!
//! **Names.** A built-in mod's tool keeps its own name (`maven_test`, as
//! PLAN §7.4 names them). An installed mod's is prefixed with its mod id,
//! dots as `_` (`acme.maven` → `acme_maven_maven_test`), so it cannot
//! shadow a built-in or another mod's tool. Names stay within MCP's 64
//! characters: a longer one is cut and ends in a hash.
//!
//! **Access:** a session token lists tools for its own workspace and calls
//! them for its own workspace and agent session; the handler is told both.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use coqo_kernel::ProviderKind;
use coqo_kernel::id::{ContributionId, ProviderId};
use coqo_mod_api::points;
use coqo_mod_api::{McpToolCall, contrib};
use coqo_protocol::methods::codes::UNKNOWN_TOOL;
use coqo_protocol::methods::mcp::{
    self as mcpm, CallParams, CallResult, ToolInfo, ToolsChangedEvent, ToolsParams, ToolsResult,
};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, SeqLog};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::mods::Mods;

/// Longest tool name MCP allows.
const MAX_NAME: usize = 64;
/// Change events kept for slow subscribers.
const CHANGE_LOG: usize = 16;

/// A tool as the daemon resolves it.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    info: ToolInfo,
    /// Its id in the `mcp_tools` point, for the handler.
    id: ContributionId,
}

/// The daemon's mod-tool service.
#[derive(Debug)]
pub struct McpToolService {
    mods: Mods,
    changes: Arc<SeqLog<ToolsChangedEvent>>,
    /// The list the last change event described.
    last: Mutex<Vec<ToolInfo>>,
}

/// The name agents see for `provider`'s tool `tool`.
pub fn exposed_name(provider: &str, tool: &str, builtin: bool) -> String {
    let raw = if builtin {
        tool.to_owned()
    } else {
        format!("{provider}_{tool}")
    };
    let name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.len() <= MAX_NAME {
        return name;
    }
    // FNV-1a of the full name keeps cut names apart.
    let hash = raw.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{}_{:08x}", &name[..MAX_NAME - 9], hash as u32)
}

fn object_schema(schema: &Value) -> Map<String, Value> {
    match schema {
        Value::Object(o) => o.clone(),
        _ => Map::new(),
    }
}

impl McpToolService {
    /// The tools of `mods`. Publishes a change whenever the set changes,
    /// until `shutdown`.
    pub fn new(mods: Mods, shutdown: CancellationToken) -> Arc<Self> {
        let svc = Arc::new(Self {
            mods,
            changes: Arc::new(SeqLog::new(CHANGE_LOG)),
            last: Mutex::default(),
        });
        *svc.last.lock().unwrap_or_else(PoisonError::into_inner) = svc.infos();
        let watcher = Arc::clone(&svc);
        let mut generation = svc.mods.contributions.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    changed = generation.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        watcher.check_changed();
                    }
                }
            }
        });
        svc
    }

    /// Every tool, by exposed name. A name two mods would share goes to the
    /// first contribution.
    fn entries(&self) -> BTreeMap<String, Entry> {
        let mut entries = BTreeMap::new();
        let tools = match self
            .mods
            .contributions
            .contributions_of::<points::McpTools>()
        {
            Ok(tools) => tools,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list MCP tools");
                return entries;
            }
        };
        for c in tools {
            let builtin = self
                .mods
                .manager
                .status_of(&c.provider)
                .is_some_and(|s| s.kind == ProviderKind::Builtin);
            let tool: &contrib::McpTool = &c.value;
            let name = exposed_name(c.provider.as_str(), tool.name.as_str(), builtin);
            if entries.contains_key(&name) {
                tracing::warn!(tool = %name, provider = %c.provider, "duplicate MCP tool name; skipped");
                continue;
            }
            let info = ToolInfo {
                name: name.clone(),
                title: tool.title.clone(),
                description: tool.description.clone(),
                input_schema: object_schema(&tool.input_schema),
                read_only: tool.read_only,
                provider: c.provider.to_string(),
            };
            entries.insert(name, Entry { info, id: c.id });
        }
        entries
    }

    fn infos(&self) -> Vec<ToolInfo> {
        self.entries().into_values().map(|e| e.info).collect()
    }

    fn check_changed(&self) {
        let now = self.infos();
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if *last != now {
            *last = now;
            let generation = self.changes.head() + 1;
            self.changes.publish(ToolsChangedEvent { generation });
        }
    }

    /// `mcp/tools`.
    pub fn tools(&self, _p: &ToolsParams) -> ToolsResult {
        ToolsResult {
            tools: self.infos(),
        }
    }

    /// `mcp/call`.
    pub async fn call(&self, p: CallParams) -> Result<CallResult, ErrorObject> {
        let Some(entry) = self.entries().remove(&p.name) else {
            return Err(ErrorObject::new(
                UNKNOWN_TOOL,
                format!("no MCP tool named `{}`", p.name),
            ));
        };
        if let Err(e) = coqo_mcp::schema::validate_arguments(&entry.info.input_schema, &p.arguments)
        {
            return Ok(CallResult {
                text: format!("invalid arguments: {e}"),
                structured: None,
                is_error: true,
            });
        }
        let call = McpToolCall {
            workspace: p.workspace.0,
            session: p.session,
            arguments: p.arguments,
        };
        let provider: ProviderId = entry.info.provider.parse().map_err(|e| {
            ErrorObject::internal(format!("bad provider id `{}`: {e}", entry.info.provider))
        })?;
        tracing::debug!(tool = %p.name, provider = %provider, "mcp/call");
        Ok(
            match self.mods.manager.call_mcp_tool(&entry.id, call).await {
                Ok(output) => CallResult {
                    text: output.text,
                    structured: output.structured,
                    is_error: false,
                },
                Err(e) => CallResult {
                    text: e.to_string(),
                    structured: None,
                    is_error: true,
                },
            },
        )
    }

    /// `mcp/toolsChanged`.
    pub fn follow(&self) -> Result<EventFeed<ToolsChangedEvent>, ErrorObject> {
        self.changes.subscribe(None)
    }
}

/// Register `mcp/*` on `router`.
pub fn register(router: Router, svc: Arc<McpToolService>) -> Router {
    let list = Arc::clone(&svc);
    let call = Arc::clone(&svc);
    let follow = svc;
    let own_session = Access::check(|scope, p: &CallParams| {
        scope.require_workspace(&p.workspace.0)?;
        scope.require_session(&p.session)
    });
    router
        .method_with::<mcpm::Tools, _, _>(Access::workspace(|p| &p.workspace.0), move |_ctx, p| {
            std::future::ready(Ok(list.tools(&p)))
        })
        .method_with::<mcpm::Call, _, _>(own_session, move |_ctx, p| {
            let s = Arc::clone(&call);
            async move { s.call(p).await }
        })
        .topic_with::<mcpm::ToolsChanged, _, _>(
            Access::workspace(|p| &p.workspace.0),
            move |_ctx, _p, _since| std::future::ready(follow.follow()),
        )
}

#[cfg(test)]
mod bridge_tests;
#[cfg(test)]
pub(crate) mod tests;
