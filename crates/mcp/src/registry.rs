// SPDX-License-Identifier: Apache-2.0

//! The tool registry: what `coqod mcp` offers agents.
//!
//! A [`Tool`] describes itself with a [`ToolDefinition`] (name, description,
//! JSON Schema of its arguments) and runs with a [`ToolContext`]: the
//! session-scoped daemon connection, the agent's workspace and session, and
//! a way to report progress. Tools reach editor state only through daemon
//! methods, so the route policy applies to everything they do.
//!
//! A [`ToolRegistry`] is shared and can change while the server runs (mods
//! contributing tools come and go). Every change bumps a generation that the
//! server turns into `notifications/tools/list_changed`.
//!
//! ```
//! use coqo_mcp::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput, ToolRegistry};
//! use serde_json::{Map, Value, json};
//!
//! struct Echo;
//!
//! impl Tool for Echo {
//!     fn definition(&self) -> ToolDefinition {
//!         ToolDefinition::new("echo", "Repeats `text`.").with_schema(json!({
//!             "type": "object",
//!             "properties": { "text": { "type": "string" } },
//!             "required": ["text"],
//!         }))
//!     }
//!
//!     fn call<'a>(
//!         &'a self,
//!         _cx: &'a ToolContext,
//!         args: Map<String, Value>,
//!     ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
//!         Box::pin(async move {
//!             let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
//!             Ok(ToolOutput::text(text))
//!         })
//!     }
//! }
//!
//! let registry = ToolRegistry::new();
//! registry.register(Echo).unwrap();
//! assert_eq!(registry.definitions()[0].name, "echo");
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};

use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_protocol::{Client, ProtocolError};
use serde_json::{Map, Value, json};
use tokio::sync::watch;

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How a tool describes itself to agents (`tools/list`).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    /// Unique name: 1 to 64 of `A-Z a-z 0-9 _ -`.
    pub name: String,
    /// Display name, if different from `name`.
    pub title: Option<String>,
    /// What it does and when to use it. Agents read this to choose tools.
    pub description: String,
    /// JSON Schema of the arguments: an object schema.
    pub input_schema: Map<String, Value>,
    /// Whether it only reads (MCP `readOnlyHint`).
    pub read_only: bool,
}

impl ToolDefinition {
    /// A tool without arguments.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: description.into(),
            input_schema: no_arguments(),
            read_only: false,
        }
    }

    /// Set the arguments' JSON Schema. Anything but a JSON object is
    /// replaced by an empty object schema.
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.input_schema = match schema {
            Value::Object(o) => o,
            _ => no_arguments(),
        };
        self
    }

    /// Set the display name.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Mark it as read-only.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

fn no_arguments() -> Map<String, Value> {
    match json!({ "type": "object", "properties": {} }) {
        Value::Object(o) => o,
        _ => Map::new(),
    }
}

/// What a tool call returns to the agent.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    /// Text for the model.
    pub text: String,
    /// The same result as JSON (MCP `structuredContent`), if it has one.
    pub structured: Option<Value>,
}

impl ToolOutput {
    /// A plain-text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
        }
    }

    /// A JSON result: sent as structured content and, for clients that read
    /// only text, as pretty-printed JSON.
    pub fn json(value: Value) -> Self {
        Self {
            text: serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            structured: Some(value),
        }
    }
}

/// Why a tool call failed. The agent sees the message as an error result
/// (MCP `isError`), so it can correct itself.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The arguments are wrong.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The daemon refused or failed the call, e.g. `FORBIDDEN` for anything
    /// outside the session's workspace.
    #[error("coqod: {0}")]
    Daemon(#[from] ProtocolError),
    /// Anything else.
    #[error("{0}")]
    Failed(String),
}

/// An editor capability offered to agents.
pub trait Tool: Send + Sync + 'static {
    /// Name, description and argument schema.
    fn definition(&self) -> ToolDefinition;

    /// Run the tool. `args` are the call's arguments (an empty map if it
    /// sent none). A call the agent cancels is dropped.
    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

