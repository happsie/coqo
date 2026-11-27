// SPDX-License-Identifier: Apache-2.0

//! Application methods served by the daemon, grouped by prefix.
//!
//! Each module declares its methods and topics as types implementing
//! [`Method`](crate::Method) / [`Topic`](crate::Topic), so the UI, the CLI and
//! `coqod` share one definition. The daemon implements them; clients call
//! them with [`Client::request`](crate::Client::request) and
//! [`Client::subscribe`](crate::Client::subscribe).
//!
//! - [`workspace`]: `workspace/*`, workspaces (folder roots) and their state.
//! - [`fs`]: `fs/*`, read-only file-system access inside a workspace and
//!   live change events.

pub mod fs;
pub mod workspace;

/// Error codes of the application methods.
///
/// They live outside JSON-RPC's reserved range `-32768..=-32000`.
pub mod codes {
    /// No workspace has the given id.
    pub const UNKNOWN_WORKSPACE: i64 = 1001;
    /// The workspace has no root with the given id.
    pub const UNKNOWN_ROOT: i64 = 1002;
    /// The path is not a valid workspace-relative path, or it resolves
    /// (through `..` or a symlink) outside the workspace's roots.
    pub const PATH_OUTSIDE_WORKSPACE: i64 = 1003;
    /// The path does not exist.
    pub const NOT_FOUND: i64 = 1004;
    /// The file is larger than the read limit. `data` is
    /// [`FileTooLarge`](super::fs::FileTooLarge).
    pub const FILE_TOO_LARGE: i64 = 1005;
    /// A directory was expected.
    pub const NOT_A_DIRECTORY: i64 = 1006;
    /// A regular file was expected.
    pub const NOT_A_FILE: i64 = 1007;
    /// The operation needs an open workspace.
    pub const WORKSPACE_CLOSED: i64 = 1008;
    /// A file-system error (permissions, I/O).
    pub const IO_ERROR: i64 = 1009;
}
