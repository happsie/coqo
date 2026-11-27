// SPDX-License-Identifier: Apache-2.0

//! `threads/*` and `review/*` end to end: a protocol client, the workspace,
//! git and thread services and the real `coqo.git` mod, on fixture
//! repositories: anchors through edits above them (in the work tree and the
//! index) and a restart, replies, resolve, the pending review, the changes
//! feed and agent replies.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coqo_kernel::mods::BuiltinMods;
use coqo_mod_git::testing::Fixture;
use coqo_protocol::methods::agents::SessionId;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::git::DiffSide;
use coqo_protocol::methods::questions::AnchorStatus;
use coqo_protocol::methods::threads::{
    self as tm, Author, ChangesParams, CommentParams, CreateParams, EditCommentParams, ListParams,
    ReplyParams, ReviewParams, Thread, ThreadEvent, ThreadLines, ThreadParams, ThreadSide,
    ThreadStatus,
};
use coqo_protocol::methods::workspace::{self as wsm, OpenParams, WorkspaceInfo};
use coqo_protocol::{
    Client, ClientConfig, ErrorObject, Method, ProtocolError, Router, Server, ServerConfig,
};
use coqo_storage::DataDirs;
use futures_util::StreamExt as _;
use tempfile::TempDir;

use super::{ThreadService, register};
use crate::git::GitService;
use crate::mods::Mods;
use crate::workspace::{self, WorkspaceService};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Twelve numbered lines.
fn numbered() -> String {
    (1..=12).map(|i| format!("line {i}\n")).collect()
}

struct Harness {
    svc: Arc<ThreadService>,
    client: Client,
    mods: Mods,
}

/// The services over data in `data`, served in-process.
async fn harness(data: &Path) -> Harness {
    let workspaces = Arc::new(WorkspaceService::open(DataDirs::at(data)).unwrap());
    let mods = Mods::load(
        BuiltinMods::new().with(coqo_mod_git::MANIFEST, coqo_mod_git::GitMod::default()),
    );
    mods.start().await;
    let git = GitService::new(Arc::clone(&workspaces), mods.clone());
    let svc = ThreadService::open(
        &DataDirs::at(data).global_db(),
        Arc::clone(&workspaces),
        git,
    )
    .unwrap();
    let router = workspace::register(Router::new(), workspaces);
    let router = register(router, Arc::clone(&svc));
    let server = Server::new(router, ServerConfig::new("coqod", "test"));
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move { server.serve(server_io).await });
    let (client, _) = Client::connect(client_io, ClientConfig::new("test", "test"))
        .await
        .unwrap();
    Harness { svc, client, mods }
}

impl Harness {
    async fn call<M: Method>(&self, params: M::Params) -> M::Result {
        self.client.request::<M>(params).await.unwrap()
    }

    async fn fail<M: Method>(&self, params: M::Params) -> ErrorObject
    where
        M::Result: std::fmt::Debug,
    {
        match self.client.request::<M>(params).await {
            Err(ProtocolError::Rpc(e)) => e,
            other => panic!("{} should fail, got {other:?}", M::NAME),
        }
    }

    async fn open(&self, path: &Path) -> WorkspaceInfo {
        self.call::<wsm::Open>(OpenParams {
            path: Some(path.to_str().unwrap().to_owned()),
            ..OpenParams::default()
        })
        .await
    }

    async fn list(&self, ws: &WorkspaceInfo) -> tm::ThreadList {
        self.call::<tm::List>(ListParams {
            workspace: ws.id.clone(),
            root: None,
            path: None,
            diff: None,
        })
        .await
    }

    async fn pending(&self, ws: &WorkspaceInfo) -> tm::PendingReview {
        self.call::<tm::Pending>(ReviewParams {
            workspace: ws.id.clone(),
            diff: None,
        })
        .await
    }

    async fn stop(self) {
        self.mods.shutdown().await;
    }
}

const DIFF: &str = "git:0:index..worktree";

fn create(
    ws: &WorkspaceInfo,
    version: DiffSide,
    lines: Option<(u32, u32)>,
    body: &str,
) -> CreateParams {
    CreateParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
        path: "src/lib.rs".into(),
        side: if version == DiffSide::Worktree {
            ThreadSide::New
        } else {
            ThreadSide::Old
        },
        version,
        lines: lines.map(|(start, end)| ThreadLines { start, end }),
        diff: Some(DIFF.into()),
        body: body.into(),
        pending: true,
    }
}

fn repo_fixture() -> (TempDir, Fixture) {
    let tmp = tempfile::tempdir().unwrap();
    let f = Fixture::init(&tmp.path().join("repo"));
    f.write("src/lib.rs", &numbered());
    f.commit_all("initial");
    (tmp, f)
}

fn lines(t: &Thread) -> (u32, u32) {
    let l = t.anchor.lines.unwrap();
    (l.start, l.end)
}

