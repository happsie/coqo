// SPDX-License-Identifier: Apache-2.0

//! Workspace persistence in the global database (`workspaces`,
//! `workspace_roots`, `workspace_state`; global migration 2).
//!
//! Synchronous; the service calls these on a blocking thread, inside a
//! transaction together with the matching event-log append.

use std::collections::BTreeMap;

use coqo_protocol::methods::workspace::{
    ProjectInfo, RootId, WorkspaceId, WorkspaceInfo, WorkspaceRoot,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

pub(crate) type Result<T> = rusqlite::Result<T>;

fn roots(conn: &Connection, id: &str) -> Result<Vec<WorkspaceRoot>> {
    let mut stmt = conn.prepare_cached(
        "SELECT root_id, path, name FROM workspace_roots
         WHERE workspace = ?1 ORDER BY position, root_id",
    )?;
    stmt.query_map([id], |r| {
        Ok(WorkspaceRoot {
            id: RootId(r.get(0)?),
            path: r.get(1)?,
            name: r.get(2)?,
        })
    })?
    .collect()
}

fn info_row(conn: &Connection, row: &rusqlite::Row<'_>) -> Result<WorkspaceInfo> {
    let id: String = row.get(0)?;
    Ok(WorkspaceInfo {
        roots: roots(conn, &id)?,
        id: WorkspaceId(id),
        name: row.get(1)?,
        created_at: row.get(2)?,
        last_opened_at: row.get(3)?,
        open: row.get(4)?,
        project: row
            .get::<_, Option<String>>(5)?
            .filter(|p| !p.is_empty())
            .map(|p| ProjectInfo::from_common_dir(&p)),
    })
}

const SELECT: &str = "SELECT id, name, created_at, last_opened_at, open, project FROM workspaces";

pub(crate) fn get(conn: &Connection, id: &WorkspaceId) -> Result<Option<WorkspaceInfo>> {
    let mut stmt = conn.prepare_cached(&format!("{SELECT} WHERE id = ?1"))?;
    stmt.query_row([&id.0], |r| info_row(conn, r)).optional()
}

pub(crate) fn list(conn: &Connection) -> Result<Vec<WorkspaceInfo>> {
    let mut stmt = conn.prepare_cached(&format!("{SELECT} ORDER BY last_opened_at DESC, id"))?;
    stmt.query_map([], |r| info_row(conn, r))?.collect()
}

/// The workspace whose first root is `path`.
pub(crate) fn find_by_primary_path(conn: &Connection, path: &str) -> Result<Option<WorkspaceId>> {
    conn.prepare_cached(
        "SELECT r.workspace FROM workspace_roots r
         WHERE r.path = ?1 AND r.position =
             (SELECT min(position) FROM workspace_roots WHERE workspace = r.workspace)
         ORDER BY r.workspace LIMIT 1",
    )?
    .query_row([path], |r| r.get(0).map(WorkspaceId))
    .optional()
}

pub(crate) fn exists(conn: &Connection, id: &str) -> Result<bool> {
    conn.prepare_cached("SELECT 1 FROM workspaces WHERE id = ?1")?
        .exists([id])
}

pub(crate) fn create(conn: &Connection, id: &str, name: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at, last_opened_at, open)
         VALUES (?1, ?2, ?3, ?3, 1)",
        params![id, name, now],
    )?;
    Ok(())
}

/// Adds a root at the end; returns its id, or the existing root's id if
/// `path` already is one.
pub(crate) fn add_root(
    conn: &Connection,
    id: &str,
    path: &str,
    name: &str,
) -> Result<(RootId, bool)> {
    let existing: Option<u32> = conn
        .query_row(
            "SELECT root_id FROM workspace_roots WHERE workspace = ?1 AND path = ?2",
            params![id, path],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(root) = existing {
        return Ok((RootId(root), false));
    }
    let root: u32 = conn.query_row(
        "UPDATE workspaces SET next_root_id = next_root_id + 1 WHERE id = ?1
         RETURNING next_root_id - 1",
        [id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO workspace_roots (workspace, root_id, position, path, name)
         VALUES (?1, ?2,
                 (SELECT coalesce(max(position), -1) + 1 FROM workspace_roots WHERE workspace = ?1),
                 ?3, ?4)",
        params![id, root, path, name],
    )?;
    Ok((RootId(root), true))
}

/// Returns whether the root existed.
pub(crate) fn remove_root(conn: &Connection, id: &str, root: RootId) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM workspace_roots WHERE workspace = ?1 AND root_id = ?2",
        params![id, root.0],
    )? > 0)
}

pub(crate) fn set_open(conn: &Connection, id: &str, open: bool, now: Option<i64>) -> Result<()> {
    match now {
        Some(now) => conn.execute(
            "UPDATE workspaces SET open = ?2, last_opened_at = ?3 WHERE id = ?1",
            params![id, open, now],
        )?,
        None => conn.execute(
            "UPDATE workspaces SET open = ?2 WHERE id = ?1",
            params![id, open],
        )?,
    };
    Ok(())
}

/// Set the project (a git common directory, `""` for none; global
/// migration 15).
pub(crate) fn set_project(conn: &Connection, id: &str, project: &str) -> Result<()> {
    conn.execute(
        "UPDATE workspaces SET project = ?2 WHERE id = ?1",
        params![id, project],
    )?;
    Ok(())
}

/// Every workspace's id and first root.
pub(crate) fn first_roots(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT r.workspace, r.path FROM workspace_roots r
         WHERE r.position =
             (SELECT min(position) FROM workspace_roots WHERE workspace = r.workspace)",
    )?;
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

pub(crate) fn rename(conn: &Connection, id: &str, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE workspaces SET name = ?2 WHERE id = ?1",
        params![id, name],
    )?;
    Ok(())
}

/// Deletes the workspace with its roots and state. Returns whether it existed.
pub(crate) fn remove(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM workspaces WHERE id = ?1", [id])? > 0)
}

pub(crate) fn state(conn: &Connection, id: &str) -> Result<BTreeMap<String, Value>> {
    let mut stmt =
        conn.prepare_cached("SELECT key, value FROM workspace_state WHERE workspace = ?1")?;
    let rows = stmt.query_map([id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (key, value) = row?;
        match serde_json::from_str(&value) {
            Ok(v) => {
                out.insert(key, v);
            }
            Err(e) => {
                tracing::warn!(workspace = id, key, error = %e, "skipping corrupt state entry")
            }
        }
    }
    Ok(out)
}

/// `value: None` deletes the entry.
pub(crate) fn set_state(conn: &Connection, id: &str, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(v) => conn.execute(
            "INSERT INTO workspace_state (workspace, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT (workspace, key) DO UPDATE SET value = excluded.value",
            params![id, key, v],
        )?,
        None => conn.execute(
            "DELETE FROM workspace_state WHERE workspace = ?1 AND key = ?2",
            params![id, key],
        )?,
    };
    Ok(())
}
