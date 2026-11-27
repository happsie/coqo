// SPDX-License-Identifier: Apache-2.0

//! `coqo-client`: how the UI, the `coqo` CLI and `coqod mcp` reach the
//! daemon (PLAN §4.3).
//!
//! - [`DaemonPaths`]: the per-user runtime directory, socket and lock file.
//! - [`connect`]: attach to a running daemon.
//! - [`connect_or_spawn`]: attach, or start `coqod` detached and attach
//!   once its socket is up.
//!
//! This crate depends only on `coqo-protocol`, so clients never pull in the
//! daemon. `coqod` uses it too, so both sides agree on the paths.
//!
//! ```no_run
//! # async fn demo() -> Result<(), coqo_client::ConnectError> {
//! use coqo_client::{ConnectOptions, DaemonPaths, connect_or_spawn};
//! use coqo_protocol::{ClientConfig, daemon};
//!
//! let paths = DaemonPaths::from_env()?;
//! let options = ConnectOptions::new(ClientConfig::new("coqo-ui", "0.1.0"));
//! let conn = connect_or_spawn(&paths, &options).await?;
//! let status = conn.client.request::<daemon::Status>(()).await?;
//! println!("coqod {} (pid {}), spawned: {}", status.version, status.pid, conn.spawned);
//! # Ok(())
//! # }
//! ```

mod connect;
mod paths;

pub use connect::{
    ConnectError, ConnectOptions, Connection, DETACHED_FLAG, EXIT_ALREADY_RUNNING, SpawnOptions,
    connect, connect_or_spawn,
};
pub use paths::{
    DAEMON_BIN_ENV, DATA_DIR_ENV, DaemonPaths, LOCK_FILE, NoRuntimeDir, RUNTIME_DIR_ENV,
    SOCKET_FILE,
};
