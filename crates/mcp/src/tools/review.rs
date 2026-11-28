// SPDX-License-Identifier: Apache-2.0

//! `reply_to_review`: the agent answers a thread of the user's code review
//! (`review/reply`, issue #95).
//!
//! The user sends their pending review from Coqo's diff view in one prompt
//! that names each thread's id. The agent fixes what each asks, then calls
//! this once per thread: the reply lands in the thread next to the code,
//! and `resolve` folds it as done. A core tool because the threads live in
//! the daemon, and no harness has review threads (ADR 0014).

use coqo_protocol::methods::threads::{
    self as tm, AgentReplyParams, MAX_COMMENT, REPLY_TOOL, ThreadId, ThreadStatus,
};
use serde_json::{Map, Value, json};

use super::{opt_bool, opt_str, opt_u64};
use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

/// `reply_to_review`.
#[derive(Debug)]
pub struct ReplyToReviewTool;

impl Tool for ReplyToReviewTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            REPLY_TOOL,
            "Call it once per thread when the user sends you a code review from Coqo (a message \
             listing review threads with their ids): after you fix what a thread asks (or decide \
             not to), reply in that thread with what you did, in a sentence or two. Set `resolve: \
             true` when its request is done; leave it open when you need the user or disagree. \
             The user reads your reply in the thread, next to the code in Coqo's diff view, so do \
             not repeat the replies in chat.",
        )
        .with_title("Reply to a review thread")
        .with_schema(json!({
            "type": "object",
            "properties": {
                "thread_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The thread's id, from the review message (\"Thread 12\").",
                },
                "body": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_COMMENT,
                    "description": "Your reply, in markdown: what you changed, or why not.",
                },
                "resolve": {
                    "type": "boolean",
                    "description": "Resolve the thread: its request is done (default false).",
                },
            },
            "required": ["thread_id", "body"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let thread = opt_u64(&args, "thread_id")
                .map(ThreadId)
                .ok_or_else(|| ToolError::InvalidArguments("`thread_id` is required".into()))?;
            let body = opt_str(&args, "body")
                .ok_or_else(|| ToolError::InvalidArguments("`body` is required".into()))?
                .to_owned();
            let params = AgentReplyParams {
                workspace: cx.workspace().clone(),
                session: coqo_protocol::methods::agents::SessionId(cx.session().to_owned()),
                thread,
                body,
                resolve: opt_bool(&args, "resolve"),
            };
            let t = cx.daemon().request::<tm::AgentReply>(params).await?;
            let resolved = t.status == ThreadStatus::Resolved;
            let text = format!(
                "Replied in thread {}{}. The user sees it next to the code.",
                t.id,
                if resolved {
                    " and resolved it"
                } else {
                    "; it stays open"
                }
            );
            let mut output = ToolOutput::json(json!({
                "thread_id": t.id,
                "status": if resolved { "resolved" } else { "open" },
                "comments": t.comments.len(),
                "message": text,
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
    fn the_schema_checks_the_arguments() {
        let schema = ReplyToReviewTool.definition().input_schema;
        for bad in [
            json!({}),
            json!({ "thread_id": 3 }),
            json!({ "thread_id": 0, "body": "x" }),
            json!({ "thread_id": 3, "body": "" }),
            json!({ "thread_id": "3", "body": "x" }),
            json!({ "thread_id": 3, "body": "x", "extra": 1 }),
        ] {
            assert!(
                crate::schema::validate_arguments(&schema, &args(bad.clone())).is_err(),
                "{bad}"
            );
        }
        let ok = args(json!({ "thread_id": 3, "body": "Done.", "resolve": true }));
        assert!(crate::schema::validate_arguments(&schema, &ok).is_ok());
    }
}
