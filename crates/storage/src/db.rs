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
    Migration {
        version: 4,
        name: "agent permission rules",
        // Per-workspace auto-approve and auto-reject rules for agent
        // permission requests (#65, `coqo-agents`). `tool` is a `ToolKind`
        // (`execute`, `edit`, `read`), `pattern` a command prefix or a
        // root-relative path glob, `decision` `allow` or `reject`. One rule
        // per (workspace, tool, pattern): remembering the other decision
        // replaces it. Rules go with their workspace. Timestamps are Unix
        // milliseconds.
        sql: "
            CREATE TABLE agent_permission_rules (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                workspace  TEXT    NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
                tool       TEXT    NOT NULL,
                pattern    TEXT    NOT NULL,
                decision   TEXT    NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE (workspace, tool, pattern)
            );
        ",
    },
    Migration {
        version: 5,
        name: "agent session status",
        // The agent's own status (#75, `coqo-agents`): `status` is the
        // JSON `AgentStatus` (status line, claimed state, progress), NULL
        // when there is none; `state_since` is when `state` last changed
        // (Unix milliseconds), NULL for rows from before this migration.
        sql: "
            ALTER TABLE agent_sessions ADD COLUMN status TEXT;
            ALTER TABLE agent_sessions ADD COLUMN state_since INTEGER;
        ",
    },
    Migration {
        version: 6,
        name: "questions",
        // Questions and notes agents pin to code (#73, `questions/*`).
        // `anchors` is a JSON array of `{ root, anchor, current }`: `anchor`
        // is the `coqo-anchors` JSON as asked (kept for the original
        // snippet), `current` the anchor last rebased onto the code (absent
        // until it moves). `options` and `answer` are JSON. `change_seq`
        // orders changes for the `questions/changes` topic and only grows.
        // `workspace` and `session` are ids, not foreign keys, as for
        // sessions. Timestamps are Unix milliseconds.
        sql: "
            CREATE TABLE questions (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                workspace  TEXT    NOT NULL,
                session    TEXT    NOT NULL,
                kind       TEXT    NOT NULL,
                severity   TEXT,
                body       TEXT    NOT NULL,
                options    TEXT    NOT NULL,
                anchors    TEXT    NOT NULL,
                status     TEXT    NOT NULL,
                answer     TEXT,
                created_at INTEGER NOT NULL,
                closed_at  INTEGER,
                change_seq INTEGER NOT NULL
            );
            CREATE INDEX questions_workspace ON questions (workspace, status, id);
            CREATE INDEX questions_session ON questions (session, status);
            CREATE UNIQUE INDEX questions_change_seq ON questions (change_seq);
        ",
    },
    Migration {
        version: 7,
        name: "agent session acceptance criteria",
        // The task's acceptance criteria (#96, `coqo-agents`): the JSON
        // array of `AcceptanceCriterion` (id, text, status, evidence, who
        // changed it and when), NULL when there are none.
        sql: "
            ALTER TABLE agent_sessions ADD COLUMN criteria TEXT;
        ",
    },
    Migration {
        version: 8,
        name: "alerts",
        // The alert engine (#76, `alerts/*`). One row per source and key
        // (raising again replaces it). `target` is the JSON `AlertTarget`;
        // `read` and `dismissed` are 0 or 1; `resolved_at` is set when the
        // condition cleared. `change_seq` orders changes for the
        // `alerts/changes` topic and only grows. `alert_settings` holds the
        // user's settings as JSON values by key (`do_not_disturb`,
        // `muted`). Timestamps are Unix milliseconds.
        sql: "
            CREATE TABLE alerts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                source      TEXT    NOT NULL,
                key         TEXT    NOT NULL,
                workspace   TEXT,
                session     TEXT,
                kind        TEXT    NOT NULL,
                title       TEXT    NOT NULL,
                body        TEXT,
                target      TEXT,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                read        INTEGER NOT NULL DEFAULT 0,
                dismissed   INTEGER NOT NULL DEFAULT 0,
                resolved_at INTEGER,
                change_seq  INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX alerts_source_key ON alerts (source, key);
            CREATE UNIQUE INDEX alerts_change_seq ON alerts (change_seq);
            CREATE INDEX alerts_listed ON alerts (dismissed, created_at);
            CREATE INDEX alerts_session ON alerts (session, read);
            CREATE TABLE alert_settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
        ",
    },
    Migration {
        version: 9,
        name: "agent session tool use",
        // Tool-use stats (#216, `coqo-agents`): the JSON array of `ToolUse`
        // (tool name, group, count), NULL when the agent called no tool.
        sql: "
            ALTER TABLE agent_sessions ADD COLUMN tool_use TEXT;
        ",
    },
    Migration {
        version: 10,
        name: "walkthroughs",
        // Code walkthroughs agents publish (#97, `walkthroughs/*`), kept in
        // the daemon whatever the storage mode (committed ones are also
        // written to the repository). `document` is the JSON `Walkthrough`
        // as published (anchors as made, no resolutions, no step states);
        // `step_states` is the reader's progress, a JSON object of step id
        // to state. `workspace` and `session` are ids, not foreign keys, as
        // for questions. Timestamps are Unix milliseconds.
        sql: "
            CREATE TABLE walkthroughs (
                id          TEXT    PRIMARY KEY NOT NULL,
                workspace   TEXT    NOT NULL,
                session     TEXT    NOT NULL,
                document    TEXT    NOT NULL,
                step_states TEXT    NOT NULL DEFAULT '{}',
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            ) WITHOUT ROWID;
            CREATE INDEX walkthroughs_workspace ON walkthroughs (workspace, created_at);
        ",
    },
    Migration {
        version: 11,
        name: "question threads",
        // Threads the user starts about code (#229, `questions/ask`):
        // `author` is `agent` or `user`, and `thread` the JSON array of the
        // user's messages (`ThreadMessage`: text, turn, at), NULL for an
        // agent's question.
        sql: "
            ALTER TABLE questions ADD COLUMN author TEXT NOT NULL DEFAULT 'agent';
            ALTER TABLE questions ADD COLUMN thread TEXT;
        ",
    },
    Migration {
        version: 12,
        name: "review threads",
        // Comment threads on diff lines and the pending review (#94,
        // `threads/*`, `review/*`). `anchor` is the JSON of the thread's
        // anchor: root, root-relative path, diff side, the file version
        // (`DiffSide`) and, unless the thread is on the whole file, the
        // `coqo-anchors` anchor as made (`anchor`) and as last rebased
        // (`current`). `backend` is `local` or `github`; `diff` the diff
        // source's identity it was started in. A deleted thread keeps its
        // row (`deleted` = 1, no comments) so `threads/changes` can replay
        // the deletion. `change_seq` orders changes and only grows.
        // Comments: `author` is the JSON `Author`; `pending` is 1 while the
        // comment is in the user's pending review. Timestamps are Unix
        // milliseconds.
        sql: "
            CREATE TABLE review_threads (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                workspace   TEXT    NOT NULL,
                backend     TEXT    NOT NULL,
                diff        TEXT,
                anchor      TEXT    NOT NULL,
                status      TEXT    NOT NULL,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                resolved_at INTEGER,
                deleted     INTEGER NOT NULL DEFAULT 0,
                change_seq  INTEGER NOT NULL
            );
            CREATE INDEX review_threads_workspace ON review_threads (workspace, deleted, id);
            CREATE UNIQUE INDEX review_threads_change_seq ON review_threads (change_seq);
            CREATE TABLE review_comments (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                thread     INTEGER NOT NULL REFERENCES review_threads (id) ON DELETE CASCADE,
                author     TEXT    NOT NULL,
                body       TEXT    NOT NULL,
                pending    INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                edited_at  INTEGER
            );
            CREATE INDEX review_comments_thread ON review_comments (thread, id);
            CREATE INDEX review_comments_pending ON review_comments (pending, thread);
        ",
    },
    Migration {
        version: 13,
        name: "agent session title source",
        // Where a session's title came from (#172, `coqo-agents`): `prompt`
        // (derived from the first prompt), `agent` (the provider reported
        // it) or `user` (`sessions/create` or `sessions/rename`). NULL for an
        // untitled session; a titled row from before this migration counts
        // as `user`, since only `sessions/create` set titles then.
        sql: "
            ALTER TABLE agent_sessions ADD COLUMN title_source TEXT;
        ",
    },
    Migration {
        version: 14,
        name: "file change provenance",
        // Which agent session last changed each file (#286, `coqod`'s
        // provenance, `agents/changedBy`): one row per workspace, root and
        // root-relative path. `strength` is `write` (the session wrote it:
        // ACP `fs/write_text_file` or a completed tool call naming it) or
        // `turn` (it changed while only that session's turn ran). `at` is
        // Unix milliseconds. Rows follow renames.
        sql: "
            CREATE TABLE file_changes (
                workspace TEXT    NOT NULL,
                root      INTEGER NOT NULL,
                path      TEXT    NOT NULL,
                session   TEXT    NOT NULL,
                strength  TEXT    NOT NULL,
                at        INTEGER NOT NULL,
                PRIMARY KEY (workspace, root, path)
            ) WITHOUT ROWID;
        ",
    },
    Migration {
        version: 15,
        name: "workspace project",
        // The project a workspace belongs to (#92, ADR 0028): the canonical
        // git common directory of its first root's repository, which every
        // worktree of the repository shares. `''` when the first root is in
        // no repository; NULL until the daemon first looks (a row from
        // before this migration), which it does when it starts.
        sql: "
            ALTER TABLE workspaces ADD COLUMN project TEXT;
        ",
    },
    Migration {
        version: 16,
        name: "question origin",
        // The chat message a note was made from (#345): the JSON
        // `NoteOrigin`, NULL for what an agent pinned itself.
        sql: "
            ALTER TABLE questions ADD COLUMN origin TEXT;
        ",
    },
];

