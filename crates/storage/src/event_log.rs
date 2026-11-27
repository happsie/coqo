// SPDX-License-Identifier: Apache-2.0

//! The daemon event log: an append-only, sequence-numbered record of what
//! happened, used to replay state to a UI that reattaches (PLAN §4).
//!
//! Every event gets a [`Seq`] that is strictly increasing and never reused,
//! even after [retention](EventLog::prune) deletes old rows. A reader that has
//! seen everything up to `n` asks for the events *after* `n`; if retention has
//! already removed some of those, it gets [`StorageError::EventsPruned`] and
//! must take a fresh snapshot instead of silently missing events.
//!
//! This module only persists and replays. Pushing new events to live
//! subscribers is the kernel's job: it appends here, then broadcasts.
//!
//! Payloads are opaque bytes; the protocol crate decides their encoding.

use std::fmt;
use std::ops::ControlFlow;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::{Result, StorageError};

/// Sequence number of an event. The first event is 1; `Seq::ZERO` means
/// "before the first event", which is where a brand-new reader starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Seq(pub u64);

impl Seq {
    /// The position before the first event.
    pub const ZERO: Seq = Seq(0);

    fn to_sql(self) -> i64 {
        // SQLite rowids are at most i64::MAX, so anything bigger is "after everything".
        i64::try_from(self.0).unwrap_or(i64::MAX)
    }

    fn from_sql(v: i64) -> Self {
        Seq(u64::try_from(v).unwrap_or(0))
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Wall-clock time in milliseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The current time.
    pub fn now() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Timestamp(ms)
    }

    /// This time minus `d`, saturating.
    pub fn saturating_sub(self, d: Duration) -> Self {
        let ms = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        Timestamp(self.0.saturating_sub(ms))
    }
}

/// An event to append. Borrowed so appending doesn't force a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewEvent<'a> {
    /// The workspace the event belongs to, or `None` for daemon-wide events.
    pub workspace: Option<&'a str>,
    /// Event kind, e.g. `"session.message"`. Defined by the protocol crate.
    pub kind: &'a str,
    /// Encoded event body.
    pub payload: &'a [u8],
}

/// A stored event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Position in the log.
    pub seq: Seq,
    /// The workspace the event belongs to, or `None` for daemon-wide events.
    pub workspace: Option<String>,
    /// Event kind.
    pub kind: String,
    /// Encoded event body.
    pub payload: Vec<u8>,
    /// When the event was appended.
    pub ts: Timestamp,
}

impl Event {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Event {
            seq: Seq::from_sql(row.get(0)?),
            workspace: row.get(1)?,
            kind: row.get(2)?,
            payload: row.get(3)?,
            ts: Timestamp(row.get(4)?),
        })
    }
}

/// Which events a read returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventFilter<'a> {
    /// Every event.
    #[default]
    All,
    /// Only events of this workspace.
    Workspace(&'a str),
}

/// What [`EventLog::prune`] keeps. A limit set to `None` is not applied; an
/// event is deleted if *any* limit says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Keep at most this many of the newest events.
    pub max_events: Option<u64>,
    /// Delete events older than this.
    pub max_age: Option<Duration>,
}

impl Retention {
    /// Keep everything.
    pub const KEEP_ALL: Retention = Retention {
        max_events: None,
        max_age: None,
    };
}

impl Default for Retention {
    /// Initial defaults (tunable): the newest 200 000 events, at most 14 days old.
    fn default() -> Self {
        Retention {
            max_events: Some(200_000),
            max_age: Some(Duration::from_secs(14 * 24 * 60 * 60)),
        }
    }
}

const SELECT_COLUMNS: &str = "SELECT seq, workspace, kind, payload, ts FROM event_log";

/// The event log in a [`GlobalDb`](crate::GlobalDb). Get one with
/// [`GlobalDb::events`](crate::GlobalDb::events).
#[derive(Debug, Clone, Copy)]
pub struct EventLog<'c> {
    conn: &'c Connection,
}

impl<'c> EventLog<'c> {
    /// Wrap a connection whose database has the global schema.
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Append one event stamped with the current time. Returns its sequence number.
    pub fn append(&self, event: &NewEvent<'_>) -> Result<Seq> {
        self.append_at(event, Timestamp::now())
    }

