// SPDX-License-Identifier: Apache-2.0

//! The server side of a connection (`coqod`).
//!
//! Build a [`Router`] of typed handlers, wrap it in a [`Server`], and call
//! [`Server::serve`] for each accepted connection.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::auth::{Authenticator, Scope, Unauthorized};
use crate::encoding::Encoding;
use crate::error::ProtocolError;
use crate::frame::{DEFAULT_MAX_FRAME_LEN, HANDSHAKE_MAX_FRAME_LEN};
use crate::handshake::{
    DEFAULT_HANDSHAKE_TIMEOUT, Hello, IncompatibleProtocol, PROTOCOL_VERSION, ProtocolVersion,
    Welcome, negotiate_encoding,
};
use crate::message::{ErrorObject, Id, Message, Notification, Request, Response, codes};
use crate::method::{Method, NotificationMethod, Topic, builtin, is_reserved};
use crate::subscription::{
    EventFeed, EventParams, SubscribeParams, SubscribeResult, SubscriptionEndedParams,
    SubscriptionId, UnsubscribeParams,
};
use crate::transport;

type MethodHandler = Arc<
    dyn Fn(RequestContext, Option<Value>) -> BoxFuture<'static, Result<Value, ErrorObject>>
        + Send
        + Sync,
>;
type NotificationHandler =
    Arc<dyn Fn(RequestContext, Option<Value>) -> BoxFuture<'static, ()> + Send + Sync>;
type TopicHandler = Arc<
    dyn Fn(
            RequestContext,
            Value,
            Option<u64>,
        ) -> BoxFuture<'static, Result<EventFeed<Value>, ErrorObject>>
        + Send
        + Sync,
>;

fn parse_params<P: serde::de::DeserializeOwned>(
    method: &str,
    params: Option<Value>,
) -> Result<P, ErrorObject> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|e| ErrorObject::invalid_params(format!("invalid params for {method}: {e}")))
}

/// Typed handlers for methods, notifications and topics.
#[derive(Default)]
pub struct Router {
    methods: HashMap<&'static str, MethodHandler>,
    notifications: HashMap<&'static str, NotificationHandler>,
    topics: HashMap<&'static str, TopicHandler>,
}

impl fmt::Debug for Router {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut methods: Vec<_> = self.methods.keys().collect();
        let mut notifications: Vec<_> = self.notifications.keys().collect();
        let mut topics: Vec<_> = self.topics.keys().collect();
        methods.sort();
        notifications.sort();
        topics.sort();
        f.debug_struct("Router")
            .field("methods", &methods)
            .field("notifications", &notifications)
            .field("topics", &topics)
            .finish()
    }
}

fn check_name(name: &'static str, taken: bool) {
    assert!(!is_reserved(name), "`{name}` is reserved for the protocol");
    assert!(!taken, "`{name}` is registered twice");
}

impl Router {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle requests for `M`.
    ///
    /// # Panics
    /// If `M::NAME` is reserved (`coqo/…`, `rpc.…`) or already registered.
    pub fn method<M, F, Fut>(mut self, handler: F) -> Self
    where
        M: Method,
        F: Fn(RequestContext, M::Params) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Result, ErrorObject>> + Send + 'static,
    {
        check_name(M::NAME, self.methods.contains_key(M::NAME));
        let handler = Arc::new(handler);
        let erased: MethodHandler = Arc::new(move |ctx, params| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let params = parse_params::<M::Params>(M::NAME, params)?;
                let result = handler(ctx, params).await?;
                serde_json::to_value(result).map_err(|e| {
                    ErrorObject::internal(format!("cannot serialize result of {}: {e}", M::NAME))
                })
            })
        });
        self.methods.insert(M::NAME, erased);
        self
    }

    /// Handle notifications for `N`. Malformed params are logged and dropped.
    ///
    /// # Panics
    /// If `N::NAME` is reserved or already registered.
    pub fn notification<N, F, Fut>(mut self, handler: F) -> Self
    where
        N: NotificationMethod,
        F: Fn(RequestContext, N::Params) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        check_name(N::NAME, self.notifications.contains_key(N::NAME));
        let handler = Arc::new(handler);
        let erased: NotificationHandler = Arc::new(move |ctx, params| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                match parse_params::<N::Params>(N::NAME, params) {
                    Ok(params) => handler(ctx, params).await,
                    Err(e) => tracing::warn!(error = %e, "dropping notification"),
                }
            })
        });
        self.notifications.insert(N::NAME, erased);
        self
    }

    /// Serve subscriptions to `T`. The handler gets the topic params and the
    /// client's `since`, and returns the feed (for example from
    /// [`SeqLog::subscribe`](crate::SeqLog::subscribe)).
    ///
    /// # Panics
    /// If `T::NAME` is reserved or already registered.
    pub fn topic<T, F, Fut>(mut self, handler: F) -> Self
    where
        T: Topic,
        F: Fn(RequestContext, T::Params, Option<u64>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<EventFeed<T::Event>, ErrorObject>> + Send + 'static,
    {
        check_name(T::NAME, self.topics.contains_key(T::NAME));
        let handler = Arc::new(handler);
        let erased: TopicHandler = Arc::new(move |ctx, params, since| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let params = parse_params::<T::Params>(T::NAME, Some(params))?;
                let feed = handler(ctx, params, since).await?;
                Ok(feed.map(|event| {
                    serde_json::to_value(event).unwrap_or_else(|e| {
                        tracing::error!(topic = T::NAME, error = %e, "cannot serialize event");
                        Value::Null
                    })
                }))
            })
        });
        self.topics.insert(T::NAME, erased);
        self
    }
}

