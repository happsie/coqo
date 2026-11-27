// SPDX-License-Identifier: Apache-2.0

//! What mods contribute: the value types of the v1 contribution points.
//!
//! Each type is plain data, so it reads the same from a `coqo.mod.toml`
//! `[contributes]` table, from a WASM or process mod over the wire, or from a
//! core mod's Rust code. [`points`](crate::points) maps each type to its
//! contribution point.
//!
//! `when` fields hold a context expression (such as `editor.focused`) that
//! decides when the contribution applies. The expression language is defined
//! by the workbench; v1 stores the text as is.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::ContributionId;

/// A contribution with its own id, unique within its point.
///
/// [`ModContext::contribute`](crate::ModContext::contribute) uses it to key the
/// contribution.
pub trait Keyed {
    /// The contribution's id within its point.
    fn key(&self) -> &ContributionId;
}

macro_rules! keyed {
    ($($ty:ty => $field:ident),* $(,)?) => {
        $(impl Keyed for $ty {
            fn key(&self) -> &ContributionId {
                &self.$field
            }
        })*
    };
}

keyed!(
    Command => id,
    Setting => key,
    View => id,
    StatusItem => id,
    Language => id,
    LspServer => id,
    DebugAdapter => id,
    McpTool => name,
    AgentProviderInfo => id,
    SlashCommand => name,
    ContextProvider => id,
    AlertSource => id,
    Theme => id,
);

/// A command the user can run from the palette, a keybinding or a menu
/// (point `commands`). Core mods pair it with a
/// [`CommandHandler`](crate::CommandHandler) through
/// [`ModContext::register_command`](crate::ModContext::register_command).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// Command id, such as `git.commit`. Unique across all mods.
    pub id: ContributionId,
    /// Title shown in the command palette.
    pub title: String,
    /// Palette group, such as `Git`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Icon name from the active icon set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// When the command is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

impl Command {
    /// A command with just an id and a title.
    pub fn new(id: ContributionId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            category: None,
            icon: None,
            when: None,
        }
    }

    /// Sets the palette category.
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    /// Sets the icon.
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// Sets the `when` clause.
    pub fn with_when(mut self, when: impl Into<String>) -> Self {
        self.when = Some(when.into());
        self
    }
}

/// Binds a key sequence to a command (point `keybindings`).
///
/// Keys are written as chords separated by spaces, each chord as modifiers
/// and a key joined by `+`: `ctrl+shift+p`, `mod+k mod+s`. Modifiers are
/// `ctrl`, `alt`, `shift`, `cmd` and `mod` (`cmd` on macOS, `ctrl`
/// elsewhere). Keys are lowercase: a letter, digit or punctuation character,
/// or one of `enter`, `escape`, `tab`, `space`, `backspace`, `delete`,
/// `insert`, `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`,
/// `right`, `f1` … `f24`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keybinding {
    /// Command to run.
    pub command: ContributionId,
    /// Key sequence on every platform, unless `mac` overrides it.
    pub key: String,
    /// Key sequence on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// When the binding is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Arguments passed to the command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

/// A menu that mods can add items to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Menu {
    /// The command palette. Commands appear there by default; an item here
    /// only adds a `when` clause or ordering.
    #[serde(rename = "command_palette")]
    CommandPalette,
    /// Right-click menu in the editor.
    #[serde(rename = "editor.context")]
    EditorContext,
    /// Actions in the editor tab's title bar.
    #[serde(rename = "editor.title")]
    EditorTitle,
    /// Right-click menu in the file tree.
    #[serde(rename = "explorer.context")]
    ExplorerContext,
    /// Right-click menu on a tab.
    #[serde(rename = "tab.context")]
    TabContext,
    /// Actions in a view's title bar.
    #[serde(rename = "view.title")]
    ViewTitle,
}

