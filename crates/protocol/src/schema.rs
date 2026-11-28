// SPDX-License-Identifier: Apache-2.0

//! Generates the checked-in schema files under `protocol/` from the Rust
//! types, and fails if they are out of date.
//!
//! - `protocol/schema/coqo-protocol.schema.json`: JSON Schema (2020-12) of
//!   every method's params and result, plus a method table.
//! - `protocol/ts/coqo-protocol.ts`: TypeScript types and a typed method map
//!   derived from that schema, a stub for tooling and future clients.
//! - `protocol/schema/walkthrough.schema.json`: JSON Schema of a walkthrough
//!   draft, what agents publish (`publish_walkthrough`, PLAN §7.6).
//!
//! Regenerate with `COQO_BLESS=1 cargo test -p coqo-protocol schema`.

use std::fmt::Write as _;
use std::path::PathBuf;

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, SchemaGenerator};
use serde_json::{Map, Value, json};

use crate::daemon;
use crate::handshake::{IncompatibleProtocol, PROTOCOL_VERSION};
use crate::message::{ErrorObject, codes};
use crate::method::{Method, NotificationMethod, Topic, builtin};
use crate::methods::{
    self, agents, alerts, buffer, commands, editor, fs, git, mcp, questions, search, syntax,
    terminal, threads, walkthroughs, workspace,
};
use crate::subscription::ResumeGap;

struct Entry {
    name: &'static str,
    kind: &'static str,
    direction: &'static str,
    doc: &'static str,
    params: Value,
    result: Option<Value>,
}

fn request<M: Method>(g: &mut SchemaGenerator, direction: &'static str, doc: &'static str) -> Entry
where
    M::Params: JsonSchema,
    M::Result: JsonSchema,
{
    Entry {
        name: M::NAME,
        kind: "request",
        direction,
        doc,
        params: g.subschema_for::<M::Params>().to_value(),
        result: Some(g.subschema_for::<M::Result>().to_value()),
    }
}

fn notification<N: NotificationMethod>(
    g: &mut SchemaGenerator,
    direction: &'static str,
    doc: &'static str,
) -> Entry
where
    N::Params: JsonSchema,
{
    Entry {
        name: N::NAME,
        kind: "notification",
        direction,
        doc,
        params: g.subschema_for::<N::Params>().to_value(),
        result: None,
    }
}

