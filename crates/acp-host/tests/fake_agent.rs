// SPDX-License-Identifier: Apache-2.0

//! The ACP host against the scripted fake ACP agent (`coqo-fake-acp-agent`),
//! driven through the session manager like the daemon drives it. No
//! network, no real agent.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coqo_acp_host::{AcpOptions, AcpProvider, AgentCommand, AgentFs, FsHandle};
use coqo_agents::model::*;
use coqo_agents::{
    AgentError, ManagerOptions, McpServer, ProviderError, SessionManager, SessionStore,
    StaticProviders,
};
use coqo_kernel::{Backoff, RestartMode, RestartPolicy};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use futures_util::StreamExt;
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);
const FAKE: &str = env!("CARGO_BIN_EXE_coqo-fake-acp-agent");

/// An announced write: session, path, old text, new text.
type Expected = (String, PathBuf, Option<String>, String);

/// What the host asked of the daemon's buffers.
#[derive(Default)]
struct Buffers {
    /// Open buffers: path → text.
    open: Mutex<Vec<(PathBuf, String)>>,
    expected: Mutex<Vec<Expected>>,
    replaced: Mutex<Vec<(String, PathBuf, String)>>,
}

impl AgentFs for Buffers {
    fn expect_write(&self, session: &SessionId, path: &Path, old: Option<String>, new: String) {
        self.expected
            .lock()
            .unwrap()
            .push((session.0.clone(), path.to_path_buf(), old, new));
    }

    fn open_text(&self, path: &Path) -> Option<String> {
        let open = self.open.lock().unwrap();
        open.iter().find(|(p, _)| p == path).map(|(_, t)| t.clone())
    }

    fn replace_open(&self, session: &SessionId, path: &Path, text: &str) -> bool {
        if self.open_text(path).is_none() {
            return false;
        }
        self.replaced.lock().unwrap().push((
            session.0.clone(),
            path.to_path_buf(),
            text.to_owned(),
        ));
        true
    }
}

struct Env {
    dir: tempfile::TempDir,
    /// The workspace root (canonical, as the host compares canonical paths).
    root: PathBuf,
    log: PathBuf,
    buffers: Arc<Buffers>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let log = dir.path().join("agent.log");
        Self {
            dir,
            root,
            log,
            buffers: Arc::default(),
        }
    }

    fn provider(&self, env: &[(&str, &str)]) -> AcpProvider {
        self.provider_with(env, |_| {})
    }

    /// [`Self::provider`] with `options` changed by `change`.
    fn provider_with(
        &self,
        env: &[(&str, &str)],
        change: impl FnOnce(&mut AcpOptions),
    ) -> AcpProvider {
        let mut command = AgentCommand::new(FAKE).env("FAKE_ACP_LOG", &self.log);
        for (k, v) in env {
            command = command.env(*k, *v);
        }
        let fs = FsHandle::new();
        fs.set(Arc::clone(&self.buffers) as Arc<dyn AgentFs>);
        let mut options = AcpOptions::new("fake-acp");
        options.fs = fs;
        options.start_timeout = TIMEOUT;
        options.request_timeout = TIMEOUT;
        options.restart = RestartPolicy {
            mode: RestartMode::OnFailure,
            backoff: Backoff {
                initial: Duration::from_millis(20),
                max: Duration::from_millis(100),
                multiplier: 2,
            },
            max_restarts: Some(3),
            reset_after: Duration::from_secs(1),
        };
        change(&mut options);
        AcpProvider::command(command, options)
    }

    fn manager(&self, provider: AcpProvider) -> SessionManager {
        let store = SessionStore::open(&self.dir.path().join("coqo.db")).unwrap();
        let info = AgentProviderInfo {
            id: "fake".parse().unwrap(),
            name: "Fake ACP".into(),
            kind: AgentProviderKind::Acp,
            description: None,
        };
        let providers = StaticProviders::new().with(info, Arc::new(provider));
        let options = ManagerOptions {
            keep_alive: None,
            mcp_servers: Some(Arc::new(|_: &SessionInfo| {
                vec![McpServer {
                    name: "coqo-editor".into(),
                    command: "/usr/bin/coqod".into(),
                    args: vec!["mcp".into()],
                    env: vec![],
                }]
            })),
        };
        SessionManager::open(store, Arc::new(providers), options).unwrap()
    }

    fn params(&self, workspace: &str) -> CreateSessionParams {
        CreateSessionParams {
            provider: "fake".into(),
            cwd: self.root.display().to_string(),
            workspace: Some(workspace.into()),
            title: None,
            mode: None,
        }
    }

    /// Every line the fake agent logged for `event`.
    fn logged(&self, event: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["event"] == event)
            .map(|v| v["data"].clone())
            .collect()
    }
}

