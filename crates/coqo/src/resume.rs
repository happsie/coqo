// SPDX-License-Identifier: Apache-2.0

//! Following daemon state across reconnects: a snapshot, then a
//! subscription from the `seq` it reflects (PLAN §4.3, ADR 0003).
//!
//! A [`Resumable`] fetches a snapshot that reports its `seq`, subscribes to
//! a topic with `since` set to it, and remembers the last `seq` it
//! delivered. When the connection comes back (a daemon restart, or a drop),
//! it subscribes again from that `seq`, so the events missed meanwhile are
//! replayed exactly once. When the daemon can't resume from there
//! (`RESUME_GAP`: retention pruned the events, or the log is behind), it
//! fetches a fresh snapshot instead. The owner sees one ordered stream of
//! [`Update`]s: a `Snapshot` replaces what it shows, an `Event` applies on
//! top of it.
//!
//! ```ignore
//! // Typed: a method whose result carries the seq, and a topic.
//! let feed = cx.new(|cx| {
//!     Resumable::follow::<workspace::List, workspace::Events>(
//!         (), |list| list.seq, EventsParams::default(), cx)
//! });
//! cx.subscribe(&feed, |this, _, update, cx| match update {
//!     Update::Snapshot(list) => this.replace(list, cx),
//!     Update::Event(event) => this.apply(&event.event, cx),
//! })
//! .detach();
//!
//! // A chat tab: sessions/get, then sessions/updates.
//! Resumable::follow::<sessions::Get, sessions::Updates>(
//!     SessionParams { id: id.clone() }, |s| s.head, SessionParams { id }, cx)
//!
//! // Anything else (a snapshot built from several requests):
//! Resumable::new(
//!     |daemon| daemon.request::<buffer::Open>(params.clone()).map_ok(|b| { let seq = b.rev; (b, seq) }).boxed(),
//!     |daemon, since| daemon.subscribe_since::<buffer::Changes>(params.clone(), Some(since)).boxed(),
//!     cx)
//! ```
//!
//! Dropping the entity stops it.

use std::time::Duration;

use coqo_protocol::{Method, Topic, subscription::Sequenced};
use coqo_ui::gpui::{AsyncApp, Context, EventEmitter, Subscription, Task, WeakEntity};
use futures_util::{FutureExt as _, future::BoxFuture};

use crate::daemon::{Daemon, DaemonError, DaemonEvent, EventStream};

/// How long to wait before subscribing again after a subscription failed
/// on a live connection (e.g. this client lagged).
const RESUBSCRIBE_DELAY: Duration = Duration::from_millis(100);

/// What a [`Resumable`] emits, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Update<S, E> {
    /// A fresh snapshot: replace what you show. The first update, and again
    /// after a `RESUME_GAP`.
    Snapshot(S),
    /// An event after the snapshot (or after the last event), never
    /// delivered twice.
    Event(Sequenced<E>),
}

type SnapshotFn<S> = Box<dyn Fn(&Daemon) -> BoxFuture<'static, Result<(S, u64), DaemonError>>>;
type SubscribeFn<E> =
    Box<dyn Fn(&Daemon, u64) -> BoxFuture<'static, Result<EventStream<Sequenced<E>>, DaemonError>>>;

/// A snapshot plus a resumable subscription. See the module docs.
pub struct Resumable<S, E> {
    snapshot: SnapshotFn<S>,
    subscribe: SubscribeFn<E>,
    last_seq: Option<u64>,
    snapshots: usize,
    error: Option<DaemonError>,
    _run: Option<Task<()>>,
    _daemon: Option<Subscription>,
}

impl<S, E> std::fmt::Debug for Resumable<S, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resumable")
            .field("last_seq", &self.last_seq)
            .field("snapshots", &self.snapshots)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<S: 'static, E: 'static> EventEmitter<Update<S, E>> for Resumable<S, E> {}

/// How a subscription ended.
enum Ended {
    /// Take a fresh snapshot and subscribe from it.
    Gap,
    /// Subscribe again from the last seq.
    Resubscribe,
    /// Wait for the next connection.
    Stop,
}

impl<S: Send + 'static, E: Send + 'static> Resumable<S, E> {
    /// Follow with a custom snapshot and subscription. `snapshot` returns the
    /// snapshot and the `seq` it reflects; `subscribe` subscribes with `since`
    /// set to the given `seq` (see [`Daemon::subscribe_since`]). It starts
    /// now if connected, otherwise on the next connection.
    pub fn new(
        snapshot: impl Fn(&Daemon) -> BoxFuture<'static, Result<(S, u64), DaemonError>> + 'static,
        subscribe: impl Fn(
            &Daemon,
            u64,
        )
            -> BoxFuture<'static, Result<EventStream<Sequenced<E>>, DaemonError>>
        + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let daemon = Daemon::global(cx);
        let mut this = Self {
            snapshot: Box::new(snapshot),
            subscribe: Box::new(subscribe),
            last_seq: None,
            snapshots: 0,
            error: None,
            _run: None,
            _daemon: daemon.as_ref().map(|daemon| {
                cx.subscribe(daemon, |this, _, event, cx| match event {
                    DaemonEvent::Connected => this.start(cx),
                    DaemonEvent::Disconnected => this._run = None,
                })
            }),
        };
        if daemon.is_some_and(|d| d.read(cx).is_connected()) {
            this.start(cx);
        }
        this
    }

