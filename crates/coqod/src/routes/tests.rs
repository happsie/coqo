// SPDX-License-Identifier: Apache-2.0

//! Every route of the real daemon router declares who may call it, and a
//! session token (what `coqod mcp` gets) reaches only the allowlist below,
//! and only for its own workspace (ADR 0007, *Socket security*).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use coqo_client::DaemonPaths;
use coqo_protocol::methods::buffer::{
    self as bufm, BufferId, ChangesParams, EditParams, OpenResult, ReplicaId, TextEdit,
};
use coqo_protocol::methods::workspace::{self as wsm, WorkspaceInfo};
use coqo_protocol::{
    AccessKind, Client, ClientConfig, Method as _, NotificationMethod as _, ProtocolError,
    RouteInfo, RouteKind, Server, ServerConfig, SessionToken, Topic as _, builtin, codes,
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
    ("syntax/folding", AccessKind::Check),
    ("syntax/outline", AccessKind::Check),
    ("syntax/symbols", AccessKind::Check),
    ("workspace/events", AccessKind::Check),
    ("workspace/get", AccessKind::Workspace),
    ("workspace/getState", AccessKind::Workspace),
];

pub(crate) fn context(dir: &Path) -> DaemonContext {
    DaemonContext {
        paths: DaemonPaths::at(dir.join("run")),
        data: DataDirs::at(dir.join("data")),
        activity: Activity::new(),
        idle: watch::channel(IdlePhase::Active).1,
        idle_policy: IdlePolicy::default(),
        shutdown: CancellationToken::new(),
        started_at: SystemTime::now(),
        started: tokio::time::Instant::now(),
        mods: Mods::load(test_builtins(&dir.join("mods"))),
        auth: Auth::generate().unwrap(),
        agent_fs: coqo_acp_host::FsHandle::new(),
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
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let server = self.server.clone();
        tokio::spawn(async move { server.serve(server_io).await });
        let mut config = ClientConfig::new("test", "test");
        config.token = Some(token);
        Client::connect(client_io, config).await.unwrap().0
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

async fn open_buffer(client: &Client, ws: &WorkspaceInfo, file: &str) -> OpenResult {
    client
        .request::<bufm::Open>(bufm::OpenParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: file.to_owned(),
            replica: None,
        })
        .await
        .unwrap()
}

/// One call to every session route but `buffer/edit` (a notification,
/// tested on its own), aimed at `ws` and its open buffer.
fn probes(
    ws: &WorkspaceInfo,
    file: &str,
    buffer: BufferId,
    replica: &ReplicaId,
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
    let admin = daemon.connect(daemon.ctx.auth.daemon_token()).await;
    let (_a_dir, a) = workspace(&admin, "a.txt").await;
    let (_b_dir, b) = workspace(&admin, "b.txt").await;
    let token = daemon
        .ctx
        .auth
        .mint_session(a.id.0.clone(), "session-1")
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
    let b_probes = probes(&b, "b.txt", b_buffer.buffer, &b_buffer.replica);
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
    for (name, kind, params) in probes(&a, "a.txt", a_buffer.buffer, &a_buffer.replica) {
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
