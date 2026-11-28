// SPDX-License-Identifier: Apache-2.0

//! The alert engine on its own (dedupe, occurrences, resolving, read and
//! dismissed, persistence, settings, the changes feed), the agent sources'
//! rules, mods' alerts through `coqo.alerts@1`, and the whole path through
//! the real router: a scripted agent's permission request, question,
//! finished turn and failure.

use std::sync::Arc;
use std::time::Duration;

use coqo_agents::PermissionAsk;
use coqo_agents::model::{
    AgentState, AgentStatus, ContentBlock, CreateSessionParams, PermissionOption,
    PermissionOptionKind, PermissionOutcome, PromptParams, RespondPermissionParams, SessionId,
    SessionInfo, SessionParams, StopReason,
};
use coqo_agents::testing::{ScriptedProvider, Step};
use coqo_kernel::ContributionRegistry;
use coqo_mod_api::alerts::{AlertLevel, AlertLink, Alerts as _, RaiseAlert};
use coqo_mod_api::contrib::AlertSource;
use coqo_mod_api::{ContributionPoint as _, ModError, points};
use coqo_protocol::methods::agents as agm;
use coqo_protocol::methods::alerts::{
    self as am, Alert, AlertKind, AlertTarget, ChangesParams, DismissAllParams, DismissParams,
    FileLocation, ListParams, MarkReadParams, SetSettingsParams,
};
use coqo_protocol::methods::editor::LineRange;
use coqo_protocol::methods::questions::{
    self as qm, AnswerParams, CreateParams, NewAnchor, QuestionId, QuestionKind,
};
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceId, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig, Server, ServerConfig, SessionToken, codes};
use futures_util::StreamExt as _;
use tokio::sync::Notify;

use super::agents::{self, session_alert};
use super::{AlertService, ModAlerts, NewAlert};
use crate::daemon::DaemonContext;
use crate::questions::OpenQuestion;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(10);

fn new_alert(source: &str, key: &str, kind: AlertKind, title: &str) -> NewAlert {
    NewAlert {
        source: source.into(),
        key: key.into(),
        workspace: Some(WorkspaceId("ws".into())),
        session: Some(SessionId(key.into())),
        kind,
        title: title.into(),
        body: None,
        target: Some(AlertTarget::Session {
            session: SessionId(key.into()),
        }),
    }
}

fn engine(dir: &std::path::Path) -> Arc<AlertService> {
    AlertService::open(&dir.join("coqo.db"), None).unwrap()
}

// ---- the engine ---------------------------------------------------------------

#[test]
fn raising_again_replaces_the_alert_and_a_new_kind_is_a_new_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let svc = engine(dir.path());
    let a = svc
        .raise(new_alert("agents.error", "s-1", AlertKind::Error, "Boom"))
        .unwrap()
        .unwrap();
    assert!(a.is_unread());
    assert_eq!(a.seq, 1);
    // The same again: nothing changes, no seq is spent.
    assert!(
        svc.raise(new_alert("agents.error", "s-1", AlertKind::Error, "Boom"))
            .unwrap()
            .is_none()
    );
    // Read, then a new text for the same kind and target: still read, same
    // occurrence.
    svc.mark_read(MarkReadParams {
        ids: vec![a.id],
        session: None,
    })
    .unwrap();
    let b = svc
        .raise(new_alert("agents.error", "s-1", AlertKind::Error, "Boom 2"))
        .unwrap()
        .unwrap();
    assert_eq!((b.id, b.title.as_str(), b.read), (a.id, "Boom 2", true));
    assert_eq!(b.created_at, a.created_at);
    // Another kind under the same key: one alert, unread again.
    let c = svc
        .raise(new_alert(
            "agents.error",
            "s-1",
            AlertKind::NeedsYou,
            "Help",
        ))
        .unwrap()
        .unwrap();
    assert_eq!(c.id, a.id);
    assert!(c.is_unread());
    // Another key is another alert.
    let d = svc
        .raise(new_alert("agents.error", "s-2", AlertKind::Error, "Boom"))
        .unwrap()
        .unwrap();
    assert_ne!(d.id, a.id);
    let listed = svc.list(ListParams::default()).unwrap();
    assert_eq!(listed.alerts.len(), 2);
    assert_eq!(listed.alerts[0].id, d.id, "newest first");
    assert_eq!(listed.seq, d.seq);
}

