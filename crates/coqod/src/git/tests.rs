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
use crate::workspace::{self, Trash, WatchConfig, WorkspaceService};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    _data: TempDir,
    client: Client,
    workspaces: Arc<WorkspaceService>,
}

async fn harness(with_git: bool) -> Harness {
    let data = tempfile::tempdir().unwrap();
    let config = WatchConfig {
        debounce: Duration::from_millis(50),
        ..WatchConfig::default()
    };
    // Never the user's trash.
    let workspaces = Arc::new(
        WorkspaceService::open_with(DataDirs::at(data.path()), config)
            .unwrap()
            .with_trash(Trash::Folder(data.path().join("bin"))),
    );
    let ws_handle = Arc::clone(&workspaces);
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
        workspaces: ws_handle,
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
            orig_path: None,
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

fn discard(ws: &WorkspaceInfo, paths: &[&str], lines: Option<LineSelection>) -> gm::DiscardParams {
    gm::DiscardParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        lines,
    }
}

#[tokio::test]
async fn discard_restores_files_and_trashes_untracked_ones() {
    let (_tmp, f) = repo_fixture();
    f.write(
        "big.txt",
        &(1..=30).map(|i| format!("{i}\n")).collect::<String>(),
    );
    f.commit_all("more");
    let h = harness(true).await;
    let ws = h.open(f.path()).await;
    let mut changes = h
        .client
        .subscribe::<gm::Changes>(
            gm::ChangesParams {
                workspace: ws.id.clone(),
            },
            None,
        )
        .await
        .unwrap();

    // A hunk of one file: the other hunk stays.
    let edited: String = (1..=30)
        .map(|i| match i {
            2 => "two\n".to_owned(),
            28 => "twenty-eight\n".to_owned(),
            i => format!("{i}\n"),
        })
        .collect();
    f.write("big.txt", &edited);
    let diff = h
        .call::<gm::Diff>(gm::DiffParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: "big.txt".into(),
            orig_path: None,
            base: DiffSide::Index,
            head: DiffSide::Worktree,
            context: None,
        })
        .await;
    assert_eq!(diff.hunks.len(), 2);
    let first = LineSelection {
        removed: diff.hunks[0]
            .lines
            .iter()
            .filter_map(|l| (l.kind == LineKind::Removed).then_some(l.old_line?))
            .collect(),
        added: diff.hunks[0]
            .lines
            .iter()
            .filter_map(|l| (l.kind == LineKind::Added).then_some(l.new_line?))
            .collect(),
    };
    let none = h
        .call::<gm::Discard>(discard(&ws, &["big.txt"], Some(first.clone())))
        .await;
    assert!(none.trashed.is_empty());
    assert_eq!(f.read("big.txt"), edited.replace("two\n", "2\n"));
    until_kind(&mut changes, GitChangeKind::Worktree).await;
    // The same lines again are stale.
    let stale = h
        .fail::<gm::Discard>(discard(&ws, &["big.txt"], Some(first)))
        .await;
    assert_eq!(stale.code, codes::STALE_SELECTION);

    // A whole file and an untracked one: back to the index, and to the
    // trash (restorable).
    f.write("a.txt", "a changed\n");
    f.write("src/new.rs", "fn new() {}\n");
    let done = h
        .call::<gm::Discard>(discard(&ws, &["a.txt", "src/new.rs"], None))
        .await;
    assert_eq!(f.read("a.txt"), "a\n");
    assert!(!f.path().join("src/new.rs").exists());
    assert_eq!(done.trashed.len(), 1);
    let trashed = &done.trashed[0];
    assert_eq!(
        (trashed.path.as_str(), trashed.root_path.as_str()),
        ("src/new.rs", "src/new.rs")
    );
    let token = trashed.restore.clone().expect("restorable");
    h.workspaces
        .restore_entry(coqo_protocol::methods::fs::RestoreParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: trashed.root_path.clone(),
            restore: token,
        })
        .await
        .unwrap();
    assert_eq!(f.read("src/new.rs"), "fn new() {}\n");

    // Refused before anything changes: an untracked file with a tracked
    // one that can't be discarded, and line selections over two paths.
    f.write("a.txt", "a again\n");
    f.git(&["add", "-N", "src/new.rs"]);
    let err = h
        .fail::<gm::Discard>(discard(&ws, &["a.txt", "src/new.rs"], None))
        .await;
    assert_eq!(err.code, codes::UNSUPPORTED);
    assert_eq!(f.read("a.txt"), "a again\n");
    assert!(f.path().join("src/new.rs").exists());
    let err = h
        .fail::<gm::Discard>(discard(
            &ws,
            &["a.txt", "big.txt"],
            Some(LineSelection::default()),
        ))
        .await;
    assert_eq!(err.code, ErrorObject::invalid_params("").code);
}

