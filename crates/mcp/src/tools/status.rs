// SPDX-License-Identifier: Apache-2.0

//! `set_status`: the agent tells the user what it is doing
//! (`sessions/setStatus`).
//!
//! A core tool rather than a mod's, because the state it changes lives in
//! the daemon's session manager (`coqo-agents`), which is not a mod: the
//! bridge already knows its session, and `sessions/setStatus` lets a session
//! token reach only that session (ADR 0015).

use coqo_protocol::methods::agents::{
    self as agm, AgentState, MAX_STATUS_CHARS, SessionId, SetStatusParams, StatusProgress,
    StatusState,
};
use serde_json::{Map, Value, json};

use super::{opt_str, opt_u64};
use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

/// `set_status`.
#[derive(Debug)]
pub struct SetStatusTool;

fn state_name(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::NeedsInput => "needs_input",
        AgentState::NeedsPermission => "needs_permission",
        AgentState::Blocked => "blocked",
        AgentState::Error => "error",
        AgentState::Done => "done",
    }
}

fn claimed(args: &Map<String, Value>) -> Result<Option<StatusState>, ToolError> {
    Ok(match opt_str(args, "state") {
        None => None,
        Some("working") => Some(StatusState::Working),
        Some("blocked") => Some(StatusState::Blocked),
        Some("done") => Some(StatusState::Done),
        Some(other) => {
            return Err(ToolError::InvalidArguments(format!(
                "`state` must be working, blocked or done, not `{other}`"
            )));
        }
    })
}

fn progress(args: &Map<String, Value>) -> Option<StatusProgress> {
    let p = args.get("progress")?.as_object()?;
    Some(StatusProgress {
        current: opt_u64(p, "current")?,
        total: opt_u64(p, "total")?,
    })
}

impl Tool for SetStatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "set_status",
            "Tell the user what you are doing, in a few words. Coqo shows it next to this chat in \
             the chat list, the chat's session panel and the workspace rail, so the user can follow \
             along without reading the transcript. Call it when you start a new step of a longer \
             task (\"Running the test suite\"), with `progress` for countable work. Set `state` to \
             \"blocked\" when you cannot go on without the user (say why in `status`; the user is \
             alerted), \"working\" when you go on after that, and \"done\" when the task is \
             finished (`status` is then the outcome, kept after the turn). A status without a \
             state clears when the turn ends; every new prompt clears it. To ask the user a \
             question, use a question tool instead.",
        )
        .with_title("Set status")
        .with_schema(json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "minLength": 1,
                    "description": format!(
                        "One short line: what you are doing now, why you are blocked, or the \
                         outcome. Longer than {MAX_STATUS_CHARS} characters is cut."
                    ),
                },
                "state": {
                    "type": "string",
                    "enum": ["working", "blocked", "done"],
                    "description": "working: going on (clears blocked). blocked: you need the user. done: the task is finished.",
                },
                "progress": {
                    "type": "object",
                    "properties": {
                        "current": { "type": "integer", "minimum": 0, "description": "Steps done." },
                        "total": { "type": "integer", "minimum": 1, "description": "All steps." },
                    },
                    "required": ["current", "total"],
                    "additionalProperties": false,
                    "description": "Countable progress, e.g. tests run or files migrated.",
                },
            },
            "required": ["status"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let status = opt_str(&args, "status")
                .ok_or_else(|| ToolError::InvalidArguments("`status` is required".into()))?
                .to_owned();
            let params = SetStatusParams {
                session: SessionId(cx.session().to_owned()),
                status,
                state: claimed(&args)?,
                progress: progress(&args),
            };
            let info = cx.daemon().request::<agm::SetStatus>(params).await?;
            let state = state_name(info.state);
            let shown = info.status.as_ref();
            let text = match shown {
                Some(s) => match s.progress {
                    Some(p) => format!(
                        "Status shown: \"{}\" ({} of {}). Session state: {state}.",
                        s.text, p.current, p.total
                    ),
                    None => format!("Status shown: \"{}\". Session state: {state}.", s.text),
                },
                None => format!("Session state: {state}."),
            };
            let mut output = ToolOutput::json(json!({
                "state": state,
                "status": shown.map(|s| s.text.clone()),
                "progress": shown.and_then(|s| s.progress).map(|p| json!({
                    "current": p.current,
                    "total": p.total,
                })),
            }));
            output.text = text;
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(o) => o,
            _ => unreachable!(),
        }
    }

    #[test]
    fn arguments_map_to_the_request() {
        let a = args(
            json!({ "status": "x", "state": "blocked", "progress": { "current": 2, "total": 4 } }),
        );
        assert_eq!(claimed(&a).unwrap(), Some(StatusState::Blocked));
        assert_eq!(
            progress(&a),
            Some(StatusProgress {
                current: 2,
                total: 4
            })
        );
        let none = args(json!({ "status": "x" }));
        assert_eq!(claimed(&none).unwrap(), None);
        assert_eq!(progress(&none), None);
        assert!(claimed(&args(json!({ "state": "needs_input" }))).is_err());
        // The schema checks the arguments first.
        let schema = SetStatusTool.definition().input_schema;
        for bad in [
            json!({}),
            json!({ "status": "" }),
            json!({ "status": "x", "state": "needs_input" }),
            json!({ "status": "x", "progress": { "current": 1, "total": 0 } }),
            json!({ "status": "x", "progress": { "current": 1 } }),
            json!({ "status": "x", "extra": true }),
        ] {
            assert!(
                crate::schema::validate_arguments(&schema, &args(bad.clone())).is_err(),
                "{bad}"
            );
        }
        assert!(crate::schema::validate_arguments(&schema, &a).is_ok());
    }
}
