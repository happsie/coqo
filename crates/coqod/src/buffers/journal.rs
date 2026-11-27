// SPDX-License-Identifier: Apache-2.0

//! The buffer journal: unsaved edits in the workspace database, so they
//! survive daemon restarts (tables `buffer_journal` and
//! `buffer_journal_edits`, workspace migration 2).
//!
//! A buffer with unsaved edits has one snapshot row (disk baseline, unsaved
//! edits, revision, replicas' last seqs) and the changes since it as rows of
//! JSON `buffer/changes` events. A clean buffer has no rows.
//!
//! Writes are queued in order and written by one task in batches, one
//! transaction per workspace database per batch, off the async workers, so
//! a keystroke never waits for SQLite.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use coqo_buffers::Snapshot;
use coqo_protocol::methods::buffer::{BufferChange, TextEdit};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_storage::{DataDirs, StorageError, WorkspaceDb};
use rusqlite::{OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};

/// One file's journal: what [`load`] returns.
#[derive(Debug)]
pub(crate) struct Entry {
    pub snapshot: Snapshot,
    /// Changes after the snapshot, in order.
    pub changes: Vec<BufferChange>,
}

#[derive(Debug)]
enum Op {
    Snapshot {
        workspace: WorkspaceId,
        path: String,
        snapshot: Box<Snapshot>,
    },
    Change {
        workspace: WorkspaceId,
        path: String,
        change: Box<BufferChange>,
    },
    Delete {
        workspace: WorkspaceId,
        path: String,
    },
    Flush(oneshot::Sender<()>),
}

/// Queues journal writes. Cheap to clone.
#[derive(Clone, Debug)]
pub(crate) struct Journal {
    tx: mpsc::UnboundedSender<Op>,
}

impl Journal {
    /// Start the writer task. Must run inside a Tokio runtime.
    pub fn start(dirs: DataDirs) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(writer(dirs, rx));
        Self { tx }
    }

    fn send(&self, op: Op) {
        if self.tx.send(op).is_err() {
            tracing::error!(
                "the buffer journal writer has stopped; unsaved edits are not journaled"
            );
        }
    }

    /// Replace a file's journal with `snapshot`.
    pub fn snapshot(&self, workspace: &WorkspaceId, path: &str, snapshot: Snapshot) {
        self.send(Op::Snapshot {
            workspace: workspace.clone(),
            path: path.to_owned(),
            snapshot: Box::new(snapshot),
        });
    }

    /// Append a change after the file's snapshot.
    pub fn change(&self, workspace: &WorkspaceId, path: &str, change: BufferChange) {
        self.send(Op::Change {
            workspace: workspace.clone(),
            path: path.to_owned(),
            change: Box::new(change),
        });
    }

    /// Forget a file's journal (it has no unsaved edits).
    pub fn delete(&self, workspace: &WorkspaceId, path: &str) {
        self.send(Op::Delete {
            workspace: workspace.clone(),
            path: path.to_owned(),
        });
    }

    /// Wait until everything queued so far is written.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        self.send(Op::Flush(tx));
        let _ = rx.await;
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

async fn writer(dirs: DataDirs, mut rx: mpsc::UnboundedReceiver<Op>) {
    let mut dbs: HashMap<WorkspaceId, WorkspaceDb> = HashMap::new();
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(op) = rx.try_recv() {
            batch.push(op);
        }
        let dirs = dirs.clone();
        let result = tokio::task::spawn_blocking(move || {
            let flushed = write_batch(&dirs, &mut dbs, batch);
            (dbs, flushed)
        })
        .await;
        match result {
            Ok((back, flushed)) => {
                dbs = back;
                for tx in flushed {
                    let _ = tx.send(());
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "buffer journal batch failed");
                dbs = HashMap::new();
            }
        }
    }
}

/// Write a batch; returns the flush waiters to wake.
fn write_batch(
    dirs: &DataDirs,
    dbs: &mut HashMap<WorkspaceId, WorkspaceDb>,
    batch: Vec<Op>,
) -> Vec<oneshot::Sender<()>> {
    let mut flushed = Vec::new();
    let mut in_tx: Vec<WorkspaceId> = Vec::new();
    for op in batch {
        let (workspace, path) = match &op {
            Op::Flush(_) => {
                if let Op::Flush(tx) = op {
                    flushed.push(tx);
                }
                continue;
            }
            Op::Snapshot {
                workspace, path, ..
            }
            | Op::Change {
                workspace, path, ..
            }
            | Op::Delete { workspace, path } => (workspace.clone(), path.clone()),
        };
        if !dbs.contains_key(&workspace) {
            match WorkspaceDb::open(dirs, &workspace.0) {
                Ok(db) => {
                    dbs.insert(workspace.clone(), db);
                }
                Err(e) => {
                    tracing::error!(%workspace, error = %e, "cannot open the workspace database for the buffer journal");
                    continue;
                }
            }
        }
        let db = dbs.get_mut(&workspace).expect("inserted above");
        if !in_tx.contains(&workspace) {
            if let Err(e) = db.connection().execute_batch("BEGIN") {
                tracing::error!(%workspace, error = %e, "cannot start a journal transaction");
                continue;
            }
            in_tx.push(workspace.clone());
        }
        if let Err(e) = apply(db.connection(), &op) {
            tracing::error!(%workspace, path, error = %e, "cannot write the buffer journal");
        }
    }
    for workspace in in_tx {
        if let Some(db) = dbs.get(&workspace)
            && let Err(e) = db.connection().execute_batch("COMMIT")
        {
            tracing::error!(%workspace, error = %e, "cannot commit the buffer journal");
            let _ = db.connection().execute_batch("ROLLBACK");
        }
    }
    flushed
}