#[tokio::test]
async fn renames_over_the_protocol() {
    let (_tmp, f) = repo_fixture();
    let h = harness(true).await;
    let ws = h.open(f.path()).await;
    f.git(&["mv", "src/lib.rs", "src/core.rs"]);
    let status = h.call::<gm::Status>(repo(&ws)).await;
    let entry = status
        .entries
        .iter()
        .find(|e| e.path == "src/core.rs")
        .unwrap();
    assert_eq!(entry.staged, Some(FileChange::Renamed));
    assert_eq!(entry.orig_path.as_deref(), Some("src/lib.rs"));
    assert!(!status.entries.iter().any(|e| e.path == "src/lib.rs"));
    let files = h
        .call::<gm::DiffFiles>(gm::DiffFilesParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            base: DiffSide::head(),
            head: DiffSide::Index,
        })
        .await;
    assert_eq!(files.files.len(), 1);
    assert_eq!(files.files[0].change, FileChange::Renamed);
    assert_eq!(files.files[0].orig_path.as_deref(), Some("src/lib.rs"));
    let diff = h
        .call::<gm::Diff>(gm::DiffParams {
            workspace: ws.id.clone(),
            root: ws.roots[0].id,
            path: "src/core.rs".into(),
            orig_path: Some("src/lib.rs".into()),
            base: DiffSide::head(),
            head: DiffSide::Index,
            context: None,
        })
        .await;
    assert!(diff.hunks.is_empty(), "an exact rename has no line changes");
    assert_eq!(diff.orig_path.as_deref(), Some("src/lib.rs"));
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
            orig_path: None,
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
async fn changes_topic_watches_roots_added_later() {
    let (_tmp, f) = repo_fixture();
    let (_tmp2, other) = repo_fixture();
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
    // The watch is up once it reports something for the first root.
    f.write("a.txt", "edited\n");
    until_kind(&mut sub, GitChangeKind::Worktree).await;

    let info = h
        .call::<wsm::AddRoot>(wsm::AddRootParams {
            id: ws.id.clone(),
            path: other.path().to_str().unwrap().to_owned(),
            name: None,
        })
        .await;
    let added = info
        .roots
        .iter()
        .find(|r| r.id != ws.roots[0].id)
        .unwrap()
        .id;
    // The new root's watch starts shortly after the event: `git add` (or
    // undo it) outside Coqo until its index change is reported.
    other.write("a.txt", "staged in the new root\n");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    for round in 0.. {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no index change for the added root"
        );
        if round % 2 == 0 {
            other.git(&["add", "a.txt"]);
        } else {
            other.git(&["reset", "-q", "a.txt"]);
        }
        let seen = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let item = sub.next().await.expect("open").expect("event ok");
                if item.event.root == added && item.event.kinds.contains(&GitChangeKind::Index) {
                    return;
                }
            }
        })
        .await;
        if seen.is_ok() {
            break;
        }
    }
    // A branch made in the added root is reported as a refs change.
    other.git(&["branch", "later"]);
    loop {
        let ev = until_kind(&mut sub, GitChangeKind::Refs).await;
        if ev.root == added {
            break;
        }
    }
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
    // Worktrees made, removed or pruned, from the main worktree's git
    // directory (the common one) or a linked worktree's common directory.
    assert_eq!(c("worktrees", true), Some(K::Worktrees));
    assert_eq!(c("worktrees/feature-x", false), Some(K::Worktrees));
    assert_eq!(c("worktrees/feature-x", true), Some(K::Worktrees));
    assert_eq!(c("worktrees/feature-x/index", false), None);
    assert_eq!(c("worktrees/feature-x/HEAD", true), None);
}

