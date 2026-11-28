// SPDX-License-Identifier: Apache-2.0

//! Translation between ACP v1 messages and Coqo's provider-neutral agent
//! model (`coqo_agents::model`). This is the adapter layer that keeps the
//! fast-moving `agent-client-protocol` crate out of the rest of Coqo.

use agent_client_protocol::v1::schema as acp;
use coqo_agents::model as m;
use coqo_agents::{AgentUpdate, McpServer, PermissionAsk};
use serde_json::{Map, Value};

// ---- Coqo → ACP ------------------------------------------------------------------

/// A prompt block for `session/prompt`.
pub(crate) fn to_acp_content(block: m::ContentBlock) -> acp::ContentBlock {
    match block {
        m::ContentBlock::Text { text } => acp::ContentBlock::Text(acp::TextContent::new(text)),
        m::ContentBlock::Image {
            mime_type,
            data,
            uri,
        } => acp::ContentBlock::Image(acp::ImageContent::new(data, mime_type).uri(uri)),
        m::ContentBlock::ResourceLink {
            uri,
            name,
            mime_type,
            size,
        } => acp::ContentBlock::ResourceLink(
            acp::ResourceLink::new(name, uri)
                .mime_type(mime_type)
                .size(size.and_then(|s| i64::try_from(s).ok())),
        ),
        m::ContentBlock::Resource {
            uri,
            mime_type,
            text,
        } => acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(
                acp::TextResourceContents::new(text, uri).mime_type(mime_type),
            ),
        )),
    }
}

