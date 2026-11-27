// SPDX-License-Identifier: Apache-2.0

//! The ACP host: runs any [Agent Client Protocol](https://agentclientprotocol.com)
//! agent as a Coqo agent provider (PLAN §7.3, ADR 0005).
//!
//! [`AcpProvider`] implements [`coqo_agents::AgentProvider`] over an agent
//! command ([`AgentCommand`], built per process by a [`Launcher`]):
//!
//! - **Process model:** one agent process per workspace, with every session
//!   of that workspace on one connection. The process runs under the kernel
//!   supervisor (its own process group, stdio as a protocol channel,
//!   restarts with backoff) and stops when its last session closes.
//! - **Lifecycle:** `initialize` advertises `fs` and `plan` (never
//!   `auth.terminal`: Coqo runs no login flow); `session/new` with the
//!   session's `cwd` and MCP servers; `session/prompt` streams
//!   `session/update`s into the session manager; `session/cancel`,
//!   `session/set_mode` and `session/close`. Sessions reattach with
//!   `session/resume` (no replay) or, failing that, `session/load` (full
//!   replay), after a daemon restart or an agent crash.
//! - **Permissions** go to the user through the session manager; the
//!   answer is the option id the user picked, never a position.
//! - **Files:** `fs/read_text_file` and `fs/write_text_file` go through
//!   [`AgentFs`] (the daemon's open buffers) or, inside the workspace
//!   roots only, the disk. Every `diff` in a tool call is announced with
//!   [`AgentFs::expect_write`] before the agent writes, so the write shows
//!   up in open buffers as the agent's change (Claude Code writes files
//!   itself, ADR 0005).
//! - **Auth:** `auth_required` becomes
//!   [`ProviderError::AuthRequired`](coqo_agents::ProviderError::AuthRequired);
//!   `auth/status_update` and the Claude adapter's `_auth/status_update`
//!   become the session's billing source, kind and plan only (emails and
//!   organizations are dropped).
//!
//! `agent-client-protocol` is pinned exactly and used only in this crate,
//! so its churn stays here.
//!
//! [`testing`] has a scripted fake ACP agent (binary `coqo-fake-acp-agent`).

mod convert;
mod fs;
mod host;
mod provider;
pub mod testing;

pub use fs::{AgentFs, FsHandle};
pub use provider::{AcpOptions, AcpProvider, AgentCommand, Launcher};
