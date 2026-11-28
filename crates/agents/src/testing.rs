// SPDX-License-Identifier: Apache-2.0

//! A scripted in-process [`AgentProvider`] for tests and UI development.
//!
//! Each prompt plays the next scripted turn: updates, permission requests,
//! waits and a stop reason. Everything the manager asks of it is recorded in
//! [`ScriptedProvider::calls`], and the context of each start and resume in
//! [`ScriptedProvider::contexts`].
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
    AgentState, ConfigOptionKind, ConfigOptionValue, ConfigValue, ContentBlock, PermissionOutcome,
    ProviderCapabilities, ProviderStatus, SessionConfigOption, SessionMode, SessionModes,
    StopReason, config_category,
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
    /// `set_config_option`.
    SetConfigOption(String, ConfigValue),
    /// `close`.
    Close,
    /// `run_action`.
    RunAction(String),
}

#[derive(Default)]
struct Script {
    capabilities: ProviderCapabilities,
    on_start: Vec<AgentUpdate>,
    on_resume: Vec<AgentUpdate>,
    fail_start: Option<ProviderError>,
    fail_resume: Option<ProviderError>,
    /// The status reported, and what each action changes it to.
    status: Mutex<Option<ProviderStatus>>,
    actions: Vec<(String, Option<ProviderStatus>)>,
    turns: Mutex<VecDeque<Vec<Step>>>,
    calls: Mutex<Vec<Call>>,
    contexts: Mutex<Vec<SessionContext>>,
    /// The config options of each `resume_session`.
    resume_options: Mutex<Vec<Vec<SessionConfigOption>>>,
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

/// Config options like Claude's: `model` (Opus or Sonnet, category
/// `model`), `effort` (Low, Medium or High, `thought_level`) and `fast` (a
/// toggle, `speed`).
pub fn test_config_options() -> Vec<SessionConfigOption> {
    let select = |id: &str, name: &str, category: &str, current: &str, values: &[(&str, &str)]| {
        SessionConfigOption {
            id: id.into(),
            name: name.into(),
            description: None,
            category: Some(category.into()),
            kind: ConfigOptionKind::Select {
                current_value: current.into(),
                options: values
                    .iter()
                    .map(|(value, name)| ConfigOptionValue {
                        value: (*value).into(),
                        name: (*name).into(),
                        description: None,
                        group: None,
                    })
                    .collect(),
            },
        }
    };
    vec![
        select(
            "model",
            "Model",
            config_category::MODEL,
            "opus",
            &[("opus", "Opus"), ("sonnet", "Sonnet")],
        ),
        select(
            "effort",
            "Effort",
            config_category::THOUGHT_LEVEL,
            "medium",
            &[("low", "Low"), ("medium", "Medium"), ("high", "High")],
        ),
        SessionConfigOption {
            id: "fast".into(),
            name: "Fast mode".into(),
            description: None,
            category: Some(config_category::SPEED.into()),
            kind: ConfigOptionKind::Boolean {
                current_value: false,
            },
        },
    ]
}

/// The last config options in `updates`, if any.
fn config_options(updates: &[AgentUpdate]) -> Option<Vec<SessionConfigOption>> {
    updates.iter().rev().find_map(|u| match u {
        AgentUpdate::ConfigOptions(o) => Some(o.clone()),
        _ => None,
    })
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

    /// Fail every `resume_session` with `error`.
    pub fn fail_resume(self, error: ProviderError) -> Self {
        self.edit(|s| s.fail_resume = Some(error))
    }

    /// Reports `status` in `agents/providers`.
    pub fn status(self, status: ProviderStatus) -> Self {
        self.edit(|s| *lock(&s.status) = Some(status))
    }

    /// Offers the daemon action `id`; running it sets the status to
    /// `after`.
    pub fn action(self, id: &str, after: Option<ProviderStatus>) -> Self {
        self.edit(|s| s.actions.push((id.to_owned(), after)))
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

    /// The context of each `start_session` and `resume_session`, in order:
    /// what a real provider would launch the agent with, MCP servers
    /// included.
    pub fn contexts(&self) -> Vec<SessionContext> {
        lock(&self.script.contexts).clone()
    }

    /// The config options each `resume_session` was asked to keep, in
    /// order.
    pub fn resume_options(&self) -> Vec<Vec<SessionConfigOption>> {
        lock(&self.script.resume_options).clone()
    }

    /// The answers to its permission requests, in order.
    pub fn outcomes(&self) -> Vec<PermissionOutcome> {
        lock(&self.script.outcomes).clone()
    }

    fn call(&self, call: Call) {
        lock(&self.script.calls).push(call);
    }

    fn session(
        &self,
        id: String,
        events: SessionEvents,
        options: Vec<SessionConfigOption>,
    ) -> Arc<dyn ProviderSession> {
        Arc::new(ScriptedSession {
            provider: self.clone(),
            id,
            events,
            cancel: Mutex::new(CancellationToken::new()),
            options: Mutex::new(options),
        })
    }
}

impl AgentProvider for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.script.capabilities
    }

