// SPDX-License-Identifier: Apache-2.0

//! The client side of a connection (UI, CLI, `coqod mcp`).

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};

use crate::encoding::Encoding;
use crate::error::ProtocolError;
use crate::frame::{DEFAULT_MAX_FRAME_LEN, HANDSHAKE_MAX_FRAME_LEN};
use crate::handshake::{
    DEFAULT_HANDSHAKE_TIMEOUT, Hello, IncompatibleProtocol, PROTOCOL_VERSION, ProtocolVersion,
    Welcome,
};
use crate::message::{ErrorObject, Id, Message, Notification, Request, Response, codes};
use crate::method::{Method, NotificationMethod, Topic, builtin};
use crate::subscription::{
    EventParams, Sequenced, SubscribeParams, SubscribeResult, SubscriptionEndedParams,
    SubscriptionId, UnsubscribeParams,
};
use crate::transport;

/// Client settings.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// What this client is, e.g. `coqo-ui`.
    pub client: String,
    /// This client's build id.
    pub build: String,
    /// The protocol version to announce (normally [`PROTOCOL_VERSION`]).
    pub protocol: ProtocolVersion,
    /// Accepted encodings in preference order.
    pub encodings: Vec<Encoding>,
    /// How long to wait for the server's `coqo/hello` answer.
    pub handshake_timeout: Duration,
    /// Default timeout for requests; `None` waits until the connection closes.
    pub request_timeout: Option<Duration>,
    /// Maximum frame payload after the handshake.
    pub max_frame_len: usize,
    /// Events buffered per subscription before it is ended as lagged.
    pub subscription_buffer: usize,
    /// Outgoing messages buffered before senders wait.
    pub outgoing_buffer: usize,
}

impl ClientConfig {
    /// Defaults for a client called `client` with build id `build`.
    pub fn new(client: impl Into<String>, build: impl Into<String>) -> Self {
        Self {
            client: client.into(),
            build: build.into(),
            protocol: PROTOCOL_VERSION,
            encodings: Encoding::ALL.to_vec(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            request_timeout: None,
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
            subscription_buffer: 1024,
            outgoing_buffer: 256,
        }
    }

    fn hello(&self) -> Hello {
        Hello {
            protocol: self.protocol,
            build: self.build.clone(),
            client: self.client.clone(),
            encodings: self.encodings.clone(),
        }
    }
}

type CallReply = oneshot::Sender<Result<Value, ErrorObject>>;
type SubscribeReply = oneshot::Sender<Result<SubHandle, ErrorObject>>;

enum Pending {
    Call(CallReply),
    Subscribe(SubscribeReply),
}

enum SubItem {
    Event(u64, Value),
    Ended(Option<ErrorObject>),
}

struct SubEntry {
    tx: mpsc::Sender<SubItem>,
    lagged: Arc<AtomicBool>,
}

struct SubHandle {
    result: SubscribeResult,
    rx: mpsc::Receiver<SubItem>,
    lagged: Arc<AtomicBool>,
}

#[derive(Default)]
struct PendingTable {
    closed: bool,
    map: HashMap<i64, Pending>,
}

/// State shared by the handles and the reader task. Holds no strong
/// outgoing sender, so dropping every handle closes the connection.
struct State {
    pending: Mutex<PendingTable>,
    subs: Mutex<HashMap<SubscriptionId, SubEntry>>,
    next_id: AtomicI64,
    closed: watch::Sender<bool>,
    subscription_buffer: usize,
}

impl State {
    fn pending(&self) -> MutexGuard<'_, PendingTable> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn subs(&self) -> MutexGuard<'_, HashMap<SubscriptionId, SubEntry>> {
        self.subs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn unsubscribe_request(&self, subscription: SubscriptionId) -> Message {
        Request {
            id: Id::Number(self.next_id()),
            method: builtin::Unsubscribe::NAME.to_owned(),
            params: serde_json::to_value(UnsubscribeParams { subscription }).ok(),
        }
        .into()
    }
}

struct Inner {
    outgoing: mpsc::Sender<Message>,
    state: Arc<State>,
    welcome: Welcome,
    request_timeout: Option<Duration>,
}

/// A connected, handshaken client. Cheap to clone; the connection closes
/// when every clone and every [`Subscription`] is dropped.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("welcome", &self.inner.welcome)
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Server → client notifications that are not subscription events.
#[derive(Debug)]
pub struct Notifications {
    rx: mpsc::UnboundedReceiver<Notification>,
}

impl Notifications {
    /// The next notification; `None` once the connection is closed.
    pub async fn recv(&mut self) -> Option<Notification> {
        self.rx.recv().await
    }
}

impl Stream for Notifications {
    type Item = Notification;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Notification>> {
        self.rx.poll_recv(cx)
    }
}

impl Client {
    /// Handshake over `io` and start the connection.
    ///
    /// Fails with [`ProtocolError::IncompatibleVersion`] if the server
    /// speaks an incompatible version, and with
    /// [`ProtocolError::HandshakeTimeout`] if it does not answer within
    /// `config.handshake_timeout`, so this never hangs.
    pub async fn connect<IO>(
        io: IO,
        config: ClientConfig,
    ) -> Result<(Client, Notifications), ProtocolError>
    where
        IO: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mut reader, mut writer) = transport::split(io, HANDSHAKE_MAX_FRAME_LEN);
        let hello = config.hello();
        let timeout = config.handshake_timeout;

