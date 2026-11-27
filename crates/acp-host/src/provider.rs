// SPDX-License-Identifier: Apache-2.0

//! [`AcpProvider`]: an [`AgentProvider`] over any ACP agent command.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use agent_client_protocol::v1::schema as acp;
use coqo_agents::model::{ContentBlock, ProviderCapabilities, StopReason};
use coqo_agents::{
    AgentProvider, AgentUpdate, ProviderError, ProviderSession, ResumeRequest, SessionContext,
    SessionEvents,
};
use coqo_kernel::{RestartPolicy, Supervisor};
use coqo_mod_api::BoxFuture;

use crate::convert;
use crate::fs::FsHandle;
use crate::host::{Conn, Host, Replay, Route};
use crate::terminal::TerminalHandle;

/// How to start an agent process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    /// The executable.
    pub program: PathBuf,
    /// Its arguments. **No secrets** (other local users can read argv).
    pub args: Vec<OsString>,
    /// Extra environment variables.
    pub env: Vec<(OsString, OsString)>,
    /// Variables of the daemon's environment not to pass on (applied
    /// before `env`).
    pub env_remove: Vec<OsString>,
}

impl AgentCommand {
    /// Runs `program` with no arguments in the daemon's environment.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
        }
    }

    /// Appends an argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Sets an environment variable.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Does not pass the daemon's variable `key` on.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env_remove.push(key.into());
        self
    }
}

/// Builds the command for a new agent process; called each time one
/// starts, so a changed setting takes effect with the next process. An
/// error (say, the agent is not installed) fails the session start with it.
pub trait Launcher: Send + Sync + 'static {
    /// The command for an agent process whose first session runs in `cwd`.
    fn command(&self, cwd: &Path) -> Result<AgentCommand, ProviderError>;
}

impl<F> Launcher for F
where
    F: Fn(&Path) -> Result<AgentCommand, ProviderError> + Send + Sync + 'static,
{
    fn command(&self, cwd: &Path) -> Result<AgentCommand, ProviderError> {
        self(cwd)
    }
}

/// Settings of an [`AcpProvider`].
#[derive(Clone, Debug)]
pub struct AcpOptions {
    /// Name for logs and the supervisor, such as `claude-agent-acp`.
    pub name: String,
    /// What the provider reports in `agents/providers`.
    pub capabilities: ProviderCapabilities,
    /// Where `fs/*` and announced edits go.
    pub fs: FsHandle,
    /// Where `terminal/*` commands run. Offered to agents (the `terminal`
    /// client capability) only if set when they connect.
    pub terminals: TerminalHandle,
    /// Restarts of a crashed agent process.
    pub restart: RestartPolicy,
    /// How long the agent may take to start and answer `initialize`.
    pub start_timeout: Duration,
    /// How long `session/new`, `session/resume`, `session/load` and
    /// `session/set_mode` may take.
    pub request_timeout: Duration,
    /// `clientInfo.name` sent in `initialize`.
    pub client_name: String,
    /// `clientInfo.version`.
    pub client_version: String,
}

impl AcpOptions {
    /// Defaults for an agent called `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            capabilities: ProviderCapabilities {
                resume: true,
                modes: true,
                images: true,
                embedded_context: true,
            },
            fs: FsHandle::new(),
            terminals: TerminalHandle::new(),
            restart: RestartPolicy::default(),
            start_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(120),
            client_name: "coqo".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

struct Inner {
    launcher: Box<dyn Launcher>,
    options: AcpOptions,
    supervisor: Supervisor,
    hosts: Mutex<HashMap<String, Arc<Host>>>,
}

impl Inner {
    /// The host for `cx`'s workspace (or, without one, its directory).
    fn host(&self, cx: &SessionContext) -> Arc<Host> {
        let key = cx
            .workspace
            .clone()
            .unwrap_or_else(|| cx.cwd.display().to_string());
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(hosts.entry(key).or_insert_with(|| {
            Arc::new(Host::new(
                self.options.name.clone(),
                self.options.fs.clone(),
                self.options.terminals.clone(),
            ))
        }))
    }