/// An MCP server for `session/new.mcpServers`.
pub(crate) fn to_acp_mcp(server: &McpServer) -> acp::McpServer {
    acp::McpServer::Stdio(
        acp::McpServerStdio::new(server.name.clone(), server.command.clone())
            .args(server.args.clone())
            .env(
                server
                    .env
                    .iter()
                    .map(|(k, v)| acp::EnvVariable::new(k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            ),
    )
}

// ---- ACP → Coqo ------------------------------------------------------------------

pub(crate) fn content(block: &acp::ContentBlock) -> m::ContentBlock {
    match block {
        acp::ContentBlock::Text(t) => m::ContentBlock::text(t.text.clone()),
        acp::ContentBlock::Image(i) => m::ContentBlock::Image {
            mime_type: i.mime_type.clone(),
            data: i.data.clone(),
            uri: i.uri.clone(),
        },
        acp::ContentBlock::ResourceLink(l) => m::ContentBlock::ResourceLink {
            uri: l.uri.clone(),
            name: l.name.clone(),
            mime_type: l.mime_type.clone(),
            size: l.size.and_then(|s| u64::try_from(s).ok()),
        },
        acp::ContentBlock::Resource(r) => match &r.resource {
            acp::EmbeddedResourceResource::TextResourceContents(t) => m::ContentBlock::Resource {
                uri: t.uri.clone(),
                mime_type: t.mime_type.clone(),
                text: t.text.clone(),
            },
            acp::EmbeddedResourceResource::BlobResourceContents(b) => {
                m::ContentBlock::ResourceLink {
                    uri: b.uri.clone(),
                    name: b.uri.clone(),
                    mime_type: b.mime_type.clone(),
                    size: None,
                }
            }
            _ => m::ContentBlock::text("[unsupported resource]"),
        },
        acp::ContentBlock::Audio(_) => m::ContentBlock::text("[audio]"),
        _ => m::ContentBlock::text("[unsupported content]"),
    }
}

fn tool_kind(kind: &acp::ToolKind) -> m::ToolKind {
    match kind {
        acp::ToolKind::Read => m::ToolKind::Read,
        acp::ToolKind::Edit => m::ToolKind::Edit,
        acp::ToolKind::Delete => m::ToolKind::Delete,
        acp::ToolKind::Move => m::ToolKind::Move,
        acp::ToolKind::Search => m::ToolKind::Search,
        acp::ToolKind::Execute => m::ToolKind::Execute,
        acp::ToolKind::Think => m::ToolKind::Think,
        acp::ToolKind::Fetch => m::ToolKind::Fetch,
        acp::ToolKind::SwitchMode => m::ToolKind::SwitchMode,
        _ => m::ToolKind::Other,
    }
}

fn tool_status(status: &acp::ToolCallStatus) -> m::ToolCallStatus {
    match status {
        acp::ToolCallStatus::InProgress => m::ToolCallStatus::InProgress,
        acp::ToolCallStatus::Completed => m::ToolCallStatus::Completed,
        acp::ToolCallStatus::Failed => m::ToolCallStatus::Failed,
        _ => m::ToolCallStatus::Pending,
    }
}

fn tool_content(items: &[acp::ToolCallContent]) -> Vec<m::ToolCallContent> {
    items
        .iter()
        .filter_map(|c| match c {
            acp::ToolCallContent::Content(c) => Some(m::ToolCallContent::Content {
                content: content(&c.content),
            }),
            acp::ToolCallContent::Diff(d) => Some(m::ToolCallContent::Diff {
                path: d.path.display().to_string(),
                old_text: d.old_text.clone(),
                new_text: d.new_text.clone(),
            }),
            acp::ToolCallContent::Terminal(t) => Some(m::ToolCallContent::Terminal {
                terminal_id: t.terminal_id.0.to_string(),
            }),
            _ => None,
        })
        .collect()
}

fn locations(items: &[acp::ToolCallLocation]) -> Vec<m::ToolCallLocation> {
    items
        .iter()
        .map(|l| m::ToolCallLocation {
            path: l.path.display().to_string(),
            line: l.line,
        })
        .collect()
}

/// The tool's name from a tool call's `_meta`: a `toolName` string at the
/// top or one level down (the Claude adapter's `_meta.claudeCode.toolName`).
fn tool_name(meta: Option<&Map<String, Value>>) -> Option<String> {
    let meta = meta?;
    let name =
        |m: &Map<String, Value>| m.get("toolName").and_then(Value::as_str).map(str::to_owned);
    name(meta)
        .or_else(|| meta.values().filter_map(Value::as_object).find_map(name))
        .filter(|n| !n.is_empty())
}

pub(crate) fn tool_call(tc: &acp::ToolCall) -> m::ToolCall {
    m::ToolCall {
        id: m::ToolCallId(tc.tool_call_id.0.to_string()),
        title: tc.title.clone(),
        name: tool_name(tc.meta.as_ref()),
        kind: tool_kind(&tc.kind),
        status: tool_status(&tc.status),
        content: tool_content(&tc.content),
        locations: locations(&tc.locations),
        raw_input: tc.raw_input.clone(),
        raw_output: tc.raw_output.clone(),
    }
}

pub(crate) fn tool_call_update(u: &acp::ToolCallUpdate) -> m::ToolCallUpdate {
    let f = &u.fields;
    m::ToolCallUpdate {
        id: m::ToolCallId(u.tool_call_id.0.to_string()),
        title: f.title.clone(),
        kind: f.kind.as_ref().map(tool_kind),
        status: f.status.as_ref().map(tool_status),
        content: f.content.as_deref().map(tool_content),
        locations: f.locations.as_deref().map(locations),
        raw_input: f.raw_input.clone(),
        raw_output: f.raw_output.clone(),
    }
}

/// File edits a tool call announces: `(path, old_text, new_text)`.
pub(crate) fn diffs(
    items: &[acp::ToolCallContent],
) -> Vec<(std::path::PathBuf, Option<String>, String)> {
    items
        .iter()
        .filter_map(|c| match c {
            acp::ToolCallContent::Diff(d) => {
                Some((d.path.clone(), d.old_text.clone(), d.new_text.clone()))
            }
            _ => None,
        })
        .collect()
}

fn plan_entries(entries: &[acp::PlanEntry]) -> Vec<m::PlanEntry> {
    entries
        .iter()
        .map(|e| m::PlanEntry {
            content: e.content.clone(),
            priority: match e.priority {
                acp::PlanEntryPriority::High => m::PlanPriority::High,
                acp::PlanEntryPriority::Low => m::PlanPriority::Low,
                _ => m::PlanPriority::Medium,
            },
            status: match e.status {
                acp::PlanEntryStatus::InProgress => m::PlanEntryStatus::InProgress,
                acp::PlanEntryStatus::Completed => m::PlanEntryStatus::Completed,
                _ => m::PlanEntryStatus::Pending,
            },
        })
        .collect()
}

pub(crate) fn modes(state: &acp::SessionModeState) -> m::SessionModes {
    m::SessionModes {
        current_mode_id: state.current_mode_id.0.to_string(),
        available_modes: state
            .available_modes
            .iter()
            .map(|mode| m::SessionMode {
                id: mode.id.0.to_string(),
                name: mode.name.clone(),
                description: mode.description.clone(),
            })
            .collect(),
    }
}

/// ACP session config options, in the agent's order. Kinds Coqo does not
/// know are left out; grouped select entries are flattened, each with its
/// group's name.
pub(crate) fn config_options(options: &[acp::SessionConfigOption]) -> Vec<m::SessionConfigOption> {
    options.iter().filter_map(config_option).collect()
}

fn config_option(option: &acp::SessionConfigOption) -> Option<m::SessionConfigOption> {
    let entry = |o: &acp::SessionConfigSelectOption, group: Option<&str>| m::ConfigOptionValue {
        value: o.value.0.to_string(),
        name: o.name.clone(),
        description: o.description.clone(),
        group: group.map(str::to_owned),
    };
    let kind = match &option.kind {
        acp::SessionConfigKind::Select(select) => m::ConfigOptionKind::Select {
            current_value: select.current_value.0.to_string(),
            options: match &select.options {
                acp::SessionConfigSelectOptions::Ungrouped(list) => {
                    list.iter().map(|o| entry(o, None)).collect()
                }
                acp::SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|g| g.options.iter().map(|o| entry(o, Some(&g.name))))
                    .collect(),
                _ => return None,
            },
        },
        acp::SessionConfigKind::Boolean(b) => m::ConfigOptionKind::Boolean {
            current_value: b.current_value,
        },
        _ => return None,
    };
    // The category's wire name: one of ACP's, or the agent's own.
    let category = option
        .category
        .as_ref()
        .and_then(|c| serde_json::to_value(c).ok())
        .and_then(|v| v.as_str().map(str::to_owned));
    Some(m::SessionConfigOption {
        id: option.id.0.to_string(),
        name: option.name.clone(),
        description: option.description.clone(),
        category,
        kind,
    })
}

/// A value for `session/set_config_option`.
pub(crate) fn to_acp_config_value(value: &m::ConfigValue) -> acp::SessionConfigOptionValue {
    match value {
        m::ConfigValue::Boolean(b) => acp::SessionConfigOptionValue::boolean(*b),
        m::ConfigValue::Value(v) => acp::SessionConfigOptionValue::value_id(v.clone()),
    }
}

fn commands(items: &[acp::AvailableCommand]) -> Vec<m::AvailableCommand> {
    items
        .iter()
        .map(|c| m::AvailableCommand {
            name: c.name.clone(),
            description: c.description.clone(),
            input_hint: match &c.input {
                Some(acp::AvailableCommandInput::Unstructured(u)) => Some(u.hint.clone()),
                _ => None,
            },
        })
        .collect()
}

fn usage(u: &acp::UsageUpdate) -> m::Usage {
    let meta = u.meta.clone().map(|meta| redact(Value::Object(meta)));
    let model = meta.as_ref().and_then(|meta| {
        meta.as_object()?.iter().find_map(|(k, v)| {
            (k == "model" || k.ends_with("/model"))
                .then(|| v.as_str().map(str::to_owned))
                .flatten()
        })
    });
    m::Usage {
        used_tokens: Some(u.used),
        context_size: Some(u.size),
        cost: u.cost.as_ref().map(|c| m::Cost {
            amount: c.amount,
            currency: c.currency.clone(),
        }),
        model,
        meta,
    }
}

/// One ACP `session/update`, as a Coqo update. `None` for updates Coqo has
/// no place for yet (notices, compaction, …).
pub(crate) fn update(u: &acp::SessionUpdate) -> Option<AgentUpdate> {
    Some(match u {
        acp::SessionUpdate::UserMessageChunk(c) => {
            AgentUpdate::UserMessageChunk(content(&c.content))
        }
        acp::SessionUpdate::AgentMessageChunk(c) => {
            AgentUpdate::AgentMessageChunk(content(&c.content))
        }
        acp::SessionUpdate::AgentThoughtChunk(c) => {
            AgentUpdate::AgentThoughtChunk(content(&c.content))
        }
        acp::SessionUpdate::ToolCall(tc) => AgentUpdate::ToolCall(tool_call(tc)),
        acp::SessionUpdate::ToolCallUpdate(tu) => AgentUpdate::ToolCallUpdate(tool_call_update(tu)),
        acp::SessionUpdate::Plan(p) => AgentUpdate::Plan(plan_entries(&p.entries)),
        acp::SessionUpdate::PlanUpdate(p) => match &p.plan {
            acp::PlanUpdateContent::Items(items) => AgentUpdate::Plan(plan_entries(&items.entries)),
            acp::PlanUpdateContent::Markdown(md) => AgentUpdate::Plan(vec![m::PlanEntry {
                content: md.content.clone(),
                priority: m::PlanPriority::Medium,
                status: m::PlanEntryStatus::Pending,
            }]),
            _ => return None,
        },
        acp::SessionUpdate::PlanRemoved(_) => AgentUpdate::Plan(Vec::new()),
        acp::SessionUpdate::AvailableCommandsUpdate(a) => {
            AgentUpdate::AvailableCommands(commands(&a.available_commands))
        }
        acp::SessionUpdate::CurrentModeUpdate(c) => {
            AgentUpdate::CurrentMode(c.current_mode_id.0.to_string())
        }
        acp::SessionUpdate::UsageUpdate(u) => AgentUpdate::Usage(usage(u)),
        acp::SessionUpdate::ConfigOptionUpdate(c) => {
            AgentUpdate::ConfigOptions(config_options(&c.config_options))
        }
        // The agent's own title (Claude Code titles a session from its first
        // prompt). A cleared title keeps the one the session has.
        acp::SessionUpdate::SessionInfoUpdate(i) => match i.title.value() {
            Some(title) if !title.trim().is_empty() => AgentUpdate::Title(title.clone()),
            _ => return None,
        },
        _ => return None,
    })
}

fn permission_kind(kind: &acp::PermissionOptionKind) -> Option<m::PermissionOptionKind> {
    Some(match kind {
        acp::PermissionOptionKind::AllowOnce => m::PermissionOptionKind::AllowOnce,
        acp::PermissionOptionKind::AllowAlways => m::PermissionOptionKind::AllowAlways,
        acp::PermissionOptionKind::RejectOnce => m::PermissionOptionKind::RejectOnce,
        acp::PermissionOptionKind::RejectAlways => m::PermissionOptionKind::RejectAlways,
        _ => return None,
    })
}

/// A `session/request_permission`, for the session manager. Options are
/// kept with their ids and kinds (never chosen by position); options of a
/// kind Coqo does not know are left out.
pub(crate) fn permission(req: &acp::RequestPermissionRequest) -> PermissionAsk {
    let tool_call = tool_call_update(&req.tool_call);
    PermissionAsk {
        title: tool_call
            .title
            .clone()
            .unwrap_or_else(|| "The agent asks for permission".into()),
        tool_call: Some(tool_call),
        options: req
            .options
            .iter()
            .filter_map(|o| {
                Some(m::PermissionOption {
                    id: o.option_id.0.to_string(),
                    name: o.name.clone(),
                    kind: permission_kind(&o.kind)?,
                })
            })
            .collect(),
    }
}

/// The ACP answer to a permission request. An option id the agent did not
/// offer becomes a cancellation.
pub(crate) fn permission_outcome(
    req: &acp::RequestPermissionRequest,
    outcome: m::PermissionOutcome,
) -> acp::RequestPermissionOutcome {
    match outcome {
        m::PermissionOutcome::Selected { option_id }
            if req.options.iter().any(|o| *o.option_id.0 == *option_id) =>
        {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(option_id),
            ))
        }
        _ => acp::RequestPermissionOutcome::Cancelled,
    }
}

