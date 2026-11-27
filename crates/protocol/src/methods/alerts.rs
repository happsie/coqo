// SPDX-License-Identifier: Apache-2.0

//! `alerts/*`: what needs the user, across workspaces (PLAN §7.9, issue
//! #76).
//!
//! The daemon's alert engine turns what happens into [`Alert`]s: an agent
//! session asking for permission, waiting on a question or blocked (all
//! [`AlertKind::NeedsYou`]), failing ([`AlertKind::Error`]) or finishing a
//! turn ([`AlertKind::Done`]), and whatever mods' alert sources raise. It
//! keeps them in `coqo.db`, so they are there when a UI attaches after
//! being closed.
//!
//! - **One alert per source and key.** A source raises an alert under a key
//!   of its own (an agent session's id, say); raising it again replaces it.
//!   A new kind or a new [`AlertTarget`] is a new occurrence: unread again,
//!   back on top, undismissed. The same kind and target only updates the
//!   text.
//! - **Resolved by itself.** When the condition clears (the permission was
//!   answered, the question answered, the session working again) the alert
//!   is resolved: `resolvedAt` is set and it counts as read. It stays in the
//!   list as history until dismissed.
//! - **Read and dismissed** are the user's. The UI marks an alert read when
//!   it is opened or its session is on screen, and dismissing hides it.
//!
//! # For the UI
//!
//! 1. [`List`] (`alerts/list`): the alerts not dismissed, newest first, and
//!    `seq`.
//! 2. Follow [`Changes`] (`alerts/changes`) with `since: seq`: each alert
//!    whose state changed, as its latest state (a dismissed one says so).
//!    Resuming replays each alert's newest state only, so seqs skip values.
//!    On `RESUME_GAP`, list again.
//! 3. [`MarkRead`], [`Dismiss`], [`DismissAll`].
//! 4. [`GetSettings`] and [`SetSettings`]: do-not-disturb, and which
//!    sources are muted. A muted source raises nothing.
//!
//! **Access:** every route takes the daemon token. Agents never need them:
//! their questions, permission requests and status raise alerts through
//! their own routes.

use serde::{Deserialize, Serialize};

use super::agents::SessionId;
use super::questions::QuestionId;
use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, Topic};

/// Alerts `alerts/list` returns at most.
pub const LIST_LIMIT: usize = 200;

/// Identifies an alert. Assigned by the daemon, never reused; an alert
/// raised again under its source and key keeps its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct AlertId(pub u64);

impl std::fmt::Display for AlertId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How much an alert wants the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// Something waits for the user: a permission request, a question, a
    /// blocked agent (sand).
    NeedsYou,
    /// Something failed (rose).
    Error,
    /// Something finished (sage).
    Done,
    /// Worth knowing (blue).
    Info,
}

impl AlertKind {
    /// Whether the user has to act.
    pub fn needs_you(self) -> bool {
        self == Self::NeedsYou
    }
}

/// A place in a file of the alert's workspace.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileLocation {
    /// The root `path` is relative to.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// The 0-based line, if a line is meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Where opening an alert takes the user, in the alert's workspace.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertTarget {
    /// An agent session's chat tab.
    Session {
        /// The session.
        session: SessionId,
    },
    /// A question an agent asked: its popover in the editor, at
    /// `location` (where the question is pinned, as asked), or the chat.
    Question {
        /// The asking session.
        session: SessionId,
        /// The question.
        question: QuestionId,
        /// Its first code location.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<FileLocation>,
    },
    /// A file.
    File {
        /// Where.
        location: FileLocation,
    },
}

impl AlertTarget {
    /// The agent session it opens, if it is one's.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            Self::Session { session } | Self::Question { session, .. } => Some(session),
            Self::File { .. } => None,
        }
    }
}

/// One alert, as it is now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    /// Id.
    pub id: AlertId,
    /// The workspace it is about; absent for one about no workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
    /// The agent session it is about, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// The source that raised it: a core source (`agents.permission`,
    /// `agents.question`, `agents.blocked`, `agents.error`, `agents.done`)
    /// or a mod's alert source id.
    pub source: String,
    /// The source's key for it: raising under the same source and key
    /// replaces it. A session's id for the agent sources.
    pub key: String,
    /// How much it wants the user.
    pub kind: AlertKind,
    /// One line: "Permission requested: Run cargo test".
    pub title: String,
    /// More detail (the question, the error), plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Where opening it goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AlertTarget>,
    /// When this occurrence was raised, Unix milliseconds.
    pub created_at: i64,
    /// When it last changed, Unix milliseconds.
    pub updated_at: i64,
    /// Whether the user has seen it (a resolved alert counts as read).
    pub read: bool,
    /// Whether the user dismissed it (hidden from the list).
    pub dismissed: bool,
    /// When the condition cleared, if it has: Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    /// The `alerts/changes` seq of this state.
    pub seq: u64,
}

impl Alert {
    /// Whether it still stands: not resolved and not dismissed.
    pub fn is_active(&self) -> bool {
        self.resolved_at.is_none() && !self.dismissed
    }

    /// Whether it counts towards the unread badge.
    pub fn is_unread(&self) -> bool {
        !self.read && self.is_active()
    }
}

/// Params of [`List`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ListParams {
    /// Only this workspace's alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// Result of [`List`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AlertList {
    /// The alerts not dismissed, most recently raised first, at most
    /// [`LIST_LIMIT`].
    pub alerts: Vec<Alert>,
    /// The `alerts/changes` seq this list reflects; subscribe with
    /// `since: seq` to keep it current.
    pub seq: u64,
}

/// Params of [`MarkRead`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct MarkReadParams {
    /// These alerts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<AlertId>,
    /// And every unread alert of this agent session (its chat is on
    /// screen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
}

