// SPDX-License-Identifier: Apache-2.0

//! Who changed which file: a scripted agent's tool calls, its writes, a
//! change made during its turn, a rename, and a restart. The ACP host's
//! side (a real `fs/write_text_file` and a tool-call diff from the fake ACP
//! agent) is in `tests/provenance.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use coqo_agents::testing::{ScriptedProvider, Step};
use coqo_agents::{AgentUpdate, ManagerOptions, SessionManager, SessionStore, StaticProviders};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::methods::agents::{
    ChangeStrength, ChangedByParams, ChangedSinceParams, ChangedSinceResult, ContentBlock,
    CreateSessionParams, SessionChange, SessionId, StopReason, ToolCall, ToolCallContent,
    ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use coqo_protocol::methods::workspace::{OpenParams, RootId, WorkspaceId};
use coqo_storage::DataDirs;
use tokio::sync::Notify;

use super::{Provenance, Signals, channel};
use crate::path_moves::{FollowsMoves, PathMove, PathMoves};
use crate::workspace::WorkspaceService;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    _dir: tempfile::TempDir,
    data: PathBuf,
    root: PathBuf,
    workspace: WorkspaceId,
    workspaces: Arc<WorkspaceService>,
    manager: SessionManager,
    signals: Signals,
    svc: Arc<Provenance>,
}

/// A workspace on a temporary folder, a session manager whose `fake`
/// provider is `provider`, and the provenance service between them.
async fn harness(provider: &ScriptedProvider) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let root = std::fs::canonicalize(dir.path()).unwrap().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
    let workspaces = Arc::new(WorkspaceService::open(DataDirs::at(&data)).unwrap());
    let workspace = workspaces
        .open_workspace(OpenParams {
            path: Some(root.to_string_lossy().into_owned()),
            ..OpenParams::default()
        })
        .await
        .unwrap()
        .id;
    let (signals, rx) = channel();
    let db = DataDirs::at(&data).global_db();
    let info = AgentProviderInfo {
        id: "fake".parse().unwrap(),
        name: "Fake agent".into(),
        kind: AgentProviderKind::Native,
        description: None,
    };
    let providers = StaticProviders::new().with(info, Arc::new(provider.clone()));
    let options = ManagerOptions {
        observer: Some(signals.observer()),
        ..ManagerOptions::default()
    };
    let manager = SessionManager::open(
        SessionStore::open(&db).unwrap(),
        Arc::new(providers),
        options,
    )
    .unwrap();
    let svc = Provenance::open(
        &db,
        Arc::clone(&workspaces),
        None,
        manager.clone(),
        &signals,
        rx,
    )
    .unwrap();
    Harness {
        _dir: dir,
        data,
        root,
        workspace,
        workspaces,
        manager,
        signals,
        svc,
    }
}

impl Harness {
    async fn session(&self, title: &str) -> SessionId {
        self.manager
            .create(CreateSessionParams {
                provider: "fake".into(),
                cwd: self.root.to_string_lossy().into_owned(),
                workspace: Some(self.workspace.0.clone()),
                title: Some(title.into()),
                mode: None,
                from: None,
            })
            .await
            .unwrap()
            .id
    }

    /// Runs one scripted turn of `id` to its end.
    async fn turn(&self, id: &SessionId) {
        let mut changes = self.manager.changes(None, None).unwrap().events;
        self.manager
            .prompt(id, vec![ContentBlock::text("go")])
            .await
            .unwrap();
        self.until_idle(id, &mut changes).await;
    }

    async fn until_idle(
        &self,
        id: &SessionId,
        changes: &mut futures_util::stream::BoxStream<
            'static,
            Result<
                coqo_protocol::Sequenced<coqo_protocol::methods::agents::SessionInfo>,
                coqo_protocol::ErrorObject,
            >,
        >,
    ) {
        use futures_util::StreamExt as _;
        tokio::time::timeout(TIMEOUT, async {
            while !self.manager.snapshot(id).is_ok_and(|s| !s.running) {
                changes.next().await;
            }
        })
        .await
        .expect("the turn ended");
        self.svc.settled().await;
    }

    async fn changed_by(&self, paths: &[&str]) -> Vec<SessionChange> {
        self.svc
            .changed_by(ChangedByParams {
                workspace: self.workspace.clone(),
                root: RootId(1),
                paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            })
            .await
            .unwrap()
            .sessions
    }

    fn path(&self, rel: &str) -> String {
        self.root.join(rel).to_string_lossy().into_owned()
    }
}

fn edit_call(id: &str, path: &str, kind: ToolKind, diff: bool) -> AgentUpdate {
    AgentUpdate::ToolCall(ToolCall {
        id: ToolCallId(id.into()),
        title: format!("Edit {path}"),
        name: None,
        kind,
        status: ToolCallStatus::Pending,
        content: if diff {
            vec![ToolCallContent::Diff {
                path: path.into(),
                old_text: Some("fn a() {}\n".into()),
                new_text: "fn b() {}\n".into(),
            }]
        } else {
            Vec::new()
        },
        locations: vec![ToolCallLocation {
            path: path.into(),
            line: None,
        }],
        raw_input: None,
        raw_output: None,
    })
}

