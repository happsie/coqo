// SPDX-License-Identifier: Apache-2.0

//! The [`AgentProvider`] API: what every agent runtime implements.
//!
//! The shape follows the Agent Client Protocol (ADR 0005), because ACP is how
//! most agents run, but nothing here is ACP-specific, so a terminal agent and
//! Coqo's native loop fit too:
//!
//! | ACP | here |
//! |---|---|
//! | `session/new` (`cwd`, `mcpServers`) | [`AgentProvider::start_session`] with a [`SessionContext`] |
//! | `session/resume`, `session/load` | [`AgentProvider::resume_session`] |
//! | `session/prompt` → `StopReason` | [`ProviderSession::prompt`] |
//! | `session/cancel` | [`ProviderSession::cancel`] |
//! | `session/set_mode` | [`ProviderSession::set_mode`] |
//! | `session/set_config_option` | [`ProviderSession::set_config_option`] |
//! | `session/update` notifications | [`SessionEvents::update`] with an [`AgentUpdate`] |
//! | `session_info_update` (title) | [`AgentUpdate::Title`] |
//! | `session/request_permission` | [`SessionEvents::request_permission`] |
//! | `auth/status` | [`AgentUpdate::AuthStatus`] |
//!
//! The session manager owns the session: it persists the transcript, runs
//! the state machine, answers permission requests from whichever UI is
//! attached, and keeps the daemon alive while a turn runs. A provider only
//! translates between its runtime and these calls.

use std::any::Any;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use coqo_mod_api::BoxFuture;
use coqo_protocol::methods::agents::{
    AgentQuestion, AgentState, AuthStatus, AvailableCommand, ConfigValue, ContentBlock,
    PermissionOption, PermissionOutcome, PlanEntry, ProviderCapabilities, ProviderStatus,
    SessionConfigOption, SessionId, SessionModes, SessionUpdate, StopReason, ToolCall,
    ToolCallUpdate, Usage,
};

/// An MCP server to inject into a session (ACP `session/new.mcpServers`),
/// such as the `coqo-editor` server (`coqod mcp`, PLAN §7.4).
///
/// The Claude adapter passes these on the CLI's argv, which other local users
/// can read through `/proc` (ADR 0005), so `env` must never hold a lasting
/// secret. The `coqo-editor` server carries a one-time token there, spent
/// when the server connects (ADR 0012).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    /// Server name, as the agent shows it.
    pub name: String,
    /// Absolute path of the executable.
    pub command: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Environment variables. **No lasting secrets** (see above).
    pub env: Vec<(String, String)>,
}

/// Where and how a session runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionContext {
    /// Coqo's id for the session (not the provider's).
    pub session: SessionId,
    /// The workspace it belongs to.
    pub workspace: Option<String>,
    /// The agent's working directory: the workspace root.
    pub cwd: PathBuf,
    /// MCP servers to inject.
    pub mcp_servers: Vec<McpServer>,
}

/// Reattach to a session the provider ran before (after a daemon restart).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeRequest {
    /// Where and how.
    pub context: SessionContext,
    /// The provider's own session id, from
    /// [`ProviderSession::provider_session_id`].
    pub provider_session: String,
    /// The session's config options as they were, with the values the user
    /// chose (empty if it had none). An agent may forget some on resume
    /// (Claude's adapter keeps the model but not the effort): the provider
    /// sets again each value the resumed session reports differently, if
    /// the option still takes it, and reports the options it ends up with.
    pub config_options: Vec<SessionConfigOption>,
}

/// Why a provider call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The agent needs the user to sign in, in the agent's own tool (for
    /// Claude: `claude` then `/login`). Coqo never handles a login itself.
    #[error("sign-in required: {0}")]
    AuthRequired(String),
    /// The provider does not support this.
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The agent can't take a session back
    /// ([`resume_session`](AgentProvider::resume_session)): it no longer has
    /// it, or refused it. Trying again won't help, unlike
    /// [`Failed`](Self::Failed).
    #[error("{0}")]
    SessionLost(String),
    /// Anything else (process died, protocol error, …).
    #[error("{0}")]
    Failed(String),
}