fn topic<T: Topic>(g: &mut SchemaGenerator, doc: &'static str) -> Entry
where
    T::Params: JsonSchema,
    T::Event: JsonSchema,
{
    Entry {
        name: T::NAME,
        kind: "topic",
        direction: "server→client",
        doc,
        params: g.subschema_for::<T::Params>().to_value(),
        result: Some(g.subschema_for::<T::Event>().to_value()),
    }
}

fn entries(g: &mut SchemaGenerator) -> Vec<Entry> {
    // Types that only appear in error `data`; registered so they are documented.
    g.subschema_for::<ErrorObject>();
    g.subschema_for::<IncompatibleProtocol>();
    g.subschema_for::<ResumeGap>();
    g.subschema_for::<fs::FileTooLarge>();
    g.subschema_for::<walkthroughs::WalkthroughProblems>();
    vec![
        request::<builtin::Hello>(
            g,
            "client→server",
            "Handshake. Always the first request, always JSON.",
        ),
        request::<builtin::Subscribe>(g, "client→server", "Start a subscription to a topic."),
        request::<builtin::Unsubscribe>(
            g,
            "client→server",
            "Stop a subscription. Result is true if it existed.",
        ),
        notification::<builtin::Event>(g, "server→client", "One event of a subscription."),
        notification::<builtin::SubscriptionEnded>(
            g,
            "server→client",
            "The server ended a subscription, with an error if abnormal.",
        ),
        request::<daemon::Status>(
            g,
            "client→server",
            "Daemon version, pid, uptime, attached clients, busy work and idle policy.",
        ),
        request::<daemon::Shutdown>(
            g,
            "client→server",
            "Stop the daemon. Refused while work is busy unless `force` is set.",
        ),
        request::<workspace::Open>(
            g,
            "client→server",
            "Open (creating if needed) the workspace for a folder, or reopen one by id.",
        ),
        request::<workspace::List>(
            g,
            "client→server",
            "Every known workspace, with the event-log seq the snapshot reflects.",
        ),
        request::<workspace::Get>(g, "client→server", "One workspace."),
        request::<workspace::Close>(
            g,
            "client→server",
            "Close a workspace (stops its watchers).",
        ),
        request::<workspace::Remove>(
            g,
            "client→server",
            "Forget a workspace and its daemon-side data. Result is true if it existed.",
        ),
        request::<workspace::AddRoot>(g, "client→server", "Add a folder root to a workspace."),
        request::<workspace::RemoveRoot>(
            g,
            "client→server",
            "Remove a folder root from a workspace.",
        ),
        request::<workspace::Rename>(g, "client→server", "Rename a workspace."),
        request::<workspace::GetState>(
            g,
            "client→server",
            "A workspace's persisted state document (tabs, layout, settings overrides).",
        ),
        request::<workspace::SetState>(
            g,
            "client→server",
            "Set (or, with null, remove) one workspace state entry.",
        ),
        topic::<workspace::Events>(
            g,
            "Workspace changes, from the persistent event log (resumable with since).",
        ),
        request::<fs::ListDir>(g, "client→server", "List a directory in a workspace root."),
        request::<fs::Stat>(
            g,
            "client→server",
            "Metadata of one path in a workspace root.",
        ),
        request::<fs::ReadFile>(
            g,
            "client→server",
            "Read a file in a workspace root, up to a size limit.",
        ),
        request::<fs::Create>(
            g,
            "client→server",
            "Create an empty file or a folder in a workspace root (daemon token only).",
        ),
        request::<fs::Rename>(
            g,
            "client→server",
            "Rename or move an entry within a workspace root; open buffers follow it (daemon token only).",
        ),
        request::<fs::Delete>(
            g,
            "client→server",
            "Move an entry to the OS trash, or delete it for good with permanent (daemon token only).",
        ),
        request::<fs::Restore>(
            g,
            "client→server",
            "Bring back an entry fs/delete deleted, from the OS trash or Coqo's copy (daemon token only).",
        ),
        request::<fs::Find>(
            g,
            "client→server",
            "Fuzzy-find files in an open workspace from the daemon's file index (daemon token only).",
        ),
        topic::<fs::Changes>(
            g,
            "Debounced file-system changes in an open workspace (live only).",
        ),
        request::<agents::Providers>(
            g,
            "client→server",
            "Every agent provider, with whether it is available.",
        ),
        request::<agents::RunAction>(
            g,
            "client→server",
            "Run a provider's setup action (such as installing its adapter) at the user's request.",
        ),
        request::<agents::List>(
            g,
            "client→server",
            "Agent sessions, with the sessions/changes seq the list reflects.",
        ),
        request::<agents::Get>(
            g,
            "client→server",
            "One session's folded state and the seq of its last update.",
        ),
        request::<agents::Create>(
            g,
            "client→server",
            "Start an agent session with a provider.",
        ),
        request::<agents::Prompt>(
            g,
            "client→server",
            "Start a turn; it runs in the daemon and streams on sessions/updates.",
        ),
        request::<agents::Cancel>(
            g,
            "client→server",
            "Cancel the running turn. Result is true if one was running.",
        ),
        request::<agents::RespondPermission>(
            g,
            "client→server",
            "Answer a pending permission request.",
        ),
        request::<agents::SetMode>(g, "client→server", "Switch a session's mode."),
        request::<agents::SetConfigOption>(
            g,
            "client→server",
            "Change one of a session's config options (model, effort, a toggle, …).",
        ),
        request::<agents::SetStatus>(
            g,
            "client→server",
            "The agent's status line, progress and claimed state (session tokens: own session).",
        ),
        request::<agents::SetCriteria>(
            g,
            "client→server",
            "Set or update the session's acceptance criteria (session tokens: own session).",
        ),
        request::<agents::Rename>(
            g,
            "client→server",
            "Give a session the user's title (kept across restarts; the agent's title no longer replaces it).",
        ),
        request::<agents::Reopen>(
            g,
            "client→server",
            "Reopen a closed session; its next prompt resumes the agent's session.",
        ),
        request::<agents::Retry>(
            g,
            "client→server",
            "Send the session's last prompt again, as a new turn.",
        ),
        request::<agents::Close>(
            g,
            "client→server",
            "End a session; its transcript stays readable. Result is true if it was open.",
        ),
        request::<agents::PermissionRules>(
            g,
            "client→server",
            "A workspace's permission rules (auto-approve and auto-reject), oldest first.",
        ),
        request::<agents::DeletePermissionRule>(
            g,
            "client→server",
            "Delete a permission rule. Result is true if it existed.",
        ),
        request::<agents::ChangedBy>(
            g,
            "client→server",
            "Which agent sessions last changed files of a workspace, most recent first (with each one's title and provider).",
        ),
        request::<agents::ChangedSince>(
            g,
            "client→server",
            "How many files agent sessions changed since a time (every workspace, or one), in all and by session.",
        ),
        topic::<agents::Updates>(
            g,
            "One session's persisted transcript (resumable with since; seqs per session).",
        ),
        topic::<agents::Changes>(
            g,
            "The latest info of each session whose state, title or mode changed (resumable).",
        ),
        request::<buffer::Open>(
            g,
            "client→server",
            "Open (or join) the buffer of a file: text, rev and a replica id.",
        ),
        notification::<buffer::Edit>(
            g,
            "client→server",
            "A replica's change against base_rev; acknowledged by its own buffer/changes event.",
        ),
        notification::<buffer::EditRejected>(
            g,
            "server→client",
            "A buffer/edit the daemon could not apply; reopen the buffer.",
        ),
        topic::<buffer::Changes>(
            g,
            "Every committed change of a buffer; seq is the buffer rev (resumable with since).",
        ),
        request::<buffer::RevertChange>(
            g,
            "client→server",
            "Undo one change (typically an agent's), transformed past later changes.",
        ),
        request::<buffer::Save>(
            g,
            "client→server",
            "Write the buffer to its file atomically.",
        ),
        request::<buffer::Close>(
            g,
            "client→server",
            "A replica is done with a buffer; it unloads once unsubscribed.",
        ),
        request::<syntax::Symbols>(
            g,
            "client→server",
            "Every symbol of an open buffer (tree-sitter tags), flat, with container paths.",
        ),
        request::<syntax::Outline>(
            g,
            "client→server",
            "The symbols of an open buffer as a tree.",
        ),
        request::<syntax::Folding>(
            g,
            "client→server",
            "The foldable line ranges of an open buffer.",
        ),
        request::<terminal::Spawn>(
            g,
            "client→server",
            "Start a program (or the login shell) in a new daemon-owned PTY.",
        ),
        request::<terminal::List>(g, "client→server", "Every terminal, running or exited."),
        request::<terminal::Snapshot>(
            g,
            "client→server",
            "A terminal's screen and the terminal/output seq it reflects.",
        ),
        request::<terminal::Input>(
            g,
            "client→server",
            "Write to a terminal's PTY. Send the next one after this one's response.",
        ),
        request::<terminal::Resize>(g, "client→server", "Resize a terminal, in cells."),
        request::<terminal::Scroll>(
            g,
            "client→server",
            "Move a terminal's viewport through its scrollback.",
        ),
        request::<terminal::Close>(
            g,
            "client→server",
            "Kill a terminal's process group and forget it. True if it existed.",
        ),
        topic::<terminal::Output>(
            g,
            "A terminal's changed rows (batched at ~60 Hz), title, cwd and exit.",
        ),
        notification::<editor::PublishContext>(
            g,
            "client→server",
            "A UI window's editor context (tabs, selections, visible lines) for its workspace.",
        ),
        request::<editor::GetContext>(
            g,
            "client→server",
            "What the windows showing a workspace have open, focused window first.",
        ),
        request::<editor::Reveal>(
            g,
            "client→server",
            "Ask the UI to open a file and select a range.",
        ),
        topic::<editor::Reveals>(g, "Reveal requests for UIs to act on (live only)."),
        request::<commands::List>(
            g,
            "client→server",
            "The commands mods contribute, for the command palette and keybindings.",
        ),
        request::<commands::Run>(g, "client→server", "Run a mod-contributed command."),
        request::<mcp::Tools>(
            g,
            "client→server",
            "The MCP tools mods contribute, as the coqo-editor bridge offers them.",
        ),
        request::<mcp::Call>(
            g,
            "client→server",
            "Call a mod-contributed MCP tool for an agent session.",
        ),
        topic::<mcp::ToolsChanged>(
            g,
            "The mod-contributed MCP tools changed; list them again (live only).",
        ),
        request::<questions::List>(
            g,
            "client→server",
            "A workspace's questions and notes, anchors resolved now, with the changes seq.",
        ),
        request::<questions::Get>(g, "client→server", "One question, anchors resolved now."),
        request::<questions::Answer>(g, "client→server", "Answer an open question."),
        request::<questions::Dismiss>(
            g,
            "client→server",
            "Close a question without an answer, or a note.",
        ),
        request::<questions::Ask>(
            g,
            "client→server",
            "The user asks an agent session about code: a thread pinned to it.",
        ),
        request::<questions::Reply>(
            g,
            "client→server",
            "A follow-up in a thread the user started.",
        ),
        request::<questions::Create>(
            g,
            "client→server",
            "An agent asks a question or adds a note pinned to code.",
        ),
        topic::<questions::Changes>(
            g,
            "Questions whose state changed, as their latest state (resumable).",
        ),
        topic::<questions::Watch>(
            g,
            "One question for the agent that asked: its state now, then every change.",
        ),
        request::<git::Status>(
            g,
            "client→server",
            "A root's repository: branch and changed paths.",
        ),
        request::<git::Diff>(
            g,
            "client→server",
            "One file's hunks between two sides (work tree, index, a revision).",
        ),
        request::<git::DiffFiles>(
            g,
            "client→server",
            "The files that differ between two sides, with line counts.",
        ),
        request::<git::Stage>(
            g,
            "client→server",
            "Stage whole files, or chosen lines of one file.",
        ),
        request::<git::Unstage>(
            g,
            "client→server",
            "Unstage whole files, or chosen lines of one file.",
        ),
        request::<git::Discard>(
            g,
            "client→server",
            "Throw away unstaged changes: files or lines back to the index version, untracked files to the trash.",
        ),
        request::<git::Commit>(g, "client→server", "Commit what is staged, or amend."),
        request::<git::Branches>(g, "client→server", "Local and remote branches."),
        request::<git::SwitchBranch>(g, "client→server", "Switch to a branch."),
        request::<git::CreateBranch>(
            g,
            "client→server",
            "Create a branch, and optionally switch to it.",
        ),
        request::<git::StashList>(g, "client→server", "The stash, newest first."),
        request::<git::StashPush>(g, "client→server", "Stash the changes."),
        request::<git::StashPop>(g, "client→server", "Apply a stash and drop it."),
        request::<git::StashApply>(g, "client→server", "Apply a stash and keep it."),
        request::<git::StashDrop>(g, "client→server", "Drop a stash."),
        request::<git::Blame>(
            g,
            "client→server",
            "Which commit last changed each line of a file.",
        ),
        request::<git::Log>(
            g,
            "client→server",
            "Commits newest first, a page at a time, with graph lanes.",
        ),
        topic::<git::Changes>(
            g,
            "A root's repository may have changed: work tree, index, HEAD, refs or stash (live only).",
        ),
        request::<alerts::List>(
            g,
            "client→server",
            "The alerts not dismissed, newest first, with the changes seq.",
        ),
        request::<alerts::MarkRead>(
            g,
            "client→server",
            "Mark alerts (or an agent session's) read.",
        ),
        request::<alerts::Dismiss>(g, "client→server", "Hide alerts."),
        request::<alerts::DismissAll>(g, "client→server", "Hide every alert, or a workspace's."),
        request::<alerts::GetSettings>(
            g,
            "client→server",
            "Do-not-disturb and the alert sources, muted or not.",
        ),
        request::<alerts::SetSettings>(
            g,
            "client→server",
            "Change do-not-disturb or mute sources.",
        ),
        topic::<alerts::Changes>(
            g,
            "Alerts whose state changed, as their latest state (resumable).",
        ),
        request::<walkthroughs::List>(
            g,
            "client→server",
            "A workspace's walkthroughs, newest first.",
        ),
        request::<walkthroughs::Get>(
            g,
            "client→server",
            "One walkthrough, anchors resolved against the files as they are now.",
        ),
        request::<walkthroughs::Delete>(
            g,
            "client→server",
            "Forget a walkthrough (and optionally its committed file).",
        ),
        request::<walkthroughs::SetStepState>(
            g,
            "client→server",
            "Mark a step unread, understood or with a question.",
        ),
        request::<walkthroughs::Create>(
            g,
            "client→server",
            "An agent publishes a walkthrough; every problem of a bad draft is reported at once.",
        ),
        topic::<walkthroughs::Changes>(
            g,
            "Walkthroughs published, updated or deleted (live only).",
        ),
        request::<threads::List>(
            g,
            "client→server",
            "A workspace's comment threads, anchors resolved now, with the changes seq.",
        ),
        request::<threads::Get>(g, "client→server", "One thread, anchors resolved now."),
        request::<threads::Create>(
            g,
            "client→server",
            "Start a thread on lines of a file version (or the whole file).",
        ),
        request::<threads::Reply>(g, "client→server", "Add the user's comment to a thread."),
        request::<threads::EditComment>(g, "client→server", "Change one of the user's comments."),
        request::<threads::DeleteComment>(
            g,
            "client→server",
            "Delete one of the user's comments; an empty thread goes with it.",
        ),
        request::<threads::Resolve>(g, "client→server", "Mark a thread resolved."),
        request::<threads::Reopen>(g, "client→server", "Open a resolved thread again."),
        request::<threads::Pending>(
            g,
            "client→server",
            "The user's pending review: the comments not sent yet.",
        ),
        request::<threads::Discard>(g, "client→server", "Delete the pending review's comments."),
        request::<threads::SendReview>(
            g,
            "client→server",
            "Send the pending review to an agent session in one prompt, then clear pending.",
        ),
        request::<threads::AgentReply>(
            g,
            "client→server",
            "An agent session replies in a review thread of its workspace, optionally resolving it.",
        ),
        topic::<threads::Changes>(
            g,
            "Threads whose state changed, as their latest state, and deleted ones (resumable).",
        ),
        topic::<search::Run>(
            g,
            "Search a workspace: batches of files with matches as they are searched, then a summary; unsubscribe to cancel.",
        ),
        request::<search::Replace>(
            g,
            "client→server",
            "Replace a search's matches in chosen files: open buffers are edited, other files written.",
        ),
    ]
}

fn error_codes() -> Value {
    // A list, not `json!`: one macro call this long hits the recursion limit.
    let all: &[(&str, i64)] = &[
        ("PARSE_ERROR", codes::PARSE_ERROR),
        ("INVALID_REQUEST", codes::INVALID_REQUEST),
        ("METHOD_NOT_FOUND", codes::METHOD_NOT_FOUND),
        ("INVALID_PARAMS", codes::INVALID_PARAMS),
        ("INTERNAL_ERROR", codes::INTERNAL_ERROR),
        ("INCOMPATIBLE_PROTOCOL", codes::INCOMPATIBLE_PROTOCOL),
        ("NOT_INITIALIZED", codes::NOT_INITIALIZED),
        ("RESUME_GAP", codes::RESUME_GAP),
        ("UNKNOWN_SUBSCRIPTION", codes::UNKNOWN_SUBSCRIPTION),
        ("REQUEST_CANCELLED", codes::REQUEST_CANCELLED),
        ("UNKNOWN_WORKSPACE", methods::codes::UNKNOWN_WORKSPACE),
        ("UNKNOWN_ROOT", methods::codes::UNKNOWN_ROOT),
        (
            "PATH_OUTSIDE_WORKSPACE",
            methods::codes::PATH_OUTSIDE_WORKSPACE,
        ),
        ("NOT_FOUND", methods::codes::NOT_FOUND),
        ("FILE_TOO_LARGE", methods::codes::FILE_TOO_LARGE),
        ("NOT_A_DIRECTORY", methods::codes::NOT_A_DIRECTORY),
        ("NOT_A_FILE", methods::codes::NOT_A_FILE),
        ("WORKSPACE_CLOSED", methods::codes::WORKSPACE_CLOSED),
        ("IO_ERROR", methods::codes::IO_ERROR),
        ("ALREADY_EXISTS", methods::codes::ALREADY_EXISTS),
        ("PERMISSION_DENIED", methods::codes::PERMISSION_DENIED),
        ("TRASH_FAILED", methods::codes::TRASH_FAILED),
        ("UNAUTHORIZED", codes::UNAUTHORIZED),
        ("FORBIDDEN", codes::FORBIDDEN),
        ("UNKNOWN_SESSION", methods::codes::UNKNOWN_SESSION),
        ("UNKNOWN_PROVIDER", methods::codes::UNKNOWN_PROVIDER),
        ("SESSION_BUSY", methods::codes::SESSION_BUSY),
        ("SESSION_CLOSED", methods::codes::SESSION_CLOSED),
        (
            "UNKNOWN_PERMISSION_REQUEST",
            methods::codes::UNKNOWN_PERMISSION_REQUEST,
        ),
        ("UNSUPPORTED", methods::codes::UNSUPPORTED),
        ("AUTH_REQUIRED", methods::codes::AUTH_REQUIRED),
        ("PROVIDER_ERROR", methods::codes::PROVIDER_ERROR),
        ("STORAGE_ERROR", methods::codes::STORAGE_ERROR),
        ("SESSION_ENDED", methods::codes::SESSION_ENDED),
        ("UNKNOWN_BUFFER", methods::codes::UNKNOWN_BUFFER),
        ("UNSUPPORTED_ENCODING", methods::codes::UNSUPPORTED_ENCODING),
        ("REVISION_GONE", methods::codes::REVISION_GONE),
        ("INVALID_EDIT", methods::codes::INVALID_EDIT),
        ("SAVE_CONFLICT", methods::codes::SAVE_CONFLICT),
        ("NO_SUCH_TERMINAL", methods::codes::NO_SUCH_TERMINAL),
        ("SPAWN_FAILED", methods::codes::SPAWN_FAILED),
        ("UNKNOWN_TOOL", methods::codes::UNKNOWN_TOOL),
        ("NOT_A_REPOSITORY", methods::codes::NOT_A_REPOSITORY),
        ("STALE_SELECTION", methods::codes::STALE_SELECTION),
        ("GIT_FAILED", methods::codes::GIT_FAILED),
        ("UNKNOWN_WALKTHROUGH", methods::codes::UNKNOWN_WALKTHROUGH),
        ("INVALID_WALKTHROUGH", methods::codes::INVALID_WALKTHROUGH),
        ("UNKNOWN_THREAD", methods::codes::UNKNOWN_THREAD),
        ("UNKNOWN_COMMENT", methods::codes::UNKNOWN_COMMENT),
        ("NOT_YOUR_COMMENT", methods::codes::NOT_YOUR_COMMENT),
        ("UNKNOWN_COMMAND", methods::codes::UNKNOWN_COMMAND),
        ("COMMAND_FAILED", methods::codes::COMMAND_FAILED),
        ("INVALID_QUERY", methods::codes::INVALID_QUERY),
        ("SEARCH_UNAVAILABLE", methods::codes::SEARCH_UNAVAILABLE),
    ];
    Value::Object(
        all.iter()
            .map(|(name, code)| ((*name).to_owned(), json!(code)))
            .collect(),
    )
}

fn generate() -> (Value, Vec<Entry>) {
    let mut g = SchemaSettings::draft2020_12().into_generator();
    let entries = entries(&mut g);
    let defs = g.take_definitions(true);
    let methods: Map<String, Value> = entries
        .iter()
        .map(|e| {
            let mut m = json!({
                "kind": e.kind,
                "direction": e.direction,
                "description": e.doc,
                "params": e.params,
            });
            if let Some(r) = &e.result {
                let key = if e.kind == "topic" { "event" } else { "result" };
                m[key] = r.clone();
            }
            (e.name.to_owned(), m)
        })
        .collect();
    let doc = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$comment": "Generated from crates/protocol by `COQO_BLESS=1 cargo test -p coqo-protocol schema`. Do not edit.",
        "title": "Coqo Protocol",
        "x-protocol-version": PROTOCOL_VERSION,
        "x-framing": "u32 big-endian payload length, then one JSON-RPC 2.0 message (JSON, or MessagePack if negotiated)",
        "x-error-codes": error_codes(),
        "x-methods": methods,
        "$defs": defs,
    });
    (canonical(doc), entries)
}

