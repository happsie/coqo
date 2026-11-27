// SPDX-License-Identifier: Apache-2.0

//! Alerts in the global database (`alerts`, `alert_settings`; global
//! migration 8).
//!
//! Synchronous and short: the service calls it under its lock, so changes
//! get their `change_seq` in the order they are published.

use std::path::Path;

use coqo_protocol::methods::agents::SessionId;
use coqo_protocol::methods::alerts::{Alert, AlertId, AlertKind, AlertTarget};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_storage::{GlobalDb, StorageError};
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

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

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// A unit enum as its serde name (`needs_you`).
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

const SELECT: &str = "SELECT id, source, key, workspace, session, kind, title, body, target, \
                      created_at, updated_at, read, dismissed, resolved_at, change_seq FROM alerts";

fn alert_row(row: &Row<'_>) -> rusqlite::Result<Alert> {
    let kind: String = row.get(5)?;
    let target: Option<String> = row.get(8)?;
    Ok(Alert {
        id: AlertId(u64_of(row.get(0)?)),
        source: row.get(1)?,
        key: row.get(2)?,
        workspace: row.get::<_, Option<String>>(3)?.map(WorkspaceId),
        session: row.get::<_, Option<String>>(4)?.map(SessionId),
        kind: parse_name::<AlertKind>(5, &kind)?,
        title: row.get(6)?,
        body: row.get(7)?,
        target: target
            .as_deref()
            .map(|t| parse_json::<AlertTarget>(8, t))
            .transpose()?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        read: row.get::<_, i64>(11)? != 0,
        dismissed: row.get::<_, i64>(12)? != 0,
        resolved_at: row.get(13)?,
        seq: u64_of(row.get(14)?),
    })
}

