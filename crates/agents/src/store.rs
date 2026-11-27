// SPDX-License-Identifier: Apache-2.0

//! Session persistence in the global database (`agent_sessions`,
//! `agent_session_updates`; global migration 3).
//!
//! Synchronous and short: the manager calls it while holding a session's
//! lock, so updates are written in the order they are streamed.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use coqo_protocol::methods::agents::{AgentState, SessionId, SessionInfo, SessionUpdate};
use coqo_storage::{GlobalDb, StorageError};
use rusqlite::{OptionalExtension, Row, params};

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

/// A session row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSession {
    /// The session as listed.
    pub info: SessionInfo,
    /// The provider's own session id, for resume.
    pub provider_session: Option<String>,
}

/// Agent sessions and their transcripts in `coqo.db`.
#[derive(Debug)]
pub struct SessionStore {
    db: Mutex<GlobalDb>,
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn state_str(state: AgentState) -> String {
    match serde_json::to_value(state) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "idle".to_owned(),
    }
}

fn parse_state(s: &str) -> AgentState {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).unwrap_or(AgentState::Error)
}

const SELECT: &str = "SELECT id, provider, workspace, cwd, title, state, state_reason, mode, \
                      closed, created_at, updated_at, provider_session, change_seq \
                      FROM agent_sessions";

fn session_row(row: &Row<'_>) -> rusqlite::Result<(StoredSession, u64)> {
    let state: String = row.get(5)?;
    Ok((
        StoredSession {
            info: SessionInfo {
                id: SessionId(row.get(0)?),
                provider: row.get(1)?,
                workspace: row.get(2)?,
                cwd: row.get(3)?,
                title: row.get(4)?,
                state: parse_state(&state),
                state_reason: row.get(6)?,
                mode: row.get(7)?,
                closed: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            },
            provider_session: row.get(11)?,
        },
        u64_of(row.get(12)?),
    ))
}