/// Sort object keys recursively, so the output does not depend on whether
/// serde_json's `preserve_order` feature is unified on in this build.
fn canonical(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, canonical(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

// ---- TypeScript ------------------------------------------------------------

fn ts_type(schema: &Value) -> String {
    let Some(obj) = schema.as_object() else {
        return if schema == &Value::Bool(false) {
            "never".into()
        } else {
            "unknown".into()
        };
    };
    if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
        return r.rsplit('/').next().unwrap_or("unknown").to_owned();
    }
    if let Some(c) = obj.get("const") {
        return c.to_string();
    }
    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(variants) = obj.get(key).and_then(Value::as_array) {
            let union = variants.iter().map(ts_type).collect::<Vec<_>>().join(" | ");
            // Fields shared by every variant (a `#[serde(flatten)]` enum).
            if obj.contains_key("properties") {
                return format!("{} & ({union})", ts_object(obj));
            }
            return union;
        }
    }
    match obj.get("type") {
        Some(Value::Array(types)) => types
            .iter()
            .map(|t| ts_primitive(t.as_str().unwrap_or(""), obj))
            .collect::<Vec<_>>()
            .join(" | "),
        Some(Value::String(t)) => ts_primitive(t, obj),
        _ => "unknown".into(),
    }
}

fn ts_primitive(t: &str, obj: &Map<String, Value>) -> String {
    match t {
        "string" => "string".into(),
        "integer" | "number" => "number".into(),
        "boolean" => "boolean".into(),
        "null" => "null".into(),
        "array" => match obj.get("prefixItems").and_then(Value::as_array) {
            Some(items) => format!(
                "[{}]",
                items.iter().map(ts_type).collect::<Vec<_>>().join(", ")
            ),
            None => format!(
                "Array<{}>",
                obj.get("items").map_or("unknown".into(), ts_type)
            ),
        },
        "object" => ts_object(obj),
        _ => "unknown".into(),
    }
}