    /// Append one event with an explicit timestamp (imports, tests).
    pub fn append_at(&self, event: &NewEvent<'_>, ts: Timestamp) -> Result<Seq> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO event_log (workspace, kind, payload, ts) VALUES (?1, ?2, ?3, ?4)",
        )?;
        stmt.execute(params![event.workspace, event.kind, event.payload, ts.0])?;
        Ok(Seq::from_sql(self.conn.last_insert_rowid()))
    }

    /// Append several events atomically in one transaction, all stamped with
    /// the current time. Returns their sequence numbers in order.
    ///
    /// Much faster than calling [`append`](Self::append) in a loop, since it
    /// syncs once.
    pub fn append_batch(&self, events: &[NewEvent<'_>]) -> Result<Vec<Seq>> {
        let ts = Timestamp::now();
        let tx = self.conn.unchecked_transaction()?;
        let seqs = events
            .iter()
            .map(|e| self.append_at(e, ts))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(seqs)
    }

    /// The newest sequence number ever assigned, or [`Seq::ZERO`] if nothing
    /// was ever appended. Still counts events that retention has deleted, so a
    /// snapshot taken now is consistent with "everything up to `head()`".
    pub fn head(&self) -> Result<Seq> {
        let seq: Option<i64> = self
            .conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'event_log'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(seq.map_or(Seq::ZERO, Seq::from_sql))
    }

    /// The oldest event still stored, if any.
    pub fn first(&self) -> Result<Option<Seq>> {
        let seq: Option<i64> = self
            .conn
            .query_row("SELECT min(seq) FROM event_log", [], |r| r.get(0))?;
        Ok(seq.map(Seq::from_sql))
    }

    /// Number of events currently stored.
    pub fn len(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM event_log", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Whether the log currently stores no events.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Up to `limit` events with `seq > after`, oldest first.
    ///
    /// Fails with [`StorageError::EventsPruned`] if retention has deleted
    /// events right after `after` (the reader would miss them). Page through a
    /// long log by passing the last returned `seq` as the next `after`.
    pub fn read_since(
        &self,
        after: Seq,
        filter: EventFilter<'_>,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let mut out = Vec::with_capacity(limit.min(1024));
        self.replay_since(after, filter, |e| {
            out.push(e);
            if out.len() >= limit {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        if limit == 0 {
            out.clear();
        }
        Ok(out)
    }

    /// Stream every event with `seq > after` to `f`, oldest first, without
    /// collecting them. `f` can stop early with [`ControlFlow::Break`].
    ///
    /// Reads from a single snapshot, so concurrent appends from other
    /// connections are either all seen or not at all. Returns the `seq` of the
    /// last event passed to `f`, or `after` if there were none; continue from
    /// there later. Fails with [`StorageError::EventsPruned`] like
    /// [`read_since`](Self::read_since).
    pub fn replay_since<F>(&self, after: Seq, filter: EventFilter<'_>, mut f: F) -> Result<Seq>
    where
        F: FnMut(Event) -> ControlFlow<()>,
    {
        // A read transaction gives the continuity check and the scan the same snapshot.
        let tx = self.conn.unchecked_transaction()?;
        self.check_continuity(after)?;

        let mut last = after;
        let mut emit = |rows: &mut rusqlite::Rows<'_>| -> Result<()> {
            while let Some(row) = rows.next()? {
                let event = Event::from_row(row)?;
                last = event.seq;
                if f(event).is_break() {
                    break;
                }
            }
            Ok(())
        };
        match filter {
            EventFilter::All => {
                let mut stmt = self
                    .conn
                    .prepare_cached(&format!("{SELECT_COLUMNS} WHERE seq > ?1 ORDER BY seq"))?;
                emit(&mut stmt.query([after.to_sql()])?)?;
            }
            EventFilter::Workspace(ws) => {
                let mut stmt = self.conn.prepare_cached(&format!(
                    "{SELECT_COLUMNS} WHERE workspace = ?1 AND seq > ?2 ORDER BY seq"
                ))?;
                emit(&mut stmt.query(params![ws, after.to_sql()])?)?;
            }
        }
        tx.finish()?;
        Ok(last)
    }

    /// Fail if events with `seq > after` existed but the first of them was pruned.
    fn check_continuity(&self, after: Seq) -> Result<()> {
        let head = self.head()?;
        if after >= head {
            return Ok(());
        }
        let first = self.first()?;
        let contiguous = first.is_some_and(|first| first.0 <= after.0.saturating_add(1));
        if contiguous {
            Ok(())
        } else {
            Err(StorageError::EventsPruned {
                after,
                first_available: first,
            })
        }
    }

    /// Apply `retention` as of `now`, deleting events it doesn't keep.
    /// Returns how many events were deleted. Sequence numbers are not reused.
    pub fn prune(&self, retention: &Retention, now: Timestamp) -> Result<u64> {
        let tx = self.conn.unchecked_transaction()?;
        let mut deleted = 0;
        if let Some(max_age) = retention.max_age {
            let cutoff = now.saturating_sub(max_age);
            deleted += tx.execute("DELETE FROM event_log WHERE ts < ?1", [cutoff.0])?;
        }
        if let Some(max_events) = retention.max_events {
            let keep = i64::try_from(max_events).unwrap_or(i64::MAX);
            deleted += tx.execute(
                "DELETE FROM event_log WHERE seq <= (
                     SELECT seq FROM event_log ORDER BY seq DESC LIMIT 1 OFFSET ?1
                 )",
                [keep],
            )?;
        }
        tx.commit()?;
        Ok(deleted as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GlobalDb;

    fn ev<'a>(workspace: Option<&'a str>, kind: &'a str, payload: &'a [u8]) -> NewEvent<'a> {
        NewEvent {
            workspace,
            kind,
            payload,
        }
    }

    #[test]
    fn append_assigns_increasing_seqs_and_round_trips() {
        let db = GlobalDb::open_in_memory().unwrap();
        let log = db.events();
        assert_eq!(log.head().unwrap(), Seq::ZERO);
        assert!(log.is_empty().unwrap());

        let a = log.append(&ev(Some("ws"), "a", b"one")).unwrap();
        let b = log.append(&ev(None, "b", b"")).unwrap();
        assert_eq!((a, b), (Seq(1), Seq(2)));
        assert_eq!(log.head().unwrap(), Seq(2));

        let all = log.read_since(Seq::ZERO, EventFilter::All, 100).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, Seq(1));
        assert_eq!(all[0].workspace.as_deref(), Some("ws"));
        assert_eq!(all[0].kind, "a");
        assert_eq!(all[0].payload, b"one");
        assert_eq!(all[1].workspace, None);
        assert!(all[0].ts.0 > 0);
    }

    #[test]
    fn reads_are_exclusive_of_after_and_paged() {
        let db = GlobalDb::open_in_memory().unwrap();
        let log = db.events();
        let batch: Vec<_> = (0..10).map(|_| ev(None, "k", b"x")).collect();
        let seqs = log.append_batch(&batch).unwrap();
        assert_eq!(seqs, (1..=10).map(Seq).collect::<Vec<_>>());

        let page = log.read_since(Seq(3), EventFilter::All, 4).unwrap();
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Seq(4), Seq(5), Seq(6), Seq(7)]
        );
        assert!(
            log.read_since(Seq(3), EventFilter::All, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            log.read_since(Seq(10), EventFilter::All, 9)
                .unwrap()
                .is_empty()
        );
        assert!(
            log.read_since(Seq(99), EventFilter::All, 9)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn workspace_filter() {
        let db = GlobalDb::open_in_memory().unwrap();
        let log = db.events();
        log.append(&ev(Some("a"), "k", b"1")).unwrap();
        log.append(&ev(Some("b"), "k", b"2")).unwrap();
        log.append(&ev(None, "k", b"3")).unwrap();
        log.append(&ev(Some("a"), "k", b"4")).unwrap();

        let a = log
            .read_since(Seq::ZERO, EventFilter::Workspace("a"), 100)
            .unwrap();
        assert_eq!(
            a.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Seq(1), Seq(4)]
        );
        let last = log
            .replay_since(Seq(1), EventFilter::Workspace("a"), |_| {
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(last, Seq(4));
    }

    #[test]
    fn replay_can_stop_early() {
        let db = GlobalDb::open_in_memory().unwrap();
        let log = db.events();
        for _ in 0..5 {
            log.append(&ev(None, "k", b"")).unwrap();
        }
        let mut seen = 0;
        let last = log
            .replay_since(Seq::ZERO, EventFilter::All, |_| {
                seen += 1;
                if seen == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
        assert_eq!((seen, last), (2, Seq(2)));
        let none = log
            .replay_since(Seq(5), EventFilter::All, |_| ControlFlow::Continue(()))
            .unwrap();
        assert_eq!(none, Seq(5));
    }

    #[test]
    fn prune_by_count_keeps_the_newest_and_never_reuses_seqs() {
        let db = GlobalDb::open_in_memory().unwrap();
        let log = db.events();
        for _ in 0..10 {
            log.append(&ev(None, "k", b"")).unwrap();
        }
        let retention = Retention {
            max_events: Some(3),
            max_age: None,
        };
        assert_eq!(log.prune(&retention, Timestamp::now()).unwrap(), 7);
        assert_eq!(log.first().unwrap(), Some(Seq(8)));
        assert_eq!(log.len().unwrap(), 3);
        assert_eq!(log.prune(&retention, Timestamp::now()).unwrap(), 0);

        // Even after deleting everything, numbering continues from the head.
        let none = Retention {
            max_events: Some(0),
            max_age: None,
        };
        assert_eq!(log.prune(&none, Timestamp::now()).unwrap(), 3);
        assert_eq!(log.head().unwrap(), Seq(10));
        assert_eq!(log.append(&ev(None, "k", b"")).unwrap(), Seq(11));
    }

    #[test]
    fn prune_by_age() {
        let db = GlobalDb::open_in_memory().unwrap();
        let log = db.events();
        let day = Duration::from_secs(24 * 60 * 60);
        let now = Timestamp(1_000 * 24 * 60 * 60 * 1_000);
        log.append_at(&ev(None, "old", b""), now.saturating_sub(day * 3))
            .unwrap();
        log.append_at(&ev(None, "new", b""), now.saturating_sub(day / 2))
            .unwrap();
        let retention = Retention {
            max_events: None,
            max_age: Some(day),
        };
        assert_eq!(log.prune(&retention, now).unwrap(), 1);
        let left = log.read_since(Seq(1), EventFilter::All, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].kind, "new");
        assert_eq!(log.prune(&Retention::KEEP_ALL, now).unwrap(), 0);
    }

    #[test]
    fn reading_across_a_pruned_gap_is_an_error() {
        let db = GlobalDb::open_in_memory().unwrap();
        let log = db.events();
        for _ in 0..10 {
            log.append(&ev(None, "k", b"")).unwrap();
        }
        let keep_three = Retention {
            max_events: Some(3),
            max_age: None,
        };
        log.prune(&keep_three, Timestamp::now()).unwrap();

        // Seqs 1..=7 are gone: a reader that saw only up to 5 missed 6 and 7.
        let err = log.read_since(Seq(5), EventFilter::All, 10).unwrap_err();
        assert!(matches!(
            err,
            StorageError::EventsPruned {
                after: Seq(5),
                first_available: Some(Seq(8))
            }
        ));
        // A reader that saw up to 7 is fine, and so is one that is caught up.
        assert_eq!(
            log.read_since(Seq(7), EventFilter::All, 10).unwrap().len(),
            3
        );
        assert!(
            log.read_since(Seq(10), EventFilter::All, 10)
                .unwrap()
                .is_empty()
        );

        // With everything pruned, any reader behind the head has a gap.
        log.prune(
            &Retention {
                max_events: Some(0),
                max_age: None,
            },
            Timestamp::now(),
        )
        .unwrap();
        assert!(matches!(
            log.read_since(Seq(9), EventFilter::All, 10),
            Err(StorageError::EventsPruned {
                first_available: None,
                ..
            })
        ));
    }

    #[test]
    fn events_survive_reopen_and_are_visible_to_a_second_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = crate::DataDirs::at(tmp.path());
        let writer = GlobalDb::open(&dirs).unwrap();
        let reader = GlobalDb::open(&dirs).unwrap();
        writer.events().append(&ev(Some("w"), "k", b"p")).unwrap();
        assert_eq!(
            reader
                .events()
                .read_since(Seq::ZERO, EventFilter::All, 10)
                .unwrap()
                .len(),
            1
        );
        drop((writer, reader));
        let again = GlobalDb::open(&dirs).unwrap();
        assert_eq!(again.events().head().unwrap(), Seq(1));
    }
}