/// Puts a command in a menu (point `menus`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MenuItem {
    /// Which menu.
    pub menu: Menu,
    /// Command to run.
    pub command: ContributionId,
    /// Group within the menu; groups are separated by dividers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Position within the group, lowest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    /// When the item is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// Type of a [`Setting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingType {
    /// `true` or `false`.
    Boolean,
    /// Free text.
    String,
    /// Whole number, optionally bounded by `minimum` / `maximum`.
    Integer,
    /// Any number, optionally bounded by `minimum` / `maximum`.
    Number,
    /// One of `values`.
    Enum,
    /// A list of strings.
    StringList,
}

/// A user setting with its schema (point `settings`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    /// Setting key, such as `git.autofetch`.
    pub key: ContributionId,
    /// Short title for the settings UI; defaults to the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What the setting does.
    pub description: String,
    /// Value type.
    #[serde(rename = "type")]
    pub ty: SettingType,
    /// Default value; must match `ty`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Allowed values, for `type = "enum"` only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Lower bound, for `integer` and `number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    /// Upper bound, for `integer` and `number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
}

/// Where a view lives in the workbench.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewLocation {
    /// A tab in the left sidebar (Files, Git, …).
    Sidebar,
    /// The bottom panel (terminal, problems, …).
    Panel,
    /// A full tab in the main tab area, like a file.
    Tab,
    /// The status bar.
    Status,
    /// The workspace rail on the far left.
    Rail,
}

/// A view slot in the sidebar, panel, tab area or rail (points
/// `views.sidebar`, `views.panel`, `views.tab`, `views.rail`). Its content is
/// a declarative view tree that the mod supplies at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    /// View id, unique across all view locations.
    pub id: ContributionId,
    /// Title shown on the view's tab or header.
    pub title: String,
    /// Icon name from the active icon set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// When the view is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Position among the location's views, lowest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
}

impl View {
    /// A view with just an id and a title.
    pub fn new(id: ContributionId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            icon: None,
            when: None,
            order: None,
        }
    }
}

/// Which side of the status bar an item sits on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusAlignment {
    /// Left side (the default).
    #[default]
    Left,
    /// Right side.
    Right,
}

impl StatusAlignment {
    fn is_left(&self) -> bool {
        *self == Self::Left
    }
}

/// A status bar item (point `views.status`). Contribute it again with the
/// same id to change its text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusItem {
    /// Item id, unique across all view locations.
    pub id: ContributionId,
    /// Text shown in the status bar.
    pub text: String,
    /// Hover text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
    /// Command run on click.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<ContributionId>,
    /// Left or right side.
    #[serde(default, skip_serializing_if = "StatusAlignment::is_left")]
    pub alignment: StatusAlignment,
    /// Higher priorities sit further towards the outer edge.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub priority: i32,
    /// When the item is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

fn is_zero(n: &i32) -> bool {
    *n == 0
}

impl StatusItem {
    /// A left-aligned item with just an id and text.
    pub fn new(id: ContributionId, text: impl Into<String>) -> Self {
        Self {
            id,
            text: text.into(),
            tooltip: None,
            command: None,
            alignment: StatusAlignment::Left,
            priority: 0,
            when: None,
        }
    }

    /// Sets the hover text.
    pub fn with_tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// Sets the command run on click.
    pub fn with_command(mut self, command: ContributionId) -> Self {
        self.command = Some(command);
        self
    }