fn ts_object(obj: &Map<String, Value>) -> String {
    let Some(props) = obj.get("properties").and_then(Value::as_object) else {
        return "Record<string, unknown>".into();
    };
    let required: Vec<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = String::from("{\n");
    for (name, prop) in props {
        if let Some(d) = prop.get("description").and_then(Value::as_str) {
            let _ = writeln!(out, "  /** {} */", d.replace('\n', " "));
        }
        let opt = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        let _ = writeln!(out, "  {name}{opt}: {};", ts_type(prop));
    }
    out.push('}');
    out
}

fn typescript(doc: &Value, entries: &[Entry]) -> String {
    let mut out = String::new();
    out.push_str(
        "// SPDX-License-Identifier: Apache-2.0\n\
         // Generated from crates/protocol by `COQO_BLESS=1 cargo test -p coqo-protocol schema`.\n\
         // Do not edit. Types for the Coqo Protocol, for tooling and future clients.\n\n",
    );
    let _ = writeln!(
        out,
        "export const PROTOCOL_VERSION = {{ major: {}, minor: {} }} as const;\n",
        PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor
    );

    out.push_str(
        "/** JSON-RPC 2.0 error codes used by the protocol. */\nexport const ErrorCodes = {\n",
    );
    for (k, v) in canonical(error_codes()).as_object().unwrap() {
        let _ = writeln!(out, "  {k}: {v},");
    }
    out.push_str("} as const;\n\n");

    out.push_str(
        "export type Id = number | string;\n\n\
         export interface Request<P = unknown> { jsonrpc: \"2.0\"; id: Id; method: string; params?: P }\n\
         export interface Notification<P = unknown> { jsonrpc: \"2.0\"; method: string; params?: P }\n\
         export type Response<R = unknown> =\n  \
           | { jsonrpc: \"2.0\"; id: Id; result: R }\n  \
           | { jsonrpc: \"2.0\"; id: Id | null; error: ErrorObject };\n\
         export type Message = Request | Notification | Response;\n\n",
    );

    let defs = doc["$defs"].as_object().unwrap();
    for (name, schema) in defs {
        if let Some(d) = schema.get("description").and_then(Value::as_str) {
            let _ = writeln!(out, "/** {} */", d.replace('\n', " "));
        }
        let _ = writeln!(out, "export type {name} = {};\n", ts_type(schema));
    }

    out.push_str(
        "/** Requests: method name → params and result. */\nexport interface Requests {\n",
    );
    for e in entries.iter().filter(|e| e.kind == "request") {
        let _ = writeln!(
            out,
            "  /** {} ({}) */\n  \"{}\": {{ params: {}; result: {} }};",
            e.doc,
            e.direction,
            e.name,
            ts_type(&e.params),
            ts_type(e.result.as_ref().unwrap())
        );
    }
    out.push_str(
        "}\n\n/** Notifications: method name → params. */\nexport interface Notifications {\n",
    );
    for e in entries.iter().filter(|e| e.kind == "notification") {
        let _ = writeln!(
            out,
            "  /** {} ({}) */\n  \"{}\": {{ params: {} }};",
            e.doc,
            e.direction,
            e.name,
            ts_type(&e.params)
        );
    }
    out.push_str(
        "}\n\n/** Topics (`coqo/subscribe`): topic name → params and event. */\nexport interface Topics {\n",
    );
    for e in entries.iter().filter(|e| e.kind == "topic") {
        let _ = writeln!(
            out,
            "  /** {} */\n  \"{}\": {{ params: {}; event: {} }};",
            e.doc,
            e.name,
            ts_type(&e.params),
            ts_type(e.result.as_ref().unwrap())
        );
    }
    out.push_str(
        "}\n\n\
         /** Encode one frame: u32 big-endian length, then the JSON payload. */\n\
         export function encodeFrame(message: Message): Uint8Array {\n  \
           const body = new TextEncoder().encode(JSON.stringify(message));\n  \
           const frame = new Uint8Array(4 + body.length);\n  \
           new DataView(frame.buffer).setUint32(0, body.length, false);\n  \
           frame.set(body, 4);\n  \
           return frame;\n\
         }\n",
    );
    out
}

