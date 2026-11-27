// SPDX-License-Identifier: Apache-2.0

//! `walkthroughs/*` against the real router: publishing with every problem
//! reported at once, anchors pinned and resolved after edits, the scope's
//! revisions, both storage modes (local across a restart, committed into a
//! git repository), step states, deletion, the changes feed and the
//! session's transcript. The bridge's `publish_walkthrough` is tested end
//! to end in `mcp_tools::bridge_tests`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coqo_agents::model::{self as agm, CreateSessionParams, SessionParams, SessionUpdate};
use coqo_agents::testing::ScriptedProvider;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::questions::AnchorStatus;
use coqo_protocol::methods::walkthroughs::{
    self as wtm, ChangeKind, ChangesParams, CreateParams, DeleteParams, ListParams, ScopeKind,
    SetStepStateParams, StepKind, StepState, StorageMode, Walkthrough, WalkthroughParams,
    WalkthroughProblems,
};
use coqo_protocol::methods::workspace::{self as wsm, SetStateParams, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig, ProtocolError, Server, ServerConfig, SessionToken};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::daemon::DaemonContext;
use crate::routes;
use crate::walkthroughs::git::tests::repo;

const TIMEOUT: Duration = Duration::from_secs(10);

const DISCOUNT_RS: &str = "pub fn discount(units: u32) -> u32 {\n    if units >= 100 {\n        10\n    } else {\n        0\n    }\n}\n";
const TEST_RS: &str = "#[test]\nfn caps() {\n    assert_eq!(discount(100), 10);\n}\n";

struct Daemon {
    ctx: DaemonContext,
    server: Server,
}

impl Daemon {
    async fn start(dir: &Path) -> Self {
        let ctx = routes::tests::context_with(dir, ScriptedProvider::new());
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

/// A project (a git repository with one commit if `git`) opened as a
/// workspace.
async fn project(ui: &Client, git: bool) -> (TempDir, WorkspaceInfo) {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("src/discount.rs", DISCOUNT_RS),
        ("tests/discount.rs", TEST_RS),
    ];
    if git {
        repo(dir.path(), &files);
    } else {
        for (path, text) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }
    let info = ui
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.path().to_str().unwrap().to_owned()),
            ..wsm::OpenParams::default()
        })
        .await
        .unwrap();
    (dir, info)
}

/// An agent session in `ws`, with acceptance criterion AC1.
async fn session(ui: &Client, dir: &TempDir, ws: &WorkspaceInfo) -> String {
    let id = ui
        .request::<agm::Create>(CreateSessionParams {
            provider: "fake".into(),
            cwd: dir.path().to_str().unwrap().to_owned(),
            workspace: Some(ws.id.0.clone()),
            title: Some("Volume discount".into()),
            mode: None,
        })
        .await
        .unwrap()
        .id;
    ui.request::<agm::SetCriteria>(agm::SetCriteriaParams {
        session: id.clone(),
        criteria: vec![agm::CriterionInput {
            text: Some("100+ units get 10% off".into()),
            ..agm::CriterionInput::default()
        }],
        replace: true,
    })
    .await
    .unwrap();
    id.0
}

fn draft() -> Value {
    json!({
        "title": "Volume discount",
        "summary": "Orders of 100 units or more get 10% off.",
        "scope": { "kind": "worktree" },
        "steps": [{
            "id": "rule",
            "title": "The discount rule",
            "kind": "logic",
            "anchors": [{ "path": "src/discount.rs", "start_line": 2, "end_line": 6, "text": "if units >= 100" }],
            "what": "Returns the discount in percent.",
            "why": "One place for the rule.",
            "risks": [{ "severity": "med", "text": "99 units get nothing." }],
            "criteria_refs": ["AC1"],
        }, {
            "id": "tests",
            "title": "The test",
            "kind": "test",
            "anchors": [{ "path": "tests/discount.rs", "start_line": 2, "end_line": 4 }],
            "what": "Checks 100 units.",
            "why": "The boundary.",
            "depends_on": ["rule"],
        }],
        "trivial_hunks": [{ "path": "src/lib.rs", "reason": "module list" }],
    })
}

fn params(ws: &WorkspaceInfo, session: &str, walkthrough: Value) -> CreateParams {
    CreateParams {
        workspace: ws.id.clone(),
        session: session.into(),
        walkthrough,
    }
}

fn problems(r: Result<Walkthrough, ProtocolError>) -> Vec<String> {
    let e = r.unwrap_err();
    let e = e.rpc().expect("an error response").clone();
    assert_eq!(e.code, codes::INVALID_WALKTHROUGH, "{e:?}");
    let data: WalkthroughProblems = serde_json::from_value(e.data.unwrap()).unwrap();
    // The message spells them out too, for the agent.
    for p in &data.problems {
        assert!(e.message.contains(&p.to_string()), "{} in {}", p, e.message);
    }
    data.problems.iter().map(ToString::to_string).collect()
}

