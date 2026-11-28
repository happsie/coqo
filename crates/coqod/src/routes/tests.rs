// SPDX-License-Identifier: Apache-2.0

//! Every route of the real daemon router declares who may call it, and a
//! session token (what `coqod mcp` gets) reaches only the allowlist below,
//! and only for its own workspace (ADR 0007, *Socket security*).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use coqo_agents::model::{self as agm, CreateSessionParams, SessionId};
use coqo_agents::register_provider;
use coqo_agents::testing::ScriptedProvider;
use coqo_client::DaemonPaths;
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError};
use coqo_protocol::methods::buffer::{
    self as bufm, BufferId, ChangesParams, EditParams, OpenResult, ReplicaId, TextEdit,
};
use coqo_protocol::methods::git::DiffSide;
use coqo_protocol::methods::threads::{self as tm, ThreadSide};
use coqo_protocol::methods::workspace::{self as wsm, WorkspaceInfo};
use coqo_protocol::{
    AccessKind, Client, ClientConfig, Method as _, NotificationMethod as _, Notifications,
    ProtocolError, RouteInfo, RouteKind, Server, ServerConfig, SessionToken, Topic as _, builtin,
    codes,
};
use coqo_storage::DataDirs;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::router;
use crate::auth::Auth;
use crate::daemon::DaemonContext;
use crate::lifecycle::{Activity, IdlePhase, IdlePolicy};
use crate::mods::Mods;
use crate::mods::tests::test_builtins;

/// The routes a session token may call, and how each binds it to its own
/// workspace. Every entry widens what an agent's `coqod mcp` can reach, so
/// add one only for a tool the bridge needs, and give it a cross-workspace
/// case in [`probes`].
const SESSION_ROUTES: &[(&str, AccessKind)] = &[
    ("buffer/changes", AccessKind::Check),
    ("buffer/close", AccessKind::Check),
    ("buffer/edit", AccessKind::Check),
    ("buffer/open", AccessKind::Workspace),
    ("editor/getContext", AccessKind::Workspace),
    ("editor/reveal", AccessKind::Workspace),
    ("fs/changes", AccessKind::Workspace),
    ("fs/list", AccessKind::Workspace),
    ("fs/read", AccessKind::Workspace),
    ("fs/stat", AccessKind::Workspace),
    ("mcp/call", AccessKind::Check),
    ("mcp/tools", AccessKind::Workspace),
    ("mcp/toolsChanged", AccessKind::Workspace),
    ("questions/create", AccessKind::Check),
    ("questions/watch", AccessKind::Check),
    ("review/reply", AccessKind::Check),
    ("sessions/setCriteria", AccessKind::Session),
    ("sessions/setStatus", AccessKind::Session),
    ("syntax/folding", AccessKind::Check),
    ("syntax/outline", AccessKind::Check),
    ("syntax/symbols", AccessKind::Check),
    ("walkthroughs/create", AccessKind::Check),
    ("workspace/events", AccessKind::Check),
    ("workspace/get", AccessKind::Workspace),
    ("workspace/getState", AccessKind::Workspace),
];

/// A built-in test mod (in the test daemons' mod table) that registers a
/// scripted agent provider, `fake`, once the mods start.
pub(crate) const FAKE_AGENT_MANIFEST: &str = r#"
id = "test.fake-agent"
name = "Fake agent"
version = "0.1.0"
engine = "^0.0"
tier = "core"
activation = ["startup"]
"#;

/// The mod behind [`FAKE_AGENT_MANIFEST`].
pub(crate) struct FakeAgent(pub ScriptedProvider);

impl Mod for FakeAgent {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            let info = AgentProviderInfo {
                id: "fake".parse()?,
                name: "Fake agent".into(),
                kind: AgentProviderKind::Native,
                description: None,
            };
            register_provider(&cx, info, std::sync::Arc::new(self.0.clone()))
        })
    }
}

pub(crate) fn context(dir: &Path) -> DaemonContext {
    context_with(dir, ScriptedProvider::new())
}

