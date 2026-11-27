// SPDX-License-Identifier: Apache-2.0

//! Coqo agents (`coqo.agents@1`, PLAN §7.3): one abstraction for every agent
//! runtime, and sessions that live in the daemon.
//!
//! - [`AgentProvider`] / [`ProviderSession`]: what an agent runtime
//!   implements (ACP host #54, Claude #56, terminal mode #57, native loop
//!   #55). Shaped by ACP (ADR 0005), provider-neutral.
//! - [`AgentProviderImpls`] and [`register_provider`]: how a provider mod
//!   registers; [`ProviderRegistry`]: how the manager finds providers.
//! - [`SessionManager`]: daemon-owned sessions. Turns run in the daemon
//!   whether or not a UI is attached; every update is persisted
//!   ([`SessionStore`]) and streamed; the agent state machine
//!   (`idle · working · needs_input · needs_permission · blocked · error ·
//!   done`) lives here, with the agent's own status line on top
//!   ([`SessionManager::set_status`], the `set_status` MCP tool), and the
//!   task's acceptance criteria ([`SessionManager::set_criteria`], the
//!   `set_acceptance_criteria` MCP tool).
//! - `rules`: per-workspace permission rules that answer a permission
//!   request before it reaches the UI (an allowlist: default is ask).
//! - [`rpc::register`]: the protocol routes (`agents/providers`,
//!   `sessions/*`, topics `sessions/updates` and `sessions/changes`). The
//!   wire types are in [`coqo_protocol::methods::agents`].
//! - [`testing::ScriptedProvider`]: a scripted fake provider.
//!
//! # A provider mod
//!
//! ```
//! use std::sync::Arc;
//! use coqo_agents::{register_provider, testing::ScriptedProvider};
//! use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
//! use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError};
//!
//! struct FakeAgentMod;
//!
//! impl Mod for FakeAgentMod {
//!     fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
//!         Box::pin(async move {
//!             let info = AgentProviderInfo {
//!                 id: "fake".parse()?,
//!                 name: "Fake agent".into(),
//!                 kind: AgentProviderKind::Native,
//!                 description: None,
//!             };
//!             register_provider(&cx, info, Arc::new(ScriptedProvider::new()))
//!         })
//!     }
//! }
//!
//! # use coqo_agents::ProviderRegistry;
//! # use coqo_mod_api::testing::{TestHost, block_on};
//! let host = TestHost::new();
//! block_on(FakeAgentMod.activate(host.context("fake-agent".parse()?)))?;
//! assert!(host.provider("fake").is_some());
//! assert!(host.providers()[0].available);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod criteria;
mod error;
mod feed;
mod fold;
mod manager;
mod provider;
mod registry;
pub mod rpc;
mod rules;
mod state;
mod store;
pub mod testing;

pub use error::AgentError;
pub use manager::{ManagerOptions, McpServerSource, SessionManager};
pub use provider::{
    AgentProvider, AgentUpdate, KeepAlive, McpServer, PermissionAsk, ProviderError,
    ProviderSession, ResumeRequest, SessionContext, SessionEvents,
};
pub use registry::{
    AgentProviderImpls, ProviderRegistry, StaticProviders, provider_info, register_provider,
};
pub use store::{SessionStore, StoreError, StoredSession};

/// The wire types, re-exported.
pub use coqo_protocol::methods::agents as model;
