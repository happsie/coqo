// SPDX-License-Identifier: Apache-2.0

//! The update routes through the real router: an in-process daemon is a
//! development build and never checks; `daemon/restartWhenIdle` waits for a
//! running agent turn, then shuts down, and the chat resumes in the next
//! daemon over the same data (ADR 0026).

use std::sync::Arc;
use std::time::Duration;

use coqo_agents::model::{
    ContentBlock, CreateSessionParams, PromptParams, SessionId, SessionParams, StopReason,
};
use coqo_agents::testing::{Call, ScriptedProvider, Step};
use coqo_protocol::daemon::{RestartParams, RestartResult, RestartWhenIdle};
use coqo_protocol::methods::agents as agm;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::update::{self as um, InstallParams, OffReason, UpdateState};
use coqo_protocol::methods::workspace as wsm;
use coqo_protocol::{AccessKind, Client, ClientConfig, ProtocolError, Server, ServerConfig};
use tokio::sync::Notify;

use crate::daemon::DaemonContext;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    ctx: DaemonContext,
    server: Server,
}

impl Daemon {
    async fn start(dir: &std::path::Path, agent: &ScriptedProvider) -> Self {
        let ctx = routes::tests::context_with(dir, agent.clone());
        ctx.mods.start().await;
        let mut config = ServerConfig::new("coqod", "test");
        config.authenticator = Some(Arc::new(ctx.auth.clone()));
        let server = Server::new(routes::router(&ctx).unwrap(), config);
        Self { ctx, server }
    }

    async fn ui(&self) -> Client {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(server_io).await });
        let mut config = ClientConfig::new("test", "test");
        config.token = Some(self.ctx.auth.daemon_token());
        Client::connect(client_io, config).await.unwrap().0
    }

    async fn stop(self) {
        self.ctx.shutdown.cancel();
        self.ctx.mods.shutdown().await;
    }
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_in_process_daemon_is_a_development_build_and_never_installs() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path(), &ScriptedProvider::new()).await;
    let ui = daemon.ui().await;
    let dev = UpdateState::Off {
        reason: OffReason::DevBuild,
        message: "Running a development build".into(),
    };
    let status = ui.request::<um::Status>(()).await.unwrap();
    assert_eq!(status.state, dev);
    assert_eq!(status.current, crate::daemon::VERSION);
    assert_eq!(status.restart, None);
    assert_eq!(ui.request::<um::Check>(()).await.unwrap().state, dev);
    let e = ui
        .request::<um::Install>(InstallParams {
            install: "/Applications/Coqo.app".into(),
        })
        .await
        .unwrap_err();
    let ProtocolError::Rpc(e) = e else {
        panic!("{e:?}")
    };
    assert_eq!(e.code, codes::UPDATE_FAILED);
    assert_eq!(e.message, "Running a development build");
    // Agents never update or restart Coqo.
    let routes = routes::router(&daemon.ctx).unwrap().routes();
    for name in [
        "update/status",
        "update/check",
        "update/install",
        "daemon/restartWhenIdle",
    ] {
        let route = routes.iter().find(|r| r.name == name).unwrap();
        assert_eq!(route.access, Some(AccessKind::Daemon), "{name}");
    }
    daemon.stop().await;
}

/// Acceptance: the daemon waits for a running turn before it restarts, and
/// the chat resumes in the daemon that comes next.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarts_only_once_the_running_turn_ends_and_the_chat_resumes() {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let go = Arc::new(Notify::new());
    let agent = ScriptedProvider::new()
        .turn([Step::Wait(Arc::clone(&go)), Step::Stop(StopReason::EndTurn)]);
    let daemon = Daemon::start(data.path(), &agent).await;
    let ui = daemon.ui().await;
    let ws = ui
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(project.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    let id: SessionId = ui
        .request::<agm::Create>(CreateSessionParams {
            provider: "fake".into(),
            cwd: project.path().to_str().unwrap().into(),
            workspace: Some(ws.id.0.clone()),
            title: Some("Long task".into()),
            mode: None,
            from: None,
        })
        .await
        .unwrap()
        .id;
    ui.request::<agm::Prompt>(PromptParams {
        session: id.clone(),
        content: vec![ContentBlock::text("go")],
    })
    .await
    .unwrap();
    let activity = daemon.ctx.activity.clone();
    until("the turn runs", || activity.running_turns() == 1).await;

    let answer = ui
        .request::<RestartWhenIdle>(RestartParams { now: false })
        .await
        .unwrap();
    assert_eq!(
        answer,
        RestartResult {
            running: 1,
            restarting: false
        }
    );
    let status = ui.request::<um::Status>(()).await.unwrap();
    assert_eq!(status.restart.map(|r| r.running), Some(1));
    // Asking again only reports.
    let again = ui
        .request::<RestartWhenIdle>(RestartParams::default())
        .await
        .unwrap();
    assert_eq!(again.running, 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!daemon.ctx.shutdown.is_cancelled(), "a turn still runs");

    // The turn ends: the daemon shuts down.
    go.notify_one();
    let shutdown = daemon.ctx.shutdown.clone();
    until("the daemon restarts", || shutdown.is_cancelled()).await;
    daemon.stop().await;

    // The next daemon, over the same data, resumes the chat.
    let next_agent = ScriptedProvider::new().turn([Step::Stop(StopReason::EndTurn)]);
    let next = Daemon::start(data.path(), &next_agent).await;
    let ui = next.ui().await;
    let snapshot = ui
        .request::<agm::Get>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(snapshot.info.title.as_deref(), Some("Long task"));
    ui.request::<agm::Prompt>(PromptParams {
        session: id.clone(),
        content: vec![ContentBlock::text("and now?")],
    })
    .await
    .unwrap();
    until("the session resumes on the agent", || {
        next_agent
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Resume(s) if s == "fake-1"))
    })
    .await;
    until("the prompt reaches it", || {
        next_agent
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Prompt(_)))
    })
    .await;
    next.stop().await;
}

#[tokio::test]
async fn restart_now_does_not_wait() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path(), &ScriptedProvider::new()).await;
    let _busy = daemon
        .ctx
        .activity
        .busy(format!("{}s-1", coqo_agents::TURN_BUSY_PREFIX));
    let _terminal = daemon.ctx.activity.busy("terminal 1 (zsh)");
    assert_eq!(
        daemon.ctx.activity.running_turns(),
        1,
        "terminals are not turns"
    );
    let ui = daemon.ui().await;
    let answer = ui
        .request::<RestartWhenIdle>(RestartParams { now: true })
        .await
        .unwrap();
    assert_eq!(
        answer,
        RestartResult {
            running: 1,
            restarting: true
        }
    );
    assert!(daemon.ctx.shutdown.is_cancelled());
    daemon.stop().await;
}