/// A test daemon's context whose `fake` agent provider (once its mods
/// start) is `agent`.
pub(crate) fn context_with(dir: &Path, agent: ScriptedProvider) -> DaemonContext {
    DaemonContext {
        paths: DaemonPaths::at(dir.join("run")),
        data: DataDirs::at(dir.join("data")),
        activity: Activity::new(),
        idle: watch::channel(IdlePhase::Active).1,
        idle_policy: IdlePolicy::default(),
        shutdown: CancellationToken::new(),
        started_at: SystemTime::now(),
        started: tokio::time::Instant::now(),
        mods: Mods::load(
            test_builtins(&dir.join("mods"))
                .with(
                    crate::mcp_tools::tests::PROBE_MANIFEST,
                    crate::mcp_tools::tests::Probe,
                )
                .with(FAKE_AGENT_MANIFEST, FakeAgent(agent)),
        ),
        auth: Auth::generate().unwrap(),
        agent_fs: coqo_acp_host::FsHandle::new(),
        agent_terminals: coqo_acp_host::TerminalHandle::new(),
    }
}

#[tokio::test]
async fn every_route_declares_who_may_call_it() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let routes = router(&ctx).unwrap().routes();
    assert!(routes.len() > 40, "{routes:?}");

    let undeclared: Vec<_> = routes
        .iter()
        .filter(|r| r.access.is_none())
        .map(|r| r.name)
        .collect();
    assert!(
        undeclared.is_empty(),
        "these routes do not declare an Access; register them with \
         method_with / notification_with / topic_with: {undeclared:?}"
    );

    let mut open: Vec<_> = routes
        .iter()
        .filter(|r| r.opens_to_sessions())
        .map(|r| (r.name, r.access.unwrap()))
        .collect();
    open.sort_by_key(|(name, _)| *name);
    assert_eq!(open, SESSION_ROUTES, "the session-token allowlist changed");
    ctx.shutdown.cancel();
}

/// The daemon's router and authentication, served in-process.
struct Daemon {
    _dir: TempDir,
    ctx: DaemonContext,
    routes: Vec<RouteInfo>,
    server: Server,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let router = router(&ctx).unwrap();
        let routes = router.routes();
        let mut config = ServerConfig::new("coqod", "test");
        config.authenticator = Some(Arc::new(ctx.auth.clone()));
        let server = Server::new(router, config);
        Self {
            _dir: dir,
            ctx,
            routes,
            server,
        }
    }

    async fn connect(&self, token: SessionToken) -> Client {
        self.connect_with_notes(token).await.0
    }

    async fn connect_with_notes(&self, token: SessionToken) -> (Client, Notifications) {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(server_io).await });
        let mut config = ClientConfig::new("test", "test");
        config.token = Some(token);
        Client::connect(client_io, config).await.unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.ctx.shutdown.cancel();
    }
}

fn code<T>(result: &Result<T, ProtocolError>) -> Option<i64> {
    result.as_ref().err().and_then(|e| e.rpc()).map(|e| e.code)
}

async fn subscribe_raw(
    client: &Client,
    topic: &str,
    params: Value,
) -> Result<Value, ProtocolError> {
    client
        .request_raw(
            builtin::Subscribe::NAME,
            Some(json!({ "topic": topic, "params": params })),
        )
        .await
}

/// A folder with one file, opened as a workspace with the daemon token.
async fn workspace(admin: &Client, file: &str) -> (TempDir, WorkspaceInfo) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(file), "text\n").unwrap();
    let info = admin
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    (dir, info)
}

/// A thread on the whole of `file`, by the daemon token; its id.
async fn file_thread(admin: &Client, ws: &WorkspaceInfo, file: &str) -> u64 {
    admin
        .request::<tm::Create>(tm::CreateParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: file.to_owned(),
            side: ThreadSide::New,
            version: DiffSide::Worktree,
            lines: None,
            diff: None,
            body: "Probe this.".into(),
            pending: true,
        })
        .await
        .unwrap()
        .id
        .0
}

async fn open_buffer(client: &Client, ws: &WorkspaceInfo, file: &str) -> OpenResult {
    reopen_buffer(client, ws, file, None).await.unwrap()
}

async fn reopen_buffer(
    client: &Client,
    ws: &WorkspaceInfo,
    file: &str,
    replica: Option<&ReplicaId>,
) -> Result<OpenResult, ProtocolError> {
    client
        .request::<bufm::Open>(bufm::OpenParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: file.to_owned(),
            replica: replica.cloned(),
        })
        .await
}

