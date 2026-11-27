// SPDX-License-Identifier: Apache-2.0

//! `workspace/*`: workspaces and their persisted state (PLAN §4.2).
//!
//! A workspace is one or more folder **roots** with a stable
//! [`WorkspaceId`], a name, and timestamps. Workspaces live in the daemon and
//! survive UI and daemon restarts. Each workspace also keeps a small
//! key/value **state** document (open tabs, layout hints, settings overrides,
//! …) that the UI owns the shape of.
//!
//! Changes are published on the [`Events`] topic, backed by the daemon's
//! persistent event log, so a reattaching UI can take a snapshot with
//! [`List`] (which reports the `seq` it reflects) and then subscribe with
//! `since` set to that `seq`.
//!
//! Errors use the codes in [`codes`](super::codes), e.g.
//! [`UNKNOWN_WORKSPACE`](super::codes::UNKNOWN_WORKSPACE).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::method::{Method, Topic};

/// Stable identifier of a workspace, e.g. `ws-3f2a9c1d0b7e4a65`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for WorkspaceId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Identifies a root within its workspace. Stable for the root's lifetime and
/// never reused within the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct RootId(pub u32);

impl fmt::Display for RootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One folder root of a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WorkspaceRoot {
    /// The root's id within the workspace.
    pub id: RootId,
    /// Absolute, canonical path of the folder.
    pub path: String,
    /// Display name (the folder name by default).
    pub name: String,
}

/// A workspace as the daemon knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    /// Stable id.
    pub id: WorkspaceId,
    /// Display name.
    pub name: String,
    /// Folder roots, in display order. Never empty.
    pub roots: Vec<WorkspaceRoot>,
    /// When the workspace was created, in Unix milliseconds.
    pub created_at: i64,
    /// When the workspace was last opened, in Unix milliseconds.
    pub last_opened_at: i64,
    /// Whether the workspace is open (watched, shown in the rail).
    pub open: bool,
}

/// Params of [`Open`]. Give exactly one of `path` and `id`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct OpenParams {
    /// Open the workspace for this folder, creating it if needed. A
    /// workspace whose first root is this folder (after canonicalising
    /// the path) is reused, so opening the same folder twice is idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Reopen a known workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<WorkspaceId>,
    /// Name for a newly created workspace (default: the folder name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Params naming one workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WorkspaceParams {
    /// The workspace.
    pub id: WorkspaceId,
}

/// Result of [`List`]: a snapshot of every known workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ListResult {
    /// Every known workspace, most recently opened first.
    pub workspaces: Vec<WorkspaceInfo>,
    /// The event-log `seq` this snapshot reflects. Subscribe to [`Events`]
    /// with `since` set to it to get every later change.
    pub seq: u64,
}

/// Params of [`AddRoot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AddRootParams {
    /// The workspace.
    pub id: WorkspaceId,
    /// Folder to add. Adding a folder that is already a root is a no-op.
    pub path: String,
    /// Display name (default: the folder name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Params of [`RemoveRoot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RemoveRootParams {
    /// The workspace.
    pub id: WorkspaceId,
    /// The root to remove. The last root cannot be removed.
    pub root: RootId,
}

/// Params of [`Rename`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RenameParams {
    /// The workspace.
    pub id: WorkspaceId,
    /// The new display name (not empty).
    pub name: String,
}

/// A workspace's persisted state document, e.g. `tabs`, `layout`,
/// `settings`. The keys and values belong to the UI; the daemon only stores
/// them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WorkspaceState {
    /// State entries by key.
    pub entries: BTreeMap<String, Value>,
    /// The event-log `seq` this snapshot reflects (see [`ListResult::seq`]).
    pub seq: u64,
}

/// Params of [`SetState`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SetStateParams {
    /// The workspace.
    pub id: WorkspaceId,
    /// The entry to set (1–128 characters).
    pub key: String,
    /// The new value; `null` removes the entry.
    pub value: Value,
}

