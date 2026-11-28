// SPDX-License-Identifier: Apache-2.0

//! Questions in the global database (`questions`; global migration 5).
//!
//! Synchronous and short: the service calls it under its lock, so changes
//! get their `change_seq` in the order they are published.

use std::path::Path;

use coqo_anchors::Anchor;
use coqo_protocol::methods::agents::SessionId;
use coqo_protocol::methods::questions::{
    QuestionAnswer, QuestionAuthor, QuestionId, QuestionKind, QuestionOption, QuestionStatus,
    Severity, ThreadMessage,
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

/// One code location of a stored question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredAnchor {
    /// The root the anchor's path is relative to.
    pub root: RootId,
    /// As asked: the original snippet and fingerprint (`coqo-anchors`).
    pub anchor: Anchor,
    /// Rebased onto the code where it was last found unchanged, if it
    /// moved since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Anchor>,
}

/// A question row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Record {
    pub id: QuestionId,
    pub workspace: WorkspaceId,
    pub session: SessionId,
    pub author: QuestionAuthor,
    pub kind: QuestionKind,
    pub severity: Option<Severity>,
    pub body: String,
    pub options: Vec<QuestionOption>,
    pub anchors: Vec<StoredAnchor>,
    /// A user's thread: their messages (empty for an agent's question).
    pub thread: Vec<ThreadMessage>,
    pub status: QuestionStatus,
    pub answer: Option<QuestionAnswer>,
    pub created_at: i64,
    pub closed_at: Option<i64>,
    /// The change seq of this state.
    pub seq: u64,
}

/// What [`Store::insert`] needs.
#[derive(Clone, Debug)]
pub(crate) struct NewRecord {
    pub workspace: WorkspaceId,
    pub session: SessionId,
    pub author: QuestionAuthor,
    pub kind: QuestionKind,
    pub severity: Option<Severity>,
    pub body: String,
    pub options: Vec<QuestionOption>,
    pub anchors: Vec<StoredAnchor>,
    pub created_at: i64,
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// A unit enum as its serde name (`decision`, `open`).
fn name_of<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn parse_name<T: DeserializeOwned>(s: &str) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })
}

fn parse_json<T: DeserializeOwned>(col: usize, s: &str) -> rusqlite::Result<T> {
    serde_json::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, e.into())
    })
}

const SELECT: &str = "SELECT id, workspace, session, kind, severity, body, options, anchors, \
                      status, answer, created_at, closed_at, change_seq, author, thread \
                      FROM questions";

fn record_row(row: &Row<'_>) -> rusqlite::Result<Record> {
    let kind: String = row.get(3)?;
    let severity: Option<String> = row.get(4)?;
    let options: String = row.get(6)?;
    let anchors: String = row.get(7)?;
    let status: String = row.get(8)?;
    let answer: Option<String> = row.get(9)?;
    let author: String = row.get(13)?;
    let thread: Option<String> = row.get(14)?;
    Ok(Record {
        id: QuestionId(u64_of(row.get(0)?)),
        workspace: WorkspaceId(row.get(1)?),
        session: SessionId(row.get(2)?),
        author: parse_name(&author)?,
        kind: parse_name(&kind)?,
        severity: severity.as_deref().map(parse_name).transpose()?,
        body: row.get(5)?,
        options: parse_json(6, &options)?,
        anchors: parse_json(7, &anchors)?,
        thread: thread
            .as_deref()
            .map(|t| parse_json(14, t))
            .transpose()?
            .unwrap_or_default(),
        status: parse_name(&status)?,
        answer: answer.as_deref().map(|a| parse_json(9, a)).transpose()?,
        created_at: row.get(10)?,
        closed_at: row.get(11)?,
        seq: u64_of(row.get(12)?),
    })
}