/// One call to every session route but `buffer/edit` (a notification,
/// tested on its own), aimed at `ws`, its open buffer and its agent session.
fn probes(
    ws: &WorkspaceInfo,
    file: &str,
    buffer: BufferId,
    replica: &ReplicaId,
    session: &SessionId,
    question: u64,
    thread: u64,
) -> Vec<(&'static str, RouteKind, Value)> {
    let id = json!({ "id": ws.id });
    let path = |p: &str| json!({ "workspace": ws.id, "root": ws.roots[0].id, "path": p });
    vec![
        ("workspace/get", RouteKind::Method, id.clone()),
        ("workspace/getState", RouteKind::Method, id),
        ("fs/list", RouteKind::Method, path("")),
        ("fs/stat", RouteKind::Method, path(file)),
        ("fs/read", RouteKind::Method, path(file)),
        ("buffer/open", RouteKind::Method, path(file)),
        (
            "editor/getContext",
            RouteKind::Method,
            json!({ "workspace": ws.id }),
        ),
        ("editor/reveal", RouteKind::Method, path(file)),
        (
            "mcp/tools",
            RouteKind::Method,
            json!({ "workspace": ws.id }),
        ),
        (
            "mcp/toolsChanged",
            RouteKind::Topic,
            json!({ "workspace": ws.id }),
        ),
        (
            "mcp/call",
            RouteKind::Method,
            json!({
                "workspace": ws.id,
                "session": session,
                "name": crate::mcp_tools::tests::PROBE_TOOL,
                "arguments": { "text": "probe" },
            }),
        ),
        // The question `questions/watch` follows is the one this creates.
        (
            "questions/create",
            RouteKind::Method,
            json!({
                "workspace": ws.id,
                "session": session,
                "kind": "clarification",
                "body": "probe?",
                "anchors": [{ "root": ws.roots[0].id, "path": file, "lines": { "start": 0, "end": 0 } }],
            }),
        ),
        (
            "questions/watch",
            RouteKind::Topic,
            json!({ "workspace": ws.id, "session": session, "id": question }),
        ),
        // A thread of this workspace: the agent may reply in it.
        (
            tm::AgentReply::NAME,
            RouteKind::Method,
            json!({ "workspace": ws.id, "session": session, "thread": thread, "body": "Probed." }),
        ),
        (
            agm::SetStatus::NAME,
            RouteKind::Method,
            json!({ "session": session, "status": "Probing" }),
        ),
        (
            agm::SetCriteria::NAME,
            RouteKind::Method,
            json!({ "session": session, "criteria": [{ "text": "Probing works" }] }),
        ),
        (
            "walkthroughs/create",
            RouteKind::Method,
            json!({
                "workspace": ws.id,
                "session": session,
                "walkthrough": {
                    "title": "Probe",
                    "summary": "A probe.",
                    "scope": { "kind": "worktree" },
                    "steps": [{
                        "id": "s1", "title": "The file", "kind": "logic",
                        "anchors": [{ "path": file, "start_line": 1 }],
                        "what": "Text.", "why": "Probing.",
                    }],
                },
            }),
        ),
        (
            "buffer/close",
            RouteKind::Method,
            json!({ "buffer": buffer, "replica": replica }),
        ),
        (
            "workspace/events",
            RouteKind::Topic,
            json!({ "workspace": ws.id }),
        ),
        (
            "fs/changes",
            RouteKind::Topic,
            json!({ "workspace": ws.id }),
        ),
        (
            "buffer/changes",
            RouteKind::Topic,
            json!({ "buffer": buffer }),
        ),
        (
            "syntax/symbols",
            RouteKind::Method,
            json!({ "buffer": buffer }),
        ),
        (
            "syntax/outline",
            RouteKind::Method,
            json!({ "buffer": buffer }),
        ),
        (
            "syntax/folding",
            RouteKind::Method,
            json!({ "buffer": buffer }),
        ),
    ]
}

async fn call(
    client: &Client,
    kind: RouteKind,
    name: &str,
    params: Value,
) -> Result<Value, ProtocolError> {
    match kind {
        RouteKind::Method => client.request_raw(name, Some(params)).await,
        RouteKind::Topic => subscribe_raw(client, name, params).await,
        RouteKind::Notification => unreachable!("no response to check"),
    }
}

