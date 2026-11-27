// SPDX-License-Identifier: Apache-2.0

//! `git/*`: everyday git on a workspace root's repository (PLAN §7.7, issue
//! #91), served from the `coqo.git` mod's service.
//!
//! Every call resolves `coqo.git@1` from the mod registry, so a disabled
//! git mod turns these methods off (they fail with `NOT_A_REPOSITORY`) and
//! a mod providing the service replaces the built-in one. Calls run on
//! blocking threads: reads use gix, writes the user's `git`.
//!
//! **`git/changes`** merges two sources per workspace: the workspace's
//! `fs/changes` feed (work-tree edits, already debounced and ignore-aware)
//! and a watch on each root's git directory (`HEAD`, the index, refs,
//! reflogs, the stash), classified into [`GitChangeKind`]s and debounced.
//! Writes made through `git/*` publish their event at once. The watch
//! starts with the first subscriber and stops with the last, or when the
//! workspace closes (the feeds end then).
//!
//! **Access:** every route needs the daemon token. Agents have their own
//! git; the editor's MCP tools stay to what harnesses lack, so no git tools
//! are offered to them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use coqo_mod_api::Service as _;
use coqo_mod_git::{self as mg, Git, GitError, Repo};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::ChangesParams as FsChangesParams;
use coqo_protocol::methods::git::{self as gm, GitChangeKind, GitChanges};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, SeqLog};
use futures_util::StreamExt as _;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::{Notify, watch};

use crate::mods::Mods;
use crate::workspace::WorkspaceService;

/// Default context of `git/diff`, as `git diff`.
const DEFAULT_CONTEXT: u32 = 3;
/// Largest `git/diff` context.
const MAX_CONTEXT: u32 = 10_000;
/// Default `git/log` page.
const DEFAULT_LOG_PAGE: u32 = 100;
/// Batches kept for `git/changes` subscribers catching up.
const RETAINED: usize = 256;

/// The daemon's git service.
#[derive(Debug)]
pub struct GitService {
    workspaces: Arc<WorkspaceService>,
    mods: Mods,
    debounce: Duration,
    watches: Mutex<HashMap<WorkspaceId, Weak<GitWatch>>>,
    /// Held while a watch starts, so two first subscribers share one.
    starting: tokio::sync::Mutex<()>,
}

fn error(e: GitError) -> ErrorObject {
    use mg::ErrorKind as K;
    match e.kind {
        K::NotARepository => ErrorObject::new(codes::NOT_A_REPOSITORY, e.message),
        K::NotFound => ErrorObject::new(codes::NOT_FOUND, e.message),
        K::StaleSelection => ErrorObject::new(codes::STALE_SELECTION, e.message),
        K::Unsupported => ErrorObject::new(codes::UNSUPPORTED, e.message),
        K::Invalid => ErrorObject::invalid_params(e.message),
        K::Command => ErrorObject::new(codes::GIT_FAILED, e.message),
        K::Other => ErrorObject::new(codes::IO_ERROR, e.message),
    }
}

impl GitService {
    /// A service over `workspaces`, resolving `coqo.git@1` from `mods`.
    pub fn new(workspaces: Arc<WorkspaceService>, mods: Mods) -> Arc<Self> {
        Self::with_debounce(workspaces, mods, Duration::from_millis(100))
    }