    async fn connection(&self, host: &Host, cwd: &Path) -> Result<Arc<Conn>, ProviderError> {
        host.connection(
            &self.supervisor,
            &self.options,
            || self.launcher.command(cwd),
            cwd,
        )
        .await
    }

    async fn request<R>(
        &self,
        what: &str,
        call: impl Future<Output = Result<R, agent_client_protocol::Error>>,
    ) -> Result<R, ProviderError> {
        match tokio::time::timeout(self.options.request_timeout, call).await {
            Ok(result) => result.map_err(provider_error),
            Err(_) => Err(ProviderError::Failed(format!(
                "the agent did not answer {what} within {:?}",
                self.options.request_timeout
            ))),
        }
    }

    /// Reattaches ACP session `id` on `conn`: `session/resume` if the agent
    /// has it, else `session/load` (whose replay `replay` decides about).
    async fn reattach(
        &self,
        host: &Host,
        conn: &Conn,
        id: &str,
        cx: &SessionContext,
        events: &SessionEvents,
        replay: Replay,
    ) -> Result<Option<acp::SessionModeState>, ProviderError> {
        let route = |replay| Route {
            coqo: cx.session.clone(),
            events: events.clone(),
            workspace: cx.workspace.clone(),
            cwd: cx.cwd.clone(),
            replay,
            auth_sent: None,
        };
        let session_id = acp::SessionId::new(id);
        let mcp: Vec<acp::McpServer> = cx.mcp_servers.iter().map(convert::to_acp_mcp).collect();
        let mut failure = None;
        if conn.caps.resume {
            host.shared.register(id, route(Replay::Live));
            let resumed = self
                .request(
                    "session/resume",
                    conn.cx
                        .send_request(
                            acp::ResumeSessionRequest::new(session_id.clone(), cx.cwd.clone())
                                .mcp_servers(mcp.clone()),
                        )
                        .block_task(),
                )
                .await;
            match resumed {
                Ok(r) => return Ok(r.modes),
                Err(e @ ProviderError::AuthRequired(_)) => {
                    host.shared.unregister(id);
                    return Err(e);
                }
                Err(e) => {
                    tracing::info!(agent = %self.options.name, error = %e, "session/resume failed");
                    failure = Some(e);
                }
            }
        }
        if !conn.caps.load {
            host.shared.unregister(id);
            return Err(failure.unwrap_or_else(|| {
                ProviderError::Unsupported("the agent cannot resume sessions".into())
            }));
        }
        host.shared.register(id, route(replay));
        let loaded = self
            .request(
                "session/load",
                conn.cx
                    .send_request(
                        acp::LoadSessionRequest::new(session_id, cx.cwd.clone()).mcp_servers(mcp),
                    )
                    .block_task(),
            )
            .await;
        match loaded {
            Ok(r) => {
                host.shared.set_replay(id, Replay::Live);
                Ok(r.modes)
            }
            Err(e) => {
                host.shared.unregister(id);
                Err(e)
            }
        }
    }
}

