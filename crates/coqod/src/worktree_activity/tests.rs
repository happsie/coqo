// SPDX-License-Identifier: Apache-2.0

//! Which other worktrees a session works in (#359): the worktree a path is
//! in, a scripted agent's tool calls in a subagent's worktree until its
//! turn ends, and a quiet worktree fading.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use coqo_agents::testing::{ScriptedProvider, Step};
use coqo_agents::{AgentUpdate, ManagerOptions, SessionManager, SessionStore, StaticProviders};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_mod_git::testing::Fixture;
use coqo_protocol::methods::agents::{
    ContentBlock, CreateSessionParams, SessionId, StopReason, ToolCall, ToolCallId,
    ToolCallLocation, ToolCallStatus, ToolKind,
};
use coqo_protocol::methods::workspace::{OpenParams, WorkspaceId};
use coqo_storage::DataDirs;
use tokio::sync::Notify;

use super::{WorktreeActivityTracker, elsewhere};
use crate::provenance::{Provenance, channel};
use crate::workspace::WorkspaceService;

const TIMEOUT: Duration = Duration::from_secs(10);

/// A repository with a sibling worktree and one of Claude Code's inside
/// it: `(tmp, repo, sibling, agent)`, all canonical.
fn repo() -> (tempfile::TempDir, Fixture, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let f = Fixture::init(&base.join("shop"));
    f.write("src/lib.rs", "fn a() {}\n");
    f.commit_all("initial");
    let sibling = base.join("shop-hotfix");
    f.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "hotfix",
        sibling.to_str().unwrap(),
    ]);
    let agent = f.path().join(".claude/worktrees/agent-1");
    f.git(&["worktree", "add", "-q", "--detach", agent.to_str().unwrap()]);
    (tmp, f, sibling, agent)
}

