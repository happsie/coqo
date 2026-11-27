// SPDX-License-Identifier: Apache-2.0

//! The `coqo-editor` MCP server: what `coqod mcp` runs (PLAN §7.4, ADR 0012).
//!
//! An agent (Claude Code, say) launches `coqod mcp --workspace <id>
//! --session <id>` as a stdio MCP server. The bridge connects to the
//! running daemon over its socket with a **session-scoped** credential, so
//! every daemon call it makes is limited by the daemon's route policy to the
//! agent's own workspace and session (ADR 0007, *Route policy*). It then
//! serves MCP on stdin and stdout until either side goes away.
//!
//! - [`registry`]: the tool registry. A [`Tool`] has a
//!   [`ToolDefinition`] and an async `call` that gets a [`ToolContext`]
//!   (the daemon connection, the workspace and session, progress
//!   notifications). Adding or removing a tool sends
//!   `notifications/tools/list_changed`.
//! - [`tools`]: the core tools ([`tools::core`]).
//! - [`bridge`]: the MCP server over any reader and writer
//!   ([`bridge::serve`]) and the `coqod mcp` entry point
//!   ([`bridge::run_stdio`]).
//!
//! The MCP side is the official Rust SDK (`rmcp`); its types stay inside
//! this crate, so tools never depend on it.

pub mod bridge;
pub mod registry;
mod server;
pub mod tools;

pub use bridge::{BridgeError, BridgeOptions, Exit, run_stdio, serve};
pub use registry::{
    BoxFuture, RegistryError, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput,
    ToolRegistry,
};

/// The MCP server name agents see (`mcp__coqo-editor__<tool>` in Claude
/// Code).
pub const SERVER_NAME: &str = "coqo-editor";

#[cfg(test)]
mod tests;