#[tokio::test]
async fn a_walkthrough_is_published_resolved_and_kept_across_a_restart() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path()).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui, true).await;
    let head = {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let session = session(&ui, &dir, &ws).await;
    let mut changes = ui
        .subscribe::<wtm::Changes>(ChangesParams::default(), None)
        .await
        .unwrap();
    let agent = daemon.agent(&ws, &session).await;

    let w = agent
        .request::<wtm::Create>(params(&ws, &session, draft()))
        .await
        .unwrap();
    assert!(w.id.0.starts_with("volume-discount-"), "{}", w.id);
    assert_eq!(w.created_by.session.0, session);
    assert_eq!(w.created_by.agent, "fake");
    assert_eq!(w.scope.kind, ScopeKind::Worktree);
    assert_eq!(w.scope.base.as_deref(), Some("HEAD"));
    assert_eq!(w.scope.base_commit.as_deref(), Some(head.as_str()));
    assert_eq!(w.storage.mode, StorageMode::Local);
    // The session's criterion, as it was.
    assert_eq!(w.acceptance_criteria[0].id, "AC1");
    assert_eq!(w.acceptance_criteria[0].text, "100+ units get 10% off");
    let rule = &w.steps[0];
    assert_eq!(rule.kind, StepKind::Logic);
    assert_eq!(rule.anchors[0].anchor.range.start.line, 1);
    assert!(
        rule.anchors[0]
            .anchor
            .text
            .starts_with("    if units >= 100 {\n")
    );
    assert_eq!(w.trivial_hunks[0].path, "src/lib.rs");

    let change = tokio::time::timeout(TIMEOUT, changes.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .event;
    assert_eq!(change.kind, ChangeKind::Published);
    let summary = change.summary.unwrap();
    assert_eq!(
        (summary.steps, summary.title.as_str()),
        (2, "Volume discount")
    );
    let start = summary.start.unwrap();
    assert_eq!(
        (start.path.as_str(), start.lines.start, start.lines.end),
        ("src/discount.rs", 1, 5)
    );

    // The session's transcript has it, for the chat's card.
    let snap = ui
        .request::<agm::Get>(SessionParams {
            session: agm::SessionId(session.clone()),
        })
        .await
        .unwrap();
    let mut updates = ui
        .subscribe::<agm::Updates>(
            SessionParams {
                session: agm::SessionId(session.clone()),
            },
            Some(0),
        )
        .await
        .unwrap();
    let mut recorded = None;
    for _ in 0..snap.head {
        let u = tokio::time::timeout(TIMEOUT, updates.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let SessionUpdate::Walkthrough(s) = u.event {
            recorded = Some(s);
        }
    }
    assert_eq!(recorded.unwrap().id, w.id);

    // Lines added above the rule: get finds it where it moved.
    std::fs::write(
        dir.path().join("src/discount.rs"),
        format!("// Discounts.\n\n{DISCOUNT_RS}"),
    )
    .unwrap();
    let got = ui
        .request::<wtm::Get>(WalkthroughParams { id: w.id.clone() })
        .await
        .unwrap();
    let r = got.steps[0].anchors[0].resolution.unwrap();
    assert_eq!(r.status, AnchorStatus::Moved);
    assert_eq!(r.range.unwrap().start.line, 3);
    assert_eq!(
        got.steps[1].anchors[0].resolution.unwrap().status,
        AnchorStatus::Exact
    );

    // The reader's progress.
    let s = ui
        .request::<wtm::SetStepState>(SetStepStateParams {
            id: w.id.clone(),
            step: "rule".into(),
            state: StepState::Understood,
        })
        .await
        .unwrap();
    assert_eq!(s.understood, 1);
    let e = ui
        .request::<wtm::SetStepState>(SetStepStateParams {
            id: w.id.clone(),
            step: "nope".into(),
            state: StepState::Question,
        })
        .await
        .unwrap_err();
    assert!(e.to_string().contains("its steps: rule, tests"), "{e}");

    // A restart: still there, with its progress.
    drop((ui, agent, changes, updates));
    daemon.stop().await;
    let daemon = Daemon::start(data.path()).await;
    let ui = daemon.ui().await;
    let list = ui
        .request::<wtm::List>(ListParams {
            workspace: ws.id.clone(),
            session: None,
        })
        .await
        .unwrap();
    assert_eq!(list.walkthroughs.len(), 1);
    assert_eq!(list.walkthroughs[0].understood, 1);
    let got = ui
        .request::<wtm::Get>(WalkthroughParams { id: w.id.clone() })
        .await
        .unwrap();
    assert_eq!(got.step_states.get("rule"), Some(&StepState::Understood));

    // Deleting it.
    assert!(
        ui.request::<wtm::Delete>(DeleteParams {
            id: w.id.clone(),
            delete_file: false,
        })
        .await
        .unwrap()
    );
    let e = ui
        .request::<wtm::Get>(WalkthroughParams { id: w.id.clone() })
        .await
        .unwrap_err();
    assert_eq!(e.rpc().unwrap().code, codes::UNKNOWN_WALKTHROUGH);
    daemon.stop().await;
}

#[tokio::test]
async fn every_problem_is_reported_at_once() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path()).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui, true).await;
    let session = session(&ui, &dir, &ws).await;
    let agent = daemon.agent(&ws, &session).await;

    let mut bad = draft();
    bad["steps"][0]["kind"] = json!("refactor");
    bad["steps"][0]["anchors"] = json!([
        { "path": "src/discount.rs", "start_line": 1, "end_line": 2, "text": "assert_eq!" },
        { "path": "src/discount.rs", "start_line": 7, "end_line": 30 },
        { "path": "src/gone.rs", "start_line": 1 },
        { "path": "../outside.rs", "start_line": 1 },
        { "path": "/etc/hosts", "start_line": 1 },
    ]);
    bad["steps"][1]["depends_on"] = json!(["rule", "nope"]);
    bad["steps"][1]["criteria_refs"] = json!(["AC9"]);
    bad["scope"] = json!({ "kind": "commits", "base": "no-such-branch", "head": "HEAD" });
    let got = problems(
        agent
            .request::<wtm::Create>(params(&ws, &session, bad))
            .await,
    );
    let root = format!("{} ({})", ws.roots[0].id.0, ws.roots[0].path);
    let want = [
        "steps[0].kind: `refactor` is not a step kind; use one of entry_point, flow, data_model, logic, edge_case, test, config, migration".to_owned(),
        "steps[1].criteria_refs[0]: no acceptance criterion `AC9`; the walkthrough's are AC1".to_owned(),
        "steps[1].depends_on[1]: no step has id `nope`; the step ids are rule, tests".to_owned(),
        format!("scope.base: `no-such-branch` is not a commit in root {root}; check the name (git branch -a, git log), and fetch a remote branch first"),
        format!("steps[0].anchors[4].path: /etc/hosts is outside the workspace; its roots are {root}"),
        "steps[0].anchors[0].text: `assert_eq!` is not in lines 1-2 of src/discount.rs (which start `pub fn discount(units: u32) -> u32 {`); it is not in the file at all; read src/discount.rs again and quote the code as it is now".to_owned(),
        "steps[0].anchors[1].end_line: src/discount.rs has 7 lines; line 30 is past its end".to_owned(),
        format!("steps[0].anchors[2].path: there is no file src/gone.rs in root {root}; check the path"),
        format!("steps[0].anchors[3].path: ../outside.rs is outside the workspace; use a path inside root {root}"),
    ];
    for w in &want {
        assert!(got.contains(w), "missing\n  {w}\nin {got:#?}");
    }
    assert_eq!(got.len(), want.len(), "{got:#?}");

    // Not a git repository: a named base cannot be resolved, a worktree
    // scope without one is fine.
    let (plain_dir, plain) = project(&ui, false).await;
    let plain_session = self::session(&ui, &plain_dir, &plain).await;
    let agent = daemon.agent(&plain, &plain_session).await;
    let mut v = draft();
    v["scope"] = json!({ "kind": "worktree", "base": "main" });
    let got = problems(
        agent
            .request::<wtm::Create>(params(&plain, &plain_session, v))
            .await,
    );
    assert_eq!(got.len(), 1, "{got:#?}");
    assert!(got[0].starts_with("scope.base: root "), "{got:#?}");
    assert!(got[0].contains("is not a git repository"), "{got:#?}");
    let w = agent
        .request::<wtm::Create>(params(&plain, &plain_session, draft()))
        .await
        .unwrap();
    assert_eq!((w.scope.base, w.scope.base_commit), (None, None));

    // Nothing refused was stored.
    let list = ui
        .request::<wtm::List>(ListParams {
            workspace: ws.id.clone(),
            session: None,
        })
        .await
        .unwrap();
    assert!(list.walkthroughs.is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn a_committed_walkthrough_is_written_to_the_repository() {
    let data = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(data.path()).await;
    let ui = daemon.ui().await;
    let (dir, ws) = project(&ui, true).await;
    let session = session(&ui, &dir, &ws).await;
    ui.request::<wsm::SetState>(SetStateParams {
        id: ws.id.clone(),
        key: wtm::STORAGE_STATE_KEY.into(),
        value: json!("committed"),
    })
    .await
    .unwrap();
    let agent = daemon.agent(&ws, &session).await;
    let w = agent
        .request::<wtm::Create>(params(&ws, &session, draft()))
        .await
        .unwrap();
    assert_eq!(w.storage.mode, StorageMode::Committed);
    assert!(w.storage.error.is_none(), "{:?}", w.storage);
    let rel = format!(".coqo/walkthroughs/{}.json", w.id);
    assert_eq!(w.storage.path.as_deref(), Some(rel.as_str()));
    let file = dir.path().join(&rel);
    let written: Walkthrough =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written, w);
    // Written, never committed: git sees an untracked file.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "?? .coqo/\n");
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(log.stdout).unwrap().trim(), "1");

    // Deleting it with its file.
    assert!(
        ui.request::<wtm::Delete>(DeleteParams {
            id: w.id.clone(),
            delete_file: true,
        })
        .await
        .unwrap()
    );
    assert!(!file.exists());
    daemon.stop().await;
}
