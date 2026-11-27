// SPDX-License-Identifier: Apache-2.0

//! The session manager: daemon-owned agent sessions.
//!
//! - Every session update is written to the [`SessionStore`] and then
//!   broadcast, under the session's lock, so the stored transcript and the
//!   live stream have the same order and seqs.
//! - A turn runs in its own task, owned by the manager, so it does not care
//!   whether the client that started it is still connected.
//! - While a turn runs (or the agent reports `working`), the session holds a
//!   [`KeepAlive`] guard, so the daemon does not idle-exit mid-turn.
//! - When the daemon starts, sessions whose stored state says a turn was
//!   running get their turn ended as `interrupted` and their state set to
//!   `error`. Their provider session is resumed on the next prompt.
//! - The session's state follows the state machine in [`crate::state`]:
//!   every change goes through `Inner::transition`, which records a
//!   `state` update when the state or its reason changes and saves the info
//!   (state, `state_since`, the agent's status) as a `sessions/changes`
//!   event.
//! - A permission request first meets the workspace's permission rules
//!   ([`crate::rules`]): one that a rule answers is recorded as requested and
//!   resolved at once (`byRule`) and never waits for the user. Every other
//!   request waits, in `needs_permission`, for `sessions/respondPermission`;
//!   with `remember` the answer also stores the request's suggested rule.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use coqo_mod_api::BoxFuture;
use coqo_protocol::methods::agents::{
    AgentState, AgentStatus, CreateSessionParams, ListSessionsParams, PermissionOutcome,
    PermissionRequest, PermissionRequestId, PermissionRule, PermissionRuleId, PromptResult,
    ProviderInfo, SessionId, SessionInfo, SessionList, SessionSnapshot, SessionUpdate,
    SetStatusParams, StatusProgress, StopReason, ToolCallUpdate,
};
use coqo_protocol::{ErrorObject, EventFeed, Sequenced};
use coqo_storage::Timestamp;
use tokio::sync::{broadcast, oneshot};

use crate::error::AgentError;
use crate::feed::{Page, feed};
use crate::fold::Fold;
use crate::provider::{
    AgentUpdate, EventSink, KeepAlive, McpServer, PermissionAsk, ProviderError, ProviderSession,
    ResumeRequest, SessionContext, SessionEvents,
};
use crate::registry::ProviderRegistry;
use crate::rules::{self, Subject};
use crate::state::{self, Event, Tracked};
use crate::store::{SessionStore, StoredSession};

type Result<T> = std::result::Result<T, AgentError>;

/// Live updates buffered per session before a slow subscriber falls back to
/// reading the store.
const UPDATE_CHANNEL: usize = 1024;
/// Live session-info changes buffered.
const CHANGE_CHANNEL: usize = 256;
/// Rows read per replay page.
const PAGE: usize = 256;

/// Chooses the MCP servers injected into a session, such as `coqo-editor`.
pub type McpServerSource = Arc<dyn Fn(&SessionInfo) -> Vec<McpServer> + Send + Sync>;

/// Optional parts of a [`SessionManager`].
#[derive(Clone, Default)]
pub struct ManagerOptions {
    /// Keeps the daemon alive while turns run.
    pub keep_alive: Option<Arc<dyn KeepAlive>>,
    /// MCP servers to inject into each session.
    pub mcp_servers: Option<McpServerSource>,
}

impl fmt::Debug for ManagerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagerOptions")
            .field("keep_alive", &self.keep_alive.is_some())
            .field("mcp_servers", &self.mcp_servers.is_some())
            .finish()
    }
}

fn now() -> i64 {
    Timestamp::now().0
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Daemon-owned agent sessions. Cheap to clone.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

impl fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionManager")
            .field("loaded", &lock(&self.inner.sessions).len())
            .finish_non_exhaustive()
    }
}

struct Inner {
    store: Arc<SessionStore>,
    providers: Arc<dyn ProviderRegistry>,
    options: ManagerOptions,
    /// Sessions loaded from the store. Never held while a session is locked.
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    /// Head of `sessions/changes`. Locked after a session, never before.
    changes: Mutex<u64>,
    changes_tx: broadcast::Sender<Sequenced<SessionInfo>>,
}

struct Session {
    id: SessionId,
    state: Mutex<SessionState>,
    /// Serializes starting or resuming the provider session.
    live_lock: tokio::sync::Mutex<()>,
    updates: broadcast::Sender<Sequenced<SessionUpdate>>,
}

