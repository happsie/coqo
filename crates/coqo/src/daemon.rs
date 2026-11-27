// SPDX-License-Identifier: Apache-2.0

//! The UI's connection to `coqod` (PLAN §4.3, ADR 0007).
//!
//! GPUI isn't tokio, and the protocol client is. [`Daemon`] owns a small
//! tokio runtime on background threads and runs every request there; the UI
//! awaits the results inside GPUI tasks (`cx.spawn`), so the UI thread never
//! blocks. One [`Daemon`] entity per app lives in a global:
//!
//! ```ignore
//! let daemon = Daemon::global(cx).expect("daemon::init ran");
//! // Requests: an `impl Future` that is `Send`, awaited from a GPUI task.
//! let list = daemon.read(cx).request::<fs::ListDir>(params);
//! cx.spawn(async move |this, cx| {
//!     let entries = list.await?;
//!     this.update(cx, |this, cx| this.show(entries, cx))
//! })
//! .detach();
//! // Topics: a stream of events, ended when the connection drops.
//! let changes = daemon.read(cx).subscribe::<fs::Changes>(params);
//! // React to (re)connects, e.g. to re-list after the daemon restarted.
//! cx.subscribe(&daemon, |this, _, event: &DaemonEvent, cx| { … });
//! ```
//!
//! On start the daemon is attached to, or spawned (`connect_or_spawn`).
//! When the connection drops (the daemon crashed or was shut down), the
//! daemon reconnects once on its own, spawning a new `coqod`; if that fails
//! it waits for [`Daemon::retry`] (the `daemon-status` pill's Retry button).

use std::{future::Future, sync::Arc, time::Duration};

use coqo_client::{ConnectError, ConnectOptions, DaemonPaths, connect, connect_or_spawn};
use coqo_protocol::{Client, ClientConfig, ErrorObject, Method, ProtocolError, Topic};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        App, AppContext as _, Context, Entity, EventEmitter, Global, InteractiveElement as _,
        IntoElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _, Task,
        div, px,
    },
    slots::{Slot, Slots},
    widgets::{Icon, TestSupportExt as _, icon, status_dot},
};
use futures_util::StreamExt as _;
use tokio::{runtime::Runtime, sync::mpsc};

/// The build id this UI announces in the handshake: `$COQO_BUILD` at compile
/// time (release builds), else the version.
pub const BUILD: &str = match option_env!("COQO_BUILD") {
    Some(build) => build,
    None => env!("CARGO_PKG_VERSION"),
};

/// How the UI reaches the daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// The runtime directory (socket and lock).
    pub paths: DaemonPaths,
    /// Handshake identity and how to spawn `coqod`.
    pub options: ConnectOptions,
    /// Start `coqod` when it isn't running. Tests that serve a daemon
    /// in-process turn this off.
    pub spawn: bool,
}

impl DaemonConfig {
    /// The app's defaults: `$COQO_RUNTIME_DIR` or the platform runtime
    /// directory, and `coqod` from `$COQO_DAEMON`, next to `coqo`, or `PATH`.
    pub fn from_env() -> Result<Self, ConnectError> {
        Ok(Self::at(DaemonPaths::from_env()?))
    }

    /// Connect (and spawn) at `paths` with the default options.
    pub fn at(paths: DaemonPaths) -> Self {
        Self {
            paths,
            options: ConnectOptions::new(ClientConfig::new("coqo-ui", BUILD)),
            spawn: true,
        }
    }
}

/// Where the connection is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Attaching, or starting `coqod` and waiting for its socket.
    Connecting,
    /// Attached.
    Connected {
        /// The daemon's build id.
        build: String,
        /// Whether this UI started the daemon.
        spawned: bool,
    },
    /// Not attached; [`Daemon::retry`] tries again.
    Disconnected {
        /// Why, for the status pill's tooltip and the log.
        error: String,
    },
}

/// Emitted by the [`Daemon`] entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonEvent {
    /// A connection is up (the first one, or a new one after a drop).
    /// Re-fetch whatever you show: live topics don't replay across it.
    Connected,
    /// The connection dropped or an attempt failed.
    Disconnected,
}

/// Why a request didn't produce a result.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonError {
    /// No connection right now.
    NotConnected,
    /// The daemon answered with an error (codes in
    /// `coqo_protocol::methods::codes`).
    Rpc(ErrorObject),
    /// The connection or the protocol failed.
    Protocol(String),
}