fn apply(conn: &rusqlite::Connection, op: &Op) -> rusqlite::Result<()> {
    match op {
        Op::Snapshot { path, snapshot, .. } => {
            let s = snapshot.as_ref();
            conn.execute(
                "INSERT INTO buffer_journal
                     (path, rev, saved_rev, disk, unsaved, encoding, line_ending, replicas, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (path) DO UPDATE SET
                     rev = excluded.rev, saved_rev = excluded.saved_rev, disk = excluded.disk,
                     unsaved = excluded.unsaved, encoding = excluded.encoding,
                     line_ending = excluded.line_ending, replicas = excluded.replicas,
                     updated_at = excluded.updated_at",
                params![
                    path,
                    i64::try_from(s.rev).unwrap_or(i64::MAX),
                    i64::try_from(s.saved_rev).unwrap_or(i64::MAX),
                    s.disk,
                    json(&s.unsaved),
                    json(&s.encoding),
                    json(&s.line_ending),
                    json(&s.replicas),
                    now_ms(),
                ],
            )?;
            conn.execute("DELETE FROM buffer_journal_edits WHERE path = ?1", [path])?;
        }
        Op::Change { path, change, .. } => {
            conn.execute(
                "INSERT OR REPLACE INTO buffer_journal_edits (path, rev, change) VALUES (?1, ?2, ?3)",
                params![
                    path,
                    i64::try_from(change.rev).unwrap_or(i64::MAX),
                    json(change.as_ref())
                ],
            )?;
        }
        Op::Delete { path, .. } => {
            conn.execute("DELETE FROM buffer_journal WHERE path = ?1", [path])?;
        }
        Op::Flush(_) => {}
    }
    Ok(())
}

/// Why a journal could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LoadError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error("corrupt journal entry: {0}")]
    Corrupt(#[from] serde_json::Error),
}

/// Read a file's journal, if it has one. Blocking.
pub(crate) fn load(
    dirs: &DataDirs,
    workspace: &WorkspaceId,
    path: &str,
) -> Result<Option<Entry>, LoadError> {
    let db = WorkspaceDb::open(dirs, &workspace.0)?;
    let conn = db.connection();
    type Row = (i64, i64, String, String, String, String, String);
    let row: Option<Row> = conn
        .query_row(
            "SELECT rev, saved_rev, disk, unsaved, encoding, line_ending, replicas
             FROM buffer_journal WHERE path = ?1",
            [path],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((rev, saved_rev, disk, unsaved, encoding, line_ending, replicas)) = row else {
        return Ok(None);
    };
    let snapshot = Snapshot {
        rev: u64::try_from(rev).unwrap_or(0),
        saved_rev: u64::try_from(saved_rev).unwrap_or(0),
        disk,
        unsaved: serde_json::from_str::<Vec<TextEdit>>(&unsaved)?,
        encoding: serde_json::from_str(&encoding)?,
        line_ending: serde_json::from_str(&line_ending)?,
        replicas: serde_json::from_str(&replicas)?,
    };
    let mut stmt = conn.prepare(
        "SELECT change FROM buffer_journal_edits WHERE path = ?1 AND rev > ?2 ORDER BY rev",
    )?;
    let changes = stmt
        .query_map(params![path, rev], |r| r.get::<_, String>(0))?
        .map(|c| Ok(serde_json::from_str::<BufferChange>(&c?)?))
        .collect::<Result<Vec<_>, LoadError>>()?;
    Ok(Some(Entry { snapshot, changes }))
}

/// Forget a file's journal right away (it could not be restored). Blocking.
pub(crate) fn discard(dirs: &DataDirs, workspace: &WorkspaceId, path: &str) {
    match WorkspaceDb::open(dirs, &workspace.0) {
        Ok(db) => {
            if let Err(e) = db
                .connection()
                .execute("DELETE FROM buffer_journal WHERE path = ?1", [path])
            {
                tracing::warn!(%workspace, path, error = %e, "cannot discard a buffer journal");
            }
        }
        Err(e) => tracing::warn!(%workspace, path, error = %e, "cannot discard a buffer journal"),
    }
}
