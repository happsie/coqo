// SPDX-License-Identifier: Apache-2.0

//! Where a terminal tab's data comes from. [`DaemonBackend`] talks to `coqod`
//! over the app's daemon connection ([`crate::daemon`]), on that
//! connection's tokio runtime; tests use an in-memory backend.

use std::collections::HashMap;
use std::time::Duration;

use coqo_protocol::methods::terminal::{
    self as proto, InputParams, ResizeParams, ScrollParams, ScrollTo, SpawnParams, TermSize,
    TerminalEvent, TerminalId, TerminalInfo, TerminalRef, TerminalSnapshot,
};
use coqo_protocol::{Client, ProtocolError, codes};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};

use crate::daemon::DaemonHandle;

/// How long a call waits for the daemon connection (at startup, or while it
/// reconnects) before giving up.
const CONNECT_WAIT: Duration = Duration::from_secs(15);

/// What an attached view receives.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// The whole screen: on attach, and again if the view fell behind.
    Snapshot(TerminalSnapshot),
    /// One event after the snapshot.
    Event(TerminalEvent),
    /// The terminal is gone (closed, or the daemon went away).
    Lost(String),
}

/// A terminal source. Calls return immediately; results arrive on channels.
pub trait TerminalBackend: 'static {
    /// Start a terminal.
    fn spawn(&self, params: SpawnParams) -> oneshot::Receiver<Result<TerminalInfo, String>>;
    /// Follow a terminal: a snapshot, then its events. Drop the receiver to
    /// detach.
    fn attach(&self, id: TerminalId) -> mpsc::UnboundedReceiver<Update>;
    /// Type, paste or send keys. Delivered in order.
    fn input(&self, id: TerminalId, data: String);
    fn resize(&self, id: TerminalId, size: TermSize);
    fn scroll(&self, id: TerminalId, scroll: ScrollTo);
    /// End the terminal's process and forget it.
    fn close(&self, id: TerminalId);
}

/// One operation on a terminal, run in order with the others.
#[derive(Debug)]
enum Op {
    Input(String),
    Resize(TermSize),
    Scroll(ScrollTo),
    Close,
}

/// Terminals in `coqod`, over the app's daemon connection.
pub struct DaemonBackend {
    io: DaemonHandle,
    queues: std::cell::RefCell<HashMap<TerminalId, tokio::sync::mpsc::UnboundedSender<Op>>>,
}

impl std::fmt::Debug for DaemonBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonBackend").finish_non_exhaustive()
    }
}

impl DaemonBackend {
    /// A backend on the app's daemon connection.
    pub fn new(io: DaemonHandle) -> Self {
        Self {
            io,
            queues: Default::default(),
        }
    }

    async fn client(io: &DaemonHandle) -> Result<Client, String> {
        io.wait_for_client(CONNECT_WAIT)
            .await
            .map_err(|e| e.to_string())
    }

