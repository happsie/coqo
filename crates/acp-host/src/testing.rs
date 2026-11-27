// SPDX-License-Identifier: Apache-2.0

//! A scripted ACP agent for tests: it speaks ACP v1 on stdio through the
//! SDK's agent side, needs no network and runs no model. The binary
//! `coqo-fake-acp-agent` runs it; provider crates can link a two-line bin
//! of their own that calls [`run_fake_agent`].
//!
//! # Behaviour
//!
//! - `initialize` advertises `loadSession`, `session/resume` and
//!   `session/close`, then reports an auth status twice (the RFD
//!   `auth/status_update` and the legacy `_auth/status_update`), both
//!   carrying an email and an organization that the host must drop.
//! - `session/new` answers with three modes (`default`, `acceptEdits`,
//!   `plan`), then sends `available_commands_update`.
//! - `session/load` replays one user and one agent chunk before answering;
//!   `session/resume` accepts any id without replay.
//! - Each line of a prompt's text is a command:
//!   - `say <text>`: agent message chunks, one per word;
//!   - `think <text>`: a thought chunk;
//!   - `plan`: a two-entry plan;
//!   - `usage`: a usage update with a cost;
//!   - `mode <id>`: a current-mode update;
//!   - `edit <path> <text>`: a `tool_call` with a diff, the path's
//!     location and `rawInput.file_path`, then a permission request carrying
//!     the whole call again (options `reject`, `allow-always`, `allow-once`,
//!     in that order), as the Claude adapter sends its `Write`; if allowed it
//!     writes the file itself (like Claude Code) and completes the call;
//!   - `run <command>`: the same for a shell command (`kind: execute`,
//!     `rawInput.command`, like the adapter's `Bash`), without running it;
//!   - `write <path> <text>`: `fs/write_text_file` through the client;
//!   - `read <path>`: `fs/read_text_file`, then says what it read;
//!   - `wait`: waits for `session/cancel` and ends the turn as cancelled;
//!   - `crash`: exits with status 3.
//!
//! # Environment
//!
//! - `FAKE_ACP_SIGNED_OUT=1`: `session/new`, `session/resume` and
//!   `session/load` fail with `auth_required`.
//! - `FAKE_ACP_NO_RESUME=1`: no `session/resume` (only `session/load`).
//! - `FAKE_ACP_LOG=<file>`: appends one JSON line per event: `start` (argv
//!   pid, argv and the `CLAUDE*`/`FAKE_ACP*` variables), then
//!   each request with its params.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{ByteStreams, Client};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v == "1")
}

/// Appends events to `FAKE_ACP_LOG`.
#[derive(Clone, Default)]
struct Log(Option<Arc<Mutex<std::fs::File>>>);

impl Log {
    fn open() -> Self {
        let Some(path) = std::env::var_os("FAKE_ACP_LOG") else {
            return Self(None);
        };
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        Self(file.map(|f| Arc::new(Mutex::new(f))))
    }

    fn write(&self, event: &str, data: Value) {
        if let Some(file) = &self.0 {
            let line = json!({"event": event, "data": data}).to_string();
            let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = writeln!(file, "{line}");
        }
    }
}

#[derive(Default)]
struct State {
    next: AtomicU64,
    cancels: Mutex<HashMap<String, Arc<Notify>>>,
}

impl State {
    fn cancel_signal(&self, id: &str) -> Arc<Notify> {
        let mut cancels = self.cancels.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(cancels.entry(id.to_owned()).or_default())
    }
}