pub(crate) fn stop_reason(r: &acp::StopReason) -> m::StopReason {
    match r {
        acp::StopReason::EndTurn => m::StopReason::EndTurn,
        acp::StopReason::MaxTokens => m::StopReason::MaxTokens,
        acp::StopReason::MaxTurnRequests => m::StopReason::MaxTurnRequests,
        acp::StopReason::Refusal => m::StopReason::Refusal,
        acp::StopReason::Cancelled => m::StopReason::Cancelled,
        _ => m::StopReason::EndTurn,
    }
}

// ---- auth status -----------------------------------------------------------------

/// Keys that carry identity. Coqo never stores or shows them (ADR 0005).
const PERSONAL: &[&str] = &[
    "email",
    "emailAddress",
    "organization",
    "organizationName",
    "organizationUuid",
    "orgId",
    "accountUuid",
    "userId",
];

/// `value` without identity fields, at any depth.
pub(crate) fn redact(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            map.retain(|k, _| !PERSONAL.contains(&k.as_str()));
            for v in map.values_mut() {
                *v = redact(v.take());
            }
        }
        Value::Array(items) => {
            for v in items {
                *v = redact(v.take());
            }
        }
        _ => {}
    }
    value
}

fn auth_kind(kind: &str) -> m::AuthKind {
    match kind {
        "account" | "subscription" | "claudeai" | "claude.ai" => m::AuthKind::Account,
        "api_key" | "apiKey" => m::AuthKind::ApiKey,
        "gateway" => m::AuthKind::Gateway,
        "external" | "bedrock" | "vertex" | "foundry" => m::AuthKind::External,
        "none" => m::AuthKind::None,
        _ => m::AuthKind::Unknown,
    }
}