// ---- golden test -----------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn check_or_bless(rel: &str, contents: &str) {
    let path = repo_root().join(rel);
    if std::env::var_os("COQO_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        return;
    }
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        current == contents,
        "{rel} is out of date; regenerate with `COQO_BLESS=1 cargo test -p coqo-protocol schema`"
    );
}

#[test]
fn schema_files_are_up_to_date() {
    let (doc, entries) = generate();
    let json = serde_json::to_string_pretty(&doc).unwrap() + "\n";
    check_or_bless("protocol/schema/coqo-protocol.schema.json", &json);
    check_or_bless("protocol/ts/coqo-protocol.ts", &typescript(&doc, &entries));
    let walkthrough = serde_json::to_string_pretty(&walkthrough_schema()).unwrap() + "\n";
    check_or_bless("protocol/schema/walkthrough.schema.json", &walkthrough);
}

/// The schema of what agents publish (`publish_walkthrough`, served as the
/// MCP resource `coqo://schemas/walkthrough.json`): self-contained, with
/// its own `$defs`.
fn walkthrough_schema() -> Value {
    let g = SchemaSettings::draft2020_12().into_generator();
    let mut schema = g
        .into_root_schema_for::<walkthroughs::WalkthroughDraft>()
        .to_value();
    let obj = schema.as_object_mut().unwrap();
    obj.insert("$id".into(), json!(walkthroughs::SCHEMA_URI));
    obj.insert(
        "$comment".into(),
        json!("Generated from crates/protocol (WalkthroughDraft) by `COQO_BLESS=1 cargo test -p coqo-protocol schema`. Do not edit."),
    );
    canonical(schema)
}