/// For every service: a session token gets `FORBIDDEN` from every
/// daemon-level route, and from every route it may call when aimed at
/// another workspace. The daemon token reaches both workspaces.
#[tokio::test]
async fn a_session_token_reaches_only_its_own_workspace() {
    let daemon = Daemon::start();
    daemon.ctx.mods.start().await;
    let admin = daemon.connect(daemon.ctx.auth.daemon_token()).await;
    let (a_dir, a) = workspace(&admin, "a.txt").await;
    let (b_dir, b) = workspace(&admin, "b.txt").await;
    // An agent session in each workspace; the token is A's.
    let session_in = |dir: &TempDir, ws: &WorkspaceInfo| CreateSessionParams {
        provider: "fake".into(),
        cwd: dir.path().to_str().unwrap().to_owned(),
        workspace: Some(ws.id.0.clone()),
        title: None,
        mode: None,
    };
    let a_session = admin
        .request::<agm::Create>(session_in(&a_dir, &a))
        .await
        .unwrap()
        .id;
    let b_session = admin
        .request::<agm::Create>(session_in(&b_dir, &b))
        .await
        .unwrap()
        .id;
    let token = daemon
        .ctx
        .auth
        .mint_session(a.id.0.clone(), a_session.0.clone())
        .unwrap();
    let agent = daemon.connect(token).await;

    // Daemon-level routes, whatever the params: refused before they are read.
    let mut refused = 0;
    for route in daemon.routes.iter().filter(|r| !r.opens_to_sessions()) {
        let result = match route.kind {
            RouteKind::Notification => continue,
            kind => call(&agent, kind, route.name, Value::Null).await,
        };
        assert_eq!(
            code(&result),
            Some(codes::FORBIDDEN),
            "{}: {result:?}",
            route.name
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains(route.name), "{message}");
        refused += 1;
    }
    // workspace/*, terminal/*, agents/*, sessions/*, daemon/*, buffer/save…
    assert!(refused >= 30, "{refused}");

    // Session routes aimed at workspace B: refused.
    let b_buffer = open_buffer(&admin, &b, "b.txt").await;
    let a_buffer = open_buffer(&agent, &a, "a.txt").await;
    // Questions: the agent's own in A is 1; the daemon token's in B is 2.
    // Threads: A's is 1, B's is 2.
    let a_thread = file_thread(&admin, &a, "a.txt").await;
    let b_thread = file_thread(&admin, &b, "b.txt").await;
    assert_eq!((a_thread, b_thread), (1, 2));
    let b_probes = probes(
        &b,
        "b.txt",
        b_buffer.buffer,
        &b_buffer.replica,
        &b_session,
        2,
        b_thread,
    );
    let mut covered: Vec<_> = b_probes.iter().map(|(name, ..)| *name).collect();
    covered.push(bufm::Edit::NAME);
    covered.sort_unstable();
    let allowlist: Vec<_> = SESSION_ROUTES.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        covered, allowlist,
        "every session route needs a cross-workspace probe"
    );
    for (name, kind, params) in b_probes.clone() {
        let result = call(&agent, kind, name, params).await;
        assert_eq!(
            code(&result),
            Some(codes::FORBIDDEN),
            "{name} on B: {result:?}"
        );
    }
    // Its own session's status in workspace A, but another session's: refused.
    let other = agent
        .request::<agm::SetStatus>(agm::SetStatusParams {
            session: b_session.clone(),
            status: "Not mine".into(),
            state: None,
            progress: None,
        })
        .await;
    assert_eq!(code(&other), Some(codes::FORBIDDEN), "{other:?}");
    let other = agent
        .request::<agm::SetCriteria>(agm::SetCriteriaParams {
            session: b_session.clone(),
            criteria: vec![],
            replace: true,
        })
        .await;
    assert_eq!(code(&other), Some(codes::FORBIDDEN), "{other:?}");
    let other = agent
        .request_raw(
            "walkthroughs/create",
            Some(json!({ "workspace": a.id, "session": b_session, "walkthrough": {} })),
        )
        .await;
    assert_eq!(code(&other), Some(codes::FORBIDDEN), "{other:?}");
    // B's thread named as A's: not found, so nothing about it shows.
    let other = agent
        .request::<tm::AgentReply>(tm::AgentReplyParams {
            workspace: a.id.clone(),
            session: a_session.clone(),
            thread: tm::ThreadId(b_thread),
            body: "Not mine".into(),
            resolve: true,
        })
        .await;
    assert_eq!(
        code(&other),
        Some(coqo_protocol::methods::codes::UNKNOWN_THREAD),
        "{other:?}"
    );
    // An unfiltered workspace feed covers every workspace.
    let result = subscribe_raw(&agent, wsm::Events::NAME, json!({})).await;
    assert_eq!(code(&result), Some(codes::FORBIDDEN), "{result:?}");

    // Editing B's buffer is dropped; editing its own is committed.
    let edit = |buffer: &OpenResult| EditParams {
        buffer: buffer.buffer,
        replica: a_buffer.replica.clone(),
        base_rev: buffer.rev,
        client_seq: 1,
        edits: vec![TextEdit::insert(0, "agent: ")],
    };
    let mut a_changes = admin
        .subscribe::<bufm::Changes>(
            ChangesParams {
                buffer: a_buffer.buffer,
            },
            Some(a_buffer.rev),
        )
        .await
        .unwrap();
    agent.notify::<bufm::Edit>(edit(&b_buffer)).await.unwrap();
    agent.notify::<bufm::Edit>(edit(&a_buffer)).await.unwrap();
    let change = tokio::time::timeout(Duration::from_secs(5), a_changes.next())
        .await
        .expect("A's edit in time")
        .expect("open")
        .expect("ok");
    assert_eq!(change.event.edits, [TextEdit::insert(0, "agent: ")]);
    let b_now = open_buffer(&admin, &b, "b.txt").await;
    assert_eq!((b_now.rev, b_now.text.as_str()), (b_buffer.rev, "text\n"));

    // The same session routes aimed at its own workspace A: allowed.
    for (name, kind, params) in probes(
        &a,
        "a.txt",
        a_buffer.buffer,
        &a_buffer.replica,
        &a_session,
        1,
        a_thread,
    ) {
        let result = call(&agent, kind, name, params).await;
        assert!(result.is_ok(), "{name} on A: {result:?}");
    }

    // The daemon token reaches B, and the daemon-level routes.
    for (name, kind, params) in b_probes {
        let result = call(&admin, kind, name, params).await;
        assert!(
            result.is_ok(),
            "{name} on B with the daemon token: {result:?}"
        );
    }
    admin.request::<wsm::List>(()).await.unwrap();
    admin
        .request::<coqo_protocol::daemon::Status>(())
        .await
        .unwrap();
}

