// SPDX-License-Identifier: Apache-2.0

//! Subscription feeds over a persisted log: replay from the store, then live
//! events from a broadcast channel.
//!
//! Every event is written to the store *before* it is broadcast, and the
//! receiver is created before the replay starts, so nothing falls between
//! the two; events seen twice are dropped by seq. A subscriber that lags
//! behind the channel goes back to reading the store, so a slow UI never
//! loses events and never makes the daemon buffer without bound.

use std::collections::VecDeque;

use coqo_protocol::subscription::ResumeGap;
use coqo_protocol::{ErrorObject, EventFeed, Sequenced};
use futures_util::StreamExt;
use futures_util::stream;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

/// Reads up to a page of `(seq, event)` after a seq, oldest first.
pub(crate) type Page<T> = Box<dyn FnMut(u64) -> Result<Vec<(u64, T)>, ErrorObject> + Send>;

struct Cursor<T> {
    after: u64,
    replaying: bool,
    done: bool,
    buf: VecDeque<(u64, T)>,
    rx: broadcast::Receiver<Sequenced<T>>,
    page: Page<T>,
    keep: Box<dyn Fn(&T) -> bool + Send>,
}

/// A feed for a source at `head`, resuming after `since` (see
/// `coqo_protocol::subscription`). `rx` must have been subscribed before
/// `head` was read. `keep` filters live events (replay pages are filtered by
/// `page` itself).
pub(crate) fn feed<T>(
    head: u64,
    since: Option<u64>,
    rx: broadcast::Receiver<Sequenced<T>>,
    page: Page<T>,
    keep: impl Fn(&T) -> bool + Send + 'static,
) -> Result<EventFeed<T>, ErrorObject>
where
    T: Clone + Send + 'static,
{
    let (after, replaying) = match since {
        None => (head, false),
        Some(n) if n > head => {
            return Err(ResumeGap {
                needed: n.saturating_add(1),
                oldest: 1,
                head,
            }
            .to_error());
        }
        Some(n) => (n, n < head),
    };
    let cursor = Cursor {
        after,
        replaying,
        done: false,
        buf: VecDeque::new(),
        rx,
        page,
        keep: Box::new(keep),
    };
    let events = stream::unfold(cursor, |mut c| async move {
        if c.done {
            return None;
        }
        loop {
            if let Some((seq, event)) = c.buf.pop_front() {
                if seq > c.after {
                    c.after = seq;
                    return Some((Ok(Sequenced { seq, event }), c));
                }
                continue;
            }
            if c.replaying {
                match (c.page)(c.after) {
                    Ok(page) if page.is_empty() => c.replaying = false,
                    Ok(page) => c.buf.extend(page),
                    Err(e) => {
                        c.done = true;
                        return Some((Err(e), c));
                    }
                }
                continue;
            }
            match c.rx.recv().await {
                Ok(s) => {
                    if s.seq > c.after && (c.keep)(&s.event) {
                        c.after = s.seq;
                        return Some((Ok(s), c));
                    }
                }
                // Fell behind the channel: catch up from the store.
                Err(RecvError::Lagged(_)) => c.replaying = true,
                Err(RecvError::Closed) => return None,
            }
        }
    })
    .boxed();
    Ok(EventFeed { head, events })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use coqo_protocol::codes;

    /// A persisted log: a vec plus a broadcast channel.
    struct Log {
        events: Arc<Mutex<Vec<(u64, u32)>>>,
        tx: broadcast::Sender<Sequenced<u32>>,
    }

    impl Log {
        fn new(capacity: usize) -> Self {
            Self {
                events: Arc::default(),
                tx: broadcast::channel(capacity).0,
            }
        }
        fn publish(&self, event: u32) {
            let seq = {
                let mut events = self.events.lock().unwrap();
                let seq = events.len() as u64 + 1;
                events.push((seq, event));
                seq
            };
            let _ = self.tx.send(Sequenced { seq, event });
        }
        fn subscribe(&self, since: Option<u64>) -> Result<EventFeed<u32>, ErrorObject> {
            let rx = self.tx.subscribe();
            let head = self.events.lock().unwrap().len() as u64;
            let events = Arc::clone(&self.events);
            let page: Page<u32> = Box::new(move |after| {
                Ok(events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(s, _)| *s > after)
                    .take(2)
                    .copied()
                    .collect())
            });
            feed(head, since, rx, page, |_| true)
        }
    }

    async fn take(feed: &mut EventFeed<u32>, n: usize) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        for _ in 0..n {
            let s = feed.events.next().await.unwrap().unwrap();
            out.push((s.seq, s.event));
        }
        out
    }

    #[tokio::test]
    async fn replays_in_pages_then_goes_live() {
        let log = Log::new(16);
        for i in 1..=5 {
            log.publish(i * 10);
        }
        let mut f = log.subscribe(Some(1)).unwrap();
        assert_eq!(f.head, 5);
        log.publish(60);
        assert_eq!(
            take(&mut f, 5).await,
            [(2, 20), (3, 30), (4, 40), (5, 50), (6, 60)]
        );
        let mut live = log.subscribe(None).unwrap();
        log.publish(70);
        assert_eq!(take(&mut live, 1).await, [(7, 70)]);
        assert_eq!(take(&mut f, 1).await, [(7, 70)]);
    }

    #[tokio::test]
    async fn a_lagging_subscriber_catches_up_from_the_store() {
        let log = Log::new(2);
        let mut f = log.subscribe(Some(0)).unwrap();
        for i in 1..=10 {
            log.publish(i);
        }
        let got = take(&mut f, 10).await;
        assert_eq!(got, (1..=10).map(|i| (i, i as u32)).collect::<Vec<_>>());
    }

    #[test]
    fn resuming_ahead_of_the_source_is_a_gap() {
        let log = Log::new(4);
        log.publish(1);
        let err = log.subscribe(Some(5)).unwrap_err();
        assert_eq!(err.code, codes::RESUME_GAP);
        assert!(log.subscribe(Some(1)).is_ok());
    }
}
