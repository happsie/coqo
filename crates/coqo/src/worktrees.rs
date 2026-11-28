// SPDX-License-Identifier: Apache-2.0

//! The worktree switcher (#92, ADR 0028), at the top of the sidebar under
//! the project's name.
//!
//! Each worktree of a repository is its own workspace (files, chats,
//! terminals, git state), and the rail shows the repository once. The
//! switcher shows the window's worktree and its branch, and opens a list of
//! every worktree of the repository (`git/worktrees`, so worktrees made
//! outside Coqo are there too) with its branch, its change count and what
//! its agents do. From the list:
//!
//! - a **click** switches the window to that worktree's workspace (opening
//!   one on its folder the first time); the others' agents keep running;
//! - **New worktree…** asks for a branch name and a base (the current
//!   branch by default) and adds it (`git/addWorktree`), then switches;
//! - the **trash** removes one (`git/removeWorktree`), never the main
//!   worktree and never forced: uncommitted changes stop it, and commits no
//!   other branch has need a confirmation.
//!
//! The palette's "Switch worktree" and "New worktree" open the list and
//! the form.
//!
//! **Live** (#359): worktrees made, removed or pruned outside Coqo (an
//! agent's `git worktree add`, Claude Code's `.claude/worktrees/*`, another
//! tool) reach `git/changes` as `worktrees`, and an open list fetches
//! itself again. Nothing opens or takes focus: a new worktree is a row,
//! and a click opens it as any other. A row a session of another worktree
//! is working in (a subagent's, [`SessionInfo::worktrees`]) says so:
//! "Claude working · from <chat>". The work stays that chat's, so the
//! project's rail button already counts it.
//!
//! [`SessionInfo::worktrees`]: coqo_protocol::methods::agents::SessionInfo::worktrees

use coqo_protocol::methods::{
    agents::{AgentState, SessionInfo},
    git::{
        self as gm, AddWorktreeParams, GitChangeKind, RemoveWorktreeParams, RepoParams, Worktree,
        WorktreesResult,
    },
    workspace::WorkspaceInfo,
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
        IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
        Styled as _, Subscription, Task, Window, deferred, div, prelude::FluentBuilder as _, px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{Icon, TestSupportExt as _, icon, popover, primary, raised},
};

use crate::{
    activity::AgentActivity,
    daemon::Daemon,
    git::model::{GitModel, GitModelEvent},
    shell::rail::{Badge, Dot},
    workspace::{KnownWorkspaces, Workspaces, WorkspacesEvent},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    worktree,
    [
        /// Open the worktree switcher's list.
        SwitchWorktree,
        /// Open the switcher's "New worktree" form.
        NewWorktree,
    ]
);

/// Register the switcher's palette commands.
pub fn init(cx: &mut App) {
    crate::commands::register(
        cx,
        [
            crate::commands::Command::new(
                "worktree.switch",
                "Switch worktree",
                "Git",
                SwitchWorktree,
            ),
            crate::commands::Command::new("worktree.new", "New worktree", "Git", NewWorktree),
        ],
    );
}

/// The "New worktree" form.
struct NewForm {
    branch: Entity<PromptInput>,
    base: Entity<PromptInput>,
    _events: Vec<Subscription>,
}

/// A removal that needs the user: the worktree has work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRemoval {
    pub path: String,
    pub name: String,
    pub changes: u32,
    pub unpushed: u32,
}

/// The worktree switcher of one window. See the module docs.
pub struct WorktreeSwitcher {
    workspaces: Entity<Workspaces>,
    git: Entity<GitModel>,
    open: bool,
    list: Option<WorktreesResult>,
    loading: bool,
    busy: bool,
    error: Option<String>,
    form: Option<NewForm>,
    removal: Option<PendingRemoval>,
    _request: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for WorktreeSwitcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorktreeSwitcher")
            .field("open", &self.open)
            .field("list", &self.list.as_ref().map(|l| l.worktrees.len()))
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// The folder name of `path`, which names a worktree in the list.
fn folder_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_owned()
}