/// An agent runtime: ACP, terminal or native.
///
/// Built-in providers are in-process mods that contribute an
/// `Arc<dyn AgentProvider>` to [`AgentProviderImpls`](crate::AgentProviderImpls)
/// together with their metadata in `agent_providers` (see
/// [`register_provider`](crate::register_provider)).
pub trait AgentProvider: Send + Sync + 'static {
    /// What it supports.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Whether it is set up on this machine, and the setup and sign-in
    /// actions it offers. Called for every `agents/providers`, so keep it
    /// cheap (cache what is slow to find out). `None`, the default, means
    /// always ready.
    fn status(&self) -> Option<ProviderStatus> {
        None
    }

    /// Runs the daemon-side setup action `action` from [`status`](Self::status)
    /// (`agents/runAction`), at the user's explicit request. The default
    /// has none.
    fn run_action(&self, action: String) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(std::future::ready(Err(ProviderError::Unsupported(
            format!("action `{action}`"),
        ))))
    }

    /// Starts a new session (ACP `session/new`). `events` is where the
    /// session reports updates and asks for permission, for as long as it
    /// lives. Updates sent before this returns (modes, slash commands, auth
    /// status) are recorded.
    fn start_session(
        &self,
        cx: SessionContext,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>>;

    /// Reattaches to a session this provider ran before (ACP
    /// `session/resume`, or `session/load` if that is all it has).
    ///
    /// Coqo keeps its own transcript, so updates sent **before this returns**
    /// are treated as history replay and dropped when the transcript is not
    /// empty. The default says it is unsupported. If the session has a
    /// transcript, the manager then marks it ended rather than start a
    /// fresh agent session that knows none of it. Return
    /// [`ProviderError::SessionLost`] when the agent refused the session (it
    /// ends too), and [`ProviderError::Failed`] when it could not be asked
    /// (the next prompt tries again).
    fn resume_session(
        &self,
        request: ResumeRequest,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>> {
        let _ = (request, events);
        Box::pin(std::future::ready(Err(ProviderError::Unsupported(
            "resume".into(),
        ))))
    }
}

/// One live session of a provider.
///
/// Calls may overlap: `cancel`, `set_mode`, `set_config_option` and
/// `close` can arrive while a `prompt` is running.
pub trait ProviderSession: Send + Sync + 'static {
    /// The provider's own id for the session (ACP `sessionId`, the terminal
    /// agent's `--session-id`), persisted for
    /// [`AgentProvider::resume_session`]. `None` if it cannot be resumed.
    fn provider_session_id(&self) -> Option<String>;

    /// Runs one turn (ACP `session/prompt`) and returns why it ended. Stream
    /// output through the session's [`SessionEvents`] meanwhile. After
    /// [`cancel`](Self::cancel), return [`StopReason::Cancelled`].
    fn prompt(
        &self,
        content: Vec<ContentBlock>,
    ) -> BoxFuture<'_, Result<StopReason, ProviderError>>;

    /// Asks the running turn to stop (ACP `session/cancel`). Returns without
    /// waiting for the turn to end.
    fn cancel(&self) -> BoxFuture<'_, Result<(), ProviderError>>;

    /// Switches the mode (ACP `session/set_mode`).
    fn set_mode(&self, mode_id: String) -> BoxFuture<'_, Result<(), ProviderError>> {
        let _ = mode_id;
        Box::pin(std::future::ready(Err(ProviderError::Unsupported(
            "session modes".into(),
        ))))
    }

    /// Changes one of the session's config options (ACP
    /// `session/set_config_option`). Report the options it leaves, with
    /// their values, as an [`AgentUpdate::ConfigOptions`].
    fn set_config_option(
        &self,
        config_id: String,
        value: ConfigValue,
    ) -> BoxFuture<'_, Result<(), ProviderError>> {
        let _ = (config_id, value);
        Box::pin(std::future::ready(Err(ProviderError::Unsupported(
            "session config options".into(),
        ))))
    }

    /// Ends the session and releases its resources. The default does
    /// nothing.
    fn close(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// What an agent reports during a session (ACP `session/update`).
#[derive(Clone, Debug, PartialEq)]
pub enum AgentUpdate {
    /// Part of a user message, replayed from the agent's own history.
    UserMessageChunk(ContentBlock),
    /// Streaming output.
    AgentMessageChunk(ContentBlock),
    /// Streaming reasoning.
    AgentThoughtChunk(ContentBlock),
    /// A new tool call.
    ToolCall(ToolCall),
    /// A change to a tool call.
    ToolCallUpdate(ToolCallUpdate),
    /// The plan (replaces the earlier one).
    Plan(Vec<PlanEntry>),
    /// Slash commands and skills (replaces the earlier list).
    AvailableCommands(Vec<AvailableCommand>),
    /// The session's modes.
    Modes(SessionModes),
    /// The current mode changed.
    CurrentMode(String),
    /// The session's config options (model, effort, …) with their current
    /// values; replaces the earlier list.
    ConfigOptions(Vec<SessionConfigOption>),
    /// Context and cost usage.
    Usage(Usage),
    /// The billing source. Report kind and plan only, never an email or
    /// organization.
    AuthStatus(AuthStatus),
    /// The agent's own title for the session (ACP `session_info_update`;
    /// Claude Code titles a session from its first prompt). It replaces a
    /// title Coqo derived from the prompt, never one the user gave. Not part
    /// of the transcript: it arrives on `sessions/changes`.
    Title(String),
}

impl AgentUpdate {
    /// The transcript entry this update records, if it is one.
    pub(crate) fn into_transcript(self) -> Option<SessionUpdate> {
        Some(match self {
            Self::UserMessageChunk(content) => SessionUpdate::UserMessageChunk { content },
            Self::AgentMessageChunk(content) => SessionUpdate::AgentMessageChunk { content },
            Self::AgentThoughtChunk(content) => SessionUpdate::AgentThoughtChunk { content },
            Self::ToolCall(c) => SessionUpdate::ToolCall(c),
            Self::ToolCallUpdate(u) => SessionUpdate::ToolCallUpdate(u),
            Self::Plan(entries) => SessionUpdate::Plan { entries },
            Self::AvailableCommands(commands) => SessionUpdate::AvailableCommands { commands },
            Self::Modes(m) => SessionUpdate::Modes(m),
            Self::CurrentMode(mode_id) => SessionUpdate::CurrentMode { mode_id },
            Self::ConfigOptions(config_options) => SessionUpdate::ConfigOptions { config_options },
            Self::Usage(u) => SessionUpdate::Usage(u),
            Self::AuthStatus(a) => SessionUpdate::AuthStatus(a),
            Self::Title(_) => return None,
        })
    }
}

/// A permission request from the agent (ACP `session/request_permission`),
/// or questions it asks with its own tooling (ACP form elicitation, such
/// as Claude Code's `AskUserQuestion`): then `questions` is not empty and
/// `options` is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PermissionAsk {
    /// What is asked, such as `Write pong.txt`, or the questions' message.
    pub title: String,
    /// The tool call it is about.
    pub tool_call: Option<ToolCallUpdate>,
    /// The possible answers.
    pub options: Vec<PermissionOption>,
    /// The questions, for a question request.
    pub questions: Vec<AgentQuestion>,
}