struct SessionState {
    stored: StoredSession,
    /// Seq of the last update.
    head: u64,
    fold: Fold,
    live: Option<Arc<dyn ProviderSession>>,
    /// The running turn.
    turn: Option<u64>,
    /// Providers waiting for an answer, by request.
    waiters: HashMap<PermissionRequestId, oneshot::Sender<PermissionOutcome>>,
    /// Drop agent updates: the provider is replaying history we already have.
    replaying: bool,
    busy: Option<Box<dyn Any + Send + Sync>>,
}

impl SessionState {
    fn info(&self) -> &SessionInfo {
        &self.stored.info
    }

    /// What the state machine tracks.
    fn tracked(&self) -> Tracked {
        let info = self.info();
        Tracked {
            state: info.state,
            reason: info.state_reason.clone(),
            status: info.status.clone(),
        }
    }
}

impl Inner {
    // ---- recording ---------------------------------------------------------

    /// Persists `update` as the session's next seq, then broadcasts it.
    fn record(&self, s: &Session, st: &mut SessionState, update: SessionUpdate) -> Result<u64> {
        let seq = st.head + 1;
        self.store.append(&s.id, seq, &update, now())?;
        st.head = seq;
        st.fold.apply(&update);
        let _ = s.updates.send(Sequenced { seq, event: update });
        Ok(seq)
    }

    /// Persists the session's info as the next change, then broadcasts it.
    fn save_info(&self, st: &mut SessionState) -> Result<()> {
        st.stored.info.updated_at = now();
        let mut head = lock(&self.changes);
        let seq = *head + 1;
        self.store.save(&st.stored, seq)?;
        *head = seq;
        let _ = self.changes_tx.send(Sequenced {
            seq,
            event: st.stored.info.clone(),
        });
        Ok(())
    }

    /// Applies `event` to the session's state machine ([`crate::state`]):
    /// records a `state` update if the state or its reason changed, and
    /// saves the info if anything changed.
    fn transition(&self, s: &Session, st: &mut SessionState, event: Event) -> Result<()> {
        let current = st.tracked();
        let next = state::next(&current, event);
        if next != current {
            let Tracked {
                state,
                reason,
                status,
            } = next;
            let info = &mut st.stored.info;
            let changed = info.state != state || info.state_reason != reason;
            if info.state != state {
                info.state_since = Some(now());
            }
            info.state = state;
            info.state_reason.clone_from(&reason);
            info.status = status;
            if changed {
                self.record(s, st, SessionUpdate::State { state, reason })?;
            }
            self.save_info(st)?;
        }
        self.update_busy(s, st);
        Ok(())
    }

    /// Holds a keep-alive guard while the session has work running.
    fn update_busy(&self, s: &Session, st: &mut SessionState) {
        let busy = !st.info().closed
            && (st.turn.is_some()
                || matches!(
                    st.info().state,
                    AgentState::Working | AgentState::NeedsPermission
                ));
        if !busy {
            st.busy = None;
        } else if st.busy.is_none()
            && let Some(keep_alive) = &self.options.keep_alive
        {
            st.busy = Some(keep_alive.hold(format!("agent session {}", s.id)));
        }
    }

    /// Mirrors the folded current mode into the session info.
    fn sync_mode(&self, st: &mut SessionState) -> Result<()> {
        let mode = st.fold.mode().map(str::to_owned);
        if mode.is_some() && st.info().mode != mode {
            st.stored.info.mode = mode;
            self.save_info(st)?;
        }
        Ok(())
    }

    fn resolve(
        &self,
        s: &Session,
        st: &mut SessionState,
        request_id: PermissionRequestId,
        outcome: PermissionOutcome,
        remembered: Option<PermissionRule>,
    ) -> Result<()> {
        if let Some(waiter) = st.waiters.remove(&request_id) {
            let _ = waiter.send(outcome.clone());
        }
        self.record(
            s,
            st,
            SessionUpdate::PermissionResolved {
                request_id,
                outcome,
                by_rule: None,
                remembered,
            },
        )?;
        Ok(())
    }

    /// The session's workspace and what a permission request about
    /// `tool_call` does, if a rule may answer it (see [`crate::rules`]).
    fn rule_subject(
        &self,
        st: &SessionState,
        tool_call: Option<&ToolCallUpdate>,
    ) -> Option<(String, Subject)> {
        let workspace = st.info().workspace.clone()?;
        let tool_call = tool_call?;
        let mut facts = st
            .fold
            .tools
            .get(&tool_call.id)
            .cloned()
            .unwrap_or_default();
        facts.apply(tool_call);
        let roots = match self.store.workspace_roots(&workspace) {
            Ok(roots) => rules::canonical_roots(roots),
            Err(e) => {
                tracing::warn!(%workspace, error = %e, "cannot read the workspace's roots");
                return None;
            }
        };
        let subject = Subject::of(&facts, &roots, Path::new(&st.info().cwd))?;
        Some((workspace, subject))
    }

