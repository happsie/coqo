//! The ACP client half of the spike: what `crates/acp-host` will do in coqod.

use crate::{Args, which};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    ClientCapabilities, ContentBlock, FileSystemCapabilities, Implementation, InitializeRequest,
    LoadSessionRequest, McpServer, McpServerStdio, NewSessionRequest, PermissionOptionKind,
    PlanCapabilities, PromptRequest, ReadTextFileRequest, ReadTextFileResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate,
    SetSessionModeRequest, TextContent, ToolCallContent, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, UntypedMessage};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

struct State {
    t0: Instant,
    first_chunk: AtomicBool,
    updates: AtomicUsize,
    reply: Mutex<String>,
}

impl State {
    fn ms(&self) -> u128 {
        self.t0.elapsed().as_millis()
    }
    fn log(&self, msg: impl AsRef<str>) {
        println!("[{:>6} ms] {}", self.ms(), msg.as_ref());
    }
    fn on_update(&self, n: &SessionNotification) {
        self.updates.fetch_add(1, Ordering::Relaxed);
        match &n.update {
            SessionUpdate::AgentMessageChunk(c) => {
                if let ContentBlock::Text(t) = &c.content {
                    if !self.first_chunk.swap(true, Ordering::Relaxed) {
                        self.log(format!("FIRST agent_message_chunk {:?}", t.text));
                    }
                    self.reply.lock().unwrap().push_str(&t.text);
                }
            }
            SessionUpdate::UserMessageChunk(c) => {
                if let ContentBlock::Text(t) = &c.content {
                    self.log(format!("user_message_chunk (replay) {:?}", short(&t.text)));
                }
            }
            SessionUpdate::AgentThoughtChunk(_) => {}
            SessionUpdate::ToolCall(tc) => self.log(format!(
                "tool_call id={} title={:?} kind={:?} status={:?} content=[{}] raw_input={}",
                tc.tool_call_id.0,
                tc.title,
                tc.kind,
                tc.status,
                content_kinds(&tc.content),
                tc.raw_input
                    .as_ref()
                    .map(|v| short(&v.to_string()))
                    .unwrap_or_default()
            )),
            SessionUpdate::ToolCallUpdate(u) => self.log(format!(
                "tool_call_update id={} status={:?} title={:?} content=[{}]",
                u.tool_call_id.0,
                u.fields.status,
                u.fields.title,
                u.fields
                    .content
                    .as_deref()
                    .map(content_kinds)
                    .unwrap_or_default()
            )),
            SessionUpdate::Plan(p) => self.log(format!("plan entries={}", p.entries.len())),
            SessionUpdate::AvailableCommandsUpdate(a) => {
                let names: Vec<&str> = a
                    .available_commands
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect();
                self.log(format!(
                    "available_commands_update n={} first={:?}",
                    names.len(),
                    &names[..names.len().min(12)]
                ));
            }
            SessionUpdate::UsageUpdate(u) => self.log(format!(
                "usage_update {}",
                short(&serde_json::to_string(u).unwrap_or_default())
            )),
            other => {
                let v = serde_json::to_value(other).unwrap_or_default();
                self.log(format!(
                    "session_update {}",
                    v.get("sessionUpdate")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                ));
            }
        }
    }
}

