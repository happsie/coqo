// SPDX-License-Identifier: Apache-2.0

//! Coqo storage: the daemon's SQLite databases, their migrations, and the
//! event log used for UI reattach (PLAN §4, §8).
//!
//! - [`DataDirs`]: where data lives (`$XDG_DATA_HOME/coqo`,
//!   `~/Library/Application Support/Coqo`), overridable for tests.
//! - [`GlobalDb`]: `coqo.db`, which holds the [`EventLog`].
//! - [`WorkspaceDb`]: one database per workspace under `workspaces/<id>/`.
//! - [`migrate`](mod@migrate): the `user_version`-based migration runner.
//!
//! All databases use WAL mode. The API is synchronous; async callers should
//! run it on a blocking thread (e.g. `spawn_blocking`).
//!
//! ```
//! use coqo_storage::{DataDirs, EventFilter, GlobalDb, NewEvent, Seq};
//!
//! let tmp = tempfile::tempdir()?;
//! let db = GlobalDb::open(&DataDirs::at(tmp.path()))?;
//! let log = db.events();
//! log.append(&NewEvent { workspace: Some("ws"), kind: "hello", payload: b"{}" })?;
//!
//! // A reattaching UI continues from the last sequence number it saw.
//! let events = log.read_since(Seq::ZERO, EventFilter::All, 1000)?;
//! assert_eq!(events[0].seq, Seq(1));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod db;
mod error;
pub mod event_log;
pub mod migrate;
mod paths;

pub use db::{
    BUSY_TIMEOUT, GLOBAL_MIGRATIONS, GlobalDb, WORKSPACE_MIGRATIONS, WorkspaceDb,
    open_in_memory_with, open_with,
};
pub use error::{Result, StorageError};
pub use event_log::{Event, EventFilter, EventLog, NewEvent, Retention, Seq, Timestamp};
pub use migrate::Migration;
pub use paths::{DataDirs, GLOBAL_DB_FILE, WORKSPACE_DB_FILE, WORKSPACES_DIR};

/// Name of this crate, used by the workspace smoke tests.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_set() {
        assert_eq!(CRATE_NAME, "coqo-storage");
    }
}