impl WorktreeSwitcher {
    pub fn new(
        workspaces: Entity<Workspaces>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git = GitModel::for_window(window, cx);
        let mut subscriptions = vec![
            cx.observe(&git, |_, _, cx| cx.notify()),
            // Worktrees made or removed anywhere: list them again.
            cx.subscribe(&git, |this, _, event, cx| {
                if let GitModelEvent::Changed { root, kinds } = event
                    && kinds.contains(&GitChangeKind::Worktrees)
                    && this.repo(cx).is_some_and(|r| r.root == *root)
                    && this.open
                    && !this.busy
                {
                    this.refresh(cx);
                }
            }),
            cx.subscribe(&workspaces, |this, _, event, cx| {
                if *event == WorkspacesEvent::CurrentChanged {
                    this.list = None;
                    this.removal = None;
                    this.error = None;
                    if this.open {
                        this.refresh(cx);
                    }
                    cx.notify();
                }
            }),
        ];
        if let Some(known) = KnownWorkspaces::global(cx) {
            subscriptions.push(cx.observe(&known, |_, _, cx| cx.notify()));
        }
        if let Some(activity) = AgentActivity::global(cx) {
            subscriptions.push(cx.observe(&activity, |_, _, cx| cx.notify()));
        }
        Self {
            workspaces,
            git,
            open: false,
            list: None,
            loading: false,
            busy: false,
            error: None,
            form: None,
            removal: None,
            _request: None,
            _subscriptions: subscriptions,
        }
    }

    fn current(&self, cx: &App) -> Option<WorkspaceInfo> {
        self.workspaces.read(cx).current().cloned()
    }

    /// The current workspace and its first root, when it is in a repository.
    fn repo(&self, cx: &App) -> Option<RepoParams> {
        let current = self.current(cx)?;
        current.project.as_ref()?;
        Some(RepoParams {
            workspace: current.id.clone(),
            root: current.roots.first()?.id,
        })
    }

    fn daemon(cx: &App) -> Option<Entity<Daemon>> {
        Daemon::global(cx).filter(|d| d.read(cx).is_connected())
    }

    /// Whether the list is open.
    #[cfg_attr(not(test), allow(dead_code))] // Tests use it.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The worktrees last listed.
    #[cfg_attr(not(test), allow(dead_code))] // Tests use it.
    pub fn worktrees(&self) -> &[Worktree] {
        self.list.as_ref().map_or(&[], |l| &l.worktrees)
    }

    /// Whether a list, an add or a remove is in flight.
    #[cfg_attr(not(test), allow(dead_code))] // Tests use it.
    pub fn is_busy(&self) -> bool {
        self.loading || self.busy
    }

    /// Why the last request failed.
    #[cfg_attr(not(test), allow(dead_code))] // Tests use it.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// A removal waiting for the user.
    #[cfg_attr(not(test), allow(dead_code))] // Tests use it.
    pub fn pending_removal(&self) -> Option<&PendingRemoval> {
        self.removal.as_ref()
    }

    /// The form's branch and base fields, while it is open.
    #[cfg_attr(not(test), allow(dead_code))] // Tests use it.
    pub fn form_inputs(&self) -> Option<(Entity<PromptInput>, Entity<PromptInput>)> {
        self.form
            .as_ref()
            .map(|f| (f.branch.clone(), f.base.clone()))
    }