/// The questions table.
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
        let head: Option<i64> =
            self.db
                .connection()
                .query_row("SELECT MAX(change_seq) FROM questions", [], |r| r.get(0))?;
        Ok(head.map_or(0, u64_of))
    }

    /// Adds a question as change `seq`.
    pub fn insert(&self, new: &NewRecord, seq: u64) -> Result<Record> {
        let conn = self.db.connection();
        conn.execute(
            "INSERT INTO questions (workspace, session, kind, severity, body, options, anchors, \
             status, created_at, change_seq, author) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                new.workspace.0,
                new.session.0,
                name_of(&new.kind),
                new.severity.as_ref().map(name_of),
                new.body,
                serde_json::to_string(&new.options)?,
                serde_json::to_string(&new.anchors)?,
                name_of(&QuestionStatus::Open),
                new.created_at,
                i64_of(seq),
                name_of(&new.author),
            ],
        )?;
        let id = QuestionId(u64_of(conn.last_insert_rowid()));
        Ok(Record {
            id,
            workspace: new.workspace.clone(),
            session: new.session.clone(),
            author: new.author,
            kind: new.kind,
            severity: new.severity,
            body: new.body.clone(),
            options: new.options.clone(),
            anchors: new.anchors.clone(),
            thread: Vec::new(),
            status: QuestionStatus::Open,
            answer: None,
            created_at: new.created_at,
            closed_at: None,
            seq,
        })
    }

    /// One question.
    pub fn get(&self, id: QuestionId) -> Result<Option<Record>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
            .query_row([i64_of(id.0)], record_row)
            .optional()?)
    }

    /// Writes `record`'s status, answer, anchors and thread as change `seq`.
    pub fn update(&self, record: &Record, seq: u64) -> Result<()> {
        self.db.connection().execute(
            "UPDATE questions SET status = ?2, answer = ?3, closed_at = ?4, anchors = ?5, \
             change_seq = ?6, thread = ?7 WHERE id = ?1",
            params![
                i64_of(record.id.0),
                name_of(&record.status),
                record
                    .answer
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                record.closed_at,
                serde_json::to_string(&record.anchors)?,
                i64_of(seq),
                (!record.thread.is_empty())
                    .then(|| serde_json::to_string(&record.thread))
                    .transpose()?,
            ],
        )?;
        Ok(())
    }

    /// Writes `record`'s anchors without a change (a rebase nobody needs to
    /// hear about).
    pub fn save_anchors(&self, record: &Record) -> Result<()> {
        self.db.connection().execute(
            "UPDATE questions SET anchors = ?2 WHERE id = ?1",
            params![i64_of(record.id.0), serde_json::to_string(&record.anchors)?],
        )?;
        Ok(())
    }

    /// A workspace's open questions, oldest first, then (with
    /// `include_closed`) up to `closed_limit` closed ones, most recently
    /// closed first.
    pub fn list(
        &self,
        workspace: &WorkspaceId,
        session: Option<&SessionId>,
        include_closed: bool,
        closed_limit: usize,
    ) -> Result<Vec<Record>> {
        let conn = self.db.connection();
        let session = session.map(|s| s.0.as_str());
        let mut out: Vec<Record> = conn
            .prepare_cached(&format!(
                "{SELECT} WHERE workspace = ?1 AND status = 'open' \
                 AND (?2 IS NULL OR session = ?2) ORDER BY id"
            ))?
            .query_map(params![workspace.0, session], record_row)?
            .collect::<rusqlite::Result<_>>()?;
        if include_closed {
            let closed = conn
                .prepare_cached(&format!(
                    "{SELECT} WHERE workspace = ?1 AND status != 'open' \
                     AND (?2 IS NULL OR session = ?2) ORDER BY closed_at DESC, id DESC LIMIT ?3"
                ))?
                .query_map(
                    params![
                        workspace.0,
                        session,
                        i64::try_from(closed_limit).unwrap_or(0)
                    ],
                    record_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.extend(closed);
        }
        Ok(out)
    }

    /// Every question of a workspace, open or closed, oldest first (for
    /// re-keying after a rename).
    pub fn all_of(&self, workspace: &WorkspaceId) -> Result<Vec<Record>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!("{SELECT} WHERE workspace = ?1 ORDER BY id"))?
            .query_map([&workspace.0], record_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Questions changed after `seq`, oldest change first, at most `limit`.
    pub fn changed_since(
        &self,
        seq: u64,
        workspace: Option<&WorkspaceId>,
        limit: usize,
    ) -> Result<Vec<Record>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!(
                "{SELECT} WHERE change_seq > ?1 AND (?2 IS NULL OR workspace = ?2) \
                 ORDER BY change_seq LIMIT ?3"
            ))?
            .query_map(
                params![
                    i64_of(seq),
                    workspace.map(|w| w.0.as_str()),
                    i64::try_from(limit).unwrap_or(0)
                ],
                record_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The open questions (not notes, nor threads the user started) of
    /// `session`, oldest first: what it waits on the user for.
    pub fn open_questions_of(&self, session: &SessionId) -> Result<Vec<Record>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!(
                "{SELECT} WHERE session = ?1 AND status = 'open' AND kind != 'annotation' \
                 AND author = 'agent' ORDER BY id"
            ))?
            .query_map([&session.0], record_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use coqo_anchors::Snapshot;

    use super::*;

    fn new_record(workspace: &str, session: &str, kind: QuestionKind) -> NewRecord {
        let snap = Snapshot::new("fn main() {\n    let x = 1;\n}\n");
        NewRecord {
            workspace: WorkspaceId(workspace.into()),
            session: SessionId(session.into()),
            author: QuestionAuthor::Agent,
            kind,
            severity: (kind == QuestionKind::Annotation).then_some(Severity::Warning),
            body: "Why **1**?".into(),
            options: vec![QuestionOption {
                id: "1".into(),
                label: "Because".into(),
                description: None,
            }],
            anchors: vec![StoredAnchor {
                root: RootId(0),
                anchor: Anchor::lines("src/main.rs", &snap, 1..=1).unwrap(),
                current: None,
            }],
            created_at: 10,
        }
    }

    #[test]
    fn questions_round_trip_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coqo.db");
        let q = {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.head().unwrap(), 0);
            let q = store
                .insert(&new_record("ws", "s-1", QuestionKind::Decision), 1)
                .unwrap();
            let n = store
                .insert(&new_record("ws", "s-1", QuestionKind::Annotation), 2)
                .unwrap();
            assert!(n.id > q.id);
            let mut answered = q.clone();
            answered.status = QuestionStatus::Answered;
            answered.answer = Some(QuestionAnswer {
                option: Some("1".into()),
                text: Some("ok".into()),
            });
            answered.closed_at = Some(20);
            answered.anchors[0].current = Some(answered.anchors[0].anchor.clone());
            store.update(&answered, 3).unwrap();
            answered.seq = 3;
            answered
        };
        // A thread the user started, with a follow-up.
        let thread = {
            let store = Store::open(&path).unwrap();
            let mut new = new_record("ws", "s-1", QuestionKind::Clarification);
            new.author = QuestionAuthor::User;
            let mut t = store.insert(&new, 4).unwrap();
            t.thread = vec![
                ThreadMessage {
                    text: "Why 1?".into(),
                    turn: 1,
                    at: 11,
                },
                ThreadMessage {
                    text: "And 2?".into(),
                    turn: 2,
                    at: 12,
                },
            ];
            store.update(&t, 5).unwrap();
            t.seq = 5;
            t
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.head().unwrap(), 5);
        assert_eq!(store.get(q.id).unwrap().unwrap(), q);
        assert_eq!(store.get(thread.id).unwrap().unwrap(), thread);
        let ws = WorkspaceId("ws".into());
        let open = store.list(&ws, None, false, 10).unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].kind, QuestionKind::Annotation);
        assert_eq!(open[0].severity, Some(Severity::Warning));
        assert_eq!(open[1].author, QuestionAuthor::User);
        assert_eq!(store.list(&ws, None, true, 10).unwrap().len(), 3);
        assert!(
            store
                .list(&ws, Some(&SessionId("other".into())), true, 10)
                .unwrap()
                .is_empty()
        );
        let changed = store.changed_since(1, None, 10).unwrap();
        assert_eq!(
            changed.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [2, 3, 5],
            "oldest change first"
        );
        assert!(
            store
                .changed_since(0, Some(&WorkspaceId("other".into())), 10)
                .unwrap()
                .is_empty()
        );
        // Neither the annotation nor the user's thread is a question
        // waiting for an answer.
        assert!(
            store
                .open_questions_of(&SessionId("s-1".into()))
                .unwrap()
                .is_empty()
        );
    }
}