    /// Resolves every pending permission request as cancelled.
    fn cancel_pending(&self, s: &Session, st: &mut SessionState) -> Result<()> {
        let pending: Vec<_> = st.fold.pending.keys().copied().collect();
        for id in pending {
            self.resolve(s, st, id, PermissionOutcome::Cancelled, None)?;
        }
        Ok(())
    }

    /// Ends a turn the daemon can no longer finish.
    fn interrupt(&self, s: &Session, st: &mut SessionState, reason: &str) -> Result<()> {
        self.cancel_pending(s, st)?;
        if let Some(turn) = st.turn.take().or(st.fold.open_turn) {
            self.record(
                s,
                st,
                SessionUpdate::TurnEnded {
                    turn,
                    stop_reason: StopReason::Interrupted,
                    error: Some(reason.to_owned()),
                },
            )?;
        }
        self.transition(s, st, Event::Failed(reason.to_owned()))
    }

    // ---- loading -------------------------------------------------------------

    fn session(&self, id: &SessionId) -> Result<Arc<Session>> {
        let mut sessions = lock(&self.sessions);
        if let Some(s) = sessions.get(id) {
            return Ok(Arc::clone(s));
        }
        let stored = self
            .store
            .get(id)?
            .ok_or_else(|| AgentError::UnknownSession(id.clone()))?;
        let mut fold = Fold::default();
        let mut head = 0;
        loop {
            let page = self.store.updates_since(id, head, PAGE)?;
            let Some((last, _)) = page.last() else { break };
            head = *last;
            for (_, u) in &page {
                fold.apply(u);
            }
        }
        let s = Arc::new(Session::new(stored, head, fold));
        sessions.insert(id.clone(), Arc::clone(&s));
        Ok(s)
    }

    fn mcp_servers(&self, info: &SessionInfo) -> Vec<McpServer> {
        self.options
            .mcp_servers
            .as_ref()
            .map(|f| f(info))
            .unwrap_or_default()
    }

    // ---- provider sessions -----------------------------------------------------

    /// The live provider session, starting or resuming it if needed.
    async fn ensure_live(self: &Arc<Self>, s: &Arc<Session>) -> Result<Arc<dyn ProviderSession>> {
        let _starting = s.live_lock.lock().await;
        let (info, provider_session, head) = {
            let st = lock(&s.state);
            if st.info().closed {
                return Err(AgentError::Closed(s.id.clone()));
            }
            if let Some(live) = &st.live {
                return Ok(Arc::clone(live));
            }
            (
                st.info().clone(),
                st.stored.provider_session.clone(),
                st.head,
            )
        };
        let provider = self
            .providers
            .provider(&info.provider)
            .ok_or_else(|| AgentError::UnknownProvider(info.provider.clone()))?;
        // Built for each start or resume: the MCP servers may carry one-time
        // credentials (the `coqo-editor` token), so a start after a failed
        // resume gets its own.
        let context = || SessionContext {
            session: s.id.clone(),
            workspace: info.workspace.clone(),
            cwd: PathBuf::from(&info.cwd),
            mcp_servers: self.mcp_servers(&info),
        };
        let events = SessionEvents::new(Arc::new(Sink {
            inner: Arc::downgrade(self),
            session: Arc::clone(s),
        }));

        let mut live = None;
        if let Some(provider_session) = provider_session
            && provider.capabilities().resume
        {
            lock(&s.state).replaying = head > 0;
            let resumed = provider
                .resume_session(
                    ResumeRequest {
                        context: context(),
                        provider_session,
                    },
                    events.clone(),
                )
                .await;
            lock(&s.state).replaying = false;
            match resumed {
                Ok(l) => live = Some(l),
                Err(e @ ProviderError::AuthRequired(_)) => return Err(e.into()),
                Err(e) => tracing::warn!(
                    session = %s.id, error = %e,
                    "cannot resume the provider session; starting a new one"
                ),
            }
        }
        let live = match live {
            Some(live) => live,
            None => provider.start_session(context(), events).await?,
        };

        let mut st = lock(&s.state);
        let provider_session = live.provider_session_id();
        if provider_session != st.stored.provider_session {
            st.stored.provider_session = provider_session;
            self.save_info(&mut st)?;
        }
        st.live = Some(Arc::clone(&live));
        Ok(live)
    }

