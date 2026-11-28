// SPDX-License-Identifier: Apache-2.0

//! The Git sidebar pane (board 1's "Git" tab): the branch and its picker,
//! a commit box, changed files grouped staged / unstaged (conflicts on
//! their own) with per-file stage and unstage, and the stash list.
//!
//! Clicking a file opens its diff tab (staged: `HEAD` → index; unstaged:
//! index → work tree). With several roots that are repositories, chips at
//! the top pick which one the pane shows. "Draft" beside Commit asks an
//! agent for the message ([`super::draft`]).
//!
//! A renamed file shows as `R old → new` (#226). Discard (the arrow on an
//! unstaged row, or "Discard all") asks first, in a card that says what
//! each file loses: tracked files go back to their index version, and
//! untracked ones go to the trash, which the note after it can undo.

use coqo_protocol::methods::{
    git::{self as gm, Branch, ChangedFile, FileChange, StatusEntry},
    workspace::RootId,
};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    dock::{AnyTab, Dock},
    gpui::{
        AnyElement, AnyView, App, AppContext as _, Context, Entity, FontWeight,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window, div,
        px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{
        FluentBuilder as _, Icon, TestSupportExt as _, icon, popover, primary, raised,
        section_label,
    },
};

use crate::{daemon::Daemon, files::editor::FileEditor};

use super::{
    diff::{self, DiffTab},
    draft::{CommitDrafter, DraftEvent, DraftRequest, DraftState},
    model::{GitModel, GitModelEvent, RootGit, StashAction},
};

const ROW_HEIGHT: f32 = 26.;

/// Which list a file row is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Staged,
    Unstaged,
    Conflicts,
}

impl Group {
    fn id(self) -> &'static str {
        match self {
            Group::Staged => "git-staged-row",
            Group::Unstaged => "git-unstaged-row",
            Group::Conflicts => "git-conflict-row",
        }
    }
}

/// A discard waiting for the user's yes (#226): what it would throw away.
#[derive(Debug)]
pub struct PendingDiscard {
    pub root: RootId,
    /// The rows to discard, as they were when asked.
    pub entries: Vec<StatusEntry>,
    /// Each tracked file's unstaged line counts (`git/diffFiles` index →
    /// work tree), once in.
    pub stats: Option<Vec<ChangedFile>>,
    /// Names of open tabs with unsaved edits to these files.
    pub dirty: Vec<String>,
    _load: Option<Task<()>>,
}

impl PendingDiscard {
    fn tracked(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.unstaged != Some(FileChange::Untracked))
            .count()
    }

    fn untracked(&self) -> usize {
        self.entries.len() - self.tracked()
    }
}

/// Whether the pane offers to discard `entry`'s unstaged change: a tracked
/// file's change, or an untracked file. An intent-to-add file, an unstaged
/// rename, a conflict or a nested repository is left to the user.
pub fn discardable(entry: &StatusEntry) -> bool {
    entry.conflict.is_none()
        && !entry.path.ends_with('/')
        && matches!(
            entry.unstaged,
            Some(
                FileChange::Modified
                    | FileChange::Deleted
                    | FileChange::TypeChanged
                    | FileChange::Untracked
            )
        )
}

/// The paths a whole-file stage or unstage of `entry` takes: a rename's
/// old path goes with it, so the pair moves together.
fn row_paths(entry: &StatusEntry, change: Option<FileChange>) -> Vec<String> {
    let mut paths = vec![entry.path.clone()];
    if change == Some(FileChange::Renamed)
        && let Some(orig) = &entry.orig_path
    {
        paths.push(orig.clone());
    }
    paths
}

/// The Git pane's view.
pub struct GitPane {
    dock: WeakEntity<Dock>,
    model: Entity<GitModel>,
    message: Entity<PromptInput>,
    /// Drafts the commit message with an agent (#224).
    drafter: Entity<CommitDrafter>,
    new_branch: Entity<PromptInput>,
    picker_open: bool,
    pending_discard: Option<PendingDiscard>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for GitPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitPane")
            .field("picker_open", &self.picker_open)
            .finish_non_exhaustive()
    }
}

/// A status letter's colour (board 1: `M` sand, `A` green).
fn letter_color(letter: char, cx: &App) -> Rgb {
    super::decorations::tone(letter).rgb(&cx.tokens().palette)
}

