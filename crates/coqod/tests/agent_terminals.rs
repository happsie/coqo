// SPDX-License-Identifier: Apache-2.0

//! ACP agents' `terminal/*` commands in the daemon's PTYs (issue #185): the
//! scripted fake ACP agent through the ACP host, the session manager and
//! [`AgentTerminalService`], as the daemon wires them. No network.

use std::sync::Arc;
use std::time::Duration;

use coqo_acp_host::{AcpOptions, AcpProvider, AgentCommand, TerminalHandle};
use coqo_agents::model::*;
use coqo_agents::{ManagerOptions, SessionManager, SessionStore, StaticProviders};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::methods::terminal::{TerminalId, TerminalStatus};
use coqod::terminal::{AgentTerminalService, SessionTitle, Terminals};
use futures_util::StreamExt;
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);
const FAKE: &str = env!("CARGO_BIN_EXE_coqod-fake-acp-agent");

struct Env {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    terminals: Terminals,
    manager: SessionManager,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let terminals = Terminals::new();
        let handle = TerminalHandle::new();
        let mut options = AcpOptions::new("fake-acp");
        options.start_timeout = TIMEOUT;
        options.request_timeout = TIMEOUT;
        options.terminals = handle.clone();
        let provider = AcpProvider::command(AgentCommand::new(FAKE), options);
        let store = SessionStore::open(&dir.path().join("coqo.db")).unwrap();
        let info = AgentProviderInfo {
            id: "fake".parse().unwrap(),
            name: "Fake ACP".into(),
            kind: AgentProviderKind::Acp,
            description: None,
        };
        let providers = StaticProviders::new().with(info, Arc::new(provider));
        let manager =
            SessionManager::open(store, Arc::new(providers), ManagerOptions::default()).unwrap();
        // Set after the provider exists, as `routes::router` does.
        let sessions = manager.clone();
        let title: Arc<SessionTitle> = Arc::new(move |id| sessions.snapshot(id).ok()?.info.title);
        handle.set(Arc::new(AgentTerminalService::new(
            terminals.clone(),
            title,
        )));
        Self {
            _dir: dir,
            root,
            terminals,
            manager,
        }
    }

    async fn session(&self) -> SessionId {
        let params = CreateSessionParams {
            provider: "fake".into(),
            cwd: self.root.display().to_string(),
            workspace: Some("ws-1".into()),
            title: Some("Fix the build".into()),
            mode: None,
        };
        self.manager.create(params).await.unwrap().id
    }

    /// Starts `script` as a turn; returns the feed's start.
    async fn start(&self, id: &SessionId, script: &str) -> u64 {
        let since = self.manager.snapshot(id).unwrap().head;
        self.manager
            .prompt(id, vec![ContentBlock::text(script)])
            .await
            .unwrap();
        since
    }

    /// The updates from `since` up to the first that matches `stop`.
    async fn until(
        &self,
        id: &SessionId,
        since: u64,
        stop: impl Fn(&SessionUpdate) -> bool,
    ) -> Vec<SessionUpdate> {
        let mut feed = self.manager.updates(id, Some(since)).unwrap().events;
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
        })
        .await
        .unwrap_or_else(|_| panic!("timed out; got {out:?}"));
        out
    }

    async fn turn(&self, id: &SessionId, script: &str) -> Vec<SessionUpdate> {
        let since = self.start(id, script).await;
        self.until(id, since, |u| matches!(u, SessionUpdate::TurnEnded { .. }))
            .await
    }
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

fn reports(updates: &[SessionUpdate]) -> Vec<Value> {
    said(updates)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agents_command_runs_in_a_listed_daemon_terminal_until_released() {
    let env = Env::new();
    let id = env.session().await;

    let since = env.start(&id, "terminal -- echo hi; sleep 1").await;
    env.until(&id, since, |u| {
        matches!(u, SessionUpdate::AgentMessageChunk { content: ContentBlock::Text { text } } if text.starts_with("created"))
    })
    .await;
    // While it runs: in `terminal/list`, titled with the session.
    let listed = env.terminals.list();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].status, TerminalStatus::Running);
    assert_eq!(listed[0].title, "Fix the build: echo hi; sleep 1");
    assert_eq!(
        listed[0].cwd.as_deref(),
        Some(env.root.to_str().unwrap()),
        "the session's cwd"
    );

    let updates = env
        .until(&id, since, |u| matches!(u, SessionUpdate::TurnEnded { .. }))
        .await;
    let r = reports(&updates);
    assert_eq!(r.len(), 1, "{}", said(&updates));
    assert_eq!(r[0]["output"], "hi\n");
    assert_eq!(r[0]["exit_code"], 0);
    assert_eq!(r[0]["truncated"], false);
    // Released: gone from the list.
    assert!(env.terminals.list().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_is_plain_text_with_the_byte_limit_and_kill_reports_a_signal() {
    let env = Env::new();
    let id = env.session().await;
    let updates = env
        .turn(
            &id,
            "terminal -- printf '\\033[1;31mred\\033[0m %0300d\\n' 0\n\
             terminal limit=20 -- for i in $(seq 1 2000); do echo line $i; done\n\
             terminal kill -- sleep 30",
        )
        .await;
    let r = reports(&updates);
    assert_eq!(r.len(), 3, "{}", said(&updates));
    // No escape sequences; a line wider than the terminal is one line.
    assert_eq!(r[0]["output"], format!("red {}\n", "0".repeat(300)));
    assert_eq!(r[1]["output"], "line 1999\nline 2000\n");
    assert_eq!(r[1]["truncated"], true);
    assert_eq!(r[2]["exit_code"], Value::Null);
    assert!(
        r[2]["signal"]
            .as_str()
            .is_some_and(|s| s.starts_with("SIG")),
        "{}",
        r[2]
    );
    assert!(env.terminals.list().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shown_terminal_stays_for_the_transcript_until_the_session_closes() {
    let env = Env::new();
    let id = env.session().await;
    let updates = env.turn(&id, "terminal show -- echo shown").await;
    let listed = env.terminals.list();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(matches!(
        listed[0].status,
        TerminalStatus::Exited { code: Some(0) }
    ));
    // The transcript names the daemon terminal.
    let daemon_id = listed[0].id.0.to_string();
    assert!(updates.iter().any(|u| matches!(
        u,
        SessionUpdate::ToolCall(tc) if tc.content == [ToolCallContent::Terminal { terminal_id: daemon_id.clone() }]
    )));
    let term = env.terminals.get(TerminalId(listed[0].id.0)).unwrap();
    assert!(term.text().0.contains("shown"));

    env.manager.close(&id).await.unwrap();
    assert!(env.terminals.list().is_empty());
}