fn status(id: &str, status: ToolCallStatus) -> AgentUpdate {
    AgentUpdate::ToolCallUpdate(ToolCallUpdate {
        id: ToolCallId(id.into()),
        status: Some(status),
        ..ToolCallUpdate::default()
    })
}

fn paths_of(c: &SessionChange) -> Vec<&str> {
    c.paths.iter().map(String::as_str).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_tool_calls_record_the_files_they_changed() {
    let provider = ScriptedProvider::new();
    let h = harness(&provider).await;
    let (lib, readme, notes, other) = (
        h.path("src/lib.rs"),
        h.path("README.md"),
        h.path("notes.md"),
        h.path("other.rs"),
    );
    provider.push_turn([
        // A diff (Claude Code's Edit), completed.
        Step::Update(edit_call("t1", &lib, ToolKind::Edit, true)),
        Step::Update(status("t1", ToolCallStatus::Completed)),
        // An edit with only a location, completed.
        Step::Update(edit_call("t2", &readme, ToolKind::Edit, false)),
        Step::Update(status("t2", ToolCallStatus::Completed)),
        // A read's location is no change.
        Step::Update(edit_call("t3", &notes, ToolKind::Read, false)),
        Step::Update(status("t3", ToolCallStatus::Completed)),
        // A rejected edit changed nothing.
        Step::Update(edit_call("t4", &other, ToolKind::Edit, true)),
        Step::Update(status("t4", ToolCallStatus::Failed)),
        Step::Stop(StopReason::EndTurn),
    ]);
    let s = h.session("Volume discount").await;
    h.turn(&s).await;

    let got = h
        .changed_by(&[
            "src/lib.rs",
            "README.md",
            "notes.md",
            "other.rs",
            "never.rs",
        ])
        .await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].session, s);
    assert_eq!(got[0].title.as_deref(), Some("Volume discount"));
    assert_eq!(got[0].provider, "fake");
    assert!(!got[0].closed);
    assert_eq!(got[0].strength, ChangeStrength::Write);
    assert_eq!(paths_of(&got[0]), ["README.md", "src/lib.rs"]);
}

