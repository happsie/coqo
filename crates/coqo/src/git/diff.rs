// SPDX-License-Identifier: Apache-2.0

//! The `git-diff` tab kind: one file's unified diff, staged (`HEAD` →
//! index) or unstaged (index → work tree), from `git/diff`.
//!
//! Deliberately minimal and read-only, with board 4's colours and type; the
//! full diff component (side by side, hunk and line staging, comments) is
//! #93 and replaces the body of this tab.

use coqo_protocol::methods::{
    git::{self as gm, DiffSide, FileDiff, GitChangeKind, LineKind},
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    ActiveTokens as _,
    dock::{AnyTab, TabItem},
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FocusHandle, FontWeight,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px,
    },
    serde_json::{Value, json},
    widgets::{Icon, TestSupportExt as _, icon},
};

use super::model::{GitModel, GitModelEvent};
use crate::daemon::{Daemon, DaemonError};

/// The tab kind. Persisted: never change it.
pub const KIND: &str = "git-diff";

const LINE_HEIGHT: f32 = 23.;
const NUMBER_WIDTH: f32 = 40.;

/// The de-duplication key of a diff tab.
pub fn key(workspace: &WorkspaceId, root: RootId, path: &str, staged: bool) -> String {
    let side = if staged { "staged" } else { "unstaged" };
    format!("{workspace}/{root}/{side}/{path}")
}

/// Recreate a saved diff tab.
pub fn restore(state: &Value, window: &mut Window, cx: &mut App) -> Option<AnyTab> {
    let workspace = WorkspaceId(state.get("workspace")?.as_str()?.to_owned());
    let root = RootId(u32::try_from(state.get("root")?.as_u64()?).ok()?);
    let path = state.get("path")?.as_str()?.to_owned();
    let staged = state.get("staged")?.as_bool()?;
    Some(AnyTab::new(cx.new(|cx| {
        DiffTab::new(workspace, root, path, staged, window, cx)
    })))
}

/// What the tab shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Loading,
    Diff(FileDiff),
    Failed(String),
}

/// One file's diff.
pub struct DiffTab {
    workspace: WorkspaceId,
    root: RootId,
    /// Repository-relative.
    path: String,
    staged: bool,
    content: Content,
    focus: FocusHandle,
    _model: Entity<GitModel>,
    _loading: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for DiffTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffTab")
            .field("root", &self.root)
            .field("path", &self.path)
            .field("staged", &self.staged)
            .finish_non_exhaustive()
    }
}

impl DiffTab {
    pub fn new(
        workspace: WorkspaceId,
        root: RootId,
        path: String,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = GitModel::for_window(window, cx);
        let subscriptions = vec![cx.subscribe(&model, |this: &mut Self, _, event, cx| {
            if let GitModelEvent::Changed { root, kinds } = event {
                use GitChangeKind as K;
                let ours = if this.staged {
                    [K::Index, K::Head].as_slice()
                } else {
                    [K::Index, K::Worktree].as_slice()
                };
                if *root == this.root && kinds.iter().any(|k| ours.contains(k)) {
                    this.load(cx);
                }
            }
        })];
        let mut this = Self {
            workspace,
            root,
            path,
            staged,
            content: Content::Loading,
            focus: cx.focus_handle(),
            _model: model,
            _loading: None,
            _subscriptions: subscriptions,
        };
        this.load(cx);
        this
    }

