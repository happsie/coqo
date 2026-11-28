// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: a scripted provider drives sessions through the session
//! manager, over the Coqo Protocol on an in-memory duplex.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use coqo_agents::model::*;
use coqo_agents::testing::{Call, ScriptedProvider, Step};
use coqo_agents::{
    AgentUpdate, KeepAlive, ManagerOptions, McpServer, PermissionAsk, ProviderError,
    SessionManager, SessionStore, StaticProviders, rpc,
};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::methods::codes as app_codes;
use coqo_protocol::{
    Client, ClientConfig, ProtocolError, Router, Sequenced, Server, ServerConfig, Subscription,
    codes,
};
use futures_util::StreamExt;
use tokio::sync::Notify;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Counts held keep-alive guards.
#[derive(Default)]
struct Busy(Arc<AtomicUsize>);

impl Busy {
    fn held(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

struct Guard(Arc<AtomicUsize>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl KeepAlive for Busy {
    fn hold(&self, _reason: String) -> Box<dyn Any + Send + Sync> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::new(Guard(Arc::clone(&self.0)))
    }
}

struct Daemon {
    manager: SessionManager,
    server: Server,
    busy: Arc<Busy>,
}

fn daemon(store: SessionStore, provider: &ScriptedProvider) -> Daemon {
    let info = AgentProviderInfo {
        id: "fake".parse().unwrap(),
        name: "Fake".into(),
        kind: AgentProviderKind::Acp,
        description: None,
    };
    let providers = StaticProviders::new().with(info, Arc::new(provider.clone()));
    let busy = Arc::new(Busy::default());
    let options = ManagerOptions {
        keep_alive: Some(Arc::clone(&busy) as Arc<dyn KeepAlive>),
        mcp_servers: Some(Arc::new(|_: &SessionInfo| {
            vec![McpServer {
                name: "coqo-editor".into(),
                command: "/usr/bin/coqod".into(),
                args: vec!["mcp".into()],
                env: vec![],
            }]
        })),
    };
    let manager = SessionManager::open(store, Arc::new(providers), options).unwrap();
    let router = rpc::register(Router::new(), manager.clone());
    let server = Server::new(router, ServerConfig::new("coqod", "test"));
    Daemon {
        manager,
        server,
        busy,
    }
}

impl Daemon {
    async fn connect(&self) -> Client {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(server_io).await });
        let (client, _notifications) =
            Client::connect(client_io, ClientConfig::new("test", "test"))
                .await
                .unwrap();
        client
    }
}

fn text(t: &str) -> ContentBlock {
    ContentBlock::text(t)
}

fn chunk(t: &str) -> Step {
    Step::Update(AgentUpdate::AgentMessageChunk(text(t)))
}