/// Params of the [`Events`] topic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EventsParams {
    /// Only events of this workspace; omit for every workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// One event of the [`Events`] topic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkspaceEvent {
    /// A workspace was opened (or created and opened).
    Opened {
        /// The workspace after the change.
        workspace: WorkspaceInfo,
    },
    /// A workspace was closed.
    Closed {
        /// The workspace after the change.
        workspace: WorkspaceInfo,
    },
    /// A workspace's name or roots changed.
    Changed {
        /// The workspace after the change.
        workspace: WorkspaceInfo,
    },
    /// A workspace was removed (forgotten; its folders are untouched).
    Removed {
        /// The removed workspace.
        id: WorkspaceId,
    },
    /// A state entry was set or removed.
    #[serde(rename_all = "camelCase")]
    StateChanged {
        /// The workspace.
        id: WorkspaceId,
        /// The entry.
        key: String,
        /// The new value; `null` if it was removed.
        value: Value,
    },
}

/// `workspace/open`: open (creating if needed) a workspace for a folder, or
/// reopen one by id.
#[derive(Debug)]
pub enum Open {}
impl Method for Open {
    const NAME: &'static str = "workspace/open";
    type Params = OpenParams;
    type Result = WorkspaceInfo;
}

/// `workspace/list`: every known workspace, with the `seq` it reflects.
#[derive(Debug)]
pub enum List {}
impl Method for List {
    const NAME: &'static str = "workspace/list";
    type Params = ();
    type Result = ListResult;
}

/// `workspace/get`: one workspace.
#[derive(Debug)]
pub enum Get {}
impl Method for Get {
    const NAME: &'static str = "workspace/get";
    type Params = WorkspaceParams;
    type Result = WorkspaceInfo;
}

/// `workspace/close`: close a workspace (stops its watchers). It stays known
/// and can be reopened.
#[derive(Debug)]
pub enum Close {}
impl Method for Close {
    const NAME: &'static str = "workspace/close";
    type Params = WorkspaceParams;
    type Result = WorkspaceInfo;
}

/// `workspace/remove`: forget a workspace and its daemon-side data. Its
/// folders are not touched. Result is `true` if it existed.
#[derive(Debug)]
pub enum Remove {}
impl Method for Remove {
    const NAME: &'static str = "workspace/remove";
    type Params = WorkspaceParams;
    type Result = bool;
}

/// `workspace/addRoot`: add a folder root.
#[derive(Debug)]
pub enum AddRoot {}
impl Method for AddRoot {
    const NAME: &'static str = "workspace/addRoot";
    type Params = AddRootParams;
    type Result = WorkspaceInfo;
}

/// `workspace/removeRoot`: remove a folder root.
#[derive(Debug)]
pub enum RemoveRoot {}
impl Method for RemoveRoot {
    const NAME: &'static str = "workspace/removeRoot";
    type Params = RemoveRootParams;
    type Result = WorkspaceInfo;
}

/// `workspace/rename`: change a workspace's display name.
#[derive(Debug)]
pub enum Rename {}
impl Method for Rename {
    const NAME: &'static str = "workspace/rename";
    type Params = RenameParams;
    type Result = WorkspaceInfo;
}

/// `workspace/getState`: a workspace's persisted state document.
#[derive(Debug)]
pub enum GetState {}
impl Method for GetState {
    const NAME: &'static str = "workspace/getState";
    type Params = WorkspaceParams;
    type Result = WorkspaceState;
}

/// `workspace/setState`: set or remove one state entry.
#[derive(Debug)]
pub enum SetState {}
impl Method for SetState {
    const NAME: &'static str = "workspace/setState";
    type Params = SetStateParams;
    type Result = ();
}

/// `workspace/events`: workspace changes, from the persistent event log.
#[derive(Debug)]
pub enum Events {}
impl Topic for Events {
    const NAME: &'static str = "workspace/events";
    type Params = EventsParams;
    type Event = WorkspaceEvent;
}
