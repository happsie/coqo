// SPDX-License-Identifier: Apache-2.0

//! Tool-use stats (#216): how often a session's agent called each tool,
//! grouped into Coqo's core `coqo-editor` tools, mod tools, other MCP
//! servers' tools and the agent's own tools. Counted from live tool calls
//! (one per tool call id, replays excluded) into
//! [`SessionInfo::tool_use`](coqo_protocol::methods::agents::SessionInfo),
//! which is saved with the session.

use coqo_protocol::methods::agents::{ToolCall, ToolGroup, ToolKind, ToolUse};
use coqo_protocol::methods::mcp::{CORE_TOOLS, EDITOR_SERVER};

/// The name and group a tool call counts under.
///
/// - `mcp__coqo-editor__<tool>`: `<tool>`, [`ToolGroup::Coqo`] for a
///   [`CORE_TOOLS`] name, else [`ToolGroup::Mod`];
/// - another `mcp__<server>__<tool>`: the whole name, [`ToolGroup::Mcp`];
/// - any other name: the name, [`ToolGroup::Agent`];
/// - no name: an MCP-shaped title as above, else the kind (`edit`),
///   [`ToolGroup::Agent`].
pub(crate) fn classify(call: &ToolCall) -> (String, ToolGroup) {
    let name = call
        .name
        .as_deref()
        .or_else(|| Some(call.title.as_str()).filter(|t| t.starts_with("mcp__")));
    let Some(name) = name else {
        return (kind_name(call.kind).to_owned(), ToolGroup::Agent);
    };
    match name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) if server == EDITOR_SERVER => {
            let group = if CORE_TOOLS.contains(&tool) {
                ToolGroup::Coqo
            } else {
                ToolGroup::Mod
            };
            (tool.to_owned(), group)
        }
        Some(_) => (name.to_owned(), ToolGroup::Mcp),
        None => (name.to_owned(), ToolGroup::Agent),
    }
}

fn kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        ToolKind::Other => "other",
    }
}

/// Counts one call of `name` ([`classify`]) in `stats`: one more for it,
/// or a new entry at the end.
pub(crate) fn count(stats: &mut Vec<ToolUse>, name: String, group: ToolGroup) {
    match stats
        .iter_mut()
        .find(|u| u.name == name && u.group == group)
    {
        Some(u) => u.count += 1,
        None => stats.push(ToolUse {
            name,
            group,
            count: 1,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_protocol::methods::agents::{ToolCallId, ToolCallStatus};

    fn call(name: Option<&str>, title: &str, kind: ToolKind) -> ToolCall {
        ToolCall {
            id: ToolCallId("t".into()),
            title: title.into(),
            name: name.map(str::to_owned),
            kind,
            status: ToolCallStatus::Pending,
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: None,
        }
    }

    #[test]
    fn classifies_core_mod_mcp_and_agent_tools() {
        let named = |n: &str| classify(&call(Some(n), "title", ToolKind::Other));
        assert_eq!(
            named("mcp__coqo-editor__ask_question"),
            ("ask_question".into(), ToolGroup::Coqo)
        );
        for core in CORE_TOOLS {
            assert_eq!(
                named(&format!("mcp__coqo-editor__{core}")).1,
                ToolGroup::Coqo
            );
        }
        assert_eq!(
            named("mcp__coqo-editor__maven_test"),
            ("maven_test".into(), ToolGroup::Mod)
        );
        assert_eq!(
            named("mcp__github__get_issue"),
            ("mcp__github__get_issue".into(), ToolGroup::Mcp)
        );
        assert_eq!(named("Read"), ("Read".into(), ToolGroup::Agent));
        // No name: an MCP-shaped title, else the kind.
        assert_eq!(
            classify(&call(None, "mcp__coqo-editor__set_status", ToolKind::Other)),
            ("set_status".into(), ToolGroup::Coqo)
        );
        assert_eq!(
            classify(&call(None, "Write a.txt", ToolKind::Edit)),
            ("edit".into(), ToolGroup::Agent)
        );
    }

    #[test]
    fn counts_in_order_of_first_use() {
        let mut stats = Vec::new();
        for n in [
            "mcp__coqo-editor__set_status",
            "Read",
            "mcp__coqo-editor__set_status",
            "mcp__coqo-editor__editor_context",
            "Read",
            "mcp__coqo-editor__set_status",
        ] {
            let (name, group) = classify(&call(Some(n), "", ToolKind::Other));
            count(&mut stats, name, group);
        }
        let got: Vec<_> = stats
            .iter()
            .map(|u| (u.name.as_str(), u.group, u.count))
            .collect();
        assert_eq!(
            got,
            [
                ("set_status", ToolGroup::Coqo, 3),
                ("Read", ToolGroup::Agent, 2),
                ("editor_context", ToolGroup::Coqo, 1),
            ]
        );
    }
}
