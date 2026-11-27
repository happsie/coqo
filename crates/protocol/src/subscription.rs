// SPDX-License-Identifier: Apache-2.0

//! Subscriptions with sequence numbers.
//!
//! A topic's events carry a sequence number (`seq`) that is **strictly
//! increasing** within that topic's source (it may skip values when the
//! topic params filter a larger log). `seq` starts at 1, so `0` means
//! "nothing seen yet".
//!
//! A client subscribes with `since`:
//!
//! - `since: None`: only events published after the subscription starts.
//! - `since: Some(n)`: every retained event with `seq > n`, then live
//!   events. If events after `n` are no longer retained, or `n` is ahead
//!   of the source, the subscribe fails with
//!   [`RESUME_GAP`](crate::codes::RESUME_GAP) and the client must take a
//!   fresh snapshot.
//!
//! The usual reattach flow (PLAN §4.3) is: fetch a snapshot that includes
//! the `seq` it reflects, then subscribe with `since: Some(seq)`.
//!
//! [`SeqLog`] is an in-memory event source with exactly these semantics; a
//! persistent event log can implement the same contract.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::message::{ErrorObject, codes};

/// Identifies a subscription on one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct SubscriptionId(pub u64);

impl fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Params of `coqo/subscribe`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SubscribeParams {
    /// Topic name.
    pub topic: String,
    /// Topic-specific params (`null` if none).
    #[serde(default)]
    pub params: Value,
    /// Resume after this sequence number; omit for live events only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

/// Result of `coqo/subscribe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SubscribeResult {
    /// Id used in `coqo/event` notifications and `coqo/unsubscribe`.
    pub subscription: SubscriptionId,
    /// The source's latest sequence number when the subscription started.
    pub head: u64,
}

/// Params of `coqo/unsubscribe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct UnsubscribeParams {
    /// The subscription to stop.
    pub subscription: SubscriptionId,
}

/// Params of `coqo/event`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EventParams {
    /// The subscription this event belongs to.
    pub subscription: SubscriptionId,
    /// The event's sequence number.
    pub seq: u64,
    /// The event, shaped by the topic.
    pub event: Value,
}

/// Params of `coqo/subscriptionEnded`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SubscriptionEndedParams {
    /// The subscription that ended.
    pub subscription: SubscriptionId,
    /// Why, if it ended abnormally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

/// `data` of a [`RESUME_GAP`](codes::RESUME_GAP) error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ResumeGap {
    /// The first sequence number the subscriber needed.
    pub needed: u64,
    /// The oldest sequence number still available (`head + 1` if none).
    pub oldest: u64,
    /// The source's latest sequence number.
    pub head: u64,
}

impl ResumeGap {
    /// The error object for this gap.
    pub fn to_error(self) -> ErrorObject {
        let message = if self.needed > self.head + 1 {
            format!(
                "cannot resume from seq {}: the source is only at {}; take a fresh snapshot",
                self.needed - 1,
                self.head
            )
        } else {
            format!(
                "cannot resume from seq {}: events before {} are no longer retained; take a fresh snapshot",
                self.needed.saturating_sub(1),
                self.oldest
            )
        };
        ErrorObject::new(codes::RESUME_GAP, message).with_data(self)
    }
}

/// An event with its sequence number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sequenced<T> {
    /// Sequence number, strictly increasing within the source.
    pub seq: u64,
    /// The event.
    pub event: T,
}

/// A stream of sequenced events. An `Err` item ends the stream.
pub type EventStream<T> = BoxStream<'static, Result<Sequenced<T>, ErrorObject>>;

/// What a topic handler returns: the head at subscribe time and the events.
pub struct EventFeed<T> {
    /// The source's latest sequence number when the feed was created.
    pub head: u64,
    /// Replayed events (if resuming) followed by live events.
    pub events: EventStream<T>,
}

impl<T> fmt::Debug for EventFeed<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventFeed")
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static> EventFeed<T> {
    /// Map each event, e.g. to filter fields or change its type.
    pub fn map<U, F>(self, mut f: F) -> EventFeed<U>
    where
        F: FnMut(T) -> U + Send + 'static,
    {
        EventFeed {
            head: self.head,
            events: self
                .events
                .map(move |r| {
                    r.map(|s| Sequenced {
                        seq: s.seq,
                        event: f(s.event),
                    })
                })
                .boxed(),
        }
    }
}

