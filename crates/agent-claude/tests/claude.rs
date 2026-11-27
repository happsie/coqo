// SPDX-License-Identifier: Apache-2.0

//! The Claude provider end to end, with stand-ins: shell scripts for
//! `claude`, `node` and `npm`, and the scripted ACP agent
//! (`coqo-fake-claude-acp`) as the adapter that "npm" installs. No network,
//! no real Claude Code.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use coqo_agent_claude::{
    ADAPTER_VERSION, ClaudeOptions, ClaudeProvider, INSTALL_ACTION, PROVIDER_ID, plugin,
    provider_info,
};
use coqo_agents::model::*;
use coqo_agents::{
    AgentError, ManagerOptions, ProviderError, SessionManager, SessionStore, StaticProviders,
};
use futures_util::StreamExt;
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);
const FAKE: &str = env!("CARGO_BIN_EXE_coqo-fake-claude-acp");
/// Set in the re-run of this test binary that has Claude Code session
/// markers in its environment.
const INNER: &str = "COQO_CLAUDE_TEST_INNER";

struct Env {
    dir: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    claude_real: PathBuf,
    data: PathBuf,
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Env {
    /// `claude` (a symlink into `versions/`), `node` and `npm` in `bin/`.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (root, bin, versions, data) = (
            base.join("ws"),
            base.join("bin"),
            base.join("versions"),
            base.join("data"),
        );
        for d in [&root, &bin, &versions, &data] {
            std::fs::create_dir_all(d).unwrap();
        }
        let claude_real = versions.join("2.1.0");
        script(&claude_real, "#!/bin/sh\nexit 0\n");
        std::os::unix::fs::symlink(&claude_real, bin.join("claude")).unwrap();
        let log = base.join("agent.log");
        // `node --version`, or run the adapter script (the fake agent) with
        // the fake agent's switches.
        script(
            &bin.join("node"),
            &format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = --version ]; then echo v24.1.0; exit 0; fi\n\
                 export FAKE_ACP_LOG='{log}'\n\
                 if [ -f '{base}/signed-out' ]; then export FAKE_ACP_SIGNED_OUT=1; fi\n\
                 exec \"$@\"\n",
                log = log.display(),
                base = base.display(),
            ),
        );
        // `npm install --prefix <dir> … <pkg>@<ver>`: logs its arguments and
        // "installs" the fake agent as the adapter.
        script(
            &bin.join("npm"),
            &format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> '{base}/npm.log'\n\
                 prefix=$3\n\
                 pkg=\"$prefix/node_modules/@agentclientprotocol/claude-agent-acp\"\n\
                 mkdir -p \"$pkg\" \"$prefix/node_modules/.bin\"\n\
                 echo '{{\"version\":\"{ADAPTER_VERSION}\"}}' > \"$pkg/package.json\"\n\
                 ln -sf '{FAKE}' \"$prefix/node_modules/.bin/claude-agent-acp\"\n",
                base = base.display(),
            ),
        );
        Self {
            dir,
            root,
            bin,
            claude_real,
            data,
        }
    }

    fn base(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }

    fn manager(&self, plan_logins: bool) -> SessionManager {
        self.manager_with(|o| o.plan_logins = plan_logins)
    }

    fn manager_with(&self, change: impl FnOnce(&mut ClaudeOptions)) -> SessionManager {
        let mut options = ClaudeOptions::new(&self.data);
        // The stand-ins first; the system's for the scripts' own tools.
        let path = std::env::join_paths([self.bin.as_path(), "/usr/bin".as_ref(), "/bin".as_ref()]);
        options.path = Some(path.unwrap());
        options.acp.start_timeout = TIMEOUT;
        options.acp.request_timeout = TIMEOUT;
        change(&mut options);
        let provider = ClaudeProvider::new(options);
        let providers = StaticProviders::new().with(provider_info(), Arc::new(provider));
        let store = SessionStore::open(&self.base().join("coqo.db")).unwrap();
        SessionManager::open(store, Arc::new(providers), ManagerOptions::default()).unwrap()
    }

    fn params(&self) -> CreateSessionParams {
        CreateSessionParams {
            provider: PROVIDER_ID.into(),
            cwd: self.root.display().to_string(),
            workspace: Some("ws-1".into()),
            title: None,
            mode: None,
        }
    }

    fn npm_log(&self) -> String {
        std::fs::read_to_string(self.base().join("npm.log")).unwrap_or_default()
    }

    fn agent_starts(&self) -> Vec<Value> {
        self.logged("start")
    }

    /// The fake agent's log entries of `event` (a request's params).
    fn logged(&self, event: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(self.base().join("agent.log")).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["event"] == event)
            .map(|v| v["data"].clone())
            .collect()
    }
}

fn claude(manager: &SessionManager) -> ProviderInfo {
    manager
        .providers()
        .into_iter()
        .find(|p| p.id == PROVIDER_ID)
        .unwrap()
}

async fn said(manager: &SessionManager, id: &SessionId, since: u64) -> String {
    let mut feed = manager.updates(id, Some(since)).unwrap().events;
    let mut text = String::new();
    tokio::time::timeout(TIMEOUT, async {
        while let Some(item) = feed.next().await {
            match item.unwrap().event {
                SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text { text: t },
                } => text.push_str(&t),
                SessionUpdate::TurnEnded { .. } => return,
                _ => {}
            }
        }
    })
    .await
    .expect("the turn ends");
    text
}