type ProgressFn =
    Arc<dyn Fn(f64, Option<f64>, Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;

/// What a tool call runs with.
#[derive(Clone)]
pub struct ToolContext {
    daemon: Client,
    workspace: WorkspaceId,
    session: String,
    progress: Option<ProgressFn>,
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("workspace", &self.workspace)
            .field("session", &self.session)
            .field("progress", &self.progress.is_some())
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    /// A context without progress reporting.
    pub fn new(daemon: Client, workspace: WorkspaceId, session: impl Into<String>) -> Self {
        Self {
            daemon,
            workspace,
            session: session.into(),
            progress: None,
        }
    }

    pub(crate) fn with_progress(mut self, progress: ProgressFn) -> Self {
        self.progress = Some(progress);
        self
    }

    /// The daemon connection. Its token is scoped to [`Self::workspace`] and
    /// [`Self::session`]: other workspaces and daemon-wide methods answer
    /// `FORBIDDEN`.
    pub fn daemon(&self) -> &Client {
        &self.daemon
    }

    /// The agent's workspace.
    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// The agent session (Coqo's id).
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Report progress (MCP `notifications/progress`), so a long or blocking
    /// call does not time out. A no-op unless the agent asked for progress.
    /// `progress` must increase from call to call.
    pub async fn progress(&self, progress: f64, total: Option<f64>, message: Option<String>) {
        if let Some(report) = &self.progress {
            report(progress, total, message).await;
        }
    }
}

/// Why a tool could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// Tool names are 1 to 64 of `A-Z a-z 0-9 _ -`.
    #[error("invalid tool name `{0}`: use 1 to 64 of A-Z, a-z, 0-9, `_` and `-`")]
    InvalidName(String),
    /// A tool with this name is registered already.
    #[error("a tool named `{0}` is already registered")]
    Duplicate(String),
}

/// Whether `name` is a valid tool name.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The tools a server offers. Cheap to clone; clones share the tools.
#[derive(Clone)]
pub struct ToolRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    tools: RwLock<BTreeMap<String, Arc<dyn Tool>>>,
    generation: watch::Sender<u64>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tools = self
            .inner
            .tools
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("ToolRegistry")
            .field("tools", &tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                tools: RwLock::default(),
                generation: watch::channel(0).0,
            }),
        }
    }

    /// Add `tool`. Fails on an invalid or duplicate name.
    pub fn register(&self, tool: impl Tool) -> Result<(), RegistryError> {
        self.register_arc(Arc::new(tool))
    }

    /// Add a shared tool. Fails on an invalid or duplicate name.
    pub fn register_arc(&self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        let name = tool.definition().name;
        if !valid_name(&name) {
            return Err(RegistryError::InvalidName(name));
        }
        {
            let mut tools = self
                .inner
                .tools
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if tools.contains_key(&name) {
                return Err(RegistryError::Duplicate(name));
            }
            tools.insert(name, tool);
        }
        self.bump();
        Ok(())
    }

    /// Remove the tools called `remove`, then add `add`, as one change (one
    /// `list_changed`). Returns why tools could not be added; the others
    /// are added anyway.
    pub fn update(&self, remove: &[String], add: Vec<Arc<dyn Tool>>) -> Vec<RegistryError> {
        let mut errors = Vec::new();
        let mut changed = false;
        {
            let mut tools = self
                .inner
                .tools
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            for name in remove {
                changed |= tools.remove(name).is_some();
            }
            for tool in add {
                let name = tool.definition().name;
                if !valid_name(&name) {
                    errors.push(RegistryError::InvalidName(name));
                    continue;
                }
                match tools.entry(name) {
                    std::collections::btree_map::Entry::Occupied(e) => {
                        errors.push(RegistryError::Duplicate(e.key().clone()));
                    }
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(tool);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.bump();
        }
        errors
    }

    /// Remove the tool called `name`. Returns whether it was there.
    pub fn remove(&self, name: &str) -> bool {
        let removed = self
            .inner
            .tools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name)
            .is_some();
        if removed {
            self.bump();
        }
        removed
    }

    /// The tool called `name`.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.inner
            .tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Every tool's definition, by name.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner
            .tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|t| t.definition())
            .collect()
    }

    /// Changes: the generation goes up with every register and remove.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.generation.subscribe()
    }

    fn bump(&self) {
        self.inner.generation.send_modify(|g| *g += 1);
    }
}
