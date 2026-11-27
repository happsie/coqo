// SPDX-License-Identifier: Apache-2.0

//! `ask_question`, `await_answer` and `annotate`: questions and notes pinned
//! to code, answered by the user in Coqo (`questions/*`, PLAN §7.5).
//!
//! Waiting follows `questions/watch` (the question now, then its changes),
//! sending MCP progress every [`PROGRESS_EVERY`] so the call does not time
//! out. A cancelled call drops the subscription, which ends the wait in the
//! daemon; the question stays open for a later `await_answer`.

use std::time::Duration;

use coqo_protocol::methods::editor::LineRange;
use coqo_protocol::methods::questions::{
    self as qm, CreateParams, NewAnchor, NewOption, Question, QuestionId, QuestionKind,
    QuestionStatus, Severity, WatchParams,
};
use futures_util::StreamExt as _;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use super::{absolute, opt_str, opt_u64, path_properties, target};
use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

/// How long a call waits for an answer unless it says otherwise.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(600);
/// The longest wait a call may ask for.
pub const MAX_WAIT_SECS: u64 = 3600;
/// How often a waiting call reports progress.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(10);

/// The location properties `ask_question` and `annotate` take.
fn location_properties(what: &str) -> Map<String, Value> {
    let mut props = path_properties(&format!("File the {what} is about"));
    let line = |d: &str| json!({ "type": "integer", "minimum": 1, "description": d });
    props.insert(
        "line".into(),
        line(&format!(
            "The 1-based line the {what} is about. Or give `start_line` and `end_line`."
        )),
    );
    props.insert(
        "start_line".into(),
        line("First 1-based line of the range."),
    );
    props.insert(
        "end_line".into(),
        line("Last 1-based line of the range (inclusive)."),
    );
    props
}

/// The whole lines a call names, 0-based.
fn lines(args: &Map<String, Value>) -> Result<LineRange, ToolError> {
    let zero = |n: u64| u32::try_from(n.saturating_sub(1)).unwrap_or(u32::MAX);
    match (
        opt_u64(args, "line"),
        opt_u64(args, "start_line"),
        opt_u64(args, "end_line"),
    ) {
        (Some(line), None, None) => Ok(LineRange {
            start: zero(line),
            end: zero(line),
        }),
        (None, Some(start), end) => {
            let end = end.unwrap_or(start);
            if end < start {
                return Err(ToolError::InvalidArguments(
                    "`end_line` is before `start_line`".into(),
                ));
            }
            Ok(LineRange {
                start: zero(start),
                end: zero(end),
            })
        }
        (None, None, Some(_)) => Err(ToolError::InvalidArguments(
            "`end_line` needs a `start_line`".into(),
        )),
        (Some(_), ..) => Err(ToolError::InvalidArguments(
            "give either `line` or `start_line` and `end_line`, not both".into(),
        )),
        (None, None, None) => Err(ToolError::InvalidArguments(
            "say which code: `line`, or `start_line` and `end_line`".into(),
        )),
    }
}

/// `path:12` or `path:12-14`, 1-based.
fn place(path: &str, lines: LineRange) -> String {
    if lines.start == lines.end {
        format!("{path}:{}", lines.start + 1)
    } else {
        format!("{path}:{}-{}", lines.start + 1, lines.end + 1)
    }
}

/// The wait a call asks for.
fn timeout(args: &Map<String, Value>) -> Duration {
    opt_u64(args, "timeout_s").map_or(DEFAULT_WAIT, |s| {
        Duration::from_secs(s.clamp(1, MAX_WAIT_SECS))
    })
}

fn timeout_property() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_WAIT_SECS,
        "description": format!(
            "How long to wait for the answer, in seconds (default {}). The question stays \
             open after that; call await_answer to wait again.",
            DEFAULT_WAIT.as_secs()
        ),
    })
}