#[test]
fn the_walkthrough_schema_describes_the_draft() {
    let schema = walkthrough_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let required = schema["required"].as_array().unwrap();
    for field in ["title", "summary", "scope", "steps"] {
        assert!(required.contains(&json!(field)), "{field}");
    }
    let step = &schema["$defs"]["StepDraft"];
    assert!(step["properties"]["criteria_refs"].is_object(), "{step}");
    assert!(step["properties"]["depends_on"].is_object(), "{step}");
    assert_eq!(step["additionalProperties"], false);
    // The committed copy the bridge serves is this schema.
    let committed: Value = serde_json::from_str(walkthroughs::SCHEMA_JSON).unwrap();
    assert_eq!(
        committed, schema,
        "walkthrough.schema.json is out of date; regenerate with `COQO_BLESS=1 cargo test -p coqo-protocol schema`"
    );
}

#[test]
fn schema_covers_every_builtin_method() {
    let (doc, _) = generate();
    let methods = doc["x-methods"].as_object().unwrap();
    for name in [
        builtin::Hello::NAME,
        builtin::Subscribe::NAME,
        builtin::Unsubscribe::NAME,
        builtin::Event::NAME,
        builtin::SubscriptionEnded::NAME,
        daemon::Status::NAME,
        daemon::Shutdown::NAME,
        agents::Providers::NAME,
        agents::RunAction::NAME,
        agents::Create::NAME,
        agents::Prompt::NAME,
        agents::SetStatus::NAME,
        agents::SetCriteria::NAME,
        agents::Rename::NAME,
        agents::Reopen::NAME,
        agents::Retry::NAME,
        agents::PermissionRules::NAME,
        agents::DeletePermissionRule::NAME,
        agents::ChangedBy::NAME,
        agents::ChangedSince::NAME,
        agents::Updates::NAME,
        agents::Changes::NAME,
        buffer::Open::NAME,
        buffer::Edit::NAME,
        buffer::EditRejected::NAME,
        buffer::Changes::NAME,
        buffer::Save::NAME,
        syntax::Symbols::NAME,
        syntax::Outline::NAME,
        syntax::Folding::NAME,
    ] {
        assert!(methods.contains_key(name), "{name} missing from the schema");
    }
    for def in [
        "Hello",
        "Welcome",
        "ErrorObject",
        "ResumeGap",
        "IncompatibleProtocol",
    ] {
        assert!(doc["$defs"].get(def).is_some(), "{def} missing from $defs");
    }
}

