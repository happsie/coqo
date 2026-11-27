// SPDX-License-Identifier: Apache-2.0

//! `git/*` end to end: a protocol client, the workspace and git services
//! and the real `coqo.git` mod, on fixture repositories made by `git`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coqo_kernel::mods::BuiltinMods;
use coqo_mod_git::testing::Fixture;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::git::{
    self as gm, DiffSide, FileChange, GitChangeKind, LineKind, LineSelection, RepoParams,
};
use coqo_protocol::methods::workspace::{self as wsm, OpenParams, WorkspaceInfo};
use coqo_protocol::{
    Client, ClientConfig, ErrorObject, Method, ProtocolError, Router, Server, ServerConfig,
    Subscription,
};
use coqo_storage::DataDirs;
use futures_util::StreamExt as _;
use tempfile::TempDir;

use super::{GitService, classify, register};
use crate::mods::Mods;
use crate::workspace::{self, WatchConfig, WorkspaceService};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    _data: TempDir,
    client: Client,
}

async fn harness(with_git: bool) -> Harness {
    let data = tempfile::tempdir().unwrap();
    let config = WatchConfig {
        debounce: Duration::from_millis(50),
        ..WatchConfig::default()
    };
    let workspaces =
        Arc::new(WorkspaceService::open_with(DataDirs::at(data.path()), config).unwrap());
    let mut builtins = BuiltinMods::new();
    if with_git {
        builtins = builtins.with(coqo_mod_git::MANIFEST, coqo_mod_git::GitMod::default());
    }
    let mods = Mods::load(builtins);
    mods.start().await;
    let git = GitService::with_debounce(Arc::clone(&workspaces), mods, Duration::from_millis(50));
    let router = workspace::register(Router::new(), workspaces);
    let router = register(router, git);
    let server = Server::new(router, ServerConfig::new("coqod", "test"));
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move { server.serve(server_io).await });
    let (client, _) = Client::connect(client_io, ClientConfig::new("test", "test"))
        .await
        .unwrap();
    Harness {
        _data: data,
        client,
    }
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
}

fn repo(ws: &WorkspaceInfo) -> RepoParams {
    RepoParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
    }
}

fn stage(ws: &WorkspaceInfo, paths: &[&str], lines: Option<LineSelection>) -> gm::StageParams {
    gm::StageParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        lines,
    }
}

fn repo_fixture() -> (TempDir, Fixture) {
    let tmp = tempfile::tempdir().unwrap();
    let f = Fixture::init(&tmp.path().join("repo"));
    f.write("a.txt", "a\n");
    f.write("src/lib.rs", "fn one() {}\n");
    f.commit_all("initial");
    (tmp, f)
}

#[tokio::test]
async fn status_stage_and_commit_over_the_protocol() {
    let (_tmp, f) = repo_fixture();
    let h = harness(true).await;
    let ws = h.open(f.path()).await;

    f.write("a.txt", "a changed\n");
    f.write("new.txt", "new\n");
    let status = h.call::<gm::Status>(repo(&ws)).await;
    assert_eq!(status.repo.prefix, "");
    assert_eq!(status.head.branch.as_deref(), Some("main"));
    let summary: Vec<_> = status
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.letter()))
        .collect();
    assert_eq!(summary, vec![("a.txt", Some('M')), ("new.txt", Some('?'))]);

    // The unstaged diff, then stage one file.
    let diff = h
        .call::<gm::Diff>(gm::DiffParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: "a.txt".into(),
            base: DiffSide::Index,
            head: DiffSide::Worktree,
            context: None,
        })
        .await;
    assert_eq!((diff.additions, diff.deletions), (1, 1));
    h.call::<gm::Stage>(stage(&ws, &["a.txt", "new.txt"], None))
        .await;
    let status = h.call::<gm::Status>(repo(&ws)).await;
    assert!(
        status
            .entries
            .iter()
            .all(|e| e.staged.is_some() && e.unstaged.is_none()),
        "{status:?}"
    );
    let files = h
        .call::<gm::DiffFiles>(gm::DiffFilesParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            base: DiffSide::head(),
            head: DiffSide::Index,
        })
        .await;
    assert_eq!(files.files.len(), 2);
    assert_eq!(files.files[1].change, FileChange::Added);

    let commit = h
        .call::<gm::Commit>(gm::CommitParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            message: "Change a, add new".into(),
            amend: false,
        })
        .await;
    assert_eq!(commit.oid, f.git(&["rev-parse", "HEAD"]).trim());
    assert!(h.call::<gm::Status>(repo(&ws)).await.entries.is_empty());
    let log = h
        .call::<gm::Log>(gm::LogParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            revs: vec![],
            skip: 0,
            limit: Some(10),
        })
        .await;
    assert_eq!(log.commits.len(), 2);
    assert_eq!(log.commits[0].summary, "Change a, add new");
    let blame = h
        .call::<gm::Blame>(gm::BlameParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: "a.txt".into(),
            rev: None,
        })
        .await;
    assert_eq!(blame.hunks[0].oid, commit.oid);

    // git's own refusal comes back as GIT_FAILED with its message.
    let err = h
        .fail::<gm::Commit>(gm::CommitParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            message: "Nothing staged".into(),
            amend: false,
        })
        .await;
    assert_eq!(err.code, codes::GIT_FAILED);
    let err = h
        .fail::<gm::Stage>(stage(
            &ws,
            &["a.txt", "new.txt"],
            Some(LineSelection::default()),
        ))
        .await;
    assert_eq!(err.code, coqo_protocol::codes::INVALID_PARAMS);
}