/// Follow question `id` until it is answered or dismissed, or `wait` is
/// over: then it is returned as it is (still open). Reports progress while
/// it waits.
pub(crate) async fn wait(
    cx: &ToolContext,
    id: QuestionId,
    wait: Duration,
) -> Result<Question, ToolError> {
    let mut watch = cx
        .daemon()
        .subscribe::<qm::Watch>(
            WatchParams {
                workspace: cx.workspace().clone(),
                session: cx.session().to_owned(),
                id,
            },
            None,
        )
        .await?;
    let started = Instant::now();
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    let mut ticks = tokio::time::interval_at(started + PROGRESS_EVERY, PROGRESS_EVERY);
    let mut latest = None;
    loop {
        tokio::select! {
            item = watch.next() => match item {
                Some(Ok(s)) => {
                    if s.event.status != QuestionStatus::Open {
                        return Ok(s.event);
                    }
                    latest = Some(s.event);
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Err(ToolError::Failed("the daemon ended the wait".into())),
            },
            _ = ticks.tick() => {
                cx.progress(
                    started.elapsed().as_secs_f64().floor(),
                    Some(wait.as_secs_f64()),
                    Some(format!("Waiting for the user to answer question {id} in Coqo")),
                )
                .await;
            }
            () = &mut deadline => {
                return latest.ok_or_else(|| ToolError::Failed("no answer and no question".into()));
            }
        }
    }
}

/// What the agent reads about a question that is answered, dismissed, or
/// still open after a wait.
fn outcome(q: &Question, waited: Option<Duration>) -> ToolOutput {
    let label = |id: &str| {
        q.options
            .iter()
            .find(|o| o.id == id)
            .map_or(id, |o| o.label.as_str())
            .to_owned()
    };
    let (text, answer) = match q.status {
        QuestionStatus::Answered => {
            let answer = q.answer.clone().unwrap_or_default();
            let mut lines = vec![format!("The user answered question {}.", q.id)];
            if let Some(option) = &answer.option {
                lines.push(format!("Choice: {}", label(option)));
            }
            if let Some(text) = &answer.text {
                lines.push(format!("Reply:\n{text}"));
            }
            let json = json!({
                "option": answer.option,
                "label": answer.option.as_deref().map(label),
                "text": answer.text,
            });
            (lines.join("\n"), json)
        }
        QuestionStatus::Dismissed => (
            format!(
                "The user dismissed question {} without answering. Decide yourself, or ask \
                 differently.",
                q.id
            ),
            Value::Null,
        ),
        QuestionStatus::Open => {
            let after = waited.map_or(String::new(), |w| format!(" after {} s", w.as_secs()));
            (
                format!(
                    "No answer yet{after}. Question {id} stays open in Coqo and the user can \
                     still answer it; call await_answer with id {id} to wait again, or carry \
                     on and check later.",
                    id = q.id
                ),
                Value::Null,
            )
        }
    };
    reply(
        json!({
            "id": q.id,
            "status": q.status,
            "kind": q.kind,
            "answer": answer,
        }),
        text,
    )
}

/// A JSON result that also carries `text` as its `message`: clients that
/// show the model only the structured content (Claude Code does) still
/// pass on what to do next.
fn reply(mut value: Value, text: String) -> ToolOutput {
    if let Value::Object(o) = &mut value {
        o.insert("message".into(), json!(text));
    }
    let mut output = ToolOutput::json(value);
    output.text = text;
    output
}

/// `ask_question`.
#[derive(Debug)]
pub struct AskQuestionTool;

impl Tool for AskQuestionTool {
    fn definition(&self) -> ToolDefinition {
        let mut props = location_properties("question");
        props.insert(
            "question".into(),
            json!({
                "type": "string",
                "minLength": 1,
                "maxLength": qm::MAX_BODY,
                "description": "The question, in markdown. The user sees it next to the code.",
            }),
        );
        props.insert(
            "kind".into(),
            json!({
                "type": "string",
                "enum": ["decision", "clarification", "confirmation", "info"],
                "description": "decision: pick one of `options`; clarification: a free-text \
                                reply; confirmation: yes or no; info: something the user \
                                should acknowledge. Default: decision with options, else \
                                clarification.",
            }),
        );
        props.insert(
            "options".into(),
            json!({
                "type": "array",
                "items": { "type": "string", "minLength": 1 },
                "minItems": 2,
                "maxItems": qm::MAX_OPTIONS,
                "description": "A decision's choices, as short button labels. The user can \
                                also reply in free text.",
            }),
        );
        props.insert(
            "wait".into(),
            json!({
                "type": "boolean",
                "description": "Wait for the answer (default true). With false the call \
                                returns the question's id at once; get the answer later with \
                                await_answer.",
            }),
        );
        props.insert("timeout_s".into(), timeout_property());
        ToolDefinition::new(
            "ask_question",
            "Use it instead of asking in chat, and instead of deciding yourself, when a choice \
             about code is the user's: a product or business value, a trade-off they want to \
             make, anything they are unsure about. The question is pinned to lines of code: it \
             appears in the Coqo editor at those lines, and the user answers there with the code \
             in view. Also for any other question about specific code (which approach to take \
             here, whether this behaviour is intended, whether you may change this API). Give \
             `options` for a decision. It blocks until the user answers (with progress) \
             and returns the answer, unless `wait: false`: then it returns an id at once, so \
             you can go on and collect the answer later with await_answer. Lines are 1-based.",
        )
        .with_title("Ask a question about code")
        .with_schema(json!({
            "type": "object",
            "properties": props,
            "required": ["path", "question"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let lines = lines(&args)?;
            let body = opt_str(&args, "question").unwrap_or_default().to_owned();
            let options: Vec<NewOption> = args
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|label| NewOption {
                            label: label.to_owned(),
                            description: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let kind = match opt_str(&args, "kind") {
                Some("decision") => QuestionKind::Decision,
                Some("confirmation") => QuestionKind::Confirmation,
                Some("info") => QuestionKind::Info,
                Some("clarification") => QuestionKind::Clarification,
                _ if !options.is_empty() => QuestionKind::Decision,
                _ => QuestionKind::Clarification,
            };
            let wait_for = args.get("wait").and_then(Value::as_bool).unwrap_or(true);
            let target = target(cx, &args, None).await?;
            let question = cx
                .daemon()
                .request::<qm::Create>(CreateParams {
                    workspace: cx.workspace().clone(),
                    session: cx.session().to_owned(),
                    kind,
                    body,
                    options,
                    severity: None,
                    anchors: vec![NewAnchor {
                        root: target.root,
                        path: target.path.clone(),
                        lines,
                    }],
                })
                .await?;
            let at = place(&target.absolute(), lines);
            if !wait_for {
                let text = format!(
                    "Asked question {id} at {at}. The user sees it in Coqo; call await_answer \
                     with id {id} for the answer.",
                    id = question.id
                );
                return Ok(reply(
                    json!({
                        "id": question.id,
                        "status": question.status,
                        "kind": question.kind,
                    }),
                    text,
                ));
            }
            let wait_for = timeout(&args);
            let now = wait(cx, question.id, wait_for).await?;
            Ok(outcome(&now, Some(wait_for)))
        })
    }
}

/// `await_answer`.
#[derive(Debug)]
pub struct AwaitAnswerTool;

impl Tool for AwaitAnswerTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "await_answer",
            "Wait for the user's answer to a question asked with ask_question (by its id), \
             with progress, and return it. Use it after ask_question with `wait: false` once \
             you need the answer, or to wait again after a question timed out. Returns at once \
             if it is already answered or dismissed; if the wait runs out, the question stays \
             open.",
        )
        .with_title("Wait for an answer")
        .read_only()
        .with_schema(json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The question's id, from ask_question.",
                },
                "timeout_s": timeout_property(),
            },
            "required": ["id"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let id = QuestionId(opt_u64(&args, "id").unwrap_or_default());
            let wait_for = timeout(&args);
            let now = wait(cx, id, wait_for).await?;
            Ok(outcome(&now, Some(wait_for)))
        })
    }
}