/// Sends `fields` as a `tool_call`, then asks permission with the whole call
/// again, as the Claude adapter does for a v1 client. `Some(allowed)`, or
/// `None` when the turn was cancelled.
async fn ask(
    cx: &ConnectionTo<Client>,
    id: &acp::SessionId,
    call_id: &acp::ToolCallId,
    fields: acp::ToolCallUpdateFields,
) -> Result<Option<bool>, agent_client_protocol::Error> {
    let mut call = acp::ToolCall::new(call_id.clone(), fields.title.clone().unwrap_or_default());
    call.kind = fields.kind.unwrap_or_default();
    call.status = fields.status.unwrap_or_default();
    call.content = fields.content.clone().unwrap_or_default();
    call.locations = fields.locations.clone().unwrap_or_default();
    call.raw_input = fields.raw_input.clone();
    notify(cx, id, acp::SessionUpdate::ToolCall(call));
    let option = |id: &str, name: &str, kind| {
        acp::PermissionOption::new(acp::PermissionOptionId::new(id), name, kind)
    };
    let answer = cx
        .send_request(acp::RequestPermissionRequest::new(
            id.clone(),
            acp::ToolCallUpdate::new(call_id.clone(), fields),
            vec![
                option("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
                option(
                    "allow-always",
                    "Always allow",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                option("allow-once", "Allow", acp::PermissionOptionKind::AllowOnce),
            ],
        ))
        .block_task()
        .await?;
    Ok(match answer.outcome {
        acp::RequestPermissionOutcome::Selected(s) => Some(s.option_id.0.starts_with("allow")),
        _ => None,
    })
}

fn text(t: impl Into<String>) -> acp::ContentChunk {
    acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(t)))
}

fn notify(cx: &ConnectionTo<Client>, id: &acp::SessionId, update: acp::SessionUpdate) {
    let _ = cx.send_notification(acp::SessionNotification::new(id.clone(), update));
}

fn modes() -> acp::SessionModeState {
    let mode = |id: &str, name: &str| acp::SessionMode::new(acp::SessionModeId::new(id), name);
    acp::SessionModeState::new(
        acp::SessionModeId::new("default"),
        vec![
            mode("default", "Manual"),
            mode("acceptEdits", "Accept edits"),
            mode("plan", "Plan"),
        ],
    )
}

fn signed_out() -> agent_client_protocol::Error {
    agent_client_protocol::Error::auth_required()
        .data(json!({"reason": "signed_out", "message": "run claude /login"}))
}