    /// Open the list (and fetch it), or close it.
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
        } else {
            self.open_list(cx);
        }
    }

    /// Open the list and fetch it.
    pub fn open_list(&mut self, cx: &mut Context<Self>) {
        if self.repo(cx).is_none() {
            return;
        }
        self.open = true;
        self.refresh(cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.form = None;
        self.removal = None;
        self.error = None;
        cx.notify();
    }

    /// Fetch the list again (`git/worktrees`).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (Some(repo), Some(daemon)) = (self.repo(cx), Self::daemon(cx)) else {
            return;
        };
        let listed = daemon.read(cx).request::<gm::Worktrees>(repo);
        self.loading = true;
        self._request = Some(cx.spawn(async move |this, cx| {
            let result = listed.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(list) => this.list = Some(list),
                    Err(err) => this.error = Some(format!("Couldn't list the worktrees: {err}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Switch the window to the worktree at `path`: its workspace, or a new
    /// one on its folder.
    pub fn select(&mut self, path: &str, cx: &mut Context<Self>) {
        let known = KnownWorkspaces::global(cx)
            .and_then(|k| k.read(cx).on_folder(path).map(|w| w.id.clone()));
        self.close(cx);
        self.workspaces.update(cx, |w, cx| match known {
            Some(id) => w.switch_to(id, cx),
            None => w.open_path(path.into(), cx),
        });
    }

    /// Open the "New worktree" form, its base set to the current branch.
    pub fn start_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo(cx).is_none() {
            return;
        }
        if !self.open {
            self.open = true;
            self.refresh(cx);
        }
        self.removal = None;
        let base = self
            .git
            .read(cx)
            .roots()
            .first()
            .and_then(|r| r.status.as_ref()?.head.branch.clone())
            .unwrap_or_default();
        let branch = cx.new(|cx| PromptInput::new("Branch name", 1, 1, window, cx));
        let base_input = cx.new(|cx| {
            let mut input = PromptInput::new("Base (default: HEAD)", 1, 1, window, cx);
            input.set_text(base, window, cx);
            input
        });
        let on_event =
            |this: &mut Self, event: &PromptInputEvent, cx: &mut Context<Self>| match event {
                PromptInputEvent::Submit => this.create(cx),
                PromptInputEvent::Cancel => {
                    this.form = None;
                    cx.notify();
                }
                _ => {}
            };
        let events = vec![
            cx.subscribe(&branch, move |this, _, event, cx| on_event(this, event, cx)),
            cx.subscribe(&base_input, move |this, _, event, cx| {
                on_event(this, event, cx)
            }),
        ];
        let focus = coqo_ui::gpui::Focusable::focus_handle(branch.read(cx), cx);
        window.focus(&focus, cx);
        self.form = Some(NewForm {
            branch,
            base: base_input,
            _events: events,
        });
        cx.notify();
    }

    /// Add the worktree the form describes, then switch to it.
    pub fn create(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let branch = form.branch.read(cx).text(cx).trim().to_owned();
        let base = form.base.read(cx).text(cx).trim().to_owned();
        if branch.is_empty() || self.busy {
            return;
        }
        let (Some(repo), Some(daemon)) = (self.repo(cx), Self::daemon(cx)) else {
            return;
        };
        let added = daemon
            .read(cx)
            .request::<gm::AddWorktree>(AddWorktreeParams {
                workspace: repo.workspace,
                root: repo.root,
                branch,
                base: (!base.is_empty()).then_some(base),
                path: None,
            });
        self.busy = true;
        self.error = None;
        cx.notify();
        self._request = Some(cx.spawn(async move |this, cx| {
            let result = added.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(added) => {
                        this.close(cx);
                        let id = added.workspace.id;
                        this.workspaces.update(cx, |w, cx| w.switch_to(id, cx));
                    }
                    Err(err) => this.error = Some(format!("Couldn't add the worktree: {err}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Remove the worktree at `path` (`confirm`: although it has commits no
    /// other branch has). Work in it asks the user first.
    pub fn remove(&mut self, path: &str, confirm: bool, cx: &mut Context<Self>) {
        let (Some(repo), Some(daemon)) = (self.repo(cx), Self::daemon(cx)) else {
            return;
        };
        if self.busy {
            return;
        }
        let current_path = self
            .current(cx)
            .and_then(|c| c.roots.first().map(|r| r.path.clone()));
        let main = self
            .list
            .as_ref()
            .and_then(|l| l.worktrees.iter().find(|w| w.main))
            .map(|w| w.path.clone());
        let removed = daemon
            .read(cx)
            .request::<gm::RemoveWorktree>(RemoveWorktreeParams {
                workspace: repo.workspace,
                root: repo.root,
                path: path.to_owned(),
                confirm,
            });
        let path = path.to_owned();
        self.busy = true;
        self.error = None;
        cx.notify();
        self._request = Some(cx.spawn(async move |this, cx| {
            let result = removed.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(r) if r.removed => {
                        this.removal = None;
                        // The window's own worktree went: show the main one.
                        if current_path.as_deref() == Some(path.as_str())
                            && let Some(main) = main
                        {
                            this.select(&main, cx);
                        } else {
                            this.refresh(cx);
                        }
                    }
                    Ok(r) => {
                        this.removal = Some(PendingRemoval {
                            name: folder_name(&path),
                            path,
                            changes: r.changes,
                            unpushed: r.unpushed,
                        });
                    }
                    Err(err) => {
                        this.removal = None;
                        this.error = Some(format!("Couldn't remove the worktree: {err}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Show `list`, open or not, without asking the daemon, for
    /// screenshots.
    #[cfg(test)]
    pub fn set_for_tests(&mut self, list: WorktreesResult, open: bool, cx: &mut Context<Self>) {
        self.list = Some(list);
        self.open = open;
        cx.notify();
    }

    fn cancel_removal(&mut self, cx: &mut Context<Self>) {
        self.removal = None;
        cx.notify();
    }
}

/// Whether a worktree's folder is just its branch's name (`feature/login`
/// in `feature-login`, as new worktrees are named).
fn same_name(branch: &str, folder: &str) -> bool {
    branch.replace('/', "-") == folder
}

/// A session of another worktree working in this one: its state, and
/// "Claude working · from <chat>" (#359).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visitor {
    pub state: AgentState,
    pub label: String,
}

/// The busy session that went to the worktree at `path` last, among
/// `sessions`.
pub fn visitor<'a>(
    sessions: impl IntoIterator<Item = &'a SessionInfo>,
    path: &str,
) -> Option<Visitor> {
    let (info, _) = sessions
        .into_iter()
        .filter(|s| !s.closed && !matches!(s.state, AgentState::Idle | AgentState::Done))
        .filter_map(|s| {
            let since = s.worktrees.iter().find(|w| w.path == path)?.since;
            Some((s, since))
        })
        .max_by(|(a, x), (b, y)| x.cmp(y).then(b.id.cmp(&a.id)))?;
    let agent = crate::threads::provider_name(Some(&info.provider), &Default::default());
    let doing = match info.state {
        AgentState::NeedsInput | AgentState::NeedsPermission | AgentState::Blocked => "waiting",
        AgentState::Error => "failed",
        _ => "working",
    };
    let title = info
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "New chat".into());
    Some(Visitor {
        state: info.state,
        label: format!("{agent} {doing} · from {title}"),
    })
}

/// An agent-state dot for a worktree's row, as the rail draws them.
fn state_of(path: &str, cx: &App) -> (Option<Badge>, Option<Dot>) {
    let id =
        KnownWorkspaces::global(cx).and_then(|k| k.read(cx).on_folder(path).map(|w| w.id.clone()));
    let activity = match (id, AgentActivity::global(cx)) {
        (Some(id), Some(a)) => a.read(cx).of(&id.0),
        _ => Default::default(),
    };
    (Badge::of(activity), Dot::of(activity))
}

impl Render for WorktreeSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(current) = self.current(cx) else {
            return div().into_any_element();
        };
        let Some(project) = current.project.clone() else {
            return div().into_any_element();
        };
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let root = current
            .roots
            .first()
            .map(|r| r.path.clone())
            .unwrap_or_default();
        let branch = self
            .git
            .read(cx)
            .roots()
            .first()
            .and_then(|r| r.head_label());
        let folder = folder_name(&root);
        // The branch leads; the folder follows when it says something else
        // (`feature/login` in `feature-login` says nothing more).
        let (label, detail) = match branch {
            Some(b) if same_name(&b, &folder) => (b, None),
            Some(b) => (b, Some(folder)),
            None => (folder, None),
        };
        let button = raised(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(10.))
                .py(px(5.))
                .rounded(m.radius_control)
                .text_size(m.text_sm),
            cx,
        )
        .id("worktree-switcher")
        .test_support()
        .cursor_pointer()
        .child(icon(Icon::GitBranch, px(13.), p.text_muted.hsla()))
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .overflow_hidden()
                .whitespace_nowrap()
                .child(
                    div()
                        .id("worktree-branch")
                        .test_support()
                        .min_w_0()
                        .text_ellipsis()
                        .overflow_hidden()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text.hsla())
                        .child(label),
                )
                .children(detail.map(|d| {
                    div()
                        .flex_none()
                        .max_w(px(120.))
                        .text_ellipsis()
                        .overflow_hidden()
                        .text_color(p.text_subtle.hsla())
                        .child(d)
                })),
        )
        .child(icon(Icon::ChevronDown, px(12.), p.text_muted.hsla()))
        .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)));
        let mut slot = div().relative().child(button);
        if self.open {
            slot = slot.child(self.render_list(&root, cx));
        }
        div()
            .id("worktree-header")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(m.gap)
            .pt(m.gap)
            .child(
                div()
                    .px(px(4.))
                    .text_size(m.text_label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_subtle.hsla())
                    .truncate()
                    .child(project.name.to_uppercase()),
            )
            .child(slot)
            .into_any_element()
    }
}

impl WorktreeSwitcher {
    fn render_list(&self, current: &str, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let light = t.appearance == coqo_ui::Appearance::Light;
        let rows: Vec<AnyElement> = self
            .worktrees()
            .iter()
            .enumerate()
            .map(|(ix, wt)| self.render_row(ix, wt, wt.path == current, light, cx))
            .collect();
        let mut card = popover(cx)
            .id("worktree-popover")
            .occlude()
            .test_support()
            .w(px(320.))
            .py(px(6.))
            .text_size(m.text_sm)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .child(
                div()
                    .px(px(16.))
                    .pt(px(6.))
                    .pb(px(6.))
                    .text_size(m.text_label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_subtle.hsla())
                    .child("WORKTREES"),
            )
            .children(rows)
            .when(self.list.is_none() && self.loading, |card| {
                card.child(
                    div()
                        .px(px(16.))
                        .py(px(6.))
                        .text_color(p.text_subtle.hsla())
                        .child("Loading…"),
                )
            });
        if let Some(removal) = &self.removal {
            card = card.child(self.render_removal(removal, cx));
        }
        if let Some(error) = &self.error {
            card = card.child(
                div()
                    .id("worktree-error")
                    .test_support()
                    .mx(px(12.))
                    .my(px(4.))
                    .text_size(m.text_xs)
                    .text_color(p.rose.hsla())
                    .child(error.clone()),
            );
        }
        card = card.child(
            div()
                .h(px(1.))
                .mx(px(12.))
                .my(px(5.))
                .bg(p.border_strong.hsla()),
        );
        card = match &self.form {
            Some(form) => card.child(self.render_form(form, cx)),
            None => card.child(
                div()
                    .id("worktree-new")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .mx(px(6.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_color(p.text.hsla())
                    .hover(|s| s.bg(p.surface_selected.hsla()))
                    .child(icon(Icon::Plus, px(14.), p.text.hsla()))
                    .child("New worktree…")
                    .on_click(cx.listener(|this, _, window, cx| this.start_new(window, cx))),
            ),
        };
        deferred(div().absolute().left(px(0.)).top(px(36.)).child(card))
            .with_priority(3)
            .into_any_element()
    }

    fn render_row(
        &self,
        ix: usize,
        wt: &Worktree,
        current: bool,
        light: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let (badge, dot) = state_of(&wt.path, cx);
        let state: Option<(coqo_ui::Rgb, &str)> = match (badge, dot) {
            (Some(Badge::Waiting(_)), _) => Some((p.sand, "waiting")),
            (Some(Badge::Error), _) => Some((p.rose, "failed")),
            (None, Some(Dot::Working)) => Some((p.blue, "working")),
            (None, Some(Dot::Done)) => Some((p.sage, "done")),
            (None, None) => None,
        };
        // A chat of another worktree working here (a subagent's), on a
        // line of its own: "Claude working · from <chat>".
        let visiting = AgentActivity::global(cx)
            .and_then(|a| visitor(a.read(cx).sessions(), &wt.path))
            .map(|v| {
                let color = match v.state {
                    AgentState::Error => p.rose,
                    AgentState::Working => p.blue,
                    _ => p.sand,
                };
                (color, v.label)
            });
        let branch = wt.branch.clone().unwrap_or_else(|| match &wt.head {
            Some(oid) => format!("detached at {}", &oid[..oid.len().min(7)]),
            None => "no commits".into(),
        });
        let path = wt.path.clone();
        let remove_path = wt.path.clone();
        let removable = !wt.main && !wt.locked;
        let text = if light { p.panel_raised } else { p.ground };
        div()
            .id(("worktree-row", ix))
            .test_support()
            .flex()
            .items_center()
            .gap(px(8.))
            .mx(px(6.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(current, |d| d.bg(p.surface_selected.hsla()))
            .hover(|s| s.bg(p.surface_selected.hsla()))
            .on_click(cx.listener(move |this, _, _, cx| this.select(&path, cx)))
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .children(current.then(|| icon(Icon::Check, px(13.), p.sage.hsla()))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text.hsla())
                                    .child(folder_name(&wt.path)),
                            )
                            .when(wt.main, |d| {
                                d.child(
                                    div()
                                        .text_size(m.text_xs)
                                        .text_color(p.text_subtle.hsla())
                                        .child("main worktree"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(m.text_xs)
                            .text_color(p.text_subtle.hsla())
                            .child(icon(Icon::GitBranch, px(11.), p.text_subtle.hsla()))
                            .child(div().truncate().child(branch))
                            .children(state.map(|(color, label)| {
                                div()
                                    .id(("worktree-agent", ix))
                                    .test_support()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .child(div().size(px(7.)).rounded_full().bg(color.hsla()))
                                    .child(label)
                            })),
                    )
                    .children(visiting.map(|(color, label)| {
                        div()
                            .id(("worktree-visitor", ix))
                            .test_support()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .min_w_0()
                            .text_size(m.text_xs)
                            .text_color(p.text_muted.hsla())
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(11.))
                                    .flex()
                                    .justify_center()
                                    .child(div().size(px(7.)).rounded_full().bg(color.hsla())),
                            )
                            .child(div().min_w_0().truncate().child(label))
                    })),
            )
            .children(wt.changes.filter(|n| *n > 0).map(|n| {
                div()
                    .id(("worktree-changes", ix))
                    .test_support()
                    .min_w(px(18.))
                    .h(px(18.))
                    .px(px(5.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(9.))
                    .bg(p.sand.hsla())
                    .text_color(text.hsla())
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .child(n.to_string())
            }))
            .children(removable.then(|| {
                div()
                    .id(("worktree-remove", ix))
                    .test_support()
                    .p(px(3.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(p.surface.hsla()))
                    .child(icon(Icon::Trash, px(13.), p.text_muted.hsla()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.remove(&remove_path, false, cx);
                    }))
            }))
            .into_any_element()
    }

    fn render_removal(&self, removal: &PendingRemoval, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let dirty = removal.changes > 0;
        let message: SharedString = if dirty {
            format!(
                "{} has {} uncommitted {}. Commit or stash {} first: Coqo never forces a removal.",
                removal.name,
                removal.changes,
                if removal.changes == 1 {
                    "change"
                } else {
                    "changes"
                },
                if removal.changes == 1 { "it" } else { "them" },
            )
        } else {
            format!(
                "{} has {} {} no other branch has. Its branch stays, so {} kept.",
                removal.name,
                removal.unpushed,
                if removal.unpushed == 1 {
                    "commit"
                } else {
                    "commits"
                },
                if removal.unpushed == 1 {
                    "it is"
                } else {
                    "they are"
                },
            )
        }
        .into();
        let path = removal.path.clone();
        div()
            .id("worktree-removal")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .mx(px(10.))
            .my(px(4.))
            .p(px(10.))
            .rounded(px(10.))
            .bg(p.ground.mix(p.sand, 0.12).hsla())
            .border_1()
            .border_color(p.sand.alpha(0.4))
            .text_size(m.text_xs)
            .text_color(p.text.hsla())
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(icon(Icon::CircleAlert, px(14.), p.sand.hsla()))
                    .child(div().flex_1().child(message)),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(6.))
                    .child(
                        raised(div().px(px(10.)).py(px(4.)).rounded(px(8.)), cx)
                            .id("worktree-remove-cancel")
                            .test_support()
                            .cursor_pointer()
                            .child(if dirty { "OK" } else { "Keep it" })
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_removal(cx))),
                    )
                    .when(!dirty, |d| {
                        d.child(
                            div()
                                .id("worktree-remove-anyway")
                                .test_support()
                                .px(px(10.))
                                .py(px(4.))
                                .rounded(px(8.))
                                .cursor_pointer()
                                .bg(p.rose.hsla())
                                .text_color(p.ground.hsla())
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Remove anyway")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.remove(&path, true, cx)),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_form(&self, form: &NewForm, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let field = |label: &'static str, input: Entity<PromptInput>| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(m.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .child(label),
                )
                .child(
                    div()
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(m.radius_control)
                        .bg(p.panel_raised.hsla())
                        .border_1()
                        .border_color(p.border_strong.hsla())
                        .child(input),
                )
        };
        let dir = self
            .list
            .as_ref()
            .map(|l| format!("In {}", l.worktrees_dir));
        div()
            .id("worktree-new-form")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .py(px(6.))
            .child(
                div()
                    .text_size(m.text_label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_subtle.hsla())
                    .child("NEW WORKTREE"),
            )
            .child(field("Branch", form.branch.clone()))
            .child(field("Base", form.base.clone()))
            .children(dir.map(|dir| {
                div()
                    .text_size(m.text_xs)
                    .text_color(p.text_subtle.hsla())
                    .truncate()
                    .child(dir)
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(m.text_xs)
                    .text_color(p.text_subtle.hsla())
                    .child(div().flex_1())
                    .child(
                        primary(div().px(px(12.)).py(px(4.)).rounded(px(8.)), cx)
                            .id("worktree-create")
                            .test_support()
                            .cursor_pointer()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if self.busy { "Creating…" } else { "Create" })
                            .on_click(cx.listener(|this, _, _, cx| this.create(cx))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests;