impl GitPane {
    pub fn new(dock: WeakEntity<Dock>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = GitModel::for_window(window, cx);
        let message = cx.new(|cx| PromptInput::new("Commit message", 2, 8, window, cx));
        let new_branch = cx.new(|cx| PromptInput::new("New branch name", 1, 1, window, cx));
        let drafter = cx.new(CommitDrafter::new);
        let subscriptions = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe(&drafter, |_, _, cx| cx.notify()),
            // The draft fills the commit box, to edit before committing.
            cx.subscribe_in(&drafter, window, |this, _, event, window, cx| {
                let DraftEvent::Drafted { root, message } = event;
                if Some(*root) == this.root(cx) {
                    this.message
                        .update(cx, |m, cx| m.set_text(message, window, cx));
                }
            }),
            cx.subscribe_in(&model, window, |this, _, event, window, cx| {
                if let GitModelEvent::Committed { root, .. } = event
                    && Some(*root) == this.root(cx)
                {
                    this.message.update(cx, |m, cx| m.set_text("", window, cx));
                }
            }),
            cx.subscribe(&message, |this, _, event, cx| {
                if *event == PromptInputEvent::Submit {
                    this.commit(cx);
                }
            }),
            cx.subscribe_in(
                &new_branch,
                window,
                |this, input, event, window, cx| match event {
                    PromptInputEvent::Submit => {
                        let name = input.read(cx).text(cx).trim().to_owned();
                        if let (false, Some(root)) = (name.is_empty(), this.root(cx)) {
                            this.model
                                .update(cx, |m, cx| m.create_branch(root, name, cx));
                            input.update(cx, |i, cx| i.set_text("", window, cx));
                            this.picker_open = false;
                            cx.notify();
                        }
                    }
                    PromptInputEvent::Cancel => {
                        this.picker_open = false;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];
        Self {
            dock,
            model,
            message,
            drafter,
            new_branch,
            picker_open: false,
            pending_discard: None,
            _subscriptions: subscriptions,
        }
    }

    /// The discard waiting for a yes, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pending_discard(&self) -> Option<&PendingDiscard> {
        self.pending_discard.as_ref()
    }

    /// The discard waiting for a yes, to fill in (screenshots).
    #[cfg(test)]
    pub fn pending_discard_mut(&mut self) -> Option<&mut PendingDiscard> {
        self.pending_discard.as_mut()
    }

    /// Ask before discarding `entries` of the shown root: the card says what
    /// each loses, from the unstaged line counts.
    pub fn request_discard(&mut self, entries: Vec<StatusEntry>, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else {
            return;
        };
        let entries: Vec<StatusEntry> = entries.into_iter().filter(discardable).collect();
        if entries.is_empty() {
            return;
        }
        let model = self.model.read(cx);
        let load = model
            .workspace()
            .map(|w| w.id.clone())
            .zip(Daemon::global(cx))
            .map(|(workspace, daemon)| {
                daemon
                    .read(cx)
                    .request::<gm::DiffFiles>(gm::DiffFilesParams {
                        workspace,
                        root,
                        base: gm::DiffSide::Index,
                        head: gm::DiffSide::Worktree,
                    })
            });
        let dirty = self.dirty_tabs(root, &entries, cx);
        let load = load.map(|call| {
            cx.spawn(async move |this, cx| {
                let result = call.await;
                this.update(cx, |this, cx| {
                    if let (Some(p), Ok(r)) = (this.pending_discard.as_mut(), result) {
                        p.stats = Some(r.files);
                        cx.notify();
                    }
                })
                .ok();
            })
        });
        self.pending_discard = Some(PendingDiscard {
            root,
            entries,
            stats: None,
            dirty,
            _load: load,
        });
        cx.notify();
    }

    /// The user said yes: discard what the card lists.
    pub fn confirm_discard(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.pending_discard.take() else {
            return;
        };
        let paths = p.entries.into_iter().map(|e| e.path).collect();
        self.model.update(cx, |m, cx| m.discard(p.root, paths, cx));
        cx.notify();
    }

    pub fn cancel_discard(&mut self, cx: &mut Context<Self>) {
        self.pending_discard = None;
        cx.notify();
    }

    /// Names of this window's file tabs with unsaved edits to `entries`.
    fn dirty_tabs(&self, root: RootId, entries: &[StatusEntry], cx: &App) -> Vec<String> {
        let (Some(dock), Some(model)) = (self.dock.upgrade(), Some(self.model.read(cx))) else {
            return Vec::new();
        };
        let (Some(workspace), Some(status)) = (
            model.workspace().map(|w| w.id.clone()),
            model.root(root).and_then(|r| r.status.as_ref()),
        ) else {
            return Vec::new();
        };
        let prefix = status.repo.prefix.clone();
        let git_path = |rel: &str| {
            if prefix.is_empty() {
                rel.to_owned()
            } else {
                format!("{prefix}/{rel}")
            }
        };
        let dock = dock.read(cx);
        let mut names: Vec<String> = dock
            .tree()
            .tabs()
            .into_iter()
            .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
            .filter_map(|tab| {
                let tab = tab.read(cx);
                let path = git_path(tab.path());
                (tab.workspace() == &workspace
                    && tab.root() == root
                    && tab.is_dirty()
                    && entries.iter().any(|e| e.path == path))
                .then(|| {
                    tab.path()
                        .rsplit('/')
                        .next()
                        .unwrap_or(tab.path())
                        .to_owned()
                })
            })
            .collect();
        names.dedup();
        names
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn model(&self) -> &Entity<GitModel> {
        &self.model
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn message_input(&self) -> &Entity<PromptInput> {
        &self.message
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn drafter(&self) -> &Entity<CommitDrafter> {
        &self.drafter
    }

    /// The root shown ([`GitModel::shown_root`]).
    pub fn root(&self, cx: &App) -> Option<RootId> {
        self.model.read(cx).shown_root()
    }

    /// The rows of `group` in the shown root, top to bottom.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rows(&self, group: Group, cx: &App) -> Vec<StatusEntry> {
        let Some(root) = self.root(cx).and_then(|r| self.model.read(cx).root(r)) else {
            return Vec::new();
        };
        rows(root, group).into_iter().cloned().collect()
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else {
            return;
        };
        let message = self.message.read(cx).text(cx);
        let staged = self
            .model
            .read(cx)
            .root(root)
            .is_some_and(|r| r.staged().next().is_some());
        if message.trim().is_empty() || !staged || self.model.read(cx).is_busy() {
            return;
        }
        self.model.update(cx, |m, cx| m.commit(root, message, cx));
    }

    fn open_diff(&mut self, entry: &StatusEntry, staged: bool, window: &mut Window, cx: &mut App) {
        let Some(dock) = self.dock.upgrade() else {
            return;
        };
        let (Some(root), Some(workspace)) = (
            self.root(cx),
            self.model.read(cx).workspace().map(|w| w.id.clone()),
        ) else {
            return;
        };
        let key = diff::key(&workspace, root, staged);
        let path = entry.path.clone();
        if let Some(open) = dock.read(cx).find_tab(diff::KIND, &key, cx) {
            let view = dock
                .read(cx)
                .tab(open)
                .and_then(|t| t.downcast::<DiffTab>())
                .map(|tab| tab.read(cx).view().clone());
            if let Some(view) = view {
                view.update(cx, |v, cx| v.select_file(&path, cx));
            }
            dock.update(cx, |dock, cx| dock.activate_tab(open, window, cx));
            return;
        }
        let tab: Entity<DiffTab> =
            cx.new(|cx| DiffTab::new(workspace, root, staged, Some(path), window, cx));
        dock.update(cx, |dock, cx| dock.open_tab(AnyTab::new(tab), window, cx));
    }

    fn render_roots(&self, shown: RootId, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = self.model.read(cx);
        let repos: Vec<_> = model
            .roots()
            .iter()
            .filter(|r| r.status.is_some())
            .map(|r| (r.root, r.name.clone()))
            .collect();
        if repos.len() < 2 {
            return None;
        }
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        Some(
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .px_2()
                .children(repos.into_iter().map(|(root, name)| {
                    let selected = root == shown;
                    let chip = div()
                        .px_2()
                        .py(px(3.))
                        .rounded(m.radius_control)
                        .text_size(m.text_xs);
                    let chip = if selected {
                        raised(chip, cx).text_color(p.text.hsla())
                    } else {
                        chip.text_color(p.text_muted.hsla())
                    };
                    chip.id(("git-root", root.0 as usize))
                        .test_support()
                        .cursor_pointer()
                        .child(name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.model.update(cx, |m, cx| m.select_root(root, cx));
                        }))
                }))
                .into_any_element(),
        )
    }

    fn render_branch(&self, repo: &RootGit, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let name = repo.head_label().unwrap_or_default();
        let track = repo.tracking_label();
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
        .id("git-branch")
        .test_support()
        .cursor_pointer()
        .child(icon(Icon::GitBranch, px(13.), p.text_muted.hsla()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text.hsla())
                .child(name),
        )
        .when_some(track, |d, track| {
            d.child(
                div()
                    .text_size(m.text_xs)
                    .text_color(p.text_subtle.hsla())
                    .child(track),
            )
        })
        .child(icon(
            if self.picker_open {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            px(12.),
            p.text_subtle.hsla(),
        ))
        .on_click(cx.listener(|this, _, _, cx| {
            this.picker_open = !this.picker_open;
            cx.notify();
        }));
        let picker = self.picker_open.then(|| {
            let root = repo.root;
            // Where HEAD is comes from the status, as on the button: the
            // list can be a switch behind, and clicking the branch HEAD
            // just left would then do nothing.
            let locals: Vec<_> = repo
                .branches
                .iter()
                .filter(|b| !b.remote)
                .map(|b| Branch {
                    current: repo.is_current(b),
                    ..b.clone()
                })
                .collect();
            div()
                .id("git-branch-picker")
                .test_support()
                .flex()
                .flex_col()
                .gap(px(2.))
                .p_1()
                .rounded(m.radius_control)
                .border_1()
                .border_color(p.border.hsla())
                .children(locals.into_iter().enumerate().map(|(ix, b)| {
                    let name = b.name.clone();
                    div()
                        .id(("git-branch-item", ix))
                        .test_support()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .h(px(ROW_HEIGHT))
                        .rounded(px(8.))
                        .text_size(m.text_sm)
                        .cursor_pointer()
                        .hover(|s| s.bg(p.surface.hsla()))
                        .child(
                            div().w(px(14.)).flex_none().children(
                                b.current.then(|| icon(Icon::Check, px(12.), p.sage.hsla())),
                            ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(if b.current { p.text } else { p.text_muted }.hsla())
                                .child(b.name.clone()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.picker_open = false;
                            if !b.current {
                                let name = name.clone();
                                this.model
                                    .update(cx, |m, cx| m.switch_branch(root, name, cx));
                            }
                            cx.notify();
                        }))
                }))
                .child(
                    div()
                        .id("git-new-branch")
                        .test_support()
                        .px_1()
                        .text_size(m.text_sm)
                        .child(self.new_branch.clone()),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .child(button)
            .children(picker)
            .into_any_element()
    }

    fn render_commit(&self, repo: &RootGit, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let staged = repo.staged().count();
        let has_message = !self.message.read(cx).text(cx).trim().is_empty();
        let busy = self.model.read(cx).is_busy();
        let enabled = staged > 0 && has_message && !busy;
        let label = match staged {
            0 => "Commit".to_owned(),
            1 => "Commit 1 file".to_owned(),
            n => format!("Commit {n} files"),
        };
        let button = div()
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .py(px(6.))
            .rounded(m.radius_control)
            .text_size(m.text_sm);
        let button = if enabled {
            primary(button, cx).cursor_pointer()
        } else {
            raised(button, cx).text_color(p.text_subtle.hsla())
        };
        let draft = self.render_draft(repo, cx);
        let draft_error = match self.drafter.read(cx).state() {
            DraftState::Failed(err) => Some(
                div()
                    .id("git-draft-error")
                    .test_support()
                    .px_1()
                    .text_size(m.text_xs)
                    .text_color(p.rose.hsla())
                    .cursor_pointer()
                    .child(format!("Couldn't draft a message: {err}"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.drafter.update(cx, |d, cx| d.cancel(cx));
                    })),
            ),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .child(
                div()
                    .id("git-message")
                    .test_support()
                    .px_2()
                    .py_1()
                    .rounded(m.radius_field)
                    .bg(p.surface.hsla())
                    .border_1()
                    .border_color(p.border.hsla())
                    .text_size(m.text_sm)
                    .child(self.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        button
                            .id("git-commit")
                            .test_support()
                            .flex_1()
                            .child(icon(
                                Icon::Check,
                                px(13.),
                                if enabled {
                                    p.ground.hsla()
                                } else {
                                    p.text_subtle.hsla()
                                },
                            ))
                            .child(label)
                            .on_click(cx.listener(|this, _, _, cx| this.commit(cx))),
                    )
                    .child(draft),
            )
            .children(draft_error)
            .into_any_element()
    }

    /// The "Draft" button beside Commit: asks an agent for a message
    /// (#224). While it drafts it reads "Drafting…" and a click stops it.
    fn render_draft(&self, repo: &RootGit, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let drafter = self.drafter.read(cx);
        let drafting = drafter.is_drafting();
        let unavailable = drafter.unavailable(repo.staged().count(), cx);
        let provider = drafter.provider().map(|p| p.name.clone());
        let base = raised(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .py(px(6.))
                .rounded(m.radius_control)
                .text_size(m.text_sm),
            cx,
        )
        .id("git-draft")
        .test_support();
        let (tip, button): (SharedString, _) = if drafting {
            (
                "Stop drafting".into(),
                base.cursor_pointer()
                    .text_color(p.sand.hsla())
                    .child(icon(Icon::LoaderCircle, px(13.), p.sand.hsla()))
                    .child("Drafting…")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.drafter.update(cx, |d, cx| d.cancel(cx));
                    })),
            )
        } else if let Some(why) = unavailable {
            (
                why.into(),
                base.text_color(p.text_subtle.hsla())
                    .child(icon(Icon::Sparkles, px(13.), p.text_subtle.hsla()))
                    .child("Draft"),
            )
        } else {
            (
                format!(
                    "Ask {} to draft a message from the staged changes",
                    provider.as_deref().unwrap_or("the agent")
                )
                .into(),
                base.cursor_pointer()
                    .text_color(p.text.hsla())
                    .child(icon(Icon::Sparkles, px(13.), p.text_muted.hsla()))
                    .child("Draft")
                    .on_click(cx.listener(|this, _, _, cx| this.draft(cx))),
            )
        };
        button
            .tooltip(move |_, cx| tooltip(tip.clone(), cx))
            .into_any_element()
    }

    /// Ask the agent for a message for what the shown root has staged.
    pub fn draft(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else {
            return;
        };
        let model = self.model.read(cx);
        let staged = model.root(root).map_or(0, |r| r.staged().count());
        if !self.drafter.read(cx).can_start(staged, cx) {
            return;
        }
        let Some(workspace) = model.workspace() else {
            return;
        };
        let Some(cwd) = workspace
            .roots
            .iter()
            .find(|r| r.id == root)
            .map(|r| r.path.clone())
        else {
            return;
        };
        let request = DraftRequest {
            workspace: workspace.id.clone(),
            root,
            cwd,
        };
        self.drafter.update(cx, |d, cx| d.start(request, cx));
    }

    fn render_group(
        &self,
        repo: &RootGit,
        group: Group,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let entries: Vec<StatusEntry> = rows(repo, group).into_iter().cloned().collect();
        if entries.is_empty() && group == Group::Conflicts {
            return None;
        }
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let root = repo.root;
        let title = match group {
            Group::Staged => "Staged",
            Group::Unstaged => "Changes",
            Group::Conflicts => "Conflicts",
        };
        let paths: Vec<String> = entries
            .iter()
            .flat_map(|e| {
                row_paths(
                    e,
                    if group == Group::Staged {
                        e.staged
                    } else {
                        e.unstaged
                    },
                )
            })
            .collect();
        let discardable: Vec<StatusEntry> = entries
            .iter()
            .filter(|e| group == Group::Unstaged && discardable(e))
            .cloned()
            .collect();
        let discard_all = (!discardable.is_empty()).then(|| {
            div()
                .id("git-discard-all")
                .test_support()
                .px_2()
                .text_size(m.text_xs)
                .text_color(p.text_subtle.hsla())
                .cursor_pointer()
                .hover(|s| s.text_color(p.rose.hsla()))
                .child("Discard all")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.request_discard(discardable.clone(), cx);
                }))
        });
        let all = (!entries.is_empty()).then(|| {
            let (id, text, stage) = match group {
                Group::Staged => ("git-unstage-all", "Unstage all", false),
                _ => ("git-stage-all", "Stage all", true),
            };
            div()
                .id(id)
                .test_support()
                .px_2()
                .text_size(m.text_xs)
                .text_color(p.text_subtle.hsla())
                .cursor_pointer()
                .hover(|s| s.text_color(p.text.hsla()))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let paths = paths.clone();
                    this.model.update(cx, |m, cx| {
                        if stage {
                            m.stage(root, paths, cx);
                        } else {
                            m.unstage(root, paths, cx);
                        }
                    });
                }))
        });
        let header = div()
            .flex()
            .items_center()
            .child(section_label(format!("{title} · {}", entries.len()), cx))
            .child(div().flex_1())
            .children(discard_all)
            .children(all);
        let confirm = self
            .pending_discard
            .as_ref()
            .filter(|p| group == Group::Unstaged && p.root == root)
            .map(|pending| self.render_discard(pending, cx));
        let rows = entries
            .into_iter()
            .enumerate()
            .map(|(ix, entry)| self.render_row(group, ix, entry, cx));
        let empty = (group != Group::Conflicts).then(|| {
            div()
                .px_2()
                .text_size(m.text_xs)
                .text_color(p.text_subtle.hsla())
                .child(match group {
                    Group::Staged => "Nothing staged",
                    _ => "No changes",
                })
        });
        let rows: Vec<AnyElement> = rows.collect();
        Some(
            div()
                .flex()
                .flex_col()
                .child(header)
                .children(confirm)
                .when(rows.is_empty(), |d| d.children(empty))
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_row(
        &self,
        group: Group,
        ix: usize,
        entry: StatusEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let change = match group {
            Group::Staged => entry.staged,
            _ => entry.unstaged,
        };
        let letter = if group == Group::Conflicts {
            'U'
        } else {
            change.map_or(' ', FileChange::letter)
        };
        let trimmed = entry.path.trim_end_matches('/');
        let (dir, name) = match trimmed.rsplit_once('/') {
            Some((d, n)) => (Some(d.to_owned()), n.to_owned()),
            None => (None, trimmed.to_owned()),
        };
        let name = if entry.path.ends_with('/') {
            format!("{name}/")
        } else {
            name
        };
        // `old → new`: the old name if it stayed in its folder, else the
        // old path.
        let name = match (&entry.orig_path, change) {
            (Some(orig), Some(FileChange::Renamed)) => {
                let (orig_dir, orig_name) = orig.rsplit_once('/').unwrap_or(("", orig));
                let from = if Some(orig_dir) == dir.as_deref().or(Some("")) {
                    orig_name
                } else {
                    orig
                };
                format!("{from} → {name}")
            }
            _ => name,
        };
        let staged = group == Group::Staged;
        let root = self.root(cx);
        let (button_id, button_icon_text) = match group {
            Group::Staged => ("git-unstage", "−"),
            _ => ("git-stage", "+"),
        };
        let paths = row_paths(&entry, change);
        let discard = (group == Group::Unstaged && discardable(&entry)).then(|| {
            let entry = entry.clone();
            div()
                .id(("git-discard", ix))
                .test_support()
                .size(px(20.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(p.surface_strong.hsla()))
                .tooltip(|_, cx| tooltip("Discard changes…".into(), cx))
                .child(icon(Icon::Undo, px(13.), p.text_muted.hsla()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.request_discard(vec![entry.clone()], cx);
                }))
        });
        let action = div()
            .id((button_id, ix))
            .test_support()
            .size(px(20.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .font_family(m.font_mono.clone())
            .text_size(m.text_sm)
            .text_color(p.text_muted.hsla())
            .cursor_pointer()
            .hover(|s| s.bg(p.surface_strong.hsla()).text_color(p.text.hsla()))
            .child(button_icon_text)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let Some(root) = root else { return };
                let paths = paths.clone();
                this.model.update(cx, |m, cx| {
                    if staged {
                        m.unstage(root, paths, cx);
                    } else {
                        m.stage(root, paths, cx);
                    }
                });
            }));
        let color = letter_color(letter, cx);
        div()
            .id((group.id(), ix))
            .test_support()
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(8.))
            .pr(px(4.))
            .rounded(px(9.))
            .text_size(m.text_sm)
            .cursor_pointer()
            .hover(|s| s.bg(p.surface.hsla()))
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .font_family(m.font_mono.clone())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color.hsla())
                    .child(letter.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().flex_none().text_color(p.text.hsla()).child(name))
                    .children(dir.map(|d| {
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(m.text_xs)
                            .text_color(p.text_subtle.hsla())
                            .child(d)
                    })),
            )
            .children(discard)
            .child(action)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_diff(&entry, staged, window, cx);
            }))
            .into_any_element()
    }

    fn render_stashes(&self, repo: &RootGit, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let root = repo.root;
        let push = div()
            .id("git-stash-push")
            .test_support()
            .px_2()
            .text_size(m.text_xs)
            .text_color(p.text_subtle.hsla())
            .cursor_pointer()
            .hover(|s| s.text_color(p.text.hsla()))
            .child("Stash changes")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.update(cx, |m, cx| m.stash_push(root, cx));
            }));
        let action = |id: &'static str,
                      ix: usize,
                      text: &'static str,
                      what: StashAction,
                      index: u32,
                      cx: &mut Context<Self>| {
            div()
                .id((id, ix))
                .test_support()
                .px(px(6.))
                .rounded(px(6.))
                .text_size(m.text_xs)
                .text_color(p.text_subtle.hsla())
                .cursor_pointer()
                .hover(|s| s.bg(p.surface_strong.hsla()).text_color(p.text.hsla()))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.model
                        .update(cx, |m, cx| m.stash(root, index, what, cx));
                }))
        };
        let rows: Vec<AnyElement> = repo
            .stashes
            .iter()
            .enumerate()
            .map(|(ix, s)| {
                div()
                    .id(("git-stash", ix))
                    .test_support()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(8.))
                    .pr(px(4.))
                    .text_size(m.text_sm)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(p.text_muted.hsla())
                            .child(s.message.clone()),
                    )
                    .child(action(
                        "git-stash-apply",
                        ix,
                        "Apply",
                        StashAction::Apply,
                        s.index,
                        cx,
                    ))
                    .child(action(
                        "git-stash-pop",
                        ix,
                        "Pop",
                        StashAction::Pop,
                        s.index,
                        cx,
                    ))
                    .child(action(
                        "git-stash-drop",
                        ix,
                        "Drop",
                        StashAction::Drop,
                        s.index,
                        cx,
                    ))
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(section_label(
                        format!("Stashes · {}", repo.stashes.len()),
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(push),
            )
            .children(rows)
            .into_any_element()
    }

    /// The card asking before a discard: each file and what it loses.
    fn render_discard(&self, pending: &PendingDiscard, cx: &mut Context<Self>) -> AnyElement {
        const SHOWN: usize = 6;
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let base = |path: &str| {
            let path = path.trim_end_matches('/');
            path.rsplit('/').next().unwrap_or(path).to_owned()
        };
        let (tracked, untracked) = (pending.tracked(), pending.untracked());
        let title = match (pending.entries.as_slice(), tracked, untracked) {
            ([one], 0, _) => format!("Move “{}” to the trash?", base(&one.path)),
            ([one], _, _) => format!("Discard the changes to “{}”?", base(&one.path)),
            (_, 0, n) => format!("Move {n} untracked files to the trash?"),
            (all, _, _) => format!("Discard the changes to {} files?", all.len()),
        };
        let mut detail = Vec::new();
        if tracked > 0 {
            detail.push(if tracked == 1 {
                "The file goes back to its staged version (or the last commit's). This can't be undone."
            } else {
                "Tracked files go back to their staged version (or the last commit's). This can't be undone."
            });
        }
        if untracked > 0 {
            detail.push(if untracked == 1 && tracked == 0 {
                "It isn't tracked: it moves to the trash, and Undo brings it back."
            } else {
                "Untracked files move to the trash, and Undo brings them back."
            });
        }
        let lost = |e: &StatusEntry| -> String {
            match e.unstaged {
                Some(FileChange::Untracked) => "untracked".into(),
                Some(FileChange::Deleted) => "brings it back".into(),
                _ => match pending
                    .stats
                    .as_ref()
                    .and_then(|s| s.iter().find(|f| f.path == e.path))
                {
                    Some(f) if f.binary => "binary change".into(),
                    Some(f) => format!("+{} −{}", f.additions, f.deletions),
                    None if pending.stats.is_some() => "changed".into(),
                    None => "…".into(),
                },
            }
        };
        let files: Vec<AnyElement> = pending
            .entries
            .iter()
            .take(SHOWN)
            .enumerate()
            .map(|(ix, e)| {
                div()
                    .id(("git-discard-file", ix))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(m.text_xs)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_family(m.font_mono.clone())
                            .text_color(p.text.hsla())
                            .child(e.path.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(m.font_mono.clone())
                            .text_color(p.text_subtle.hsla())
                            .child(lost(e)),
                    )
                    .into_any_element()
            })
            .collect();
        let more = pending.entries.len().saturating_sub(SHOWN);
        let unsaved = (!pending.dirty.is_empty()).then(|| {
            format!(
                "Unsaved edits in {} stay in the open tab; saving writes them again.",
                pending.dirty.join(", ")
            )
        });
        let button = |id: &'static str, label: &'static str, fill: Option<Rgb>, cx: &App| {
            let el = div()
                .px(px(12.))
                .py(px(5.))
                .rounded(m.radius_control)
                .text_size(m.text_sm)
                .font_weight(FontWeight::SEMIBOLD);
            let el = match fill {
                Some(fill) => el
                    .bg(fill.alpha(0.18))
                    .border_1()
                    .border_color(fill.alpha(0.45))
                    .text_color(fill.hsla()),
                None => raised(el, cx).text_color(p.text.hsla()),
            };
            el.id(id).test_support().cursor_pointer().child(label)
        };
        let confirm = if tracked == 0 {
            "Move to Trash"
        } else {
            "Discard"
        };
        let busy = self.model.read(cx).is_busy();
        div()
            .id("git-discard-confirm")
            .test_support()
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.))
            .mx(px(2.))
            .my(px(4.))
            .p(px(12.))
            .rounded(m.radius_field)
            .bg(p.surface.hsla())
            .border_1()
            .border_color(p.border_strong.hsla())
            .text_size(m.text_sm)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(icon(
                        if tracked == 0 {
                            Icon::Trash
                        } else {
                            Icon::Undo
                        },
                        px(14.),
                        p.rose.hsla(),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child(title),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .children(files)
                    .when(more > 0, |d| {
                        d.child(
                            div()
                                .text_size(m.text_xs)
                                .text_color(p.text_subtle.hsla())
                                .child(format!("and {more} more")),
                        )
                    }),
            )
            .child(
                div()
                    .id("git-discard-detail")
                    .test_support()
                    .text_size(m.text_xs)
                    .text_color(p.text_muted.hsla())
                    .child(detail.join(" ")),
            )
            .children(unsaved.map(|text| {
                div()
                    .id("git-discard-unsaved")
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .text_size(m.text_xs)
                    .text_color(p.text_muted.hsla())
                    .child(icon(Icon::CircleAlert, px(12.), p.sand.hsla()))
                    .child(div().flex_1().min_w_0().child(text))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(6.))
                    .child(
                        button("git-discard-cancel", "Cancel", None, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_discard(cx))),
                    )
                    .child(
                        button("git-discard-ok", confirm, Some(p.rose), cx)
                            .when(busy, |b| b.opacity(0.6))
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_discard(cx))),
                    ),
            )
            .into_any_element()
    }

    /// What the last discard did, with Undo while its trash moves can be
    /// undone.
    fn render_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = self.model.read(cx);
        let notice = model.notice()?.clone();
        let undo = model.can_undo_discard(cx);
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        Some(
            div()
                .id("git-discard-notice")
                .test_support()
                .flex()
                .items_center()
                .gap(px(8.))
                .mx_2()
                .px(px(10.))
                .py(px(6.))
                .rounded(m.radius_field)
                .bg(p.surface.hsla())
                .text_size(m.text_xs)
                .text_color(p.text.hsla())
                .child(icon(Icon::Check, px(13.), p.green.hsla()))
                .child(div().flex_1().min_w_0().child(notice.text))
                .when(undo, |d| {
                    d.child(
                        div()
                            .id("git-discard-undo")
                            .test_support()
                            .px_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.sage.hsla())
                            .cursor_pointer()
                            .child("Undo")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |m, cx| m.undo_discard(cx));
                            })),
                    )
                })
                .child(
                    div()
                        .id("git-discard-notice-close")
                        .test_support()
                        .cursor_pointer()
                        .child(icon(Icon::X, px(12.), p.text_subtle.hsla()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |m, cx| m.dismiss_notice(cx));
                        })),
                )
                .into_any_element(),
        )
    }

    fn note(text: impl Into<SharedString>, cx: &App) -> AnyElement {
        let t = cx.tokens();
        div()
            .px_2()
            .py_1()
            .text_size(t.metrics.text_sm)
            .text_color(t.palette.text_subtle.hsla())
            .child(text.into())
            .into_any_element()
    }
}