#[tokio::test]
async fn line_staging_over_the_protocol() {
    let (_tmp, f) = repo_fixture();
    let h = harness(true).await;
    let ws = h.open(f.path()).await;
    f.write("a.txt", "a\nb\nc\n");
    let diff = h
        .call::<gm::Diff>(gm::DiffParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: "a.txt".into(),
            base: DiffSide::Index,
            head: DiffSide::Worktree,
            context: None,
        })
        .await;
    let c = diff.hunks[0]
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Added && l.text == "c")
        .unwrap();
    let only_c = LineSelection {
        removed: vec![],
        added: vec![c.new_line.unwrap()],
    };
    h.call::<gm::Stage>(stage(&ws, &["a.txt"], Some(only_c.clone())))
        .await;
    assert_eq!(f.git(&["show", ":a.txt"]), "a\nc\n");
    // Same lines again: "c" is staged now, so line 3 is no longer an added
    // line of the unstaged diff.
    let err = h
        .fail::<gm::Stage>(stage(&ws, &["a.txt"], Some(only_c)))
        .await;
    assert_eq!(err.code, codes::STALE_SELECTION);
}

#[tokio::test]
async fn branches_and_stash_over_the_protocol() {
    let (_tmp, f) = repo_fixture();
    let h = harness(true).await;
    let ws = h.open(f.path()).await;
    let (id, root) = (ws.id.clone(), ws.roots[0].id);
    h.call::<gm::CreateBranch>(gm::CreateBranchParams {
        workspace: id.clone(),
        root,
        name: "topic".into(),
        start: None,
        switch: true,
    })
    .await;
    let branches = h.call::<gm::Branches>(repo(&ws)).await.branches;
    let current: Vec<_> = branches
        .iter()
        .filter(|b| b.current)
        .map(|b| &b.name)
        .collect();
    assert_eq!(current, ["topic"]);
    h.call::<gm::SwitchBranch>(gm::SwitchBranchParams {
        workspace: id.clone(),
        root,
        name: "main".into(),
    })
    .await;
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "main");

    f.write("a.txt", "stash me\n");
    let pushed = h
        .call::<gm::StashPush>(gm::StashPushParams {
            workspace: id.clone(),
            root,
            message: Some("wip".into()),
            include_untracked: false,
        })
        .await;
    assert!(pushed.created);
    let list = h.call::<gm::StashList>(repo(&ws)).await.stashes;
    assert_eq!(list.len(), 1);
    assert!(list[0].message.ends_with("wip"), "{list:?}");
    h.call::<gm::StashPop>(gm::StashParams {
        workspace: id.clone(),
        root,
        index: 0,
    })
    .await;
    assert_eq!(f.read("a.txt"), "stash me\n");
    let err = h
        .fail::<gm::StashDrop>(gm::StashParams {
            workspace: id,
            root,
            index: 0,
        })
        .await;
    assert_eq!(err.code, codes::NOT_FOUND);
}

