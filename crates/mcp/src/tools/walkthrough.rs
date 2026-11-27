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
//!
//! The daemon also checks coverage (#99): every changed hunk of the scope
//! must be in a step's anchors or in `trivial_hunks`. A published
//! walkthrough that leaves hunks uncovered says so first thing in the
//! result, with each hunk, so the agent can explain them and publish again
//! (`require_coverage` refuses it instead).

use coqo_protocol::ProtocolError;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::walkthroughs::{
    self as wtm, CreateParams, SCHEMA_JSON, SCHEMA_URI, StorageMode, Walkthrough,
    WalkthroughCoverage,
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

/// Most uncovered hunks spelled out in the result.
const RESULT_UNCOVERED: usize = 30;

/// The coverage, for the agent: the uncovered hunks first and in full.
fn coverage_text(c: &WalkthroughCoverage) -> String {
    if let Some(why) = &c.unavailable {
        return format!("Coverage was not checked: {why}.");
    }
    let n = c.uncovered_count();
    let counts = format!(
        "{} of {} changed hunk{} covered ({} by steps, {} trivial)",
        c.covered + c.trivial,
        c.total,
        if c.total == 1 { "" } else { "s" },
        c.covered,
        c.trivial,
    );
    if n == 0 {
        return format!("Coverage: {counts}: every changed hunk is explained.");
    }
    let mut text = format!(
        "COVERAGE INCOMPLETE: {counts}. {n} hunk{} {} not explained by any step and not in trivial_hunks, and the user sees {} flagged:",
        if n == 1 { "" } else { "s" },
        if n == 1 { "is" } else { "are" },
        if n == 1 { "it" } else { "them" },
    );
    for h in c.uncovered.iter().take(RESULT_UNCOVERED) {
        text.push_str(&format!("\n- {} ({})", h.location(), h.change()));
        for line in h.excerpt.lines() {
            text.push_str(&format!("\n    {line}"));
        }
    }
    let listed = c.uncovered.len().min(RESULT_UNCOVERED);
    if (n as usize) > listed {
        text.push_str(&format!("\n- … and {} more", n as usize - listed));
    }
    text.push_str(
        "\nAdd each to a step's anchors (a new step, or an existing step's anchors) or to trivial_hunks with a reason, and call publish_walkthrough again.",
    );
    text
}

/// What the agent reads back.
fn published(w: &Walkthrough) -> String {
    let incomplete = w
        .coverage
        .as_ref()
        .filter(|c| c.unavailable.is_none() && c.uncovered_count() > 0);
    let mut text = String::new();
    if let Some(c) = incomplete {
        // First, so it is not missed.
        text.push_str(&coverage_text(c));
        text.push_str("\n\n");
    }
    text.push_str(&format!(
        "Published walkthrough `{}` \"{}\": {} step{}. The user can replay it in Coqo; the chat shows a card for it.",
        w.id,
        w.title,
        w.steps.len(),
        if w.steps.len() == 1 { "" } else { "s" },
    ));
    match (&w.storage.mode, &w.storage.path, &w.storage.error) {
        (StorageMode::Committed, _, Some(e)) => text.push_str(&format!(
            " This workspace commits walkthroughs, but the file could not be written ({e}); it is kept in Coqo only."
        )),
        (StorageMode::Committed, Some(path), None) => text.push_str(&format!(
            " Also written to {path} in the repository for the user to commit (do not commit it yourself unless asked)."
        )),
        _ => {}
    }
    if let Some(c) = w.coverage.as_ref().filter(|_| incomplete.is_none()) {
        text.push(' ');
        text.push_str(&coverage_text(c));
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
        "coverage": w.coverage,
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
                 with its JSON path, so fix them all and call it again. Coqo also diffs the scope \
                 and checks that every changed hunk is in a step's anchors or in `trivial_hunks`; \
                 the result lists any hunk that is not (set `require_coverage` to refuse the \
                 walkthrough instead), so explain those and publish again. The schema is also the \
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
        assert!(def.description.contains("require_coverage"));
    }

    fn walkthrough(coverage: Option<WalkthroughCoverage>) -> Walkthrough {
        serde_json::from_value(json!({
            "id": "circle-area-0a1b2c3d",
            "workspace": "ws",
            "title": "Circle area",
            "summary": "s",
            "scope": { "kind": "worktree", "root": 0 },
            "steps": [],
            "coverage": coverage,
            "createdBy": { "session": "s", "agent": "fake" },
            "createdAt": 1,
        }))
        .unwrap()
    }

    #[test]
    fn uncovered_hunks_lead_the_result() {
        let coverage: WalkthroughCoverage = serde_json::from_value(json!({
            "total": 3, "covered": 1, "trivial": 0,
            "uncovered": [{
                "root": 0, "path": "src/main.rs",
                "newRange": { "start": 9, "end": 11 },
                "added": 3, "removed": 0, "excerpt": "+fn perimeter() {\n+    2.0",
            }, {
                "root": 0, "path": "old.rs",
                "newRange": { "start": 0, "end": 0 },
                "oldRange": { "start": 0, "end": 4 },
                "added": 0, "removed": 5, "wholeFile": "deleted", "excerpt": "-fn old() {}",
            }],
        }))
        .unwrap();
        let text = published(&walkthrough(Some(coverage)));
        assert!(
            text.starts_with("COVERAGE INCOMPLETE: 1 of 3 changed hunks covered (1 by steps, 0 trivial). 2 hunks are not explained"),
            "{text}"
        );
        assert!(
            text.contains("\n- src/main.rs:10-12 (+3)\n    +fn perimeter() {\n    +    2.0"),
            "{text}"
        );
        assert!(
            text.contains("\n- old.rs (file deleted)\n    -fn old() {}"),
            "{text}"
        );
        assert!(text.contains("call publish_walkthrough again"), "{text}");
        assert!(
            text.contains("\n\nPublished walkthrough `circle-area-0a1b2c3d`"),
            "{text}"
        );

        let full = WalkthroughCoverage {
            total: 2,
            covered: 1,
            trivial: 1,
            ..Default::default()
        };
        let text = published(&walkthrough(Some(full)));
        assert!(text.starts_with("Published walkthrough"), "{text}");
        assert!(text.ends_with("Coverage: 2 of 2 changed hunks covered (1 by steps, 1 trivial): every changed hunk is explained."), "{text}");
        let text = published(&walkthrough(Some(WalkthroughCoverage::unavailable(
            "root 0 is not a git repository",
        ))));
        assert!(
            text.ends_with("Coverage was not checked: root 0 is not a git repository."),
            "{text}"
        );
    }
}