fn ask(title: &str) -> PermissionAsk {
    PermissionAsk {
        title: title.into(),
        tool_call: Some(ToolCallUpdate {
            id: ToolCallId("t1".into()),
            ..ToolCallUpdate::default()
        }),
        options: vec![
            PermissionOption {
                id: "allow-once".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                id: "reject".into(),
                name: "Reject".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
    }
}

async fn create(client: &Client) -> SessionInfo {
    client
        .request::<Create>(CreateSessionParams {
            provider: "fake".into(),
            cwd: "/work/app".into(),
            workspace: Some("ws-1".into()),
            title: Some("Test".into()),
            mode: None,
        })
        .await
        .unwrap()
}

async fn prompt(client: &Client, id: &SessionId, t: &str) -> Result<PromptResult, ProtocolError> {
    client
        .request::<Prompt>(PromptParams {
            session: id.clone(),
            content: vec![text(t)],
        })
        .await
}

async fn subscribe(client: &Client, id: &SessionId, since: Option<u64>) -> Subscription<Updates> {
    client
        .subscribe::<Updates>(
            SessionParams {
                session: id.clone(),
            },
            since,
        )
        .await
        .unwrap()
}

/// Collects updates up to and including the first that matches `stop`.
async fn until(
    sub: &mut Subscription<Updates>,
    stop: impl Fn(&SessionUpdate) -> bool,
) -> Vec<Sequenced<SessionUpdate>> {
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while let Some(item) = sub.next().await {
            let item = item.unwrap();
            let done = stop(&item.event);
            out.push(item);
            if done {
                return;
            }
        }
        panic!("subscription ended early: {out:?}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; got {out:?}"));
    out
}

fn turn_ended(u: &SessionUpdate) -> bool {
    matches!(u, SessionUpdate::TurnEnded { .. })
}

fn kind(u: &SessionUpdate) -> String {
    serde_json::to_value(u).unwrap()["update"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn app_error(e: ProtocolError) -> i64 {
    e.rpc().expect("an rpc error").code
}

async fn snapshot(client: &Client, id: &SessionId) -> SessionSnapshot {
    client
        .request::<Get>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap()
}

/// Polls `sessions/get` until `pred` holds.
async fn wait_for(
    client: &Client,
    id: &SessionId,
    pred: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let s = snapshot(client, id).await;
            if pred(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("timed out waiting for the session")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fake_provider_drives_a_session_end_to_end() {
    let diff = ToolCall {
        id: ToolCallId("t1".into()),
        title: "Write pong.txt".into(),
        name: None,
        kind: ToolKind::Edit,
        status: ToolCallStatus::Pending,
        content: vec![ToolCallContent::Diff {
            path: "/work/app/pong.txt".into(),
            old_text: None,
            new_text: "pong\n".into(),
        }],
        locations: vec![ToolCallLocation {
            path: "/work/app/pong.txt".into(),
            line: None,
        }],
        raw_input: None,
        raw_output: None,
    };
    let provider = ScriptedProvider::new().turn([
        Step::Update(AgentUpdate::AgentThoughtChunk(text("thinking"))),
        Step::Update(AgentUpdate::ToolCall(diff)),
        Step::Permission(ask("Write pong.txt")),
        Step::Update(AgentUpdate::ToolCallUpdate(ToolCallUpdate {
            id: ToolCallId("t1".into()),
            status: Some(ToolCallStatus::Completed),
            ..ToolCallUpdate::default()
        })),
        Step::Update(AgentUpdate::Plan(vec![PlanEntry {
            content: "write pong".into(),
            priority: PlanPriority::High,
            status: PlanEntryStatus::Completed,
        }])),
        chunk("Wrote "),
        chunk("pong.txt"),
        Step::Update(AgentUpdate::Usage(Usage {
            used_tokens: Some(1200),
            context_size: Some(200_000),
            ..Usage::default()
        })),
        Step::Update(AgentUpdate::AuthStatus(AuthStatus {
            kind: AuthKind::Account,
            label: Some("Claude Max".into()),
            plan: Some("max".into()),
        })),
        Step::Stop(StopReason::EndTurn),
    ]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;

    let providers = client.request::<Providers>(()).await.unwrap();
    assert_eq!(providers.len(), 1);
    assert!(providers[0].available);
    assert_eq!(providers[0].kind, ProviderKind::Acp);
    assert!(providers[0].capabilities.unwrap().resume);

    let info = create(&client).await;
    assert_eq!(info.state, AgentState::Idle);
    assert_eq!(info.mode.as_deref(), Some("default"));
    assert_eq!(
        provider.calls(),
        [Call::Start {
            cwd: "/work/app".into(),
            mcp_servers: vec!["coqo-editor".into()],
        }]
    );
    let id = info.id.clone();
    let mut sub = subscribe(&client, &id, Some(0)).await;

    let started = prompt(&client, &id, "write pong").await.unwrap();
    assert_eq!(started.turn, 1);
    let events = until(&mut sub, |u| {
        matches!(u, SessionUpdate::PermissionRequested(_))
    })
    .await;
    let SessionUpdate::PermissionRequested(request) = &events.last().unwrap().event else {
        unreachable!()
    };

    // The turn waits for the answer; the session says so.
    let snap = snapshot(&client, &id).await;
    assert_eq!(snap.info.state, AgentState::NeedsPermission);
    assert!(snap.running);
    assert_eq!(snap.pending_permissions, std::slice::from_ref(request));
    assert_eq!(d.busy.held(), 1);
    let busy = prompt(&client, &id, "again").await.unwrap_err();
    assert_eq!(app_error(busy), app_codes::SESSION_BUSY);
    let bad = client
        .request::<RespondPermission>(RespondPermissionParams {
            session: id.clone(),
            request_id: request.id,
            outcome: PermissionOutcome::Selected {
                option_id: "nope".into(),
            },
            remember: false,
        })
        .await
        .unwrap_err();
    assert_eq!(app_error(bad), app_codes::UNKNOWN_PERMISSION_REQUEST);

    client
        .request::<RespondPermission>(RespondPermissionParams {
            session: id.clone(),
            request_id: request.id,
            outcome: PermissionOutcome::Selected {
                option_id: "allow-once".into(),
            },
            remember: false,
        })
        .await
        .unwrap();
    let rest = until(&mut sub, |u| {
        matches!(
            u,
            SessionUpdate::State {
                state: AgentState::Done,
                ..
            }
        )
    })
    .await;

    let all: Vec<_> = events.iter().chain(&rest).collect();
    // Seqs are contiguous from 1.
    for (i, e) in all.iter().enumerate() {
        assert_eq!(e.seq, i as u64 + 1);
    }
    let kinds: Vec<_> = all.iter().map(|e| kind(&e.event)).collect();
    assert_eq!(
        kinds,
        [
            "modes",
            "user_message",
            "turn_started",
            "state",
            "agent_thought_chunk",
            "tool_call",
            "permission_requested",
            "state",
            "permission_resolved",
            "state",
            "tool_call_update",
            "plan",
            "agent_message_chunk",
            "agent_message_chunk",
            "usage",
            "auth_status",
            "turn_ended",
            "state",
        ]
    );
    assert_eq!(
        provider.outcomes(),
        [PermissionOutcome::Selected {
            option_id: "allow-once".into()
        }]
    );

    let snap = snapshot(&client, &id).await;
    assert_eq!(snap.info.state, AgentState::Done);
    assert!(!snap.running);
    assert_eq!(snap.head, all.len() as u64);
    assert_eq!(snap.turns, 1);
    assert_eq!(snap.plan.len(), 1);
    assert_eq!(snap.usage.unwrap().used_tokens, Some(1200));
    assert_eq!(snap.auth.unwrap().kind, AuthKind::Account);
    assert_eq!(d.busy.held(), 0);

    // Modes.
    client
        .request::<SetMode>(SetModeParams {
            session: id.clone(),
            mode_id: "plan".into(),
        })
        .await
        .unwrap();
    let bad = client
        .request::<SetMode>(SetModeParams {
            session: id.clone(),
            mode_id: "nope".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(app_error(bad), codes::INVALID_PARAMS);
    let snap = snapshot(&client, &id).await;
    assert_eq!(snap.info.mode.as_deref(), Some("plan"));
    assert_eq!(snap.modes.unwrap().current_mode_id, "plan");

    // The sidebar list and its change feed.
    let list = client
        .request::<List>(ListSessionsParams::default())
        .await
        .unwrap();
    assert_eq!(list.sessions, std::slice::from_ref(&snap.info));
    let mut changes = client
        .subscribe::<Changes>(ChangesParams::default(), Some(0))
        .await
        .unwrap();
    let latest = changes.next().await.unwrap().unwrap();
    assert_eq!(latest.seq, list.seq);
    assert_eq!(latest.event, snap.info);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_ui_mid_turn_does_not_interrupt_the_agent() {
    let gate = Arc::new(Notify::new());
    let provider = ScriptedProvider::new().turn([
        chunk("first half"),
        Step::Wait(Arc::clone(&gate)),
        chunk("second half"),
        Step::Stop(StopReason::EndTurn),
    ]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);

    let ui = d.connect().await;
    let id = create(&ui).await.id;
    let mut sub = subscribe(&ui, &id, Some(0)).await;
    prompt(&ui, &id, "go").await.unwrap();
    let seen = until(&mut sub, |u| {
        matches!(u, SessionUpdate::AgentMessageChunk { .. })
    })
    .await;
    let last_seen = seen.last().unwrap().seq;
    assert_eq!(d.busy.held(), 1, "a running turn keeps the daemon alive");

    // The UI goes away mid-turn.
    drop(sub);
    drop(ui);
    gate.notify_one();

    // A new UI reattaches: snapshot, then replay from the last seq it saw.
    let ui = d.connect().await;
    let snap = wait_for(&ui, &id, |s| s.info.state == AgentState::Done).await;
    assert!(!snap.running);
    assert_eq!(d.busy.held(), 0);
    let mut sub = subscribe(&ui, &id, Some(last_seen)).await;
    assert_eq!(sub.head(), snap.head);
    let rest = until(&mut sub, |u| {
        matches!(
            u,
            SessionUpdate::State {
                state: AgentState::Done,
                ..
            }
        )
    })
    .await;
    assert_eq!(rest[0].seq, last_seen + 1);
    assert_eq!(
        rest[0].event,
        SessionUpdate::AgentMessageChunk {
            content: text("second half")
        }
    );
    assert!(rest.iter().any(|e| matches!(
        e.event,
        SessionUpdate::TurnEnded {
            stop_reason: StopReason::EndTurn,
            ..
        }
    )));
    assert_eq!(rest.last().unwrap().seq, snap.head);

    // Resuming from the head replays nothing and continues live.
    let mut live = subscribe(&ui, &id, Some(snap.head)).await;
    prompt(&ui, &id, "more").await.unwrap();
    let next = until(&mut live, turn_ended).await;
    assert_eq!(next[0].seq, snap.head + 1);
    assert_eq!(kind(&next[0].event), "user_message");

    // Resuming ahead of the session is a gap: take a fresh snapshot.
    let err = ui
        .subscribe::<Updates>(
            SessionParams {
                session: id.clone(),
            },
            Some(10_000),
        )
        .await
        .unwrap_err();
    assert_eq!(app_error(err), codes::RESUME_GAP);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_resolves_pending_permissions_and_ends_the_turn() {
    let provider = ScriptedProvider::new()
        .turn([
            Step::Permission(ask("Run rm")),
            Step::Stop(StopReason::EndTurn),
        ])
        .turn([
            Step::Wait(Arc::new(Notify::new())),
            Step::Stop(StopReason::EndTurn),
        ]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;
    let id = create(&client).await.id;
    let mut sub = subscribe(&client, &id, None).await;

    // Cancel while a permission request waits.
    prompt(&client, &id, "clean up").await.unwrap();
    until(&mut sub, |u| {
        matches!(u, SessionUpdate::PermissionRequested(_))
    })
    .await;
    let session = SessionParams {
        session: id.clone(),
    };
    assert!(client.request::<Cancel>(session.clone()).await.unwrap());
    let events = until(&mut sub, |u| {
        matches!(
            u,
            SessionUpdate::State {
                state: AgentState::Idle,
                ..
            }
        )
    })
    .await;
    assert!(events.iter().any(|e| e.event
        == SessionUpdate::PermissionResolved {
            request_id: PermissionRequestId(1),
            outcome: PermissionOutcome::Cancelled,
            by_rule: None,
            remembered: None,
        }));
    assert!(events.iter().any(|e| matches!(
        e.event,
        SessionUpdate::TurnEnded {
            stop_reason: StopReason::Cancelled,
            ..
        }
    )));
    assert_eq!(provider.outcomes(), [PermissionOutcome::Cancelled]);
    assert!(
        !client.request::<Cancel>(session.clone()).await.unwrap(),
        "nothing left to cancel"
    );

    // Cancel a turn that is busy working.
    prompt(&client, &id, "wait").await.unwrap();
    until(&mut sub, |u| {
        matches!(
            u,
            SessionUpdate::State {
                state: AgentState::Working,
                ..
            }
        )
    })
    .await;
    assert!(client.request::<Cancel>(session.clone()).await.unwrap());
    let snap = wait_for(&client, &id, |s| !s.running).await;
    assert_eq!(snap.info.state, AgentState::Idle);
    assert_eq!(
        provider
            .calls()
            .iter()
            .filter(|c| **c == Call::Cancel)
            .count(),
        2
    );
    assert_eq!(d.busy.held(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcripts_survive_a_daemon_restart_and_sessions_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("coqo.db");

    let first = ScriptedProvider::new().turn([chunk("hello"), Step::Stop(StopReason::EndTurn)]);
    let (id, transcript) = {
        let d = daemon(SessionStore::open(&db).unwrap(), &first);
        let client = d.connect().await;
        let id = create(&client).await.id;
        let mut sub = subscribe(&client, &id, Some(0)).await;
        prompt(&client, &id, "hi").await.unwrap();
        let transcript = until(&mut sub, |u| {
            matches!(
                u,
                SessionUpdate::State {
                    state: AgentState::Done,
                    ..
                }
            )
        })
        .await;
        (id, transcript)
    };

    // A new daemon on the same database. The provider replays history on
    // resume, which Coqo already has.
    let second = ScriptedProvider::new()
        .on_resume([AgentUpdate::AgentMessageChunk(text("replayed history"))])
        .turn([chunk("welcome back"), Step::Stop(StopReason::EndTurn)]);
    let d = daemon(SessionStore::open(&db).unwrap(), &second);
    let client = d.connect().await;

    let list = client
        .request::<List>(ListSessionsParams {
            workspace: Some("ws-1".into()),
            include_closed: false,
        })
        .await
        .unwrap();
    assert_eq!(list.sessions.len(), 1);
    assert_eq!(list.sessions[0].id, id);
    assert_eq!(list.sessions[0].state, AgentState::Done);

    let mut sub = subscribe(&client, &id, Some(0)).await;
    let replayed = until(&mut sub, |u| {
        matches!(
            u,
            SessionUpdate::State {
                state: AgentState::Done,
                ..
            }
        )
    })
    .await;
    assert_eq!(replayed, transcript);

    let next = prompt(&client, &id, "again").await.unwrap();
    assert_eq!(next.turn, 2);
    assert_eq!(next.seq, transcript.last().unwrap().seq + 1);
    let more = until(&mut sub, turn_ended).await;
    assert_eq!(
        second.calls()[..2],
        [
            Call::Resume("fake-1".into()),
            Call::Prompt(vec![text("again")])
        ]
    );
    assert!(
        !more.iter().any(|e| e.event
            == SessionUpdate::AgentMessageChunk {
                content: text("replayed history")
            }),
        "history replayed on resume is not recorded twice"
    );
    assert!(more.iter().any(|e| e.event
        == SessionUpdate::AgentMessageChunk {
            content: text("welcome back")
        }));
}

#[test]
fn a_turn_cut_off_by_a_crash_is_marked_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("coqo.db");
    let gate = Arc::new(Notify::new());
    let provider = ScriptedProvider::new().turn([
        chunk("working on it"),
        Step::Wait(Arc::clone(&gate)),
        Step::Stop(StopReason::EndTurn),
    ]);

    // The daemon dies mid-turn: dropping the runtime drops the turn's task.
    let id = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let d = daemon(SessionStore::open(&db).unwrap(), &provider);
            let client = d.connect().await;
            let id = create(&client).await.id;
            let mut sub = subscribe(&client, &id, None).await;
            prompt(&client, &id, "go").await.unwrap();
            until(&mut sub, |u| {
                matches!(u, SessionUpdate::AgentMessageChunk { .. })
            })
            .await;
            id
        })
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let restarted = ScriptedProvider::new();
        let d = daemon(SessionStore::open(&db).unwrap(), &restarted);
        let client = d.connect().await;
        let snap = snapshot(&client, &id).await;
        assert_eq!(snap.info.state, AgentState::Error);
        assert!(snap.info.state_reason.unwrap().contains("interrupted"));
        assert!(!snap.running);
        assert_eq!(d.busy.held(), 0);

        let mut sub = subscribe(&client, &id, Some(snap.head - 2)).await;
        let tail = until(&mut sub, |u| matches!(u, SessionUpdate::State { .. })).await;
        assert!(matches!(
            tail[0].event,
            SessionUpdate::TurnEnded {
                turn: 1,
                stop_reason: StopReason::Interrupted,
                ..
            }
        ));

        // The session carries on with a resumed provider session.
        prompt(&client, &id, "try again").await.unwrap();
        until(&mut sub, turn_ended).await;
        assert_eq!(restarted.calls()[0], Call::Resume("fake-1".into()));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_session_mid_turn_keeps_its_transcript() {
    let provider = ScriptedProvider::new().turn([
        chunk("started"),
        Step::Wait(Arc::new(Notify::new())),
        Step::Stop(StopReason::EndTurn),
    ]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;
    let id = create(&client).await.id;
    let mut sub = subscribe(&client, &id, Some(0)).await;
    prompt(&client, &id, "go").await.unwrap();
    until(&mut sub, |u| {
        matches!(u, SessionUpdate::AgentMessageChunk { .. })
    })
    .await;

    let session = SessionParams {
        session: id.clone(),
    };
    assert!(client.request::<Close>(session.clone()).await.unwrap());
    let tail = until(&mut sub, |u| *u == SessionUpdate::Closed).await;
    assert!(tail.iter().any(|e| matches!(
        e.event,
        SessionUpdate::TurnEnded {
            stop_reason: StopReason::Cancelled,
            ..
        }
    )));
    assert!(!client.request::<Close>(session.clone()).await.unwrap());
    assert!(provider.calls().contains(&Call::Close));
    assert_eq!(d.busy.held(), 0);

    let err = prompt(&client, &id, "more").await.unwrap_err();
    assert_eq!(app_error(err), app_codes::SESSION_CLOSED);
    let snap = snapshot(&client, &id).await;
    assert!(snap.info.closed);
    assert_eq!(snap.info.state, AgentState::Idle);
    let open = client
        .request::<List>(ListSessionsParams::default())
        .await
        .unwrap();
    assert!(open.sessions.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_reported_states_and_failures() {
    let provider = ScriptedProvider::new()
        .turn([
            Step::State(AgentState::NeedsInput, Some("which file?".into())),
            Step::Stop(StopReason::EndTurn),
        ])
        .turn([Step::Fail("adapter crashed".into())]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;
    let id = create(&client).await.id;

    prompt(&client, &id, "edit it").await.unwrap();
    let snap = wait_for(&client, &id, |s| !s.running).await;
    assert_eq!(snap.info.state, AgentState::NeedsInput);
    assert_eq!(snap.info.state_reason.as_deref(), Some("which file?"));

    prompt(&client, &id, "main.rs").await.unwrap();
    let snap = wait_for(&client, &id, |s| !s.running).await;
    assert_eq!(snap.info.state, AgentState::Error);
    assert_eq!(snap.info.state_reason.as_deref(), Some("adapter crashed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_that_cannot_start_fails_create() {
    let provider = ScriptedProvider::new().fail_start(ProviderError::AuthRequired(
        "run `claude` and `/login`".into(),
    ));
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;

    let err = client
        .request::<Create>(CreateSessionParams {
            provider: "fake".into(),
            cwd: "/work/app".into(),
            workspace: None,
            title: None,
            mode: None,
        })
        .await
        .unwrap_err();
    assert_eq!(app_error(err), app_codes::AUTH_REQUIRED);
    let all = client
        .request::<List>(ListSessionsParams {
            workspace: None,
            include_closed: true,
        })
        .await
        .unwrap();
    assert_eq!(all.sessions.len(), 1);
    assert!(all.sessions[0].closed);
    assert_eq!(all.sessions[0].state, AgentState::Error);

    for (provider, cwd, code) in [
        ("nope", "/work/app", app_codes::UNKNOWN_PROVIDER),
        ("fake", "relative", codes::INVALID_PARAMS),
    ] {
        let err = client
            .request::<Create>(CreateSessionParams {
                provider: provider.into(),
                cwd: cwd.into(),
                workspace: None,
                title: None,
                mode: None,
            })
            .await
            .unwrap_err();
        assert_eq!(app_error(err), code);
    }
    let err = client
        .request::<Get>(SessionParams {
            session: SessionId("missing".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(app_error(err), app_codes::UNKNOWN_SESSION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_shutdown_interrupts_running_turns() {
    let provider = ScriptedProvider::new().turn([
        Step::Wait(Arc::new(Notify::new())),
        Step::Stop(StopReason::EndTurn),
    ]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;
    let id = create(&client).await.id;
    prompt(&client, &id, "go").await.unwrap();
    assert_eq!(d.busy.held(), 1);
    d.manager.shutdown().await;
    let snap = snapshot(&client, &id).await;
    assert_eq!(snap.info.state, AgentState::Error);
    assert!(!snap.running);
    assert_eq!(d.busy.held(), 0);
}

/// #293: an idle session's provider can still report after shutdown (an
/// ACP agent's `available_commands_update` follows `session/new`). Once
/// `shutdown` returns nothing is recorded, so a new daemon on the same
/// database does not hand out a seq the old one also wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_recorded_after_shutdown() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("coqo.db");
    let first = ScriptedProvider::new();
    let d = daemon(SessionStore::open(&db).unwrap(), &first);
    let client = d.connect().await;
    let id = create(&client).await.id;
    d.manager.shutdown().await;

    // The next daemon reads the head while the old agent is still talking.
    let second = ScriptedProvider::new();
    let next = daemon(SessionStore::open(&db).unwrap(), &second);
    let head = next.manager.snapshot(&id).unwrap().head;
    let late = first.events().pop().unwrap();
    late.update(AgentUpdate::AgentMessageChunk(text("late")));
    late.report_state(AgentState::Working, None);
    assert_eq!(d.manager.snapshot(&id).unwrap().head, head);

    let next_client = next.connect().await;
    let result = prompt(&next_client, &id, "again").await.unwrap();
    assert_eq!(result.seq, head + 1);
}

/// Workspace `ws-rules` with one root, as the workspace service stores it.
fn add_workspace(db: &std::path::Path, root: &std::path::Path) {
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

/// A permission request to run `command`, as an ACP agent sends it.
fn run(id: &str, command: &str) -> Step {
    Step::Permission(PermissionAsk {
        title: format!("`{command}`"),
        tool_call: Some(ToolCallUpdate {
            id: ToolCallId(id.into()),
            kind: Some(ToolKind::Execute),
            raw_input: Some(serde_json::json!({ "command": command })),
            ..ToolCallUpdate::default()
        }),
        options: ask("").options,
    })
}

async fn create_in(client: &Client, root: &std::path::Path) -> SessionId {
    client
        .request::<Create>(CreateSessionParams {
            provider: "fake".into(),
            cwd: root.to_string_lossy().into_owned(),
            workspace: Some("ws-rules".into()),
            title: None,
            mode: None,
        })
        .await
        .unwrap()
        .id
}

async fn rules_of(client: &Client) -> Vec<PermissionRule> {
    client
        .request::<PermissionRules>(WorkspaceRulesParams {
            workspace: "ws-rules".into(),
        })
        .await
        .unwrap()
}

fn allow_once() -> PermissionOutcome {
    PermissionOutcome::Selected {
        option_id: "allow-once".into(),
    }
}

fn reject() -> PermissionOutcome {
    PermissionOutcome::Selected {
        option_id: "reject".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remembered_rules_answer_matching_requests_and_survive_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("app");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let db = tmp.path().join("coqo.db");
    let store = SessionStore::open(&db).unwrap();
    add_workspace(&db, &root);

    let stop = || Step::Stop(StopReason::EndTurn);
    let provider = ScriptedProvider::new()
        .turn([run("t1", "cargo test -p shop"), stop()])
        .turn([run("t2", "cargo test"), stop()])
        .turn([run("t3", "cargo build"), stop()])
        .turn([run("t4", "cargo test; rm -rf /"), stop()]);
    let rule = {
        let d = daemon(store, &provider);
        let client = d.connect().await;
        let id = create_in(&client, &root).await;
        let mut sub = subscribe(&client, &id, Some(0)).await;

        // Asked, with the rule "remember" would create.
        prompt(&client, &id, "test").await.unwrap();
        let events = until(&mut sub, |u| {
            matches!(u, SessionUpdate::PermissionRequested(_))
        })
        .await;
        let SessionUpdate::PermissionRequested(request) = &events.last().unwrap().event else {
            unreachable!()
        };
        assert_eq!(
            request.suggested_rule,
            Some(PermissionRuleSpec {
                tool: ToolKind::Execute,
                pattern: "cargo test".into(),
            })
        );
        wait_for(&client, &id, |s| {
            s.info.state == AgentState::NeedsPermission
        })
        .await;
        client
            .request::<RespondPermission>(RespondPermissionParams {
                session: id.clone(),
                request_id: request.id,
                outcome: allow_once(),
                remember: true,
            })
            .await
            .unwrap();
        let events = until(&mut sub, turn_ended).await;
        let remembered = events
            .iter()
            .find_map(|e| match &e.event {
                SessionUpdate::PermissionResolved { remembered, .. } => remembered.clone(),
                _ => None,
            })
            .expect("the answer says which rule it created");
        assert_eq!(
            (
                remembered.tool,
                remembered.pattern.as_str(),
                remembered.decision
            ),
            (ToolKind::Execute, "cargo test", RuleDecision::Allow)
        );
        assert_eq!(rules_of(&client).await, std::slice::from_ref(&remembered));

        // The next matching request is answered by the rule: never pending.
        prompt(&client, &id, "test again").await.unwrap();
        let events = until(&mut sub, turn_ended).await;
        let kinds: Vec<_> = events.iter().map(|e| kind(&e.event)).collect();
        assert!(
            kinds
                .windows(2)
                .any(|w| w == ["permission_requested", "permission_resolved"]),
            "{kinds:?}"
        );
        assert!(events.iter().any(|e| matches!(
            &e.event,
            SessionUpdate::PermissionResolved { by_rule: Some(r), outcome, .. }
                if r.id == remembered.id && *outcome == allow_once()
        )));
        assert!(
            !events.iter().any(|e| matches!(
                e.event,
                SessionUpdate::State {
                    state: AgentState::NeedsPermission,
                    ..
                }
            )),
            "no needs_permission for a rule's answer"
        );

        // Anything else is still asked: another command, or one a rule may
        // not answer (which has no suggestion either).
        for command in ["cargo build", "cargo test; rm -rf /"] {
            prompt(&client, &id, command).await.unwrap();
            let events = until(&mut sub, |u| {
                matches!(u, SessionUpdate::PermissionRequested(_))
            })
            .await;
            let SessionUpdate::PermissionRequested(request) = &events.last().unwrap().event else {
                unreachable!()
            };
            assert_eq!(
                request.suggested_rule.is_some(),
                command == "cargo build",
                "{command}"
            );
            wait_for(&client, &id, |s| {
                s.info.state == AgentState::NeedsPermission
            })
            .await;
            let bad = client
                .request::<RespondPermission>(RespondPermissionParams {
                    session: id.clone(),
                    request_id: request.id,
                    outcome: PermissionOutcome::Cancelled,
                    remember: true,
                })
                .await
                .unwrap_err();
            assert_eq!(app_error(bad), codes::INVALID_PARAMS);
            client
                .request::<RespondPermission>(RespondPermissionParams {
                    session: id.clone(),
                    request_id: request.id,
                    outcome: reject(),
                    remember: false,
                })
                .await
                .unwrap();
            until(&mut sub, turn_ended).await;
        }
        assert_eq!(
            provider.outcomes(),
            [allow_once(), allow_once(), reject(), reject()]
        );
        remembered
    };

    // A new daemon on the same database still has the rule.
    let provider = ScriptedProvider::new()
        .turn([run("t1", "cargo test --release"), stop()])
        .turn([run("t2", "cargo test"), stop()]);
    let d = daemon(SessionStore::open(&db).unwrap(), &provider);
    let client = d.connect().await;
    assert_eq!(rules_of(&client).await, std::slice::from_ref(&rule));
    let id = create_in(&client, &root).await;
    let mut sub = subscribe(&client, &id, Some(0)).await;
    prompt(&client, &id, "test").await.unwrap();
    until(&mut sub, turn_ended).await;
    assert_eq!(provider.outcomes(), [allow_once()]);

    // Deleted, it asks again.
    let deleted = client
        .request::<DeletePermissionRule>(RuleParams { id: rule.id })
        .await
        .unwrap();
    assert!(deleted);
    assert!(rules_of(&client).await.is_empty());
    prompt(&client, &id, "test").await.unwrap();
    wait_for(&client, &id, |s| {
        s.info.state == AgentState::NeedsPermission
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_status_and_actions_over_the_protocol() {
    let install = ProviderAction {
        id: "install".into(),
        label: "Install the adapter".into(),
        run: ProviderActionRun::Daemon,
    };
    let sign_in = ProviderAction {
        id: "sign-in".into(),
        label: "Open `claude` in a terminal".into(),
        run: ProviderActionRun::Terminal {
            program: "/usr/bin/claude".into(),
            args: vec![],
        },
    };
    let provider = ScriptedProvider::new()
        .status(ProviderStatus {
            ready: false,
            message: Some("The adapter isn't installed".into()),
            actions: vec![install],
            sign_in: Some(sign_in.clone()),
        })
        .action(
            "install",
            Some(ProviderStatus {
                sign_in: Some(sign_in),
                ..ProviderStatus::ready()
            }),
        );
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;

    let listed = client.request::<Providers>(()).await.unwrap();
    assert!(!listed[0].available, "not set up: not available");
    assert_eq!(listed[0].status.as_ref().unwrap().actions[0].id, "install");

    let info = client
        .request::<RunAction>(RunActionParams {
            provider: "fake".into(),
            action: "install".into(),
        })
        .await
        .unwrap();
    assert!(info.available);
    assert!(info.status.unwrap().ready);
    assert_eq!(provider.calls(), [Call::RunAction("install".into())]);
    assert_eq!(d.busy.held(), 0);

    for (provider, action, code) in [
        ("fake", "nope", app_codes::UNSUPPORTED),
        ("nope", "install", app_codes::UNKNOWN_PROVIDER),
    ] {
        let err = client
            .request::<RunAction>(RunActionParams {
                provider: provider.into(),
                action: action.into(),
            })
            .await
            .unwrap_err();
        assert_eq!(app_error(err), code);
    }
}

async fn set_status(
    client: &Client,
    id: &SessionId,
    status: &str,
    state: Option<StatusState>,
    progress: Option<(u64, u64)>,
) -> Result<SessionInfo, ProtocolError> {
    client
        .request::<SetStatus>(SetStatusParams {
            session: id.clone(),
            status: status.into(),
            state,
            progress: progress.map(|(current, total)| StatusProgress { current, total }),
        })
        .await
}

/// The next `sessions/changes` event of session `id`.
async fn next_change(changes: &mut Subscription<Changes>, id: &SessionId) -> SessionInfo {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let change = changes.next().await.expect("open").expect("ok").event;
            if &change.id == id {
                return change;
            }
        }
    })
    .await
    .expect("a change in time")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agents_status_follows_its_turns() {
    let go = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let provider = ScriptedProvider::new()
        .turn([Step::Wait(Arc::clone(&go)), Step::Stop(StopReason::EndTurn)])
        .turn([
            Step::Wait(Arc::clone(&resume)),
            Step::Stop(StopReason::EndTurn),
        ]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;
    let created = create(&client).await;
    let id = created.id.clone();
    assert_eq!(created.state_since, Some(created.created_at));
    assert!(created.status.is_none());
    let head = client
        .request::<List>(ListSessionsParams::default())
        .await
        .unwrap()
        .seq;
    let mut changes = client
        .subscribe::<Changes>(ChangesParams::default(), Some(head))
        .await
        .unwrap();

    // Between turns a status is just text; the state stays.
    let info = set_status(&client, &id, "  Reading\nthe code ", None, None)
        .await
        .unwrap();
    assert_eq!(info.state, AgentState::Idle);
    let status = info.status.clone().unwrap();
    assert_eq!(status.text, "Reading the code");
    assert_eq!((status.turn, status.progress), (None, None));
    assert_eq!(next_change(&mut changes, &id).await, info);

    // A turn clears it; during the turn the agent reports progress.
    prompt(&client, &id, "fix the tests").await.unwrap();
    let working = next_change(&mut changes, &id).await;
    assert_eq!(working.state, AgentState::Working);
    assert!(working.status.is_none(), "{working:?}");
    assert!(working.state_since >= created.state_since);
    let info = set_status(&client, &id, "Running the tests", None, Some((7, 5)))
        .await
        .unwrap();
    let status = info.status.clone().unwrap();
    assert_eq!(
        status.progress,
        Some(StatusProgress {
            current: 5,
            total: 5
        }),
        "current is capped"
    );
    assert_eq!(status.turn, Some(1));
    assert_eq!(next_change(&mut changes, &id).await, info);
    // The same again changes nothing and keeps its time.
    let again = set_status(&client, &id, "Running the tests", None, Some((5, 5)))
        .await
        .unwrap();
    assert_eq!(again, info);
    // Blocked: the status is the reason, recorded in the transcript too.
    let blocked = set_status(
        &client,
        &id,
        "Need DATABASE_URL",
        Some(StatusState::Blocked),
        None,
    )
    .await
    .unwrap();
    assert_eq!(blocked.state, AgentState::Blocked);
    assert_eq!(blocked.state_reason.as_deref(), Some("Need DATABASE_URL"));
    assert!(blocked.state_since >= working.state_since);
    assert_eq!(next_change(&mut changes, &id).await, blocked);
    let snap = snapshot(&client, &id).await;
    assert_eq!(snap.info, blocked);
    let mut sub = subscribe(&client, &id, Some(snap.head - 1)).await;
    let last = until(&mut sub, |_| true).await;
    assert_eq!(
        last[0].event,
        SessionUpdate::State {
            state: AgentState::Blocked,
            reason: Some("Need DATABASE_URL".into())
        }
    );
    // Working again, then done: the turn still runs, so it works.
    let info = set_status(&client, &id, "Retrying", Some(StatusState::Working), None)
        .await
        .unwrap();
    assert_eq!((info.state, info.state_reason), (AgentState::Working, None));
    let info = set_status(
        &client,
        &id,
        "All 12 tests pass",
        Some(StatusState::Done),
        None,
    )
    .await
    .unwrap();
    assert_eq!(info.state, AgentState::Working);
    go.notify_one();
    // The turn ends done, keeping the outcome.
    let snap = wait_for(&client, &id, |s| !s.running).await;
    assert_eq!(snap.info.state, AgentState::Done);
    assert_eq!(
        snap.info.status.as_ref().map(|s| s.text.as_str()),
        Some("All 12 tests pass")
    );

    // A plain status does not outlive its turn.
    prompt(&client, &id, "and the docs").await.unwrap();
    wait_for(&client, &id, |s| s.running).await;
    set_status(&client, &id, "Editing README.md", None, Some((1, 3)))
        .await
        .unwrap();
    resume.notify_one();
    let snap = wait_for(&client, &id, |s| !s.running).await;
    assert_eq!(snap.info.state, AgentState::Done);
    assert!(snap.info.status.is_none(), "{:?}", snap.info.status);

    // A daemon feature's report (a question, #73).
    let asked = d
        .manager
        .report_state(&id, AgentState::NeedsInput, Some("Which DB?".into()))
        .unwrap();
    assert_eq!(asked.state, AgentState::NeedsInput);
    assert_eq!(asked.state_reason.as_deref(), Some("Which DB?"));

    // Bad statuses.
    for (status, progress) in [(" \n", None), ("ok", Some((0, 0)))] {
        let err = set_status(&client, &id, status, None, progress)
            .await
            .unwrap_err();
        assert_eq!(app_error(err), codes::INVALID_PARAMS, "{status:?}");
    }
    let err = set_status(&client, &SessionId("nope".into()), "x", None, None)
        .await
        .unwrap_err();
    assert_eq!(app_error(err), app_codes::UNKNOWN_SESSION);
    client
        .request::<Close>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap();
    let err = set_status(&client, &id, "x", None, None).await.unwrap_err();
    assert_eq!(app_error(err), app_codes::SESSION_CLOSED);
    assert_eq!(d.busy.held(), 0);
}

fn criterion(
    id: Option<&str>,
    text: Option<&str>,
    status: Option<CriterionStatus>,
) -> CriterionInput {
    CriterionInput {
        id: id.map(Into::into),
        text: text.map(Into::into),
        status,
        evidence: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acceptance_criteria_stream_and_survive_a_daemon_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("coqo.db");
    let provider = ScriptedProvider::new();
    let id = {
        let d = daemon(SessionStore::open(&db).unwrap(), &provider);
        let client = d.connect().await;
        let id = create(&client).await.id;
        let head = client
            .request::<List>(ListSessionsParams::default())
            .await
            .unwrap()
            .seq;
        let mut changes = client
            .subscribe::<Changes>(ChangesParams::default(), Some(head))
            .await
            .unwrap();

        // The agent registers its criteria (the manager, as the route does
        // for a session token).
        let set = SetCriteriaParams {
            session: id.clone(),
            criteria: vec![
                criterion(None, Some("100+ units get 10% off"), None),
                criterion(None, Some("Discount capped at 500 EUR"), None),
            ],
            replace: true,
        };
        let info = d
            .manager
            .set_criteria(set.clone(), CriterionAuthor::Agent)
            .unwrap();
        let ids: Vec<_> = info.criteria.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["AC1", "AC2"]);
        let seen = next_change(&mut changes, &id).await;
        assert_eq!(seen.criteria, info.criteria);

        // It verifies one, with evidence relative to its cwd.
        let mut met = criterion(Some("AC1"), None, Some(CriterionStatus::Met));
        met.evidence = Some(vec![CriterionEvidence {
            text: "test volume_discount passes".into(),
            path: Some("src/discount.rs".into()),
            line: Some(12),
        }]);
        let update = SetCriteriaParams {
            session: id.clone(),
            criteria: vec![met],
            replace: false,
        };
        let info = d
            .manager
            .set_criteria(update.clone(), CriterionAuthor::Agent)
            .unwrap();
        let ac1 = &info.criteria[0];
        assert_eq!(ac1.status, CriterionStatus::Met);
        assert_eq!(
            ac1.evidence[0].path.as_deref(),
            Some("/work/app/src/discount.rs")
        );
        next_change(&mut changes, &id).await;

        // The same call again sends no change. The user's mark over the
        // daemon's scope (this server has no authenticator) is the user's.
        d.manager
            .set_criteria(update, CriterionAuthor::Agent)
            .unwrap();
        let info = client
            .request::<SetCriteria>(SetCriteriaParams {
                session: id.clone(),
                criteria: vec![criterion(Some("AC2"), None, Some(CriterionStatus::Failed))],
                replace: false,
            })
            .await
            .unwrap();
        assert_eq!(info.criteria[1].changed_by, CriterionAuthor::User);
        let seen = next_change(&mut changes, &id).await;
        assert_eq!(
            seen.criteria[1].status,
            CriterionStatus::Failed,
            "no change in between"
        );

        // Bad calls.
        let err = client
            .request::<SetCriteria>(SetCriteriaParams {
                session: id.clone(),
                criteria: vec![criterion(Some("AC9"), None, None)],
                replace: false,
            })
            .await
            .unwrap_err();
        assert_eq!(app_error(err), codes::INVALID_PARAMS);
        let err = client
            .request::<SetCriteria>(SetCriteriaParams {
                session: SessionId("nope".into()),
                ..set
            })
            .await
            .unwrap_err();
        assert_eq!(app_error(err), app_codes::UNKNOWN_SESSION);
        id
    };

    // A new daemon on the same database has them.
    let d = daemon(SessionStore::open(&db).unwrap(), &provider);
    let client = d.connect().await;
    let snap = client
        .request::<Get>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap();
    let got: Vec<_> = snap
        .info
        .criteria
        .iter()
        .map(|c| (c.id.as_str(), c.status, c.changed_by))
        .collect();
    assert_eq!(
        got,
        [
            ("AC1", CriterionStatus::Met, CriterionAuthor::Agent),
            ("AC2", CriterionStatus::Failed, CriterionAuthor::User),
        ]
    );
    assert_eq!(snap.info.criteria[0].evidence[0].line, Some(12));
    let listed = client
        .request::<List>(ListSessionsParams::default())
        .await
        .unwrap();
    assert_eq!(listed.sessions[0].criteria, snap.info.criteria);

    // A closed session takes no more.
    client
        .request::<Close>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap();
    let err = client
        .request::<SetCriteria>(SetCriteriaParams {
            session: id,
            criteria: vec![],
            replace: true,
        })
        .await
        .unwrap_err();
    assert_eq!(app_error(err), app_codes::SESSION_CLOSED);
}

fn current(snap: &SessionSnapshot, id: &str) -> ConfigValue {
    snap.config_options
        .iter()
        .find(|o| o.id == id)
        .unwrap_or_else(|| panic!("no option {id}: {:?}", snap.config_options))
        .value()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_options_are_set_and_kept_across_a_daemon_restart() {
    use coqo_agents::testing::{test_config_options, test_modes};

    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("coqo.db");
    let set = |id: &SessionId, config_id: &str, value: ConfigValue| SetConfigOptionParams {
        session: id.clone(),
        config_id: config_id.into(),
        value,
    };
    let start = [
        AgentUpdate::Modes(test_modes()),
        AgentUpdate::ConfigOptions(test_config_options()),
    ];
    let first = ScriptedProvider::new().on_start(start.clone());
    let id = {
        let d = daemon(SessionStore::open(&db).unwrap(), &first);
        let client = d.connect().await;
        let id = create(&client).await.id;
        let snap = snapshot(&client, &id).await;
        let ids: Vec<_> = snap.config_options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["model", "effort", "fast"], "the agent's order");

        for (config_id, value) in [
            ("model", ConfigValue::Value("sonnet".into())),
            ("effort", ConfigValue::Value("high".into())),
            ("fast", ConfigValue::Boolean(true)),
        ] {
            client
                .request::<SetConfigOption>(set(&id, config_id, value))
                .await
                .unwrap();
        }
        let snap = snapshot(&client, &id).await;
        assert_eq!(current(&snap, "model"), ConfigValue::Value("sonnet".into()));
        assert_eq!(current(&snap, "effort"), ConfigValue::Value("high".into()));
        assert_eq!(current(&snap, "fast"), ConfigValue::Boolean(true));
        assert!(first.calls().contains(&Call::SetConfigOption(
            "effort".into(),
            ConfigValue::Value("high".into())
        )));

        // Unknown options, values the option doesn't take and the wrong
        // type are refused before they reach the agent.
        for (config_id, value) in [
            ("nope", ConfigValue::Boolean(true)),
            ("model", ConfigValue::Value("gpt".into())),
            ("fast", ConfigValue::Value("on".into())),
        ] {
            let err = client
                .request::<SetConfigOption>(set(&id, config_id, value))
                .await
                .unwrap_err();
            assert_eq!(app_error(err), codes::INVALID_PARAMS);
        }
        let sets = first
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::SetConfigOption(..)))
            .count();
        assert_eq!(sets, 3);
        id
    };

    // A new daemon: the resume carries the chosen values, and an agent
    // that kept them changes nothing.
    let second = ScriptedProvider::new().on_start(start);
    let d = daemon(SessionStore::open(&db).unwrap(), &second);
    let client = d.connect().await;
    let head = snapshot(&client, &id).await.head;
    prompt(&client, &id, "hi").await.unwrap();
    let snap = wait_for(&client, &id, |s| s.info.state == AgentState::Done).await;
    let resumed = second.resume_options();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0], snap.config_options);
    assert_eq!(current(&snap, "effort"), ConfigValue::Value("high".into()));
    let mut sub = subscribe(&client, &id, Some(head)).await;
    let after = until(&mut sub, turn_ended).await;
    assert!(
        !after
            .iter()
            .any(|u| matches!(u.event, SessionUpdate::ConfigOptions { .. })),
        "{after:?}"
    );
}

async fn rename(
    client: &Client,
    id: &SessionId,
    title: &str,
) -> Result<SessionInfo, ProtocolError> {
    client
        .request::<Rename>(RenameParams {
            session: id.clone(),
            title: title.into(),
        })
        .await
}

async fn listed_title(client: &Client, id: &SessionId) -> Option<String> {
    let list = client
        .request::<List>(ListSessionsParams {
            workspace: Some("ws-1".into()),
            include_closed: true,
        })
        .await
        .unwrap();
    list.sessions
        .into_iter()
        .find(|s| &s.id == id)
        .expect("listed")
        .title
}

/// Prompts and waits for the turn to end.
async fn turn(client: &Client, sub: &mut Subscription<Updates>, id: &SessionId, t: &str) {
    prompt(client, id, t).await.unwrap();
    until(sub, turn_ended).await;
}

/// Waits for a `sessions/changes` event of session `id` with title `title`.
async fn titled(changes: &mut Subscription<Changes>, id: &SessionId, title: &str) {
    while next_change(changes, id).await.title.as_deref() != Some(title) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn titles_come_from_the_first_prompt_the_agent_and_the_user() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("coqo.db");
    let title = |t: &str| Step::Update(AgentUpdate::Title(t.into()));
    let first = ScriptedProvider::new()
        .turn([chunk("on it"), Step::Stop(StopReason::EndTurn)])
        .turn([title("  Flaky\nIT fix "), Step::Stop(StopReason::EndTurn)])
        .turn([
            title("The agent's later title"),
            Step::Stop(StopReason::EndTurn),
        ]);
    let id = {
        let d = daemon(SessionStore::open(&db).unwrap(), &first);
        let client = d.connect().await;
        let created = client
            .request::<Create>(CreateSessionParams {
                provider: "fake".into(),
                cwd: "/work/app".into(),
                workspace: Some("ws-1".into()),
                title: None,
                mode: None,
            })
            .await
            .unwrap();
        let id = created.id;
        assert_eq!(created.title, None, "untitled until the first prompt");
        let mut changes = client
            .subscribe::<Changes>(ChangesParams::default(), None)
            .await
            .unwrap();
        let mut sub = subscribe(&client, &id, Some(0)).await;

        // The first prompt titles it, on `sessions/changes` too (the sidebar
        // shows it without opening the chat).
        turn(
            &client,
            &mut sub,
            &id,
            "## Fix the **flaky** OrderControllerIT\n\nIt fails on CI about once a day.",
        )
        .await;
        let derived = "Fix the flaky OrderControllerIT";
        assert_eq!(listed_title(&client, &id).await.as_deref(), Some(derived));
        titled(&mut changes, &id, derived).await;

        // The agent's own title replaces the derived one; it is not part of
        // the transcript.
        turn(&client, &mut sub, &id, "and the other one").await;
        assert_eq!(
            listed_title(&client, &id).await.as_deref(),
            Some("Flaky IT fix")
        );
        let transcript = until(&mut subscribe(&client, &id, Some(0)).await, |u| {
            matches!(u, SessionUpdate::TurnEnded { turn: 2, .. })
        })
        .await;
        assert!(transcript.iter().all(|u| kind(&u.event) != "title"));

        // The user's title wins over the agent's, from then on.
        let renamed = rename(&client, &id, "  Volume\n discount ").await.unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Volume discount"));
        titled(&mut changes, &id, "Volume discount").await;
        turn(&client, &mut sub, &id, "one more").await;
        assert_eq!(
            listed_title(&client, &id).await.as_deref(),
            Some("Volume discount")
        );

        let bad = rename(&client, &id, " \n ").await.unwrap_err();
        assert_eq!(app_error(bad), codes::INVALID_PARAMS);
        let unknown = rename(&client, &SessionId("nope".into()), "x")
            .await
            .unwrap_err();
        assert_eq!(app_error(unknown), app_codes::UNKNOWN_SESSION);
        id
    };

    // After a restart the user's title is kept, even when the resumed agent
    // reports its own again.
    let second = ScriptedProvider::new()
        .on_resume([AgentUpdate::Title("Resumed title".into())])
        .turn([Step::Stop(StopReason::EndTurn)]);
    let d = daemon(SessionStore::open(&db).unwrap(), &second);
    let client = d.connect().await;
    assert_eq!(
        listed_title(&client, &id).await.as_deref(),
        Some("Volume discount")
    );
    let mut sub = subscribe(&client, &id, None).await;
    turn(&client, &mut sub, &id, "back again").await;
    assert_eq!(second.calls()[0], Call::Resume("fake-1".into()));
    assert_eq!(
        listed_title(&client, &id).await.as_deref(),
        Some("Volume discount")
    );

    // A closed chat can be renamed too.
    client
        .request::<Close>(SessionParams {
            session: id.clone(),
        })
        .await
        .unwrap();
    let renamed = rename(&client, &id, "Volume discount, done").await.unwrap();
    assert!(renamed.closed);
    assert_eq!(
        listed_title(&client, &id).await.as_deref(),
        Some("Volume discount, done")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_title_given_at_create_is_the_users() {
    let provider = ScriptedProvider::new().turn([
        Step::Update(AgentUpdate::Title("Agent title".into())),
        Step::Stop(StopReason::EndTurn),
    ]);
    let d = daemon(SessionStore::open_in_memory().unwrap(), &provider);
    let client = d.connect().await;
    let id = create(&client).await.id;
    let mut sub = subscribe(&client, &id, Some(0)).await;
    turn(&client, &mut sub, &id, "Something else entirely").await;
    assert_eq!(listed_title(&client, &id).await.as_deref(), Some("Test"));
}
