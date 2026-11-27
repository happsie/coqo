// SPDX-License-Identifier: Apache-2.0

//! `publish_walkthrough`: the agent publishes a code walkthrough of its
//! change (`walkthroughs/create`, PLAN §7.6, ADR 0020).
//!
//! The arguments are the walkthrough draft itself (`walkthrough.schema.json`,
//! also the MCP resource [`SCHEMA_URI`]). The daemon checks all of it (the
//! shape, references, every anchor against its file, the scope's git
//! revisions) and reports every problem at once with its JSON path, so the
//! bridge does not check the arguments first: its schema check would stop
//! at the first few.

use coqo_protocol::ProtocolError;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::walkthroughs::{
    self as wtm, CreateParams, SCHEMA_JSON, SCHEMA_URI, StorageMode, Walkthrough,
};
use serde_json::{Map, Value, json};

use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

/// `publish_walkthrough`.
#[derive(Debug)]
pub struct PublishWalkthroughTool;

/// The tool's input schema: the committed walkthrough schema, which is
/// self-contained (`$defs`).
fn schema() -> Value {
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap_or_else(|_| json!({}));
    if let Some(obj) = schema.as_object_mut() {
        // Metadata of the file, not of the arguments.
        for key in ["$id", "$comment", "$schema", "title"] {
            obj.remove(key);
        }
        obj.insert("type".into(), json!("object"));
    }
    schema
}

/// What the agent reads back.
fn published(w: &Walkthrough) -> String {
    let mut text = format!(
        "Published walkthrough `{}` \"{}\": {} step{}. The user can replay it in Coqo; the chat shows a card for it.",
        w.id,
        w.title,
        w.steps.len(),
        if w.steps.len() == 1 { "" } else { "s" },
    );
    match (&w.storage.mode, &w.storage.path, &w.storage.error) {
        (StorageMode::Committed, _, Some(e)) => text.push_str(&format!(
            " This workspace commits walkthroughs, but the file could not be written ({e}); it is kept in Coqo only."
        )),
        (StorageMode::Committed, Some(path), None) => text.push_str(&format!(
            " Also written to {path} in the repository for the user to commit (do not commit it yourself unless asked)."
        )),
        _ => {}
    }
    text
}

fn structured(w: &Walkthrough) -> Value {
    json!({
        "id": w.id.0,
        "title": w.title,
        "steps": w.steps.len(),
        "storage": w.storage.mode,
        "path": w.storage.path,
        "baseCommit": w.scope.base_commit,
    })
}

impl Tool for PublishWalkthroughTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "publish_walkthrough",
            format!(
                "Publish a code walkthrough of the change you just made, so the user can replay \
                 it in Coqo step by step. Call it when you have finished a change (code written, \
                 tests run) and before you tell the user it is done; not for a question or a \
                 one-line fix. Order the steps the way a reader should follow the code: entry \
                 point, then flow, data model, logic, edge cases, tests. Anchor each step to the \
                 lines it explains as they are now (re-read the file first; add `text` with a \
                 line of the code to catch stale line numbers), and say what the code does, why \
                 it is done this way, and its risks. Refer to criteria from \
                 set_acceptance_criteria by id in `criteria_refs`, and list hunks that need no \
                 explanation (formatting, imports) in `trivial_hunks`. The whole walkthrough is \
                 checked: if anything is wrong, nothing is published and every problem is listed \
                 with its JSON path, so fix them all and call it again. The schema is also the \
                 resource {SCHEMA_URI}."
            ),
        )
        .with_title("Publish walkthrough")
        .with_schema(schema())
        .checks_own_arguments()
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let params = CreateParams {
                workspace: cx.workspace().clone(),
                session: cx.session().to_owned(),
                walkthrough: Value::Object(args),
            };
            match cx.daemon().request::<wtm::Create>(params).await {
                Ok(w) => {
                    let mut output = ToolOutput::json(structured(&w));
                    output.text = published(&w);
                    Ok(output)
                }
                // The message lists every problem already, without the code.
                Err(ProtocolError::Rpc(e)) if e.code == codes::INVALID_WALKTHROUGH => {
                    Err(ToolError::Failed(e.message))
                }
                Err(e) => Err(e.into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_the_drafts() {
        let def = PublishWalkthroughTool.definition();
        assert!(def.checks_own_arguments);
        let schema = Value::Object(def.input_schema);
        assert_eq!(schema["type"], "object");
        assert!(schema["$defs"]["StepDraft"].is_object(), "{schema}");
        assert!(schema.get("$id").is_none());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("steps")));
        assert!(
            def.description
                .contains("before you tell the user it is done")
        );
        assert!(def.description.contains(SCHEMA_URI));
    }
}
