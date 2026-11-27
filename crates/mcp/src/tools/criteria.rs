// SPDX-License-Identifier: Apache-2.0

//! `set_acceptance_criteria`: the agent states what "done" means for its
//! task and records, criterion by criterion, whether it holds
//! (`sessions/setCriteria`).
//!
//! One tool for both: a call with new criteria registers them, a call that
//! names an id updates that criterion (status, evidence, text). A core tool,
//! like `set_status`, because the criteria live on the session in the
//! daemon's session manager, and `sessions/setCriteria` lets a session token
//! reach only its own session (ADR 0017).

use std::fmt::Write as _;

use coqo_protocol::methods::agents::{
    self as agm, AcceptanceCriterion, CriterionAuthor, CriterionInput, CriterionStatus,
    MAX_CRITERIA, MAX_CRITERION_CHARS, MAX_EVIDENCE, SessionId, SetCriteriaParams,
};
use serde_json::{Map, Value, json};

use super::opt_bool;
use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

/// `set_acceptance_criteria`.
#[derive(Debug)]
pub struct SetAcceptanceCriteriaTool;

fn status_name(status: CriterionStatus) -> &'static str {
    match status {
        CriterionStatus::Pending => "pending",
        CriterionStatus::Met => "met",
        CriterionStatus::Failed => "failed",
        CriterionStatus::Unknown => "unknown",
    }
}

fn entries(args: &Map<String, Value>) -> Result<Vec<CriterionInput>, ToolError> {
    let list = args.get("criteria").cloned().unwrap_or(Value::Null);
    serde_json::from_value(list)
        .map_err(|e| ToolError::InvalidArguments(format!("`criteria`: {e}")))
}

/// The list as the agent reads it back: one line per criterion, with who
/// changed it when that was the user.
fn summary(criteria: &[AcceptanceCriterion]) -> String {
    if criteria.is_empty() {
        return "No acceptance criteria.".into();
    }
    let met = criteria
        .iter()
        .filter(|c| c.status == CriterionStatus::Met)
        .count();
    let mut text = format!(
        "Acceptance criteria shown to the user ({met} of {} met):",
        criteria.len()
    );
    for c in criteria {
        let by = match c.changed_by {
            CriterionAuthor::User => ", set by the user",
            CriterionAuthor::Agent => "",
        };
        let _ = write!(
            text,
            "\n- {} [{}{by}] {}",
            c.id,
            status_name(c.status),
            c.text
        );
        for e in &c.evidence {
            let _ = match (&e.path, e.line) {
                (Some(p), Some(l)) => write!(text, "\n    evidence: {} ({p}:{l})", e.text),
                (Some(p), None) => write!(text, "\n    evidence: {} ({p})", e.text),
                _ => write!(text, "\n    evidence: {}", e.text),
            };
        }
    }
    text
}