#[test]
fn a_new_target_is_a_new_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let svc = engine(dir.path());
    let mut q = new_alert("agents.question", "s-1", AlertKind::NeedsYou, "Asked");
    q.target = Some(AlertTarget::Question {
        session: SessionId("s-1".into()),
        question: QuestionId(1),
        location: None,
    });
    let a = svc.raise(q.clone()).unwrap().unwrap();
    svc.mark_read(MarkReadParams {
        ids: vec![a.id],
        session: None,
    })
    .unwrap();
    q.target = Some(AlertTarget::Question {
        session: SessionId("s-1".into()),
        question: QuestionId(2),
        location: None,
    });
    let b = svc.raise(q).unwrap().unwrap();
    assert_eq!(b.id, a.id);
    assert!(b.is_unread(), "the next question is news");
}

#[test]
fn resolving_reads_the_alert_and_raising_after_it_starts_over() {
    let dir = tempfile::tempdir().unwrap();
    let svc = engine(dir.path());
    assert!(svc.resolve("agents.done", "nobody").unwrap().is_none());
    let a = svc
        .raise(new_alert("agents.done", "s-1", AlertKind::Done, "Done"))
        .unwrap()
        .unwrap();
    let r = svc.resolve("agents.done", "s-1").unwrap().unwrap();
    assert!(r.read && r.resolved_at.is_some() && !r.is_active());
    assert!(svc.resolve("agents.done", "s-1").unwrap().is_none(), "once");
    // Still listed, as history.
    assert_eq!(
        svc.list(ListParams::default()).unwrap().alerts,
        std::slice::from_ref(&r)
    );
    let again = svc
        .raise(new_alert("agents.done", "s-1", AlertKind::Done, "Done"))
        .unwrap()
        .unwrap();
    assert_eq!(again.id, a.id);
    assert!(again.is_unread() && again.resolved_at.is_none());
}

#[test]
fn dismissed_alerts_stay_hidden_until_something_new() {
    let dir = tempfile::tempdir().unwrap();
    let svc = engine(dir.path());
    let a = svc
        .raise(new_alert(
            "agents.blocked",
            "s-1",
            AlertKind::NeedsYou,
            "Stuck",
        ))
        .unwrap()
        .unwrap();
    let b = svc
        .raise(new_alert("agents.error", "s-2", AlertKind::Error, "Boom"))
        .unwrap()
        .unwrap();
    let changed = svc.dismiss(DismissParams { ids: vec![a.id] }).unwrap();
    assert_eq!(changed.changed, [a.id]);
    assert!(
        svc.dismiss(DismissParams { ids: vec![a.id] })
            .unwrap()
            .changed
            .is_empty()
    );
    assert_eq!(
        svc.list(ListParams::default()).unwrap().alerts.len(),
        1,
        "hidden"
    );
    // A new reason for the same occurrence stays dismissed.
    let quiet = svc
        .raise(new_alert(
            "agents.blocked",
            "s-1",
            AlertKind::NeedsYou,
            "Still",
        ))
        .unwrap()
        .unwrap();
    assert!(quiet.dismissed);
    // Resolved and raised again: back.
    svc.resolve("agents.blocked", "s-1").unwrap();
    let back = svc
        .raise(new_alert(
            "agents.blocked",
            "s-1",
            AlertKind::NeedsYou,
            "Again",
        ))
        .unwrap()
        .unwrap();
    assert!(!back.dismissed && back.is_unread());
    // Dismiss all of a workspace.
    let mut other = new_alert("x.y", "k", AlertKind::Info, "Elsewhere");
    other.workspace = Some(WorkspaceId("other".into()));
    svc.raise(other).unwrap();
    let all = svc
        .dismiss_all(DismissAllParams {
            workspace: Some(WorkspaceId("ws".into())),
        })
        .unwrap();
    assert_eq!(all.changed.len(), 2, "{all:?}");
    assert!(all.changed.contains(&b.id));
    let left = svc.list(ListParams::default()).unwrap().alerts;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].title, "Elsewhere");
}

