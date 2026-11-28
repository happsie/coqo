// SPDX-License-Identifier: Apache-2.0

//! A window's git state: each root's status, branches and stashes, kept
//! fresh from `git/changes`, and the writes the Git pane makes.
//!
//! One model per window ([`GitModel::for_window`]), shared by the Git pane,
//! the diff tabs and anything else that wants per-path status (the file
//! tree's letters, #163: [`GitModel::letter`]).

use std::collections::HashSet;

use coqo_protocol::methods::{
    codes,
    git::{self as gm, Branch, GitChangeKind, GitChanges, Stash, StatusEntry, StatusResult},
    workspace::{RootId, WorkspaceId, WorkspaceInfo},
};
use coqo_ui::gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription,
    Task, WeakEntity, Window,
};

use crate::{
    daemon::{Daemon, DaemonError, DaemonEvent},
    workspace::{Workspaces, WorkspacesEvent},
};

/// What a root's repository is, as far as the UI knows.
#[derive(Clone, Debug, PartialEq)]
pub struct RootGit {
    pub root: RootId,
    pub name: String,
    /// The last `git/status`.
    pub status: Option<StatusResult>,
    pub branches: Vec<Branch>,
    pub stashes: Vec<Stash>,
    /// Why there is no status: not a repository, git off, an error.
    pub unavailable: Option<String>,
}

impl RootGit {
    /// A root nothing is known about yet.
    pub fn new(root: RootId) -> Self {
        Self {
            root,
            name: String::new(),
            status: None,
            branches: Vec::new(),
            stashes: Vec::new(),
            unavailable: None,
        }
    }

    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries().filter(|e| e.staged.is_some())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries()
            .filter(|e| e.unstaged.is_some() && e.conflict.is_none())
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries().filter(|e| e.conflict.is_some())
    }

    /// How many files have changes, staged or not (the Git tab's badge).
    pub fn changed_files(&self) -> usize {
        self.entries().count()
    }

    /// What `HEAD` is, for the branch button and the status bar: the
    /// branch, `detached at <short oid>`, or `no commits`. `None` until the
    /// status is in.
    pub fn head_label(&self) -> Option<String> {
        let head = &self.status.as_ref()?.head;
        Some(head.branch.clone().unwrap_or_else(|| match &head.oid {
            Some(oid) => format!("detached at {}", &oid[..oid.len().min(7)]),
            None => "no commits".into(),
        }))
    }

    /// Whether `HEAD` is on local branch `branch`, for the branch picker.
    /// The status decides once it is in, as for the branch button: the
    /// branch list and the status load separately, so after a switch the
    /// list's `current` can still name the old branch for a while.
    pub fn is_current(&self, branch: &Branch) -> bool {
        match &self.status {
            Some(s) => !branch.remote && s.head.branch.as_deref() == Some(branch.name.as_str()),
            None => branch.current,
        }
    }

    /// `↑n ↓m` against the upstream, or `None` when level with it.
    pub fn tracking_label(&self) -> Option<String> {
        let head = &self.status.as_ref()?.head;
        let mut track = Vec::new();
        if head.ahead > 0 {
            track.push(format!("↑{}", head.ahead));
        }
        if head.behind > 0 {
            track.push(format!("↓{}", head.behind));
        }
        (!track.is_empty()).then(|| track.join(" "))
    }

    fn entries(&self) -> impl Iterator<Item = &StatusEntry> {
        self.status.iter().flat_map(|s| s.entries.iter())
    }
}

/// One part of a root's state, loaded on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Part {
    Status,
    Branches,
    Stashes,
}

/// Emitted by a [`GitModel`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitModelEvent {
    /// `git/changes` (or a write made here) changed `kinds` of `root`.
    Changed {
        root: RootId,
        kinds: Vec<GitChangeKind>,
    },
    /// A commit landed.
    Committed { root: RootId, summary: String },
}

/// What a stash row's buttons do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StashAction {
    Apply,
    Pop,
    Drop,
}

/// The git state of one window's workspace.
pub struct GitModel {
    workspace: Option<WorkspaceInfo>,
    roots: Vec<RootGit>,
    /// The root the Git pane shows, picked with its chips; `None` follows
    /// the first repository.
    selected: Option<RootId>,
    in_flight: HashSet<(RootId, Part)>,
    stale: HashSet<(RootId, Part)>,
    /// The last write that failed, for the pane to show.
    error: Option<String>,
    /// Writes in flight.
    busy: usize,
    _changes: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for GitModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitModel")
            .field("workspace", &self.workspace.as_ref().map(|w| &w.id))
            .field("roots", &self.roots.len())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<GitModelEvent> for GitModel {}

/// The models, by window.
#[derive(Default)]
struct GitModels(Vec<(AnyWindowHandle, WeakEntity<GitModel>)>);

impl Global for GitModels {}

impl GitModel {
    /// `window`'s model, made on first use. The views that hold it keep it
    /// alive.
    pub fn for_window(window: &mut Window, cx: &mut App) -> Entity<GitModel> {
        let handle = window.window_handle();
        let models = cx.default_global::<GitModels>();
        models.0.retain(|(_, m)| m.upgrade().is_some());
        if let Some(model) = models
            .0
            .iter()
            .find(|(h, _)| *h == handle)
            .and_then(|(_, m)| m.upgrade())
        {
            return model;
        }
        let workspaces = Workspaces::for_window(window, cx);
        let model = cx.new(|cx| GitModel::new(workspaces, cx));
        cx.default_global::<GitModels>()
            .0
            .push((handle, model.downgrade()));
        model
    }

