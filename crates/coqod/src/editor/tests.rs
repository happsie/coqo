// SPDX-License-Identifier: Apache-2.0

//! `editor/*` through the real router: a UI publishes its windows' context,
//! an agent's session token reads it for its own workspace only, and reveals
//! reach UIs that follow them.

use std::sync::Arc;
use std::time::Duration;

use coqo_protocol::methods::editor::{
    self as edm, EditorTab, GetContextParams, LineRange, Position, PublishContextParams, Range,
    RevealParams, RevealsParams, Selection, WindowContext,
};
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig, Server, ServerConfig, SessionToken, codes};
use futures_util::StreamExt as _;
use tempfile::TempDir;

use crate::daemon::DaemonContext;
use crate::routes;

const TIMEOUT: Duration = Duration::from_secs(5);

struct Harness {
    _dir: TempDir,
    ctx: DaemonContext,
    server: Server,
}

impl Harness {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = routes::tests::context(dir.path());
        let mut config = ServerConfig::new("coqod", "test");
        config.authenticator = Some(Arc::new(ctx.auth.clone()));
        let server = Server::new(routes::router(&ctx).unwrap(), config);
        Self {
            _dir: dir,
            ctx,
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

    async fn ui(&self) -> Client {
        self.connect(self.ctx.auth.daemon_token()).await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.ctx.shutdown.cancel();
    }
}

async fn workspace(ui: &Client, dir: &TempDir) -> WorkspaceInfo {
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    ui.request::<wsm::Open>(wsm::OpenParams {
        path: Some(dir.path().to_str().unwrap().to_owned()),
        ..wsm::OpenParams::default()
    })
    .await
    .unwrap()
}

fn tab(path: &str, active: bool) -> EditorTab {
    EditorTab {
        root: RootId(0),
        path: path.into(),
        active,
        dirty: false,
        selections: vec![Selection {
            range: Range::new(Position::new(0, 3), Position::new(0, 7)),
            text: Some("main".into()),
            truncated: false,
        }],
        visible: Some(LineRange { start: 0, end: 30 }),
    }
}

async fn publish(ui: &Client, ws: &WorkspaceInfo, window: &str, context: Option<WindowContext>) {
    ui.notify::<edm::PublishContext>(PublishContextParams {
        workspace: ws.id.clone(),
        window: window.into(),
        context,
    })
    .await
    .unwrap();
}

/// Ask until `ok` holds: notifications are not acknowledged.
async fn context_until(
    client: &Client,
    ws: &WorkspaceInfo,
    ok: impl Fn(&edm::EditorContext) -> bool,
) -> edm::EditorContext {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let cx = client
                .request::<edm::GetContext>(GetContextParams {
                    workspace: ws.id.clone(),
                })
                .await
                .unwrap();
            if ok(&cx) {
                return cx;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the context in time")
}

#[tokio::test]
async fn context_round_trip_and_scope() {
    let h = Harness::start();
    let ui = h.ui().await;
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = workspace(&ui, &a_dir).await;
    let b = workspace(&ui, &b_dir).await;
    let token = h.ctx.auth.mint_session(a.id.0.clone(), "s-1").unwrap();
    let agent = h.connect(token).await;

    // Nothing published yet.
    let cx = context_until(&agent, &a, |_| true).await;
    assert!(cx.windows.is_empty());

    publish(
        &ui,
        &a,
        "w-1",
        Some(WindowContext {
            focused: false,
            tabs: vec![tab("README.md", false)],
        }),
    )
    .await;
    publish(
        &ui,
        &a,
        "w-2",
        Some(WindowContext {
            focused: true,
            tabs: vec![tab("src/main.rs", true)],
        }),
    )
    .await;
    let cx = context_until(&agent, &a, |cx| cx.windows.len() == 2).await;
    // The focused window first.
    assert_eq!(cx.windows[0].window, "w-2");
    assert_eq!(cx.windows[0].context.tabs[0].path, "src/main.rs");
    assert_eq!(
        cx.windows[0].context.tabs[0].selections[0].text.as_deref(),
        Some("main")
    );

    // Another workspace is out of the session's reach.
    let err = agent
        .request::<edm::GetContext>(GetContextParams {
            workspace: b.id.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.rpc().map(|e| e.code), Some(codes::FORBIDDEN), "{err}");
    // And publishing takes the daemon token: dropped.
    publish(&agent, &a, "w-agent", Some(WindowContext::default())).await;

    // A window that switches to B leaves A.
    publish(&ui, &b, "w-1", Some(WindowContext::default())).await;
    let cx = context_until(&agent, &a, |cx| cx.windows.len() == 1).await;
    assert_eq!(cx.windows[0].window, "w-2");
    // Closing it with `null`.
    publish(&ui, &a, "w-2", None).await;
    context_until(&agent, &a, |cx| cx.windows.is_empty()).await;

    // A UI that goes away takes its windows with it.
    let other = h.ui().await;
    publish(&other, &a, "w-9", Some(WindowContext::default())).await;
    context_until(&agent, &a, |cx| cx.windows.len() == 1).await;
    drop(other);
    context_until(&agent, &a, |cx| cx.windows.is_empty()).await;
}

#[tokio::test]
async fn reveals_reach_the_ui() {
    let h = Harness::start();
    let ui = h.ui().await;
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = workspace(&ui, &a_dir).await;
    let b = workspace(&ui, &b_dir).await;
    let token = h.ctx.auth.mint_session(a.id.0.clone(), "s-1").unwrap();
    let agent = h.connect(token).await;
    let mut reveals = ui
        .subscribe::<edm::Reveals>(
            RevealsParams {
                workspace: Some(a.id.clone()),
            },
            None,
        )
        .await
        .unwrap();

    let reveal = |ws: &WorkspaceInfo, path: &str| RevealParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
        path: path.into(),
        range: Some(Range::new(Position::new(0, 3), Position::new(0, 7))),
    };
    // No window shows A yet: published, but nobody sees it.
    let r = agent
        .request::<edm::Reveal>(reveal(&a, "src/main.rs"))
        .await
        .unwrap();
    assert!(!r.delivered);
    publish(&ui, &a, "w-1", Some(WindowContext::default())).await;
    context_until(&agent, &a, |cx| !cx.windows.is_empty()).await;
    let r = agent
        .request::<edm::Reveal>(reveal(&a, "src/main.rs"))
        .await
        .unwrap();
    assert!(r.delivered);

    for _ in 0..2 {
        let event = tokio::time::timeout(TIMEOUT, reveals.next())
            .await
            .expect("a reveal in time")
            .expect("open")
            .expect("ok")
            .event;
        assert_eq!(event.path, "src/main.rs");
        assert_eq!(event.session.as_deref(), Some("s-1"));
        assert_eq!(event.range.unwrap().end, Position::new(0, 7));
    }

    // Missing files, folders, backwards ranges and other workspaces are refused.
    for (params, code) in [
        (
            reveal(&a, "nope.rs"),
            coqo_protocol::methods::codes::NOT_FOUND,
        ),
        (reveal(&a, "src"), coqo_protocol::methods::codes::NOT_A_FILE),
        (
            reveal(&a, "../outside"),
            coqo_protocol::methods::codes::PATH_OUTSIDE_WORKSPACE,
        ),
        (
            RevealParams {
                range: Some(Range::new(Position::new(2, 0), Position::new(1, 0))),
                ..reveal(&a, "src/main.rs")
            },
            codes::INVALID_PARAMS,
        ),
        (reveal(&b, "src/main.rs"), codes::FORBIDDEN),
    ] {
        let err = agent
            .request::<edm::Reveal>(params.clone())
            .await
            .unwrap_err();
        assert_eq!(err.rpc().map(|e| e.code), Some(code), "{params:?}: {err}");
    }
    // Following reveals takes the daemon token.
    let err = agent
        .subscribe::<edm::Reveals>(RevealsParams::default(), None)
        .await
        .err()
        .expect("refused");
    assert_eq!(err.rpc().map(|e| e.code), Some(codes::FORBIDDEN), "{err}");
}