        let handshake = async {
            let request = Request {
                id: Id::Number(0),
                method: builtin::Hello::NAME.to_owned(),
                params: Some(
                    serde_json::to_value(&hello)
                        .map_err(|e| ProtocolError::conversion("hello", e))?,
                ),
            };
            transport::send(&mut writer, Encoding::Json, &request.into()).await?;
            match transport::recv(&mut reader, Encoding::Json).await {
                None => Err(ProtocolError::Handshake(
                    "the server closed the connection during the handshake".into(),
                )),
                Some(Err(e)) => Err(e),
                Some(Ok(Err(e))) => Err(ProtocolError::Handshake(format!(
                    "unreadable answer to coqo/hello: {e}"
                ))),
                Some(Ok(Ok(Message::Response(Response {
                    id: Some(Id::Number(0)),
                    outcome,
                })))) => Ok(outcome),
                Some(Ok(Ok(other))) => Err(ProtocolError::Handshake(format!(
                    "expected the answer to coqo/hello, got {other:?}"
                ))),
            }
        };
        let outcome = tokio::time::timeout(timeout, handshake)
            .await
            .map_err(|_| ProtocolError::HandshakeTimeout(timeout))??;

        let welcome: Welcome = match outcome {
            Ok(v) => serde_json::from_value(v)
                .map_err(|e| ProtocolError::conversion("coqo/hello result", e))?,
            Err(e) if e.code == codes::INCOMPATIBLE_PROTOCOL => {
                let data: Option<IncompatibleProtocol> =
                    e.data.clone().and_then(|d| serde_json::from_value(d).ok());
                return Err(match data {
                    Some(d) => ProtocolError::IncompatibleVersion {
                        local: config.protocol,
                        peer: d.protocol,
                        peer_build: d.build,
                    },
                    None => ProtocolError::Handshake(e.message),
                });
            }
            Err(e) => return Err(ProtocolError::Handshake(e.to_string())),
        };
        if !config.protocol.is_compatible_with(welcome.protocol) {
            return Err(ProtocolError::IncompatibleVersion {
                local: config.protocol,
                peer: welcome.protocol,
                peer_build: welcome.build,
            });
        }
        if welcome.encoding != Encoding::Json && !config.encodings.contains(&welcome.encoding) {
            return Err(ProtocolError::Handshake(format!(
                "the server chose encoding {}, which this client did not offer",
                welcome.encoding
            )));
        }
        transport::set_max_frame_len(&mut reader, &mut writer, config.max_frame_len);

        let encoding = welcome.encoding;
        let (out_tx, out_rx) = mpsc::channel(config.outgoing_buffer.max(1));
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        let (closed, _) = watch::channel(false);
        let state = Arc::new(State {
            pending: Mutex::default(),
            subs: Mutex::default(),
            next_id: AtomicI64::new(1),
            closed,
            subscription_buffer: config.subscription_buffer.max(1),
        });
        transport::spawn_writer(writer, encoding, out_rx);
        tokio::spawn(read_loop(
            reader,
            encoding,
            Arc::clone(&state),
            out_tx.downgrade(),
            notif_tx,
        ));

