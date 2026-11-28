// SPDX-License-Identifier: Apache-2.0

//! Anchored items follow renames (issue #288): the path arithmetic, the
//! watcher's own-rename skip, and every store end to end against the
//! daemon's real router: an agent's question, the user's thread, a review
//! thread, a walkthrough and the editor context, through a file rename and
//! a folder rename with `fs/rename`, live on each changes topic, across a
//! restart, and (on Linux) through a rename made outside Coqo.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coqo_agents::AgentUpdate;
use coqo_agents::model::{self as agm, ContentBlock, CreateSessionParams, StopReason};
use coqo_agents::testing::{ScriptedProvider, Step};
use coqo_protocol::methods::editor::{
    self as edm, EditorTab, GetContextParams, LineRange, PublishContextParams, WindowContext,
};
use coqo_protocol::methods::fs::{self as fsm, RenameParams};
use coqo_protocol::methods::git::DiffSide;
use coqo_protocol::methods::questions::{
    self as qm, AnchorStatus, AskParams, NewAnchor, Question, QuestionKind,
};
use coqo_protocol::methods::threads::{self as tm, Thread, ThreadLines, ThreadSide};
use coqo_protocol::methods::walkthroughs::{self as wtm, Walkthrough, WalkthroughParams};
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceId, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig, Server, ServerConfig, SessionToken};
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use serde_json::json;
use tempfile::TempDir;

use super::{FollowsMoves, PathMove, PathMoves};
use crate::daemon::DaemonContext;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(10);

fn mv(from: &str, to: &str) -> PathMove {
    PathMove {
        workspace: WorkspaceId("ws".into()),
        root: RootId(1),
        from: from.into(),
        to: to.into(),
    }
}

#[test]
fn paths_at_or_under_the_entry_follow() {
    let m = mv("src", "lib");
    assert_eq!(m.apply(RootId(1), "src").as_deref(), Some("lib"));
    assert_eq!(
        m.apply(RootId(1), "src/a/b.rs").as_deref(),
        Some("lib/a/b.rs")
    );
    assert_eq!(m.apply(RootId(1), "srcx/b.rs"), None);
    assert_eq!(m.apply(RootId(1), "other.rs"), None);
    assert_eq!(m.apply(RootId(2), "src/a.rs"), None, "another root");
    assert_eq!(mv("", "x").apply(RootId(1), "a"), None);
    let mut p = "src/main.rs".to_owned();
    assert!(mv("src/main.rs", "src/app.rs").rekey(RootId(1), &mut p));
    assert_eq!(p, "src/app.rs");
    assert_eq!(mv("a", "b").reversed(), mv("b", "a"));
}

#[derive(Debug, Default)]
struct Count(Mutex<Vec<PathMove>>);

impl FollowsMoves for Count {
    fn path_moved(self: Arc<Self>, mv: PathMove) -> BoxFuture<'static, ()> {
        Box::pin(async move { self.0.lock().unwrap().push(mv) })
    }
}

#[tokio::test]
async fn the_watchers_report_of_an_own_rename_is_skipped() {
    let moves = PathMoves::new();
    let count = Arc::new(Count::default());
    moves.follow(Arc::clone(&count) as Arc<dyn FollowsMoves>);
    moves.moved(mv("a.rs", "b.rs")).await;
    moves.seen(mv("a.rs", "b.rs")).await;
    assert_eq!(count.0.lock().unwrap().len(), 1, "applied once");
    // Outside Coqo: applied.
    moves.seen(mv("b.rs", "c.rs")).await;
    assert_eq!(
        *count.0.lock().unwrap(),
        [mv("a.rs", "b.rs"), mv("b.rs", "c.rs")]
    );
}

// ---- end to end ------------------------------------------------------------------

const CART_RS: &str = "pub struct Cart {\n    lines: Vec<u32>,\n}\n\npub fn total(cart: &Cart) -> u32 {\n    cart.lines.iter().sum()\n}\n";
const TAX_RS: &str = "pub fn tax(cents: u32) -> u32 {\n    cents / 4\n}\n";