/// Events until one for `root` has `kind`.
async fn until_kind(sub: &mut Subscription<gm::Changes>, kind: GitChangeKind) -> gm::GitChanges {
    loop {
        let item = tokio::time::timeout(TIMEOUT, sub.next())
            .await
            .unwrap_or_else(|_| panic!("no {kind:?} event in time"))
            .expect("subscription open")
            .expect("event ok");
        if item.event.kinds.contains(&kind) {
            return item.event;
        }
    }
}

#[tokio::test]
async fn changes_topic_reports_edits_index_head_and_stash() {
    let (_tmp, f) = repo_fixture();
    let h = harness(true).await;
    let ws = h.open(f.path()).await;
    let mut sub = h
        .client
        .subscribe::<gm::Changes>(
            gm::ChangesParams {
                workspace: ws.id.clone(),
            },
            None,
        )
        .await
        .unwrap();

    // An edit in the work tree.
    f.write("a.txt", "edited\n");
    let ev = until_kind(&mut sub, GitChangeKind::Worktree).await;
    assert_eq!(ev.root, ws.roots[0].id);
    // `git add` outside Coqo: the index changed.
    f.git(&["add", "a.txt"]);
    until_kind(&mut sub, GitChangeKind::Index).await;
    // A commit outside Coqo moves HEAD and the branch.
    f.git(&["commit", "-q", "-m", "outside"]);
    until_kind(&mut sub, GitChangeKind::Head).await;
    // A branch made outside Coqo.
    f.git(&["branch", "elsewhere"]);
    until_kind(&mut sub, GitChangeKind::Refs).await;
    // A stash made through Coqo is reported at once.
    f.write("a.txt", "again\n");
    h.call::<gm::StashPush>(gm::StashPushParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
        message: None,
        include_untracked: false,
    })
    .await;
    until_kind(&mut sub, GitChangeKind::Stash).await;

    // Closing the workspace ends the feed.
    h.call::<wsm::Close>(wsm::WorkspaceParams { id: ws.id.clone() })
        .await;
    let end = tokio::time::timeout(TIMEOUT, async {
        while let Some(Ok(_)) = sub.next().await {}
    })
    .await;
    assert!(end.is_ok(), "the feed ends when the workspace closes");
}

#[tokio::test]
async fn not_a_repository_and_a_disabled_mod() {
    let plain = tempfile::tempdir().unwrap();
    let h = harness(true).await;
    let ws = h.open(plain.path()).await;
    let err = h.fail::<gm::Status>(repo(&ws)).await;
    assert_eq!(err.code, codes::NOT_A_REPOSITORY);

    let (_tmp, f) = repo_fixture();
    let h = harness(false).await;
    let ws = h.open(f.path()).await;
    let err = h.fail::<gm::Status>(repo(&ws)).await;
    assert_eq!(err.code, codes::NOT_A_REPOSITORY);
    assert!(err.message.contains("coqo.git"), "{}", err.message);
}

#[test]
fn git_directory_changes_are_classified() {
    use GitChangeKind as K;
    let c = |p: &str, in_git_dir| classify(Path::new(p), in_git_dir);
    assert_eq!(c("index", true), Some(K::Index));
    assert_eq!(c("index", false), None, "another worktree's index");
    assert_eq!(c("index.lock", true), None);
    assert_eq!(c("HEAD", true), Some(K::Head));
    assert_eq!(c("logs/HEAD", true), Some(K::Head));
    assert_eq!(c("refs/heads/main", false), Some(K::Refs));
    assert_eq!(c("refs/heads/main.lock", false), None);
    assert_eq!(c("packed-refs", false), Some(K::Refs));
    assert_eq!(c("refs/stash", false), Some(K::Stash));
    assert_eq!(c("logs/refs/stash", true), Some(K::Stash));
    assert_eq!(c("objects/ab/cdef", true), None);
    assert_eq!(c("COMMIT_EDITMSG", true), None);
}