    fn status(&self) -> Option<ProviderStatus> {
        lock(&self.script.status).clone()
    }

    fn run_action(&self, action: String) -> BoxFuture<'_, Result<(), ProviderError>> {
        self.call(Call::RunAction(action.clone()));
        let result = match self.script.actions.iter().find(|(id, _)| *id == action) {
            Some((_, after)) => {
                *lock(&self.script.status) = after.clone();
                Ok(())
            }
            None => Err(ProviderError::Unsupported(format!("action `{action}`"))),
        };
        Box::pin(std::future::ready(result))
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
        lock(&self.script.contexts).push(cx);
        let result = match &self.script.fail_start {
            Some(e) => Err(e.clone()),
            None => {
                for u in &self.script.on_start {
                    events.update(u.clone());
                }
                let n = self.script.next_session.fetch_add(1, Ordering::Relaxed) + 1;
                let options = config_options(&self.script.on_start).unwrap_or_default();
                Ok(self.session(format!("fake-{n}"), events, options))
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
        lock(&self.script.contexts).push(request.context);
        lock(&self.script.resume_options).push(request.config_options.clone());
        if let Some(e) = &self.script.fail_resume {
            return Box::pin(std::future::ready(Err(e.clone())));
        }
        for u in &self.script.on_resume {
            events.update(u.clone());
        }
        // Like an agent that keeps its settings across a resume.
        let options = config_options(&self.script.on_resume).unwrap_or(request.config_options);
        Box::pin(std::future::ready(Ok(self.session(
            request.provider_session,
            events,
            options,
        ))))
    }
}

struct ScriptedSession {
    provider: ScriptedProvider,
    id: String,
    events: SessionEvents,
    cancel: Mutex<CancellationToken>,
    /// Its config options, as a real agent keeps them.
    options: Mutex<Vec<SessionConfigOption>>,
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

    /// Sets the value if the session has the option and it takes the
    /// value, then reports every option.
    fn set_config_option(
        &self,
        config_id: String,
        value: ConfigValue,
    ) -> BoxFuture<'_, Result<(), ProviderError>> {
        self.provider
            .call(Call::SetConfigOption(config_id.clone(), value.clone()));
        let result = {
            let mut options = lock(&self.options);
            let set = options
                .iter_mut()
                .find(|o| o.id == config_id)
                .is_some_and(|o| o.set_value(&value));
            if set {
                Ok(options.clone())
            } else {
                Err(ProviderError::Failed(format!(
                    "no value {value:?} for config option {config_id}"
                )))
            }
        };
        Box::pin(std::future::ready(result.map(|options| {
            self.events.update(AgentUpdate::ConfigOptions(options));
        })))
    }

    fn close(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        self.provider.call(Call::Close);
        lock(&self.cancel).cancel();
        Box::pin(std::future::ready(Ok(())))
    }
}