fn insert(buffer: &OpenResult, replica: &ReplicaId, client_seq: u64, text: &str) -> EditParams {
    EditParams {
        buffer: buffer.buffer,
        replica: replica.clone(),
        base_rev: buffer.rev,
        client_seq,
        edits: vec![TextEdit::insert(0, text)],
    }
}

/// In its own workspace, a session token edits, closes and reopens only
/// the replicas its connection opened (#198). The UI's replica of the same
/// buffer is `FORBIDDEN`; the daemon token is not restricted.
#[tokio::test]
async fn a_session_token_manages_only_the_replicas_it_opened() {
    let daemon = Daemon::start();
    let admin = daemon.connect(daemon.ctx.auth.daemon_token()).await;
    let (_dir, a) = workspace(&admin, "a.txt").await;
    let token = daemon
        .ctx
        .auth
        .mint_session(a.id.0.clone(), "session-1")
        .unwrap();
    let (agent, mut notes) = daemon.connect_with_notes(token.clone()).await;
    let ui = open_buffer(&admin, &a, "a.txt").await;
    let mine = open_buffer(&agent, &a, "a.txt").await;
    assert_eq!(ui.buffer, mine.buffer, "one buffer, two replicas");
    let mut changes = admin
        .subscribe::<bufm::Changes>(ChangesParams { buffer: ui.buffer }, Some(ui.rev))
        .await
        .unwrap();

    // The UI's replica: closing it, reopening it by id and editing as it
    // are refused. So is a replica id nobody handed out.
    let close = |replica: &ReplicaId| bufm::CloseParams {
        buffer: ui.buffer,
        replica: replica.clone(),
    };
    let result = agent.request::<bufm::Close>(close(&ui.replica)).await;
    assert_eq!(code(&result), Some(codes::FORBIDDEN), "{result:?}");
    let result = reopen_buffer(&agent, &a, "a.txt", Some(&ui.replica)).await;
    assert_eq!(code(&result), Some(codes::FORBIDDEN), "{result:?}");
    let made_up = ReplicaId("r-made-up".into());
    let result = reopen_buffer(&agent, &a, "a.txt", Some(&made_up)).await;
    assert_eq!(code(&result), Some(codes::FORBIDDEN), "{result:?}");
    agent
        .notify::<bufm::Edit>(insert(&ui, &ui.replica, 1, "forged "))
        .await
        .unwrap();
    let note = tokio::time::timeout(Duration::from_secs(5), notes.recv())
        .await
        .expect("buffer/editRejected in time")
        .unwrap();
    assert_eq!(note.method, bufm::EditRejected::NAME);
    let rejected: bufm::EditRejectedParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(rejected.error.code, codes::FORBIDDEN);
    assert_eq!(rejected.replica, ui.replica);

    // Its own replica: edits are committed, reopening by id and closing work.
    agent
        .notify::<bufm::Edit>(insert(&mine, &mine.replica, 1, "agent: "))
        .await
        .unwrap();
    let change = tokio::time::timeout(Duration::from_secs(5), changes.next())
        .await
        .expect("the agent's edit in time")
        .expect("open")
        .expect("ok")
        .event;
    assert_eq!(
        change.origin.id.as_ref(),
        Some(&mine.replica.0),
        "not the forged one"
    );
    assert_eq!(change.edits, [TextEdit::insert(0, "agent: ")]);
    let again = reopen_buffer(&agent, &a, "a.txt", Some(&mine.replica))
        .await
        .unwrap();
    assert_eq!(again.last_seq, 1);
    agent
        .request::<bufm::Close>(close(&mine.replica))
        .await
        .unwrap();
    // Closed: it no longer acts as that replica in this buffer.
    let result = agent.request::<bufm::Close>(close(&mine.replica)).await;
    assert_eq!(code(&result), Some(codes::FORBIDDEN), "{result:?}");

    // Another connection of the same session does not own them either.
    let other = daemon.connect(token).await;
    let result = reopen_buffer(&other, &a, "a.txt", Some(&mine.replica)).await;
    assert_eq!(code(&result), Some(codes::FORBIDDEN), "{result:?}");

    // The daemon token is unrestricted.
    let theirs = open_buffer(&agent, &a, "a.txt").await;
    admin
        .request::<bufm::Close>(close(&theirs.replica))
        .await
        .unwrap();
    admin
        .request::<bufm::Close>(close(&ui.replica))
        .await
        .unwrap();
}