/// The receiving side of [`SessionEvents`]; the session manager implements it.
pub(crate) trait EventSink: Send + Sync + 'static {
    fn update(&self, update: AgentUpdate);
    fn request_permission(
        &self,
        ask: PermissionAsk,
        withdrawn: Option<BoxFuture<'static, ()>>,
    ) -> BoxFuture<'static, PermissionOutcome>;
    fn report_state(&self, state: AgentState, reason: Option<String>);
}

/// Where a provider session reports to. Cheap to clone; keep it for the
/// session's lifetime.
#[derive(Clone)]
pub struct SessionEvents {
    sink: Arc<dyn EventSink>,
}

impl fmt::Debug for SessionEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionEvents")
    }
}

impl SessionEvents {
    pub(crate) fn new(sink: Arc<dyn EventSink>) -> Self {
        Self { sink }
    }

    /// Events that go nowhere, with every permission request cancelled; for
    /// testing a provider on its own.
    pub fn detached() -> Self {
        struct Nowhere;
        impl EventSink for Nowhere {
            fn update(&self, _: AgentUpdate) {}
            fn request_permission(
                &self,
                _: PermissionAsk,
                _: Option<BoxFuture<'static, ()>>,
            ) -> BoxFuture<'static, PermissionOutcome> {
                Box::pin(std::future::ready(PermissionOutcome::Cancelled))
            }
            fn report_state(&self, _: AgentState, _: Option<String>) {}
        }
        Self::new(Arc::new(Nowhere))
    }

    /// Records an update: it is persisted and streamed to every attached UI.
    pub fn update(&self, update: AgentUpdate) {
        self.sink.update(update);
    }

    /// Asks the user. Resolves when a UI answers, or with
    /// [`PermissionOutcome::Cancelled`] when the turn is cancelled or the
    /// session closes. With no UI attached it simply waits (the daemon
    /// raises an alert).
    pub fn request_permission(&self, ask: PermissionAsk) -> BoxFuture<'static, PermissionOutcome> {
        self.sink.request_permission(ask, None)
    }

    /// [`Self::request_permission`], for a request the agent may take back:
    /// once `withdrawn` resolves before an answer, the request is resolved
    /// as [`PermissionOutcome::Cancelled`] (its card closes, the session
    /// stops waiting) and so is the returned future.
    pub fn request_permission_until(
        &self,
        ask: PermissionAsk,
        withdrawn: BoxFuture<'static, ()>,
    ) -> BoxFuture<'static, PermissionOutcome> {
        self.sink.request_permission(ask, Some(withdrawn))
    }

    /// Reports the agent's own state, such as `needs_input` or `blocked`
    /// (ACP signals, the `set_status` MCP tool, or a terminal agent's
    /// process state). `needs_permission` is the manager's and is ignored.
    pub fn report_state(&self, state: AgentState, reason: Option<String>) {
        self.sink.report_state(state, reason);
    }
}

/// Keeps the daemon from idle-exiting while agent work runs. `coqod`
/// implements it with its busy guards.
pub trait KeepAlive: Send + Sync + 'static {
    /// Marks work as busy until the returned guard is dropped.
    fn hold(&self, reason: String) -> Box<dyn Any + Send + Sync>;
}
