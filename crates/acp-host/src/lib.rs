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
//! - **Lifecycle:** `initialize` advertises `fs`, `plan`, boolean config
//!   options (`session.configOptions.boolean`), form elicitation
//!   (`elicitation.form`, which turns on Claude's `AskUserQuestion`; no
//!   `url` mode) and, with a terminal backend, `terminal` (never
//!   `auth.terminal`: Coqo runs no login flow);
//!   `session/new` with the session's `cwd` and MCP servers;
//!   `session/prompt` streams `session/update`s into the session manager;
//!   `session/cancel`, `session/set_mode`, `session/set_config_option` and
//!   `session/close`. Sessions reattach with `session/resume` (no replay)
//!   or, failing that, `session/load` (full replay), after a daemon restart
//!   or an agent crash.
//! - **Config options** (model, effort, Fast mode, …): `configOptions`
//!   and `config_option_update` become the session's config options. A
//!   mode change goes through the agent's `mode` config option when it has
//!   one, else `session/set_mode`. After a reattach every value the agent
//!   lost is set again with `session/set_config_option`, in order.
//! - **Permissions** go to the user through the session manager; the
//!   answer is the option id the user picked, never a position.
//! - **Files:** `fs/read_text_file` and `fs/write_text_file` go through
//!   [`AgentFs`] (the daemon's open buffers) or, inside the workspace
//!   roots only, the disk. Every `diff` in a tool call is announced with
//!   [`AgentFs::expect_write`] before the agent writes, so the write shows
//!   up in open buffers as the agent's change (Claude Code writes files
//!   itself, ADR 0005). A successful `fs/write_text_file` is reported with
//!   [`AgentFs::wrote`], for the daemon's record of who changed a file.
//! - **Terminals:** `terminal/*` run through [`AgentTerminals`] (the
//!   daemon's PTYs, so the user can open, watch and kill them as terminal
//!   tabs), in the session's workspace only, with ACP's output byte limit.
//!   Tool-call `terminal` content is mapped to the daemon terminal id.
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
mod elicit;
mod fs;
mod host;
mod provider;
mod terminal;
pub mod testing;

pub use fs::{AgentFs, FsHandle};
pub use provider::{AcpOptions, AcpProvider, AgentCommand, Launcher};
pub use terminal::{
    AgentTerminal, AgentTerminals, TerminalExit, TerminalHandle, TerminalRequest, TerminalText,
};