/// The daemon's router over data in `dir`, served in-process.
struct Daemon {
    ctx: DaemonContext,
    server: Server,
}

impl Daemon {
    async fn start(dir: &Path, agent: ScriptedProvider) -> Self {
        let ctx = routes::tests::context_with(dir, agent);
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

    async fn agent(&self, ws: &WorkspaceInfo, session: &str) -> Client {
        let token = self
            .ctx
            .auth
            .mint_session(ws.id.0.clone(), session)
            .unwrap();
        self.connect(token).await
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

/// What each store has, as the UI lists it.
struct Items {
    question: Question,
    user_thread: Question,
    review: Thread,
    walkthrough: Walkthrough,
}

async fn items(ui: &Client, ws: &WorkspaceInfo, walkthrough: &wtm::WalkthroughId) -> Items {
    let questions = ui
        .request::<qm::List>(qm::ListParams {
            workspace: ws.id.clone(),
            root: None,
            path: None,
            session: None,
            include_closed: true,
        })
        .await
        .unwrap()
        .questions;
    let by = |author: qm::QuestionAuthor| {
        questions
            .iter()
            .find(|q| q.author == author)
            .cloned()
            .unwrap()
    };
    let review = ui
        .request::<tm::List>(tm::ListParams {
            workspace: ws.id.clone(),
            root: None,
            path: None,
            diff: None,
        })
        .await
        .unwrap()
        .threads
        .remove(0);
    let walkthrough = ui
        .request::<wtm::Get>(WalkthroughParams {
            id: walkthrough.clone(),
        })
        .await
        .unwrap();
    Items {
        question: by(qm::QuestionAuthor::Agent),
        user_thread: by(qm::QuestionAuthor::User),
        review,
        walkthrough,
    }
}

impl Items {
    /// Every item's path, and whether its code is found there.
    fn paths(&self) -> Vec<(String, bool)> {
        let q = |q: &Question| {
            let a = &q.anchors[0];
            (
                a.anchor.path.clone(),
                a.resolution.status != AnchorStatus::Orphaned,
            )
        };
        let mut out = vec![q(&self.question), q(&self.user_thread)];
        out.push((
            self.review.anchor.path.clone(),
            self.review
                .anchor
                .resolution
                .is_some_and(|r| r.status != AnchorStatus::Orphaned),
        ));
        for a in self.walkthrough.steps.iter().flat_map(|s| &s.anchors) {
            out.push((
                a.anchor.path.clone(),
                a.resolution
                    .is_some_and(|r| r.status != AnchorStatus::Orphaned),
            ));
        }
        out
    }
}

fn found(paths: &[&str]) -> Vec<(String, bool)> {
    paths.iter().map(|p| ((*p).to_owned(), true)).collect()
}

async fn rename(ui: &Client, ws: &WorkspaceInfo, from: &str, to: &str) {
    ui.request::<fsm::Rename>(RenameParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
        from: from.into(),
        to: to.into(),
    })
    .await
    .unwrap();
}

/// The editor context's tab paths.
async fn context_paths(ui: &Client, ws: &WorkspaceInfo) -> Vec<String> {
    ui.request::<edm::GetContext>(GetContextParams {
        workspace: ws.id.clone(),
    })
    .await
    .unwrap()
    .windows
    .iter()
    .flat_map(|w| w.context.tabs.iter().map(|t| t.path.clone()))
    .collect()
}

#[tokio::test]
async fn every_anchored_item_follows_a_rename_and_keeps_it_across_a_restart() {
    let data = tempfile::tempdir().unwrap();
    let say = [
        Step::Update(AgentUpdate::AgentMessageChunk(ContentBlock::text(
            "It's a quarter.",
        ))),
        Step::Stop(StopReason::EndTurn),
    ];
    let daemon = Daemon::start(data.path(), ScriptedProvider::new().turn(say)).await;
    let ui = daemon.ui().await;
    let dir: TempDir = tempfile::tempdir().unwrap();
    for (path, text) in [("src/cart.rs", CART_RS), ("src/util/tax.rs", TAX_RS)] {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let ws = ui
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    let root = ws.roots[0].id;
    let session = ui
        .request::<agm::Create>(CreateSessionParams {
            provider: "fake".into(),
            cwd: dir.path().to_str().unwrap().into(),
            workspace: Some(ws.id.0.clone()),
            title: None,
            mode: None,
            from: None,
        })
        .await
        .unwrap()
        .id;
    let agent = daemon.agent(&ws, &session.0).await;

    // An agent's question on `total`, the user's thread on `tax`, a review
    // thread on the work tree's `total`, a walkthrough over both files, and
    // a window showing cart.rs.
    let anchor = |path: &str, start, end| NewAnchor {
        root,
        path: path.into(),
        lines: LineRange { start, end },
    };
    agent
        .request::<qm::Create>(qm::CreateParams {
            workspace: ws.id.clone(),
            session: session.0.as_str().into(),
            kind: QuestionKind::Clarification,
            body: "Why not saturating?".into(),
            options: Vec::new(),
            severity: None,
            anchors: vec![anchor("src/cart.rs", 4, 6)],
        })
        .await
        .unwrap();
    ui.request::<qm::Ask>(AskParams {
        workspace: ws.id.clone(),
        session: session.clone(),
        body: "Why a quarter?".into(),
        anchors: vec![anchor("src/util/tax.rs", 1, 1)],
    })
    .await
    .unwrap();
    ui.request::<tm::Create>(tm::CreateParams {
        workspace: ws.id.clone(),
        root,
        path: "src/cart.rs".into(),
        side: ThreadSide::default(),
        version: DiffSide::Worktree,
        lines: Some(ThreadLines { start: 6, end: 6 }),
        diff: None,
        body: "Overflow?".into(),
        pending: false,
    })
    .await
    .unwrap();
    let walkthrough = agent
        .request::<wtm::Create>(wtm::CreateParams {
            workspace: ws.id.clone(),
            session: session.0.as_str().into(),
            walkthrough: json!({
                "title": "Cart totals",
                "summary": "Totals and tax.",
                "scope": { "kind": "worktree" },
                "steps": [{
                    "id": "total",
                    "title": "The total",
                    "kind": "logic",
                    "anchors": [
                        { "path": "src/cart.rs", "start_line": 5, "end_line": 7 },
                        { "path": "src/util/tax.rs", "start_line": 1, "end_line": 3 },
                    ],
                    "what": "Sums the lines.",
                    "why": "One place.",
                }],
                "trivial_hunks": [{ "path": "src/util/tax.rs", "reason": "moved" }],
            }),
        })
        .await
        .unwrap()
        .id;
    let tab = |path: &str| EditorTab {
        root,
        path: path.into(),
        active: true,
        dirty: false,
        selections: Vec::new(),
        visible: None,
    };
    ui.notify::<edm::PublishContext>(PublishContextParams {
        workspace: ws.id.clone(),
        window: "w1".into(),
        context: Some(WindowContext {
            focused: true,
            tabs: vec![tab("src/cart.rs")],
        }),
    })
    .await
    .unwrap();
    until("the window's context", async || {
        (!context_paths(&ui, &ws).await.is_empty()).then_some(())
    })
    .await;
    let before = items(&ui, &ws, &walkthrough).await;
    assert_eq!(
        before.paths(),
        found(&[
            "src/cart.rs",
            "src/util/tax.rs",
            "src/cart.rs",
            "src/cart.rs",
            "src/util/tax.rs"
        ])
    );

    // Every store's changes topic, from now on.
    let mut questions = ui
        .subscribe::<qm::Changes>(
            qm::ChangesParams {
                workspace: Some(ws.id.clone()),
            },
            Some(before.question.seq.max(before.user_thread.seq)),
        )
        .await
        .unwrap();
    let mut threads = ui
        .subscribe::<tm::Changes>(
            tm::ChangesParams {
                workspace: Some(ws.id.clone()),
            },
            Some(before.review.seq),
        )
        .await
        .unwrap();
    let mut walkthroughs = ui
        .subscribe::<wtm::Changes>(
            wtm::ChangesParams {
                workspace: Some(ws.id.clone()),
            },
            None,
        )
        .await
        .unwrap();

    // Rename the file: the question, the review thread, the walkthrough's
    // first anchor and the window's tab follow it, found where they were.
    rename(&ui, &ws, "src/cart.rs", "src/basket.rs").await;
    let after = items(&ui, &ws, &walkthrough).await;
    assert_eq!(
        after.paths(),
        found(&[
            "src/basket.rs",
            "src/util/tax.rs",
            "src/basket.rs",
            "src/basket.rs",
            "src/util/tax.rs"
        ])
    );
    assert_eq!(context_paths(&ui, &ws).await, ["src/basket.rs"]);
    let q = tokio::time::timeout(TIMEOUT, questions.next())
        .await
        .expect("a question change")
        .unwrap()
        .unwrap();
    assert_eq!(q.event.anchors[0].anchor.path, "src/basket.rs");
    let t = tokio::time::timeout(TIMEOUT, threads.next())
        .await
        .expect("a thread change")
        .unwrap()
        .unwrap();
    let tm::ThreadEvent::Changed { thread } = t.event else {
        panic!("{:?}", t.event)
    };
    assert_eq!(thread.anchor.path, "src/basket.rs");
    let w = tokio::time::timeout(TIMEOUT, walkthroughs.next())
        .await
        .expect("a walkthrough change")
        .unwrap()
        .unwrap();
    assert_eq!(w.event.kind, wtm::ChangeKind::Updated);
    assert_eq!(
        w.event
            .summary
            .and_then(|s| s.start)
            .map(|s| s.path)
            .as_deref(),
        Some("src/basket.rs")
    );

    // Rename the folder: everything under it follows.
    rename(&ui, &ws, "src/util", "src/helpers").await;
    let after = items(&ui, &ws, &walkthrough).await;
    assert_eq!(
        after.paths(),
        found(&[
            "src/basket.rs",
            "src/helpers/tax.rs",
            "src/basket.rs",
            "src/basket.rs",
            "src/helpers/tax.rs"
        ])
    );
    assert_eq!(
        after.walkthrough.trivial_hunks[0].path,
        "src/helpers/tax.rs"
    );
    let q = tokio::time::timeout(TIMEOUT, questions.next())
        .await
        .expect("the thread's change")
        .unwrap()
        .unwrap();
    assert_eq!(q.event.anchors[0].anchor.path, "src/helpers/tax.rs");

    // Across a restart.
    drop((agent, ui, questions, threads, walkthroughs));
    daemon.stop().await;
    let daemon = Daemon::start(data.path(), ScriptedProvider::new()).await;
    let ui = daemon.ui().await;
    let ws = ui
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    let restarted = items(&ui, &ws, &walkthrough).await;
    assert_eq!(restarted.paths(), after.paths());

    // A rename made outside Coqo, while the workspace is watched: inotify
    // reports it as one, and the items follow.
    #[cfg(target_os = "linux")]
    {
        let _watching = ui
            .subscribe::<fsm::Changes>(
                fsm::ChangesParams {
                    workspace: ws.id.clone(),
                },
                None,
            )
            .await
            .unwrap();
        std::fs::rename(
            dir.path().join("src/basket.rs"),
            dir.path().join("src/cart.rs"),
        )
        .unwrap();
        until("the items to follow the outside rename", async || {
            let now = items(&ui, &ws, &walkthrough).await;
            (now.question.anchors[0].anchor.path == "src/cart.rs").then_some(now)
        })
        .await;
        let now = items(&ui, &ws, &walkthrough).await;
        assert_eq!(
            now.paths(),
            found(&[
                "src/cart.rs",
                "src/helpers/tax.rs",
                "src/cart.rs",
                "src/cart.rs",
                "src/helpers/tax.rs"
            ])
        );
    }
    drop(ui);
    daemon.stop().await;
}