    fn queue(&self, id: TerminalId) -> tokio::sync::mpsc::UnboundedSender<Op> {
        let mut queues = self.queues.borrow_mut();
        if let Some(tx) = queues.get(&id).filter(|tx| !tx.is_closed()) {
            return tx.clone();
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let io = self.io.clone();
        self.io.spawn(async move {
            run_ops(id, rx, &io).await;
        });
        queues.insert(id, tx.clone());
        tx
    }
}

/// Run one terminal's operations in order, merging keystrokes typed while
/// the previous input was in flight.
async fn run_ops(
    id: TerminalId,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Op>,
    io: &DaemonHandle,
) {
    let mut pending: Option<Op> = None;
    loop {
        let op = match pending.take() {
            Some(op) => op,
            None => match rx.recv().await {
                Some(op) => op,
                None => return,
            },
        };
        let op = match op {
            Op::Input(mut data) => {
                while let Ok(next) = rx.try_recv() {
                    match next {
                        Op::Input(more) => data.push_str(&more),
                        other => {
                            pending = Some(other);
                            break;
                        }
                    }
                }
                Op::Input(data)
            }
            other => other,
        };
        let Ok(client) = DaemonBackend::client(io).await else {
            continue;
        };
        let result = match op {
            Op::Input(data) => client
                .request::<proto::Input>(InputParams { id, data })
                .await
                .map(drop),
            Op::Resize(size) => client
                .request::<proto::Resize>(ResizeParams { id, size })
                .await
                .map(drop),
            Op::Scroll(scroll) => client
                .request::<proto::Scroll>(ScrollParams { id, scroll })
                .await
                .map(drop),
            Op::Close => {
                let _ = client.request::<proto::Close>(TerminalRef { id }).await;
                return;
            }
        };
        if let Err(error) = result {
            tracing_like_debug(id, &error);
        }
    }
}

fn tracing_like_debug(id: TerminalId, error: &ProtocolError) {
    if cfg!(debug_assertions) {
        eprintln!("coqo: terminal {id}: {error}");
    }
}

/// The view fell behind (the daemon no longer has the events, or this client
/// lagged): start over from a fresh snapshot.
fn is_resume_gap(e: &ProtocolError) -> bool {
    matches!(e, ProtocolError::Lagged { .. })
        || matches!(e, ProtocolError::Rpc(obj) if obj.code == codes::RESUME_GAP)
}

/// Snapshot, subscribe from its seq, forward events; re-snapshot when the
/// view fell behind. Ends when `tx` is dropped or the terminal is gone.
async fn follow(client: Client, id: TerminalId, tx: mpsc::UnboundedSender<Update>) {
    loop {
        let snap = match client.request::<proto::Snapshot>(TerminalRef { id }).await {
            Ok(snap) => snap,
            Err(e) => {
                let _ = tx.unbounded_send(Update::Lost(e.to_string()));
                return;
            }
        };
        let seq = snap.seq;
        if tx.unbounded_send(Update::Snapshot(snap)).is_err() {
            return;
        }
        let mut sub = match client
            .subscribe::<proto::Output>(TerminalRef { id }, Some(seq))
            .await
        {
            Ok(sub) => sub,
            Err(e) if is_resume_gap(&e) => continue,
            Err(e) => {
                let _ = tx.unbounded_send(Update::Lost(e.to_string()));
                return;
            }
        };
        loop {
            let item = tokio::select! {
                item = sub.next() => item,
                () = closed(&tx) => return,
            };
            match item {
                Some(Ok(event)) => {
                    if tx.unbounded_send(Update::Event(event.event)).is_err() {
                        return;
                    }
                }
                Some(Err(e)) if is_resume_gap(&e) => break,
                Some(Err(e)) => {
                    let _ = tx.unbounded_send(Update::Lost(e.to_string()));
                    return;
                }
                None => {
                    let _ = tx.unbounded_send(Update::Lost("the terminal was closed".into()));
                    return;
                }
            }
        }
    }
}

/// Resolves when the view dropped its receiver.
async fn closed(tx: &mpsc::UnboundedSender<Update>) {
    while !tx.is_closed() {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

impl TerminalBackend for DaemonBackend {
    fn spawn(&self, params: SpawnParams) -> oneshot::Receiver<Result<TerminalInfo, String>> {
        let (tx, rx) = oneshot::channel();
        let io = self.io.clone();
        self.io.spawn(async move {
            let result = match Self::client(&io).await {
                Ok(client) => client
                    .request::<proto::Spawn>(params)
                    .await
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            let _ = tx.send(result);
        });
        rx
    }

    fn attach(&self, id: TerminalId) -> mpsc::UnboundedReceiver<Update> {
        let (tx, rx) = mpsc::unbounded();
        let io = self.io.clone();
        self.io.spawn(async move {
            match Self::client(&io).await {
                Ok(client) => follow(client, id, tx).await,
                Err(e) => {
                    let _ = tx.unbounded_send(Update::Lost(e));
                }
            }
        });
        rx
    }

    fn input(&self, id: TerminalId, data: String) {
        let _ = self.queue(id).send(Op::Input(data));
    }

    fn resize(&self, id: TerminalId, size: TermSize) {
        let _ = self.queue(id).send(Op::Resize(size));
    }

    fn scroll(&self, id: TerminalId, scroll: ScrollTo) {
        let _ = self.queue(id).send(Op::Scroll(scroll));
    }

    fn close(&self, id: TerminalId) {
        let _ = self.queue(id).send(Op::Close);
        self.queues.borrow_mut().remove(&id);
    }
}