/// Runs one prompt; returns why it ended.
async fn turn(
    cx: ConnectionTo<Client>,
    state: Arc<State>,
    req: acp::PromptRequest,
) -> Result<acp::StopReason, agent_client_protocol::Error> {
    let id = req.session_id.clone();
    let script: String = req
        .prompt
        .iter()
        .filter_map(|b| match b {
            acp::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for (n, line) in script.lines().enumerate() {
        let (cmd, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        match cmd {
            "say" => {
                for word in rest.split_inclusive(' ') {
                    notify(&cx, &id, acp::SessionUpdate::AgentMessageChunk(text(word)));
                }
            }
            "think" => notify(&cx, &id, acp::SessionUpdate::AgentThoughtChunk(text(rest))),
            "plan" => notify(
                &cx,
                &id,
                acp::SessionUpdate::Plan(acp::Plan::new(vec![
                    acp::PlanEntry::new(
                        "read the code",
                        acp::PlanEntryPriority::High,
                        acp::PlanEntryStatus::Completed,
                    ),
                    acp::PlanEntry::new(
                        "write the fix",
                        acp::PlanEntryPriority::Medium,
                        acp::PlanEntryStatus::InProgress,
                    ),
                ])),
            ),
            "usage" => notify(
                &cx,
                &id,
                acp::SessionUpdate::UsageUpdate(
                    acp::UsageUpdate::new(1200, 200_000)
                        .cost(acp::Cost::new(0.25, "USD"))
                        .meta(
                            json!({"_claude/model": "fake-model", "email": "me@example.com"})
                                .as_object()
                                .cloned(),
                        ),
                ),
            ),
            "mode" => notify(
                &cx,
                &id,
                acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
                    acp::SessionModeId::new(rest),
                )),
            ),
            "edit" => {
                // Shaped like the Claude adapter's `Write`: the tool call,
                // then a permission request carrying the whole call again.
                let (path, new_text) = rest.split_once(' ').unwrap_or((rest, ""));
                let path = PathBuf::from(path);
                let old_text = std::fs::read_to_string(&path).ok();
                let call_id = acp::ToolCallId::new(format!("edit-{n}"));
                let fields = acp::ToolCallUpdateFields::new()
                    .title(format!("Edit {}", path.display()))
                    .kind(acp::ToolKind::Edit)
                    .status(acp::ToolCallStatus::Pending)
                    .raw_input(json!({"file_path": path, "content": new_text}))
                    .content(vec![acp::ToolCallContent::Diff(
                        acp::Diff::new(path.clone(), new_text).old_text(old_text),
                    )])
                    .locations(vec![acp::ToolCallLocation::new(path.clone())]);
                let allowed = match ask(&cx, &id, &call_id, fields).await? {
                    Some(allowed) => allowed,
                    None => return Ok(acp::StopReason::Cancelled),
                };
                let status = if allowed {
                    std::fs::write(&path, new_text)
                        .map_err(agent_client_protocol::Error::into_internal_error)?;
                    acp::ToolCallStatus::Completed
                } else {
                    acp::ToolCallStatus::Failed
                };
                notify(
                    &cx,
                    &id,
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        call_id,
                        acp::ToolCallUpdateFields::new().status(status),
                    )),
                );
            }
            "run" => {
                // Shaped like the Claude adapter's `Bash`: `rawInput.command`.
                let call_id = acp::ToolCallId::new(format!("run-{n}"));
                let fields = acp::ToolCallUpdateFields::new()
                    .title(rest)
                    .kind(acp::ToolKind::Execute)
                    .status(acp::ToolCallStatus::Pending)
                    .raw_input(json!({"command": rest, "description": "Run it"}))
                    .content(vec![]);
                let status = match ask(&cx, &id, &call_id, fields).await? {
                    Some(true) => acp::ToolCallStatus::Completed,
                    Some(false) => acp::ToolCallStatus::Failed,
                    None => return Ok(acp::StopReason::Cancelled),
                };
                notify(
                    &cx,
                    &id,
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        call_id,
                        acp::ToolCallUpdateFields::new().status(status),
                    )),
                );
            }
            "write" => {
                let (path, content) = rest.split_once(' ').unwrap_or((rest, ""));
                cx.send_request(acp::WriteTextFileRequest::new(id.clone(), path, content))
                    .block_task()
                    .await?;
                notify(
                    &cx,
                    &id,
                    acp::SessionUpdate::AgentMessageChunk(text("wrote")),
                );
            }
            "read" => {
                let read = cx
                    .send_request(acp::ReadTextFileRequest::new(id.clone(), rest))
                    .block_task()
                    .await;
                let said = match read {
                    Ok(r) => r.content,
                    Err(e) => format!("error: {}", e.message),
                };
                notify(&cx, &id, acp::SessionUpdate::AgentMessageChunk(text(said)));
            }
            "wait" => {
                state.cancel_signal(&id.0).notified().await;
                return Ok(acp::StopReason::Cancelled);
            }
            "crash" => std::process::exit(3),
            "" => {}
            other => notify(
                &cx,
                &id,
                acp::SessionUpdate::AgentMessageChunk(text(format!("unknown command {other}"))),
            ),
        }
    }
    Ok(acp::StopReason::EndTurn)
}