struct LogState<T> {
    /// Retained events, contiguous by seq, oldest first.
    events: VecDeque<(u64, T)>,
    head: u64,
}

impl<T> LogState<T> {
    fn oldest(&self) -> u64 {
        self.events.front().map_or(self.head + 1, |(s, _)| *s)
    }
}

/// An in-memory event source with sequence numbers and bounded replay.
///
/// Keeps the last `capacity` events for resume. A live subscriber that
/// falls more than `capacity` events behind gets a
/// [`RESUME_GAP`](codes::RESUME_GAP) error and must resubscribe (after a
/// fresh snapshot), so a slow client never makes the log grow without
/// bound.
pub struct SeqLog<T> {
    state: Mutex<LogState<T>>,
    head_tx: watch::Sender<u64>,
    capacity: usize,
}

impl<T> fmt::Debug for SeqLog<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("SeqLog")
            .field("head", &state.head)
            .field("oldest", &state.oldest())
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl<T> SeqLog<T> {
    fn lock(&self) -> MutexGuard<'_, LogState<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<T: Clone + Send + Sync + 'static> SeqLog<T> {
    /// An empty log retaining up to `capacity` events (at least 1).
    pub fn new(capacity: usize) -> Self {
        Self::starting_after(0, capacity)
    }

    /// An empty log whose first event gets `head + 1`, e.g. to continue
    /// numbering from a persisted log.
    pub fn starting_after(head: u64, capacity: usize) -> Self {
        let (head_tx, _) = watch::channel(head);
        Self {
            state: Mutex::new(LogState {
                events: VecDeque::new(),
                head,
            }),
            head_tx,
            capacity: capacity.max(1),
        }
    }

    /// Append an event and wake subscribers. Returns its sequence number.
    pub fn publish(&self, event: T) -> u64 {
        let seq = {
            let mut state = self.lock();
            state.head += 1;
            let seq = state.head;
            if state.events.len() == self.capacity {
                state.events.pop_front();
            }
            state.events.push_back((seq, event));
            seq
        };
        self.head_tx.send_replace(seq);
        seq
    }

    /// The latest sequence number (0 if nothing was published).
    pub fn head(&self) -> u64 {
        self.lock().head
    }

    /// The oldest retained sequence number (`head + 1` if none retained).
    pub fn oldest(&self) -> u64 {
        self.lock().oldest()
    }

    /// Subscribe, replaying from `since` if given (see the module docs).
    pub fn subscribe(self: &Arc<Self>, since: Option<u64>) -> Result<EventFeed<T>, ErrorObject> {
        let (head, next, rx) = {
            let state = self.lock();
            let head = state.head;
            let next = match since {
                None => head + 1,
                Some(n) => {
                    let needed = n.saturating_add(1);
                    let oldest = state.oldest();
                    if n > head || needed < oldest {
                        return Err(ResumeGap {
                            needed,
                            oldest,
                            head,
                        }
                        .to_error());
                    }
                    needed
                }
            };
            // Subscribing under the lock means no publish can slip between
            // reading `head` and watching for the next one.
            (head, next, self.head_tx.subscribe())
        };

        struct Cursor<T> {
            log: Arc<SeqLog<T>>,
            rx: watch::Receiver<u64>,
            next: u64,
            done: bool,
        }

        let cursor = Cursor {
            log: Arc::clone(self),
            rx,
            next,
            done: false,
        };
        let events = stream::unfold(cursor, |mut c| async move {
            if c.done {
                return None;
            }
            loop {
                {
                    let state = c.log.lock();
                    if c.next <= state.head {
                        let oldest = state.oldest();
                        if c.next < oldest {
                            let gap = ResumeGap {
                                needed: c.next,
                                oldest,
                                head: state.head,
                            };
                            drop(state);
                            c.done = true;
                            return Some((Err(gap.to_error()), c));
                        }
                        let (seq, event) = state.events[(c.next - oldest) as usize].clone();
                        drop(state);
                        c.next = seq + 1;
                        return Some((Ok(Sequenced { seq, event }), c));
                    }
                }
                // The sender lives in the log, which the cursor keeps alive,
                // so this only fails if the log is torn down.
                if c.rx.changed().await.is_err() {
                    return None;
                }
            }
        })
        .boxed();
        Ok(EventFeed { head, events })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    async fn take(feed: &mut EventFeed<u32>, n: usize) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        for _ in 0..n {
            let s = feed.events.next().await.unwrap().unwrap();
            out.push((s.seq, s.event));
        }
        out
    }

    #[tokio::test]
    async fn live_subscription_sees_only_new_events() {
        let log = Arc::new(SeqLog::new(16));
        log.publish(1);
        let mut feed = log.subscribe(None).unwrap();
        assert_eq!(feed.head, 1);
        log.publish(2);
        log.publish(3);
        assert_eq!(take(&mut feed, 2).await, vec![(2, 2), (3, 3)]);
    }

    #[tokio::test]
    async fn resume_replays_then_goes_live() {
        let log = Arc::new(SeqLog::new(16));
        for i in 1..=5 {
            log.publish(i * 10);
        }
        let mut feed = log.subscribe(Some(3)).unwrap();
        assert_eq!(feed.head, 5);
        assert_eq!(take(&mut feed, 2).await, vec![(4, 40), (5, 50)]);
        let publisher = {
            let log = Arc::clone(&log);
            tokio::spawn(async move { log.publish(60) })
        };
        assert_eq!(take(&mut feed, 1).await, vec![(6, 60)]);
        publisher.await.unwrap();
    }

    #[tokio::test]
    async fn resume_from_zero_and_from_head() {
        let log = Arc::new(SeqLog::new(16));
        log.publish(1);
        log.publish(2);
        let mut all = log.subscribe(Some(0)).unwrap();
        assert_eq!(take(&mut all, 2).await, vec![(1, 1), (2, 2)]);
        let mut at_head = log.subscribe(Some(2)).unwrap();
        log.publish(3);
        assert_eq!(take(&mut at_head, 1).await, vec![(3, 3)]);
    }

    #[test]
    fn resume_gap_when_evicted_or_ahead() {
        let log = Arc::new(SeqLog::new(2));
        for i in 0..5 {
            log.publish(i);
        }
        assert_eq!(log.oldest(), 4);
        let err = log.subscribe(Some(1)).unwrap_err();
        assert_eq!(err.code, codes::RESUME_GAP);
        let gap: ResumeGap = serde_json::from_value(err.data.unwrap()).unwrap();
        assert_eq!(
            gap,
            ResumeGap {
                needed: 2,
                oldest: 4,
                head: 5
            }
        );

        let err = log.subscribe(Some(9)).unwrap_err();
        assert_eq!(err.code, codes::RESUME_GAP);
        assert!(err.message.contains("only at 5"), "{}", err.message);

        assert!(log.subscribe(Some(3)).is_ok());
    }

    #[tokio::test]
    async fn slow_subscriber_gets_a_gap_error_and_the_stream_ends() {
        let log = Arc::new(SeqLog::new(2));
        let mut feed = log.subscribe(None).unwrap();
        for i in 0..5 {
            log.publish(i);
        }
        let err = feed.events.next().await.unwrap().unwrap_err();
        assert_eq!(err.code, codes::RESUME_GAP);
        assert!(feed.events.next().await.is_none());
    }

    #[tokio::test]
    async fn starting_after_continues_numbering() {
        let log = Arc::new(SeqLog::starting_after(100, 4));
        assert_eq!(log.head(), 100);
        let mut feed = log.subscribe(Some(100)).unwrap();
        assert_eq!(log.publish(7), 101);
        assert_eq!(take(&mut feed, 1).await, vec![(101, 7)]);
    }

    #[tokio::test]
    async fn concurrent_publishers_yield_strictly_increasing_contiguous_seqs() {
        let log = Arc::new(SeqLog::new(10_000));
        let mut feed = log.subscribe(Some(0)).unwrap();
        let mut tasks = Vec::new();
        for t in 0..4 {
            let log = Arc::clone(&log);
            tasks.push(tokio::spawn(async move {
                for i in 0..250 {
                    log.publish(t * 1000 + i);
                    tokio::task::yield_now().await;
                }
            }));
        }
        let mut last = 0;
        for _ in 0..1000 {
            let s = feed.events.next().await.unwrap().unwrap();
            assert_eq!(s.seq, last + 1);
            last = s.seq;
        }
        for t in tasks {
            t.await.unwrap();
        }
    }
}
