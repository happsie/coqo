// SPDX-License-Identifier: Apache-2.0

//! The `file_changes` table (global migration 14): the last session that
//! changed each file of a workspace.

use std::path::Path;

use coqo_protocol::methods::agents::{ChangeStrength, SessionId};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_storage::{GlobalDb, StorageError};
use rusqlite::params;

use crate::path_moves::PathMove;

/// What the store can fail with.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database could not be opened or migrated.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A query failed.
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

type Result<T> = std::result::Result<T, StoreError>;

/// One file's last change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Root-relative path.
    pub path: String,
    pub session: SessionId,
    pub strength: ChangeStrength,
    /// Unix milliseconds.
    pub at: i64,
}

fn strength_name(s: ChangeStrength) -> &'static str {
    match s {
        ChangeStrength::Write => "write",
        ChangeStrength::Turn => "turn",
    }
}

fn strength_of(name: &str) -> ChangeStrength {
    match name {
        "write" => ChangeStrength::Write,
        _ => ChangeStrength::Turn,
    }
}

/// The provenance table.
#[derive(Debug)]
pub struct Store {
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

    /// Records that `row.session` changed `row.path` of `root`. It replaces
    /// the file's earlier record, except that a session's `turn` evidence
    /// never weakens its own `write` record (the watcher usually reports a
    /// write after the tool call that made it completed). Returns whether
    /// the record changed.
    pub fn record(&self, workspace: &WorkspaceId, root: RootId, row: &Row) -> Result<bool> {
        let changed = self.db.connection().execute(
            "INSERT INTO file_changes (workspace, root, path, session, strength, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (workspace, root, path) DO UPDATE SET \
               session = excluded.session, strength = excluded.strength, at = excluded.at \
             WHERE NOT (excluded.strength = 'turn' AND file_changes.strength = 'write' \
                        AND file_changes.session = excluded.session)",
            params![
                workspace.0,
                root.0,
                row.path,
                row.session.0,
                strength_name(row.strength),
                row.at
            ],
        )?;
        Ok(changed > 0)
    }

    /// The records of `root`'s files.
    pub fn rows(&self, workspace: &WorkspaceId, root: RootId) -> Result<Vec<Row>> {
        let conn = self.db.connection();
        let mut stmt = conn.prepare_cached(
            "SELECT path, session, strength, at FROM file_changes \
             WHERE workspace = ?1 AND root = ?2",
        )?;
        let rows = stmt.query_map(params![workspace.0, root.0], |r| {
            Ok(Row {
                path: r.get(0)?,
                session: SessionId(r.get(1)?),
                strength: strength_of(&r.get::<_, String>(2)?),
                at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Moves the records `mv` touches to their new paths (a record already
    /// at a new path is replaced: the moved file is there now). Returns how
    /// many moved.
    pub fn rekey(&self, mv: &PathMove) -> Result<usize> {
        let moved: Vec<(Row, String)> = self
            .rows(&mv.workspace, mv.root)?
            .into_iter()
            .filter_map(|r| {
                let to = mv.apply(mv.root, &r.path)?;
                Some((r, to))
            })
            .collect();
        if moved.is_empty() {
            return Ok(0);
        }
        let conn = self.db.connection();
        let tx = conn.unchecked_transaction()?;
        for (row, _) in &moved {
            tx.execute(
                "DELETE FROM file_changes WHERE workspace = ?1 AND root = ?2 AND path = ?3",
                params![mv.workspace.0, mv.root.0, row.path],
            )?;
        }
        for (row, to) in &moved {
            tx.execute(
                "INSERT OR REPLACE INTO file_changes (workspace, root, path, session, strength, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    mv.workspace.0,
                    mv.root.0,
                    to,
                    row.session.0,
                    strength_name(row.strength),
                    row.at
                ],
            )?;
        }
        tx.commit()?;
        Ok(moved.len())
    }
}