#[tokio::test]
async fn a_thread_survives_edits_above_it_and_a_restart() {
    let (_tmp, f) = repo_fixture();
    let data = tempfile::tempdir().unwrap();
    // The agent's change: line 8 rewritten.
    f.write("src/lib.rs", &numbered().replace("line 8\n", "eight\n"));
    let h = harness(data.path()).await;
    let ws = h.open(f.path()).await;

    let t = h
        .call::<tm::Create>(create(
            &ws,
            DiffSide::Worktree,
            Some((8, 8)),
            "  Why eight?  ",
        ))
        .await;
    assert_eq!(lines(&t), (8, 8));
    assert_eq!(t.anchor.code.as_ref().unwrap().text, "eight\n");
    assert_eq!(t.anchor.resolution.unwrap().status, AnchorStatus::Exact);
    assert_eq!(t.comments.len(), 1);
    let c = &t.comments[0];
    assert_eq!(
        (c.author.clone(), c.body.as_str(), c.pending),
        (Author::User, "Why eight?", true)
    );
    assert_eq!(t.diff.as_deref(), Some(DIFF));
    assert_eq!(h.pending(&ws).await.count, 1);

    // Two lines added above (saved): the thread moves down with its code.
    f.write(
        "src/lib.rs",
        &numbered()
            .replace("line 8\n", "eight\n")
            .replace("line 2\n", "line 2\nnew a\nnew b\n"),
    );
    let listed = h.list(&ws).await;
    assert_eq!(listed.threads.len(), 1);
    let moved = &listed.threads[0];
    assert_eq!(moved.anchor.resolution.unwrap().status, AnchorStatus::Moved);
    assert_eq!(lines(moved), (10, 10));
    h.stop().await;

    // After a restart: still there, still pending, found where it moved.
    let h = harness(data.path()).await;
    let ws = h.open(f.path()).await;
    let listed = h.list(&ws).await;
    assert_eq!(listed.threads.len(), 1);
    let again = &listed.threads[0];
    assert_eq!(again.id, t.id);
    assert_eq!(lines(again), (10, 10));
    assert_eq!(again.comments, t.comments);
    let review = h.pending(&ws).await;
    assert_eq!(review.count, 1);
    assert_eq!(review.comments[0].thread, t.id);
    assert_eq!(
        review.comments[0].anchor.lines,
        Some(ThreadLines { start: 10, end: 10 })
    );

    // The code deleted: orphaned, keeping where it was last.
    f.write("src/lib.rs", "line 1\n");
    let gone = h.list(&ws).await.threads.remove(0);
    assert!(gone.anchor.is_orphaned());
    assert_eq!(lines(&gone), (10, 10));
    h.stop().await;
}

#[tokio::test]
async fn an_old_side_thread_follows_the_index() {
    let (_tmp, f) = repo_fixture();
    let data = tempfile::tempdir().unwrap();
    f.write("src/lib.rs", &numbered().replace("line 5\n", ""));
    let h = harness(data.path()).await;
    let ws = h.open(f.path()).await;
    // A comment on the removed line, on the old side (the index).
    let t = h
        .call::<tm::Create>(create(&ws, DiffSide::Index, Some((5, 5)), "Why remove it?"))
        .await;
    assert_eq!(t.anchor.side, ThreadSide::Old);
    assert_eq!(t.anchor.code.as_ref().unwrap().text, "line 5\n");
    // A line added at the top is staged: the index moved.
    f.write("src/lib.rs", &format!("top\n{}", numbered()));
    f.git(&["add", "src/lib.rs"]);
    f.write(
        "src/lib.rs",
        &format!("top\n{}", numbered()).replace("line 5\n", ""),
    );
    let moved = h.list(&ws).await.threads.remove(0);
    assert_eq!(moved.anchor.resolution.unwrap().status, AnchorStatus::Moved);
    assert_eq!(lines(&moved), (6, 6));
    // A version without the file, or lines past its end, are refused.
    let e = h
        .fail::<tm::Create>(CreateParams {
            path: "missing.rs".into(),
            ..create(&ws, DiffSide::Index, Some((1, 1)), "x")
        })
        .await;
    assert_eq!(
        e.code,
        coqo_protocol::codes::INVALID_PARAMS,
        "{}",
        e.message
    );
    let e = h
        .fail::<tm::Create>(create(&ws, DiffSide::Worktree, Some((40, 41)), "x"))
        .await;
    assert!(e.message.contains("past its end"), "{}", e.message);
    let e = h
        .fail::<tm::Create>(create(&ws, DiffSide::Worktree, Some((1, 1)), "   "))
        .await;
    assert_eq!(e.code, coqo_protocol::codes::INVALID_PARAMS);
    // A thread on the whole file needs no lines.
    let file = h
        .call::<tm::Create>(create(&ws, DiffSide::Worktree, None, "Split this file"))
        .await;
    assert!(file.anchor.code.is_none() && file.anchor.lines.is_none());
    h.stop().await;
}