/// Re-runs `test` in a child test process whose environment has Claude Code
/// session markers, as when coqod starts inside a Claude Code session.
fn with_session_markers(test: &str) -> bool {
    if std::env::var_os(INNER).is_some() {
        return false;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(INNER, "1")
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_CHILD_SESSION", "1")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env("CLAUDE_CODE_EXECUTABLE", "/somewhere/else/claude")
        .env("CLAUDE_CODE_USE_BEDROCK", "0")
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed with session markers set");
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_on_request_then_chat_with_the_users_claude() {
    if with_session_markers("install_on_request_then_chat_with_the_users_claude") {
        return;
    }
    let env = Env::new();
    let manager = env.manager(false);

    // Not installed: not available, with the install button and a sign-in
    // terminal. Nothing is installed until asked.
    let info = claude(&manager);
    assert!(!info.available);
    let status = info.status.expect("a status");
    assert!(!status.ready);
    assert_eq!(status.actions[0].id, INSTALL_ACTION);
    assert_eq!(
        status.sign_in.unwrap().run,
        ProviderActionRun::Terminal {
            program: env.bin.join("claude").display().to_string(),
            args: vec![]
        }
    );
    let err = manager.create(env.params()).await.unwrap_err();
    assert!(
        matches!(&err, AgentError::Provider(ProviderError::Failed(m)) if m.contains("adapter")),
        "{err:?}"
    );
    assert_eq!(env.npm_log(), "");

    // The user asks: npm installs the pinned adapter without optional deps.
    let info = manager
        .run_action(PROVIDER_ID, INSTALL_ACTION.into())
        .await
        .unwrap();
    assert!(info.available, "{info:?}");
    assert!(info.status.unwrap().ready);
    let npm = env.npm_log();
    assert!(npm.contains(&format!(
        "install --prefix {}",
        env.data.join("claude-acp").display()
    )));
    assert!(npm.contains("--omit=optional"), "{npm}");
    assert!(npm.contains(&format!(
        "@agentclientprotocol/claude-agent-acp@{ADAPTER_VERSION}"
    )));

    let session = manager.create(env.params()).await.unwrap();
    let snap = manager.snapshot(&session.id).unwrap();
    assert!(snap.modes.is_some());
    manager
        .prompt(&session.id, vec![ContentBlock::text("say pong")])
        .await
        .unwrap();
    assert_eq!(said(&manager, &session.id, snap.head).await, "pong");

    // The adapter ran the user's claude, with no session markers, and with
    // plan logins hidden as the setting asks.
    let starts = env.agent_starts();
    assert_eq!(starts.len(), 1, "{starts:?}");
    let start = &starts[0];
    assert_eq!(start["args"], serde_json::json!(["--hide-claude-auth"]));
    let vars = &start["env"];
    assert_eq!(
        vars["CLAUDE_CODE_EXECUTABLE"],
        env.claude_real.display().to_string()
    );
    for marker in [
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_ENTRYPOINT",
    ] {
        assert!(vars.get(marker).is_none(), "{marker} leaked: {vars}");
    }
    assert_eq!(
        vars["CLAUDE_CODE_USE_BEDROCK"], "0",
        "configuration is kept"
    );

    // Installing again is a no-op.
    manager
        .run_action(PROVIDER_ID, INSTALL_ACTION.into())
        .await
        .unwrap();
    assert_eq!(env.npm_log().lines().count(), 1);
    manager.close(&session.id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_out_claude_asks_for_a_login_in_its_own_terminal() {
    let env = Env::new();
    std::fs::write(env.base().join("signed-out"), "").unwrap();
    let manager = env.manager(true);
    manager
        .run_action(PROVIDER_ID, INSTALL_ACTION.into())
        .await
        .unwrap();

    let err = manager.create(env.params()).await.unwrap_err();
    assert!(
        matches!(err, AgentError::Provider(ProviderError::AuthRequired(_))),
        "{err:?}"
    );
    let sign_in = claude(&manager).status.unwrap().sign_in.unwrap();
    assert_eq!(sign_in.label, "Open `claude` in a terminal");
    // With plan logins on, the adapter offers them.
    let starts = env.agent_starts();
    assert_eq!(starts[0]["args"], serde_json::json!([]));

    let err = manager
        .run_action(PROVIDER_ID, "nope".into())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        AgentError::Provider(ProviderError::Unsupported(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_load_coqos_plugin_from_the_data_directory() {
    let env = Env::new();
    let manager = env.manager(true);
    manager
        .run_action(PROVIDER_ID, INSTALL_ACTION.into())
        .await
        .unwrap();
    let session = manager.create(env.params()).await.unwrap();

    // The plugin is in Coqo's data directory, not in the user's Claude
    // Code configuration, and the session names it as a local plugin.
    let dir = env.data.join("claude-plugins").join(plugin::PLUGIN_NAME);
    for (path, contents) in plugin::PLUGIN_FILES {
        assert_eq!(std::fs::read_to_string(dir.join(path)).unwrap(), *contents);
    }
    let created = env.logged("session/new");
    assert_eq!(
        created[0]["_meta"],
        serde_json::json!({"claudeCode": {"options": {"plugins": [
            {"type": "local", "path": dir.display().to_string()}
        ]}}})
    );
    manager.close(&session.id).await.unwrap();

    // Turned off: no plugin.
    let env = Env::new();
    let manager = env.manager_with(|o| o.plugin = false);
    manager
        .run_action(PROVIDER_ID, INSTALL_ACTION.into())
        .await
        .unwrap();
    let session = manager.create(env.params()).await.unwrap();
    let created = env.logged("session/new");
    assert!(created[0].get("_meta").is_none(), "{}", created[0]);
    assert!(!env.data.join("claude-plugins").exists());
    manager.close(&session.id).await.unwrap();
}