/// Params of [`Dismiss`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DismissParams {
    /// The alerts.
    pub ids: Vec<AlertId>,
}

/// Params of [`DismissAll`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DismissAllParams {
    /// Only this workspace's; every alert if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// Result of [`MarkRead`], [`Dismiss`] and [`DismissAll`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AlertsChanged {
    /// The alerts that changed (each is also an `alerts/changes` event).
    pub changed: Vec<AlertId>,
}

/// Params of the [`Changes`] topic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// Only this workspace's alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// The user's alert settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AlertSettings {
    /// Do not disturb: alerts are still recorded, but nothing pops up
    /// (no toasts, no desktop notifications).
    #[serde(default)]
    pub do_not_disturb: bool,
    /// Sources that raise nothing, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub muted: Vec<String>,
}

/// A source of alerts, for the per-source settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AlertSourceInfo {
    /// Id, as in [`Alert::source`].
    pub id: String,
    /// Display name.
    pub name: String,
    /// What it alerts about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether it is muted.
    pub muted: bool,
}

/// Result of [`GetSettings`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AlertSettingsInfo {
    /// The settings.
    pub settings: AlertSettings,
    /// Every source: the core ones, then mods', by id.
    pub sources: Vec<AlertSourceInfo>,
}

/// Params of [`SetSettings`]: what to change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SetSettingsParams {
    /// Turn do-not-disturb on or off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_not_disturb: Option<bool>,
    /// Mute these sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mute: Vec<String>,
    /// Unmute these sources.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmute: Vec<String>,
}

// ---- methods ---------------------------------------------------------------------

/// `alerts/list`: the alerts not dismissed, newest first.
#[derive(Debug)]
pub enum List {}
impl Method for List {
    const NAME: &'static str = "alerts/list";
    type Params = ListParams;
    type Result = AlertList;
}

/// `alerts/markRead`: the user saw these alerts. Unknown ids are ignored.
#[derive(Debug)]
pub enum MarkRead {}
impl Method for MarkRead {
    const NAME: &'static str = "alerts/markRead";
    type Params = MarkReadParams;
    type Result = AlertsChanged;
}

/// `alerts/dismiss`: hide these alerts. One raised again comes back.
#[derive(Debug)]
pub enum Dismiss {}
impl Method for Dismiss {
    const NAME: &'static str = "alerts/dismiss";
    type Params = DismissParams;
    type Result = AlertsChanged;
}

/// `alerts/dismissAll`: hide every alert (of a workspace).
#[derive(Debug)]
pub enum DismissAll {}
impl Method for DismissAll {
    const NAME: &'static str = "alerts/dismissAll";
    type Params = DismissAllParams;
    type Result = AlertsChanged;
}

/// `alerts/settings`: do-not-disturb and the sources, muted or not.
#[derive(Debug)]
pub enum GetSettings {}
impl Method for GetSettings {
    const NAME: &'static str = "alerts/settings";
    type Params = ();
    type Result = AlertSettingsInfo;
}

/// `alerts/setSettings`: change them; returns them as they are now.
#[derive(Debug)]
pub enum SetSettings {}
impl Method for SetSettings {
    const NAME: &'static str = "alerts/setSettings";
    type Params = SetSettingsParams;
    type Result = AlertSettingsInfo;
}

/// `alerts/changes`: alerts whose state changed, as their latest state.
/// Resumable from [`AlertList::seq`]; resuming replays each alert's newest
/// state only.
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "alerts/changes";
    type Params = ChangesParams;
    type Event = Alert;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn alert_wire_shape() {
        let alert = Alert {
            id: AlertId(3),
            workspace: Some(WorkspaceId("ws-1".into())),
            session: Some(SessionId("s-1".into())),
            source: "agents.question".into(),
            key: "s-1".into(),
            kind: AlertKind::NeedsYou,
            title: "Claude asked a question".into(),
            body: Some("Which map?".into()),
            target: Some(AlertTarget::Question {
                session: SessionId("s-1".into()),
                question: QuestionId(7),
                location: Some(FileLocation {
                    root: RootId(0),
                    path: "src/main.rs".into(),
                    line: Some(4),
                }),
            }),
            created_at: 1,
            updated_at: 2,
            read: false,
            dismissed: false,
            resolved_at: None,
            seq: 9,
        };
        let v = serde_json::to_value(&alert).unwrap();
        assert_eq!(
            v,
            json!({
                "id": 3,
                "workspace": "ws-1",
                "session": "s-1",
                "source": "agents.question",
                "key": "s-1",
                "kind": "needs_you",
                "title": "Claude asked a question",
                "body": "Which map?",
                "target": {
                    "kind": "question",
                    "session": "s-1",
                    "question": 7,
                    "location": {"root": 0, "path": "src/main.rs", "line": 4},
                },
                "createdAt": 1,
                "updatedAt": 2,
                "read": false,
                "dismissed": false,
                "seq": 9,
            })
        );
        let back: Alert = serde_json::from_value(v).unwrap();
        assert_eq!(back, alert);
        assert!(back.is_unread());
        assert_eq!(
            serde_json::to_value(AlertTarget::Session {
                session: SessionId("s".into())
            })
            .unwrap(),
            json!({"kind": "session", "session": "s"})
        );
    }

    #[test]
    fn settings_params_default_to_no_change() {
        let p: SetSettingsParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p, SetSettingsParams::default());
        let p: SetSettingsParams =
            serde_json::from_value(json!({"doNotDisturb": true, "mute": ["agents.done"]})).unwrap();
        assert_eq!(p.do_not_disturb, Some(true));
        assert_eq!(p.mute, ["agents.done"]);
    }
}