#[test]
fn schema_covers_the_application_methods() {
    let (doc, _) = generate();
    let methods = doc["x-methods"].as_object().unwrap();
    for name in [
        workspace::Open::NAME,
        workspace::List::NAME,
        workspace::Get::NAME,
        workspace::Close::NAME,
        workspace::Remove::NAME,
        workspace::AddRoot::NAME,
        workspace::RemoveRoot::NAME,
        workspace::Rename::NAME,
        workspace::GetState::NAME,
        workspace::SetState::NAME,
        fs::ListDir::NAME,
        fs::Stat::NAME,
        fs::ReadFile::NAME,
        fs::Create::NAME,
        fs::Rename::NAME,
        fs::Delete::NAME,
        fs::Restore::NAME,
        fs::Find::NAME,
    ] {
        assert_eq!(methods[name]["kind"], "request", "{name}");
    }
    for name in [workspace::Events::NAME, fs::Changes::NAME] {
        assert_eq!(methods[name]["kind"], "topic", "{name}");
        assert!(methods[name].get("event").is_some(), "{name} has no event");
    }
    assert!(doc["$defs"].get("FileTooLarge").is_some());
}

/// Wire shapes the UI relies on.
#[test]
fn application_wire_shapes() {
    let ev = workspace::WorkspaceEvent::StateChanged {
        id: "ws-1".into(),
        key: "tabs".into(),
        value: json!([]),
    };
    assert_eq!(
        serde_json::to_value(&ev).unwrap(),
        json!({"type": "stateChanged", "id": "ws-1", "key": "tabs", "value": []})
    );
    let open: workspace::OpenParams = serde_json::from_value(json!({"path": "/src"})).unwrap();
    assert_eq!(open.path.as_deref(), Some("/src"));
    let read: fs::ReadParams = serde_json::from_value(
        json!({"workspace": "ws-1", "root": 1, "path": "a", "maxBytes": 10}),
    )
    .unwrap();
    assert_eq!(read.max_bytes, Some(10));
    let list: fs::PathParams =
        serde_json::from_value(json!({"workspace": "ws-1", "root": 1})).unwrap();
    assert_eq!(list.path, "");
    let create: fs::CreateParams = serde_json::from_value(
        json!({"workspace": "ws-1", "root": 1, "path": "src/new", "kind": "dir"}),
    )
    .unwrap();
    assert_eq!(create.kind, fs::EntryKind::Dir);
    let delete: fs::DeleteParams =
        serde_json::from_value(json!({"workspace": "ws-1", "root": 1, "path": "a"})).unwrap();
    assert!(!delete.permanent);
    assert_eq!(
        serde_json::to_value(&delete).unwrap(),
        json!({"workspace": "ws-1", "root": 1, "path": "a"})
    );
    let find: fs::FindParams = serde_json::from_value(json!({"workspace": "ws-1"})).unwrap();
    assert_eq!((find.query.as_str(), find.limit), ("", None));
    let found = fs::FindResult {
        matches: vec![fs::FindMatch {
            root: workspace::RootId(1),
            path: "src/main.rs".into(),
            score: 42,
            positions: vec![4, 5],
        }],
        total: 1,
        files: 10,
        indexing: false,
    };
    assert_eq!(
        serde_json::to_value(&found).unwrap(),
        json!({
            "matches": [{"root": 1, "path": "src/main.rs", "score": 42, "positions": [4, 5]}],
            "total": 1,
            "files": 10
        })
    );
}