fn structured(criteria: &[AcceptanceCriterion]) -> Value {
    json!({
        "criteria": criteria.iter().map(|c| json!({
            "id": c.id,
            "text": c.text,
            "status": status_name(c.status),
            "changedBy": match c.changed_by {
                CriterionAuthor::Agent => "agent",
                CriterionAuthor::User => "user",
            },
            "evidence": c.evidence.iter().map(|e| json!({
                "text": e.text,
                "path": e.path,
                "line": e.line,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

impl Tool for SetAcceptanceCriteriaTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "set_acceptance_criteria",
            "Record what \"done\" means for your task as acceptance criteria, and whether each \
             one holds. Coqo shows them in the chat's session panel, and walkthroughs and live \
             traces refer to them by id. Call it at the start of a task (from the ticket or the \
             prompt; `replace: true` sets the whole list), then again as you verify each one: \
             name its `id` with `status` \"met\" or \"failed\" and `evidence` (the test that \
             passed, the file and line that shows it). Use \"unknown\" for one you cannot check \
             here. New criteria get ids AC1, AC2, … unless you give one. The result lists every \
             criterion; the user can mark one met or failed themselves, shown as \"set by the \
             user\".",
        )
        .with_title("Set acceptance criteria")
        .with_schema(json!({
            "type": "object",
            "properties": {
                "criteria": {
                    "type": "array",
                    "maxItems": MAX_CRITERIA,
                    "description": "New criteria (with `text`), or changes to existing ones (with their `id`).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": agm::MAX_CRITERION_ID,
                                "description": "The criterion to update, or the id for a new one (letters, digits, `.`, `_`, `-`). Default for new: AC<n>.",
                            },
                            "text": {
                                "type": "string",
                                "minLength": 1,
                                "description": format!(
                                    "What must hold, one sentence (at most {MAX_CRITERION_CHARS} characters). Required for a new criterion."
                                ),
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "met", "failed", "unknown"],
                                "description": "pending: not checked yet (default). met: verified. failed: checked, does not hold. unknown: cannot be checked here.",
                            },
                            "evidence": {
                                "type": "array",
                                "maxItems": MAX_EVIDENCE,
                                "description": "Why it has its status; replaces earlier evidence ([] clears it).",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "text": { "type": "string", "minLength": 1, "description": "E.g. \"test volume_discount_caps passes\"." },
                                        "path": { "type": "string", "description": "A file that shows it, relative to the workspace root or absolute." },
                                        "line": { "type": "integer", "minimum": 1, "description": "1-based line in `path`." },
                                    },
                                    "required": ["text"],
                                    "additionalProperties": false,
                                },
                            },
                        },
                        "additionalProperties": false,
                    },
                },
                "replace": {
                    "type": "boolean",
                    "description": "The list becomes exactly `criteria`, in order (others are dropped). Default: false, which updates named criteria and adds new ones at the end.",
                },
            },
            "required": ["criteria"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let params = SetCriteriaParams {
                session: SessionId(cx.session().to_owned()),
                criteria: entries(&args)?,
                replace: opt_bool(&args, "replace"),
            };
            let info = cx.daemon().request::<agm::SetCriteria>(params).await?;
            let mut output = ToolOutput::json(structured(&info.criteria));
            output.text = summary(&info.criteria);
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::agents::CriterionEvidence;

    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(o) => o,
            _ => unreachable!(),
        }
    }

    #[test]
    fn arguments_map_to_the_request() {
        let a = args(json!({
            "criteria": [
                { "text": "100+ units get 10% off" },
                { "id": "AC2", "status": "met", "evidence": [
                    { "text": "test caps passes", "path": "src/lib.rs", "line": 3 },
                ] },
            ],
            "replace": true,
        }));
        let list = entries(&a).unwrap();
        assert_eq!(list[0].text.as_deref(), Some("100+ units get 10% off"));
        assert_eq!(list[1].status, Some(CriterionStatus::Met));
        assert_eq!(
            list[1].evidence.as_deref(),
            Some(
                &[CriterionEvidence {
                    text: "test caps passes".into(),
                    path: Some("src/lib.rs".into()),
                    line: Some(3),
                }][..]
            )
        );
        let schema = SetAcceptanceCriteriaTool.definition().input_schema;
        assert!(crate::schema::validate_arguments(&schema, &a).is_ok());
        for bad in [
            json!({}),
            json!({ "criteria": [{ "status": "done" }] }),
            json!({ "criteria": [{ "text": "" }] }),
            json!({ "criteria": [{ "text": "x", "extra": 1 }] }),
            json!({ "criteria": [{ "id": "AC1", "evidence": [{ "path": "a.rs" }] }] }),
            json!({ "criteria": [{ "id": "AC1", "evidence": [{ "text": "x", "line": 0 }] }] }),
            json!({ "criteria": [], "replace": "yes" }),
        ] {
            assert!(
                crate::schema::validate_arguments(&schema, &args(bad.clone())).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_summary_says_who_set_what() {
        let c = |id: &str, status, by| AcceptanceCriterion {
            id: id.into(),
            text: format!("criterion {id}"),
            status,
            evidence: Vec::new(),
            changed_by: by,
            updated_at: 0,
        };
        let mut list = vec![
            c("AC1", CriterionStatus::Met, CriterionAuthor::Agent),
            c("AC2", CriterionStatus::Failed, CriterionAuthor::User),
        ];
        list[0].evidence.push(CriterionEvidence {
            text: "test a passes".into(),
            path: Some("/w/a.rs".into()),
            line: Some(4),
        });
        let text = summary(&list);
        assert!(text.contains("(1 of 2 met)"), "{text}");
        assert!(text.contains("- AC1 [met] criterion AC1"), "{text}");
        assert!(
            text.contains("evidence: test a passes (/w/a.rs:4)"),
            "{text}"
        );
        assert!(text.contains("- AC2 [failed, set by the user]"), "{text}");
        assert_eq!(summary(&[]), "No acceptance criteria.");
        assert_eq!(structured(&list)["criteria"][1]["changedBy"], "user");
    }
}