/// Runs the fake agent on stdin/stdout until stdin closes.
pub async fn run_fake_agent() -> Result<(), agent_client_protocol::Error> {
    let log = Log::open();
    let env: serde_json::Map<String, Value> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE") || k.starts_with("FAKE_ACP"))
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    log.write(
        "start",
        json!({
            "pid": std::process::id(),
            "args": std::env::args().skip(1).collect::<Vec<_>>(),
            "env": env,
        }),
    );
    let state = Arc::new(State::default());
    let transport = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );
    let (l1, l2, l3, l4, l5, l6) = (
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
        log.clone(),
    );
    let (s1, s2, s3) = (Arc::clone(&state), Arc::clone(&state), Arc::clone(&state));
    agent_client_protocol::Agent
        .v1()
        .name("coqo-fake-acp-agent")
        .on_receive_request(
            async move |req: acp::InitializeRequest, responder, cx| {
                l1.write("initialize", serde_json::to_value(&req).unwrap_or_default());
                let mut sessions =
                    acp::SessionCapabilities::new().close(acp::SessionCloseCapabilities::new());
                if !flag("FAKE_ACP_NO_RESUME") {
                    sessions = sessions.resume(acp::SessionResumeCapabilities::new());
                }
                responder.respond(
                    acp::InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(
                            acp::AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(sessions),
                        )
                        .agent_info(acp::Implementation::new("coqo-fake-acp-agent", "0.0.0")),
                )?;
                let account = acp::AuthAccount::new("Claude Max").meta(
                    json!({"claudeCode": {"kind": "account", "plan": "max",
                        "email": "me@example.com", "organization": "Acme"}})
                    .as_object()
                    .cloned(),
                );
                cx.send_notification(
                    acp::AuthStatusUpdateNotification::new(acp::AuthState::Authenticated)
                        .accounts(vec![account]),
                )?;
                cx.send_notification(agent_client_protocol::UntypedMessage::new(
                    "_auth/status_update",
                    json!({"authStatus": {"kind": "account", "label": "Claude Max",
                        "account": {"plan": "max", "email": "me@example.com"}}}),
                )?)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::NewSessionRequest, responder, cx| {
                l2.write(
                    "session/new",
                    serde_json::to_value(&req).unwrap_or_default(),
                );
                if flag("FAKE_ACP_SIGNED_OUT") {
                    return responder.respond_with_error(signed_out());
                }
                let n = s1.next.fetch_add(1, Ordering::Relaxed) + 1;
                let id = acp::SessionId::new(format!("fake-{}-{n}", std::process::id()));
                responder.respond(acp::NewSessionResponse::new(id.clone()).modes(modes()))?;
                let commands = vec![
                    acp::AvailableCommand::new("review", "Review the change"),
                    acp::AvailableCommand::new("my-skill", "A user skill").input(
                        acp::AvailableCommandInput::Unstructured(
                            acp::UnstructuredCommandInput::new("what to do"),
                        ),
                    ),
                ];
                notify(
                    &cx,
                    &id,
                    acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                        commands,
                    )),
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::LoadSessionRequest, responder, cx| {
                l3.write(
                    "session/load",
                    serde_json::to_value(&req).unwrap_or_default(),
                );
                if flag("FAKE_ACP_SIGNED_OUT") {
                    return responder.respond_with_error(signed_out());
                }
                let id = req.session_id.clone();
                notify(
                    &cx,
                    &id,
                    acp::SessionUpdate::UserMessageChunk(text("(replayed prompt)")),
                );
                notify(
                    &cx,
                    &id,
                    acp::SessionUpdate::AgentMessageChunk(text("(replayed answer)")),
                );
                responder.respond(acp::LoadSessionResponse::new().modes(modes()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::ResumeSessionRequest, responder, _cx| {
                l4.write(
                    "session/resume",
                    serde_json::to_value(&req).unwrap_or_default(),
                );
                if flag("FAKE_ACP_SIGNED_OUT") {
                    return responder.respond_with_error(signed_out());
                }
                responder.respond(acp::ResumeSessionResponse::new().modes(modes()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::PromptRequest, responder, cx| {
                l5.write(
                    "session/prompt",
                    serde_json::to_value(&req).unwrap_or_default(),
                );
                let state = Arc::clone(&s2);
                let task_cx = cx.clone();
                cx.spawn(async move {
                    match turn(task_cx, state, req).await {
                        Ok(stop) => responder.respond(acp::PromptResponse::new(stop)),
                        Err(e) => responder.respond_with_error(e),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::SetSessionModeRequest, responder, cx| {
                l6.write(
                    "session/set_mode",
                    serde_json::to_value(&req).unwrap_or_default(),
                );
                responder.respond(acp::SetSessionModeResponse::new())?;
                notify(
                    &cx,
                    &req.session_id,
                    acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
                        req.mode_id.clone(),
                    )),
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: acp::CloseSessionRequest, responder, _cx| {
                log.write(
                    "session/close",
                    serde_json::to_value(&req).unwrap_or_default(),
                );
                responder.respond(acp::CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: acp::CancelNotification, _cx| {
                s3.cancel_signal(&n.session_id.0).notify_one();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}
