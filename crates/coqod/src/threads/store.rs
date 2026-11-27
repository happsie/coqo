// SPDX-License-Identifier: Apache-2.0

//! Threads and their comments in the global database (`review_threads`,
//! `review_comments`; global migration 12).
//!
//! Synchronous and short: the service calls it under its lock, so changes
//! get their `change_seq` in the order they are published. A deleted
//! thread keeps its row (`deleted`, no comments) for replaying the
//! deletion.

use std::path::Path;

use coqo_anchors::Anchor;
use coqo_protocol::methods::git::DiffSide;
use coqo_protocol::methods::threads::{
    Author, Backend, Comment, CommentId, ThreadId, ThreadSide, ThreadStatus,
};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_storage::{GlobalDb, StorageError};
use rusqlite::{OptionalExtension, Row, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A store failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Opening or migrating the database failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A query failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// A stored value could not be (de)serialized.
    #[error("bad stored value: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) type Result<T> = std::result::Result<T, StoreError>;

/// Where a stored thread is pinned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredAnchor {
    pub root: RootId,
    /// Root-relative.
    pub path: String,
    pub side: ThreadSide,
    pub version: DiffSide,
    /// The lines as the thread was started on them (`coqo-anchors`); absent
    /// for a thread on the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    /// Rebased onto the code where it was last found unchanged, if it moved
    /// since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Anchor>,
}

/// A thread row with its comments.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Record {
    pub id: ThreadId,
    pub workspace: WorkspaceId,
    pub backend: Backend,
    pub diff: Option<String>,
    pub anchor: StoredAnchor,
    pub status: ThreadStatus,
    /// Oldest first.
    pub comments: Vec<Comment>,
    pub created_at: i64,
    pub updated_at: i64,
    pub resolved_at: Option<i64>,
    pub deleted: bool,
    /// The change seq of this state.
    pub seq: u64,
}

/// What [`Store::create`] needs.
#[derive(Clone, Debug)]
pub(crate) struct NewThread {
    pub workspace: WorkspaceId,
    pub backend: Backend,
    pub diff: Option<String>,
    pub anchor: StoredAnchor,
    pub created_at: i64,
}

/// A comment to add.
#[derive(Clone, Debug)]
pub(crate) struct NewComment {
    pub author: Author,
    pub body: String,
    pub pending: bool,
    pub created_at: i64,
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// A unit enum as its serde name (`local`, `open`).
fn name_of<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn parse_name<T: DeserializeOwned>(col: usize, s: &str) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, e.into())
    })
}

fn parse_json<T: DeserializeOwned>(col: usize, s: &str) -> rusqlite::Result<T> {
    serde_json::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, e.into())
    })
}

const SELECT: &str = "SELECT id, workspace, backend, diff, anchor, status, created_at, \
                      updated_at, resolved_at, deleted, change_seq FROM review_threads";

fn thread_row(row: &Row<'_>) -> rusqlite::Result<Record> {
    let backend: String = row.get(2)?;
    let anchor: String = row.get(4)?;
    let status: String = row.get(5)?;
    Ok(Record {
        id: ThreadId(u64_of(row.get(0)?)),
        workspace: WorkspaceId(row.get(1)?),
        backend: parse_name(2, &backend)?,
        diff: row.get(3)?,
        anchor: parse_json(4, &anchor)?,
        status: parse_name(5, &status)?,
        comments: Vec::new(),
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        resolved_at: row.get(8)?,
        deleted: row.get(9)?,
        seq: u64_of(row.get(10)?),
    })
}

const SELECT_COMMENT: &str =
    "SELECT id, thread, author, body, pending, created_at, edited_at FROM review_comments";

fn comment_row(row: &Row<'_>) -> rusqlite::Result<(ThreadId, Comment)> {
    let author: String = row.get(2)?;
    Ok((
        ThreadId(u64_of(row.get(1)?)),
        Comment {
            id: CommentId(u64_of(row.get(0)?)),
            author: parse_json(2, &author)?,
            body: row.get(3)?,
            pending: row.get(4)?,
            created_at: row.get(5)?,
            edited_at: row.get(6)?,
        },
    ))
}

/// The threads tables.
#[derive(Debug)]
pub(crate) struct Store {
    db: GlobalDb,
}

