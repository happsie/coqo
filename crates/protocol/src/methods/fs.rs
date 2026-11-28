// SPDX-License-Identifier: Apache-2.0

//! `fs/*`: file-system access inside a workspace and live change events, for
//! the file tree.
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
//! **File operations** ([`Create`], [`Rename`], [`Delete`], [`Restore`])
//! create, rename or move, delete and restore entries for the file tree.
//! They take the daemon token
//! only: agents have file tools of their own. Writing a file's contents
//! belongs to buffers, not to this module.

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

/// Params of [`Create`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CreateParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// `/`-separated path of the new entry, relative to the root. Missing
    /// folders on the way are created.
    pub path: String,
    /// [`File`](EntryKind::File) (empty) or [`Dir`](EntryKind::Dir).
    pub kind: EntryKind,
}

/// Params of [`Rename`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RenameParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root both paths are relative to.
    pub root: RootId,
    /// The entry to rename or move. A symlink is moved itself, not its
    /// target.
    pub from: String,
    /// Its new path. The folder it goes in must exist, and nothing may be
    /// there yet ([`ALREADY_EXISTS`](super::codes::ALREADY_EXISTS)).
    pub to: String,
}

/// Params of [`Delete`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DeleteParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// The entry to delete (a folder with everything in it). A symlink is
    /// deleted itself, not its target.
    pub path: String,
    /// Delete it for good instead of moving it to the OS trash. Without it,
    /// an entry that cannot go to the trash is left alone and the call fails
    /// with [`TRASH_FAILED`](super::codes::TRASH_FAILED).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub permanent: bool,
}

/// Result of [`Create`] and [`Rename`]: the entry's path, normalised
/// (`./a//b` is `a/b`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EntryResult {
    /// `/`-separated path relative to the root.
    pub path: String,
}

/// Result of [`Delete`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DeleteResult {
    /// Whether it went to the OS trash (`false`: deleted for good).
    pub trashed: bool,
    /// What [`Restore`] takes to bring the entry back (undo), while this
    /// daemon runs. Absent when it can't be restored; `no_restore` says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
    /// Why the entry can't be restored (deleted for good, too large for a
    /// copy where the trash can't give it back), for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_restore: Option<String>,
}

/// Params of [`Restore`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RestoreParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// Where the entry was: the path [`Delete`] was given.
    pub path: String,
    /// [`DeleteResult::restore`].
    pub restore: String,
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

/// `fs/create`: create an empty file or a folder. Daemon token only.
#[derive(Debug)]
pub enum Create {}
impl Method for Create {
    const NAME: &'static str = "fs/create";
    type Params = CreateParams;
    type Result = EntryResult;
}

/// `fs/rename`: rename or move an entry within one root. Open buffers of
/// the files it moves follow it (same buffer id, unsaved edits kept), and
/// so does everything anchored to them (questions, review threads,
/// walkthroughs, the editor context), published on each one's changes
/// topic before the call returns. Daemon token only.
#[derive(Debug)]
pub enum Rename {}
impl Method for Rename {
    const NAME: &'static str = "fs/rename";
    type Params = RenameParams;
    type Result = EntryResult;
}

/// `fs/delete`: move an entry to the OS trash, or delete it for good with
/// `permanent`. Open buffers of the files it deletes keep their text.
/// Daemon token only.
#[derive(Debug)]
pub enum Delete {}
impl Method for Delete {
    const NAME: &'static str = "fs/delete";
    type Params = DeleteParams;
    type Result = DeleteResult;
}

/// `fs/restore`: bring back an entry [`Delete`] deleted (undo in the file
/// tree), from the OS trash where the platform can restore from it, else
/// from the copy Coqo kept of a small entry. Nothing may be at the path
/// ([`ALREADY_EXISTS`](super::codes::ALREADY_EXISTS)); an entry no longer
/// in the trash is [`NOT_FOUND`](super::codes::NOT_FOUND). A rename that
/// follows it is the client's. Daemon token only.
#[derive(Debug)]
pub enum Restore {}
impl Method for Restore {
    const NAME: &'static str = "fs/restore";
    type Params = RestoreParams;
    type Result = EntryResult;
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