#[test]
fn marking_a_sessions_alerts_read() {
    let dir = tempfile::tempdir().unwrap();
    let svc = engine(dir.path());
    let a = svc
        .raise(new_alert(
            "agents.permission",
            "s-1",
            AlertKind::NeedsYou,
            "P",
        ))
        .unwrap()
        .unwrap();
    let b = svc
        .raise(new_alert(
            "agents.permission",
            "s-2",
            AlertKind::NeedsYou,
            "P",
        ))
        .unwrap()
        .unwrap();
    let changed = svc
        .mark_read(MarkReadParams {
            ids: vec![],
            session: Some(SessionId("s-1".into())),
        })
        .unwrap();
    assert_eq!(changed.changed, [a.id]);
    let listed = svc.list(ListParams::default()).unwrap().alerts;
    let unread: Vec<_> = listed
        .iter()
        .filter(|a| a.is_unread())
        .map(|a| a.id)
        .collect();
    assert_eq!(unread, [b.id]);
}

#[test]
fn alerts_and_settings_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let a = {
        let svc = engine(dir.path());
        let a = svc
            .raise(new_alert("agents.error", "s-1", AlertKind::Error, "Boom"))
            .unwrap()
            .unwrap();
        svc.set_settings(SetSettingsParams {
            do_not_disturb: Some(true),
            mute: vec!["agents.done".into()],
            unmute: vec![],
        })
        .unwrap();
        a
    };
    let svc = engine(dir.path());
    let listed = svc.list(ListParams::default()).unwrap();
    assert_eq!(listed.alerts, std::slice::from_ref(&a));
    assert_eq!(listed.seq, a.seq);
    let info = svc.get_settings();
    assert!(info.settings.do_not_disturb);
    assert_eq!(info.settings.muted, ["agents.done"]);
    let done = info.sources.iter().find(|s| s.id == "agents.done").unwrap();
    assert!(done.muted);
    assert_eq!(info.sources.len(), super::CORE_SOURCES.len());
    // A muted source raises nothing.
    assert!(
        svc.raise(new_alert("agents.done", "s-1", AlertKind::Done, "Done"))
            .unwrap()
            .is_none()
    );
    let info = svc
        .set_settings(SetSettingsParams {
            do_not_disturb: Some(false),
            mute: vec![],
            unmute: vec!["agents.done".into()],
        })
        .unwrap();
    assert!(!info.settings.do_not_disturb && info.settings.muted.is_empty());
    assert!(
        svc.raise(new_alert("agents.done", "s-1", AlertKind::Done, "Done"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn the_changes_feed_resumes_with_each_alerts_newest_state() {
    let dir = tempfile::tempdir().unwrap();
    let svc = engine(dir.path());
    let a = svc
        .raise(new_alert("agents.error", "s-1", AlertKind::Error, "Boom"))
        .unwrap()
        .unwrap();
    let mut other = new_alert("agents.error", "s-2", AlertKind::Error, "Other");
    other.workspace = Some(WorkspaceId("other".into()));
    svc.raise(other).unwrap();
    svc.resolve("agents.error", "s-1").unwrap();

    // From the first change: a's newest state only, and the other one.
    let mut feed = svc
        .changes(ChangesParams::default(), Some(a.seq - 1))
        .unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let e = tokio::time::timeout(TIMEOUT, feed.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seen.push((e.event.key.clone(), e.event.resolved_at.is_some(), e.seq));
    }
    assert_eq!(seen[0].0, "s-2");
    assert_eq!((seen[1].0.as_str(), seen[1].1), ("s-1", true));
    // Then live, filtered by workspace.
    let mut ws_feed = svc
        .changes(
            ChangesParams {
                workspace: Some(WorkspaceId("ws".into())),
            },
            None,
        )
        .unwrap();
    let mut elsewhere = new_alert("agents.done", "s-9", AlertKind::Done, "x");
    elsewhere.workspace = Some(WorkspaceId("other".into()));
    svc.raise(elsewhere).unwrap();
    let b = svc
        .raise(new_alert("agents.done", "s-3", AlertKind::Done, "Done"))
        .unwrap()
        .unwrap();
    let e = tokio::time::timeout(TIMEOUT, ws_feed.events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(e.event.id, b.id);
    // Ahead of the head is a gap.
    let gap = svc
        .changes(ChangesParams::default(), Some(b.seq + 5))
        .err()
        .unwrap();
    assert_eq!(gap.code, codes::RESUME_GAP);
}

// ---- the agent sources' rules ---------------------------------------------------

fn info(state: AgentState) -> SessionInfo {
    SessionInfo {
        id: SessionId("s-1".into()),
        provider: "fake".into(),
        workspace: Some("ws".into()),
        cwd: "/".into(),
        title: Some("Fix the flaky test".into()),
        state,
        state_reason: None,
        state_since: None,
        status: None,
        mode: None,
        closed: false,
        criteria: Vec::new(),
        tool_use: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn each_state_maps_to_its_source_kind_and_target() {
    let chat = Some(AlertTarget::Session {
        session: SessionId("s-1".into()),
    });
    assert!(session_alert(&info(AgentState::Working), "Claude", None, None).is_none());
    assert!(session_alert(&info(AgentState::Idle), "Claude", None, None).is_none());
    let mut closed = info(AgentState::Error);
    closed.closed = true;
    assert!(session_alert(&closed, "Claude", None, None).is_none());

    let p = session_alert(
        &info(AgentState::NeedsPermission),
        "Claude",
        Some("`cargo test`"),
        None,
    )
    .unwrap();
    assert_eq!(
        (p.source.as_str(), p.kind, p.title.as_str()),
        (
            agents::PERMISSION,
            AlertKind::NeedsYou,
            "Permission requested: cargo test"
        )
    );
    assert_eq!(p.body.as_deref(), Some("Fix the flaky test"));
    assert_eq!(p.target, chat);
    assert_eq!(p.workspace, Some(WorkspaceId("ws".into())));
    assert_eq!(p.key, "s-1");

    let question = OpenQuestion {
        id: QuestionId(4),
        body: "Which **map**?\n\nMore".into(),
        location: Some((RootId(0), "src/a.rs".into(), 3)),
    };
    let q = session_alert(
        &info(AgentState::NeedsInput),
        "Claude",
        None,
        Some(&question),
    )
    .unwrap();
    assert_eq!(
        (q.source.as_str(), q.kind),
        (agents::QUESTION, AlertKind::NeedsYou)
    );
    assert_eq!(q.title, "Claude asked a question");
    assert_eq!(q.body.as_deref(), Some("Which map?"));
    assert_eq!(
        q.target,
        Some(AlertTarget::Question {
            session: SessionId("s-1".into()),
            question: QuestionId(4),
            location: Some(FileLocation {
                root: RootId(0),
                path: "src/a.rs".into(),
                line: Some(3),
            }),
        })
    );
    // Without a question (a provider reported it), the chat.
    let mut waiting = info(AgentState::NeedsInput);
    waiting.state_reason = Some("Pick one".into());
    let q = session_alert(&waiting, "Claude", None, None).unwrap();
    assert_eq!(
        (q.body.as_deref(), q.target.clone()),
        (Some("Pick one"), chat.clone())
    );

    let mut blocked = info(AgentState::Blocked);
    blocked.state_reason = Some("Need the API key".into());
    let b = session_alert(&blocked, "Claude", None, None).unwrap();
    assert_eq!(
        (b.source.as_str(), b.kind),
        (agents::BLOCKED, AlertKind::NeedsYou)
    );
    assert_eq!(b.body.as_deref(), Some("Need the API key"));

    let mut failed = info(AgentState::Error);
    failed.state_reason = Some("npm ERR! ERESOLVE".into());
    let e = session_alert(&failed, "Claude", None, None).unwrap();
    assert_eq!(
        (e.source.as_str(), e.kind),
        (agents::ERROR, AlertKind::Error)
    );
    assert_eq!(e.title, "Claude failed");

    let mut done = info(AgentState::Done);
    done.status = Some(AgentStatus {
        text: "Tests pass".into(),
        state: None,
        progress: None,
        turn: None,
        updated_at: 0,
    });
    let d = session_alert(&done, "Claude", None, None).unwrap();
    assert_eq!((d.source.as_str(), d.kind), (agents::DONE, AlertKind::Done));
    assert_eq!(
        (d.title.as_str(), d.body.as_deref()),
        ("Claude finished", Some("Tests pass"))
    );
}

// ---- mods ---------------------------------------------------------------------

#[test]
fn mods_raise_alerts_of_the_sources_they_contribute() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ContributionRegistry::new());
    registry.declare::<points::AlertSources>().unwrap();
    registry
        .contribute(
            &points::AlertSources::id(),
            "test.ci".parse().unwrap(),
            "ci.checks".parse().unwrap(),
            AlertSource {
                id: "ci.checks".parse().unwrap(),
                name: "CI checks".into(),
                description: None,
            },
        )
        .unwrap();
    let svc = AlertService::open(&dir.path().join("coqo.db"), Some(Arc::clone(&registry))).unwrap();
    let sink = ModAlerts::new(&svc);

    let unknown = sink.raise(RaiseAlert::new(
        "ci.other".parse().unwrap(),
        "pr-1",
        AlertLevel::Error,
        "CI failed",
    ));
    assert!(
        matches!(unknown, Err(ModError::PermissionDenied(_))),
        "{unknown:?}"
    );
    let core = sink.raise(RaiseAlert::new(
        "agents.done".parse().unwrap(),
        "s-1",
        AlertLevel::Done,
        "Spoofed",
    ));
    assert!(core.is_err(), "a mod cannot raise a core source's alerts");

    sink.raise(
        RaiseAlert::new(
            "ci.checks".parse().unwrap(),
            "pr-1",
            AlertLevel::Error,
            "CI failed",
        )
        .in_workspace("ws")
        .with_body("2 of 14 checks failed")
        .with_link(AlertLink::File {
            root: 0,
            path: ".github/workflows/ci.yml".into(),
            line: None,
        }),
    )
    .unwrap();
    let listed = svc.list(ListParams::default()).unwrap().alerts;
    assert_eq!(listed.len(), 1);
    let a = &listed[0];
    assert_eq!((a.source.as_str(), a.kind), ("ci.checks", AlertKind::Error));
    assert_eq!(a.workspace, Some(WorkspaceId("ws".into())));
    assert!(matches!(a.target, Some(AlertTarget::File { .. })));
    sink.resolve(&"ci.checks".parse().unwrap(), "pr-1").unwrap();
    assert!(
        svc.list(ListParams::default()).unwrap().alerts[0]
            .resolved_at
            .is_some()
    );
    // Listed for the settings, after the core ones.
    let sources = svc.get_settings().sources;
    assert_eq!(sources.last().unwrap().id, "ci.checks");
    assert_eq!(sources.last().unwrap().name, "CI checks");
}

// ---- end to end ------------------------------------------------------------------

/// A daemon's router with the scripted agent `fake`, served in-process.
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

    async fn connect(&self, token: SessionToken) -> Client {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(server_io).await });
        let mut config = ClientConfig::new("test", "test");
        config.token = Some(token);
        Client::connect(client_io, config).await.unwrap().0
    }

    async fn ui(&self) -> Client {
        self.connect(self.ctx.auth.daemon_token()).await
    }

    async fn stop(self) {
        self.ctx.shutdown.cancel();
        self.ctx.mods.shutdown().await;
    }
}

async fn until<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Some(t) = f().await {
            return t;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The alert of `source` for `session`, once `ok` holds for it.
async fn alert_where(
    ui: &Client,
    what: &str,
    source: &str,
    session: &SessionId,
    ok: impl Fn(&Alert) -> bool,
) -> Alert {
    until(what, async || {
        let list = ui.request::<am::List>(ListParams::default()).await.unwrap();
        list.alerts
            .into_iter()
            .find(|a| a.source == source && a.session.as_ref() == Some(session) && ok(a))
    })
    .await
}

fn ask() -> PermissionAsk {
    PermissionAsk {
        title: "Run cargo test".into(),
        tool_call: None,
        options: vec![PermissionOption {
            id: "allow-once".into(),
            name: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        }],
    }
}

async fn project(ui: &Client) -> (tempfile::TempDir, WorkspaceInfo) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    let x = 1;\n}\n",
    )
    .unwrap();
    let ws = ui
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    (dir, ws)
}