/// Schema of a per-workspace database, `workspaces/<id>/workspace.db`.
///
/// Version 1 holds a key/value `meta` table and version 2 the buffer
/// journal; annotations, walkthroughs and the like add their tables as later
/// migrations.
pub const WORKSPACE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "workspace meta",
        sql: "
            CREATE TABLE meta (
                key   TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            ) WITHOUT ROWID;
        ",
    },
    Migration {
        version: 2,
        name: "buffer journal",
        // Unsaved buffer edits (#40), so they survive daemon restarts. One
        // snapshot per file with unsaved edits (keyed by its canonical path):
        // the file as last read or written (`disk`), the unsaved edits
        // against it as JSON, and the highest committed seq per replica.
        // Changes since the snapshot are appended to `buffer_journal_edits`
        // as JSON `buffer/changes` events. A rowid table for the snapshots,
        // since their rows are large.
        sql: "
            CREATE TABLE buffer_journal (
                id          INTEGER PRIMARY KEY,
                path        TEXT    NOT NULL UNIQUE,
                rev         INTEGER NOT NULL,
                saved_rev   INTEGER NOT NULL,
                disk        TEXT    NOT NULL,
                unsaved     TEXT    NOT NULL,
                encoding    TEXT    NOT NULL,
                line_ending TEXT    NOT NULL,
                replicas    TEXT    NOT NULL,
                updated_at  INTEGER NOT NULL
            );
            CREATE TABLE buffer_journal_edits (
                path   TEXT    NOT NULL REFERENCES buffer_journal (path) ON DELETE CASCADE,
                rev    INTEGER NOT NULL,
                change TEXT    NOT NULL,
                PRIMARY KEY (path, rev)
            ) WITHOUT ROWID;
        ",
    },
];

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
    set_wal(conn)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// Switch to WAL (a setting stored in the file, so only its first opener