    /// Follow topic `T` from the snapshot method `M`: `seq` reads the
    /// snapshot's `seq` (e.g. `|list| list.seq`, `|session| session.head`).
    pub fn follow<M, T>(
        snapshot: M::Params,
        seq: fn(&S) -> u64,
        topic: T::Params,
        cx: &mut Context<Self>,
    ) -> Self
    where
        M: Method<Result = S>,
        T: Topic<Event = E>,
        M::Params: Clone,
        T::Params: Clone,
    {
        Self::new(
            move |daemon| {
                daemon
                    .request::<M>(snapshot.clone())
                    .map(move |result| {
                        result.map(|s| {
                            let at = seq(&s);
                            (s, at)
                        })
                    })
                    .boxed()
            },
            move |daemon, since| daemon.subscribe_since::<T>(topic.clone(), since).boxed(),
            cx,
        )
    }

    /// The last `seq` delivered (or the snapshot's), if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// How many snapshots were taken: one, plus one per `RESUME_GAP`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn snapshots(&self) -> usize {
        self.snapshots
    }

    /// Why following stopped, if it did: it tries again on the next
    /// connection.
    #[allow(dead_code)] // For views that show it; tests use it.
    pub fn error(&self) -> Option<&DaemonError> {
        self.error.as_ref()
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self._run = Some(cx.spawn(async move |this, cx| {
            Self::run(this, cx).await;
        }));
    }

    fn failed(&mut self, what: &str, err: DaemonError, cx: &mut Context<Self>) {
        if err != DaemonError::NotConnected {
            eprintln!("coqo: couldn't {what}: {err}");
            self.error = Some(err);
            cx.notify();
        }
    }

    async fn run(this: WeakEntity<Self>, cx: &mut AsyncApp) {
        loop {
            let Ok(since) = this.read_with(cx, |this, _| this.last_seq) else {
                return;
            };
            let since = match since {
                Some(seq) => seq,
                None => {
                    let Ok(Some(fetch)) = this.update(cx, |this, cx| {
                        Daemon::global(cx).map(|d| (this.snapshot)(d.read(cx)))
                    }) else {
                        return;
                    };
                    match fetch.await {
                        Ok((snapshot, seq)) => {
                            let delivered = this.update(cx, |this, cx| {
                                this.last_seq = Some(seq);
                                this.snapshots += 1;
                                this.error = None;
                                cx.emit(Update::Snapshot(snapshot));
                            });
                            if delivered.is_err() {
                                return;
                            }
                            seq
                        }
                        Err(err) => {
                            this.update(cx, |this, cx| this.failed("take a snapshot", err, cx))
                                .ok();
                            return;
                        }
                    }
                }
            };
            let Ok(Some(subscribe)) = this.update(cx, |this, cx| {
                Daemon::global(cx).map(|d| (this.subscribe)(d.read(cx), since))
            }) else {
                return;
            };
            let mut stream = match subscribe.await {
                Ok(stream) => stream,
                Err(err) if err.is_resume_gap() => {
                    if this.update(cx, |this, _| this.last_seq = None).is_err() {
                        return;
                    }
                    continue;
                }
                Err(err) => {
                    this.update(cx, |this, cx| this.failed("subscribe", err, cx))
                        .ok();
                    return;
                }
            };
            let ended = loop {
                match stream.next().await {
                    Some(Ok(event)) => {
                        let delivered = this.update(cx, |this, cx| {
                            // A replay never repeats what was delivered.
                            if this.last_seq.is_none_or(|last| event.seq > last) {
                                this.last_seq = Some(event.seq);
                                cx.emit(Update::Event(event));
                            }
                        });
                        if delivered.is_err() {
                            return;
                        }
                    }
                    Some(Err(err)) if err.is_resume_gap() => break Ended::Gap,
                    Some(Err(DaemonError::NotConnected)) | None => break Ended::Stop,
                    Some(Err(err)) => {
                        eprintln!("coqo: a subscription ended: {err}; resubscribing");
                        break Ended::Resubscribe;
                    }
                }
            };
            match ended {
                Ended::Gap => {
                    if this.update(cx, |this, _| this.last_seq = None).is_err() {
                        return;
                    }
                }
                Ended::Resubscribe => {
                    cx.background_executor().timer(RESUBSCRIBE_DELAY).await;
                    let connected = this
                        .update(cx, |_, cx| {
                            Daemon::global(cx).is_some_and(|d| d.read(cx).is_connected())
                        })
                        .unwrap_or(false);
                    if !connected {
                        return;
                    }
                }
                Ended::Stop => return,
            }
        }
    }
}