impl DaemonError {
    /// The application error code, if the daemon answered with an error.
    pub fn code(&self) -> Option<i64> {
        match self {
            DaemonError::Rpc(e) => Some(e.code),
            _ => None,
        }
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::NotConnected => f.write_str("not connected to coqod"),
            DaemonError::Rpc(e) => f.write_str(&e.message),
            DaemonError::Protocol(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<ProtocolError> for DaemonError {
    fn from(e: ProtocolError) -> Self {
        match e {
            ProtocolError::Rpc(e) => DaemonError::Rpc(e),
            ProtocolError::Closed => DaemonError::NotConnected,
            other => DaemonError::Protocol(other.to_string()),
        }
    }
}

/// A live topic subscription: events in order, then `None` when the
/// subscription or the connection ends. Dropping it unsubscribes.
#[derive(Debug)]
pub struct EventStream<E> {
    rx: mpsc::UnboundedReceiver<Result<E, DaemonError>>,
}

impl<E> EventStream<E> {
    /// The next event; `None` once the stream has ended.
    pub async fn next(&mut self) -> Option<Result<E, DaemonError>> {
        self.rx.recv().await
    }
}

/// The UI's daemon connection. See the module docs.
pub struct Daemon {
    /// Kept in an `Option` so dropping can shut it down without blocking.
    runtime: Option<Arc<Runtime>>,
    config: DaemonConfig,
    state: ConnectionState,
    client: Option<Client>,
    /// Bumped per attempt, so a stale attempt's result is ignored.
    generation: u64,
    /// Reconnect once by itself after a drop, not after a failed attempt.
    auto_reconnect: bool,
    _connecting: Option<Task<()>>,
    _watching: Option<Task<()>>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("paths", &self.config.paths)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<DaemonEvent> for Daemon {}

struct GlobalDaemon(Entity<Daemon>);

impl Global for GlobalDaemon {}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take().and_then(|rt| Arc::try_unwrap(rt).ok()) {
            runtime.shutdown_background();
        }
    }
}

/// Create the app's [`Daemon`], start connecting, and register the
/// `daemon-status` top-bar pill. Call once after `coqo_ui::init`.
pub fn init(config: DaemonConfig, cx: &mut App) -> Entity<Daemon> {
    let daemon = cx.new(|cx| Daemon::new(config, cx));
    cx.set_global(GlobalDaemon(daemon.clone()));
    register_status_slot(cx);
    daemon
}

impl Daemon {
    /// A daemon connection that starts connecting right away.
    pub fn new(config: DaemonConfig, cx: &mut Context<Self>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("coqo-daemon-io")
            .enable_all()
            .build()
            .expect("start the daemon connection's runtime");
        let mut this = Self {
            runtime: Some(Arc::new(runtime)),
            config,
            state: ConnectionState::Connecting,
            client: None,
            generation: 0,
            auto_reconnect: false,
            _connecting: None,
            _watching: None,
        };
        this.connect(cx);
        this
    }

    /// The app's daemon, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<Daemon>> {
        cx.try_global::<GlobalDaemon>().map(|g| g.0.clone())
    }

    /// Where the connection is.
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// Whether a connection is up.
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    fn runtime(&self) -> &Runtime {
        self.runtime.as_deref().expect("runtime lives until drop")
    }

    /// Try to connect again (after a failure). No-op while connected or
    /// connecting.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ConnectionState::Disconnected { .. }) {
            self.connect(cx);
        }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.client = None;
        self._watching = None;
        self.state = ConnectionState::Connecting;
        cx.notify();

