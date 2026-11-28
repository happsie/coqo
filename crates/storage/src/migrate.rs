// SPDX-License-Identifier: Apache-2.0

//! A small schema migration runner built on SQLite's `user_version` pragma.
//!
//! Each database kind has an ordered list of [`Migration`]s with versions
//! 1, 2, 3, …. `user_version` records the last one applied. Opening a database
//! applies every migration above that version, each in its own transaction
//! together with the `user_version` bump, so a crash mid-upgrade leaves the
//! database at the last fully applied version.
//!
//! Migrations are append-only: never edit one that has shipped, add a new one.

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Result, StorageError};

/// One schema step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// Schema version after this migration runs. The first migration is 1.
    pub version: u32,
    /// Short human-readable name, for errors and logs.
    pub name: &'static str,
    /// SQL to run. May contain several statements.
    pub sql: &'static str,
}

/// The schema version currently recorded in `conn` (0 for a fresh database).
pub fn schema_version(conn: &Connection) -> Result<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// The newest version in `migrations` (0 if empty).
pub fn latest_version(migrations: &[Migration]) -> u32 {
    migrations.last().map_or(0, |m| m.version)
}

/// Bring `conn` up to the newest version in `migrations`.
///
/// Returns the versions that were applied (empty if already up to date).
/// Fails with [`StorageError::SchemaTooNew`] if the database is newer than
/// `migrations` knows about, and with [`StorageError::BadMigrationOrder`] if
/// the versions aren't 1, 2, 3, … in order.
pub fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<Vec<u32>> {
    for (i, m) in migrations.iter().enumerate() {
        let expected = u32::try_from(i + 1).expect("fewer than u32::MAX migrations");
        if m.version != expected {
            return Err(StorageError::BadMigrationOrder {
                name: m.name,
                version: m.version,
                expected,
            });
        }
    }

    let current = schema_version(conn)?;
    let supported = latest_version(migrations);
    if current > supported {
        return Err(StorageError::SchemaTooNew {
            found: current,
            supported,
        });
    }

    let mut applied = Vec::new();
    for m in migrations.iter().filter(|m| m.version > current) {
        // Immediate, and the version read again inside: another connection
        // may be migrating the same file right now (the daemon's services
        // open a new workspace's database at once). Taking the write lock
        // first waits for it (the busy timeout) instead of failing with
        // "database is locked" when upgrading a read, and a migration it
        // already applied is skipped rather than run twice.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if schema_version(&tx)? >= m.version {
            continue;
        }
        tx.execute_batch(m.sql)?;
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        applied.push(m.version);
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: Migration = Migration {
        version: 1,
        name: "create t",
        sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL);",
    };
    const V2: Migration = Migration {
        version: 2,
        name: "add t.b",
        sql: "ALTER TABLE t ADD COLUMN b INTEGER NOT NULL DEFAULT 7;",
    };
    const BROKEN_V3: Migration = Migration {
        version: 3,
        name: "broken",
        sql: "CREATE TABLE u (x); THIS IS NOT SQL;",
    };

    #[test]
    fn fresh_database_gets_every_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 0);
        assert_eq!(migrate(&mut conn, &[V1, V2]).unwrap(), vec![1, 2]);
        assert_eq!(schema_version(&conn).unwrap(), 2);
        conn.execute("INSERT INTO t (a) VALUES ('x')", []).unwrap();
        let b: i64 = conn.query_row("SELECT b FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(b, 7);
    }

    #[test]
    fn upgrade_keeps_existing_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &[V1]).unwrap();
        conn.execute("INSERT INTO t (a) VALUES ('old')", [])
            .unwrap();

        assert_eq!(migrate(&mut conn, &[V1, V2]).unwrap(), vec![2]);
        let (a, b): (String, i64) = conn
            .query_row("SELECT a, b FROM t", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((a.as_str(), b), ("old", 7));
    }

    #[test]
    fn rerun_is_a_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &[V1, V2]).unwrap();
        assert!(migrate(&mut conn, &[V1, V2]).unwrap().is_empty());
    }

    #[test]
    fn failed_migration_rolls_back_to_last_good_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(migrate(&mut conn, &[V1, V2, BROKEN_V3]).is_err());
        assert_eq!(schema_version(&conn).unwrap(), 2);
        let u_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'u'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            u_exists, 0,
            "the broken migration's partial work is rolled back"
        );
    }

    #[test]
    fn newer_database_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &[V1, V2]).unwrap();
        assert!(matches!(
            migrate(&mut conn, &[V1]),
            Err(StorageError::SchemaTooNew {
                found: 2,
                supported: 1
            })
        ));
    }

    #[test]
    fn out_of_order_migrations_are_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            migrate(&mut conn, &[V2]),
            Err(StorageError::BadMigrationOrder { expected: 1, .. })
        ));
        assert_eq!(schema_version(&conn).unwrap(), 0);
    }
}
