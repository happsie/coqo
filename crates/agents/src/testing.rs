// SPDX-License-Identifier: Apache-2.0

//! A scripted in-process [`AgentProvider`] for tests and UI development.
//!
//! Each prompt plays the next scripted turn: updates, permission requests,
//! waits and a stop reason. Everything the manager asks of it is recorded in
//! [`ScriptedProvider::calls`].
//!
//! ```
//! use coqo_agents::testing::{ScriptedProvider, Step};
//! use coqo_agents::AgentUpdate;
//! use coqo_protocol::methods::agents::{ContentBlock, StopReason};
//!
//! let provider = ScriptedProvider::new().turn([
//!     Step::Update(AgentUpdate::AgentMessageChunk(ContentBlock::text("Hello"))),
//!     Step::Stop(StopReason::EndTurn),
//! ]);
//! # let _ = provider;
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use coqo_mod_api::BoxFuture;
use coqo_protocol::methods::agents::{
    AgentState, ContentBlock, PermissionOutcome, ProviderCapabilities, SessionMode, SessionModes,
    StopReason,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::provider::{
    AgentProvider, AgentUpdate, PermissionAsk, ProviderError, ProviderSession, ResumeRequest,
    SessionContext, SessionEvents,
};

/// One step of a scripted turn.
#[derive(Clone)]
pub enum Step {
    /// Report an update.
    Update(AgentUpdate),
    /// Ask for permission and wait for the answer. A cancelled answer ends
    /// the turn as cancelled.
    Permission(PermissionAsk),
    /// Wait until the [`Notify`] fires, or the turn is cancelled.
    Wait(Arc<Notify>),
    /// Report the agent's own state.
    State(AgentState, Option<String>),
    /// Fail the turn.
    Fail(String),
    /// End the turn.
    Stop(StopReason),
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Update(u) => f.debug_tuple("Update").field(u).finish(),
            Self::Permission(p) => f.debug_tuple("Permission").field(&p.title).finish(),
            Self::Wait(_) => f.write_str("Wait"),
            Self::State(s, r) => f.debug_tuple("State").field(s).field(r).finish(),
            Self::Fail(e) => f.debug_tuple("Fail").field(e).finish(),
            Self::Stop(r) => f.debug_tuple("Stop").field(r).finish(),
        }
    }
}

/// What the manager asked a [`ScriptedProvider`] to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    /// `start_session`.
    Start {
        /// The session's working directory.
        cwd: String,
        /// Names of the MCP servers injected.
        mcp_servers: Vec<String>,
    },
    /// `resume_session` with this provider session id.
    Resume(String),
    /// `prompt`.
    Prompt(Vec<ContentBlock>),
    /// `cancel`.
    Cancel,
    /// `set_mode`.
    SetMode(String),
    /// `close`.
    Close,
}

#[derive(Default)]
struct Script {
    capabilities: ProviderCapabilities,
    on_start: Vec<AgentUpdate>,
    on_resume: Vec<AgentUpdate>,
    fail_start: Option<ProviderError>,
    turns: Mutex<VecDeque<Vec<Step>>>,
    calls: Mutex<Vec<Call>>,
    outcomes: Mutex<Vec<PermissionOutcome>>,
    next_session: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A scripted provider. Cheap to clone; clones share the script and the
/// call log.
#[derive(Clone)]
pub struct ScriptedProvider {
    script: Arc<Script>,
}

impl fmt::Debug for ScriptedProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptedProvider")
            .field("calls", &*lock(&self.script.calls))
            .finish_non_exhaustive()
    }
}

impl Default for ScriptedProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// The modes a [`ScriptedProvider`] with modes reports.
pub fn test_modes() -> SessionModes {
    let mode = |id: &str, name: &str| SessionMode {
        id: id.into(),
        name: name.into(),
        description: None,
    };
    SessionModes {
        current_mode_id: "default".into(),
        available_modes: vec![mode("default", "Manual"), mode("plan", "Plan")],
    }
}

impl ScriptedProvider {
    /// A provider that can resume and has modes (`default`, `plan`). A
    /// prompt with no scripted turn left answers "ok".
    pub fn new() -> Self {
        Self {
            script: Arc::new(Script {
                capabilities: ProviderCapabilities {
                    resume: true,
                    modes: true,
                    images: false,
                    embedded_context: true,
                },
                on_start: vec![AgentUpdate::Modes(test_modes())],
                ..Script::default()
            }),
        }
    }

    fn edit(mut self, f: impl FnOnce(&mut Script)) -> Self {
        f(Arc::get_mut(&mut self.script).expect("configure before cloning"));
        self
    }

    /// Sets the capabilities.
    pub fn capabilities(self, capabilities: ProviderCapabilities) -> Self {
        self.edit(|s| s.capabilities = capabilities)
    }

    /// Updates reported while a session starts (replaces the default modes).
    pub fn on_start(self, updates: impl IntoIterator<Item = AgentUpdate>) -> Self {
        self.edit(|s| s.on_start = updates.into_iter().collect())
    }

