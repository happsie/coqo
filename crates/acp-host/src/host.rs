// SPDX-License-Identifier: Apache-2.0

//! One workspace's agent: a supervised ACP agent process, the ACP
//! connection over its stdio, and the routing of `session/*` traffic to the
//! Coqo sessions that live on it.
//!
//! - A [`Host`] serves every session of one workspace. It starts an
//!   [`AgentProcess`] when a session needs one, replaces it if it went down
//!   for good, and stops it when its last session closes.
//! - The process runs under the kernel supervisor (own process group,
//!   restart with backoff on failure). Each instance gets a fresh ACP
//!   connection; each connection has a new **generation**, and sessions
//!   reattach (`session/resume`, else `session/load`) when they find a newer
//!   one than they were started on.
//! - Agent → client traffic (updates, permission requests, `fs/*`, auth
//!   status) is routed by ACP session id. Updates for a session that is not
//!   registered yet (they can race the `session/new` response) are held and
//!   delivered on registration.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{Agent, ByteStreams, Client, UntypedMessage};
use coqo_agents::model::{AuthStatus, SessionId};
use coqo_agents::{AgentUpdate, ProviderError, SessionEvents};
use coqo_kernel::{
    ChildEvent, ChildHandle, ChildIo, ChildSpec, ChildStdio, OutputStream, Supervisor,
};
use tokio::sync::watch;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::convert;
use crate::fs::{self, FsHandle};
use crate::provider::{AcpOptions, AgentCommand};

/// Updates held per session that is not registered yet.
const EARLY_PER_SESSION: usize = 256;
/// Unregistered sessions whose updates are held.
const EARLY_SESSIONS: usize = 16;
/// Stderr lines kept for error messages.
const STDERR_LINES: usize = 8;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What to do with a session's updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Replay {
    /// Normal operation.
    Live,
    /// `session/load` is replaying history the session manager may want:
    /// forward it, but announce no edits (they happened long ago).
    Forward,
    /// `session/load` is replaying history Coqo already has: drop it.
    Drop,
}

/// Where one ACP session's traffic goes.
pub(crate) struct Route {
    pub coqo: SessionId,
    pub events: SessionEvents,
    pub workspace: Option<String>,
    pub cwd: PathBuf,
    pub replay: Replay,
    /// The auth status this session last recorded.
    pub auth_sent: Option<AuthStatus>,
}

/// State shared by every process and connection of one host.
pub(crate) struct Shared {
    name: String,
    fs: FsHandle,
    routes: Mutex<HashMap<String, Route>>,
    early: Mutex<HashMap<String, Vec<acp::SessionUpdate>>>,
    auth: Mutex<Option<AuthStatus>>,
    stderr: Mutex<VecDeque<String>>,
    generation: AtomicU64,
}

impl Shared {
    /// Announces `diffs` as expected agent writes.
    fn announce(&self, route: &Route, diffs: &[acp::ToolCallContent]) {
        if route.replay != Replay::Live {
            return;
        }
        let diffs = convert::diffs(diffs);
        if diffs.is_empty() {
            return;
        }
        let Some(fs) = self.fs.get() else { return };
        for (path, old, new) in diffs {
            fs.expect_write(&route.coqo, &path, old, new);
        }
    }

    fn deliver(&self, route: &mut Route, update: &acp::SessionUpdate) {
        if route.replay == Replay::Drop {
            return;
        }
        match update {
            acp::SessionUpdate::ToolCall(tc) => self.announce(route, &tc.content),
            acp::SessionUpdate::ToolCallUpdate(tu) => {
                if let Some(content) = &tu.fields.content {
                    self.announce(route, content);
                }
            }
            _ => {}
        }
        match convert::update(update) {
            Some(u) => route.events.update(u),
            None => {
                tracing::trace!(agent = %self.name, "ignoring a session update Coqo does not show")
            }
        }
    }