    /// Sets the side of the status bar.
    pub fn with_alignment(mut self, alignment: StatusAlignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Sets the priority.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

/// The `[contributes.views]` table: views by location.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Views {
    /// `views.sidebar`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidebar: Vec<View>,
    /// `views.panel`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panel: Vec<View>,
    /// `views.tab`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tab: Vec<View>,
    /// `views.rail`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rail: Vec<View>,
    /// `views.status`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<StatusItem>,
}

impl Views {
    /// Whether there are no views at all.
    pub fn is_empty(&self) -> bool {
        self.sidebar.is_empty()
            && self.panel.is_empty()
            && self.tab.is_empty()
            && self.rail.is_empty()
            && self.status.is_empty()
    }
}

/// A language: file associations and an optional tree-sitter grammar
/// (point `languages`). Its language server is a separate [`LspServer`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Language {
    /// Language id, such as `java` or `typescriptreact`.
    pub id: ContributionId,
    /// Display name.
    pub name: String,
    /// File extensions including the dot: `.java`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    /// Exact file names: `pom.xml`, `Dockerfile`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filenames: Vec<String>,
    /// Path of the tree-sitter grammar, relative to the mod's folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar: Option<String>,
    /// Line comment token, such as `//`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_comment: Option<String>,
}

/// A language server definition (point `lsp_servers`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServer {
    /// Server id, such as `jdtls`.
    pub id: ContributionId,
    /// Display name; defaults to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Language ids the server handles.
    pub languages: Vec<ContributionId>,
    /// Program to run: a name on `PATH`, or a path relative to the mod's folder.
    pub command: String,
    /// Program arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Files that mark a project root, such as `pom.xml`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_markers: Vec<String>,
    /// Sent as `initializationOptions` in the LSP `initialize` request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<Value>,
}

/// A debug adapter (DAP) definition (point `debug_adapters`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugAdapter {
    /// Adapter type, such as `java`; launch configurations name it.
    pub id: ContributionId,
    /// Display name.
    pub name: String,
    /// Language ids the adapter debugs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<ContributionId>,
    /// Program to run. Leave it out if the mod starts the adapter itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Program arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// A tool agents can call through `coqod mcp` (point `mcp_tools`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpTool {
    /// Tool name, such as `maven_test`: lowercase letters, digits, `_` and
    /// `-`, at most 64 characters, no dots.
    pub name: ContributionId,
    /// What the tool does, written for the agent.
    pub description: String,
    /// JSON Schema of the tool's arguments; an object schema.
    pub input_schema: Value,
}

/// How an agent provider runs the agent (PLAN §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProviderKind {
    /// An ACP agent the daemon runs as a child process (Claude Code via ACP,
    /// Codex CLI, Gemini CLI, …).
    Acp,
    /// An unmodified CLI agent in a Coqo terminal tab.
    Terminal,
    /// Coqo's own agent loop over a provider's HTTP API.
    Native,
}

/// An agent provider shown in the chat's provider picker (point
/// `agent_providers`). The runtime side (sessions, messages) is a service the
/// mod provides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProviderInfo {
    /// Provider id, such as `claude-acp`.
    pub id: ContributionId,
    /// Display name.
    pub name: String,
    /// How the agent runs.
    pub kind: AgentProviderKind,
    /// Short description for the picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A chat slash command, available to every agent provider (point
/// `slash_commands`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlashCommand {
    /// Name without the slash: `review` for `/review`.
    pub name: ContributionId,
    /// What it does.
    pub description: String,
    /// Placeholder for the arguments, such as `<pr number>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// A chat context provider: an `@`-mention kind such as `@file` or `@pr`
/// (point `context_providers`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextProvider {
    /// Mention keyword without the `@`.
    pub id: ContributionId,
    /// Display name.
    pub name: String,
    /// What it adds to the chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A source of alerts for the alert center (point `alert_sources`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertSource {
    /// Source id, such as `github.reviews`.
    pub id: ContributionId,
    /// Display name, used in alert settings.
    pub name: String,
    /// What it alerts about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Whether a theme is light or dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeAppearance {
    /// Light background.
    Light,
    /// Dark background.
    Dark,
}

/// A colour theme (point `themes`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    /// Theme id.
    pub id: ContributionId,
    /// Display name.
    pub name: String,
    /// Light or dark.
    pub appearance: ThemeAppearance,
    /// Path of the theme file, relative to the mod's folder.
    pub path: String,
}