fn named(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn a_path_is_in_the_deepest_worktree_holding_it() {
    let (tmp, f, sibling, agent) = repo();
    let main = f.path().to_path_buf();
    let at = |root: &Path, paths: &[PathBuf]| elsewhere(root, paths);

    // From the main worktree: the subagent's worktree inside it is another.
    assert_eq!(
        at(&main, &[agent.join("src/lib.rs")]),
        [named(&agent)],
        "inside the main worktree's folder, still another worktree"
    );
    // A file it is about to make counts too.
    assert_eq!(at(&main, &[agent.join("src/new.rs")]), [named(&agent)]);
    assert_eq!(at(&main, &[sibling.join("src/lib.rs")]), [named(&sibling)]);
    assert!(at(&main, &[main.join("src/lib.rs")]).is_empty(), "its own");
    assert!(at(&main, &[tmp.path().join("elsewhere.txt")]).is_empty());
    // Several at once, each once, in order.
    assert_eq!(
        at(
            &main,
            &[
                sibling.join("a"),
                agent.join("b"),
                sibling.join("c"),
                main.join("d")
            ]
        ),
        [named(&sibling), named(&agent)]
    );
    // From a linked worktree: the main one is another.
    assert_eq!(at(&sibling, &[main.join("src/lib.rs")]), [named(&main)]);
    assert!(at(&sibling, &[sibling.join("src/lib.rs")]).is_empty());

    // Outside a repository: nothing.
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    if crate::workspace::project_of(&plain).is_none() {
        assert!(at(&plain, &[agent.join("x")]).is_empty());
    }
}

struct Harness {
    _tmp: tempfile::TempDir,
    _data: tempfile::TempDir,
    repo: Fixture,
    agent: PathBuf,
    workspace: WorkspaceId,
    workspaces: Arc<WorkspaceService>,
    manager: SessionManager,
    provenance: Arc<Provenance>,
}

/// The repository's main worktree as a workspace, a session manager whose
/// `fake` agent is `provider`, and the provenance service feeding the
/// tracker.
async fn harness(provider: &ScriptedProvider) -> Harness {
    let (tmp, repo, _sibling, agent) = repo();
    let data = tempfile::tempdir().unwrap();
    let workspaces = Arc::new(WorkspaceService::open(DataDirs::at(data.path())).unwrap());
    let workspace = workspaces
        .open_workspace(OpenParams {
            path: Some(named(repo.path())),
            ..OpenParams::default()
        })
        .await
        .unwrap()
        .id;
    let (signals, rx) = channel();
    let db = DataDirs::at(data.path()).global_db();
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
    let provenance = Provenance::open(
        &db,
        Arc::clone(&workspaces),
        None,
        manager.clone(),
        &signals,
        rx,
    )
    .unwrap();
    Harness {
        _tmp: tmp,
        _data: data,
        repo,
        agent,
        workspace,
        workspaces,
        manager,
        provenance,
    }
}

impl Harness {
    async fn session(&self) -> SessionId {
        self.manager
            .create(CreateSessionParams {
                provider: "fake".into(),
                cwd: named(self.repo.path()),
                workspace: Some(self.workspace.0.clone()),
                title: Some("Volume discount".into()),
                mode: None,
                from: None,
            })
            .await
            .unwrap()
            .id
    }

    fn worktrees(&self, id: &SessionId) -> Vec<String> {
        self.manager
            .snapshot(id)
            .unwrap()
            .info
            .worktrees
            .into_iter()
            .map(|w| w.path)
            .collect()
    }

    async fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                self.provenance.settled().await;
                if done(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }
}

fn read_call(id: &str, path: &Path) -> AgentUpdate {
    AgentUpdate::ToolCall(ToolCall {
        id: ToolCallId(id.into()),
        title: format!("Read {}", path.display()),
        name: Some("Read".into()),
        kind: ToolKind::Read,
        status: ToolCallStatus::Completed,
        content: Vec::new(),
        locations: vec![ToolCallLocation {
            path: named(path),
            line: None,
        }],
        raw_input: None,
        raw_output: None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subagents_tool_calls_mark_its_worktree_until_the_turn_ends() {
    let provider = ScriptedProvider::new();
    let h = harness(&provider).await;
    let (work, finish) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (own, theirs) = (h.repo.path().join("src/lib.rs"), h.agent.join("src/lib.rs"));
    provider.push_turn([
        // Its own worktree first: nothing elsewhere.
        Step::Update(read_call("t1", &own)),
        Step::Wait(work.clone()),
        // Then a subagent's, in `.claude/worktrees/agent-1`.
        Step::Update(read_call("t2", &theirs)),
        Step::Wait(finish.clone()),
        Step::Stop(StopReason::EndTurn),
    ]);
    let id = h.session().await;
    let mut changes = h.manager.changes(None, None).unwrap().events;
    h.manager
        .prompt(&id, vec![ContentBlock::text("go")])
        .await
        .unwrap();
    h.until("the first call", |h| {
        h.manager.snapshot(&id).unwrap().info.tool_use.len() == 1
    })
    .await;
    assert!(h.worktrees(&id).is_empty());

    work.notify_one();
    h.until("the subagent's worktree", |h| {
        h.worktrees(&id) == [named(&h.agent)]
    })
    .await;
    // It went out on `sessions/changes`.
    use futures_util::StreamExt as _;
    tokio::time::timeout(TIMEOUT, async {
        while let Some(Ok(change)) = changes.next().await {
            if change.event.id == id && !change.event.worktrees.is_empty() {
                return;
            }
        }
    })
    .await
    .expect("a change with the worktree");

    finish.notify_one();
    h.until("the turn's end to clear it", |h| {
        !h.manager.snapshot(&id).unwrap().running && h.worktrees(&id).is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_worktree_fades() {
    let provider = ScriptedProvider::new();
    let h = harness(&provider).await;
    let id = h.session().await;
    let tracker = WorktreeActivityTracker::spawn(
        Arc::clone(&h.workspaces),
        h.manager.clone(),
        Duration::from_millis(300),
    );
    tracker.touched(&id, &h.workspace, vec![h.agent.join("a.rs")]);
    tracker.settled().await;
    assert_eq!(h.worktrees(&id), [named(&h.agent)]);
    // Touched again: still there past the first touch's quiet spell.
    tokio::time::sleep(Duration::from_millis(200)).await;
    tracker.touched(&id, &h.workspace, vec![h.agent.join("b.rs")]);
    tracker.settled().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.worktrees(&id), [named(&h.agent)]);
    // Then quiet.
    tokio::time::timeout(TIMEOUT, async {
        while !h.worktrees(&id).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the worktree to fade");
    // Relative paths are no touch.
    tracker.touched(&id, &h.workspace, vec![PathBuf::from("src/lib.rs")]);
    tracker.settled().await;
    assert!(h.worktrees(&id).is_empty());
}