/// `annotate`.
#[derive(Debug)]
pub struct AnnotateTool;

impl Tool for AnnotateTool {
    fn definition(&self) -> ToolDefinition {
        let mut props = location_properties("note");
        props.insert(
            "note".into(),
            json!({
                "type": "string",
                "minLength": 1,
                "maxLength": qm::MAX_BODY,
                "description": "The note, in markdown.",
            }),
        );
        props.insert(
            "severity".into(),
            json!({
                "type": "string",
                "enum": ["info", "warning"],
                "description": "info (default) or warning.",
            }),
        );
        ToolDefinition::new(
            "annotate",
            "Pin a note to lines of code for the user to see in the Coqo editor, without \
             waiting for a reply: an observation, a caveat, a risk or a follow-up at those \
             lines (`severity: warning` for one they should not miss). Use it instead of a \
             remark in chat when the remark is about particular lines, e.g. a known limitation \
             of a change you just made. It does not edit the file. Lines are 1-based.",
        )
        .with_title("Annotate code")
        .with_schema(json!({
            "type": "object",
            "properties": props,
            "required": ["path", "note"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let lines = lines(&args)?;
            let severity = match opt_str(&args, "severity") {
                Some("warning") => Severity::Warning,
                _ => Severity::Info,
            };
            let target = target(cx, &args, None).await?;
            let note = cx
                .daemon()
                .request::<qm::Create>(CreateParams {
                    workspace: cx.workspace().clone(),
                    session: cx.session().to_owned(),
                    kind: QuestionKind::Annotation,
                    body: opt_str(&args, "note").unwrap_or_default().to_owned(),
                    options: Vec::new(),
                    severity: Some(severity),
                    anchors: vec![NewAnchor {
                        root: target.root,
                        path: target.path.clone(),
                        lines,
                    }],
                })
                .await?;
            let at = place(&absolute(&target.ws, target.root, &target.path), lines);
            Ok(reply(
                json!({ "id": note.id, "severity": severity }),
                format!("Added note {} at {at}.", note.id),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn lines_are_one_based_and_inclusive() {
        let range = |start, end| LineRange { start, end };
        assert_eq!(lines(&args(json!({ "line": 3 }))).unwrap(), range(2, 2));
        assert_eq!(
            lines(&args(json!({ "start_line": 3, "end_line": 5 }))).unwrap(),
            range(2, 4)
        );
        assert_eq!(
            lines(&args(json!({ "start_line": 3 }))).unwrap(),
            range(2, 2)
        );
        for bad in [
            json!({}),
            json!({ "end_line": 3 }),
            json!({ "line": 1, "start_line": 1 }),
            json!({ "start_line": 5, "end_line": 2 }),
        ] {
            assert!(lines(&args(bad.clone())).is_err(), "{bad}");
        }
        assert_eq!(place("/a.rs", range(2, 2)), "/a.rs:3");
        assert_eq!(place("/a.rs", range(2, 4)), "/a.rs:3-5");
    }

    #[test]
    fn waits_are_bounded() {
        assert_eq!(timeout(&args(json!({}))), DEFAULT_WAIT);
        assert_eq!(
            timeout(&args(json!({ "timeout_s": 999_999 }))),
            Duration::from_secs(MAX_WAIT_SECS)
        );
        assert_eq!(
            timeout(&args(json!({ "timeout_s": 5 }))),
            Duration::from_secs(5)
        );
    }
}
