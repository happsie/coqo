// SPDX-License-Identifier: Apache-2.0

//! Generates the checked-in schema files under `protocol/` from the Rust
//! types, and fails if they are out of date.
//!
//! - `protocol/schema/coqo-protocol.schema.json`: JSON Schema (2020-12) of
//!   every method's params and result, plus a method table.
//! - `protocol/ts/coqo-protocol.ts`: TypeScript types and a typed method map
//!   derived from that schema, a stub for tooling and future clients.
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
use crate::methods::{self, fs, workspace};
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
        topic::<fs::Changes>(
            g,
            "Debounced file-system changes in an open workspace (live only).",
        ),
    ]
}

fn error_codes() -> Value {
    json!({
        "PARSE_ERROR": codes::PARSE_ERROR,
        "INVALID_REQUEST": codes::INVALID_REQUEST,
        "METHOD_NOT_FOUND": codes::METHOD_NOT_FOUND,
        "INVALID_PARAMS": codes::INVALID_PARAMS,
        "INTERNAL_ERROR": codes::INTERNAL_ERROR,
        "INCOMPATIBLE_PROTOCOL": codes::INCOMPATIBLE_PROTOCOL,
        "NOT_INITIALIZED": codes::NOT_INITIALIZED,
        "RESUME_GAP": codes::RESUME_GAP,
        "UNKNOWN_SUBSCRIPTION": codes::UNKNOWN_SUBSCRIPTION,
        "REQUEST_CANCELLED": codes::REQUEST_CANCELLED,
        "UNKNOWN_WORKSPACE": methods::codes::UNKNOWN_WORKSPACE,
        "UNKNOWN_ROOT": methods::codes::UNKNOWN_ROOT,
        "PATH_OUTSIDE_WORKSPACE": methods::codes::PATH_OUTSIDE_WORKSPACE,
        "NOT_FOUND": methods::codes::NOT_FOUND,
        "FILE_TOO_LARGE": methods::codes::FILE_TOO_LARGE,
        "NOT_A_DIRECTORY": methods::codes::NOT_A_DIRECTORY,
        "NOT_A_FILE": methods::codes::NOT_A_FILE,
        "WORKSPACE_CLOSED": methods::codes::WORKSPACE_CLOSED,
        "IO_ERROR": methods::codes::IO_ERROR,
    })
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
            return variants.iter().map(ts_type).collect::<Vec<_>>().join(" | ");
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
}