fn content_kinds(c: &[ToolCallContent]) -> String {
    c.iter()
        .map(|c| match c {
            ToolCallContent::Diff(d) => format!(
                "diff {} (old={} new={}B)",
                d.path.display(),
                d.old_text.is_some(),
                d.new_text.len()
            ),
            ToolCallContent::Content(_) => "content".into(),
            ToolCallContent::Terminal(_) => "terminal".into(),
            _ => "other".into(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn short(s: &str) -> String {
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s.to_owned()
    }
}

/// Drop personal fields before printing (spike output goes into the ADR).
fn redact(mut v: Value) -> Value {
    match &mut v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if matches!(
                    k.as_str(),
                    "email" | "organization" | "accountUuid" | "orgId"
                ) {
                    *val = Value::String("<redacted>".into());
                } else {
                    *val = redact(val.take());
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| *x = redact(x.take())),
        _ => {}
    }
    v
}

pub async fn run(raw: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::new(raw);
    let adapter = args.value("--adapter").ok_or("--adapter is required")?;
    let cwd = PathBuf::from(args.value("--cwd").ok_or("--cwd is required")?);
    let mode = args.value("--mode").unwrap_or("new").to_owned();
    let session = args.value("--session").map(str::to_owned);
    let prompt = args.value("--prompt").map(str::to_owned);
    let set_mode = args.value("--set-mode").map(str::to_owned);

    // The injected MCP server: this same binary in `mcp-echo` mode. ACP v1 wants
    // an absolute command path (v2 requires it).
    let me = std::env::current_exe()?;
    let echo_log = cwd.join("echo.log");
    let mcp = vec![McpServer::Stdio(
        McpServerStdio::new("coqo-echo", me)
            .args(vec!["mcp-echo".into()])
            .env(vec![agent_client_protocol::v1::schema::EnvVariable::new(
                "ECHO_LOG",
                echo_log.display().to_string(),
            )]),
    )];

    let mut config = AcpAgentConfig::new(adapter);
    if args.flag("--installed-claude") {
        // Use the user's own installed `claude` instead of the SDK's bundled copy.
        let claude = which("claude").ok_or("`claude` not found on PATH")?;
        let claude = std::fs::canonicalize(claude)?;
        println!("CLAUDE_CODE_EXECUTABLE={}", claude.display());
        config = config.env("CLAUDE_CODE_EXECUTABLE", claude.display().to_string());
    }

    let st = Arc::new(State {
        t0: Instant::now(),
        first_chunk: AtomicBool::new(false),
        updates: AtomicUsize::new(0),
        reply: Mutex::new(String::new()),
    });
    st.log(format!("spawn {adapter} (mode={mode})"));
    let agent = AcpAgent::new(config);

    let (s1, s2, s3, s4, s5) = (st.clone(), st.clone(), st.clone(), st.clone(), st.clone());
    Client
        .v1()
        .name("coqo-s3-spike")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                s1.on_update(&n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |n: UntypedMessage, _cx| {
                s2.log(format!(
                    "ext notification {} {}",
                    n.method,
                    short(&redact(n.params).to_string())
                ));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, _cx| {
                let opts: Vec<String> = req
                    .options
                    .iter()
                    .map(|o| format!("{}:{:?}", o.option_id.0, o.kind))
                    .collect();
                s3.log(format!(
                    "PERMISSION request tool={:?} options={opts:?}",
                    req.tool_call.fields.title
                ));
                // Coqo will raise this to the UI (and an alert). The spike approves once.
                let pick = req
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowOnce)
                    .map(|o| o.option_id.clone());
                let outcome = match pick {
                    Some(id) => {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                    }
                    None => RequestPermissionOutcome::Cancelled,
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ReadTextFileRequest, responder, _cx| {
                s4.log(format!("fs/read_text_file {}", req.path.display()));
                let text = std::fs::read_to_string(&req.path).unwrap_or_default();
                responder.respond(ReadTextFileResponse::new(text))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: WriteTextFileRequest, responder, _cx| {
                s5.log(format!(
                    "fs/write_text_file {} ({} B)",
                    req.path.display(),
                    req.content.len()
                ));
                let _ = std::fs::write(&req.path, &req.content);
                responder.respond(WriteTextFileResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(
                            ClientCapabilities::new()
                                .fs(FileSystemCapabilities::new()
                                    .read_text_file(true)
                                    .write_text_file(true))
                                .plan(PlanCapabilities::new()),
                        )
                        .client_info(Implementation::new("coqo-s3-spike", "0.0.0")),
                )
                .block_task()
                .await?;
            st.log(format!(
                "initialize OK agent={} protocol={:?}",
                serde_json::to_string(&init.agent_info).unwrap_or_default(),
                init.protocol_version
            ));
            println!(
                "  agentCapabilities: {}",
                serde_json::to_string(&init.agent_capabilities).unwrap_or_default()
            );
            println!(
                "  authMethods: {}",
                serde_json::to_string(&init.auth_methods).unwrap_or_default()
            );

            let sid: SessionId = match mode.as_str() {
                "new" => {
                    let r = cx
                        .send_request(NewSessionRequest::new(cwd.clone()).mcp_servers(mcp))
                        .block_task()
                        .await?;
                    st.log(format!("session/new OK id={}", r.session_id.0));
                    println!(
                        "  modes: {}",
                        short(&serde_json::to_string(&r.modes).unwrap_or_default())
                    );
                    r.session_id
                }
                "load" => {
                    let id = SessionId::new(session.clone().ok_or_else(|| {
                        agent_client_protocol::Error::invalid_params().data("--session")
                    })?);
                    cx.send_request(
                        LoadSessionRequest::new(id.clone(), cwd.clone()).mcp_servers(mcp),
                    )
                    .block_task()
                    .await?;
                    st.log(format!(
                        "session/load OK id={} (replayed {} updates)",
                        id.0,
                        st.updates.load(Ordering::Relaxed)
                    ));
                    id
                }
                "resume" => {
                    let id = SessionId::new(session.clone().ok_or_else(|| {
                        agent_client_protocol::Error::invalid_params().data("--session")
                    })?);
                    cx.send_request(
                        ResumeSessionRequest::new(id.clone(), cwd.clone()).mcp_servers(mcp),
                    )
                    .block_task()
                    .await?;
                    st.log(format!(
                        "session/resume OK id={} (updates so far {})",
                        id.0,
                        st.updates.load(Ordering::Relaxed)
                    ));
                    id
                }
                other => {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data(format!("unknown mode {other}")));
                }
            };

            if let Some(mode_id) = &set_mode {
                cx.send_request(SetSessionModeRequest::new(sid.clone(), mode_id.clone()))
                    .block_task()
                    .await?;
                st.log(format!("session/set_mode {mode_id} OK"));
            }

            if let Some(prompt) = prompt {
                st.log(format!("session/prompt {prompt:?}"));
                let r = cx
                    .send_request(PromptRequest::new(
                        sid.clone(),
                        vec![ContentBlock::Text(TextContent::new(prompt))],
                    ))
                    .block_task()
                    .await?;
                st.log(format!("prompt done stop_reason={:?}", r.stop_reason));
                println!("  reply: {:?}", st.reply.lock().unwrap());
            }
            st.log(format!("SESSION_ID {}", sid.0));
            Ok(())
        })
        .await?;
    Ok(())
}
