// SPDX-License-Identifier: Apache-2.0

//! A smoke test against a real ACP agent, run by hand (no prompt, so no
//! model usage):
//!
//! ```sh
//! COQO_ACP_AGENT=<dir>/node_modules/.bin/claude-agent-acp \
//! COQO_ACP_CLAUDE=$(realpath "$(command -v claude)") \
//!   cargo test -p coqo-acp-host --test real_agent -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use coqo_acp_host::{AcpOptions, AcpProvider, AgentCommand};
use coqo_agents::model::*;
use coqo_agents::{ManagerOptions, SessionManager, SessionStore, StaticProviders};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ACP agent in COQO_ACP_AGENT"]
async fn a_real_agent_starts_a_session() {
    let Some(agent) = std::env::var_os("COQO_ACP_AGENT") else {
        eprintln!("COQO_ACP_AGENT is not set; skipping");
        return;
    };
    let mut command = AgentCommand::new(agent);
    if let Some(claude) = std::env::var_os("COQO_ACP_CLAUDE") {
        command = command.env("CLAUDE_CODE_EXECUTABLE", claude);
    }
    // Claude Code session markers inherited from a developer's shell turn
    // transcript saving off (ADR 0005).
    for (key, _) in std::env::vars() {
        if key == "CLAUDECODE"
            || (key.starts_with("CLAUDE_CODE_") && key != "CLAUDE_CODE_EXECUTABLE")
        {
            command = command.env_remove(key);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let info = AgentProviderInfo {
        id: "real".parse().unwrap(),
        name: "Real ACP agent".into(),
        kind: AgentProviderKind::Acp,
        description: None,
    };
    let provider = AcpProvider::command(command, AcpOptions::new("real-acp"));
    let providers = StaticProviders::new().with(info, Arc::new(provider));
    let store = SessionStore::open(&dir.path().join("coqo.db")).unwrap();
    let manager =
        SessionManager::open(store, Arc::new(providers), ManagerOptions::default()).unwrap();
    let started = std::time::Instant::now();
    let session = manager
        .create(CreateSessionParams {
            provider: "real".into(),
            cwd: cwd.display().to_string(),
            workspace: None,
            title: None,
            mode: None,
        })
        .await
        .unwrap();
    eprintln!("session ready in {:?}", started.elapsed());
    let snap = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snap = manager.snapshot(&session.id).unwrap();
            if !snap.available_commands.is_empty() && snap.auth.is_some() {
                return snap;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("available commands and an auth status");
    let modes = snap.modes.expect("modes");
    eprintln!(
        "modes: {:?} (current {})",
        modes
            .available_modes
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        modes.current_mode_id
    );
    eprintln!("commands: {}", snap.available_commands.len());
    eprintln!("auth: {:?}", snap.auth);
    for option in &snap.config_options {
        let values = match &option.kind {
            ConfigOptionKind::Select { options, .. } => options
                .iter()
                .map(|e| format!("{} ({})", e.value, e.name))
                .collect::<Vec<_>>()
                .join(", "),
            ConfigOptionKind::Boolean { .. } => "on/off".into(),
        };
        eprintln!(
            "option {} \"{}\" [{}]: {} of {values}",
            option.id,
            option.name,
            option.category.as_deref().unwrap_or("-"),
            option.value_name(),
        );
    }
    // Change the effort, if the agent offers one, to another value: a
    // `session/set_config_option`, still no prompt.
    let effort = snap
        .config_options
        .iter()
        .find(|o| o.is(config_category::THOUGHT_LEVEL));
    if let Some(effort) = effort
        && let ConfigOptionKind::Select {
            current_value,
            options,
        } = &effort.kind
        && let Some(other) = options.iter().find(|e| &e.value != current_value)
    {
        manager
            .set_config_option(
                &session.id,
                effort.id.clone(),
                ConfigValue::Value(other.value.clone()),
            )
            .await
            .unwrap();
        let after = manager.snapshot(&session.id).unwrap();
        let now = after
            .config_options
            .iter()
            .find(|o| o.id == effort.id)
            .map(SessionConfigOption::value_name);
        eprintln!("set {} to {}: now {now:?}", effort.id, other.value);
        assert_eq!(now.as_deref(), Some(other.name.as_str()));
    }
    manager.close(&session.id).await.unwrap();
}