/// The git wire shapes the UI and later clients (diff view, review threads,
/// walkthroughs) rely on.
#[test]
fn git_wire_shapes() {
    use git::{DiffParams, DiffSide, StageParams};
    let p: DiffParams = serde_json::from_value(json!({
        "workspace": "ws-1", "root": 1, "path": "src/a.rs",
        "base": {"rev": "main"}, "head": "worktree"
    }))
    .unwrap();
    assert_eq!(p.base, DiffSide::Rev("main".into()));
    assert_eq!(p.head, DiffSide::Worktree);
    assert_eq!(
        serde_json::to_value(DiffSide::Index).unwrap(),
        json!("index")
    );
    let s: StageParams = serde_json::from_value(json!({
        "workspace": "ws-1", "root": 1, "paths": ["a"], "lines": {"added": [3]}
    }))
    .unwrap();
    assert_eq!(s.lines.unwrap().added, vec![3]);
    let entry = git::StatusEntry {
        path: "a".into(),
        staged: Some(git::FileChange::Added),
        unstaged: Some(git::FileChange::Modified),
        conflict: None,
        orig_path: None,
    };
    assert_eq!(
        serde_json::to_value(&entry).unwrap(),
        json!({"path": "a", "staged": "added", "unstaged": "modified"})
    );
    assert_eq!(entry.letter(), Some('M'));

    // Renames (#226): additive, an `origPath` beside the new path.
    let renamed = git::StatusEntry {
        path: "b.rs".into(),
        staged: Some(git::FileChange::Renamed),
        unstaged: None,
        conflict: None,
        orig_path: Some("a.rs".into()),
    };
    assert_eq!(
        serde_json::to_value(&renamed).unwrap(),
        json!({"path": "b.rs", "staged": "renamed", "origPath": "a.rs"})
    );
    assert_eq!(renamed.letter(), Some('R'));
    let d: DiffParams = serde_json::from_value(json!({
        "workspace": "ws-1", "root": 1, "path": "b.rs", "origPath": "a.rs",
        "base": {"rev": "HEAD"}, "head": "index"
    }))
    .unwrap();
    assert_eq!(d.orig_path.as_deref(), Some("a.rs"));

    // Discard (#226).
    let p: git::DiscardParams = serde_json::from_value(json!({
        "workspace": "ws-1", "root": 1, "paths": ["a"], "lines": {"removed": [2]}
    }))
    .unwrap();
    assert_eq!(p.lines.unwrap().removed, vec![2]);
    assert_eq!(
        serde_json::to_value(git::DiscardResult {
            trashed: vec![git::TrashedFile {
                path: "sub/new.txt".into(),
                root_path: "new.txt".into(),
                restore: Some("t1".into()),
                no_restore: None,
            }],
        })
        .unwrap(),
        json!({"trashed": [{"path": "sub/new.txt", "rootPath": "new.txt", "restore": "t1"}]})
    );
}