/// Server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// What this server is, e.g. `coqod`.
    pub server: String,
    /// This server's build id.
    pub build: String,
    /// The protocol version spoken (normally [`PROTOCOL_VERSION`]).
    pub protocol: ProtocolVersion,
    /// Encodings this server accepts (JSON is always accepted).
    pub encodings: Vec<Encoding>,
    /// How long to wait for the client's `coqo/hello`.
    pub handshake_timeout: Duration,
    /// Maximum frame payload after the handshake.
    pub max_frame_len: usize,
    /// Outgoing messages buffered per connection before senders wait.
    pub outgoing_buffer: usize,
    /// Checks each client's session token and decides its [`Scope`].
    /// `None` lets every client in with [`Scope::Daemon`]: only for
    /// in-process and test servers.
    pub authenticator: Option<Arc<dyn Authenticator>>,
}

impl ServerConfig {
    /// Defaults for a server called `server` with build id `build`.
    pub fn new(server: impl Into<String>, build: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            build: build.into(),
            protocol: PROTOCOL_VERSION,
            encodings: Encoding::ALL.to_vec(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
            outgoing_buffer: 1024,
            authenticator: None,
        }
    }
}

/// The server's handle to one connected client.
#[derive(Clone, Debug)]
pub struct Peer {
    connection: u64,
    outgoing: mpsc::Sender<Message>,
}

impl Peer {
    /// The connection id (also sent in [`Welcome::connection`]).
    pub fn connection(&self) -> u64 {
        self.connection
    }

    /// Whether the client is gone.
    pub fn is_closed(&self) -> bool {
        self.outgoing.is_closed()
    }

    /// Send a typed notification to the client.
    pub async fn notify<N: NotificationMethod>(
        &self,
        params: N::Params,
    ) -> Result<(), ProtocolError> {
        let params = serde_json::to_value(params)
            .map_err(|e| ProtocolError::conversion(format!("params of {}", N::NAME), e))?;
        self.send(Notification {
            method: N::NAME.to_owned(),
            params: (!params.is_null()).then_some(params),
        })
        .await
    }

    async fn send(&self, message: impl Into<Message>) -> Result<(), ProtocolError> {
        self.outgoing
            .send(message.into())
            .await
            .map_err(|_| ProtocolError::Closed)
    }
}

/// What a handler knows about the request's connection.
#[derive(Clone, Debug)]
pub struct RequestContext {
    /// The client, for pushing notifications.
    pub peer: Peer,
    /// The client's handshake, without its token.
    pub hello: Arc<Hello>,
    /// What this connection's token allows. Check it with
    /// [`Scope::require_daemon`] / [`Scope::require_workspace`].
    pub scope: Arc<Scope>,
}

/// Serves connections with a [`Router`]. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Server {
    router: Arc<Router>,
    config: Arc<ServerConfig>,
    next_connection: Arc<AtomicU64>,
}

type SubTable = Arc<Mutex<HashMap<SubscriptionId, oneshot::Sender<()>>>>;