    fn on_update(&self, n: acp::SessionNotification) {
        let id = n.session_id.0.to_string();
        let mut routes = lock(&self.routes);
        if let Some(route) = routes.get_mut(&id) {
            self.deliver(route, &n.update);
            return;
        }
        drop(routes);
        let mut early = lock(&self.early);
        if !early.contains_key(&id) && early.len() >= EARLY_SESSIONS {
            tracing::debug!(agent = %self.name, "dropping updates for an unknown session");
            return;
        }
        let held = early.entry(id).or_default();
        if held.len() < EARLY_PER_SESSION {
            held.push(n.update);
        }
    }

    /// Routes `id` to `route`, delivering updates that arrived before.
    pub(crate) fn register(&self, id: &str, mut route: Route) {
        let mut routes = lock(&self.routes);
        let held = lock(&self.early).remove(id).unwrap_or_default();
        for update in &held {
            self.deliver(&mut route, update);
        }
        if route.replay == Replay::Live {
            self.send_auth(&mut route);
        }
        routes.insert(id.to_owned(), route);
    }

    pub(crate) fn set_replay(&self, id: &str, replay: Replay) {
        if let Some(route) = lock(&self.routes).get_mut(id) {
            route.replay = replay;
            if replay == Replay::Live {
                self.send_auth(route);
            }
        }
    }

    pub(crate) fn unregister(&self, id: &str) {
        lock(&self.routes).remove(id);
        lock(&self.early).remove(id);
    }

    pub(crate) fn sessions(&self) -> usize {
        lock(&self.routes).len()
    }

    /// Records the current auth status in `route` if it has not seen it.
    fn send_auth(&self, route: &mut Route) {
        let current = lock(&self.auth).clone();
        if let Some(status) = current
            && route.auth_sent.as_ref() != Some(&status)
        {
            route.events.update(AgentUpdate::AuthStatus(status.clone()));
            route.auth_sent = Some(status);
        }
    }

    /// Brings `id`'s auth status up to date (before a prompt, say: a resumed
    /// session's updates are dropped until the resume returns).
    pub(crate) fn sync_auth(&self, id: &str) {
        if let Some(route) = lock(&self.routes).get_mut(id)
            && route.replay == Replay::Live
        {
            self.send_auth(route);
        }
    }

    fn on_auth(&self, status: AuthStatus) {
        {
            let mut auth = lock(&self.auth);
            if auth.as_ref() == Some(&status) {
                return;
            }
            *auth = Some(status);
        }
        let mut routes = lock(&self.routes);
        for route in routes.values_mut().filter(|r| r.replay == Replay::Live) {
            self.send_auth(route);
        }
    }

    fn on_extension(&self, n: &UntypedMessage) {
        if n.method == "_auth/status_update" {
            if let Some(status) = convert::legacy_auth_status(&n.params) {
                self.on_auth(status);
            }
        } else {
            // Only the method: params may carry anything.
            tracing::debug!(agent = %self.name, method = %n.method, "ignoring an extension notification");
        }
    }