        let client = Client {
            inner: Arc::new(Inner {
                outgoing: out_tx,
                state,
                welcome,
                request_timeout: config.request_timeout,
            }),
        };
        Ok((client, Notifications { rx: notif_rx }))
    }

    /// The server's handshake answer.
    pub fn welcome(&self) -> &Welcome {
        &self.inner.welcome
    }

    /// Whether the connection is closed.
    pub fn is_closed(&self) -> bool {
        *self.inner.state.closed.borrow()
    }

    /// Resolves once the connection is closed.
    pub async fn closed(&self) {
        let mut rx = self.inner.state.closed.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    /// Call a typed method.
    pub async fn request<M: Method>(&self, params: M::Params) -> Result<M::Result, ProtocolError> {
        let params = to_params(M::NAME, &params)?;
        let value = self.request_raw(M::NAME, params).await?;
        serde_json::from_value(value)
            .map_err(|e| ProtocolError::conversion(format!("result of {}", M::NAME), e))
    }

    /// Call a method by name with untyped params.
    pub async fn request_raw(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, ProtocolError> {
        let (tx, rx) = oneshot::channel();
        let outcome = self.call(method, params, Pending::Call(tx), rx).await?;
        outcome.map_err(ProtocolError::Rpc)
    }

    /// Send a typed notification.
    pub async fn notify<N: NotificationMethod>(
        &self,
        params: N::Params,
    ) -> Result<(), ProtocolError> {
        let params = to_params(N::NAME, &params)?;
        self.notify_raw(N::NAME, params).await
    }

    /// Send a notification by name with untyped params.
    pub async fn notify_raw(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), ProtocolError> {
        let message = Notification {
            method: method.to_owned(),
            params,
        };
        self.inner
            .outgoing
            .send(message.into())
            .await
            .map_err(|_| ProtocolError::Closed)
    }

    /// Subscribe to a topic.
    ///
    /// With `since: Some(n)` the server replays events after `n` first; see
    /// [`crate::subscription`]. Fails with an [`ProtocolError::Rpc`] carrying
    /// [`RESUME_GAP`](codes::RESUME_GAP) if that is impossible.
    pub async fn subscribe<T: Topic>(
        &self,
        params: T::Params,
        since: Option<u64>,
    ) -> Result<Subscription<T>, ProtocolError> {
        let params = serde_json::to_value(&params)
            .map_err(|e| ProtocolError::conversion(format!("params of {}", T::NAME), e))?;
        let request = SubscribeParams {
            topic: T::NAME.to_owned(),
            params,
            since,
        };
        let request = serde_json::to_value(&request)
            .map_err(|e| ProtocolError::conversion("coqo/subscribe params", e))?;
        let (tx, rx) = oneshot::channel();
        let handle = self
            .call(
                builtin::Subscribe::NAME,
                Some(request),
                Pending::Subscribe(tx),
                rx,
            )
            .await?
            .map_err(ProtocolError::Rpc)?;
        Ok(Subscription {
            id: handle.result.subscription,
            head: handle.result.head,
            start: since.unwrap_or(handle.result.head),
            last_seq: None,
            rx: handle.rx,
            lagged: handle.lagged,
            client: Arc::clone(&self.inner),
            finished: false,
            _topic: PhantomData,
        })
    }

    async fn call<T>(
        &self,
        method: &str,
        params: Option<Value>,
        pending: Pending,
        rx: oneshot::Receiver<Result<T, ErrorObject>>,
    ) -> Result<Result<T, ErrorObject>, ProtocolError> {
        let state = &self.inner.state;
        let id = state.next_id();
        {
            let mut table = state.pending();
            if table.closed {
                return Err(ProtocolError::Closed);
            }
            table.map.insert(id, pending);
        }
        let request = Request {
            id: Id::Number(id),
            method: method.to_owned(),
            params,
        };
        if self.inner.outgoing.send(request.into()).await.is_err() {
            state.pending().map.remove(&id);
            return Err(ProtocolError::Closed);
        }
        let reply = match self.inner.request_timeout {
            None => rx.await,
            Some(timeout) => match tokio::time::timeout(timeout, rx).await {
                Ok(r) => r,
                Err(_) => {
                    state.pending().map.remove(&id);
                    return Err(ProtocolError::RequestTimeout {
                        method: method.to_owned(),
                        timeout,
                    });
                }
            },
        };
        reply.map_err(|_| ProtocolError::Closed)
    }
}

fn to_params(method: &str, params: &impl Serialize) -> Result<Option<Value>, ProtocolError> {
    let value = serde_json::to_value(params)
        .map_err(|e| ProtocolError::conversion(format!("params of {method}"), e))?;
    // JSON-RPC 2.0 params must be structured; `()` is sent as no params.
    Ok((!value.is_null()).then_some(value))
}

async fn read_loop<IO>(
    mut reader: transport::Reader<IO>,
    encoding: Encoding,
    state: Arc<State>,
    outgoing: mpsc::WeakSender<Message>,
    notifications: mpsc::UnboundedSender<Notification>,
) where
    IO: AsyncRead,
{
    while let Some(next) = transport::recv(&mut reader, encoding).await {
        let message = match next {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "ignoring an undecodable message from the server");
                continue;
            }
            Err(e) => {
                tracing::debug!(error = %e, "client connection failed");
                break;
            }
        };
        match message {
            Message::Response(r) => on_response(&state, r),
            Message::Notification(n) if n.method == builtin::Event::NAME => {
                match n.params.map(serde_json::from_value::<EventParams>) {
                    Some(Ok(p)) => on_event(&state, &outgoing, p),
                    _ => tracing::warn!("ignoring a malformed coqo/event"),
                }
            }
            Message::Notification(n) if n.method == builtin::SubscriptionEnded::NAME => {
                match n
                    .params
                    .map(serde_json::from_value::<SubscriptionEndedParams>)
                {
                    Some(Ok(p)) => {
                        if let Some(entry) = state.subs().remove(&p.subscription) {
                            let _ = entry.tx.try_send(SubItem::Ended(p.error));
                        }
                    }
                    _ => tracing::warn!("ignoring a malformed coqo/subscriptionEnded"),
                }
            }
            Message::Notification(n) => {
                let _ = notifications.send(n);
            }
            Message::Request(r) => {
                // v0 servers do not call clients; answer so they never hang.
                if let Some(out) = outgoing.upgrade() {
                    let reply = Response::err(Some(r.id), ErrorObject::method_not_found(&r.method));
                    let _ = out.try_send(reply.into());
                }
            }
        }
    }

