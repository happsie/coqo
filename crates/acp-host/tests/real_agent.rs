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
            from: None,
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

/// The command for the real agent in `COQO_ACP_AGENT`, without the Claude
/// Code session markers of a developer's shell.
fn real_command() -> Option<AgentCommand> {
    let agent = std::env::var_os("COQO_ACP_AGENT")?;
    let mut command = AgentCommand::new(agent);
    if let Some(claude) = std::env::var_os("COQO_ACP_CLAUDE") {
        command = command.env("CLAUDE_CODE_EXECUTABLE", claude);
    }
    for (key, _) in std::env::vars() {
        if key == "CLAUDECODE"
            || (key.starts_with("CLAUDE_CODE_") && key != "CLAUDE_CODE_EXECUTABLE")
        {
            command = command.env_remove(key);
        }
    }
    Some(command)
}

/// A session manager over the database at `db`, with its own agent process.
fn real_manager(command: AgentCommand, db: &std::path::Path) -> SessionManager {
    let info = AgentProviderInfo {
        id: "real".parse().unwrap(),
        name: "Real ACP agent".into(),
        kind: AgentProviderKind::Acp,
        description: None,
    };
    let provider = AcpProvider::command(command, AcpOptions::new("real-acp"));
    let providers = StaticProviders::new().with(info, Arc::new(provider));
    let store = SessionStore::open(db).unwrap();
    SessionManager::open(store, Arc::new(providers), ManagerOptions::default()).unwrap()
}