async fn session(ui: &Client, dir: &tempfile::TempDir, ws: &WorkspaceInfo) -> SessionId {
    ui.request::<agm::Create>(CreateSessionParams {
        provider: "fake".into(),
        cwd: dir.path().to_str().unwrap().into(),
        workspace: Some(ws.id.0.clone()),
        title: Some("Run the tests".into()),
        mode: None,
        from: None,
    })
    .await
    .unwrap()
    .id
}

async fn prompt(ui: &Client, id: &SessionId) {
    ui.request::<agm::Prompt>(PromptParams {
        session: id.clone(),
        content: vec![ContentBlock::text("go")],
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_agents_permission_request_question_finish_and_failure_raise_alerts() {
    let data = tempfile::tempdir().unwrap();
    let release = Arc::new(Notify::new());
    let agent = ScriptedProvider::new()
        .turn([
            Step::Permission(ask()),
            Step::Wait(Arc::clone(&release)),
            Step::Stop(StopReason::EndTurn),
        ])
        .turn([Step::Fail("npm ERR! ERESOLVE".into())]);
    let daemon = Daemon::start(data.path(), &agent).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui).await;
    let id = session(&ui, &dir, &ws).await;
    let mut changes = ui
        .subscribe::<am::Changes>(ChangesParams::default(), None)
        .await
        .unwrap();
    prompt(&ui, &id).await;

    // The permission request needs you, and opens the chat.
    let p = alert_where(
        &ui,
        "the permission alert",
        agents::PERMISSION,
        &id,
        Alert::is_active,
    )
    .await;
    assert_eq!(p.kind, AlertKind::NeedsYou);
    assert_eq!(p.title, "Permission requested: Run cargo test");
    assert_eq!(p.body.as_deref(), Some("Run the tests"));
    assert_eq!(p.workspace.as_ref(), Some(&ws.id));
    assert_eq!(
        p.target,
        Some(AlertTarget::Session {
            session: id.clone()
        })
    );
    let first = tokio::time::timeout(TIMEOUT, changes.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.event.id, p.id, "on alerts/changes too");

    // Answering it resolves it.
    let snapshot = ui
        .request::<agm::Get>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap();
    ui.request::<agm::RespondPermission>(RespondPermissionParams {
        session: id.clone(),
        request_id: snapshot.pending_permissions[0].id,
        outcome: PermissionOutcome::Selected {
            option_id: "allow-once".into(),
        },
        remember: false,
    })
    .await
    .unwrap();
    let resolved = alert_where(
        &ui,
        "the permission resolved",
        agents::PERMISSION,
        &id,
        |a| a.resolved_at.is_some(),
    )
    .await;
    assert!(resolved.read);

    // A question waits: needs you, and opens the question.
    let agent_token = daemon
        .ctx
        .auth
        .mint_session(ws.id.0.clone(), &id.0)
        .unwrap();
    let bridge = daemon.connect(agent_token).await;
    let q = bridge
        .request::<qm::Create>(CreateParams {
            workspace: ws.id.clone(),
            session: id.0.clone(),
            kind: QuestionKind::Clarification,
            body: "Why **1**?".into(),
            options: vec![],
            severity: None,
            anchors: vec![NewAnchor {
                root: ws.roots[0].id,
                path: "src/main.rs".into(),
                lines: LineRange { start: 1, end: 1 },
            }],
        })
        .await
        .unwrap();
    let qa = alert_where(
        &ui,
        "the question alert",
        agents::QUESTION,
        &id,
        Alert::is_active,
    )
    .await;
    assert_eq!(qa.body.as_deref(), Some("Why 1?"));
    assert_eq!(
        qa.target,
        Some(AlertTarget::Question {
            session: id.clone(),
            question: q.id,
            location: Some(FileLocation {
                root: ws.roots[0].id,
                path: "src/main.rs".into(),
                line: Some(1),
            }),
        })
    );
    // Agents never reach alerts.
    let forbidden = bridge.request::<am::List>(ListParams::default()).await;
    assert_eq!(
        forbidden.unwrap_err().rpc().map(|e| e.code),
        Some(codes::FORBIDDEN)
    );
    ui.request::<qm::Answer>(AnswerParams {
        id: q.id,
        option: None,
        text: Some("Because".into()),
    })
    .await
    .unwrap();
    alert_where(&ui, "the question resolved", agents::QUESTION, &id, |a| {
        a.resolved_at.is_some()
    })
    .await;

    // The turn ends: done.
    release.notify_one();
    let done = alert_where(&ui, "the done alert", agents::DONE, &id, Alert::is_active).await;
    assert_eq!(
        (done.kind, done.title.as_str()),
        (AlertKind::Done, "Fake agent finished")
    );

    // The next turn fails: an error, and done is resolved.
    prompt(&ui, &id).await;
    let failed = alert_where(&ui, "the error alert", agents::ERROR, &id, Alert::is_active).await;
    assert_eq!(failed.kind, AlertKind::Error);
    assert_eq!(failed.body.as_deref(), Some("npm ERR! ERESOLVE"));
    alert_where(&ui, "done resolved", agents::DONE, &id, |a| {
        a.resolved_at.is_some()
    })
    .await;
    daemon.stop().await;
}

#[tokio::test]
async fn after_a_restart_only_what_changed_meanwhile_raises_alerts() {
    let data = tempfile::tempdir().unwrap();
    let agent = ScriptedProvider::new().turn([Step::Stop(StopReason::EndTurn)]);
    let daemon = Daemon::start(data.path(), &agent).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui).await;
    let id = session(&ui, &dir, &ws).await;
    prompt(&ui, &id).await;
    let done = alert_where(&ui, "the done alert", agents::DONE, &id, Alert::is_active).await;
    ui.request::<am::MarkRead>(MarkReadParams {
        ids: vec![done.id],
        session: None,
    })
    .await
    .unwrap();
    daemon.stop().await;

    let daemon = Daemon::start(data.path(), &agent).await;
    let ui = daemon.ui().await;
    // Give the source time to catch up, then check nothing came back.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let list = ui.request::<am::List>(ListParams::default()).await.unwrap();
    assert_eq!(list.alerts.len(), 1, "{list:?}");
    assert!(list.alerts[0].read, "a restart does not make it unread");
    assert_eq!(list.alerts[0].seq, done.seq + 1, "only the read changed it");
    daemon.stop().await;
}