/// The error for a failed ACP request.
fn provider_error(e: agent_client_protocol::Error) -> ProviderError {
    if agent_client_protocol::is_connection_terminated(&e)
        || agent_client_protocol::is_incoming_transport_closed(&e)
    {
        return ProviderError::Failed("the agent process exited".into());
    }
    let detail = match &e.data {
        Some(serde_json::Value::String(s)) => format!(" ({s})"),
        Some(serde_json::Value::Object(o)) => o
            .get("reason")
            .or_else(|| o.get("message"))
            .and_then(serde_json::Value::as_str)
            .map(|s| format!(" ({s})"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let message = format!("{}{detail}", e.message);
    if e.code == acp::ErrorCode::AuthRequired {
        ProviderError::AuthRequired(message)
    } else {
        ProviderError::Failed(message)
    }
}

/// An [`AgentProvider`] that runs an ACP agent: one process per workspace,
/// supervised by the kernel, with every session of that workspace on it.
/// Cheap to clone.
#[derive(Clone)]
pub struct AcpProvider {
    inner: Arc<Inner>,
}

impl fmt::Debug for AcpProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcpProvider")
            .field("name", &self.inner.options.name)
            .finish_non_exhaustive()
    }
}

impl AcpProvider {
    /// A provider that starts agents with `launcher`.
    pub fn new(launcher: impl Launcher, options: AcpOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                launcher: Box::new(launcher),
                options,
                supervisor: Supervisor::new(),
                hosts: Mutex::default(),
            }),
        }
    }

    /// A provider that runs `command` for every workspace.
    pub fn command(command: AgentCommand, options: AcpOptions) -> Self {
        Self::new(move |_: &Path| Ok(command.clone()), options)
    }
}

impl AgentProvider for AcpProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.options.capabilities
    }

    fn start_session(
        &self,
        cx: SessionContext,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>> {
        Box::pin(async move {
            let inner = &self.inner;
            let host = inner.host(&cx);
            let _starting = host.starting();
            let conn = inner.connection(&host, &cx.cwd).await?;
            let mcp: Vec<acp::McpServer> = cx.mcp_servers.iter().map(convert::to_acp_mcp).collect();
            let created = inner
                .request(
                    "session/new",
                    conn.cx
                        .send_request(acp::NewSessionRequest::new(cx.cwd.clone()).mcp_servers(mcp))
                        .block_task(),
                )
                .await;
            let created = match created {
                Ok(created) => created,
                Err(e) => {
                    drop(_starting);
                    host.release_if_idle().await;
                    return Err(e);
                }
            };
            let id = created.session_id.0.to_string();
            if let Some(modes) = &created.modes {
                events.update(AgentUpdate::Modes(convert::modes(modes)));
            }
            host.shared.register(
                &id,
                Route {
                    coqo: cx.session.clone(),
                    events: events.clone(),
                    workspace: cx.workspace.clone(),
                    cwd: cx.cwd.clone(),
                    replay: Replay::Live,
                    auth_sent: None,
                },
            );
            Ok(Arc::new(AcpSession::new(
                Arc::clone(inner),
                Arc::clone(&host),
                id,
                cx,
                events,
                conn.generation,
            )) as Arc<dyn ProviderSession>)
        })
    }

    fn resume_session(
        &self,
        request: ResumeRequest,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>> {
        Box::pin(async move {
            let inner = &self.inner;
            let cx = request.context;
            let id = request.provider_session;
            let host = inner.host(&cx);
            let starting = host.starting();
            let conn = inner.connection(&host, &cx.cwd).await?;
            match inner
                .reattach(&host, &conn, &id, &cx, &events, Replay::Forward)
                .await
            {
                Ok(modes) => {
                    if let Some(modes) = &modes {
                        events.update(AgentUpdate::Modes(convert::modes(modes)));
                    }
                }
                Err(e) => {
                    drop(starting);
                    host.release_if_idle().await;
                    return Err(e);
                }
            }
            Ok(Arc::new(AcpSession::new(
                Arc::clone(inner),
                Arc::clone(&host),
                id,
                cx,
                events,
                conn.generation,
            )) as Arc<dyn ProviderSession>)
        })
    }
}

/// One ACP session.
struct AcpSession {
    inner: Arc<Inner>,
    host: Arc<Host>,
    id: String,
    cx: SessionContext,
    events: SessionEvents,
    /// The connection generation the session is attached to.
    attached: AtomicU64,
    /// Serializes reattaching after an agent restart.
    reattach: tokio::sync::Mutex<()>,
    cancelled: AtomicBool,
    closed: AtomicBool,
}

impl AcpSession {
    fn new(
        inner: Arc<Inner>,
        host: Arc<Host>,
        id: String,
        cx: SessionContext,
        events: SessionEvents,
        generation: u64,
    ) -> Self {
        Self {
            inner,
            host,
            id,
            cx,
            events,
            attached: AtomicU64::new(generation),
            reattach: tokio::sync::Mutex::new(()),
            cancelled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    fn session_id(&self) -> acp::SessionId {
        acp::SessionId::new(self.id.clone())
    }

    /// The connection, reattaching the session if the agent restarted
    /// since it was last used.
    async fn conn(&self) -> Result<Arc<Conn>, ProviderError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ProviderError::Failed("the session is closed".into()));
        }
        let conn = self.inner.connection(&self.host, &self.cx.cwd).await?;
        if conn.generation == self.attached.load(Ordering::SeqCst) {
            return Ok(conn);
        }
        let _reattaching = self.reattach.lock().await;
        if conn.generation != self.attached.load(Ordering::SeqCst) {
            tracing::info!(agent = %self.inner.options.name, session = %self.cx.session, "the agent restarted; reattaching the session");
            self.inner
                .reattach(
                    &self.host,
                    &conn,
                    &self.id,
                    &self.cx,
                    &self.events,
                    Replay::Drop,
                )
                .await?;
            self.attached.store(conn.generation, Ordering::SeqCst);
        }
        Ok(conn)
    }

    /// The connection the session is attached to, if it is up right now
    /// (for cancel and close, which must never start an agent).
    fn current_conn(&self) -> Option<Arc<Conn>> {
        self.host
            .live()
            .filter(|conn| conn.generation == self.attached.load(Ordering::SeqCst))
    }
}