/// `agents/changedSince` (#265): the files changed from a time on, each
/// counted once, for the session that changed it last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counts_the_files_changed_since_a_time() {
    let provider = ScriptedProvider::new();
    let h = harness(&provider).await;
    let (lib, readme, notes) = (
        h.path("src/lib.rs"),
        h.path("README.md"),
        h.path("notes.md"),
    );
    let edits = |calls: &[(&str, &str)]| {
        let mut steps: Vec<Step> = calls
            .iter()
            .flat_map(|(id, path)| {
                [
                    Step::Update(edit_call(id, path, ToolKind::Edit, true)),
                    Step::Update(status(id, ToolCallStatus::Completed)),
                ]
            })
            .collect();
        steps.push(Step::Stop(StopReason::EndTurn));
        steps
    };
    provider.push_turn(edits(&[("t1", &lib), ("t2", &readme)]));
    provider.push_turn(edits(&[("t3", &lib), ("t4", &notes)]));
    let first = h.session("First").await;
    let second = h.session("Second").await;
    h.turn(&first).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let between = coqo_storage::Timestamp::now().0;
    tokio::time::sleep(Duration::from_millis(5)).await;
    h.turn(&second).await;

    let since = |since: i64, workspace: Option<WorkspaceId>| {
        h.svc
            .changed_since(&ChangedSinceParams { since, workspace })
            .unwrap()
    };
    // From the start: three files, `src/lib.rs` the second session's.
    let all = since(0, None);
    assert_eq!(all.files, 3, "{all:?}");
    let by: Vec<_> = all.sessions.iter().map(|s| (&s.session, s.files)).collect();
    assert_eq!(by, [(&second, 2), (&first, 1)]);
    // Only what the second turn changed.
    let later = since(between, Some(h.workspace.clone()));
    assert_eq!(later.files, 2, "{later:?}");
    assert_eq!(later.sessions.len(), 1);
    assert_eq!(later.sessions[0].session, second);
    // Another workspace's, and from now on: nothing.
    assert_eq!(since(0, Some(WorkspaceId("elsewhere".into()))).files, 0);
    assert_eq!(since(i64::MAX, None), ChangedSinceResult::default());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_last_session_wins_and_closing_it_keeps_the_record() {
    let provider = ScriptedProvider::new();
    let h = harness(&provider).await;
    let lib = h.path("src/lib.rs");
    for _ in 0..2 {
        provider.push_turn([
            Step::Update(edit_call("t1", &lib, ToolKind::Edit, true)),
            Step::Update(status("t1", ToolCallStatus::Completed)),
            Step::Stop(StopReason::EndTurn),
        ]);
    }
    let first = h.session("First").await;
    let second = h.session("Second").await;
    h.turn(&first).await;
    // Timestamps are in milliseconds.
    tokio::time::sleep(Duration::from_millis(5)).await;
    h.turn(&second).await;
    let got = h.changed_by(&["src/lib.rs"]).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].session, second);

    // The chat is closed: the record stays, marked closed.
    assert!(h.manager.close(&second).await.unwrap());
    let got = h.changed_by(&["src/lib.rs"]).await;
    assert_eq!(got[0].session, second);
    assert!(got[0].closed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acp_write_is_the_sessions_and_survives_a_restart() {
    let go = Arc::new(Notify::new());
    let provider =
        ScriptedProvider::new().turn([Step::Wait(go.clone()), Step::Stop(StopReason::EndTurn)]);
    let h = harness(&provider).await;
    let s = h.session("Volume discount").await;
    let mut changes = h.manager.changes(None, None).unwrap().events;
    h.manager
        .prompt(&s, vec![ContentBlock::text("go")])
        .await
        .unwrap();
    // `fs/write_text_file` (the ACP host reports it through `BufferFs`).
    h.signals.wrote(&s, Path::new(&h.path("src/new.rs")));
    go.notify_one();
    h.until_idle(&s, &mut changes).await;
    let got = h.changed_by(&["src/new.rs"]).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].strength, ChangeStrength::Write);

    // Another daemon on the same database still knows.
    let Harness {
        _dir,
        data,
        workspace,
        workspaces,
        manager,
        svc,
        ..
    } = h;
    drop(svc);
    let (signals, rx) = channel();
    let svc = Provenance::open(
        &DataDirs::at(&data).global_db(),
        workspaces,
        None,
        manager,
        &signals,
        rx,
    )
    .unwrap();
    let got = svc
        .changed_by(ChangedByParams {
            workspace,
            root: RootId(1),
            paths: vec!["src/new.rs".into()],
        })
        .await
        .unwrap()
        .sessions;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].session, s);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_follow_a_rename() {
    let provider = ScriptedProvider::new();
    let h = harness(&provider).await;
    let lib = h.path("src/lib.rs");
    provider.push_turn([
        Step::Update(edit_call("t1", &lib, ToolKind::Edit, true)),
        Step::Update(status("t1", ToolCallStatus::Completed)),
        Step::Stop(StopReason::EndTurn),
    ]);
    let s = h.session("Volume discount").await;
    h.turn(&s).await;

    let moves = PathMoves::new();
    moves.follow(Arc::clone(&h.svc) as Arc<dyn FollowsMoves>);
    moves
        .moved(PathMove {
            workspace: h.workspace.clone(),
            root: RootId(1),
            from: "src".into(),
            to: "core".into(),
        })
        .await;
    assert!(h.changed_by(&["src/lib.rs"]).await.is_empty());
    let got = h.changed_by(&["core/lib.rs"]).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].session, s);
    assert_eq!(paths_of(&got[0]), ["core/lib.rs"]);
}

/// Polls `changed_by(paths)` until `done` holds.
async fn eventually(h: &Harness, paths: &[&str], done: impl Fn(&[SessionChange]) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        h.svc.settled().await;
        let got = h.changed_by(paths).await;
        if done(&got) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out; got {got:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_during_the_only_running_turn_is_weak_evidence() {
    let go = Arc::new(Notify::new());
    let provider =
        ScriptedProvider::new().turn([Step::Wait(go.clone()), Step::Stop(StopReason::EndTurn)]);
    let h = harness(&provider).await;
    let s = h.session("Volume discount").await;
    let mut changes = h.manager.changes(None, None).unwrap().events;
    h.manager
        .prompt(&s, vec![ContentBlock::text("go")])
        .await
        .unwrap();
    h.svc.settled().await;
    // The agent's own tool (say `sed -i`) changes a file: no tool call
    // names it, but the watcher sees it during the turn.
    let started = Instant::now();
    loop {
        // The watcher starts following when the turn does; write until a
        // change lands.
        std::fs::write(
            h.root.join("src/lib.rs"),
            format!("fn b() {{}} // {started:?}\n"),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.svc.settled().await;
        if !h.changed_by(&["src/lib.rs"]).await.is_empty() || started.elapsed() > TIMEOUT {
            break;
        }
    }
    eventually(&h, &["src/lib.rs"], |got| {
        got.len() == 1 && got[0].session == s && got[0].strength == ChangeStrength::Turn
    })
    .await;
    go.notify_one();
    h.until_idle(&s, &mut changes).await;

    // Long after the turn, a change is nobody's.
    tokio::time::sleep(super::LINGER + Duration::from_millis(500)).await;
    std::fs::write(h.root.join("later.rs"), "fn later() {}\n").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.svc.settled().await;
    assert!(h.changed_by(&["later.rs"]).await.is_empty());
}