/// A label fit to show: never something that looks like an email.
fn label(label: Option<&str>) -> Option<String> {
    label
        .filter(|l| !l.is_empty() && !l.contains('@'))
        .map(str::to_owned)
}

/// The first object with a string `kind` inside an account's `_meta` (the
/// Claude adapter uses `_meta.claudeCode.{kind, plan}`).
fn meta_kind(meta: Option<&Map<String, Value>>) -> Option<&Map<String, Value>> {
    let meta = meta?;
    if meta.get("kind").and_then(Value::as_str).is_some() {
        return Some(meta);
    }
    meta.values()
        .filter_map(Value::as_object)
        .find(|o| o.get("kind").and_then(Value::as_str).is_some())
}

/// The billing source from the RFD notification `auth/status_update`
/// (`unstable_auth_status`): kind, label and plan only.
pub(crate) fn auth_status(n: &acp::AuthStatusUpdateNotification) -> m::AuthStatus {
    let account = n
        .accounts
        .iter()
        .find(|a| a.active == Some(true))
        .or_else(|| n.accounts.first());
    let meta = account.and_then(|a| meta_kind(a.meta.as_ref()));
    let kind = match (&n.state, meta) {
        (_, Some(meta)) => auth_kind(meta.get("kind").and_then(Value::as_str).unwrap_or("")),
        (acp::AuthState::Unauthenticated, None) => m::AuthKind::None,
        _ => m::AuthKind::Unknown,
    };
    m::AuthStatus {
        kind,
        label: label(account.map(|a| a.label.as_str())),
        plan: meta
            .and_then(|meta| meta.get("plan"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// The billing source from the Claude adapter's legacy extension
/// notification `_auth/status_update`: `{authStatus: {kind, label,
/// account: {plan}}}`.
pub(crate) fn legacy_auth_status(params: &Value) -> Option<m::AuthStatus> {
    let status = params.get("authStatus").unwrap_or(params);
    let kind = status.get("kind")?.as_str()?;
    Some(m::AuthStatus {
        kind: auth_kind(kind),
        label: label(status.get("label").and_then(Value::as_str)),
        plan: status
            .get("account")
            .and_then(|a| a.get("plan"))
            .or_else(|| status.get("plan"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_options_as_the_claude_adapter_sends_them() {
        // Shaped like claude-agent-acp 0.113: a grouped model list, effort,
        // Fast mode with the unstable `speed` category, and an option kind
        // Coqo does not know (left out).
        let update: acp::ConfigOptionUpdate = serde_json::from_value(json!({"configOptions": [
            {"id": "model", "name": "Model", "category": "model", "type": "select",
             "currentValue": "default", "options": [
                {"group": "latest", "name": "Latest", "options": [
                    {"value": "default", "name": "Default (recommended)", "description": "Opus"},
                    {"value": "sonnet", "name": "Sonnet"}]}]},
            {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
             "currentValue": "high", "options": [
                {"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
            {"id": "fast_mode", "name": "Fast mode", "category": "speed", "type": "boolean",
             "currentValue": false},
            {"id": "note", "name": "Note", "type": "text", "currentValue": "x"},
        ]}))
        .unwrap();
        let got = config_options(&update.config_options);
        let ids: Vec<_> = got.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["model", "effort", "fast_mode"]);
        assert_eq!(got[0].category.as_deref(), Some("model"));
        assert_eq!(got[0].value_name(), "Default (recommended)");
        let m::ConfigOptionKind::Select { options, .. } = &got[0].kind else {
            panic!("a select");
        };
        assert_eq!(options[1].group.as_deref(), Some("Latest"));
        assert_eq!(options[0].description.as_deref(), Some("Opus"));
        assert_eq!(got[1].category.as_deref(), Some("thought_level"));
        assert_eq!(got[2].category.as_deref(), Some("speed"));
        assert_eq!(
            got[2].kind,
            m::ConfigOptionKind::Boolean {
                current_value: false
            }
        );

        let wire = |v| {
            serde_json::to_value(acp::SetSessionConfigOptionRequest::new(
                "s",
                "x",
                to_acp_config_value(&v),
            ))
            .unwrap()
        };
        assert_eq!(
            wire(m::ConfigValue::Boolean(true)),
            json!({"sessionId": "s", "configId": "x", "type": "boolean", "value": true})
        );
        assert_eq!(
            wire(m::ConfigValue::Value("high".into())),
            json!({"sessionId": "s", "configId": "x", "value": "high"})
        );
    }

    #[test]
    fn tool_calls_carry_the_adapters_tool_name() {
        let call = |meta: Value| -> acp::ToolCall {
            serde_json::from_value(json!({
                "toolCallId": "t1", "title": "editor_context", "_meta": meta,
                // The adapter's own extra field, which ACP drops.
                "name": "ignored",
            }))
            .unwrap()
        };
        let named = call(json!({"claudeCode": {"toolName": "mcp__coqo-editor__editor_context"}}));
        assert_eq!(
            tool_call(&named).name.as_deref(),
            Some("mcp__coqo-editor__editor_context")
        );
        assert_eq!(
            tool_call(&call(json!({"toolName": "Read"})))
                .name
                .as_deref(),
            Some("Read")
        );
        assert_eq!(tool_call(&call(json!({"other": 1}))).name, None);
        assert_eq!(tool_call(&call(Value::Null)).name, None);
    }

    #[test]
    fn auth_status_keeps_kind_and_plan_only() {
        let n: acp::AuthStatusUpdateNotification = serde_json::from_value(json!({
            "state": "authenticated",
            "accounts": [{
                "label": "Claude Max",
                "_meta": {"claudeCode": {
                    "kind": "account", "plan": "max",
                    "email": "someone@example.com", "organization": "Acme"
                }}
            }]
        }))
        .unwrap();
        let status = auth_status(&n);
        assert_eq!(
            status,
            m::AuthStatus {
                kind: m::AuthKind::Account,
                label: Some("Claude Max".into()),
                plan: Some("max".into()),
            }
        );
        let text = serde_json::to_string(&status).unwrap();
        assert!(
            !text.contains("example.com") && !text.contains("Acme"),
            "{text}"
        );

        let signed_out: acp::AuthStatusUpdateNotification =
            serde_json::from_value(json!({"state": "unauthenticated"})).unwrap();
        assert_eq!(auth_status(&signed_out).kind, m::AuthKind::None);
    }

    #[test]
    fn legacy_auth_status_and_labels_that_look_like_emails() {
        let status = legacy_auth_status(&json!({"authStatus": {
            "kind": "api_key", "label": "someone@example.com",
            "account": {"plan": null, "email": "someone@example.com"}
        }}))
        .unwrap();
        assert_eq!(
            status,
            m::AuthStatus {
                kind: m::AuthKind::ApiKey,
                label: None,
                plan: None
            }
        );
        assert!(legacy_auth_status(&json!({"other": 1})).is_none());
    }

    #[test]
    fn redact_drops_identity_at_any_depth() {
        let v = redact(json!({"a": [{"email": "x@y", "keep": 1}], "organization": "o"}));
        assert_eq!(v, json!({"a": [{"keep": 1}]}));
    }

    #[test]
    fn usage_reads_the_model_from_meta() {
        let u: acp::UsageUpdate = serde_json::from_value(json!({
            "used": 1200, "size": 200000,
            "cost": {"amount": 0.12, "currency": "USD"},
            "_meta": {"_claude/model": "claude-opus", "_claude/rateLimit": {"five_hour": 3}}
        }))
        .unwrap();
        let usage = usage(&u);
        assert_eq!(usage.used_tokens, Some(1200));
        assert_eq!(usage.context_size, Some(200000));
        assert_eq!(usage.model.as_deref(), Some("claude-opus"));
        assert_eq!(usage.cost.unwrap().currency, "USD");
    }

    #[test]
    fn permission_outcomes_only_select_offered_options() {
        let req: acp::RequestPermissionRequest = serde_json::from_value(json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "t1", "title": "Write a.txt"},
            "options": [
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                {"optionId": "allow-once", "name": "Allow", "kind": "allow_once"}
            ]
        }))
        .unwrap();
        let ask = permission(&req);
        assert_eq!(ask.title, "Write a.txt");
        assert_eq!(ask.options[1].kind, m::PermissionOptionKind::AllowOnce);
        let picked = permission_outcome(
            &req,
            m::PermissionOutcome::Selected {
                option_id: "allow-once".into(),
            },
        );
        assert!(
            matches!(picked, acp::RequestPermissionOutcome::Selected(s) if &*s.option_id.0 == "allow-once")
        );
        let bogus = permission_outcome(
            &req,
            m::PermissionOutcome::Selected {
                option_id: "nope".into(),
            },
        );
        assert!(matches!(bogus, acp::RequestPermissionOutcome::Cancelled));
    }

    #[test]
    fn the_agents_title_comes_from_session_info_updates() {
        // As claude-agent-acp 0.113 sends the title Claude Code generated.
        let title = |v| {
            let u: acp::SessionUpdate = serde_json::from_value(v).unwrap();
            update(&u)
        };
        assert_eq!(
            title(
                json!({"sessionUpdate": "session_info_update", "title": "Fix the flaky IT",
                          "updatedAt": "2026-11-28T10:00:00Z"})
            ),
            Some(AgentUpdate::Title("Fix the flaky IT".into()))
        );
        assert_eq!(
            title(json!({"sessionUpdate": "session_info_update", "title": null})),
            None
        );
        assert_eq!(
            title(
                json!({"sessionUpdate": "session_info_update", "updatedAt": "2026-11-28T10:00:00Z"})
            ),
            None
        );
        assert_eq!(
            title(json!({"sessionUpdate": "session_info_update", "title": "  "})),
            None
        );
    }
}