        let paths = self.config.paths.clone();
        let options = self.config.options.clone();
        let spawn = self.config.spawn;
        let attempt = self.runtime().spawn(async move {
            let (client, notifications, spawned) = if spawn {
                let c = connect_or_spawn(&paths, &options).await?;
                (c.client, c.notifications, c.spawned)
            } else {
                let (client, notifications) = connect(&paths, options.client.clone()).await?;
                (client, notifications, false)
            };
            // Nothing reads plain notifications yet; drain them so the
            // connection's buffer stays empty.
            tokio::spawn(notifications.for_each(|_| async {}));
            Ok::<_, ConnectError>((client, spawned))
        });
        self._connecting = Some(cx.spawn(async move |this, cx| {
            let result = match attempt.await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(e) => Err(format!("connecting failed: {e}")),
            };
            this.update(cx, |this, cx| this.connected(generation, result, cx))
                .ok();
        }));
    }

    fn connected(
        &mut self,
        generation: u64,
        result: Result<(Client, bool), String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self._connecting = None;
        match result {
            Ok((client, spawned)) => {
                self.state = ConnectionState::Connected {
                    build: client.welcome().build.clone(),
                    spawned,
                };
                let watched = client.clone();
                let closed = self.runtime().spawn(async move { watched.closed().await });
                self._watching = Some(cx.spawn(async move |this, cx| {
                    let _ = closed.await;
                    this.update(cx, |this, cx| this.lost(generation, cx)).ok();
                }));
                self.client = Some(client);
                self.auto_reconnect = true;
                cx.emit(DaemonEvent::Connected);
            }
            Err(error) => {
                eprintln!("coqo: cannot reach coqod: {error}");
                self.state = ConnectionState::Disconnected { error };
                cx.emit(DaemonEvent::Disconnected);
            }
        }
        cx.notify();
    }

    fn lost(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        self.client = None;
        self._watching = None;
        self.state = ConnectionState::Disconnected {
            error: "the connection to coqod closed".into(),
        };
        cx.emit(DaemonEvent::Disconnected);
        cx.notify();
        if std::mem::take(&mut self.auto_reconnect) {
            // Give a daemon that is shutting down a moment to let go of the
            // socket and lock before we start another.
            let delay = cx.background_executor().timer(Duration::from_millis(250));
            self._connecting = Some(cx.spawn(async move |this, cx| {
                delay.await;
                this.update(cx, |this, cx| {
                    if generation == this.generation {
                        this.connect(cx);
                    }
                })
                .ok();
            }));
        }
    }

    /// Call `M` on the daemon. The future is `Send` and runs the request on
    /// the connection's runtime; await it from a GPUI task.
    pub fn request<M: Method>(
        &self,
        params: M::Params,
    ) -> impl Future<Output = Result<M::Result, DaemonError>> + Send + 'static + use<M> {
        let call = self.client.clone().map(|client| {
            self.runtime()
                .spawn(async move { client.request::<M>(params).await })
        });
        async move {
            match call {
                None => Err(DaemonError::NotConnected),
                Some(call) => match call.await {
                    Ok(result) => result.map_err(DaemonError::from),
                    Err(e) => Err(DaemonError::Protocol(e.to_string())),
                },
            }
        }
    }

    /// Subscribe to topic `T` from now on (no replay).
    pub fn subscribe<T: Topic>(
        &self,
        params: T::Params,
    ) -> impl Future<Output = Result<EventStream<T::Event>, DaemonError>> + Send + 'static + use<T>
    {
        let call = self.client.clone().map(|client| {
            self.runtime().spawn(async move {
                let mut subscription = client.subscribe::<T>(params, None).await?;
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            item = subscription.next() => {
                                let Some(item) = item else { break };
                                let item = item.map(|e| e.event).map_err(DaemonError::from);
                                if tx.send(item).is_err() {
                                    break;
                                }
                            }
                            () = tx.closed() => break,
                        }
                    }
                });
                Ok::<_, ProtocolError>(EventStream { rx })
            })
        });
        async move {
            match call {
                None => Err(DaemonError::NotConnected),
                Some(call) => match call.await {
                    Ok(result) => result.map_err(DaemonError::from),
                    Err(e) => Err(DaemonError::Protocol(e.to_string())),
                },
            }
        }
    }
}

/// The top bar's `daemon-status` pill: connecting, connected (sage dot), or
/// not connected with a Retry button.
pub(crate) fn register_status_slot(cx: &mut App) {
    Slots::register(cx, Slot::TopBarEnd, "daemon-status", 100, |_, cx| {
        let state = Daemon::global(cx).map(|d| d.read(cx).state().clone());
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let (dot, label, retry) = match &state {
            Some(ConnectionState::Connected { .. }) => (status_dot(p.sage, true), "coqod", false),
            Some(ConnectionState::Connecting) => (
                status_dot(p.text_subtle, false),
                "coqod · connecting",
                false,
            ),
            Some(ConnectionState::Disconnected { .. }) => {
                (status_dot(p.rose, false), "coqod · not connected", true)
            }
            None => (
                status_dot(p.text_subtle, false),
                "coqod · not connected",
                false,
            ),
        };
        let mut pill = div()
            .id("daemon-status")
            .test_support()
            .flex()
            .items_center()
            .gap(px(7.))
            .px(px(12.))
            .py(px(7.))
            .rounded(m.radius_field)
            .bg(p.panel_raised.hsla())
            .text_size(px(12.))
            .text_color(p.text_muted.hsla())
            .child(dot)
            .child(label);
        if retry {
            pill = pill.child(
                div()
                    .id("daemon-retry")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .ml(px(4.))
                    .text_color(p.blue.hsla())
                    .cursor_pointer()
                    .child(icon(Icon::RotateCw, px(12.), p.blue.hsla()))
                    .child("Retry")
                    .on_click(|_, _, cx| {
                        if let Some(daemon) = Daemon::global(cx) {
                            daemon.update(cx, |d, cx| d.retry(cx));
                        }
                    }),
            );
        }
        pill.into_any_element()
    });
}