    /// Updates reported while a session resumes (history replay).
    pub fn on_resume(self, updates: impl IntoIterator<Item = AgentUpdate>) -> Self {
        self.edit(|s| s.on_resume = updates.into_iter().collect())
    }

    /// Fail every `start_session` with `error`.
    pub fn fail_start(self, error: ProviderError) -> Self {
        self.edit(|s| s.fail_start = Some(error))
    }

    /// Appends a scripted turn.
    pub fn turn(self, steps: impl IntoIterator<Item = Step>) -> Self {
        self.push_turn(steps);
        self
    }

    /// Appends a scripted turn after the provider is shared.
    pub fn push_turn(&self, steps: impl IntoIterator<Item = Step>) {
        lock(&self.script.turns).push_back(steps.into_iter().collect());
    }

    /// Everything the manager asked, in order.
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.script.calls).clone()
    }

    /// The answers to its permission requests, in order.
    pub fn outcomes(&self) -> Vec<PermissionOutcome> {
        lock(&self.script.outcomes).clone()
    }

    fn call(&self, call: Call) {
        lock(&self.script.calls).push(call);
    }

    fn session(&self, id: String, events: SessionEvents) -> Arc<dyn ProviderSession> {
        Arc::new(ScriptedSession {
            provider: self.clone(),
            id,
            events,
            cancel: Mutex::new(CancellationToken::new()),
        })
    }
}

impl AgentProvider for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.script.capabilities
    }

    fn start_session(
        &self,
        cx: SessionContext,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>> {
        self.call(Call::Start {
            cwd: cx.cwd.display().to_string(),
            mcp_servers: cx.mcp_servers.iter().map(|m| m.name.clone()).collect(),
        });
        let result = match &self.script.fail_start {
            Some(e) => Err(e.clone()),
            None => {
                for u in &self.script.on_start {
                    events.update(u.clone());
                }
                let n = self.script.next_session.fetch_add(1, Ordering::Relaxed) + 1;
                Ok(self.session(format!("fake-{n}"), events))
            }
        };
        Box::pin(std::future::ready(result))
    }

    fn resume_session(
        &self,
        request: ResumeRequest,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>> {
        self.call(Call::Resume(request.provider_session.clone()));
        for u in &self.script.on_resume {
            events.update(u.clone());
        }
        Box::pin(std::future::ready(Ok(
            self.session(request.provider_session, events)
        )))
    }
}

struct ScriptedSession {
    provider: ScriptedProvider,
    id: String,
    events: SessionEvents,
    cancel: Mutex<CancellationToken>,
}

impl ProviderSession for ScriptedSession {
    fn provider_session_id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn prompt(
        &self,
        content: Vec<ContentBlock>,
    ) -> BoxFuture<'_, Result<StopReason, ProviderError>> {
        self.provider.call(Call::Prompt(content));
        let token = {
            let mut cancel = lock(&self.cancel);
            *cancel = CancellationToken::new();
            cancel.clone()
        };
        let steps = lock(&self.provider.script.turns)
            .pop_front()
            .unwrap_or_else(|| {
                vec![
                    Step::Update(AgentUpdate::AgentMessageChunk(ContentBlock::text("ok"))),
                    Step::Stop(StopReason::EndTurn),
                ]
            });
        Box::pin(async move {
            for step in steps {
                if token.is_cancelled() {
                    return Ok(StopReason::Cancelled);
                }
                match step {
                    Step::Update(u) => self.events.update(u),
                    Step::Permission(ask) => {
                        let outcome = self.events.request_permission(ask).await;
                        lock(&self.provider.script.outcomes).push(outcome.clone());
                        if outcome == PermissionOutcome::Cancelled {
                            return Ok(StopReason::Cancelled);
                        }
                    }
                    Step::Wait(notify) => {
                        tokio::select! {
                            () = notify.notified() => {}
                            () = token.cancelled() => return Ok(StopReason::Cancelled),
                        }
                    }
                    Step::State(state, reason) => self.events.report_state(state, reason),
                    Step::Fail(e) => return Err(ProviderError::Failed(e)),
                    Step::Stop(reason) => return Ok(reason),
                }
            }
            Ok(StopReason::EndTurn)
        })
    }

    fn cancel(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        self.provider.call(Call::Cancel);
        lock(&self.cancel).cancel();
        Box::pin(std::future::ready(Ok(())))
    }

    fn set_mode(&self, mode_id: String) -> BoxFuture<'_, Result<(), ProviderError>> {
        self.provider.call(Call::SetMode(mode_id.clone()));
        self.events.update(AgentUpdate::CurrentMode(mode_id));
        Box::pin(std::future::ready(Ok(())))
    }

    fn close(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        self.provider.call(Call::Close);
        lock(&self.cancel).cancel();
        Box::pin(std::future::ready(Ok(())))
    }
}