impl Store {
    /// Opens (creating and migrating if needed) the global database at
    /// `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            db: GlobalDb::open_path(path)?,
        })
    }

    /// The latest change seq (0 if none).
    pub fn head(&self) -> Result<u64> {
        let head: Option<i64> = self.db.connection().query_row(
            "SELECT MAX(change_seq) FROM review_threads",
            [],
            |r| r.get(0),
        )?;
        Ok(head.map_or(0, u64_of))
    }

    /// Start a thread with its first comment, as change `seq`.
    pub fn create(&self, new: &NewThread, first: &NewComment, seq: u64) -> Result<Record> {
        let conn = self.db.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO review_threads (workspace, backend, diff, anchor, status, created_at, \
             updated_at, change_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            params![
                new.workspace.0,
                name_of(&new.backend),
                new.diff,
                serde_json::to_string(&new.anchor)?,
                name_of(&ThreadStatus::Open),
                new.created_at,
                i64_of(seq),
            ],
        )?;
        let id = ThreadId(u64_of(tx.last_insert_rowid()));
        let comment = insert_comment(&tx, id, first)?;
        tx.commit()?;
        Ok(Record {
            id,
            workspace: new.workspace.clone(),
            backend: new.backend,
            diff: new.diff.clone(),
            anchor: new.anchor.clone(),
            status: ThreadStatus::Open,
            comments: vec![comment],
            created_at: new.created_at,
            updated_at: new.created_at,
            resolved_at: None,
            deleted: false,
            seq,
        })
    }

    /// One thread with its comments (a deleted one too).
    pub fn get(&self, id: ThreadId) -> Result<Option<Record>> {
        let conn = self.db.connection();
        let Some(mut record) = conn
            .prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
            .query_row([i64_of(id.0)], thread_row)
            .optional()?
        else {
            return Ok(None);
        };
        record.comments = conn
            .prepare_cached(&format!("{SELECT_COMMENT} WHERE thread = ?1 ORDER BY id"))?
            .query_map([i64_of(id.0)], comment_row)?
            .map(|r| r.map(|(_, c)| c))
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(record))
    }

    /// The thread a comment is in.
    pub fn thread_of(&self, comment: CommentId) -> Result<Option<ThreadId>> {
        Ok(self
            .db
            .connection()
            .prepare_cached("SELECT thread FROM review_comments WHERE id = ?1")?
            .query_row([i64_of(comment.0)], |r| r.get::<_, i64>(0))
            .optional()?
            .map(|t| ThreadId(u64_of(t))))
    }

    /// Add a comment to `thread` (not a change by itself: [`Self::bump`]
    /// it).
    pub fn add_comment(&self, thread: ThreadId, new: &NewComment) -> Result<Comment> {
        insert_comment(self.db.connection(), thread, new)
    }

    /// Change a comment's text.
    pub fn edit_comment(&self, comment: CommentId, body: &str, at: i64) -> Result<()> {
        self.db.connection().execute(
            "UPDATE review_comments SET body = ?2, edited_at = ?3 WHERE id = ?1",
            params![i64_of(comment.0), body, at],
        )?;
        Ok(())
    }

    /// Delete comments.
    pub fn delete_comments(&self, comments: &[CommentId]) -> Result<()> {
        let conn = self.db.connection();
        let tx = conn.unchecked_transaction()?;
        {
            let mut delete = tx.prepare_cached("DELETE FROM review_comments WHERE id = ?1")?;
            for c in comments {
                delete.execute([i64_of(c.0)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Write `record`'s status, timestamps, deletion and anchor as change
    /// `seq`.
    pub fn bump(&self, record: &Record, seq: u64) -> Result<()> {
        self.db.connection().execute(
            "UPDATE review_threads SET status = ?2, updated_at = ?3, resolved_at = ?4, \
             deleted = ?5, anchor = ?6, change_seq = ?7 WHERE id = ?1",
            params![
                i64_of(record.id.0),
                name_of(&record.status),
                record.updated_at,
                record.resolved_at,
                record.deleted,
                serde_json::to_string(&record.anchor)?,
                i64_of(seq),
            ],
        )?;
        Ok(())
    }

    /// Write `record`'s anchor without a change (a rebase nobody needs to
    /// hear about).
    pub fn save_anchor(&self, record: &Record) -> Result<()> {
        self.db.connection().execute(
            "UPDATE review_threads SET anchor = ?2 WHERE id = ?1",
            params![i64_of(record.id.0), serde_json::to_string(&record.anchor)?],
        )?;
        Ok(())
    }

    /// A workspace's threads (not deleted), oldest first, with comments.
    pub fn list(&self, workspace: &WorkspaceId) -> Result<Vec<Record>> {
        let conn = self.db.connection();
        let mut records: Vec<Record> = conn
            .prepare_cached(&format!(
                "{SELECT} WHERE workspace = ?1 AND deleted = 0 ORDER BY id"
            ))?
            .query_map([&workspace.0], thread_row)?
            .collect::<rusqlite::Result<_>>()?;
        let comments: Vec<(ThreadId, Comment)> = conn
            .prepare_cached(&format!(
                "{SELECT_COMMENT} WHERE thread IN (SELECT id FROM review_threads \
                 WHERE workspace = ?1 AND deleted = 0) ORDER BY id"
            ))?
            .query_map([&workspace.0], comment_row)?
            .collect::<rusqlite::Result<_>>()?;
        attach(&mut records, comments);
        Ok(records)
    }

    /// Threads changed after `seq` (deleted ones too), oldest change first,
    /// at most `limit`.
    pub fn changed_since(
        &self,
        seq: u64,
        workspace: Option<&WorkspaceId>,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let conn = self.db.connection();
        let workspace = workspace.map(|w| w.0.as_str());
        let mut records: Vec<Record> = conn
            .prepare_cached(&format!(
                "{SELECT} WHERE change_seq > ?1 AND (?2 IS NULL OR workspace = ?2) \
                 ORDER BY change_seq LIMIT ?3"
            ))?
            .query_map(
                params![i64_of(seq), workspace, i64::try_from(limit).unwrap_or(0)],
                thread_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        for r in &mut records {
            r.comments = conn
                .prepare_cached(&format!("{SELECT_COMMENT} WHERE thread = ?1 ORDER BY id"))?
                .query_map([i64_of(r.id.0)], comment_row)?
                .map(|c| c.map(|(_, c)| c))
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(records)
    }
}

fn insert_comment(
    conn: &rusqlite::Connection,
    thread: ThreadId,
    new: &NewComment,
) -> Result<Comment> {
    conn.execute(
        "INSERT INTO review_comments (thread, author, body, pending, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            i64_of(thread.0),
            serde_json::to_string(&new.author)?,
            new.body,
            new.pending,
            new.created_at,
        ],
    )?;
    Ok(Comment {
        id: CommentId(u64_of(conn.last_insert_rowid())),
        author: new.author.clone(),
        body: new.body.clone(),
        pending: new.pending,
        created_at: new.created_at,
        edited_at: None,
    })
}

/// Put each comment (oldest first) on its thread.
fn attach(records: &mut [Record], comments: Vec<(ThreadId, Comment)>) {
    let at: std::collections::HashMap<ThreadId, usize> =
        records.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
    for (thread, comment) in comments {
        if let Some(&i) = at.get(&thread) {
            records[i].comments.push(comment);
        }
    }
}

#[cfg(test)]
mod tests {
    use coqo_anchors::Snapshot;
    use coqo_protocol::methods::agents::SessionId;

    use super::*;

    fn new_thread(workspace: &str) -> NewThread {
        let snap = Snapshot::new("fn main() {\n    let x = 1;\n}\n");
        NewThread {
            workspace: WorkspaceId(workspace.into()),
            backend: Backend::Local,
            diff: Some("git:0:index..worktree".into()),
            anchor: StoredAnchor {
                root: RootId(0),
                path: "src/main.rs".into(),
                side: ThreadSide::New,
                version: DiffSide::Worktree,
                anchor: Some(Anchor::lines("src/main.rs", &snap, 1..=1).unwrap()),
                current: None,
            },
            created_at: 10,
        }
    }

    fn comment(author: Author, body: &str, pending: bool) -> NewComment {
        NewComment {
            author,
            body: body.into(),
            pending,
            created_at: 11,
        }
    }

    #[test]
    fn threads_and_comments_round_trip_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coqo.db");
        let (a, b) = {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.head().unwrap(), 0);
            let a = store
                .create(&new_thread("ws"), &comment(Author::User, "Why 1?", true), 1)
                .unwrap();
            let b = store
                .create(
                    &new_thread("ws"),
                    &comment(Author::User, "And here", true),
                    2,
                )
                .unwrap();
            assert!(b.id > a.id && b.comments[0].id > a.comments[0].id);
            let reply = store
                .add_comment(
                    a.id,
                    &comment(
                        Author::Agent {
                            session: SessionId("s-1".into()),
                        },
                        "Because.",
                        false,
                    ),
                )
                .unwrap();
            assert_eq!(store.thread_of(reply.id).unwrap(), Some(a.id));
            store
                .edit_comment(a.comments[0].id, "Why one?", 20)
                .unwrap();
            let mut a = store.get(a.id).unwrap().unwrap();
            a.status = ThreadStatus::Resolved;
            a.resolved_at = Some(21);
            a.updated_at = 21;
            store.bump(&a, 3).unwrap();
            a.seq = 3;
            (a, b)
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.head().unwrap(), 3);
        let got = store.get(a.id).unwrap().unwrap();
        assert_eq!(got, a);
        assert_eq!(got.comments.len(), 2);
        assert_eq!(got.comments[0].body, "Why one?");
        assert_eq!(got.comments[0].edited_at, Some(20));
        assert!(got.comments[0].pending && !got.comments[1].pending);
        let listed = store.list(&WorkspaceId("ws".into())).unwrap();
        assert_eq!(
            listed.iter().map(|r| r.id).collect::<Vec<_>>(),
            [a.id, b.id]
        );
        assert_eq!(listed[1].comments.len(), 1);
        assert!(store.list(&WorkspaceId("other".into())).unwrap().is_empty());
        assert_eq!(
            store
                .changed_since(1, None, 10)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            [2, 3],
            "oldest change first"
        );

        // Deleting the last comment, then the thread: it is kept as a
        // tombstone for the replay, and listed no more.
        store.delete_comments(&[b.comments[0].id]).unwrap();
        let mut b = store.get(b.id).unwrap().unwrap();
        assert!(b.comments.is_empty());
        b.deleted = true;
        store.bump(&b, 4).unwrap();
        assert_eq!(store.list(&WorkspaceId("ws".into())).unwrap().len(), 1);
        let replay = store
            .changed_since(3, Some(&WorkspaceId("ws".into())), 10)
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert!(replay[0].deleted);
    }
}