    fn finish_turn(
        &self,
        s: &Session,
        turn: u64,
        result: std::result::Result<StopReason, ProviderError>,
    ) -> Result<()> {
        let mut st = lock(&s.state);
        if st.turn != Some(turn) {
            // Already ended by close or shutdown.
            return Ok(());
        }
        self.cancel_pending(s, &mut st)?;
        let (stop_reason, error) = match result {
            Ok(r) => (r, None),
            Err(e) => (StopReason::Error, Some(e.to_string())),
        };
        self.record(
            s,
            &mut st,
            SessionUpdate::TurnEnded {
                turn,
                stop_reason,
                error: error.clone(),
            },
        )?;
        st.turn = None;
        self.transition(
            s,
            &mut st,
            Event::TurnEnded {
                stop: stop_reason,
                error,
            },
        )
    }

    /// Applies a provider's or a daemon feature's reported state.
    fn report(
        &self,
        s: &Session,
        st: &mut SessionState,
        state: AgentState,
        reason: Option<String>,
    ) -> Result<()> {
        if state == AgentState::NeedsPermission {
            tracing::warn!(session = %s.id, "only the session manager reports needs_permission");
        }
        let running = st.turn.is_some();
        self.transition(
            s,
            st,
            Event::Reported {
                state,
                reason,
                running,
            },
        )
    }
}

impl Session {
    fn new(stored: StoredSession, head: u64, fold: Fold) -> Self {
        Self {
            id: stored.info.id.clone(),
            state: Mutex::new(SessionState {
                stored,
                head,
                fold,
                live: None,
                turn: None,
                waiters: HashMap::new(),
                replaying: false,
                busy: None,
            }),
            live_lock: tokio::sync::Mutex::new(()),
            updates: broadcast::channel(UPDATE_CHANNEL).0,
        }
    }
}

/// What a provider session reports to.
struct Sink {
    inner: Weak<Inner>,
    session: Arc<Session>,
}

impl EventSink for Sink {
    fn update(&self, update: AgentUpdate) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let s = &self.session;
        let mut st = lock(&s.state);
        if st.replaying || st.info().closed {
            return;
        }
        let mode = matches!(update, AgentUpdate::Modes(_) | AgentUpdate::CurrentMode(_));
        let result = inner.record(s, &mut st, update.into()).and_then(|_| {
            if mode {
                inner.sync_mode(&mut st)
            } else {
                Ok(())
            }
        });
        if let Err(e) = result {
            tracing::error!(session = %s.id, error = %e, "cannot record an agent update");
        }
    }

    fn request_permission(&self, ask: PermissionAsk) -> BoxFuture<'static, PermissionOutcome> {
        let cancelled = || -> BoxFuture<'static, PermissionOutcome> {
            Box::pin(std::future::ready(PermissionOutcome::Cancelled))
        };
        let Some(inner) = self.inner.upgrade() else {
            return cancelled();
        };
        let s = &self.session;
        let mut st = lock(&s.state);
        if st.replaying || st.info().closed {
            return cancelled();
        }
        let subject = inner.rule_subject(&st, ask.tool_call.as_ref());
        let request = PermissionRequest {
            id: PermissionRequestId(st.fold.last_permission + 1),
            title: ask.title,
            tool_call: ask.tool_call,
            options: ask.options,
            suggested_rule: subject.as_ref().and_then(|(_, subject)| subject.suggest()),
        };
        let id = request.id;
        let answer = subject.and_then(|(workspace, subject)| {
            let rules = inner
                .store
                .rules(&workspace)
                .inspect_err(
                    |e| tracing::warn!(%workspace, error = %e, "cannot read permission rules"),
                )
                .ok()?;
            let (rule, option_id) = rules::decide(&rules, &subject, &request.options)?;
            Some((rule.clone(), option_id))
        });
        if let Some((rule, option_id)) = answer {
            let outcome = PermissionOutcome::Selected { option_id };
            let result = inner
                .record(s, &mut st, SessionUpdate::PermissionRequested(request))
                .and_then(|_| {
                    inner.record(
                        s,
                        &mut st,
                        SessionUpdate::PermissionResolved {
                            request_id: id,
                            outcome: outcome.clone(),
                            by_rule: Some(rule),
                            remembered: None,
                        },
                    )
                });
            if let Err(e) = result {
                tracing::error!(session = %s.id, error = %e, "cannot record a rule's answer");
                return cancelled();
            }
            return Box::pin(std::future::ready(outcome));
        }
        let (tx, rx) = oneshot::channel();
        let result = inner
            .record(s, &mut st, SessionUpdate::PermissionRequested(request))
            .and_then(|_| {
                st.waiters.insert(id, tx);
                inner.transition(s, &mut st, Event::PermissionAsked)
            });
        if let Err(e) = result {
            tracing::error!(session = %s.id, error = %e, "cannot record a permission request");
            st.waiters.remove(&id);
            return cancelled();
        }
        Box::pin(async move { rx.await.unwrap_or(PermissionOutcome::Cancelled) })
    }

    fn report_state(&self, state: AgentState, reason: Option<String>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let s = &self.session;
        let mut st = lock(&s.state);
        if st.replaying || st.info().closed {
            return;
        }
        if let Err(e) = inner.report(s, &mut st, state, reason) {
            tracing::error!(session = %s.id, error = %e, "cannot record a state change");
        }
    }
}