fn tooltip(text: SharedString, cx: &mut App) -> AnyView {
    cx.new(|_| Tip(text)).into()
}

struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        popover(cx)
            .px(px(10.))
            .py(px(6.))
            .max_w(px(320.))
            .text_size(t.metrics.text_sm)
            .text_color(t.palette.text.hsla())
            .font_family(t.metrics.font_ui.clone())
            .child(self.0.clone())
    }
}

fn rows(repo: &RootGit, group: Group) -> Vec<&StatusEntry> {
    match group {
        Group::Staged => repo.staged().collect(),
        Group::Unstaged => repo.unstaged().collect(),
        Group::Conflicts => repo.conflicts().collect(),
    }
}

impl Render for GitPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let root = self.root(cx);
        let repo = root.and_then(|r| model.root(r)).cloned();
        let error = model.error().map(ToOwned::to_owned);
        let loading = root.is_some_and(|r| model.is_loading(r));
        let has_workspace = model.workspace().is_some();
        let t = cx.tokens();
        let p = t.palette.clone();
        let m = t.metrics.clone();
        let mut pane = div()
            .id("sidebar-git-pane")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll();
        let body: Vec<AnyElement> = match (&repo, has_workspace) {
            (_, false) => vec![Self::note("No folder open", cx)],
            (None, true) => vec![Self::note("No repository", cx)],
            (Some(_), _) if loading => vec![Self::note("Loading…", cx)],
            (Some(r), _) if r.status.is_none() => vec![Self::note(
                r.unavailable
                    .clone()
                    .unwrap_or_else(|| "No repository".into()),
                cx,
            )],
            (Some(r), _) => {
                let mut parts = Vec::new();
                parts.extend(self.render_roots(r.root, cx));
                parts.push(self.render_branch(r, cx));
                parts.push(self.render_commit(r, cx));
                parts.extend(self.render_group(r, Group::Conflicts, cx));
                parts.extend(self.render_group(r, Group::Staged, cx));
                parts.extend(self.render_group(r, Group::Unstaged, cx));
                parts.push(self.render_stashes(r, cx));
                parts
            }
        };
        if let Some(err) = error {
            pane = pane.child(
                div()
                    .id("git-error")
                    .test_support()
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded(m.radius_control)
                    .bg(p.rose.alpha(0.12))
                    .text_size(m.text_xs)
                    .text_color(p.rose.hsla())
                    .cursor_pointer()
                    .child(err)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model.update(cx, |m, cx| m.dismiss_error(cx));
                    })),
            );
        }
        if let Some(notice) = self.render_notice(cx) {
            pane = pane.child(notice);
        }
        pane.children(body)
    }
}
