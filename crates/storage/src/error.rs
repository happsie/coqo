// SPDX-License-Identifier: Apache-2.0

//! Error type for the storage crate.

use std::path::PathBuf;

use crate::event_log::Seq;

/// Result alias used throughout the storage crate.
pub type Result<T, E = StorageError> = std::result::Result<T, E>;

/// Everything that can go wrong in `coqo-storage`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// An error reported by SQLite.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A filesystem error, e.g. while creating a data directory.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path being accessed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// The platform has no data directory (no `$HOME` / `$XDG_DATA_HOME`).
    #[error("no data directory is available on this platform")]
    NoDataDir,

    /// A workspace id that can't be used as a directory name.
    #[error("invalid workspace id {0:?}: use a non-empty name without path separators")]
    InvalidWorkspaceId(String),

    /// The database was written by a newer Coqo whose schema this build doesn't know.
    #[error(
        "database schema version {found} is newer than this build supports ({supported}); \
         upgrade Coqo"
    )]
    SchemaTooNew {
        /// The `user_version` found in the database.
        found: u32,
        /// The newest version this build knows about.
        supported: u32,
    },

    /// A migration list is malformed (versions must be 1, 2, 3, … in order).
    #[error("migration {name:?} has version {version}, expected {expected}")]
    BadMigrationOrder {
        /// The migration's name.
        name: &'static str,
        /// Its declared version.
        version: u32,
        /// The version it should have had.
        expected: u32,
    },

    /// A reader asked for events after `after`, but retention already deleted
    /// some of the events that follow it. The reader must take a fresh snapshot
    /// and continue from [`first_available`](Self::EventsPruned::first_available)
    /// (or from the head of the log).
    #[error("events after {after} were pruned; the oldest retained event is {first_available:?}")]
    EventsPruned {
        /// The sequence number the reader asked to continue after.
        after: Seq,
        /// The oldest event still in the log, if any.
        first_available: Option<Seq>,
    },
}