/// Whether `replica` has left `ws`'s buffer of `file`: reopening it (with
/// the daemon token) no longer finds its last seq.
async fn has_left(admin: &Client, ws: &WorkspaceInfo, file: &str, replica: &ReplicaId) -> bool {
    let r = reopen_buffer(admin, ws, file, Some(replica)).await.unwrap();
    if r.last_seq == 0 {
        // The reopen joined it again; leave again so later checks are clean.
        admin
            .request::<bufm::Close>(bufm::CloseParams {
                buffer: r.buffer,
                replica: replica.clone(),
            })
            .await
            .unwrap();
        return true;
    }
    false
}

async fn eventually_left(admin: &Client, ws: &WorkspaceInfo, file: &str, replica: &ReplicaId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !has_left(admin, ws, file, replica).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "replica {replica} was not released"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A session connection's replicas are released when it closes, and when
/// its session's scope is revoked (which closes it).
#[tokio::test]
async fn a_session_connections_replicas_are_released_when_it_closes() {
    let daemon = Daemon::start();
    let admin = daemon.connect(daemon.ctx.auth.daemon_token()).await;
    let (_dir, a) = workspace(&admin, "a.txt").await;
    let ui = open_buffer(&admin, &a, "a.txt").await;
    let mut changes = admin
        .subscribe::<bufm::Changes>(ChangesParams { buffer: ui.buffer }, Some(ui.rev))
        .await
        .unwrap();
    let mut next_change = async || {
        tokio::time::timeout(Duration::from_secs(5), changes.next())
            .await
            .expect("the edit in time")
            .expect("open")
            .expect("ok")
    };
    // The UI's replica has a committed edit, so has_left can see it stay.
    admin
        .notify::<bufm::Edit>(insert(&ui, &ui.replica, 1, "ui "))
        .await
        .unwrap();
    next_change().await;
    let auth = &daemon.ctx.auth;
    for (session, revoke) in [("session-1", false), ("session-2", true)] {
        let token = auth.mint_session(a.id.0.clone(), session).unwrap();
        let agent = daemon.connect(token).await;
        let mine = open_buffer(&agent, &a, "a.txt").await;
        agent
            .notify::<bufm::Edit>(insert(&mine, &mine.replica, 1, "x"))
            .await
            .unwrap();
        next_change().await;
        assert!(!has_left(&admin, &a, "a.txt", &mine.replica).await);
        if revoke {
            assert!(auth.revoke_session(&a.id.0, session) > 0);
        } else {
            drop(agent);
        }
        eventually_left(&admin, &a, "a.txt", &mine.replica).await;
    }
    // The UI's replica stays.
    assert!(!has_left(&admin, &a, "a.txt", &ui.replica).await);
}