    fn on_permission(
        &self,
        req: acp::RequestPermissionRequest,
        responder: Responder<acp::RequestPermissionResponse>,
        cx: &ConnectionTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let id = req.session_id.0.to_string();
        let answer = {
            let routes = lock(&self.routes);
            routes
                .get(&id)
                .filter(|r| r.replay == Replay::Live)
                .map(|route| {
                    if let Some(content) = &req.tool_call.fields.content {
                        self.announce(route, content);
                    }
                    route.events.request_permission(convert::permission(&req))
                })
        };
        let Some(answer) = answer else {
            return responder.respond(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            ));
        };
        // Waiting for the user must not hold the dispatch loop.
        cx.spawn(async move {
            let outcome = convert::permission_outcome(&req, answer.await);
            if let Err(e) = responder.respond(acp::RequestPermissionResponse::new(outcome)) {
                tracing::debug!(error = %e, "cannot answer a permission request");
            }
            Ok(())
        })
    }

    /// The directories `route`'s session may touch.
    fn roots(&self, route: &Route) -> Vec<PathBuf> {
        let mut roots = vec![route.cwd.clone()];
        if let Some(fs) = self.fs.get() {
            roots.extend(fs.roots(route.workspace.as_deref()));
        }
        roots
    }

    fn route_for(&self, id: &acp::SessionId) -> Result<(SessionId, Vec<PathBuf>), String> {
        let routes = lock(&self.routes);
        let route = routes
            .get(&*id.0)
            .ok_or_else(|| format!("unknown session {}", id.0))?;
        Ok((route.coqo.clone(), self.roots(route)))
    }

    async fn read_text(&self, req: &acp::ReadTextFileRequest) -> Result<String, String> {
        let (_, roots) = self.route_for(&req.session_id)?;
        let path = fs::inside(&req.path, &roots).map_err(|e| e.to_string())?;
        let open = self.fs.get().and_then(|fs| fs.open_text(&path));
        let text = match open {
            Some(text) => text,
            None => tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("cannot read {}: {e}", req.path.display()))?,
        };
        Ok(fs::slice(text, req.line, req.limit))
    }

    async fn write_text(&self, req: &acp::WriteTextFileRequest) -> Result<(), String> {
        let (coqo, roots) = self.route_for(&req.session_id)?;
        let path = fs::inside(&req.path, &roots).map_err(|e| e.to_string())?;
        if let Some(fs) = self.fs.get() {
            if fs.replace_open(&coqo, &path, &req.content) {
                return Ok(());
            }
            fs.expect_write(&coqo, &path, None, req.content.clone());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        tokio::fs::write(&path, &req.content)
            .await
            .map_err(|e| format!("cannot write {}: {e}", req.path.display()))
    }

    fn stderr_tail(&self) -> String {
        let lines = lock(&self.stderr);
        lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// What the agent can do, from `initialize`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AgentCaps {
    pub load: bool,
    pub resume: bool,
    pub close: bool,
}

/// One live ACP connection.
pub(crate) struct Conn {
    pub cx: ConnectionTo<Agent>,
    pub generation: u64,
    pub caps: AgentCaps,
}

#[derive(Clone)]
enum ConnState {
    Starting,
    Ready(Arc<Conn>),
    /// Down for good: this process will not come back.
    Down(String),
}

/// One supervised agent process (with its restarts).
struct AgentProcess {
    state: watch::Receiver<ConnState>,
    stop: CancellationToken,
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl AgentProcess {
    fn spawn(
        supervisor: &Supervisor,
        shared: &Arc<Shared>,
        options: &AcpOptions,
        command: AgentCommand,
        cwd: &Path,
    ) -> Result<Self, ProviderError> {
        let mut spec = ChildSpec::new(
            options.name.clone(),
            command.program.clone().into_os_string(),
        )
        .args(command.args.clone())
        .current_dir(cwd)
        .stdio(ChildStdio::Protocol)
        .restart(options.restart);
        for key in &command.env_remove {
            spec = spec.env_remove(key.clone());
        }
        for (key, value) in &command.env {
            spec = spec.env(key.clone(), value.clone());
        }
        let child = supervisor.spawn(spec).map_err(|e| {
            ProviderError::Failed(format!(
                "cannot start {} ({}): {e}",
                options.name,
                command.program.display()
            ))
        })?;
        let (tx, rx) = watch::channel(ConnState::Starting);
        let stop = CancellationToken::new();
        tokio::spawn(run(
            child,
            Arc::clone(shared),
            tx,
            stop.clone(),
            options.clone(),
        ));
        Ok(Self { state: rx, stop })
    }

    fn is_down(&self) -> bool {
        matches!(*self.state.borrow(), ConnState::Down(_))
    }

    /// The live connection, waiting up to `timeout` for one.
    async fn connection(&self, timeout: Duration) -> Result<Arc<Conn>, ProviderError> {
        let mut state = self.state.clone();
        let ready = tokio::time::timeout(
            timeout,
            state.wait_for(|s| !matches!(s, ConnState::Starting)),
        )
        .await;
        match ready {
            Err(_) => Err(ProviderError::Failed(format!(
                "the agent did not start within {timeout:?}"
            ))),
            Ok(Err(_)) => Err(ProviderError::Failed("the agent stopped".into())),
            Ok(Ok(s)) => match &*s {
                ConnState::Ready(conn) => Ok(Arc::clone(conn)),
                ConnState::Down(why) => Err(ProviderError::Failed(why.clone())),
                ConnState::Starting => unreachable!(),
            },
        }
    }
}

/// Supervises one process: a connection per instance, until it is down
/// for good or stopped.
async fn run(
    mut child: ChildHandle,
    shared: Arc<Shared>,
    state: watch::Sender<ConnState>,
    stop: CancellationToken,
    options: AcpOptions,
) {
    let mut events = child.subscribe();
    let log = Arc::clone(&shared);
    let log_stop = stop.clone();
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                e = events.recv() => e,
                () = log_stop.cancelled() => return,
            };
            match event {
                Ok(ChildEvent::Output(line)) if line.stream == OutputStream::Stderr => {
                    tracing::debug!(target: "coqo_acp_host::agent_stderr", agent = %log.name, "{}", line.line);
                    let mut tail = lock(&log.stderr);
                    if tail.len() >= STDERR_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line.line);
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    loop {
        let io = tokio::select! {
            io = child.next_io() => io,
            () = stop.cancelled() => break,
        };
        let Some(io) = io else {
            let tail = shared.stderr_tail();
            let mut why = format!("{} exited", options.name);
            if let Some(exit) = child.status().last_exit {
                why.push_str(&format!(" ({exit})"));
            }
            if !tail.is_empty() {
                why.push_str(&format!(": {tail}"));
            }
            tracing::info!(agent = %options.name, "{why}");
            state.send_replace(ConnState::Down(why));
            return;
        };
        let generation = shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
        state.send_replace(ConnState::Starting);
        tokio::select! {
            result = connect(io, generation, Arc::clone(&shared), &state, &options) => {
                if let Err(e) = result {
                    tracing::info!(agent = %options.name, error = %e, "the agent connection ended");
                }
                // Until the supervisor hands over the next instance (or
                // gives up), there is no connection.
                state.send_replace(ConnState::Starting);
            }
            () = stop.cancelled() => break,
        }
    }
    // Stopped on purpose (last session closed, provider dropped). Agents
    // may exit with an error status when their connection closes; that is
    // expected here.
    let _ = child.stop().await;
    state.send_replace(ConnState::Down(format!("{} was stopped", options.name)));
}

/// Serves one ACP connection over an instance's stdio until it closes.
async fn connect(
    io: ChildIo,
    generation: u64,
    shared: Arc<Shared>,
    state: &watch::Sender<ConnState>,
    options: &AcpOptions,
) -> Result<(), agent_client_protocol::Error> {
    let transport = ByteStreams::new(io.stdin.compat_write(), io.stdout.compat());
    let (on_update, on_auth, on_ext, on_permission, on_read, on_write) = (
        Arc::clone(&shared),
        Arc::clone(&shared),
        Arc::clone(&shared),
        Arc::clone(&shared),
        Arc::clone(&shared),
        Arc::clone(&shared),
    );
    let state = state.clone();
    let client_name = options.client_name.clone();
    let client_version = options.client_version.clone();
    let start_timeout = options.start_timeout;
    Client
        .v1()
        .name("coqo")
        .on_receive_notification(
            async move |n: acp::SessionNotification, _cx| {
                on_update.on_update(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |n: acp::AuthStatusUpdateNotification, _cx| {
                on_auth.on_auth(convert::auth_status(&n));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |n: UntypedMessage, _cx| {
                on_ext.on_extension(&n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: acp::RequestPermissionRequest, responder, cx| {
                on_permission.on_permission(req, responder, &cx)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::ReadTextFileRequest, responder, cx| {
                let shared = Arc::clone(&on_read);
                cx.spawn(async move {
                    let result = match shared.read_text(&req).await {
                        Ok(text) => responder.respond(acp::ReadTextFileResponse::new(text)),
                        Err(e) => responder.respond_with_error(fs_error(e)),
                    };
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "cannot answer fs/read_text_file");
                    }
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::WriteTextFileRequest, responder, cx| {
                let shared = Arc::clone(&on_write);
                cx.spawn(async move {
                    let result = match shared.write_text(&req).await {
                        Ok(()) => responder.respond(acp::WriteTextFileResponse::new()),
                        Err(e) => responder.respond_with_error(fs_error(e)),
                    };
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "cannot answer fs/write_text_file");
                    }
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            let init = tokio::time::timeout(
                start_timeout,
                cx.send_request(
                    acp::InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(
                            acp::ClientCapabilities::new()
                                .fs(acp::FileSystemCapabilities::new()
                                    .read_text_file(true)
                                    .write_text_file(true))
                                .plan(acp::PlanCapabilities::new()),
                        )
                        .client_info(acp::Implementation::new(client_name, client_version)),
                )
                .block_task(),
            )
            .await
            .map_err(|_| {
                agent_client_protocol::Error::internal_error().data(format!(
                    "initialize did not answer within {start_timeout:?}"
                ))
            })??;
            let caps = &init.agent_capabilities;
            let conn = Arc::new(Conn {
                cx: cx.clone(),
                generation,
                caps: AgentCaps {
                    load: caps.load_session,
                    resume: caps.session_capabilities.resume.is_some(),
                    close: caps.session_capabilities.close.is_some(),
                },
            });
            tracing::debug!(generation, caps = ?conn.caps, "agent initialized");
            state.send_replace(ConnState::Ready(conn));
            cx.incoming_closed().await;
            Ok(())
        })
        .await
}

fn fs_error(message: String) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message)
}

/// Every session of one workspace, and the agent process serving them.
pub(crate) struct Host {
    pub shared: Arc<Shared>,
    current: tokio::sync::Mutex<Option<AgentProcess>>,
    /// The current process's connection state, readable without waiting.
    state: Mutex<Option<watch::Receiver<ConnState>>>,
    /// Sessions being started or resumed, which keep the process alive.
    starting: AtomicUsize,
}

/// Keeps a [`Host`]'s process alive while a session starts.
pub(crate) struct Starting<'a>(&'a Host);

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        self.0.starting.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Host {
    pub(crate) fn new(name: String, fs: FsHandle) -> Self {
        Self {
            shared: Arc::new(Shared {
                name,
                fs,
                routes: Mutex::default(),
                early: Mutex::default(),
                auth: Mutex::default(),
                stderr: Mutex::default(),
                generation: AtomicU64::new(0),
            }),
            current: tokio::sync::Mutex::new(None),
            state: Mutex::default(),
            starting: AtomicUsize::new(0),
        }
    }

    pub(crate) fn starting(&self) -> Starting<'_> {
        self.starting.fetch_add(1, Ordering::SeqCst);
        Starting(self)
    }

    /// The live connection, starting the agent if none runs.
    pub(crate) async fn connection(
        &self,
        supervisor: &Supervisor,
        options: &AcpOptions,
        command: impl FnOnce() -> Result<AgentCommand, ProviderError>,
        cwd: &Path,
    ) -> Result<Arc<Conn>, ProviderError> {
        let mut current = self.current.lock().await;
        if current.as_ref().is_none_or(AgentProcess::is_down) {
            lock(&self.shared.stderr).clear();
            let process = AgentProcess::spawn(supervisor, &self.shared, options, command()?, cwd)?;
            *lock(&self.state) = Some(process.state.clone());
            *current = Some(process);
        }
        let process = current.as_ref().expect("just set");
        process.connection(options.start_timeout).await
    }

    /// The live connection, if there is one right now. Never starts an
    /// agent and never waits.
    pub(crate) fn live(&self) -> Option<Arc<Conn>> {
        let state = lock(&self.state);
        match &*state.as_ref()?.borrow() {
            ConnState::Ready(conn) => Some(Arc::clone(conn)),
            _ => None,
        }
    }

    /// Stops the agent if no session uses it.
    pub(crate) async fn release_if_idle(&self) {
        let mut current = self.current.lock().await;
        if self.shared.sessions() == 0 && self.starting.load(Ordering::SeqCst) == 0 {
            lock(&self.state).take();
            if current.take().is_some() {
                tracing::debug!(agent = %self.shared.name, "no sessions left; stopping the agent");
            }
        }
    }
}