    fn new(workspaces: Option<Entity<Workspaces>>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(workspaces) = &workspaces {
            subscriptions.push(cx.subscribe(workspaces, |this, workspaces, event, cx| {
                if *event == WorkspacesEvent::CurrentChanged {
                    let info = workspaces.read(cx).current().cloned();
                    this.set_workspace(info, cx);
                }
            }));
        }
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| match event {
                DaemonEvent::Connected => {
                    let info = this.workspace.clone();
                    this.set_workspace(info, cx);
                }
                DaemonEvent::Disconnected => {
                    this.in_flight.clear();
                    this.stale.clear();
                    this.busy = 0;
                    cx.notify();
                }
            }));
        }
        let current = workspaces
            .as_ref()
            .and_then(|w| w.read(cx).current().cloned());
        let mut this = Self {
            workspace: None,
            roots: Vec::new(),
            selected: None,
            in_flight: HashSet::new(),
            stale: HashSet::new(),
            error: None,
            busy: 0,
            _changes: None,
            _subscriptions: subscriptions,
        };
        this.set_workspace(current, cx);
        this
    }

    pub fn workspace(&self) -> Option<&WorkspaceInfo> {
        self.workspace.as_ref()
    }

    /// Every root of the workspace, in display order.
    pub fn roots(&self) -> &[RootGit] {
        &self.roots
    }

    pub fn root(&self, root: RootId) -> Option<&RootGit> {
        self.roots.iter().find(|r| r.root == root)
    }

    /// The root the Git pane shows, and the sidebar badge and status bar
    /// describe: the selected one, else the first repository, else the
    /// first root.
    pub fn shown_root(&self) -> Option<RootId> {
        self.selected
            .filter(|s| self.roots.iter().any(|r| r.root == *s))
            .or_else(|| {
                self.roots
                    .iter()
                    .find(|r| r.status.is_some())
                    .map(|r| r.root)
            })
            .or_else(|| self.roots.first().map(|r| r.root))
    }

    /// Show `root` (the pane's root chips).
    pub fn select_root(&mut self, root: RootId, cx: &mut Context<Self>) {
        self.selected = Some(root);
        cx.notify();
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn dismiss_error(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        cx.notify();
    }

    /// Whether a write is in flight.
    pub fn is_busy(&self) -> bool {
        self.busy > 0
    }

    /// Whether `root`'s status is still loading for the first time.
    pub fn is_loading(&self, root: RootId) -> bool {
        self.root(root)
            .is_some_and(|r| r.status.is_none() && r.unavailable.is_none())
    }

    /// The status letter of `path` (root-relative, `/`-separated) for a
    /// file tree: `M`, `A`, `D`, `T`, `?` or `U` (see
    /// [`StatusEntry::letter`]). `None` when unchanged or unknown.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn letter(&self, root: RootId, path: &str) -> Option<char> {
        let status = self.root(root)?.status.as_ref()?;
        let git_path = if status.repo.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}/{path}", status.repo.prefix)
        };
        status
            .entries
            .iter()
            .find(|e| e.path == git_path || e.path.strip_suffix('/') == Some(&git_path))
            .and_then(StatusEntry::letter)
    }

    /// Show `info` (or nothing): subscribe to its `git/changes` and load
    /// every root. The same workspace again (a reconnect) keeps what's
    /// shown until the new state lands.
    fn set_workspace(&mut self, info: Option<WorkspaceInfo>, cx: &mut Context<Self>) {
        let same = self.workspace.as_ref().map(|w| &w.id) == info.as_ref().map(|w| &w.id);
        if !same {
            self.roots.clear();
            self.selected = None;
            self.error = None;
        }
        self.in_flight.clear();
        self.stale.clear();
        self._changes = None;
        self.workspace = info;
        let Some(info) = self.workspace.clone() else {
            cx.notify();
            return;
        };
        let previous = std::mem::take(&mut self.roots);
        self.roots = info
            .roots
            .iter()
            .map(|r| {
                let mut root = previous
                    .iter()
                    .find(|p| p.root == r.id)
                    .cloned()
                    .unwrap_or_else(|| RootGit::new(r.id));
                root.name.clone_from(&r.name);
                root
            })
            .collect();
        self.subscribe(info.id.clone(), cx);
        self.reload(cx);
        cx.notify();
    }

    /// Load every part of every root.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let roots: Vec<RootId> = self.roots.iter().map(|r| r.root).collect();
        for root in roots {
            for part in [Part::Status, Part::Branches, Part::Stashes] {
                self.load(root, part, cx);
            }
        }
    }

    fn subscribe(&mut self, workspace: WorkspaceId, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let changes = daemon
            .read(cx)
            .subscribe::<gm::Changes>(gm::ChangesParams { workspace });
        self._changes = Some(cx.spawn(async move |this, cx| {
            let Ok(mut stream) = changes.await else {
                return;
            };
            // The first loads raced the subscription: a change made before
            // the daemon's watch was up is in no event. Now that every
            // later change is, treat everything as changed once, so the
            // model and whatever follows it (the diff tabs) catch up.
            if this
                .update(cx, |this, cx| this.everything_changed(cx))
                .is_err()
            {
                return;
            }
            while let Some(Ok(changes)) = stream.next().await {
                if this
                    .update(cx, |this, cx| this.changed(changes, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn everything_changed(&mut self, cx: &mut Context<Self>) {
        use GitChangeKind as K;
        let roots: Vec<RootId> = self.roots.iter().map(|r| r.root).collect();
        for root in roots {
            let kinds = vec![K::Worktree, K::Index, K::Head, K::Refs, K::Stash];
            self.changed(GitChanges { root, kinds }, cx);
        }
    }

    fn changed(&mut self, changes: GitChanges, cx: &mut Context<Self>) {
        use GitChangeKind as K;
        let has = |k: K| changes.kinds.contains(&k);
        if has(K::Worktree) || has(K::Index) || has(K::Head) {
            self.load(changes.root, Part::Status, cx);
        }
        if has(K::Refs) || has(K::Head) {
            self.load(changes.root, Part::Branches, cx);
        }
        if has(K::Stash) {
            self.load(changes.root, Part::Stashes, cx);
        }
        cx.emit(GitModelEvent::Changed {
            root: changes.root,
            kinds: changes.kinds,
        });
    }

    fn params(&self, root: RootId) -> Option<gm::RepoParams> {
        Some(gm::RepoParams {
            workspace: self.workspace.as_ref()?.id.clone(),
            root,
        })
    }

    fn load(&mut self, root: RootId, part: Part, cx: &mut Context<Self>) {
        let (Some(params), Some(daemon)) = (self.params(root), Daemon::global(cx)) else {
            return;
        };
        if !self.in_flight.insert((root, part)) {
            self.stale.insert((root, part));
            return;
        }
        let workspace = params.workspace.clone();
        let daemon = daemon.read(cx);
        let task = match part {
            Part::Status => {
                let call = daemon.request::<gm::Status>(params);
                cx.spawn(async move |this, cx| {
                    let result = call.await;
                    this.update(cx, |this, cx| {
                        if let Some(r) = this.loaded(&workspace, root, part, cx) {
                            match result {
                                Ok(status) => {
                                    r.status = Some(status);
                                    r.unavailable = None;
                                }
                                Err(DaemonError::NotConnected) => {}
                                Err(err) => {
                                    r.status = None;
                                    r.unavailable = Some(unavailable(&err));
                                }
                            }
                        }
                    })
                    .ok();
                })
            }
            Part::Branches => {
                let call = daemon.request::<gm::Branches>(params);
                cx.spawn(async move |this, cx| {
                    let result = call.await;
                    this.update(cx, |this, cx| {
                        if let (Some(r), Ok(b)) = (this.loaded(&workspace, root, part, cx), result)
                        {
                            r.branches = b.branches;
                        }
                    })
                    .ok();
                })
            }
            Part::Stashes => {
                let call = daemon.request::<gm::StashList>(params);
                cx.spawn(async move |this, cx| {
                    let result = call.await;
                    this.update(cx, |this, cx| {
                        if let (Some(r), Ok(s)) = (this.loaded(&workspace, root, part, cx), result)
                        {
                            r.stashes = s.stashes;
                        }
                    })
                    .ok();
                })
            }
        };
        task.detach();
    }

    /// A load of `part` landed: the root to store it in, if it's still
    /// shown. Loads it again if it went stale in flight.
    fn loaded(
        &mut self,
        workspace: &WorkspaceId,
        root: RootId,
        part: Part,
        cx: &mut Context<Self>,
    ) -> Option<&mut RootGit> {
        if self.workspace.as_ref().map(|w| &w.id) != Some(workspace) {
            return None;
        }
        self.in_flight.remove(&(root, part));
        if self.stale.remove(&(root, part)) {
            self.load(root, part, cx);
        }
        cx.notify();
        self.roots.iter_mut().find(|r| r.root == root)
    }

    // ---- writes ------------------------------------------------------------

    /// Run a write; on success reload `parts` of `root` and call `done`.
    fn write<M: coqo_protocol::Method>(
        &mut self,
        root: RootId,
        params: M::Params,
        parts: &'static [Part],
        done: impl FnOnce(&mut Self, M::Result, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let call = daemon.read(cx).request::<M>(params);
        self.busy += 1;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.busy = this.busy.saturating_sub(1);
                match result {
                    Ok(value) => {
                        this.error = None;
                        for part in parts {
                            this.load(root, *part, cx);
                        }
                        done(this, value, cx);
                    }
                    Err(err) => this.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn stage_params(&self, root: RootId, paths: Vec<String>) -> Option<gm::StageParams> {
        Some(gm::StageParams {
            workspace: self.workspace.as_ref()?.id.clone(),
            root,
            paths,
            lines: None,
        })
    }

    /// Stage whole files (`git add -A`).
    pub fn stage(&mut self, root: RootId, paths: Vec<String>, cx: &mut Context<Self>) {
        if let Some(p) = self.stage_params(root, paths) {
            self.write::<gm::Stage>(root, p, &[Part::Status], |_, (), _| {}, cx);
        }
    }

    /// Unstage whole files.
    pub fn unstage(&mut self, root: RootId, paths: Vec<String>, cx: &mut Context<Self>) {
        if let Some(p) = self.stage_params(root, paths) {
            self.write::<gm::Unstage>(root, p, &[Part::Status], |_, (), _| {}, cx);
        }
    }

    /// Commit what's staged.
    pub fn commit(&mut self, root: RootId, message: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone()) else {
            return;
        };
        let params = gm::CommitParams {
            workspace,
            root,
            message,
            amend: false,
        };
        self.write::<gm::Commit>(
            root,
            params,
            &[Part::Status, Part::Branches],
            move |_, result, cx| {
                cx.emit(GitModelEvent::Committed {
                    root,
                    summary: result.summary,
                });
            },
            cx,
        );
    }

    pub fn switch_branch(&mut self, root: RootId, name: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone()) else {
            return;
        };
        let params = gm::SwitchBranchParams {
            workspace,
            root,
            name,
        };
        self.write::<gm::SwitchBranch>(
            root,
            params,
            &[Part::Status, Part::Branches],
            |_, (), _| {},
            cx,
        );
    }

    /// Create branch `name` at `HEAD` and switch to it.
    pub fn create_branch(&mut self, root: RootId, name: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone()) else {
            return;
        };
        let params = gm::CreateBranchParams {
            workspace,
            root,
            name,
            start: None,
            switch: true,
        };
        self.write::<gm::CreateBranch>(
            root,
            params,
            &[Part::Status, Part::Branches],
            |_, (), _| {},
            cx,
        );
    }

    /// Stash the work tree and the index, untracked files included.
    pub fn stash_push(&mut self, root: RootId, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone()) else {
            return;
        };
        let params = gm::StashPushParams {
            workspace,
            root,
            message: None,
            include_untracked: true,
        };
        self.write::<gm::StashPush>(
            root,
            params,
            &[Part::Status, Part::Stashes],
            |this, result, cx| {
                if !result.created {
                    this.error = Some("No local changes to stash".into());
                    cx.notify();
                }
            },
            cx,
        );
    }

    pub fn stash(&mut self, root: RootId, index: u32, action: StashAction, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.as_ref().map(|w| w.id.clone()) else {
            return;
        };
        let params = gm::StashParams {
            workspace,
            root,
            index,
        };
        let parts = &[Part::Status, Part::Stashes];
        match action {
            StashAction::Apply => {
                self.write::<gm::StashApply>(root, params, parts, |_, (), _| {}, cx);
            }
            StashAction::Pop => self.write::<gm::StashPop>(root, params, parts, |_, (), _| {}, cx),
            StashAction::Drop => {
                self.write::<gm::StashDrop>(root, params, parts, |_, (), _| {}, cx);
            }
        }
    }

    /// Show `roots` without a daemon (screenshots, tests).
    #[cfg(test)]
    pub fn set_for_tests(
        &mut self,
        workspace: WorkspaceInfo,
        roots: Vec<RootGit>,
        cx: &mut Context<Self>,
    ) {
        self._changes = None;
        self.workspace = Some(workspace);
        self.roots = roots;
        cx.notify();
    }
}

/// Why a root has no status, in words.
fn unavailable(err: &DaemonError) -> String {
    match err.code() {
        Some(codes::NOT_A_REPOSITORY) => "Not a git repository".into(),
        _ => err.to_string(),
    }
}
