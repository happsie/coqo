// SPDX-License-Identifier: Apache-2.0

//! ACP `terminal/*` (issue #185) against the scripted fake ACP agent, with
//! plain processes standing in for the daemon's PTYs. No network, no real
//! agent.

use std::collections::HashMap;
use std::io::Read as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coqo_acp_host::{
    AcpOptions, AcpProvider, AgentCommand, AgentTerminal, AgentTerminals, TerminalExit,
    TerminalHandle, TerminalRequest, TerminalText,
};
use coqo_agents::model::*;
use coqo_agents::{ManagerOptions, SessionManager, SessionStore, StaticProviders};
use coqo_mod_api::BoxFuture;
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::watch;

const TIMEOUT: Duration = Duration::from_secs(20);
const FAKE: &str = env!("CARGO_BIN_EXE_coqo-fake-acp-agent");

/// One command, run with pipes instead of a PTY.
struct Proc {
    id: u64,
    pid: u32,
    output: Arc<Mutex<Vec<u8>>>,
    exit: watch::Receiver<Option<TerminalExit>>,
    procs: Arc<Procs>,
}

impl AgentTerminal for Proc {
    fn id(&self) -> String {
        self.id.to_string()
    }

    fn output(&self) -> TerminalText {
        TerminalText {
            text: String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned(),
            truncated: false,
        }
    }

    fn exit_status(&self) -> Option<TerminalExit> {
        self.exit.borrow().clone()
    }

    fn wait(&self) -> BoxFuture<'static, TerminalExit> {
        let mut exit = self.exit.clone();
        Box::pin(async move {
            exit.wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|e| e.clone())
                .unwrap_or_default()
        })
    }

    fn kill(&self) {
        if self.exit.borrow().is_none() {
            let _ = Command::new("kill")
                .args(["-KILL", &self.pid.to_string()])
                .status();
        }
        self.procs.killed.lock().unwrap().push(self.id);
    }

    fn close(&self) {
        self.kill();
        self.procs.running.lock().unwrap().remove(&self.id);
        self.procs.closed.lock().unwrap().push(self.id);
    }
}

/// The test's terminal backend.
#[derive(Default)]
struct Procs {
    next: AtomicU64,
    requests: Mutex<Vec<TerminalRequest>>,
    /// Ids not closed yet, and their commands.
    running: Mutex<HashMap<u64, String>>,
    killed: Mutex<Vec<u64>>,
    closed: Mutex<Vec<u64>>,
}

struct Backend(Arc<Procs>);

impl AgentTerminals for Backend {
    fn create(&self, request: TerminalRequest) -> Result<Arc<dyn AgentTerminal>, String> {
        let procs = &self.0;
        procs.requests.lock().unwrap().push(request.clone());
        let mut cmd = if request.args.is_empty() {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(&request.command);
            cmd
        } else {
            let mut cmd = Command::new(&request.command);
            cmd.args(&request.args);
            cmd
        };
        cmd.current_dir(&request.cwd)
            .envs(request.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let id = procs.next.fetch_add(1, Ordering::Relaxed) + 101;
        let output = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = watch::channel(None);
        let mut stdout = child.stdout.take().unwrap();
        let sink = Arc::clone(&output);
        let pid = child.id();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            let status = child.wait().unwrap();
            tx.send_replace(Some(TerminalExit {
                code: status.code().and_then(|c| u32::try_from(c).ok()),
                signal: status.signal().map(|s| match s {
                    9 => "SIGKILL".to_owned(),
                    s => format!("signal {s}"),
                }),
            }));
        });
        procs
            .running
            .lock()
            .unwrap()
            .insert(id, request.command.clone());
        Ok(Arc::new(Proc {
            id,
            pid,
            output,
            exit: rx,
            procs: Arc::clone(procs),
        }))
    }
}

struct Env {
    dir: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
    procs: Arc<Procs>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let log = dir.path().join("agent.log");
        Self {
            dir,
            root,
            log,
            procs: Arc::default(),
        }
    }

    fn manager(&self, terminals: bool) -> SessionManager {
        let command = AgentCommand::new(FAKE).env("FAKE_ACP_LOG", &self.log);
        let mut options = AcpOptions::new("fake-acp");
        options.start_timeout = TIMEOUT;
        options.request_timeout = TIMEOUT;
        if terminals {
            let handle = TerminalHandle::new();
            handle.set(Arc::new(Backend(Arc::clone(&self.procs))));
            options.terminals = handle;
        }
        let provider = AcpProvider::command(command, options);
        let store = SessionStore::open(&self.dir.path().join("coqo.db")).unwrap();
        let info = AgentProviderInfo {
            id: "fake".parse().unwrap(),
            name: "Fake ACP".into(),
            kind: AgentProviderKind::Acp,
            description: None,
        };
        let providers = StaticProviders::new().with(info, Arc::new(provider));
        SessionManager::open(store, Arc::new(providers), ManagerOptions::default()).unwrap()
    }

    fn params(&self) -> CreateSessionParams {
        CreateSessionParams {
            provider: "fake".into(),
            cwd: self.root.display().to_string(),
            workspace: Some("ws-1".into()),
            title: None,
            mode: None,
        }
    }

    fn client_capabilities(&self) -> Value {
        let text = std::fs::read_to_string(&self.log).unwrap();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| v["event"] == "initialize")
            .expect("initialize logged")["data"]["clientCapabilities"]
            .clone()
    }
}