const INTERRUPTED: &str = "interrupted: the daemon stopped during the turn";

impl SessionManager {
    /// A manager over `store`, finding providers in `providers`.
    ///
    /// Sessions the store says were mid-turn (the daemon crashed or was
    /// killed) get that turn ended as interrupted.
    pub fn open(
        store: SessionStore,
        providers: Arc<dyn ProviderRegistry>,
        options: ManagerOptions,
    ) -> Result<Self> {
        let change_head = store.change_head()?;
        let inner = Arc::new(Inner {
            store: Arc::new(store),
            providers,
            options,
            sessions: Mutex::new(HashMap::new()),
            changes: Mutex::new(change_head),
            changes_tx: broadcast::channel(CHANGE_CHANNEL).0,
        });
        for id in inner.store.running()? {
            let s = inner.session(&id)?;
            let mut st = lock(&s.state);
            tracing::info!(session = %id, "marking a turn interrupted by a daemon restart");
            inner.interrupt(&s, &mut st, INTERRUPTED)?;
        }
        Ok(Self { inner })
    }

    /// Every provider.
    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.inner.providers.providers()
    }

    /// Runs a provider's setup action (`agents/runAction`) and returns the
    /// provider's new info. Holds the daemon open while it runs.
    pub async fn run_action(&self, provider: &str, action: String) -> Result<ProviderInfo> {
        let runtime = self
            .inner
            .providers
            .provider(provider)
            .ok_or_else(|| AgentError::UnknownProvider(provider.to_owned()))?;
        let _busy = self
            .inner
            .options
            .keep_alive
            .as_ref()
            .map(|k| k.hold(format!("agent provider {provider}: {action}")));
        runtime.run_action(action).await?;
        self.providers()
            .into_iter()
            .find(|p| p.id == provider)
            .ok_or_else(|| AgentError::UnknownProvider(provider.to_owned()))
    }

    /// Sessions, most recently updated first, and the `sessions/changes`
    /// seq they reflect.
    pub fn list(&self, params: &ListSessionsParams) -> Result<SessionList> {
        let head = lock(&self.inner.changes);
        let sessions = self
            .inner
            .store
            .list(params.workspace.as_deref(), params.include_closed)?;
        Ok(SessionList {
            sessions,
            seq: *head,
        })
    }

    /// One session's folded state.
    pub fn snapshot(&self, id: &SessionId) -> Result<SessionSnapshot> {
        let s = self.inner.session(id)?;
        let st = lock(&s.state);
        Ok(SessionSnapshot {
            info: st.info().clone(),
            head: st.head,
            turns: st.fold.turns,
            running: st.turn.is_some(),
            pending_permissions: st.fold.pending.values().cloned().collect(),
            plan: st.fold.plan.clone(),
            modes: st.fold.modes.clone(),
            available_commands: st.fold.commands.clone(),
            usage: st.fold.usage.clone(),
            auth: st.fold.auth.clone(),
        })
    }

    /// Creates a session and starts its provider session.
    ///
    /// If the provider fails to start, the session is kept (closed, in state
    /// `error`) so the failure is visible in its history, and the error is
    /// returned.
    pub async fn create(&self, params: CreateSessionParams) -> Result<SessionInfo> {
        let inner = &self.inner;
        if !Path::new(&params.cwd).is_absolute() {
            return Err(AgentError::InvalidParams(format!(
                "cwd must be an absolute path, got `{}`",
                params.cwd
            )));
        }
        if inner.providers.provider(&params.provider).is_none() {
            return Err(AgentError::UnknownProvider(params.provider));
        }
        let ts = now();
        let stored = StoredSession {
            info: SessionInfo {
                id: SessionId(uuid::Uuid::new_v4().to_string()),
                provider: params.provider,
                workspace: params.workspace,
                cwd: params.cwd,
                title: params.title,
                state: AgentState::Idle,
                state_reason: None,
                state_since: Some(ts),
                status: None,
                mode: None,
                closed: false,
                created_at: ts,
                updated_at: ts,
            },
            provider_session: None,
        };
        let id = stored.info.id.clone();
        let s = Arc::new(Session::new(stored, 0, Fold::default()));
        inner.save_info(&mut lock(&s.state))?;
        lock(&inner.sessions).insert(id.clone(), Arc::clone(&s));

        if let Err(e) = inner.ensure_live(&s).await {
            let mut st = lock(&s.state);
            inner.transition(&s, &mut st, Event::Failed(e.to_string()))?;
            st.stored.info.closed = true;
            inner.record(&s, &mut st, SessionUpdate::Closed)?;
            inner.save_info(&mut st)?;
            return Err(e);
        }
        if let Some(mode) = params.mode {
            let current = lock(&s.state).fold.mode().map(str::to_owned);
            if current.as_deref() != Some(mode.as_str()) {
                self.set_mode(&id, mode).await?;
            }
        }
        Ok(lock(&s.state).info().clone())
    }

    /// Starts a turn. It runs in the manager, not in the caller: the turn
    /// goes on if the client disconnects.
    pub async fn prompt(
        &self,
        id: &SessionId,
        content: Vec<coqo_protocol::methods::agents::ContentBlock>,
    ) -> Result<PromptResult> {
        let inner = &self.inner;
        if content.is_empty() {
            return Err(AgentError::InvalidParams("empty prompt".into()));
        }
        let s = inner.session(id)?;
        let check = |st: &SessionState| {
            if st.info().closed {
                Err(AgentError::Closed(id.clone()))
            } else if st.turn.is_some() {
                Err(AgentError::Busy(id.clone()))
            } else {
                Ok(())
            }
        };
        check(&lock(&s.state))?;
        let live = inner.ensure_live(&s).await?;

        let (turn, seq) = {
            let mut st = lock(&s.state);
            check(&st)?;
            let turn = st.fold.turns + 1;
            let seq = inner.record(
                &s,
                &mut st,
                SessionUpdate::UserMessage {
                    content: content.clone(),
                },
            )?;
            inner.record(&s, &mut st, SessionUpdate::TurnStarted { turn })?;
            st.turn = Some(turn);
            inner.transition(&s, &mut st, Event::TurnStarted)?;
            (turn, seq)
        };

        let task_inner = Arc::clone(inner);
        tokio::spawn(async move {
            let result = live.prompt(content).await;
            if let Err(e) = task_inner.finish_turn(&s, turn, result) {
                tracing::error!(session = %s.id, error = %e, "cannot record the end of a turn");
            }
        });
        Ok(PromptResult { turn, seq })
    }

    /// Cancels the running turn. Returns whether one was running.
    pub async fn cancel(&self, id: &SessionId) -> Result<bool> {
        let inner = &self.inner;
        let s = inner.session(id)?;
        let live = {
            let mut st = lock(&s.state);
            if st.turn.is_none() {
                return Ok(false);
            }
            inner.cancel_pending(&s, &mut st)?;
            inner.transition(&s, &mut st, Event::CancelRequested)?;
            st.live.clone()
        };
        if let Some(live) = live {
            live.cancel().await?;
        }
        Ok(true)
    }

    /// Answers a pending permission request.
    pub fn respond_permission(
        &self,
        id: &SessionId,
        request_id: PermissionRequestId,
        outcome: PermissionOutcome,
    ) -> Result<()> {
        self.answer_permission(id, request_id, outcome, false)
    }

    /// Answers a pending permission request. With `remember`, also stores
    /// the request's suggested rule in the session's workspace, allowing or
    /// rejecting as the chosen option does; the option goes to the agent
    /// unchanged either way.
    pub fn answer_permission(
        &self,
        id: &SessionId,
        request_id: PermissionRequestId,
        outcome: PermissionOutcome,
        remember: bool,
    ) -> Result<()> {
        let inner = &self.inner;
        let s = inner.session(id)?;
        let mut st = lock(&s.state);
        let request = st
            .fold
            .pending
            .get(&request_id)
            .ok_or_else(|| AgentError::unknown_permission(id, request_id))?;
        let option = match &outcome {
            PermissionOutcome::Selected { option_id } => Some(
                request
                    .options
                    .iter()
                    .find(|o| &o.id == option_id)
                    .ok_or_else(|| AgentError::unknown_permission(id, request_id))?,
            ),
            PermissionOutcome::Cancelled => None,
        };
        let remembered = if remember {
            let option = option.ok_or_else(|| {
                AgentError::InvalidParams("only a chosen option can be remembered".into())
            })?;
            let (Some(spec), Some(workspace)) = (&request.suggested_rule, &st.info().workspace)
            else {
                return Err(AgentError::InvalidParams(format!(
                    "permission request {} of session {id} cannot be remembered",
                    request_id.0
                )));
            };
            let decision = rules::decision_of(option.kind);
            Some(inner.store.put_rule(workspace, spec, decision, now())?)
        } else {
            None
        };
        inner.resolve(&s, &mut st, request_id, outcome, remembered)?;
        if st.fold.pending.is_empty() {
            inner.transition(&s, &mut st, Event::PermissionsSettled)?;
        }
        Ok(())
    }

    /// Sets the agent's status (`sessions/setStatus`, the `set_status` MCP
    /// tool): a short line, optional progress and an optional claimed state,
    /// applied as [`crate::state`] says. Returns the session's new info; a
    /// change also goes out on `sessions/changes`. Setting the same status
    /// again changes nothing.
    pub fn set_status(&self, params: SetStatusParams) -> Result<SessionInfo> {
        let text = state::status_line(&params.status);
        if text.is_empty() {
            return Err(AgentError::InvalidParams(
                "the status must not be empty".into(),
            ));
        }
        let progress = params
            .progress
            .map(|p| {
                if p.total == 0 {
                    Err(AgentError::InvalidParams(
                        "progress `total` must be at least 1".into(),
                    ))
                } else {
                    Ok(StatusProgress {
                        current: p.current.min(p.total),
                        total: p.total,
                    })
                }
            })
            .transpose()?;
        let inner = &self.inner;
        let id = &params.session;
        let s = inner.session(id)?;
        let mut st = lock(&s.state);
        if st.info().closed {
            return Err(AgentError::Closed(id.clone()));
        }
        let status = AgentStatus {
            text,
            state: params.state,
            progress,
            turn: st.turn,
            updated_at: now(),
        };
        let event = Event::StatusSet {
            status: status.clone(),
            running: st.turn.is_some(),
        };
        // The same status again, with nothing to change: keep the old one
        // (and its time) and send no change.
        let current = st.tracked();
        let next = state::next(&current, event.clone());
        let repeat = (next.state, &next.reason) == (current.state, &current.reason)
            && current.status.as_ref().is_some_and(|old| {
                AgentStatus {
                    updated_at: old.updated_at,
                    ..status
                } == *old
            });
        if !repeat {
            inner.transition(&s, &mut st, event)?;
        }
        Ok(st.info().clone())
    }

    /// Reports a session's state from a daemon feature, such as
    /// `needs_input` while a question waits for the user (#73), or
    /// `working` once it is answered. Applied like a provider's report
    /// ([`SessionEvents::report_state`], see [`crate::state`]):
    /// `needs_permission` is ignored, and while a permission request waits
    /// only `error` changes the state. Returns the session's new info.
    pub fn report_state(
        &self,
        id: &SessionId,
        state: AgentState,
        reason: Option<String>,
    ) -> Result<SessionInfo> {
        let inner = &self.inner;
        let s = inner.session(id)?;
        let mut st = lock(&s.state);
        if st.info().closed {
            return Err(AgentError::Closed(id.clone()));
        }
        inner.report(&s, &mut st, state, reason)?;
        Ok(st.info().clone())
    }

    /// Workspace `workspace`'s permission rules, oldest first.
    pub fn rules(&self, workspace: &str) -> Result<Vec<PermissionRule>> {
        Ok(self.inner.store.rules(workspace)?)
    }

    /// Deletes a permission rule. Returns whether it existed.
    pub fn delete_rule(&self, id: PermissionRuleId) -> Result<bool> {
        Ok(self.inner.store.delete_rule(id)?)
    }

    /// Switches the session's mode.
    pub async fn set_mode(&self, id: &SessionId, mode_id: String) -> Result<()> {
        let inner = &self.inner;
        let s = inner.session(id)?;
        if let Some(modes) = &lock(&s.state).fold.modes
            && !modes.available_modes.iter().any(|m| m.id == mode_id)
        {
            return Err(AgentError::InvalidParams(format!(
                "session {id} has no mode `{mode_id}`"
            )));
        }
        let live = inner.ensure_live(&s).await?;
        live.set_mode(mode_id.clone()).await?;
        let mut st = lock(&s.state);
        if st.fold.mode() != Some(mode_id.as_str()) {
            inner.record(&s, &mut st, SessionUpdate::CurrentMode { mode_id })?;
        }
        inner.sync_mode(&mut st)
    }

    /// Closes the session, cancelling a running turn. Returns whether it was
    /// open.
    pub async fn close(&self, id: &SessionId) -> Result<bool> {
        let inner = &self.inner;
        let s = inner.session(id)?;
        let _starting = s.live_lock.lock().await;
        let (live, running) = {
            let st = lock(&s.state);
            if st.info().closed {
                return Ok(false);
            }
            (st.live.clone(), st.turn.is_some())
        };
        if let Some(live) = &live {
            if running && let Err(e) = live.cancel().await {
                tracing::warn!(session = %id, error = %e, "cancel on close failed");
            }
            if let Err(e) = live.close().await {
                tracing::warn!(session = %id, error = %e, "provider close failed");
            }
        }
        let mut st = lock(&s.state);
        if st.info().closed {
            return Ok(false);
        }
        inner.cancel_pending(&s, &mut st)?;
        if let Some(turn) = st.turn.take() {
            inner.record(
                &s,
                &mut st,
                SessionUpdate::TurnEnded {
                    turn,
                    stop_reason: StopReason::Cancelled,
                    error: None,
                },
            )?;
        }
        st.live = None;
        inner.transition(&s, &mut st, Event::Closed)?;
        inner.record(&s, &mut st, SessionUpdate::Closed)?;
        st.stored.info.closed = true;
        inner.save_info(&mut st)?;
        inner.update_busy(&s, &mut st);
        Ok(true)
    }

    /// Ends running turns as interrupted and closes their provider sessions;
    /// call when the daemon stops. Their sessions resume on the next prompt
    /// after a restart.
    pub async fn shutdown(&self) {
        let inner = &self.inner;
        let sessions: Vec<_> = lock(&inner.sessions).values().cloned().collect();
        for s in sessions {
            let live = {
                let mut st = lock(&s.state);
                if st.turn.is_none() {
                    continue;
                }
                if let Err(e) = inner.interrupt(&s, &mut st, INTERRUPTED) {
                    tracing::error!(session = %s.id, error = %e, "cannot record an interrupted turn");
                }
                st.live.take()
            };
            if let Some(live) = live {
                let _ = live.cancel().await;
                let _ = live.close().await;
            }
        }
    }

    /// The `sessions/updates` feed of session `id`.
    pub fn updates(
        &self,
        id: &SessionId,
        since: Option<u64>,
    ) -> std::result::Result<EventFeed<SessionUpdate>, ErrorObject> {
        let s = self.inner.session(id)?;
        let (rx, head) = {
            let st = lock(&s.state);
            (s.updates.subscribe(), st.head)
        };
        let store = Arc::clone(&self.inner.store);
        let id = id.clone();
        let page: Page<SessionUpdate> = Box::new(move |after| {
            store
                .updates_since(&id, after, PAGE)
                .map_err(|e| AgentError::from(e).to_error())
        });
        feed(head, since, rx, page, |_| true)
    }

    /// The `sessions/changes` feed, optionally for one workspace.
    pub fn changes(
        &self,
        workspace: Option<String>,
        since: Option<u64>,
    ) -> std::result::Result<EventFeed<SessionInfo>, ErrorObject> {
        let (rx, head) = {
            let head = lock(&self.inner.changes);
            (self.inner.changes_tx.subscribe(), *head)
        };
        let store = Arc::clone(&self.inner.store);
        let ws = workspace.clone();
        let page: Page<SessionInfo> = Box::new(move |after| {
            store
                .changes_since(after, ws.as_deref(), PAGE)
                .map_err(|e| AgentError::from(e).to_error())
        });
        feed(head, since, rx, page, move |info: &SessionInfo| {
            workspace.is_none() || info.workspace == workspace
        })
    }
}
