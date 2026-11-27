// SPDX-License-Identifier: Apache-2.0

//! `fs/*`: read-only file-system access inside a workspace, and live change
//! events, for the file tree.
//!
//! **Paths** are relative to one of the workspace's roots: a [`RootId`] plus
//! a `/`-separated path such as `src/main.rs` (`""` is the root itself).
//! Absolute paths and `..` components are rejected, and so is any path that
//! resolves through a symlink to somewhere outside the workspace's roots
//! ([`PATH_OUTSIDE_WORKSPACE`](super::codes::PATH_OUTSIDE_WORKSPACE)).
//!
//! **Ignore rules**: entries carry an `ignored` flag computed from the
//! root's `.gitignore` files, `.git/info/exclude` and the user's global git
//! excludes, the same way git does (a path inside an ignored directory is
//! ignored too). `.git` itself is always reported as ignored. Ignored entries
//! are still listed; the UI decides how to show them.
//!
//! Writing files belongs to buffers, not to this module.

use serde::{Deserialize, Serialize};

use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, Topic};

/// Default [`ReadParams::max_bytes`]: 4 MiB.
pub const DEFAULT_READ_LIMIT: u64 = 4 * 1024 * 1024;
/// Largest accepted [`ReadParams::max_bytes`]: 8 MiB (frames are at most 16 MiB).
pub const MAX_READ_LIMIT: u64 = 8 * 1024 * 1024;

/// What a path points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// Anything else: sockets, devices, and symlinks that are broken or point
    /// outside the workspace.
    Other,
}

/// Params naming a path in a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PathParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// `/`-separated path relative to the root; `""` for the root itself.
    #[serde(default)]
    pub path: String,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DirEntry {
    /// File name (the last path component).
    pub name: String,
    /// What it is. For a symlink, the kind of its target.
    pub kind: EntryKind,
    /// Whether the entry itself is a symlink.
    pub symlink: bool,
    /// Size in bytes (of the target, for a symlink; 0 for directories).
    pub size: u64,
    /// Last modification, in Unix milliseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    /// Whether ignore rules exclude it.
    pub ignored: bool,
}

/// Result of [`ListDir`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ListDirResult {
    /// Entries: directories first, then files, each sorted by name.
    pub entries: Vec<DirEntry>,
}

/// Result of [`Stat`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileStat {
    /// What it is. For a symlink, the kind of its target.
    pub kind: EntryKind,
    /// Whether the path itself is a symlink.
    pub symlink: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Last modification, in Unix milliseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    /// Whether ignore rules exclude it.
    pub ignored: bool,
}

/// Params of [`ReadFile`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ReadParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// Fail with [`FILE_TOO_LARGE`](super::codes::FILE_TOO_LARGE) if the
    /// file is larger. Default [`DEFAULT_READ_LIMIT`], at most
    /// [`MAX_READ_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// Result of [`ReadFile`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ReadResult {
    /// The contents, if they are valid UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether the file is not valid UTF-8 (then `text` is absent).
    pub binary: bool,
    /// Size in bytes.
    pub size: u64,
    /// Last modification, in Unix milliseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
}

/// `data` of a [`FILE_TOO_LARGE`](super::codes::FILE_TOO_LARGE) error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileTooLarge {
    /// The file's size in bytes.
    pub size: u64,
    /// The limit that applied.
    pub limit: u64,
}

/// Params of the [`Changes`] topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// The workspace to watch. It must be open.
    pub workspace: WorkspaceId,
}

/// One event of the [`Changes`] topic: paths under one root that changed
/// during a debounce window (about 100 ms).
///
/// Paths are reported, not operations: the client re-lists or re-stats them
/// (a path that no longer exists was removed). Changes deep inside ignored
/// directories (such as `target/` or `node_modules/`) and inside `.git` are
/// not reported, but an ignored entry appearing or disappearing directly in
/// a non-ignored directory is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FsChanges {
    /// The root the paths are relative to.
    pub root: RootId,
    /// Changed paths, sorted and without duplicates. Empty when `rescan`.
    pub paths: Vec<String>,
    /// Too much changed to report individually (or the watcher lost events):
    /// re-list everything under this root that the client shows.
    pub rescan: bool,
}

/// `fs/list`: list a directory.
#[derive(Debug)]
pub enum ListDir {}
impl Method for ListDir {
    const NAME: &'static str = "fs/list";
    type Params = PathParams;
    type Result = ListDirResult;
}

/// `fs/stat`: metadata of one path.
#[derive(Debug)]
pub enum Stat {}
impl Method for Stat {
    const NAME: &'static str = "fs/stat";
    type Params = PathParams;
    type Result = FileStat;
}

/// `fs/read`: read a file, up to a size limit.
#[derive(Debug)]
pub enum ReadFile {}
impl Method for ReadFile {
    const NAME: &'static str = "fs/read";
    type Params = ReadParams;
    type Result = ReadResult;
}

/// `fs/changes`: live, debounced change events for an open workspace. Not
/// persisted: after a reconnect, resubscribe without `since` and re-list.
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "fs/changes";
    type Params = ChangesParams;
    type Event = FsChanges;
}