    /// A tab showing `diff`, without a daemon (screenshots).
    #[cfg(test)]
    pub fn with_diff(
        workspace: WorkspaceId,
        root: RootId,
        diff: FileDiff,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(workspace, root, diff.path.clone(), staged, window, cx);
        this.content = Content::Diff(diff);
        this
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn content(&self) -> &Content {
        &self.content
    }

    fn file_name(&self) -> &str {
        self.path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.path)
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let (base, head) = if self.staged {
            (DiffSide::head(), DiffSide::Index)
        } else {
            (DiffSide::Index, DiffSide::Worktree)
        };
        let call = daemon.read(cx).request::<gm::Diff>(gm::DiffParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: self.path.clone(),
            base,
            head,
            context: None,
        });
        self._loading = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(diff) => this.content = Content::Diff(diff),
                    Err(DaemonError::NotConnected) => {}
                    Err(err) => this.content = Content::Failed(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_diff(&self, diff: &FileDiff, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let message = |text: &str| {
            div()
                .id("git-diff-message")
                .test_support()
                .p_4()
                .text_size(m.text_sm)
                .text_color(p.text_subtle.hsla())
                .child(text.to_owned())
                .into_any_element()
        };
        if diff.binary {
            return message("Binary file differs");
        }
        if diff.hunks.is_empty() {
            return message("No changes");
        }
        let number = |n: Option<u32>| {
            div()
                .w(px(NUMBER_WIDTH))
                .flex_none()
                .pr_2()
                .flex()
                .justify_end()
                .text_color(p.text_subtle.hsla())
                .children(n.map(|n| n.to_string()))
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for (h, hunk) in diff.hunks.iter().enumerate() {
            let count = |start: u32, lines: u32| {
                if lines == 1 {
                    start.to_string()
                } else {
                    format!("{start},{lines}")
                }
            };
            let mut header = format!(
                "@@ -{} +{} @@",
                count(hunk.old_start, hunk.old_lines),
                count(hunk.new_start, hunk.new_lines)
            );
            if let Some(section) = &hunk.section {
                header.push(' ');
                header.push_str(section);
            }
            rows.push(
                div()
                    .id(("git-diff-hunk", h))
                    .test_support()
                    .h(px(LINE_HEIGHT))
                    .flex()
                    .items_center()
                    .pl(px(NUMBER_WIDTH * 2. + 8.))
                    .bg(p.blue.alpha(0.08))
                    .text_color(p.blue.hsla())
                    .child(header)
                    .into_any_element(),
            );
            for line in &hunk.lines {
                let (bg, sign, color) = match line.kind {
                    LineKind::Added => (Some(p.green.alpha(0.11)), "+", p.green),
                    LineKind::Removed => (Some(p.rose.alpha(0.12)), "-", p.rose),
                    LineKind::Context => (None, " ", p.text_muted),
                };
                let mut row = div()
                    .h(px(LINE_HEIGHT))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .child(number(line.old_line))
                    .child(number(line.new_line))
                    .child(
                        div()
                            .w(px(16.))
                            .flex_none()
                            .text_color(color.hsla())
                            .child(sign),
                    )
                    .child(div().text_color(p.text.hsla()).child(line.text.clone()));
                if let Some(bg) = bg {
                    row = row.bg(bg);
                }
                if !line.newline {
                    row = row.child(
                        div()
                            .pl_2()
                            .text_color(p.text_subtle.hsla())
                            .child("⏎̸ no newline at end of file"),
                    );
                }
                rows.push(row.into_any_element());
            }
        }
        div()
            .id("git-diff-lines")
            .test_support()
            .flex()
            .flex_col()
            .py_2()
            .font_family(m.font_mono.clone())
            .text_size(px(12.5))
            .children(rows)
            .into_any_element()
    }
}

impl TabItem for DiffTab {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        self.file_name().to_owned().into()
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(key(&self.workspace, self.root, &self.path, self.staged).into())
    }

    fn save(&self, _: &App) -> Value {
        json!({
            "workspace": self.workspace.0,
            "root": self.root.0,
            "path": self.path,
            "staged": self.staged,
        })
    }

    fn render_label(&self, _: bool, _: &mut Window, cx: &App) -> Option<AnyElement> {
        let p = &cx.tokens().palette;
        let side = if self.staged { "staged" } else { "changes" };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(icon(Icon::GitBranch, px(12.), p.text_muted.hsla()))
                .child(self.file_name().to_owned())
                .child(
                    div()
                        .text_size(cx.tokens().metrics.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .child(side),
                )
                .into_any_element(),
        )
    }

    fn focus_handle(&self, _: &App) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }
}

impl Render for DiffTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let body = match &self.content {
            Content::Loading => div()
                .p_4()
                .text_size(m.text_sm)
                .text_color(p.text_subtle.hsla())
                .child("Loading…")
                .into_any_element(),
            Content::Failed(err) => div()
                .id("git-diff-error")
                .test_support()
                .p_4()
                .text_size(m.text_sm)
                .text_color(p.rose.hsla())
                .child(err.clone())
                .into_any_element(),
            Content::Diff(diff) => self.render_diff(diff, cx),
        };
        let stats = match &self.content {
            Content::Diff(d) if !d.binary => Some((d.additions, d.deletions)),
            _ => None,
        };
        div()
            .id("git-diff")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(p.border.hsla())
                    .text_size(m.text_sm)
                    .child(
                        div()
                            .font_family(m.font_mono.clone())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child(self.path.clone()),
                    )
                    .child(
                        div()
                            .text_color(p.text_subtle.hsla())
                            .child(if self.staged {
                                "HEAD → index"
                            } else {
                                "index → working tree"
                            }),
                    )
                    .child(div().flex_1())
                    .children(stats.map(|(a, d)| {
                        div()
                            .flex()
                            .gap_2()
                            .font_family(m.font_mono.clone())
                            .child(div().text_color(p.green.hsla()).child(format!("+{a}")))
                            .child(div().text_color(p.rose.hsla()).child(format!("−{d}")))
                    })),
            )
            .child(
                div()
                    .id("git-diff-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .child(body),
            )
    }
}
