// SPDX-License-Identifier: Apache-2.0

//! Walkthroughs in the global database (`walkthroughs`, global migration
//! 8): the document as published and the reader's step states.

use std::collections::BTreeMap;
use std::path::Path;

use coqo_protocol::methods::agents::SessionId;
use coqo_protocol::methods::walkthroughs::{StepState, Walkthrough, WalkthroughId};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_storage::{GlobalDb, StorageError};
use rusqlite::{OptionalExtension, Row, params};

/// Most walkthroughs a list returns.
pub(crate) const LIST_LIMIT: usize = 200;

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

/// The walkthroughs table.
#[derive(Debug)]
pub(crate) struct Store {
    db: GlobalDb,
}

fn row(r: &Row<'_>) -> rusqlite::Result<(String, String)> {
    Ok((r.get(0)?, r.get(1)?))
}

fn decode((document, states): (String, String)) -> Result<Walkthrough> {
    let mut w: Walkthrough = serde_json::from_str(&document)?;
    w.step_states = serde_json::from_str(&states)?;
    Ok(w)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            db: GlobalDb::open_path(path)?,
        })
    }

    /// Store a new walkthrough (its `step_states` are not part of the
    /// document).
    pub fn insert(&self, w: &Walkthrough) -> Result<()> {
        let mut doc = w.clone();
        doc.step_states.clear();
        self.db.connection().execute(
            "INSERT INTO walkthroughs (id, workspace, session, document, step_states, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                w.id.0,
                w.workspace.0,
                w.created_by.session.0,
                serde_json::to_string(&doc)?,
                serde_json::to_string(&w.step_states)?,
                w.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &WalkthroughId) -> Result<Option<Walkthrough>> {
        self.db
            .connection()
            .query_row(
                "SELECT document, step_states FROM walkthroughs WHERE id = ?1",
                params![id.0],
                row,
            )
            .optional()?
            .map(decode)
            .transpose()
    }

    /// A workspace's walkthroughs (one session's, if given), newest first.
    pub fn list(
        &self,
        workspace: &WorkspaceId,
        session: Option<&SessionId>,
    ) -> Result<Vec<Walkthrough>> {
        let conn = self.db.connection();
        let mut stmt = conn.prepare_cached(
            "SELECT document, step_states FROM walkthroughs
             WHERE workspace = ?1 AND (?2 IS NULL OR session = ?2)
             ORDER BY created_at DESC, id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                workspace.0,
                session.map(|s| s.0.as_str()),
                LIST_LIMIT as i64
            ],
            row,
        )?;
        rows.map(|r| decode(r?)).collect()
    }

    /// Replace a walkthrough's step states. Returns whether it exists.
    pub fn set_states(
        &self,
        id: &WalkthroughId,
        states: &BTreeMap<String, StepState>,
        now: i64,
    ) -> Result<bool> {
        let n = self.db.connection().execute(
            "UPDATE walkthroughs SET step_states = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.0, serde_json::to_string(states)?, now],
        )?;
        Ok(n > 0)
    }

    /// Forget a walkthrough. Returns whether it existed.
    pub fn delete(&self, id: &WalkthroughId) -> Result<bool> {
        let n = self
            .db
            .connection()
            .execute("DELETE FROM walkthroughs WHERE id = ?1", params![id.0])?;
        Ok(n > 0)
    }
}