async fn prompt(manager: &SessionManager, id: &SessionId, script: &str) {
    manager
        .prompt(id, vec![ContentBlock::text(script)])
        .await
        .unwrap();
}

/// The session's updates from seq `since` up to and including the first
/// that matches `stop`.
async fn until(
    manager: &SessionManager,
    id: &SessionId,
    since: u64,
    stop: impl Fn(&SessionUpdate) -> bool,
) -> Vec<SessionUpdate> {
    let mut feed = manager.updates(id, Some(since)).unwrap().events;
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while let Some(item) = feed.next().await {
            let event = item.unwrap().event;
            let done = stop(&event);
            out.push(event);
            if done {
                return;
            }
        }
        panic!("feed ended early: {out:?}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; got {out:?}"));
    out
}

fn turn_ended(u: &SessionUpdate) -> bool {
    matches!(u, SessionUpdate::TurnEnded { .. })
}

fn said(updates: &[SessionUpdate]) -> String {
    updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn stop_reason(updates: &[SessionUpdate]) -> StopReason {
    updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::TurnEnded { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        })
        .expect("a turn_ended")
}

fn head(manager: &SessionManager, id: &SessionId) -> u64 {
    manager.snapshot(id).unwrap().head
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_a_turn_with_modes_commands_auth_and_usage() {
    let env = Env::new();
    let manager = env.manager(env.provider(&[]));
    let info = manager.create(env.params("ws-1")).await.unwrap();
    let id = info.id;

    let snap = manager.snapshot(&id).unwrap();
    let modes = snap.modes.expect("modes from session/new");
    assert_eq!(modes.current_mode_id, "default");
    assert_eq!(modes.available_modes.len(), 3);

    let since = head(&manager, &id);
    prompt(&manager, &id, "think hmm\nplan\nsay hello world\nusage").await;
    let updates = until(&manager, &id, since, turn_ended).await;
    assert_eq!(stop_reason(&updates), StopReason::EndTurn);
    assert_eq!(said(&updates), "hello world");
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::AgentThoughtChunk { .. }))
    );
    let plan = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Plan { entries } => Some(entries.clone()),
            _ => None,
        })
        .expect("a plan");
    assert_eq!(plan[1].status, PlanEntryStatus::InProgress);

    let snap = manager.snapshot(&id).unwrap();
    // Available commands arrive right after session/new, including skills.
    assert_eq!(snap.available_commands.len(), 2);
    assert_eq!(
        snap.available_commands[1].input_hint.as_deref(),
        Some("what to do")
    );
    // Billing source: kind and plan only.
    assert_eq!(
        snap.auth,
        Some(AuthStatus {
            kind: AuthKind::Account,
            label: Some("Claude Max".into()),
            plan: Some("max".into()),
        })
    );
    let usage = snap.usage.expect("usage");
    assert_eq!(usage.model.as_deref(), Some("fake-model"));
    assert_eq!(usage.used_tokens, Some(1200));

    // Nothing personal is stored or streamed.
    let all = until(&manager, &id, 0, turn_ended).await;
    let text = serde_json::to_string(&all).unwrap();
    assert!(
        !text.contains("example.com") && !text.contains("Acme"),
        "{text}"
    );

    // What the agent was told: fs and plan, no terminal (no backend set), no auth flow; the
    // session's cwd and MCP servers.
    let init = &env.logged("initialize")[0];
    let caps = &init["clientCapabilities"];
    assert_eq!(caps["fs"]["readTextFile"], true);
    assert_eq!(caps["fs"]["writeTextFile"], true);
    assert!(caps["plan"].is_object(), "{caps}");
    assert_ne!(caps["terminal"], true);
    assert_ne!(caps["auth"]["terminal"], true, "{caps}");
    let new = &env.logged("session/new")[0];
    assert_eq!(new["cwd"], env.root.display().to_string());
    assert_eq!(new["mcpServers"][0]["name"], "coqo-editor");
    assert_eq!(new["mcpServers"][0]["command"], "/usr/bin/coqod");

    // Modes: switching goes to the agent.
    manager.set_mode(&id, "plan".into()).await.unwrap();
    assert_eq!(
        manager.snapshot(&id).unwrap().info.mode.as_deref(),
        Some("plan")
    );
    assert_eq!(env.logged("session/set_mode")[0]["modeId"], "plan");

    manager.close(&id).await.unwrap();
    assert_eq!(env.logged("session/close").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_call_diff_is_announced_and_permission_is_answered_by_kind() {
    let env = Env::new();
    let manager = env.manager(env.provider(&[]));
    let id = manager.create(env.params("ws-1")).await.unwrap().id;
    let file = env.root.join("pong.txt");
    std::fs::write(&file, "ping").unwrap();

    let since = head(&manager, &id);
    prompt(&manager, &id, &format!("edit {} pong", file.display())).await;
    let updates = until(&manager, &id, since, |u| {
        matches!(u, SessionUpdate::PermissionRequested(_))
    })
    .await;

    // The diff arrives with the tool call, before the write, and is
    // announced to the buffers then.
    let call = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .expect("a tool call");
    assert_eq!(call.kind, ToolKind::Edit);
    assert_eq!(
        call.content,
        vec![ToolCallContent::Diff {
            path: file.display().to_string(),
            old_text: Some("ping".into()),
            new_text: "pong".into(),
        }]
    );
    assert_eq!(call.locations[0].path, file.display().to_string());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "ping");
    {
        let expected = env.buffers.expected.lock().unwrap();
        assert!(!expected.is_empty());
        assert_eq!(
            expected[0],
            (
                id.0.clone(),
                file.clone(),
                Some("ping".into()),
                "pong".into()
            )
        );
    }

    // Options keep their ids and kinds; pick "allow once" by kind (it is
    // last in the agent's order).
    let SessionUpdate::PermissionRequested(request) = updates.last().unwrap().clone() else {
        unreachable!()
    };
    let kinds: Vec<_> = request.options.iter().map(|o| o.kind).collect();
    assert_eq!(
        kinds,
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::AllowOnce
        ]
    );
    let allow = request
        .options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::AllowOnce)
        .unwrap();
    assert_eq!(
        manager.snapshot(&id).unwrap().info.state,
        AgentState::NeedsPermission
    );
    manager
        .respond_permission(
            &id,
            request.id,
            PermissionOutcome::Selected {
                option_id: allow.id.clone(),
            },
        )
        .unwrap();
    let rest = until(&manager, &id, since + updates.len() as u64, turn_ended).await;
    assert_eq!(stop_reason(&rest), StopReason::EndTurn);
    assert!(rest.iter().any(|u| matches!(
        u,
        SessionUpdate::ToolCallUpdate(t) if t.status == Some(ToolCallStatus::Completed)
    )));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "pong");

    // Rejecting: the agent fails the call and writes nothing.
    let since = head(&manager, &id);
    prompt(&manager, &id, &format!("edit {} nope", file.display())).await;
    let updates = until(&manager, &id, since, |u| {
        matches!(u, SessionUpdate::PermissionRequested(_))
    })
    .await;
    let SessionUpdate::PermissionRequested(request) = updates.last().unwrap().clone() else {
        unreachable!()
    };
    let reject = request
        .options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::RejectOnce)
        .unwrap();
    manager
        .respond_permission(
            &id,
            request.id,
            PermissionOutcome::Selected {
                option_id: reject.id.clone(),
            },
        )
        .unwrap();
    let rest = until(&manager, &id, since + updates.len() as u64, turn_ended).await;
    assert!(rest.iter().any(|u| matches!(
        u,
        SessionUpdate::ToolCallUpdate(t) if t.status == Some(ToolCallStatus::Failed)
    )));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "pong");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_requests_use_open_buffers_and_stay_inside_the_workspace() {
    let env = Env::new();
    let open = env.root.join("open.rs");
    let closed = env.root.join("sub/closed.rs");
    std::fs::write(&open, "on disk").unwrap();
    env.buffers
        .open
        .lock()
        .unwrap()
        .push((open.clone(), "unsaved edit".into()));
    let outside = env.dir.path().join("secret.txt");
    std::fs::write(&outside, "secret").unwrap();

    let manager = env.manager(env.provider(&[]));
    let id = manager.create(env.params("ws-1")).await.unwrap().id;

    let read = async |path: &Path| {
        let since = head(&manager, &id);
        prompt(&manager, &id, &format!("read {}", path.display())).await;
        said(&until(&manager, &id, since, turn_ended).await)
    };
    assert_eq!(read(&open).await, "unsaved edit");
    let denied = read(&outside).await;
    assert!(denied.starts_with("error:"), "{denied}");
    assert!(!denied.contains("secret\n"));
    let escape = env.root.join("../secret.txt");
    assert!(read(&escape).await.starts_with("error:"));

    // Into an open buffer: a tracked change, the disk is untouched.
    let since = head(&manager, &id);
    prompt(
        &manager,
        &id,
        &format!("write {} from agent", open.display()),
    )
    .await;
    until(&manager, &id, since, turn_ended).await;
    assert_eq!(
        env.buffers.replaced.lock().unwrap()[0],
        (id.0.clone(), open.clone(), "from agent".into())
    );
    assert_eq!(std::fs::read_to_string(&open).unwrap(), "on disk");

    // Not open: written to disk (creating directories), announced first.
    let since = head(&manager, &id);
    prompt(
        &manager,
        &id,
        &format!("write {} new file", closed.display()),
    )
    .await;
    until(&manager, &id, since, turn_ended).await;
    assert_eq!(std::fs::read_to_string(&closed).unwrap(), "new file");
    assert!(
        env.buffers
            .expected
            .lock()
            .unwrap()
            .iter()
            .any(|(s, p, _, new)| s == &id.0 && p == &closed && new == "new file")
    );

    // Writing outside the workspace is refused.
    let since = head(&manager, &id);
    prompt(&manager, &id, &format!("write {} pwned", outside.display())).await;
    let updates = until(&manager, &id, since, turn_ended).await;
    assert_eq!(stop_reason(&updates), StopReason::Error);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_ends_a_running_turn() {
    let env = Env::new();
    let manager = env.manager(env.provider(&[]));
    let id = manager.create(env.params("ws-1")).await.unwrap().id;
    let since = head(&manager, &id);
    prompt(&manager, &id, "say waiting\nwait").await;
    until(&manager, &id, since, |u| {
        matches!(u, SessionUpdate::AgentMessageChunk { .. })
    })
    .await;
    assert!(manager.cancel(&id).await.unwrap());
    let updates = until(&manager, &id, since, turn_ended).await;
    assert_eq!(stop_reason(&updates), StopReason::Cancelled);
    assert_eq!(manager.snapshot(&id).unwrap().info.state, AgentState::Idle);

    // The session still works.
    let since = head(&manager, &id);
    prompt(&manager, &id, "say again").await;
    assert_eq!(
        said(&until(&manager, &id, since, turn_ended).await),
        "again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_daemon_resumes_the_agent_session() {
    for no_resume in [false, true] {
        let env = Env::new();
        let agent_env: &[(&str, &str)] = if no_resume {
            &[("FAKE_ACP_NO_RESUME", "1")]
        } else {
            &[]
        };
        let id = {
            let manager = env.manager(env.provider(agent_env));
            let id = manager.create(env.params("ws-1")).await.unwrap().id;
            prompt(&manager, &id, "say first").await;
            until(&manager, &id, 0, turn_ended).await;
            manager.shutdown().await;
            id
        };
        let acp_id = env.logged("session/new")[0].clone();
        assert!(acp_id.is_object());

        // A new manager and provider over the same store: a new process.
        let manager = env.manager(env.provider(agent_env));
        let since = head(&manager, &id);
        prompt(&manager, &id, "say second").await;
        let updates = until(&manager, &id, since, turn_ended).await;
        assert_eq!(said(&updates), "second", "no_resume={no_resume}");
        assert_eq!(env.logged("start").len(), 2);
        let (resumed, loaded) = (env.logged("session/resume"), env.logged("session/load"));
        if no_resume {
            assert!(resumed.is_empty());
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0]["mcpServers"][0]["name"], "coqo-editor");
        } else {
            assert_eq!(resumed.len(), 1);
            assert!(loaded.is_empty());
            assert_eq!(resumed[0]["cwd"], env.root.display().to_string());
        }
        // Coqo has the transcript, so the agent's replay is not recorded.
        let all = until(&manager, &id, 0, |u| {
            matches!(u, SessionUpdate::TurnEnded { turn: 2, .. })
        })
        .await;
        assert_eq!(said(&all), "firstsecond", "no_resume={no_resume}");
        manager.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_agent_fails_the_turn_and_the_session_reattaches() {
    let env = Env::new();
    let manager = env.manager(env.provider(&[]));
    let id = manager.create(env.params("ws-1")).await.unwrap().id;
    let since = head(&manager, &id);
    prompt(&manager, &id, "crash").await;
    let updates = until(&manager, &id, since, turn_ended).await;
    assert_eq!(stop_reason(&updates), StopReason::Error);

    // The supervisor restarts the agent; the session resumes on it.
    let since = head(&manager, &id);
    prompt(&manager, &id, "say back").await;
    let updates = until(&manager, &id, since, turn_ended).await;
    assert_eq!(said(&updates), "back");
    assert_eq!(env.logged("start").len(), 2);
    assert_eq!(env.logged("session/resume").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_out_agents_report_auth_required() {
    let env = Env::new();
    let manager = env.manager(env.provider(&[("FAKE_ACP_SIGNED_OUT", "1")]));
    let err = manager.create(env.params("ws-1")).await.unwrap_err();
    let AgentError::Provider(ProviderError::AuthRequired(message)) = &err else {
        panic!("expected auth_required, got {err:?}");
    };
    assert!(message.contains("signed_out"), "{message}");
    assert_eq!(
        err.to_error().code,
        coqo_protocol::methods::codes::AUTH_REQUIRED
    );
}

fn alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_agent_process_per_workspace_stopped_with_its_last_session() {
    let env = Env::new();
    let manager = env.manager(env.provider(&[]));
    let a = manager.create(env.params("ws-1")).await.unwrap().id;
    let b = manager.create(env.params("ws-1")).await.unwrap().id;
    assert_eq!(env.logged("start").len(), 1);
    let c = manager.create(env.params("ws-2")).await.unwrap().id;
    assert_eq!(env.logged("start").len(), 2);

    // Both sessions of ws-1 share the process and still stream separately.
    for (id, word) in [(&a, "alpha"), (&b, "beta")] {
        let since = head(&manager, id);
        prompt(&manager, id, &format!("say {word}")).await;
        assert_eq!(said(&until(&manager, id, since, turn_ended).await), word);
    }

    let pids: Vec<u64> = env
        .logged("start")
        .iter()
        .map(|s| s["pid"].as_u64().unwrap())
        .collect();
    assert!(pids.iter().all(|&p| alive(p)));

    // ws-1's process outlives one of its sessions, not both.
    manager.close(&a).await.unwrap();
    assert!(alive(pids[0]));
    manager.close(&b).await.unwrap();
    wait_dead(pids[0]).await;
    assert!(alive(pids[1]));
    manager.close(&c).await.unwrap();
    wait_dead(pids[1]).await;
}

async fn wait_dead(pid: u64) {
    tokio::time::timeout(TIMEOUT, async {
        while alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("agent process {pid} is still running"));
}

/// Workspace `ws-rules` with `root`, as the workspace service stores it.
fn add_workspace(db: &Path, root: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at, last_opened_at)
         VALUES ('ws-rules', 'app', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO workspace_roots (workspace, root_id, position, path, name)
         VALUES ('ws-rules', 1, 0, ?1, 'app')",
        [root.to_string_lossy()],
    )
    .unwrap();
}

/// The Claude adapter's `Bash` and `Write` permission requests (the tool
/// call, then the request with the whole call again) give the rules engine
/// the command and the paths, so "remember for this workspace" gets a rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adapter_shaped_permission_requests_get_a_suggested_rule() {
    let env = Env::new();
    let manager = env.manager(env.provider(&[]));
    add_workspace(&env.dir.path().join("coqo.db"), &env.root);
    std::fs::create_dir_all(env.root.join("src")).unwrap();
    let id = manager.create(env.params("ws-rules")).await.unwrap().id;

    let since = head(&manager, &id);
    let file = env.root.join("src/a.rs");
    prompt(
        &manager,
        &id,
        &format!(
            "run cargo test -p shop\nedit {} fn a() {{}}",
            file.display()
        ),
    )
    .await;
    let mut feed = manager.updates(&id, Some(since)).unwrap().events;
    let mut asked = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while let Some(item) = feed.next().await {
            match item.unwrap().event {
                SessionUpdate::PermissionRequested(request) => {
                    let tool_call = request.tool_call.clone().unwrap();
                    asked.push((tool_call.kind, request.suggested_rule.clone()));
                    let allow = request
                        .options
                        .iter()
                        .find(|o| o.kind == PermissionOptionKind::AllowOnce)
                        .unwrap();
                    manager
                        .respond_permission(
                            &id,
                            request.id,
                            PermissionOutcome::Selected {
                                option_id: allow.id.clone(),
                            },
                        )
                        .unwrap();
                }
                SessionUpdate::TurnEnded { .. } => return,
                _ => {}
            }
        }
    })
    .await
    .expect("the turn ends");
    assert_eq!(
        asked,
        [
            (
                Some(ToolKind::Execute),
                Some(PermissionRuleSpec {
                    tool: ToolKind::Execute,
                    pattern: "cargo test".into(),
                })
            ),
            (
                Some(ToolKind::Edit),
                Some(PermissionRuleSpec {
                    tool: ToolKind::Edit,
                    pattern: "src/**".into(),
                })
            ),
        ]
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn a() {}");
}

const GUIDE: &str = "# Coqo operating guide (v1)\n\nUse editor_context first.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_system_prompt_is_appended_on_new_resume_and_load_when_advertised() {
    let with_guide = |o: &mut AcpOptions| o.system_prompt = Some(GUIDE.into());
    let expected = serde_json::json!({
        "mode": "append",
        "content": [{"type": "text", "text": GUIDE}],
    });
    for no_resume in [false, true] {
        let env = Env::new();
        let agent_env: &[(&str, &str)] = if no_resume {
            &[("FAKE_ACP_NO_RESUME", "1")]
        } else {
            &[]
        };
        let id = {
            let manager = env.manager(env.provider_with(agent_env, with_guide));
            let id = manager.create(env.params("ws-1")).await.unwrap().id;
            manager.shutdown().await;
            id
        };
        assert_eq!(env.logged("session/new")[0]["systemPrompt"], expected);

        // A new daemon reattaches with the same prompt.
        let manager = env.manager(env.provider_with(agent_env, with_guide));
        let since = head(&manager, &id);
        prompt(&manager, &id, "say again").await;
        until(&manager, &id, since, turn_ended).await;
        let reattached = if no_resume {
            env.logged("session/load")
        } else {
            env.logged("session/resume")
        };
        assert_eq!(reattached.len(), 1, "no_resume={no_resume}");
        assert_eq!(reattached[0]["systemPrompt"], expected);
        manager.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_meta_is_sent_on_new_resume_and_load() {
    let meta = serde_json::json!({
        "claudeCode": {"options": {"plugins": [{"type": "local", "path": "/data/claude-plugins/coqo"}]}}
    });
    let with_meta = |o: &mut AcpOptions| {
        o.session_meta = meta.as_object().cloned();
    };
    for no_resume in [false, true] {
        let env = Env::new();
        let agent_env: &[(&str, &str)] = if no_resume {
            &[("FAKE_ACP_NO_RESUME", "1")]
        } else {
            &[]
        };
        let id = {
            let manager = env.manager(env.provider_with(agent_env, with_meta));
            let id = manager.create(env.params("ws-1")).await.unwrap().id;
            manager.shutdown().await;
            id
        };
        assert_eq!(env.logged("session/new")[0]["_meta"], meta);

        let manager = env.manager(env.provider_with(agent_env, with_meta));
        let since = head(&manager, &id);
        prompt(&manager, &id, "say again").await;
        until(&manager, &id, since, turn_ended).await;
        let reattached = if no_resume {
            env.logged("session/load")
        } else {
            env.logged("session/resume")
        };
        assert_eq!(reattached[0]["_meta"], meta, "no_resume={no_resume}");
        manager.shutdown().await;
    }

    // None set: no `_meta`.
    let env = Env::new();
    let manager = env.manager(env.provider(&[]));
    manager.create(env.params("ws-1")).await.unwrap();
    let created = env.logged("session/new");
    assert!(created[0].get("_meta").is_none(), "{}", created[0]);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_system_prompt_when_not_advertised_or_not_set() {
    // The agent does not take one.
    let env = Env::new();
    let manager = env.manager(
        env.provider_with(&[("FAKE_ACP_NO_SYSTEM_PROMPT", "1")], |o| {
            o.system_prompt = Some(GUIDE.into());
        }),
    );
    manager.create(env.params("ws-1")).await.unwrap();
    let created = env.logged("session/new");
    assert!(created[0].get("systemPrompt").is_none(), "{}", created[0]);
    manager.shutdown().await;

    // None set (the guide turned off), or only whitespace.
    for prompt in [None, Some("  \n")] {
        let env = Env::new();
        let manager =
            env.manager(env.provider_with(&[], |o| o.system_prompt = prompt.map(Into::into)));
        manager.create(env.params("ws-1")).await.unwrap();
        let created = env.logged("session/new");
        assert!(created[0].get("systemPrompt").is_none(), "{}", created[0]);
        manager.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_use_is_counted_per_session_by_name_and_group_and_persisted() {
    let env = Env::new();
    let script = "tool mcp__coqo-editor__editor_context\n\
                  tool Read\n\
                  tool mcp__coqo-editor__set_status\n\
                  tool mcp__coqo-editor__set_status\n\
                  tool mcp__coqo-editor__maven_test\n\
                  tool mcp__github__get_issue";
    let id = {
        let manager = env.manager(env.provider(&[]));
        let id = manager.create(env.params("ws-1")).await.unwrap().id;
        prompt(&manager, &id, script).await;
        until(&manager, &id, 0, turn_ended).await;
        manager.shutdown().await;
        id
    };
    // Each call is reported twice with one id: counted once.
    let manager = env.manager(env.provider(&[]));
    let info = manager.snapshot(&id).unwrap().info;
    let got: Vec<_> = info
        .tool_use
        .iter()
        .map(|u| (u.name.as_str(), u.group, u.count))
        .collect();
    assert_eq!(
        got,
        [
            ("editor_context", ToolGroup::Coqo, 1),
            ("Read", ToolGroup::Agent, 1),
            ("set_status", ToolGroup::Coqo, 2),
            ("maven_test", ToolGroup::Mod, 1),
            ("mcp__github__get_issue", ToolGroup::Mcp, 1),
        ]
    );
    let listed = manager.list(&ListSessionsParams::default()).unwrap();
    assert_eq!(listed.sessions[0].tool_use, info.tool_use);
}

fn value(manager: &SessionManager, id: &SessionId, config_id: &str) -> ConfigValue {
    let snap = manager.snapshot(id).unwrap();
    snap.config_options
        .iter()
        .find(|o| o.id == config_id)
        .unwrap_or_else(|| panic!("no {config_id}: {:?}", snap.config_options))
        .value()
}

fn select(v: &str) -> ConfigValue {
    ConfigValue::Value(v.into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_options_are_shown_set_and_followed() {
    let env = Env::new();
    let config = [("FAKE_ACP_CONFIG", "1")];
    let manager = env.manager(env.provider(&config));
    let id = manager.create(env.params("ws-1")).await.unwrap().id;

    // From session/new, in the agent's order; Fast mode is a toggle because
    // Coqo takes boolean options.
    let snap = manager.snapshot(&id).unwrap();
    let ids: Vec<_> = snap.config_options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["model", "effort", "fast", "mode"]);
    assert_eq!(snap.config_options[0].category.as_deref(), Some("model"));
    assert_eq!(
        snap.config_options[1].category.as_deref(),
        Some("thought_level")
    );
    assert_eq!(snap.config_options[2].category.as_deref(), Some("speed"));
    assert_eq!(value(&manager, &id, "fast"), ConfigValue::Boolean(false));
    let caps = &env.logged("initialize")[0]["clientCapabilities"];
    assert!(
        caps["session"]["configOptions"]["boolean"].is_object(),
        "{caps}"
    );

    // Coqo's changes reach the agent.
    manager
        .set_config_option(&id, "model".into(), select("sonnet"))
        .await
        .unwrap();
    manager
        .set_config_option(&id, "effort".into(), select("high"))
        .await
        .unwrap();
    manager
        .set_config_option(&id, "fast".into(), ConfigValue::Boolean(true))
        .await
        .unwrap();
    let sets = env.logged("session/set_config_option");
    assert_eq!(sets.len(), 3);
    assert_eq!(
        (&sets[0]["configId"], &sets[0]["value"]),
        (&"model".into(), &"sonnet".into())
    );
    assert_eq!(sets[2]["type"], "boolean");
    assert_eq!(sets[2]["value"], true);
    assert_eq!(value(&manager, &id, "model"), select("sonnet"));
    assert_eq!(value(&manager, &id, "effort"), select("high"));
    assert_eq!(value(&manager, &id, "fast"), ConfigValue::Boolean(true));

    // The agent's own change shows up.
    let since = head(&manager, &id);
    prompt(&manager, &id, "config effort low").await;
    let updates = until(&manager, &id, since, turn_ended).await;
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::ConfigOptions { .. })),
        "{updates:?}"
    );
    assert_eq!(value(&manager, &id, "effort"), select("low"));

    // With a mode option, a mode change goes through it, not set_mode.
    manager.set_mode(&id, "plan".into()).await.unwrap();
    assert!(env.logged("session/set_mode").is_empty());
    assert_eq!(
        env.logged("session/set_config_option")[3]["configId"],
        "mode"
    );
    let snap = manager.snapshot(&id).unwrap();
    assert_eq!(snap.info.mode.as_deref(), Some("plan"));
    assert_eq!(value(&manager, &id, "mode"), select("plan"));
    // And the mode option keeps the modes and the session's mode in step.
    manager
        .set_config_option(&id, "mode".into(), select("acceptEdits"))
        .await
        .unwrap();
    let snap = manager.snapshot(&id).unwrap();
    assert_eq!(snap.info.mode.as_deref(), Some("acceptEdits"));
    assert_eq!(snap.modes.unwrap().current_mode_id, "acceptEdits");

    // A value the option doesn't take never reaches the agent.
    let err = manager
        .set_config_option(&id, "model".into(), select("gpt"))
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::InvalidParams(_)), "{err:?}");
    assert_eq!(env.logged("session/set_config_option").len(), 5);
    manager.close(&id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_options_are_set_again_after_a_resume_or_a_crash() {
    let env = Env::new();
    let config = [("FAKE_ACP_CONFIG", "1")];
    let id = {
        let manager = env.manager(env.provider(&config));
        let id = manager.create(env.params("ws-1")).await.unwrap().id;
        manager
            .set_config_option(&id, "model".into(), select("sonnet"))
            .await
            .unwrap();
        manager
            .set_config_option(&id, "effort".into(), select("high"))
            .await
            .unwrap();
        manager.shutdown().await;
        id
    };

    // A new daemon and agent process: the agent resumes with its defaults,
    // and Coqo sets what the session had again, in order.
    let manager = env.manager(env.provider(&config));
    let since = head(&manager, &id);
    prompt(&manager, &id, "say back").await;
    until(&manager, &id, since, turn_ended).await;
    assert_eq!(env.logged("session/resume").len(), 1);
    let sets: Vec<_> = env
        .logged("session/set_config_option")
        .iter()
        .map(|s| (s["configId"].clone(), s["value"].clone()))
        .collect();
    assert_eq!(
        sets,
        [
            ("model".into(), "sonnet".into()),
            ("effort".into(), "high".into()),
            ("model".into(), "sonnet".into()),
            ("effort".into(), "high".into()),
        ]
    );
    assert_eq!(value(&manager, &id, "model"), select("sonnet"));
    assert_eq!(value(&manager, &id, "effort"), select("high"));

    // The agent crashes: its next process gets them again too.
    let since = head(&manager, &id);
    prompt(&manager, &id, "crash").await;
    until(&manager, &id, since, turn_ended).await;
    let since = head(&manager, &id);
    prompt(&manager, &id, "say again").await;
    until(&manager, &id, since, turn_ended).await;
    assert_eq!(env.logged("session/set_config_option").len(), 6);
    assert_eq!(value(&manager, &id, "effort"), select("high"));
    manager.shutdown().await;
}