// ---- worktrees (#92) ----------------------------------------------------------

#[tokio::test]
async fn worktrees_are_workspaces_of_one_project() {
    let (tmp, f) = repo_fixture();
    let h = harness(true).await;
    let main = h.open(f.path()).await;
    let project = main.project.clone().expect("a repository has a project");
    assert_eq!(project.name, "repo");
    assert_eq!(
        project.main_worktree.as_deref(),
        Some(f.path().to_str().unwrap())
    );
    // A folder in no repository has none (unless the temporary folder is
    // in one).
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    if crate::workspace::project_of(&plain).is_none() {
        assert_eq!(h.open(&plain).await.project, None);
    }

    // Adding one opens a workspace on it, in the same project.
    let added = h
        .call::<gm::AddWorktree>(gm::AddWorktreeParams {
            workspace: main.id.clone(),
            root: main.roots[0].id,
            branch: "feature/x".into(),
            base: Some("main".into()),
            path: None,
        })
        .await;
    let dir = tmp.path().canonicalize().unwrap().join("repo-worktrees");
    assert_eq!(added.worktree.path, dir.join("feature-x").to_str().unwrap());
    assert_eq!(added.worktree.branch.as_deref(), Some("feature/x"));
    assert_eq!(added.workspace.roots[0].path, added.worktree.path);
    assert_eq!(added.workspace.project.as_ref(), Some(&project));
    assert!(added.workspace.open);

    // One made outside Coqo is listed, and joins the project once opened.
    let outside = tmp.path().join("by-hand");
    f.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "by-hand",
        outside.to_str().unwrap(),
    ]);
    std::fs::write(outside.join("a.txt"), "edited by hand\n").unwrap();
    let list = h.call::<gm::Worktrees>(repo(&added.workspace)).await;
    assert_eq!(list.worktrees_dir, dir.to_str().unwrap());
    let summary: Vec<_> = list
        .worktrees
        .iter()
        .map(|w| (w.branch.as_deref(), w.main, w.changes))
        .collect();
    // The main one first, then as git orders them (by folder).
    assert_eq!(
        summary,
        [
            (Some("main"), true, Some(0)),
            (Some("by-hand"), false, Some(1)),
            (Some("feature/x"), false, Some(0)),
        ]
    );
    let by_hand = h.open(&outside).await;
    assert_eq!(by_hand.project.as_ref(), Some(&project));

    // Removing: never the main worktree, never a dirty one.
    let remove = |path: &str, confirm: bool| gm::RemoveWorktreeParams {
        workspace: main.id.clone(),
        root: main.roots[0].id,
        path: path.to_owned(),
        confirm,
    };
    let err = h
        .fail::<gm::RemoveWorktree>(remove(f.path().to_str().unwrap(), true))
        .await;
    assert_eq!(err.code, coqo_protocol::codes::INVALID_PARAMS);
    let outside_path = list.worktrees[1].path.clone();
    let refused = h
        .call::<gm::RemoveWorktree>(remove(&outside_path, true))
        .await;
    assert!(!refused.removed);
    assert_eq!((refused.changes, refused.unpushed), (1, 0));
    assert!(outside.join("a.txt").exists());

    // A clean one goes, and so does the workspace on it.
    let removed = h
        .call::<gm::RemoveWorktree>(remove(&added.worktree.path, false))
        .await;
    assert!(removed.removed);
    assert_eq!(removed.forgotten, std::slice::from_ref(&added.workspace.id));
    assert!(!Path::new(&added.worktree.path).exists());
    let known: Vec<_> = h
        .call::<wsm::List>(())
        .await
        .workspaces
        .into_iter()
        .map(|w| w.id)
        .collect();
    assert!(!known.contains(&added.workspace.id));
    assert!(known.contains(&by_hand.id) && known.contains(&main.id));
    // Its branch stays.
    assert!(
        f.git(&["branch", "--list", "feature/x"])
            .contains("feature/x")
    );
}