/// changes anything). SQLite does not call the busy handler for this: while
/// another connection is switching a fresh file, it fails at once with
/// "database is locked". Wait that out here, within [`BUSY_TIMEOUT`].
fn set_wal(conn: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        // `journal_mode` returns the resulting mode ("wal", or "memory" for
        // in-memory databases), so read it rather than using `pragma_update`.
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0)) {
            Ok(_mode) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e.into()),
        }
    }
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

    /// Several connections opening (and migrating) one fresh file at once,
    /// as the daemon's services do for a new workspace, all succeed: they
    /// wait for each other instead of failing with "database is locked" or
    /// applying a migration twice.
    #[test]
    fn concurrent_opens_of_a_fresh_database_all_succeed() {
        for round in 0..20 {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("ws/workspace.db");
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|s| {
                let opens: Vec<_> = (0..8)
                    .map(|_| {
                        let (path, barrier) = (&path, &barrier);
                        s.spawn(move || {
                            barrier.wait();
                            let conn = open_with(path, GLOBAL_MIGRATIONS)?;
                            // A write right away, as the stores do.
                            conn.execute_batch(
                                "CREATE TABLE IF NOT EXISTS t (x); INSERT INTO t VALUES (1);",
                            )?;
                            Ok::<_, crate::StorageError>(())
                        })
                    })
                    .collect();
                for (i, open) in opens.into_iter().enumerate() {
                    if let Err(e) = open.join().unwrap() {
                        panic!("round {round}, connection {i}: {e}");
                    }
                }
            });
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