/// Prompts `text` and returns what the agent said, once the turn ended.
async fn ask(manager: &SessionManager, db: &std::path::Path, id: &SessionId, text: &str) -> String {
    let started = manager
        .prompt(id, vec![ContentBlock::text(text)])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(180), async {
        while manager.snapshot(id).unwrap().running {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the turn ends");
    let store = SessionStore::open(db).unwrap();
    store
        .updates_since(id, started.seq, 10_000)
        .unwrap()
        .into_iter()
        .filter_map(|(_, u)| match u {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
            } => Some(text),
            _ => None,
        })
        .collect()
}

/// Titles, and resuming a session after a daemon restart, against a real
/// agent. It sends two short prompts, so it uses the model:
///
/// ```sh
/// COQO_ACP_AGENT=<dir>/node_modules/.bin/claude-agent-acp \
///   cargo test -p coqo-acp-host --test real_agent resumes -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ACP agent in COQO_ACP_AGENT; uses the model"]
async fn a_real_agent_titles_and_resumes_after_a_restart() {
    let Some(command) = real_command() else {
        eprintln!("COQO_ACP_AGENT is not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let db = dir.path().join("coqo.db");
    let id = {
        let manager = real_manager(command.clone(), &db);
        let session = manager
            .create(CreateSessionParams {
                provider: "real".into(),
                cwd: cwd.display().to_string(),
                workspace: None,
                title: None,
                mode: None,
                from: None,
            })
            .await
            .unwrap();
        let said = ask(
            &manager,
            &db,
            &session.id,
            "Remember the code word PELICAN. Reply with just: noted",
        )
        .await;
        eprintln!("first reply: {said:?}");
        // Claude Code sends its own title during the first turn, in place of
        // the one derived from the prompt.
        let title = manager.snapshot(&session.id).unwrap().info.title;
        eprintln!("title after the first turn: {title:?}");
        manager.shutdown().await;
        session.id
    };
    // A new manager with a new agent process on the same database: a
    // daemon restart. The next prompt resumes the agent's session.
    let manager = real_manager(command, &db);
    let said = ask(
        &manager,
        &db,
        &id,
        "What was the code word? Reply with just the word.",
    )
    .await;
    let snap = manager.snapshot(&id).unwrap();
    eprintln!(
        "after the restart: {said:?} (state {:?}, title {:?})",
        snap.info.state, snap.info.title
    );
    assert!(
        said.to_uppercase().contains("PELICAN"),
        "the resumed session remembers: {said:?}"
    );
    manager.close(&id).await.unwrap();
}

/// A session the agent no longer has ends, rather than restart behind the
/// transcript. It sends one short prompt:
///
/// ```sh
/// COQO_ACP_AGENT=<dir>/node_modules/.bin/claude-agent-acp \
///   cargo test -p coqo-acp-host --test real_agent lost -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ACP agent in COQO_ACP_AGENT; uses the model"]
async fn a_real_agent_that_lost_the_session_ends_it() {
    let Some(command) = real_command() else {
        eprintln!("COQO_ACP_AGENT is not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let db = dir.path().join("coqo.db");
    let id = {
        let manager = real_manager(command.clone(), &db);
        let session = manager
            .create(CreateSessionParams {
                provider: "real".into(),
                cwd: cwd.display().to_string(),
                workspace: None,
                title: None,
                mode: None,
                from: None,
            })
            .await
            .unwrap();
        ask(&manager, &db, &session.id, "Reply with just: ok").await;
        manager.shutdown().await;
        session.id
    };
    // The agent's session id now names a session it never had.
    let store = SessionStore::open(&db).unwrap();
    let mut stored = store.get(&id).unwrap().unwrap();
    stored.provider_session = Some(uuid_like());
    store
        .save(&stored, store.change_head().unwrap() + 1)
        .unwrap();
    drop(store);

    let manager = real_manager(command, &db);
    let err = manager
        .prompt(&id, vec![ContentBlock::text("Hello again")])
        .await
        .unwrap_err();
    eprintln!("the second prompt: {err}");
    assert!(
        matches!(err, coqo_agents::AgentError::Ended { .. }),
        "{err:?}"
    );
    let snap = manager.snapshot(&id).unwrap();
    eprintln!("state {:?}: {:?}", snap.info.state, snap.info.state_reason);
}

/// A session id no agent has.
fn uuid_like() -> String {
    "00000000-0000-4000-8000-000000000067".into()
}

/// A new session of the real agent in a fresh directory.
async fn real_session(manager: &SessionManager, cwd: &std::path::Path) -> SessionId {
    manager
        .create(CreateSessionParams {
            provider: "real".into(),
            cwd: cwd.display().to_string(),
            workspace: None,
            title: None,
            mode: None,
            from: None,
        })
        .await
        .unwrap()
        .id
}

/// Runs the turn `started` of session `id` to its end, answering each
/// pending request with `answer` (`None`: cancel it). Returns the
/// requests asked.
async fn run_turn(
    manager: &SessionManager,
    id: &SessionId,
    mut answer: impl FnMut(&PermissionRequest) -> Option<PermissionOutcome>,
) -> Vec<PermissionRequest> {
    let mut asked = Vec::new();
    tokio::time::timeout(Duration::from_secs(240), async {
        loop {
            let snap = manager.snapshot(id).unwrap();
            for p in &snap.pending_permissions {
                eprintln!(
                    "asked (state {:?}, {:?}): {p:#?}",
                    snap.info.state, snap.info.state_reason
                );
                let outcome = answer(p).unwrap_or(PermissionOutcome::Cancelled);
                manager.respond_permission(id, p.id, outcome).unwrap();
                asked.push((p.clone(), snap.info.state));
            }
            if !snap.running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the turn ends");
    asked
        .into_iter()
        .map(|(p, state)| {
            let want = if p.is_question() {
                AgentState::NeedsInput
            } else {
                AgentState::NeedsPermission
            };
            assert_eq!(
                state, want,
                "the session waits while {:?} is asked",
                p.title
            );
            p
        })
        .collect()
}

/// Claude Code's own `AskUserQuestion` reaches Coqo as a form
/// elicitation, and the answer continues the turn (#339, ADR 0029). One
/// prompt:
///
/// ```sh
/// COQO_ACP_AGENT=<dir>/node_modules/.bin/claude-agent-acp \
///   cargo test -p coqo-acp-host --test real_agent ask_user_question -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ACP agent in COQO_ACP_AGENT; uses the model"]
async fn a_real_agent_asks_with_ask_user_question() {
    let Some(command) = real_command() else {
        eprintln!("COQO_ACP_AGENT is not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let db = dir.path().join("coqo.db");
    let manager = real_manager(command, &db);
    let id = real_session(&manager, &cwd).await;
    let started = manager
        .prompt(
            &id,
            vec![ContentBlock::text(
                "Call your AskUserQuestion tool once, with two questions: which colour I like \
                 (options Red and Blue) and which pets I like (multi-select, options Cat and \
                 Dog). Then reply with one line listing exactly the answers you got, nothing else.",
            )],
        )
        .await
        .unwrap();
    let asked = run_turn(&manager, &id, |p| {
        let answers = p
            .questions
            .iter()
            .map(|q| match q.kind {
                AgentQuestionKind::Multi => QuestionAnswer {
                    id: q.id.clone(),
                    picks: vec![q.options[0].value.clone()],
                    text: q.other.then(|| "Axolotl".to_owned()),
                },
                _ => QuestionAnswer {
                    id: q.id.clone(),
                    picks: vec![q.options[0].value.clone()],
                    text: None,
                },
            })
            .collect();
        Some(PermissionOutcome::Answered { answers })
    })
    .await;
    let said = text_since(&db, &id, started.seq);
    eprintln!("said: {said:?}");
    let question = asked
        .iter()
        .find(|p| p.is_question())
        .expect("AskUserQuestion is asked as a question");
    assert_eq!(question.questions.len(), 2, "{question:#?}");
    assert!(question.questions.iter().all(|q| q.other));
    let first = &question.questions[0].options[0].label;
    assert!(said.contains(first.as_str()), "{said:?} has {first:?}");
    assert!(said.contains("Axolotl"), "{said:?} has the typed pet");
    manager.close(&id).await.unwrap();
}

/// Plan mode's approval (`ExitPlanMode`) reaches Coqo as a permission
/// request with the plan and the adapter's options; rejecting it keeps
/// Claude planning (#339). One prompt:
///
/// ```sh
/// COQO_ACP_AGENT=<dir>/node_modules/.bin/claude-agent-acp \
///   cargo test -p coqo-acp-host --test real_agent approve_a_plan -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ACP agent in COQO_ACP_AGENT; uses the model"]
async fn a_real_agent_asks_to_approve_a_plan() {
    let Some(command) = real_command() else {
        eprintln!("COQO_ACP_AGENT is not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let db = dir.path().join("coqo.db");
    let manager = real_manager(command, &db);
    let id = real_session(&manager, &cwd).await;
    manager.set_mode(&id, "plan".into()).await.unwrap();
    let started = manager
        .prompt(
            &id,
            vec![ContentBlock::text(
                "Plan creating a file hello.txt that contains the word hi. The folder is empty, \
                 so do not explore: present the one-step plan for approval right away.",
            )],
        )
        .await
        .unwrap();
    let asked = run_turn(&manager, &id, |p| {
        let reject = p
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::RejectOnce)?;
        Some(PermissionOutcome::Selected {
            option_id: reject.id.clone(),
        })
    })
    .await;
    eprintln!("said: {:?}", text_since(&db, &id, started.seq));
    let plan = asked
        .iter()
        .find(|p| p.tool_call.as_ref().and_then(|t| t.kind) == Some(ToolKind::SwitchMode))
        .expect("the plan approval is asked");
    assert!(!plan.is_question());
    assert!(
        plan.options
            .iter()
            .any(|o| o.kind == PermissionOptionKind::RejectOnce)
    );
    assert!(
        !cwd.join("hello.txt").exists(),
        "a rejected plan writes nothing"
    );
    manager.close(&id).await.unwrap();
}

/// The agent's text since `seq`.
fn text_since(db: &std::path::Path, id: &SessionId, seq: u64) -> String {
    SessionStore::open(db)
        .unwrap()
        .updates_since(id, seq, 10_000)
        .unwrap()
        .into_iter()
        .filter_map(|(_, u)| match u {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
            } => Some(text),
            _ => None,
        })
        .collect()
}