impl SessionStore {
    /// Opens (creating and migrating if needed) the global database at
    /// `path`, normally `DataDirs::global_db()`.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            db: Mutex::new(GlobalDb::open_path(path)?),
        })
    }

    /// A private in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            db: Mutex::new(GlobalDb::open_in_memory()?),
        })
    }

    fn db(&self) -> MutexGuard<'_, GlobalDb> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Inserts or updates a session row with change seq `change_seq`.
    pub fn save(&self, session: &StoredSession, change_seq: u64) -> Result<()> {
        let db = self.db();
        let i = &session.info;
        db.connection()
            .prepare_cached(
                "INSERT INTO agent_sessions (id, provider, workspace, cwd, title, state,
                     state_reason, mode, closed, provider_session, created_at, updated_at,
                     change_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT (id) DO UPDATE SET
                     title = excluded.title, state = excluded.state,
                     state_reason = excluded.state_reason, mode = excluded.mode,
                     closed = excluded.closed, provider_session = excluded.provider_session,
                     updated_at = excluded.updated_at, change_seq = excluded.change_seq",
            )?
            .execute(params![
                i.id.0,
                i.provider,
                i.workspace,
                i.cwd,
                i.title,
                state_str(i.state),
                i.state_reason,
                i.mode,
                i.closed,
                session.provider_session,
                i.created_at,
                i.updated_at,
                i64_of(change_seq),
            ])?;
        Ok(())
    }

    /// One session.
    pub fn get(&self, id: &SessionId) -> Result<Option<StoredSession>> {
        let db = self.db();
        let row = db
            .connection()
            .prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
            .query_row([&id.0], session_row)
            .optional()?;
        Ok(row.map(|(s, _)| s))
    }

    /// Sessions, most recently updated first.
    pub fn list(&self, workspace: Option<&str>, include_closed: bool) -> Result<Vec<SessionInfo>> {
        let db = self.db();
        let mut stmt = db.connection().prepare_cached(&format!(
            "{SELECT} WHERE (?1 IS NULL OR workspace = ?1) AND (?2 OR closed = 0)
             ORDER BY updated_at DESC, id"
        ))?;
        let rows = stmt
            .query_map(params![workspace, include_closed], session_row)?
            .map(|r| r.map(|(s, _)| s.info))
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Ids of sessions whose stored state says a turn was running.
    pub fn running(&self) -> Result<Vec<SessionId>> {
        let db = self.db();
        let mut stmt = db
            .connection()
            .prepare_cached("SELECT id FROM agent_sessions WHERE state IN (?1, ?2) ORDER BY id")?;
        let rows = stmt
            .query_map(
                params![
                    state_str(AgentState::Working),
                    state_str(AgentState::NeedsPermission)
                ],
                |r| r.get(0).map(SessionId),
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The newest change seq (0 if none).
    pub fn change_head(&self) -> Result<u64> {
        let db = self.db();
        let v: i64 = db.connection().query_row(
            "SELECT COALESCE(MAX(change_seq), 0) FROM agent_sessions",
            [],
            |r| r.get(0),
        )?;
        Ok(u64_of(v))
    }

    /// Up to `limit` sessions changed after `after`, oldest change first,
    /// each with its latest change seq.
    pub fn changes_since(
        &self,
        after: u64,
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(u64, SessionInfo)>> {
        let db = self.db();
        let mut stmt = db.connection().prepare_cached(&format!(
            "{SELECT} WHERE change_seq > ?1 AND (?2 IS NULL OR workspace = ?2)
             ORDER BY change_seq LIMIT ?3"
        ))?;
        let rows = stmt
            .query_map(
                params![i64_of(after), workspace, i64_of(limit as u64)],
                session_row,
            )?
            .map(|r| r.map(|(s, seq)| (seq, s.info)))
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Appends update `seq` to session `id`'s transcript.
    pub fn append(&self, id: &SessionId, seq: u64, update: &SessionUpdate, ts: i64) -> Result<()> {
        let payload = serde_json::to_value(update)?;
        let kind = payload
            .get("update")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let db = self.db();
        db.connection()
            .prepare_cached(
                "INSERT INTO agent_session_updates (session, seq, kind, payload, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![id.0, i64_of(seq), kind, payload.to_string(), ts])?;
        Ok(())
    }

    /// Seq of session `id`'s last update (0 if none).
    pub fn head(&self, id: &SessionId) -> Result<u64> {
        let db = self.db();
        let v: i64 = db.connection().query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM agent_session_updates WHERE session = ?1",
            [&id.0],
            |r| r.get(0),
        )?;
        Ok(u64_of(v))
    }

    /// Up to `limit` updates of session `id` after `after`, oldest first.
    pub fn updates_since(
        &self,
        id: &SessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, SessionUpdate)>> {
        let db = self.db();
        let mut stmt = db.connection().prepare_cached(
            "SELECT seq, payload FROM agent_session_updates
             WHERE session = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(params![id.0, i64_of(after), i64_of(limit as u64)], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(seq, payload)| Ok((u64_of(seq), serde_json::from_str(&payload)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_protocol::methods::agents::ContentBlock;

    fn session(id: &str, ws: Option<&str>, updated_at: i64) -> StoredSession {
        StoredSession {
            info: SessionInfo {
                id: SessionId(id.into()),
                provider: "fake".into(),
                workspace: ws.map(Into::into),
                cwd: "/w".into(),
                title: None,
                state: AgentState::Idle,
                state_reason: None,
                mode: None,
                closed: false,
                created_at: 1,
                updated_at,
            },
            provider_session: Some("p-1".into()),
        }
    }

    #[test]
    fn sessions_and_updates_round_trip() {
        let store = SessionStore::open_in_memory().unwrap();
        let mut a = session("a", Some("ws"), 10);
        store.save(&a, 1).unwrap();
        store.save(&session("b", None, 20), 2).unwrap();
        a.info.state = AgentState::Working;
        a.info.updated_at = 30;
        store.save(&a, 3).unwrap();

        assert_eq!(store.get(&a.info.id).unwrap(), Some(a.clone()));
        let ids: Vec<_> = store
            .list(None, false)
            .unwrap()
            .into_iter()
            .map(|i| i.id.0)
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(store.list(Some("ws"), false).unwrap().len(), 1);
        assert_eq!(store.running().unwrap(), vec![SessionId("a".into())]);
        assert_eq!(store.change_head().unwrap(), 3);
        let changes = store.changes_since(1, None, 10).unwrap();
        assert_eq!(
            changes
                .iter()
                .map(|(s, i)| (*s, i.id.0.as_str()))
                .collect::<Vec<_>>(),
            [(2, "b"), (3, "a")]
        );

        let u = SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text("hi"),
        };
        assert_eq!(store.head(&a.info.id).unwrap(), 0);
        store.append(&a.info.id, 1, &u, 5).unwrap();
        store
            .append(&a.info.id, 2, &SessionUpdate::Closed, 6)
            .unwrap();
        assert_eq!(store.head(&a.info.id).unwrap(), 2);
        assert_eq!(
            store.updates_since(&a.info.id, 1, 10).unwrap(),
            vec![(2, SessionUpdate::Closed)]
        );
        assert_eq!(store.updates_since(&a.info.id, 0, 1).unwrap(), vec![(1, u)]);
    }
}