    /// Like [`new`](Self::new), with the git-directory watch's debounce.
    pub fn with_debounce(
        workspaces: Arc<WorkspaceService>,
        mods: Mods,
        debounce: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspaces,
            mods,
            debounce,
            watches: Mutex::default(),
            starting: tokio::sync::Mutex::new(()),
        })
    }

    fn git(&self) -> Result<Arc<Git>, ErrorObject> {
        self.mods
            .services
            .consume::<Git>(&Git::service_ref())
            .map_err(|e| {
                ErrorObject::new(
                    codes::NOT_A_REPOSITORY,
                    format!("git is not available: the coqo.git mod is not active ({e})"),
                )
            })
    }

    async fn root_dir(
        &self,
        workspace: &WorkspaceId,
        root: RootId,
    ) -> Result<PathBuf, ErrorObject> {
        self.workspaces
            .roots(workspace.clone())
            .await?
            .into_iter()
            .find(|(id, _)| *id == root)
            .map(|(_, p)| p)
            .ok_or_else(|| {
                ErrorObject::new(
                    codes::UNKNOWN_ROOT,
                    format!("workspace {workspace} has no root {root}"),
                )
            })
    }

    /// Open the repository of `(workspace, root)` and run `f` on it on a
    /// blocking thread.
    pub(crate) async fn with_repo<T, F>(
        &self,
        workspace: &WorkspaceId,
        root: RootId,
        f: F,
    ) -> Result<T, ErrorObject>
    where
        T: Send + 'static,
        F: FnOnce(&Repo) -> Result<T, GitError> + Send + 'static,
    {
        let git = self.git()?;
        let dir = self.root_dir(workspace, root).await?;
        tokio::task::spawn_blocking(move || git.open(&dir).and_then(|repo| f(&repo)))
            .await
            .map_err(|e| ErrorObject::internal(format!("git task failed: {e}")))?
            .map_err(error)
    }

    /// A write changed `kinds` of `(workspace, root)`: tell subscribers now.
    fn changed(&self, workspace: &WorkspaceId, root: RootId, kinds: &[GitChangeKind]) {
        let watch = self
            .watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace)
            .and_then(Weak::upgrade);
        if let Some(w) = watch {
            let mut kinds = kinds.to_vec();
            kinds.sort();
            w.log.publish(GitChanges { root, kinds });
        }
    }

    // ---- reads ---------------------------------------------------------------

    /// `git/status`.
    pub async fn status(&self, p: gm::RepoParams) -> Result<gm::StatusResult, ErrorObject> {
        self.with_repo(&p.workspace, p.root, |repo| {
            let status = repo.status()?;
            Ok(gm::StatusResult {
                repo: gm::RepoInfo {
                    workdir: repo.workdir().to_string_lossy().into_owned(),
                    prefix: repo.prefix().to_owned(),
                },
                head: head(status.head),
                entries: status.entries.into_iter().map(status_entry).collect(),
            })
        })
        .await
    }

    /// `git/diff`.
    pub async fn diff(&self, p: gm::DiffParams) -> Result<gm::FileDiff, ErrorObject> {
        let context = p.context.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT);
        let (base, head) = (side(p.base), side(p.head));
        let path = p.path;
        self.with_repo(&p.workspace, p.root, move |repo| {
            repo.diff(&path, &base, &head, context).map(file_diff)
        })
        .await
    }

    /// A file's text as `version` has it, for a root-relative `path`;
    /// `None` if that version has no such file, or it is not UTF-8 text.
    /// Not a route: review threads anchor to the lines a diff side shows.
    pub async fn read_text(
        &self,
        workspace: &WorkspaceId,
        root: RootId,
        path: &str,
        version: gm::DiffSide,
    ) -> Result<Option<String>, ErrorObject> {
        let path = path.to_owned();
        let version = side(version);
        self.with_repo(workspace, root, move |repo| {
            let path = match repo.prefix() {
                "" => path,
                prefix => format!("{prefix}/{path}"),
            };
            Ok(repo
                .read_file(&version, &path)?
                .and_then(|bytes| String::from_utf8(bytes).ok()))
        })
        .await
    }

    /// `git/diffFiles`.
    pub async fn diff_files(
        &self,
        p: gm::DiffFilesParams,
    ) -> Result<gm::DiffFilesResult, ErrorObject> {
        let (base, head) = (side(p.base), side(p.head));
        self.with_repo(&p.workspace, p.root, move |repo| {
            let files = repo.changed_files(&base, &head)?;
            Ok(gm::DiffFilesResult {
                files: files
                    .into_iter()
                    .map(|f| gm::ChangedFile {
                        path: f.path,
                        change: change(f.change),
                        additions: f.additions,
                        deletions: f.deletions,
                        binary: f.binary,
                    })
                    .collect(),
            })
        })
        .await
    }

    /// `git/branches`.
    pub async fn branches(&self, p: gm::RepoParams) -> Result<gm::BranchesResult, ErrorObject> {
        self.with_repo(&p.workspace, p.root, |repo| {
            Ok(gm::BranchesResult {
                branches: repo
                    .branches()?
                    .into_iter()
                    .map(|b| gm::Branch {
                        name: b.name,
                        remote: b.remote,
                        current: b.current,
                        oid: b.oid,
                        upstream: b.upstream,
                        summary: b.summary,
                        time: b.time,
                    })
                    .collect(),
            })
        })
        .await
    }

    /// `git/stashList`.
    pub async fn stash_list(&self, p: gm::RepoParams) -> Result<gm::StashListResult, ErrorObject> {
        self.with_repo(&p.workspace, p.root, |repo| {
            Ok(gm::StashListResult {
                stashes: repo
                    .stashes()?
                    .into_iter()
                    .map(|s| gm::Stash {
                        index: u32::try_from(s.index).unwrap_or(u32::MAX),
                        message: s.message,
                        oid: s.oid,
                        time: s.time,
                    })
                    .collect(),
            })
        })
        .await
    }

    /// `git/blame`.
    pub async fn blame(&self, p: gm::BlameParams) -> Result<gm::BlameResult, ErrorObject> {
        let (path, rev) = (p.path, p.rev);
        self.with_repo(&p.workspace, p.root, move |repo| {
            let b = repo.blame(&path, rev.as_deref())?;
            Ok(gm::BlameResult {
                rev: b.rev,
                line_count: b.line_count,
                hunks: b
                    .hunks
                    .into_iter()
                    .map(|h| gm::BlameHunk {
                        start: h.start,
                        lines: h.lines,
                        orig_start: h.orig_start,
                        oid: h.oid,
                        orig_path: h.orig_path,
                    })
                    .collect(),
                commits: b
                    .commits
                    .into_iter()
                    .map(|(k, c)| {
                        (
                            k,
                            gm::CommitInfo {
                                oid: c.oid,
                                author: c.author,
                                email: c.email,
                                time: c.time,
                                summary: c.summary,
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>(),
            })
        })
        .await
    }

    /// `git/log`.
    pub async fn log(&self, p: gm::LogParams) -> Result<gm::LogResult, ErrorObject> {
        let options = mg::LogOptions {
            revs: p.revs,
            skip: p.skip as usize,
            limit: p.limit.unwrap_or(DEFAULT_LOG_PAGE) as usize,
        };
        self.with_repo(&p.workspace, p.root, move |repo| {
            let page = repo.log(&options)?;
            Ok(gm::LogResult {
                commits: page.commits.into_iter().map(log_commit).collect(),
                more: page.more,
            })
        })
        .await
    }

    // ---- writes --------------------------------------------------------------

    async fn stage_or_unstage(&self, p: gm::StageParams, stage: bool) -> Result<(), ErrorObject> {
        if p.paths.is_empty() {
            return Err(ErrorObject::invalid_params("no paths given"));
        }
        let (paths, lines) = (p.paths, p.lines);
        if lines.is_some() && paths.len() != 1 {
            return Err(ErrorObject::invalid_params(
                "a line selection is for exactly one path",
            ));
        }
        self.with_repo(&p.workspace, p.root, move |repo| match lines {
            Some(l) => {
                let sel = mg::LineSelection {
                    removed: l.removed,
                    added: l.added,
                };
                if stage {
                    repo.stage_lines(&paths[0], &sel)
                } else {
                    repo.unstage_lines(&paths[0], &sel)
                }
            }
            None if stage => repo.stage(&paths),
            None => repo.unstage(&paths),
        })
        .await?;
        self.changed(&p.workspace, p.root, &[GitChangeKind::Index]);
        Ok(())
    }

    /// `git/stage`.
    pub async fn stage(&self, p: gm::StageParams) -> Result<(), ErrorObject> {
        self.stage_or_unstage(p, true).await
    }

    /// `git/unstage`.
    pub async fn unstage(&self, p: gm::StageParams) -> Result<(), ErrorObject> {
        self.stage_or_unstage(p, false).await
    }

    /// `git/commit`.
    pub async fn commit(&self, p: gm::CommitParams) -> Result<gm::CommitResult, ErrorObject> {
        let (message, amend) = (p.message, p.amend);
        let c = self
            .with_repo(&p.workspace, p.root, move |repo| {
                repo.commit(&message, amend)
            })
            .await?;
        use GitChangeKind as K;
        self.changed(&p.workspace, p.root, &[K::Head, K::Index, K::Refs]);
        Ok(gm::CommitResult {
            oid: c.oid,
            summary: c.summary,
        })
    }

    /// `git/switchBranch`.
    pub async fn switch_branch(&self, p: gm::SwitchBranchParams) -> Result<(), ErrorObject> {
        let name = p.name;
        self.with_repo(&p.workspace, p.root, move |repo| repo.switch_branch(&name))
            .await?;
        use GitChangeKind as K;
        self.changed(
            &p.workspace,
            p.root,
            &[K::Head, K::Index, K::Refs, K::Worktree],
        );
        Ok(())
    }

    /// `git/createBranch`.
    pub async fn create_branch(&self, p: gm::CreateBranchParams) -> Result<(), ErrorObject> {
        let (name, start, switch) = (p.name, p.start, p.switch);
        self.with_repo(&p.workspace, p.root, move |repo| {
            repo.create_branch(&name, start.as_deref(), switch)
        })
        .await?;
        use GitChangeKind as K;
        let kinds: &[K] = if switch {
            &[K::Head, K::Index, K::Refs, K::Worktree]
        } else {
            &[K::Refs]
        };
        self.changed(&p.workspace, p.root, kinds);
        Ok(())
    }

    /// `git/stashPush`.
    pub async fn stash_push(
        &self,
        p: gm::StashPushParams,
    ) -> Result<gm::StashPushResult, ErrorObject> {
        let (message, untracked) = (p.message, p.include_untracked);
        let created = self
            .with_repo(&p.workspace, p.root, move |repo| {
                repo.stash_push(message.as_deref(), untracked)
            })
            .await?;
        use GitChangeKind as K;
        self.changed(&p.workspace, p.root, &[K::Index, K::Stash, K::Worktree]);
        Ok(gm::StashPushResult { created })
    }

    async fn stash_op(&self, p: gm::StashParams, op: StashOp) -> Result<(), ErrorObject> {
        let index = p.index as usize;
        self.with_repo(&p.workspace, p.root, move |repo| match op {
            StashOp::Pop => repo.stash_pop(index),
            StashOp::Apply => repo.stash_apply(index),
            StashOp::Drop => repo.stash_drop(index),
        })
        .await?;
        use GitChangeKind as K;
        let kinds: &[K] = match op {
            StashOp::Drop => &[K::Stash],
            StashOp::Pop | StashOp::Apply => &[K::Index, K::Stash, K::Worktree],
        };
        self.changed(&p.workspace, p.root, kinds);
        Ok(())
    }

    // ---- git/changes ---------------------------------------------------------

    /// The `git/changes` topic. Starts the workspace's git watch if needed.
    pub async fn changes(
        self: &Arc<Self>,
        p: gm::ChangesParams,
        since: Option<u64>,
    ) -> Result<EventFeed<GitChanges>, ErrorObject> {
        let watch = self.watch(&p.workspace).await?;
        let feed = watch.log.subscribe(since)?;
        let mut closed = watch.closed.subscribe();
        let until = async move {
            let _ = closed.wait_for(|c| *c).await;
        };
        // Each subscription keeps the watch alive.
        let keep = Arc::clone(&watch);
        Ok(EventFeed {
            head: feed.head,
            events: feed
                .events
                .take_until(until)
                .map(move |e| {
                    let _ = &keep;
                    e
                })
                .boxed(),
        })
    }

    async fn watch(
        self: &Arc<Self>,
        workspace: &WorkspaceId,
    ) -> Result<Arc<GitWatch>, ErrorObject> {
        let _starting = self.starting.lock().await;
        let live = self
            .watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace)
            .and_then(Weak::upgrade)
            .filter(|w| !*w.closed.borrow());
        if let Some(w) = live {
            return Ok(w);
        }
        // Fails for a closed workspace, as `fs/changes` does.
        let fs = self
            .workspaces
            .fs_changes(
                FsChangesParams {
                    workspace: workspace.clone(),
                },
                None,
            )
            .await?;
        let roots = self.workspaces.roots(workspace.clone()).await?;
        let git = self.git().ok();
        let dirs = tokio::task::spawn_blocking(move || git_dirs(git.as_deref(), &roots))
            .await
            .map_err(|e| ErrorObject::internal(format!("git task failed: {e}")))?;
        let watch = Arc::new(GitWatch::start(fs, dirs, self.debounce));
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(workspace.clone(), Arc::downgrade(&watch));
        Ok(watch)
    }
}

#[derive(Clone, Copy, Debug)]
enum StashOp {
    Pop,
    Apply,
    Drop,
}

/// Each root's git directory and common directory (refs, logs, stash).
#[derive(Clone, Debug)]
struct GitDirs {
    root: RootId,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

fn git_dirs(git: Option<&Git>, roots: &[(RootId, PathBuf)]) -> Vec<GitDirs> {
    let Some(git) = git else {
        return Vec::new();
    };
    roots
        .iter()
        .filter_map(|(root, path)| {
            let repo = git.open(path).ok()?;
            let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
            Some(GitDirs {
                root: *root,
                git_dir: canon(repo.git_dir()),
                common_dir: canon(repo.common_dir()),
            })
        })
        .collect()
}

/// What a change at `rel` (relative to a git or common directory) means.
fn classify(rel: &Path, in_git_dir: bool) -> Option<GitChangeKind> {
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let last = comps.last()?;
    if last.ends_with(".lock") {
        return None;
    }
    use GitChangeKind as K;
    Some(match comps.as_slice() {
        ["index"] if in_git_dir => K::Index,
        ["HEAD" | "ORIG_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"]
            if in_git_dir =>
        {
            K::Head
        }
        ["logs", "HEAD"] if in_git_dir => K::Head,
        ["refs", "stash"] | ["logs", "refs", "stash"] => K::Stash,
        ["packed-refs"] | ["refs", ..] | ["logs", "refs", ..] => K::Refs,
        _ => return None,
    })
}

type Pending = Arc<Mutex<BTreeMap<RootId, BTreeSet<GitChangeKind>>>>;

/// Watches one workspace's repositories for `git/changes`. Dropping it
/// stops the watch and ends its subscriptions.
struct GitWatch {
    log: Arc<SeqLog<GitChanges>>,
    closed: watch::Sender<bool>,
    _watcher: Option<RecommendedWatcher>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for GitWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitWatch")
            .field("head", &self.log.head())
            .finish_non_exhaustive()
    }
}

impl Drop for GitWatch {
    fn drop(&mut self) {
        self.closed.send_replace(true);
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl GitWatch {
    fn start(
        mut fs: EventFeed<coqo_protocol::methods::fs::FsChanges>,
        dirs: Vec<GitDirs>,
        debounce: Duration,
    ) -> Self {
        let log = Arc::new(SeqLog::new(RETAINED));
        let pending: Pending = Arc::default();
        let wake = Arc::new(Notify::new());
        let closed = watch::channel(false).0;

        let record = {
            let (pending, wake) = (Arc::clone(&pending), Arc::clone(&wake));
            move |root: RootId, kind: GitChangeKind| {
                pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(root)
                    .or_default()
                    .insert(kind);
                wake.notify_one();
            }
        };

        let watcher = {
            let record = record.clone();
            let dirs_cb = dirs.clone();
            let cb = move |res: notify::Result<notify::Event>| {
                let Ok(ev) = res else {
                    // Lost events: anything may have changed.
                    for d in &dirs_cb {
                        for k in [
                            GitChangeKind::Index,
                            GitChangeKind::Head,
                            GitChangeKind::Refs,
                        ] {
                            record(d.root, k);
                        }
                    }
                    return;
                };
                if matches!(ev.kind, EventKind::Access(_)) {
                    return;
                }
                for path in &ev.paths {
                    for d in &dirs_cb {
                        let kind = path
                            .strip_prefix(&d.git_dir)
                            .ok()
                            .and_then(|rel| classify(rel, true))
                            .or_else(|| {
                                path.strip_prefix(&d.common_dir)
                                    .ok()
                                    .and_then(|rel| classify(rel, d.common_dir == d.git_dir))
                            });
                        if let Some(kind) = kind {
                            record(d.root, kind);
                        }
                    }
                }
            };
            match notify::recommended_watcher(cb) {
                Ok(mut w) => {
                    for d in &dirs {
                        let mut targets = vec![(d.git_dir.clone(), RecursiveMode::NonRecursive)];
                        if d.common_dir != d.git_dir {
                            targets.push((d.common_dir.clone(), RecursiveMode::NonRecursive));
                        }
                        targets.push((d.common_dir.join("refs"), RecursiveMode::Recursive));
                        targets.push((d.common_dir.join("logs"), RecursiveMode::Recursive));
                        for (path, mode) in targets {
                            if path.exists()
                                && let Err(e) = w.watch(&path, mode)
                            {
                                tracing::warn!(path = %path.display(), error = %e, "cannot watch git directory");
                            }
                        }
                    }
                    Some(w)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cannot watch git directories; git/changes reports work-tree changes only");
                    None
                }
            }
        };

        let forward = {
            let record = record.clone();
            let closed = closed.clone();
            tokio::spawn(async move {
                while let Some(Ok(batch)) = fs.events.next().await {
                    record(batch.event.root, GitChangeKind::Worktree);
                }
                // The workspace closed (or the watcher stopped).
                closed.send_replace(true);
            })
        };

        let publish = {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                loop {
                    wake.notified().await;
                    tokio::time::sleep(debounce).await;
                    let batch =
                        std::mem::take(&mut *pending.lock().unwrap_or_else(|e| e.into_inner()));
                    for (root, kinds) in batch {
                        log.publish(GitChanges {
                            root,
                            kinds: kinds.into_iter().collect(),
                        });
                    }
                }
            })
        };

        Self {
            log,
            closed,
            _watcher: watcher,
            tasks: vec![forward, publish],
        }
    }
}

// ---- conversions ------------------------------------------------------------

fn side(s: gm::DiffSide) -> mg::Side {
    match s {
        gm::DiffSide::Worktree => mg::Side::Worktree,
        gm::DiffSide::Index => mg::Side::Index,
        gm::DiffSide::Rev(r) => mg::Side::Rev(r),
    }
}

fn change(c: mg::Change) -> gm::FileChange {
    match c {
        mg::Change::Added => gm::FileChange::Added,
        mg::Change::Modified => gm::FileChange::Modified,
        mg::Change::Deleted => gm::FileChange::Deleted,
        mg::Change::TypeChanged => gm::FileChange::TypeChanged,
        mg::Change::Untracked => gm::FileChange::Untracked,
    }
}

fn conflict(c: mg::Conflict) -> gm::Conflict {
    match c {
        mg::Conflict::BothDeleted => gm::Conflict::BothDeleted,
        mg::Conflict::AddedByUs => gm::Conflict::AddedByUs,
        mg::Conflict::DeletedByThem => gm::Conflict::DeletedByThem,
        mg::Conflict::AddedByThem => gm::Conflict::AddedByThem,
        mg::Conflict::DeletedByUs => gm::Conflict::DeletedByUs,
        mg::Conflict::BothAdded => gm::Conflict::BothAdded,
        mg::Conflict::BothModified => gm::Conflict::BothModified,
    }
}

fn status_entry(e: mg::StatusEntry) -> gm::StatusEntry {
    gm::StatusEntry {
        path: e.path,
        staged: e.staged.map(change),
        unstaged: e.unstaged.map(change),
        conflict: e.conflict.map(conflict),
    }
}

fn head(h: mg::Head) -> gm::Head {
    gm::Head {
        branch: h.branch,
        oid: h.oid,
        upstream: h.upstream,
        ahead: h.ahead,
        behind: h.behind,
    }
}

fn file_diff(d: mg::FileDiff) -> gm::FileDiff {
    let side = |s: mg::FileSide| gm::FileSide {
        mode: s.mode,
        oid: s.oid,
    };
    gm::FileDiff {
        path: d.path,
        old: d.old.map(side),
        new: d.new.map(side),
        binary: d.binary,
        additions: d.additions,
        deletions: d.deletions,
        hunks: d
            .hunks
            .into_iter()
            .map(|h| gm::Hunk {
                old_start: h.old_start,
                old_lines: h.old_lines,
                new_start: h.new_start,
                new_lines: h.new_lines,
                section: h.section,
                lines: h
                    .lines
                    .into_iter()
                    .map(|l| gm::DiffLine {
                        kind: match l.kind {
                            mg::LineKind::Context => gm::LineKind::Context,
                            mg::LineKind::Added => gm::LineKind::Added,
                            mg::LineKind::Removed => gm::LineKind::Removed,
                        },
                        old_line: l.old_line,
                        new_line: l.new_line,
                        text: l.text,
                        newline: l.newline,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn log_commit(c: mg::LogCommit) -> gm::LogCommit {
    gm::LogCommit {
        oid: c.oid,
        parents: c.parents,
        author: c.author,
        email: c.email,
        time: c.time,
        summary: c.summary,
        refs: c.refs,
        graph: gm::GraphRow {
            column: c.graph.column,
            edges: c
                .graph
                .edges
                .into_iter()
                .map(|e| gm::GraphEdge {
                    from: e.from,
                    to: e.to,
                    kind: match e.kind {
                        mg::EdgeKind::Through => gm::EdgeKind::Through,
                        mg::EdgeKind::In => gm::EdgeKind::In,
                        mg::EdgeKind::Out => gm::EdgeKind::Out,
                    },
                })
                .collect(),
        },
    }
}

// ---- routes -----------------------------------------------------------------

macro_rules! route {
    ($router:expr, $svc:expr, $method:ty, |$s:ident, $p:ident| $body:expr) => {{
        let svc = Arc::clone(&$svc);
        $router.method_with::<$method, _, _>(Access::daemon(), move |_ctx, $p| {
            let $s = Arc::clone(&svc);
            async move { $body.await }
        })
    }};
}

/// Register every `git/*` method and the `git/changes` topic. All need the
/// daemon token: agents use their own git, and no MCP tool wraps these.
pub fn register(router: Router, svc: Arc<GitService>) -> Router {
    let router = route!(router, svc, gm::Status, |s, p| s.status(p));
    let router = route!(router, svc, gm::Diff, |s, p| s.diff(p));
    let router = route!(router, svc, gm::DiffFiles, |s, p| s.diff_files(p));
    let router = route!(router, svc, gm::Stage, |s, p| s.stage(p));
    let router = route!(router, svc, gm::Unstage, |s, p| s.unstage(p));
    let router = route!(router, svc, gm::Commit, |s, p| s.commit(p));
    let router = route!(router, svc, gm::Branches, |s, p| s.branches(p));
    let router = route!(router, svc, gm::SwitchBranch, |s, p| s.switch_branch(p));
    let router = route!(router, svc, gm::CreateBranch, |s, p| s.create_branch(p));
    let router = route!(router, svc, gm::StashList, |s, p| s.stash_list(p));
    let router = route!(router, svc, gm::StashPush, |s, p| s.stash_push(p));
    let router = route!(router, svc, gm::StashPop, |s, p| s
        .stash_op(p, StashOp::Pop));
    let router = route!(router, svc, gm::StashApply, |s, p| s
        .stash_op(p, StashOp::Apply));
    let router = route!(router, svc, gm::StashDrop, |s, p| s
        .stash_op(p, StashOp::Drop));
    let router = route!(router, svc, gm::Blame, |s, p| s.blame(p));
    let router = route!(router, svc, gm::Log, |s, p| s.log(p));
    router.topic_with::<gm::Changes, _, _>(Access::daemon(), move |_ctx, p, since| {
        let s = Arc::clone(&svc);
        async move { s.changes(p, since).await }
    })
}

#[cfg(test)]
mod tests;