/// Runs `script` as one turn; returns its updates.
async fn turn(manager: &SessionManager, id: &SessionId, script: &str) -> Vec<SessionUpdate> {
    let since = manager.snapshot(id).unwrap().head;
    manager
        .prompt(id, vec![ContentBlock::text(script)])
        .await
        .unwrap();
    let mut feed = manager.updates(id, Some(since)).unwrap().events;
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while let Some(item) = feed.next().await {
            let event = item.unwrap().event;
            let done = matches!(event, SessionUpdate::TurnEnded { .. });
            out.push(event);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; got {out:?}"));
    out
}

/// What the agent said, one entry per line.
fn said(updates: &[SessionUpdate]) -> Vec<String> {
    let text: String = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    text.lines().map(str::to_owned).collect()
}

/// The fake agent's JSON reports.
fn reports(updates: &[SessionUpdate]) -> Vec<Value> {
    said(updates)
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_run_commands_through_client_terminals() {
    let env = Env::new();
    let manager = env.manager(true);
    let id = manager.create(env.params()).await.unwrap().id;
    let sub = env.root.join("sub");
    let script = format!(
        "terminal -- echo hi\n\
         terminal limit=4 -- printf abcdéfgh\n\
         terminal kill -- exec sleep 30\n\
         terminal env=GREETING=hey cwd={} -- echo $GREETING; pwd; exit 3\n\
         terminal argv -- printf %s argv",
        sub.display()
    );
    let updates = turn(&manager, &id, &script).await;
    assert_eq!(env.client_capabilities()["terminal"], true);
    let r = reports(&updates);
    assert_eq!(r.len(), 5, "{:?}", said(&updates));

    // Output and exit status; the id is dead after release.
    assert_eq!(r[0]["id"], "term-101");
    assert_eq!(r[0]["output"], "hi\n");
    assert_eq!(r[0]["exit_code"], 0);
    assert_eq!(r[0]["output_exit_code"], 0);
    assert_eq!(r[0]["truncated"], false);
    let after = r[0]["after_release"].as_str().unwrap();
    assert!(after.contains("unknown terminal"), "{after}");

    // The byte limit cuts from the start, at a character boundary.
    assert_eq!(r[1]["output"], "fgh");
    assert_eq!(r[1]["truncated"], true);

    // Killed: no exit code, a signal.
    assert_eq!(r[2]["exit_code"], Value::Null);
    assert_eq!(r[2]["signal"], "SIGKILL");

    // Environment and working directory as asked.
    assert_eq!(r[3]["output"], format!("hey\n{}\n", sub.display()));
    assert_eq!(r[3]["exit_code"], 3);

    // Program and arguments, not a command line.
    assert_eq!(r[4]["output"], "argv");

    let requests = env.procs.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 5);
    assert!(
        requests
            .iter()
            .all(|r| r.session == id && r.agent == "fake-acp")
    );
    assert_eq!(requests[0].cwd, env.root, "the session's cwd by default");
    assert_eq!(requests[3].env, [("GREETING".into(), "hey".into())]);
    assert_eq!(
        (requests[4].command.as_str(), requests[4].args.as_slice()),
        ("printf", ["%s".to_owned(), "argv".to_owned()].as_slice())
    );
    // Every one was released, and no tool call shows them: all freed.
    assert!(env.procs.running.lock().unwrap().is_empty());
    assert_eq!(env.procs.closed.lock().unwrap().len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminals_stay_inside_the_workspace() {
    let env = Env::new();
    let manager = env.manager(true);
    let id = manager.create(env.params()).await.unwrap().id;
    let script = format!(
        "terminal cwd={} -- echo outside\n\
         terminal cwd={}/../.. -- echo escape\n\
         terminal cwd=relative -- echo relative\n\
         terminal cwd={}/missing -- echo missing",
        env.dir.path().display(),
        env.root.display(),
        env.root.display(),
    );
    let lines = said(&turn(&manager, &id, &script).await);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines.iter().all(|l| l.starts_with("error:")), "{lines:?}");
    assert!(lines[0].contains("outside the workspace"), "{lines:?}");
    assert!(env.procs.requests.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_in_a_tool_call_is_shown_by_its_daemon_id() {
    let env = Env::new();
    let manager = env.manager(true);
    let id = manager.create(env.params()).await.unwrap().id;
    let updates = turn(&manager, &id, "terminal show -- echo shown").await;
    let r = reports(&updates);
    assert_eq!(r[0]["output"], "shown\n");

    // The transcript names the daemon terminal, not the ACP id.
    let shown = |c: &[ToolCallContent]| {
        c == [ToolCallContent::Terminal {
            terminal_id: "101".into(),
        }]
    };
    assert!(updates.iter().any(|u| matches!(
        u,
        SessionUpdate::ToolCall(tc) if shown(&tc.content)
    )));
    assert!(updates.iter().any(|u| matches!(
        u,
        SessionUpdate::ToolCallUpdate(tu) if tu.content.as_deref().is_some_and(shown)
    )));

    // Released, but kept for the transcript's link until the session
    // closes.
    assert!(env.procs.closed.lock().unwrap().is_empty());
    assert!(env.procs.killed.lock().unwrap().contains(&101));
    manager.close(&id).await.unwrap();
    assert_eq!(*env.procs.closed.lock().unwrap(), [101]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_session_frees_terminals_it_did_not_release() {
    let env = Env::new();
    let manager = env.manager(true);
    let id = manager.create(env.params()).await.unwrap().id;
    let r = reports(&turn(&manager, &id, "terminal keep -- echo kept").await);
    assert_eq!(r[0]["output"], "kept\n");
    assert_eq!(env.procs.running.lock().unwrap().len(), 1);
    manager.close(&id).await.unwrap();
    assert!(env.procs.running.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_backend_terminals_are_not_offered() {
    let env = Env::new();
    let manager = env.manager(false);
    let id = manager.create(env.params()).await.unwrap().id;
    let lines = said(&turn(&manager, &id, "terminal -- echo hi").await);
    assert_ne!(env.client_capabilities()["terminal"], true);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("error:"), "{lines:?}");
}
