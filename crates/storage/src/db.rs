// SPDX-License-Identifier: Apache-2.0

//! Opening databases: connection settings, schemas and the two database kinds.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::Result;
use crate::event_log::EventLog;
use crate::migrate::{Migration, migrate, schema_version};
use crate::paths::{DataDirs, ensure_dir};

/// How long a connection waits for another connection's write lock before
/// failing with `SQLITE_BUSY`.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema of the global database, `coqo.db`.
///
/// Append new migrations at the end; never edit one that has shipped.
pub const GLOBAL_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "event log",
        // AUTOINCREMENT keeps `seq` strictly increasing even after retention
        // deletes the newest rows, so a reader's "last seen seq" is never reused.
        sql: "
            CREATE TABLE event_log (
                seq       INTEGER PRIMARY KEY AUTOINCREMENT,
                workspace TEXT,
                kind      TEXT    NOT NULL,
                payload   BLOB    NOT NULL,
                ts        INTEGER NOT NULL
            );
            CREATE INDEX event_log_workspace_seq ON event_log (workspace, seq);
            CREATE INDEX event_log_ts ON event_log (ts);
        ",
    },
    Migration {
        version: 2,
        name: "workspaces",
        // Workspaces (#22): folder roots with stable ids, plus the UI's
        // per-workspace state document (tabs, layout hints, settings
        // overrides) as JSON values by key. Timestamps are Unix milliseconds.
        sql: "
            CREATE TABLE workspaces (
                id             TEXT    PRIMARY KEY NOT NULL,
                name           TEXT    NOT NULL,
                created_at     INTEGER NOT NULL,
                last_opened_at INTEGER NOT NULL,
                open           INTEGER NOT NULL DEFAULT 0,
                next_root_id   INTEGER NOT NULL DEFAULT 1
            ) WITHOUT ROWID;
            CREATE TABLE workspace_roots (
                workspace TEXT    NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
                root_id   INTEGER NOT NULL,
                position  INTEGER NOT NULL,
                path      TEXT    NOT NULL,
                name      TEXT    NOT NULL,
                PRIMARY KEY (workspace, root_id),
                UNIQUE (workspace, path)
            ) WITHOUT ROWID;
            CREATE INDEX workspace_roots_path ON workspace_roots (path);
            CREATE TABLE workspace_state (
                workspace TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
                key       TEXT NOT NULL,
                value     TEXT NOT NULL,
                PRIMARY KEY (workspace, key)
            ) WITHOUT ROWID;
        ",
    },
    Migration {
        version: 3,
        name: "agent sessions",
        // Agent sessions (#53, `coqo-agents`). `agent_session_updates` is each
        // session's transcript: JSON `SessionUpdate`s with a per-session `seq`
        // from 1. It is kept apart from `event_log` so event-log retention
        // never prunes a transcript. `change_seq` orders changes to the
        // session rows for the `sessions/changes` topic and only grows.
        // `workspace` is a workspace id but not a foreign key: a session's
        // transcript outlives the workspace. Timestamps are Unix milliseconds.
        sql: "
            CREATE TABLE agent_sessions (
                id               TEXT    PRIMARY KEY NOT NULL,
                provider         TEXT    NOT NULL,
                workspace        TEXT,
                cwd              TEXT    NOT NULL,
                title            TEXT,
                state            TEXT    NOT NULL,
                state_reason     TEXT,
                mode             TEXT,
                closed           INTEGER NOT NULL DEFAULT 0,
                provider_session TEXT,
                created_at       INTEGER NOT NULL,
                updated_at       INTEGER NOT NULL,
                change_seq       INTEGER NOT NULL
            ) WITHOUT ROWID;
            CREATE INDEX agent_sessions_workspace ON agent_sessions (workspace, updated_at);
            CREATE UNIQUE INDEX agent_sessions_change_seq ON agent_sessions (change_seq);
            CREATE TABLE agent_session_updates (
                session TEXT    NOT NULL REFERENCES agent_sessions (id) ON DELETE CASCADE,
                seq     INTEGER NOT NULL,
                kind    TEXT    NOT NULL,
                payload TEXT    NOT NULL,
                ts      INTEGER NOT NULL,
                PRIMARY KEY (session, seq)
            ) WITHOUT ROWID;
        ",
    },
];

/// Schema of a per-workspace database, `workspaces/<id>/workspace.db`.
///
/// Version 1 only holds a key/value `meta` table; annotations, walkthroughs
/// and the like add their tables as later migrations.
pub const WORKSPACE_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "workspace meta",
    sql: "
        CREATE TABLE meta (
            key   TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        ) WITHOUT ROWID;
    ",
}];

/// Open (or create) the SQLite file at `path`, apply Coqo's connection
/// settings and bring it up to date with `migrations`.
///
/// Settings: WAL journal (readers don't block the writer), `synchronous =
/// NORMAL` (durable across application crashes; the last transactions can be
/// lost only on power loss), foreign keys on, and a [`BUSY_TIMEOUT`].
///
/// Parent directories are created as needed.
pub fn open_with(path: &Path, migrations: &[Migration]) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_dir(parent)?;
    }
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&mut conn, migrations)?;
    Ok(conn)
}

/// Like [`open_with`], for a private in-memory database (tests, scratch use).
/// In-memory databases don't use WAL.
pub fn open_in_memory_with(migrations: &[Migration]) -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&mut conn, migrations)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // `journal_mode` returns the resulting mode ("wal", or "memory" for
    // in-memory databases), so read it rather than using `pragma_update`.
    let _mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// The global database, `coqo.db`: workspaces (tables `workspaces`,