/// Drop the events already sent, so the next wait sees only new ones.
async fn drain(sub: &mut Subscription<gm::Changes>) {
    while tokio::time::timeout(Duration::from_millis(300), sub.next())
        .await
        .is_ok()
    {}
}

/// #359: worktrees made, removed and pruned outside Coqo (an agent's
/// `git worktree add`, Claude Code's `.claude/worktrees/*`) reach
/// `git/changes` as `worktrees`, in every worktree's workspace, from the
/// first one made to the last one gone.
#[tokio::test]
async fn changes_topic_reports_worktrees_made_outside_coqo() {
    let (tmp, f) = repo_fixture();
    let h = harness(true).await;
    let main = h.open(f.path()).await;
    let subscribe = async |ws: &WorkspaceInfo| {
        h.client
            .subscribe::<gm::Changes>(
                gm::ChangesParams {
                    workspace: ws.id.clone(),
                },
                None,
            )
            .await
            .unwrap()
    };
    let mut sub = subscribe(&main).await;
    let listed = async || h.call::<gm::Worktrees>(repo(&main)).await.worktrees.len();
    assert!(!f.path().join(".git/worktrees").exists());

    // The first one: git makes the worktrees folder too.
    let first = tmp.path().join("first");
    f.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "first",
        first.to_str().unwrap(),
    ]);
    until_kind(&mut sub, GitChangeKind::Worktrees).await;
    assert_eq!(listed().await, 2);
    drain(&mut sub).await;

    // One inside the repository, as Claude Code's subagents make them,
    // detached (no new branch, so no `refs` change).
    let agent = f.path().join(".claude/worktrees/agent-1");
    f.git(&["worktree", "add", "-q", "--detach", agent.to_str().unwrap()]);
    until_kind(&mut sub, GitChangeKind::Worktrees).await;
    assert_eq!(listed().await, 3);
    drain(&mut sub).await;

    // A linked worktree's workspace hears of them too.
    let linked = h.open(&first).await;
    let mut linked_sub = subscribe(&linked).await;
    drain(&mut linked_sub).await;

    // Removed.
    f.git(&["worktree", "remove", agent.to_str().unwrap()]);
    until_kind(&mut sub, GitChangeKind::Worktrees).await;
    until_kind(&mut linked_sub, GitChangeKind::Worktrees).await;
    assert_eq!(listed().await, 2);
    drain(&mut sub).await;

    // Deleted by hand, then pruned: git drops the worktrees folder with
    // the last one.
    drop(linked_sub);
    std::fs::remove_dir_all(&first).unwrap();
    f.git(&["worktree", "prune"]);
    until_kind(&mut sub, GitChangeKind::Worktrees).await;
    assert_eq!(listed().await, 1);
    drain(&mut sub).await;

    // And made again from nothing.
    let again = tmp.path().join("again");
    f.git(&["worktree", "add", "-q", "--detach", again.to_str().unwrap()]);
    until_kind(&mut sub, GitChangeKind::Worktrees).await;
    assert_eq!(listed().await, 2);
    drain(&mut sub).await;
    let more = tmp.path().join("more");
    f.git(&["worktree", "add", "-q", "--detach", more.to_str().unwrap()]);
    until_kind(&mut sub, GitChangeKind::Worktrees).await;
    assert_eq!(listed().await, 3);
}