#[tokio::test]
async fn reply_edit_resolve_and_the_pending_review() {
    let (_tmp, f) = repo_fixture();
    let data = tempfile::tempdir().unwrap();
    f.write("src/lib.rs", &numbered().replace("line 3\n", "three\n"));
    let h = harness(data.path()).await;
    let ws = h.open(f.path()).await;
    let first = h
        .call::<tm::Create>(create(
            &ws,
            DiffSide::Worktree,
            Some((3, 4)),
            "Rename these",
        ))
        .await;
    let second = h
        .call::<tm::Create>(create(&ws, DiffSide::Worktree, Some((1, 1)), "And this"))
        .await;
    let mut changes = h
        .client
        .subscribe::<tm::Changes>(
            ChangesParams {
                workspace: Some(ws.id.clone()),
            },
            Some(second.seq),
        )
        .await
        .unwrap();

    // A pending reply, then an agent's reply (#95 calls the service so).
    let replied = h
        .call::<tm::Reply>(ReplyParams {
            thread: first.id,
            body: "Both, please".into(),
            pending: true,
        })
        .await;
    assert_eq!(replied.comments.len(), 2);
    assert!(replied.seq > first.seq);
    let agent = Author::Agent {
        session: SessionId("s-1".into()),
    };
    let answered = h
        .svc
        .reply(
            ReplyParams {
                thread: first.id,
                body: "Renamed.".into(),
                pending: true,
            },
            agent.clone(),
        )
        .await
        .unwrap();
    let reply = answered.comments.last().unwrap();
    assert_eq!(reply.author, agent);
    assert!(!reply.pending, "only the user's comments wait in a review");
    assert_eq!(h.pending(&ws).await.count, 3);
    // The agent's comment is not the user's to change.
    let e = h
        .fail::<tm::EditComment>(EditCommentParams {
            comment: reply.id,
            body: "no".into(),
        })
        .await;
    assert_eq!(e.code, codes::NOT_YOUR_COMMENT);
    let e = h
        .fail::<tm::DeleteComment>(CommentParams { comment: reply.id })
        .await;
    assert_eq!(e.code, codes::NOT_YOUR_COMMENT);
    let e = h
        .fail::<tm::EditComment>(EditCommentParams {
            comment: tm::CommentId(999),
            body: "no".into(),
        })
        .await;
    assert_eq!(e.code, codes::UNKNOWN_COMMENT);

    // Edit the first comment; resolve, and reopen.
    let edited = h
        .call::<tm::EditComment>(EditCommentParams {
            comment: first.comments[0].id,
            body: "Rename both".into(),
        })
        .await;
    assert_eq!(edited.comments[0].body, "Rename both");
    assert!(edited.comments[0].edited_at.is_some());
    let resolved = h.call::<tm::Resolve>(ThreadParams { id: first.id }).await;
    assert_eq!(resolved.status, ThreadStatus::Resolved);
    assert!(resolved.resolved_at.is_some());
    let again = h.call::<tm::Resolve>(ThreadParams { id: first.id }).await;
    assert_eq!(again.seq, resolved.seq, "resolving twice changes nothing");
    let reopened = h.call::<tm::Reopen>(ThreadParams { id: first.id }).await;
    assert_eq!(
        (reopened.status, reopened.resolved_at),
        (ThreadStatus::Open, None)
    );

    // Deleting the only comment deletes the thread.
    let deleted = h
        .call::<tm::DeleteComment>(CommentParams {
            comment: second.comments[0].id,
        })
        .await;
    assert!(deleted.thread.is_none());
    let e = h.fail::<tm::Get>(ThreadParams { id: second.id }).await;
    assert_eq!(e.code, codes::UNKNOWN_THREAD);

    // The feed saw each change in order: four of the first, then the
    // deletion of the second.
    let mut seen = Vec::new();
    for _ in 0..6 {
        let e = tokio::time::timeout(TIMEOUT, changes.next())
            .await
            .expect("a change in time")
            .unwrap()
            .unwrap();
        seen.push(match e.event {
            ThreadEvent::Changed { thread } => {
                (thread.id, thread.comments.len(), thread.seq == e.seq)
            }
            ThreadEvent::Deleted { id, .. } => (id, 0, true),
        });
    }
    assert_eq!(
        seen,
        [
            (first.id, 2, true),
            (first.id, 3, true),
            (first.id, 3, true),
            (first.id, 3, true),
            (first.id, 3, true),
            (second.id, 0, true),
        ]
    );
    drop(changes);
    // Resuming from before them: each thread's newest state, once.
    let mut changes = h
        .client
        .subscribe::<tm::Changes>(ChangesParams::default(), Some(second.seq))
        .await
        .unwrap();
    let mut replay = Vec::new();
    for _ in 0..2 {
        let e = tokio::time::timeout(TIMEOUT, changes.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        replay.push(matches!(e.event, ThreadEvent::Deleted { .. }));
    }
    assert_eq!(replay, [false, true]);

    // Discard the review: the user's pending comments go; the thread keeps
    // the agent's reply.
    let discarded = h
        .call::<tm::Discard>(ReviewParams {
            workspace: ws.id.clone(),
            diff: Some(DIFF.into()),
        })
        .await;
    assert_eq!(discarded.discarded, 2);
    assert_eq!(h.pending(&ws).await.count, 0);
    let left = h.list(&ws).await.threads;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].comments.len(), 1);
    assert_eq!(left[0].comments[0].author, agent);
    h.stop().await;
}