    // Closed: fail everything that is waiting.
    let pending = {
        let mut table = state.pending();
        table.closed = true;
        std::mem::take(&mut table.map)
    };
    drop(pending);
    state.subs().clear();
    state.closed.send_replace(true);
}

fn on_response(state: &State, response: Response) {
    let Some(Id::Number(id)) = response.id else {
        if let Err(e) = response.outcome {
            tracing::warn!(error = %e, "server reported an error without a request id");
        }
        return;
    };
    let Some(pending) = state.pending().map.remove(&id) else {
        return; // timed out, or a fire-and-forget unsubscribe
    };
    match pending {
        Pending::Call(tx) => {
            let _ = tx.send(response.outcome);
        }
        Pending::Subscribe(tx) => {
            let reply = response.outcome.and_then(|v| {
                serde_json::from_value::<SubscribeResult>(v)
                    .map_err(|e| ErrorObject::internal(format!("bad coqo/subscribe result: {e}")))
            });
            let reply = reply.map(|result| {
                // Register before anything else is read, so no event for
                // this subscription can arrive before it is known.
                let (tx, rx) = mpsc::channel(state.subscription_buffer);
                let lagged = Arc::new(AtomicBool::new(false));
                state.subs().insert(
                    result.subscription,
                    SubEntry {
                        tx,
                        lagged: Arc::clone(&lagged),
                    },
                );
                SubHandle { result, rx, lagged }
            });
            if let Err(Ok(handle)) = tx.send(reply) {
                // The subscriber gave up (timeout); undo.
                state.subs().remove(&handle.result.subscription);
            }
        }
    }
}

fn on_event(state: &State, outgoing: &mpsc::WeakSender<Message>, p: EventParams) {
    let mut subs = state.subs();
    let Some(entry) = subs.get(&p.subscription) else {
        return; // already unsubscribed
    };
    match entry.tx.try_send(SubItem::Event(p.seq, p.event)) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            entry.lagged.store(true, Ordering::Release);
            subs.remove(&p.subscription);
            drop(subs);
            if let Some(out) = outgoing.upgrade() {
                let _ = out.try_send(state.unsubscribe_request(p.subscription));
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            subs.remove(&p.subscription);
        }
    }
}