/// The alerts table and the alert settings.
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
                .query_row("SELECT MAX(change_seq) FROM alerts", [], |r| r.get(0))?;
        Ok(head.map_or(0, u64_of))
    }

    /// One alert.
    pub fn get(&self, id: AlertId) -> Result<Option<Alert>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
            .query_row([i64_of(id.0)], alert_row)
            .optional()?)
    }

    /// The alert of `source` under `key`, if it was ever raised.
    pub fn by_key(&self, source: &str, key: &str) -> Result<Option<Alert>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!("{SELECT} WHERE source = ?1 AND key = ?2"))?
            .query_row([source, key], alert_row)
            .optional()?)
    }

    /// Adds `alert` (its id is ignored) as change `seq`; returns it with
    /// its id.
    pub fn insert(&self, alert: &Alert, seq: u64) -> Result<Alert> {
        let conn = self.db.connection();
        conn.execute(
            "INSERT INTO alerts (source, key, workspace, session, kind, title, body, target, \
             created_at, updated_at, read, dismissed, resolved_at, change_seq) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                alert.source,
                alert.key,
                alert.workspace.as_ref().map(|w| w.0.as_str()),
                alert.session.as_ref().map(|s| s.0.as_str()),
                name_of(&alert.kind),
                alert.title,
                alert.body,
                alert
                    .target
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                alert.created_at,
                alert.updated_at,
                i64::from(alert.read),
                i64::from(alert.dismissed),
                alert.resolved_at,
                i64_of(seq),
            ],
        )?;
        Ok(Alert {
            id: AlertId(u64_of(conn.last_insert_rowid())),
            seq,
            ..alert.clone()
        })
    }

    /// Writes every field of `alert` as change `seq`.
    pub fn update(&self, alert: &Alert, seq: u64) -> Result<()> {
        self.db.connection().execute(
            "UPDATE alerts SET workspace = ?2, session = ?3, kind = ?4, title = ?5, body = ?6, \
             target = ?7, created_at = ?8, updated_at = ?9, read = ?10, dismissed = ?11, \
             resolved_at = ?12, change_seq = ?13 WHERE id = ?1",
            params![
                i64_of(alert.id.0),
                alert.workspace.as_ref().map(|w| w.0.as_str()),
                alert.session.as_ref().map(|s| s.0.as_str()),
                name_of(&alert.kind),
                alert.title,
                alert.body,
                alert
                    .target
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                alert.created_at,
                alert.updated_at,
                i64::from(alert.read),
                i64::from(alert.dismissed),
                alert.resolved_at,
                i64_of(seq),
            ],
        )?;
        Ok(())
    }

    /// The alerts not dismissed, most recently raised first, at most
    /// `limit`.
    pub fn list(&self, workspace: Option<&WorkspaceId>, limit: usize) -> Result<Vec<Alert>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!(
                "{SELECT} WHERE dismissed = 0 AND (?1 IS NULL OR workspace = ?1) \
                 ORDER BY created_at DESC, id DESC LIMIT ?2"
            ))?
            .query_map(
                params![
                    workspace.map(|w| w.0.as_str()),
                    i64::try_from(limit).unwrap_or(0)
                ],
                alert_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The unread, standing alerts of `session`.
    pub fn unread_of(&self, session: &SessionId) -> Result<Vec<Alert>> {
        Ok(self
            .db
            .connection()
            .prepare_cached(&format!(
                "{SELECT} WHERE session = ?1 AND read = 0 AND dismissed = 0 \
                 AND resolved_at IS NULL ORDER BY id"
            ))?
            .query_map([&session.0], alert_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Alerts changed after `seq`, oldest change first, at most `limit`.
    pub fn changed_since(
        &self,
        seq: u64,
        workspace: Option<&WorkspaceId>,
        limit: usize,
    ) -> Result<Vec<Alert>> {
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
                alert_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes alerts dismissed before `before` (Unix milliseconds).
    /// Returns how many.
    pub fn prune_dismissed(&self, before: i64) -> Result<usize> {
        Ok(self.db.connection().execute(
            "DELETE FROM alerts WHERE dismissed = 1 AND updated_at < ?1",
            [before],
        )?)
    }

    /// A setting, if it was ever set.
    pub fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let value: Option<String> = self
            .db
            .connection()
            .prepare_cached("SELECT value FROM alert_settings WHERE key = ?1")?
            .query_row([key], |r| r.get(0))
            .optional()?;
        Ok(value.map(|v| serde_json::from_str(&v)).transpose()?)
    }

    /// Sets a setting.
    pub fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO alert_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(source: &str, key: &str, created_at: i64) -> Alert {
        Alert {
            id: AlertId(0),
            workspace: Some(WorkspaceId("ws".into())),
            session: Some(SessionId(key.into())),
            source: source.into(),
            key: key.into(),
            kind: AlertKind::NeedsYou,
            title: "t".into(),
            body: Some("b".into()),
            target: Some(AlertTarget::Session {
                session: SessionId(key.into()),
            }),
            created_at,
            updated_at: created_at,
            read: false,
            dismissed: false,
            resolved_at: None,
            seq: 0,
        }
    }

    #[test]
    fn alerts_and_settings_round_trip_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coqo.db");
        let (a, b) = {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.head().unwrap(), 0);
            let a = store.insert(&alert("agents.error", "s-1", 10), 1).unwrap();
            let mut b = store.insert(&alert("agents.done", "s-2", 20), 2).unwrap();
            assert!(b.id > a.id);
            b.read = true;
            b.resolved_at = Some(30);
            store.update(&b, 3).unwrap();
            b.seq = 3;
            store.set_setting("do_not_disturb", &true).unwrap();
            store.set_setting("do_not_disturb", &false).unwrap();
            (a, b)
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.head().unwrap(), 3);
        assert_eq!(store.get(a.id).unwrap().unwrap(), a);
        assert_eq!(store.by_key("agents.done", "s-2").unwrap().unwrap(), b);
        assert!(store.by_key("agents.done", "s-1").unwrap().is_none());
        let listed: Vec<_> = store.list(None, 10).unwrap();
        assert_eq!(listed, [b.clone(), a.clone()], "newest first");
        assert!(
            store
                .list(Some(&WorkspaceId("other".into())), 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.unread_of(&SessionId("s-1".into())).unwrap(), [a]);
        assert!(
            store
                .unread_of(&SessionId("s-2".into()))
                .unwrap()
                .is_empty()
        );
        let changed = store.changed_since(1, None, 10).unwrap();
        assert_eq!(changed.iter().map(|r| r.seq).collect::<Vec<_>>(), [3]);
        assert_eq!(
            store.setting::<bool>("do_not_disturb").unwrap(),
            Some(false)
        );
        assert_eq!(store.setting::<bool>("missing").unwrap(), None);
    }
}