impl Server {
    /// A server for `router`.
    pub fn new(router: Router, config: ServerConfig) -> Self {
        Self {
            router: Arc::new(router),
            config: Arc::new(config),
            next_connection: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Run one connection to completion: handshake, then dispatch until the
    /// client disconnects.
    ///
    /// Returns `Ok(())` when the client closes the connection. Handshake
    /// failures (timeout, incompatible version, anything but `coqo/hello`
    /// first) are answered with a JSON-RPC error where possible and returned
    /// as `Err`.
    pub async fn serve<IO>(&self, io: IO) -> Result<(), ProtocolError>
    where
        IO: AsyncRead + AsyncWrite + Send + 'static,
    {
        let config = &self.config;
        let (mut reader, mut writer) = transport::split(io, HANDSHAKE_MAX_FRAME_LEN);

        let timeout = config.handshake_timeout;
        let first = tokio::time::timeout(timeout, transport::recv(&mut reader, Encoding::Json))
            .await
            .map_err(|_| ProtocolError::HandshakeTimeout(timeout))?;
        let (id, hello) = match first {
            None => return Err(ProtocolError::Closed),
            Some(Err(e)) => return Err(e),
            Some(Ok(Err(e))) => {
                let reply = Response::err(e.id.clone(), e.to_error_object());
                let _ = transport::send(&mut writer, Encoding::Json, &reply.into()).await;
                return Err(ProtocolError::Handshake(format!(
                    "unreadable first message: {e}"
                )));
            }
            Some(Ok(Ok(Message::Request(Request { id, method, params })))) => {
                if method != builtin::Hello::NAME {
                    let reply = Response::err(
                        Some(id),
                        ErrorObject::new(
                            codes::NOT_INITIALIZED,
                            format!("expected coqo/hello before {method}"),
                        ),
                    );
                    let _ = transport::send(&mut writer, Encoding::Json, &reply.into()).await;
                    return Err(ProtocolError::Handshake(format!(
                        "client sent {method} before coqo/hello"
                    )));
                }
                match parse_params::<Hello>(builtin::Hello::NAME, params) {
                    Ok(hello) => (id, hello),
                    Err(e) => {
                        let message = e.message.clone();
                        let reply = Response::err(Some(id), e);
                        let _ = transport::send(&mut writer, Encoding::Json, &reply.into()).await;
                        return Err(ProtocolError::Handshake(message));
                    }
                }
            }
            Some(Ok(Ok(other))) => {
                // Describe it without its params: they may hold a token.
                let what = match other {
                    Message::Notification(n) => format!("a {} notification", n.method),
                    _ => "a response".to_owned(),
                };
                return Err(ProtocolError::Handshake(format!(
                    "expected a coqo/hello request, got {what}"
                )));
            }
        };

        let Some(negotiated) = config.protocol.negotiate(hello.protocol) else {
            let error = IncompatibleProtocol {
                protocol: config.protocol,
                build: config.build.clone(),
            }
            .to_error(hello.protocol);
            let _ = transport::send(
                &mut writer,
                Encoding::Json,
                &Response::err(Some(id), error).into(),
            )
            .await;
            return Err(ProtocolError::IncompatibleVersion {
                local: config.protocol,
                peer: hello.protocol,
                peer_build: hello.build,
            });
        };
        // Take the token out of the hello so handlers never see it.
        let mut hello = hello;
        let token = hello.token.take();
        let scope = match &config.authenticator {
            None => Scope::Daemon,
            Some(auth) => match auth.authenticate(token.as_ref()) {
                Ok(scope) => scope,
                Err(Unauthorized(message)) => {
                    let error = ErrorObject::new(codes::UNAUTHORIZED, message.clone());
                    let _ = transport::send(
                        &mut writer,
                        Encoding::Json,
                        &Response::err(Some(id), error).into(),
                    )
                    .await;
                    return Err(ProtocolError::Unauthorized(message));
                }
            },
        };
        drop(token);
        let encoding = negotiate_encoding(&hello.encodings, &config.encodings);
        let connection = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let welcome = Welcome {
            protocol: config.protocol,
            negotiated,
            build: config.build.clone(),
            server: config.server.clone(),
            encoding,
            connection,
        };
        let welcome =
            serde_json::to_value(&welcome).map_err(|e| ProtocolError::conversion("welcome", e))?;
        transport::send(
            &mut writer,
            Encoding::Json,
            &Response::ok(id, welcome).into(),
        )
        .await?;
        tracing::debug!(connection, client = %hello.client, build = %hello.build, %encoding, "client attached");

        transport::set_max_frame_len(&mut reader, &mut writer, config.max_frame_len);
        let (out_tx, out_rx) = mpsc::channel(config.outgoing_buffer.max(1));
        let writer_task = transport::AbortOnDrop(transport::spawn_writer(writer, encoding, out_rx));
        let ctx = RequestContext {
            peer: Peer {
                connection,
                outgoing: out_tx,
            },
            hello: Arc::new(hello),
            scope: Arc::new(scope),
        };
        let subs: SubTable = Arc::default();
        let next_sub = Arc::new(AtomicU64::new(1));
        let mut tasks = JoinSet::new();

        let result = loop {
            while tasks.try_join_next().is_some() {}
            let message = match transport::recv(&mut reader, encoding).await {
                None => break Ok(()),
                Some(Err(e)) => break Err(e),
                Some(Ok(Err(e))) => {
                    let reply = Response::err(e.id.clone(), e.to_error_object());
                    if ctx.peer.send(reply).await.is_err() {
                        break Ok(());
                    }
                    continue;
                }
                Some(Ok(Ok(m))) => m,
            };
            match message {
                Message::Request(r) => {
                    self.dispatch_request(r, &ctx, &subs, &next_sub, &mut tasks);
                }
                Message::Notification(n) => {
                    if let Some(handler) = self.router.notifications.get(n.method.as_str()) {
                        tasks.spawn(handler(ctx.clone(), n.params));
                    } else {
                        tracing::debug!(method = %n.method, "ignoring unknown notification");
                    }
                }
                Message::Response(_) => {
                    tracing::debug!("ignoring a response; v0 servers send no requests");
                }
            }
        };

        tasks.shutdown().await;
        drop(writer_task);
        tracing::debug!(connection, "client detached");
        result
    }

    fn dispatch_request(
        &self,
        request: Request,
        ctx: &RequestContext,
        subs: &SubTable,
        next_sub: &Arc<AtomicU64>,
        tasks: &mut JoinSet<()>,
    ) {
        let Request { id, method, params } = request;
        let peer = ctx.peer.clone();
        match method.as_str() {
            m if m == builtin::Hello::NAME => {
                let error = ErrorObject::invalid_request("the handshake is already complete");
                tasks.spawn(async move {
                    let _ = peer.send(Response::err(Some(id), error)).await;
                });
            }
            m if m == builtin::Subscribe::NAME => {
                let router = Arc::clone(&self.router);
                let (ctx, subs, next_sub) = (ctx.clone(), Arc::clone(subs), Arc::clone(next_sub));
                tasks.spawn(subscribe(id, params, router, ctx, subs, next_sub));
            }
            m if m == builtin::Unsubscribe::NAME => {
                let outcome = parse_params::<UnsubscribeParams>(m, params).map(|p| {
                    let cancel = subs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&p.subscription);
                    Value::Bool(cancel.is_some_and(|c| c.send(()).is_ok()))
                });
                tasks.spawn(async move {
                    let _ = peer
                        .send(Response {
                            id: Some(id),
                            outcome,
                        })
                        .await;
                });
            }
            _ => match self.router.methods.get(method.as_str()) {
                Some(handler) => {
                    let fut = handler(ctx.clone(), params);
                    tasks.spawn(async move {
                        let outcome = fut.await;
                        let _ = peer
                            .send(Response {
                                id: Some(id),
                                outcome,
                            })
                            .await;
                    });
                }
                None => {
                    let error = ErrorObject::method_not_found(&method);
                    tasks.spawn(async move {
                        let _ = peer.send(Response::err(Some(id), error)).await;
                    });
                }
            },
        }
    }
}

async fn subscribe(
    id: Id,
    params: Option<Value>,
    router: Arc<Router>,
    ctx: RequestContext,
    subs: SubTable,
    next_sub: Arc<AtomicU64>,
) {
    let peer = ctx.peer.clone();
    let params = match parse_params::<SubscribeParams>(builtin::Subscribe::NAME, params) {
        Ok(p) => p,
        Err(e) => {
            let _ = peer.send(Response::err(Some(id), e)).await;
            return;
        }
    };
    let Some(handler) = router.topics.get(params.topic.as_str()).cloned() else {
        let error = ErrorObject::new(
            codes::UNKNOWN_SUBSCRIPTION,
            format!("unknown topic: {}", params.topic),
        );
        let _ = peer.send(Response::err(Some(id), error)).await;
        return;
    };
    drop(router);
    let mut feed = match handler(ctx, params.params, params.since).await {
        Ok(feed) => feed,
        Err(e) => {
            let _ = peer.send(Response::err(Some(id), e)).await;
            return;
        }
    };

    let subscription = SubscriptionId(next_sub.fetch_add(1, Ordering::Relaxed));
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    subs.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(subscription, cancel_tx);
    let result = SubscribeResult {
        subscription,
        head: feed.head,
    };
    let result = serde_json::to_value(result).unwrap_or(Value::Null);
    // The response is queued before any event, so the client always learns
    // the id first.
    if peer.send(Response::ok(id, result)).await.is_err() {
        return;
    }

    let ended = loop {
        let item = tokio::select! {
            _ = &mut cancel_rx => return, // unsubscribed
            item = feed.events.next() => item,
        };
        match item {
            Some(Ok(s)) => {
                let params = EventParams {
                    subscription,
                    seq: s.seq,
                    event: s.event,
                };
                let note = Notification {
                    method: builtin::Event::NAME.to_owned(),
                    params: serde_json::to_value(params).ok(),
                };
                if peer.send(note).await.is_err() {
                    return;
                }
            }
            Some(Err(e)) => break Some(e),
            None => break None,
        }
    };
    subs.lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&subscription);
    let _ = peer
        .send(Notification {
            method: builtin::SubscriptionEnded::NAME.to_owned(),
            params: serde_json::to_value(SubscriptionEndedParams {
                subscription,
                error: ended,
            })
            .ok(),
        })
        .await;
}