/// A live subscription: a [`Stream`] of sequenced, typed events.
///
/// The stream ends with `None` when the server ends the subscription
/// normally, or with one `Err` item when it cannot continue:
/// [`ProtocolError::Lagged`] (this client did not keep up; resubscribe with
/// `since: Some(resume_from)`), [`ProtocolError::Rpc`] (the server ended it,
/// e.g. [`RESUME_GAP`](codes::RESUME_GAP)), or [`ProtocolError::Closed`].
///
/// Dropping it unsubscribes.
pub struct Subscription<T: Topic> {
    id: SubscriptionId,
    head: u64,
    start: u64,
    last_seq: Option<u64>,
    rx: mpsc::Receiver<SubItem>,
    lagged: Arc<AtomicBool>,
    client: Arc<Inner>,
    finished: bool,
    _topic: PhantomData<fn() -> T>,
}

impl<T: Topic> fmt::Debug for Subscription<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("topic", &T::NAME)
            .field("id", &self.id)
            .field("head", &self.head)
            .field("last_seq", &self.last_seq)
            .finish()
    }
}

impl<T: Topic> Subscription<T> {
    /// The server-assigned id.
    pub fn id(&self) -> SubscriptionId {
        self.id
    }

    /// The source's latest seq when the subscription started.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// The last seq delivered, if any.
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// What to pass as `since` to resume without missing or repeating
    /// events: the last delivered seq, or the starting point.
    pub fn resume_from(&self) -> u64 {
        self.last_seq.unwrap_or(self.start)
    }

    /// Stop the subscription and wait for the server to confirm.
    pub async fn unsubscribe(mut self) -> Result<bool, ProtocolError> {
        self.finished = true;
        self.client.state.subs().remove(&self.id);
        let client = Client {
            inner: Arc::clone(&self.client),
        };
        client
            .request::<builtin::Unsubscribe>(UnsubscribeParams {
                subscription: self.id,
            })
            .await
    }

    fn finish(
        &mut self,
        item: Option<Result<Sequenced<T::Event>, ProtocolError>>,
    ) -> Poll<Option<Result<Sequenced<T::Event>, ProtocolError>>> {
        self.finished = true;
        Poll::Ready(item)
    }
}

impl<T: Topic> Stream for Subscription<T>
where
    T::Event: DeserializeOwned,
{
    type Item = Result<Sequenced<T::Event>, ProtocolError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        match self.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(SubItem::Event(seq, event))) => {
                if self.last_seq.is_some_and(|last| seq <= last) {
                    let err = ProtocolError::Violation(format!(
                        "subscription {} went from seq {:?} to {seq}",
                        self.id, self.last_seq
                    ));
                    return self.finish(Some(Err(err)));
                }
                match serde_json::from_value::<T::Event>(event) {
                    Ok(event) => {
                        self.last_seq = Some(seq);
                        Poll::Ready(Some(Ok(Sequenced { seq, event })))
                    }
                    Err(e) => {
                        let err = ProtocolError::conversion(format!("event of {}", T::NAME), e);
                        self.finish(Some(Err(err)))
                    }
                }
            }
            Poll::Ready(Some(SubItem::Ended(None))) => self.finish(None),
            Poll::Ready(Some(SubItem::Ended(Some(e)))) => {
                self.finish(Some(Err(ProtocolError::Rpc(e))))
            }
            Poll::Ready(None) => {
                let err = if self.lagged.load(Ordering::Acquire) {
                    ProtocolError::Lagged {
                        resume_from: self.resume_from(),
                    }
                } else {
                    ProtocolError::Closed
                };
                self.finish(Some(Err(err)))
            }
        }
    }
}

impl<T: Topic> Drop for Subscription<T> {
    fn drop(&mut self) {
        // Still registered means the server still streams it: tell it to stop.
        let state = &self.client.state;
        if state.subs().remove(&self.id).is_some() {
            let _ = self
                .client
                .outgoing
                .try_send(state.unsubscribe_request(self.id));
        }
    }
}
