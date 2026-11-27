// SPDX-License-Identifier: Apache-2.0

//! `editor/*`: what the user sees in the editor, for agents (PLAN §7.4,
//! issue #70).
//!
//! - **Context.** Each UI window publishes its editor context for the
//!   workspace it shows with the [`PublishContext`] notification (`editor/context`):
//!   the open file tabs, which one is active, and each tab's selections and
//!   visible lines. The daemon keeps the latest per window, forgets it when
//!   the window publishes `context: null` or its connection closes, and
//!   answers [`GetContext`] (`editor/getContext`) for a workspace. Agents
//!   read it through the `editor_context` tool.
//! - **Reveal.** [`Reveal`] (`editor/reveal`) asks the UI to open a file and
//!   show (select) a range. The daemon publishes it on the [`Reveals`] topic
//!   (`editor/reveals`), which UIs follow; a window that shows the workspace
//!   opens the file in a tab and selects the range.
//!
//! Positions are 0-based: `line` counts lines, `character` counts Unicode
//! scalar values (not bytes, not UTF-16 units) from the start of the line.
//!
//! **Access:** publishing context and following reveals take the daemon
//! token (the UI). A session token (an agent's `coqod mcp`) may read the
//! context of, and reveal in, its own workspace only.

use serde::{Deserialize, Serialize};

use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, NotificationMethod, Topic};

/// Longest selected text a context carries, in bytes. Longer selections are
/// cut on a character boundary and marked `truncated`.
pub const MAX_SELECTION_TEXT: usize = 16 * 1024;

/// A place in a text: 0-based line and character (Unicode scalar values).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Position {
    /// 0-based line.
    pub line: u32,
    /// 0-based character within the line.
    pub character: u32,
}

impl Position {
    /// `line`, `character`.
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A range of a text. `start` is not after `end`; equal is an empty range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Range {
    /// Where it starts.
    pub start: Position,
    /// Where it ends (exclusive).
    pub end: Position,
}

impl Range {
    /// From `start` to `end`.
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// Whether it is empty (a cursor).
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// A selection or cursor in a tab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Selection {
    /// The selected range; empty for a cursor.
    pub range: Range,
    /// The selected text, for a non-empty selection, at most
    /// [`MAX_SELECTION_TEXT`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether `text` was cut short.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Lines shown in a tab's viewport: 0-based, `end` inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct LineRange {
    /// First visible line.
    pub start: u32,
    /// Last visible line.
    pub end: u32,
}

/// A file tab open in a window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EditorTab {
    /// The root the path is relative to.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// Whether this is the tab the user works in: the active tab of the
    /// focused pane. At most one per window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub active: bool,
    /// Whether it has unsaved edits.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
    /// Its selections and cursors, the primary one first. Empty if the tab
    /// shows no editor (not loaded yet, or a binary file).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selections: Vec<Selection>,
    /// The lines on screen, if the tab is visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<LineRange>,
}

/// One window's editor context for the workspace it shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WindowContext {
    /// Whether the window has the keyboard focus.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    /// Its file tabs, in tab order.
    pub tabs: Vec<EditorTab>,
}

/// Params of [`PublishContext`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PublishContextParams {
    /// The workspace the window shows.
    pub workspace: WorkspaceId,
    /// The window, unique within the publishing connection.
    pub window: String,
    /// Its context; `null` when the window closes or stops showing the
    /// workspace.
    pub context: Option<WindowContext>,
}

/// Params of [`GetContext`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct GetContextParams {
    /// The workspace.
    pub workspace: WorkspaceId,
}

/// A window's context as the daemon keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WindowState {
    /// The window, as its UI named it.
    pub window: String,
    /// When it last published, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// What it published.
    #[serde(flatten)]
    pub context: WindowContext,
}

/// Result of [`GetContext`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EditorContext {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The windows that show it: the focused one first, then the most
    /// recently updated. Empty when no UI shows the workspace.
    pub windows: Vec<WindowState>,
}

/// Params of [`Reveal`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RevealParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// `/`-separated path of a file relative to the root.
    pub path: String,
    /// What to show and select; the top of the file if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

/// Result of [`Reveal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RevealResult {
    /// Whether a UI window shows the workspace, so someone will see it.
    pub delivered: bool,
}

/// Params of the [`Reveals`] topic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RevealsParams {
    /// Only reveals in this workspace; omit for every workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// One event of the [`Reveals`] topic: open `path` and select `range`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RevealEvent {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root the path is relative to.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// What to show and select; the top of the file if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    /// The agent session that asked, if an agent did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// `editor/context`: a UI window's editor context (UI → daemon).
#[derive(Debug)]
pub enum PublishContext {}
impl NotificationMethod for PublishContext {
    const NAME: &'static str = "editor/context";
    type Params = PublishContextParams;
}

/// `editor/getContext`: what the windows showing a workspace have open.
#[derive(Debug)]
pub enum GetContext {}
impl Method for GetContext {
    const NAME: &'static str = "editor/getContext";
    type Params = GetContextParams;
    type Result = EditorContext;
}

/// `editor/reveal`: ask the UI to open a file and select a range.
#[derive(Debug)]
pub enum Reveal {}
impl Method for Reveal {
    const NAME: &'static str = "editor/reveal";
    type Params = RevealParams;
    type Result = RevealResult;
}

/// `editor/reveals`: reveal requests, for UIs to act on. Live only: there
/// is nothing to resume.
#[derive(Debug)]
pub enum Reveals {}
impl Topic for Reveals {
    const NAME: &'static str = "editor/reveals";
    type Params = RevealsParams;
    type Event = RevealEvent;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn context_wire_shape() {
        let state = WindowState {
            window: "w-1".into(),
            updated_at_ms: 5,
            context: WindowContext {
                focused: true,
                tabs: vec![EditorTab {
                    root: RootId(0),
                    path: "src/main.rs".into(),
                    active: true,
                    dirty: false,
                    selections: vec![Selection {
                        range: Range::new(Position::new(1, 0), Position::new(1, 2)),
                        text: Some("fn".into()),
                        truncated: false,
                    }],
                    visible: Some(LineRange { start: 0, end: 40 }),
                }],
            },
        };
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            json!({
                "window": "w-1",
                "updatedAtMs": 5,
                "focused": true,
                "tabs": [{
                    "root": 0,
                    "path": "src/main.rs",
                    "active": true,
                    "selections": [{
                        "range": {
                            "start": {"line": 1, "character": 0},
                            "end": {"line": 1, "character": 2},
                        },
                        "text": "fn",
                    }],
                    "visible": {"start": 0, "end": 40},
                }],
            })
        );
        let back: WindowState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(back, state);
    }
}
