// SPDX-License-Identifier: Apache-2.0

//! `mcp/*`: the MCP tools mods contribute, for the `coqo-editor` bridge
//! (`coqod mcp`, PLAN §7.4, issue #70).
//!
//! Mods run in the daemon, so their tools do too. The bridge lists them with
//! [`Tools`] (`mcp/tools`), offers them to its agent next to its own core
//! tools, forwards calls with [`Call`] (`mcp/call`), and lists again when
//! [`ToolsChanged`] (`mcp/toolsChanged`) says the set changed.
//!
//! Names are what the agent sees: a built-in mod's tool keeps its name
//! (`maven_test`), an installed mod's is prefixed with its mod id, dots
//! turned into `_` (`acme_maven_maven_test`), so mods cannot shadow each
//! other or the core tools.
//!
//! **Access:** a session token may list and call tools for its own workspace
//! and agent session only.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::workspace::WorkspaceId;
use crate::method::{Method, Topic};

/// The MCP server name of Coqo's editor bridge, as agents see it
/// (`mcp__coqo-editor__<tool>` in Claude Code).
pub const EDITOR_SERVER: &str = "coqo-editor";

/// The core tools of [`EDITOR_SERVER`] (ADR 0014); its other tools come
/// from mods. `coqo-mcp` checks that this matches what it registers.
pub const CORE_TOOLS: &[&str] = &[
    "editor_context",
    "editor_reveal",
    "set_status",
    "ask_question",
    "await_answer",
    "annotate",
    "set_acceptance_criteria",
    "publish_walkthrough",
];

/// Params of [`Tools`] and the [`ToolsChanged`] topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ToolsParams {
    /// The workspace the tools are for.
    pub workspace: WorkspaceId,
}

/// A mod-contributed tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    /// The name agents see: 1 to 64 of `A-Z a-z 0-9 _ -`.
    pub name: String,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What it does, for the agent.
    pub description: String,
    /// JSON Schema of the arguments (an object schema).
    pub input_schema: Map<String, Value>,
    /// Whether it only reads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// The mod that contributes it.
    #[serde(rename = "mod")]
    pub provider: String,
}

/// Result of [`Tools`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ToolsResult {
    /// The tools, by name.
    pub tools: Vec<ToolInfo>,
}

/// Params of [`Call`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CallParams {
    /// The calling session's workspace.
    pub workspace: WorkspaceId,
    /// The calling agent session.
    pub session: String,
    /// The tool, as [`Tools`] names it.
    pub name: String,
    /// Its arguments.
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

/// Result of [`Call`]: what the agent gets back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CallResult {
    /// Text for the model: the result, or what went wrong.
    pub text: String,
    /// The result as JSON, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    /// Whether the tool failed (invalid arguments, or the mod's error). The
    /// model sees `text` and can correct itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

/// One event of [`ToolsChanged`]: the set of tools changed; list again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ToolsChangedEvent {
    /// How many times the set changed since the daemon started.
    pub generation: u64,
}

/// `mcp/tools`: the mod-contributed tools for a workspace.
#[derive(Debug)]
pub enum Tools {}
impl Method for Tools {
    const NAME: &'static str = "mcp/tools";
    type Params = ToolsParams;
    type Result = ToolsResult;
}

/// `mcp/call`: call a mod-contributed tool. A tool that no longer exists is
/// [`UNKNOWN_TOOL`](super::codes::UNKNOWN_TOOL).
#[derive(Debug)]
pub enum Call {}
impl Method for Call {
    const NAME: &'static str = "mcp/call";
    type Params = CallParams;
    type Result = CallResult;
}

/// `mcp/toolsChanged`: the mod-contributed tools changed. Live only.
#[derive(Debug)]
pub enum ToolsChanged {}
impl Topic for ToolsChanged {
    const NAME: &'static str = "mcp/toolsChanged";
    type Params = ToolsParams;
    type Event = ToolsChangedEvent;
}