/// `workspace_roots`, `workspace_state`), sessions, transcripts, alerts and
/// the **event log** (PLAN §8).
///
/// A `GlobalDb` owns one connection and is `Send` but not `Sync`. WAL mode
/// lets several connections to the same file coexist, so open one per thread
/// that needs it (for example a writer for the kernel and a reader for UI
/// replays), or keep one behind a mutex.
#[derive(Debug)]
pub struct GlobalDb {
    conn: Connection,
}

impl GlobalDb {
    /// Open (creating and migrating if needed) `<root>/coqo.db`.
    pub fn open(dirs: &DataDirs) -> Result<Self> {
        Self::open_path(&dirs.global_db())
    }

    /// Open (creating and migrating if needed) a global database at `path`.
    pub fn open_path(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: open_with(path, GLOBAL_MIGRATIONS)?,
        })
    }

    /// A private in-memory global database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            conn: open_in_memory_with(GLOBAL_MIGRATIONS)?,
        })
    }

    /// The event log stored in this database.
    pub fn events(&self) -> EventLog<'_> {
        EventLog::new(&self.conn)
    }

    /// The schema version this database is at.
    pub fn schema_version(&self) -> Result<u32> {
        schema_version(&self.conn)
    }

    /// The underlying connection, for queries this crate doesn't wrap yet.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access to the underlying connection (e.g. for transactions).
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// A per-workspace database, `workspaces/<id>/workspace.db` (annotations,
/// local walkthroughs, …). Same threading rules as [`GlobalDb`].
#[derive(Debug)]
pub struct WorkspaceDb {
    conn: Connection,
}

impl WorkspaceDb {
    /// Open (creating and migrating if needed) the database of workspace `id`.
    pub fn open(dirs: &DataDirs, id: &str) -> Result<Self> {
        Self::open_path(&dirs.workspace_db(id)?)
    }

    /// Open (creating and migrating if needed) a workspace database at `path`.
    pub fn open_path(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: open_with(path, WORKSPACE_MIGRATIONS)?,
        })
    }

    /// A private in-memory workspace database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            conn: open_in_memory_with(WORKSPACE_MIGRATIONS)?,
        })
    }

    /// Read a value from the `meta` table.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Insert or replace a value in the `meta` table.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// The schema version this database is at.
    pub fn schema_version(&self) -> Result<u32> {
        schema_version(&self.conn)
    }

    /// The underlying connection, for queries this crate doesn't wrap yet.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access to the underlying connection (e.g. for transactions).
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::latest_version;

    #[test]
    fn global_db_is_created_in_wal_mode_at_latest_version() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = DataDirs::at(tmp.path().join("nested/root"));
        let db = GlobalDb::open(&dirs).unwrap();
        assert!(dirs.global_db().exists());
        assert_eq!(
            db.schema_version().unwrap(),
            latest_version(GLOBAL_MIGRATIONS)
        );
        let mode: String = db
            .connection()
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: bool = db
            .connection()
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert!(fk);
    }

    #[test]
    fn reopening_keeps_data_and_version() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = DataDirs::at(tmp.path());
        {
            let db = WorkspaceDb::open(&dirs, "ws1").unwrap();
            db.set_meta("root", "/src/app").unwrap();
            db.set_meta("root", "/src/app2").unwrap();
        }
        let db = WorkspaceDb::open(&dirs, "ws1").unwrap();
        assert_eq!(db.meta("root").unwrap().as_deref(), Some("/src/app2"));
        assert_eq!(db.meta("missing").unwrap(), None);
        assert_eq!(
            db.schema_version().unwrap(),
            latest_version(WORKSPACE_MIGRATIONS)
        );
        assert!(dirs.workspace_db("ws1").unwrap().exists());
    }

    #[test]
    fn workspaces_are_isolated() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = DataDirs::at(tmp.path());
        WorkspaceDb::open(&dirs, "a")
            .unwrap()
            .set_meta("k", "a")
            .unwrap();
        let b = WorkspaceDb::open(&dirs, "b").unwrap();
        assert_eq!(b.meta("k").unwrap(), None);
    }

    /// Upgrade path for the real schemas: a database left at an older version
    /// (here: a fresh file at 0, then each prefix of the list) migrates to the
    /// latest one. As migrations are added this covers every intermediate step.
    #[test]
    fn real_schemas_upgrade_from_every_older_version() {
        for migrations in [GLOBAL_MIGRATIONS, WORKSPACE_MIGRATIONS] {
            for start in 0..migrations.len() {
                let tmp = tempfile::tempdir().unwrap();
                let path = tmp.path().join("db.sqlite");
                drop(open_with(&path, &migrations[..start]).unwrap());
                let conn = open_with(&path, migrations).unwrap();
                assert_eq!(
                    schema_version(&conn).unwrap(),
                    latest_version(migrations),
                    "upgrade from version {start}"
                );
            }
        }
    }

    #[test]
    fn opening_a_newer_database_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("coqo.db");
        {
            let conn = open_with(&path, GLOBAL_MIGRATIONS).unwrap();
            conn.pragma_update(None, "user_version", 999).unwrap();
        }
        assert!(matches!(
            GlobalDb::open_path(&path),
            Err(crate::StorageError::SchemaTooNew { found: 999, .. })
        ));
    }
}
