// SPDX-License-Identifier: Apache-2.0

//! `workspace/events`, backed by the persistent event log.
//!
//! Every workspace change is appended to the event log (kind
//! `workspace.<event>`, payload the JSON [`WorkspaceEvent`]) in the same
//! transaction as the change itself. Subscribers read the log from their
//! `since` and then follow it: the service bumps a `watch` channel with the
//! new head after each commit, and each subscriber reads what it has not
//! seen yet. Nothing is buffered per subscriber, so a slow one only lags; it
//! fails with `RESUME_GAP` only if retention prunes events it still needs.

use std::collections::VecDeque;
use std::sync::Arc;

use coqo_protocol::methods::workspace::{WorkspaceEvent, WorkspaceId};
use coqo_protocol::subscription::ResumeGap;
use coqo_protocol::{ErrorObject, EventFeed, Sequenced};
use coqo_storage::{EventFilter, NewEvent, Seq, StorageError};
use futures_util::StreamExt;
use futures_util::stream;
use tokio::sync::watch;

use super::WorkspaceService;
use super::error;

/// Event-log `kind` prefix of workspace events.
pub(crate) const KIND_PREFIX: &str = "workspace.";

/// Events read from the log per round trip.
const PAGE: usize = 256;

pub(crate) fn kind(event: &WorkspaceEvent) -> &'static str {
    match event {
        WorkspaceEvent::Opened { .. } => "workspace.opened",
        WorkspaceEvent::Closed { .. } => "workspace.closed",
        WorkspaceEvent::Changed { .. } => "workspace.changed",
        WorkspaceEvent::Removed { .. } => "workspace.removed",
        WorkspaceEvent::StateChanged { .. } => "workspace.stateChanged",
    }
}

/// Append `event` for workspace `id`. Call inside the change's transaction.
pub(crate) fn append(
    conn: &rusqlite::Connection,
    id: &WorkspaceId,
    event: &WorkspaceEvent,
) -> Result<Seq, StorageError> {
    let payload = serde_json::to_vec(event).unwrap_or_default();
    coqo_storage::EventLog::new(conn).append(&NewEvent {
        workspace: Some(&id.0),
        kind: kind(event),
        payload: &payload,
    })
}

fn gap(needed: u64, oldest: Option<Seq>, head: u64) -> ErrorObject {
    ResumeGap {
        needed,
        oldest: oldest.map_or(head + 1, |s| s.0),
        head,
    }
    .to_error()
}

struct Cursor {
    svc: Arc<WorkspaceService>,
    filter: Option<WorkspaceId>,
    head_rx: watch::Receiver<u64>,
    after: u64,
    buf: VecDeque<Sequenced<WorkspaceEvent>>,
    done: bool,
}

enum Page {
    Events {
        last: u64,
        events: Vec<Sequenced<WorkspaceEvent>>,
    },
    Gap(ErrorObject),
}

fn read_page(
    svc: &WorkspaceService,
    filter: Option<&WorkspaceId>,
    after: u64,
) -> Result<Page, ErrorObject> {
    let db = svc.db();
    let log = db.events();
    let filter = filter.map_or(EventFilter::All, |w| EventFilter::Workspace(&w.0));
    match log.read_since(Seq(after), filter, PAGE) {
        Ok(rows) => {
            let last = rows.last().map_or(after, |e| e.seq.0);
            let events = rows
                .into_iter()
                .filter(|e| e.kind.starts_with(KIND_PREFIX))
                .filter_map(|e| match serde_json::from_slice(&e.payload) {
                    Ok(event) => Some(Sequenced { seq: e.seq.0, event }),
                    Err(err) => {
                        tracing::warn!(seq = e.seq.0, kind = e.kind, error = %err, "skipping undecodable workspace event");
                        None
                    }
                })
                .collect();
            Ok(Page::Events { last, events })
        }
        Err(StorageError::EventsPruned {
            after,
            first_available,
        }) => {
            let head = log.head().map_err(error::storage)?.0;
            Ok(Page::Gap(gap(after.0 + 1, first_available, head)))
        }
        Err(e) => Err(error::storage(e)),
    }
}

/// Subscribe to workspace events (see the module docs).
pub(crate) async fn subscribe(
    svc: Arc<WorkspaceService>,
    filter: Option<WorkspaceId>,
    since: Option<u64>,
) -> Result<EventFeed<WorkspaceEvent>, ErrorObject> {
    let s = Arc::clone(&svc);
    let f = filter.clone();
    let (head, after, head_rx, first) = tokio::task::spawn_blocking(move || {
        // Under the db lock no change can commit between reading the head
        // and subscribing to head updates.
        let db = s.db();
        let log = db.events();
        let head = log.head().map_err(error::storage)?.0;
        let head_rx = s.head.subscribe();
        drop(db);
        let after = match since {
            None => head,
            Some(n) if n > head => {
                let oldest = s.db().events().first().map_err(error::storage)?;
                return Err(gap(n + 1, oldest, head));
            }
            Some(n) => n,
        };
        // Read the first page now so a pruned `since` fails the subscribe.
        let first = match since {
            Some(_) => match read_page(&s, f.as_ref(), after)? {
                Page::Gap(e) => return Err(e),
                page => Some(page),
            },
            None => None,
        };
        Ok((head, after, head_rx, first))
    })
    .await
    .map_err(error::join)??;

    let mut cursor = Cursor {
        svc,
        filter,
        head_rx,
        after,
        buf: VecDeque::new(),
        done: false,
    };
    if let Some(Page::Events { last, events }) = first {
        cursor.after = last;
        cursor.buf.extend(events);
    }

    let events = stream::unfold(cursor, |mut c| async move {
        loop {
            if c.done {
                return None;
            }
            if let Some(ev) = c.buf.pop_front() {
                return Some((Ok(ev), c));
            }
            let after = c.after;
            // Everything committed up to `seen` is visible to the read below.
            let seen = *c.head_rx.borrow_and_update();
            if seen <= after {
                if c.head_rx.wait_for(|h| *h > after).await.is_err() {
                    return None;
                }
                continue;
            }
            let svc = Arc::clone(&c.svc);
            let filter = c.filter.clone();
            let page =
                tokio::task::spawn_blocking(move || read_page(&svc, filter.as_ref(), after)).await;
            match page.map_err(error::join).and_then(|p| p) {
                Ok(Page::Events { last, events }) => {
                    // An empty page means nothing up to `seen` matches the
                    // filter (other workspaces' events), so skip past it.
                    c.after = if events.is_empty() && last == after {
                        seen
                    } else {
                        last
                    };
                    c.buf.extend(events);
                }
                Ok(Page::Gap(e)) | Err(e) => {
                    c.done = true;
                    return Some((Err(e), c));
                }
            }
        }
    })
    .boxed();
    Ok(EventFeed { head, events })
}