impl ProviderSession for AcpSession {
    fn provider_session_id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    fn prompt(
        &self,
        content: Vec<ContentBlock>,
    ) -> BoxFuture<'_, Result<StopReason, ProviderError>> {
        Box::pin(async move {
            self.cancelled.store(false, Ordering::SeqCst);
            let conn = self.conn().await?;
            self.host.shared.sync_auth(&self.id);
            let prompt = acp::PromptRequest::new(
                self.session_id(),
                content.into_iter().map(convert::to_acp_content).collect(),
            );
            let result = conn.cx.send_request(prompt).block_task().await;
            let cancelled = self.cancelled.load(Ordering::SeqCst);
            match result {
                _ if cancelled => Ok(StopReason::Cancelled),
                Ok(r) => Ok(convert::stop_reason(&r.stop_reason)),
                Err(e) => Err(provider_error(e)),
            }
        })
    }

    fn cancel(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(async move {
            self.cancelled.store(true, Ordering::SeqCst);
            if let Some(conn) = self.current_conn() {
                conn.cx
                    .send_notification(acp::CancelNotification::new(self.session_id()))
                    .map_err(provider_error)?;
            }
            Ok(())
        })
    }

    fn set_mode(&self, mode_id: String) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(async move {
            let conn = self.conn().await?;
            self.inner
                .request(
                    "session/set_mode",
                    conn.cx
                        .send_request(acp::SetSessionModeRequest::new(self.session_id(), mode_id))
                        .block_task(),
                )
                .await?;
            Ok(())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(async move {
            if self.closed.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(conn) = self.current_conn()
                && conn.caps.close
            {
                let closed = tokio::time::timeout(
                    Duration::from_secs(5),
                    conn.cx
                        .send_request(acp::CloseSessionRequest::new(self.session_id()))
                        .block_task(),
                )
                .await;
                if !matches!(closed, Ok(Ok(_))) {
                    tracing::debug!(agent = %self.inner.options.name, "session/close failed");
                }
            }
            self.host.shared.unregister(&self.id);
            self.host.release_if_idle().await;
            Ok(())
        })
    }
}
