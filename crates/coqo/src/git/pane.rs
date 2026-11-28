// SPDX-License-Identifier: Apache-2.0

//! The Git sidebar pane (board 1's "Git" tab): the branch and its picker,
//! a commit box, changed files grouped staged / unstaged (conflicts on
//! their own) with per-file stage and unstage, and the stash list.
//!
//! Clicking a file opens its diff tab (staged: `HEAD` → index; unstaged:
//! index → work tree). With several roots that are repositories, chips at
//! the top pick which one the pane shows. "Draft" beside Commit asks an
//! agent for the message ([`super::draft`]).

use coqo_protocol::methods::{
    git::{Branch, FileChange, StatusEntry},
    workspace::RootId,
};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    dock::{AnyTab, Dock},
    gpui::{
        AnyElement, AnyView, App, AppContext as _, Context, Entity, FontWeight,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div, px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{
        FluentBuilder as _, Icon, TestSupportExt as _, icon, popover, primary, raised,
        section_label,
    },
};

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

/// The Git pane's view.
pub struct GitPane {
    dock: WeakEntity<Dock>,
    model: Entity<GitModel>,
    message: Entity<PromptInput>,
    /// Drafts the commit message with an agent (#224).
    drafter: Entity<CommitDrafter>,
    new_branch: Entity<PromptInput>,
    picker_open: bool,
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
    let p = &cx.tokens().palette;
    match letter {
        'M' => p.sand,
        'A' => p.green,
        'D' | 'U' => p.rose,
        'T' => p.lavender,
        _ => p.text_subtle,
    }
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
            _subscriptions: subscriptions,
        }
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
        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
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
            .children(all);
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
        let staged = group == Group::Staged;
        let root = self.root(cx);
        let (button_id, button_icon_text) = match group {
            Group::Staged => ("git-unstage", "−"),
            _ => ("git-stage", "+"),
        };
        let path = entry.path.clone();
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
                let paths = vec![path.clone()];
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
        pane.children(body)
    }
}
