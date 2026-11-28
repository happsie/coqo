// SPDX-License-Identifier: Apache-2.0

//! The git chrome outside the pane (board 1, #227): the count of changed
//! files on the sidebar's "Git" tab, in `sand`, and the branch in the status
//! bar.
//!
//! Both describe the root the Git pane shows ([`GitModel::shown_root`]).
//! Each is a small view kept per window over the window's [`GitModel`], so
//! it re-renders by itself when `git/changes` lands, with no help from the
//! shell.

use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, AnyWindowHandle, App, AppContext as _, Context, Empty, Entity, Global,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _,
        Subscription, Window, div, px,
    },
    widgets::{Icon, TestSupportExt as _, icon},
};

use super::model::GitModel;

/// What the status bar shows without a repository.
pub const NO_REPOSITORY: &str = "No repository";

/// Which piece of chrome a [`GitChrome`] view draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromePart {
    /// The count after "Git" in the sidebar's tab strip.
    Badge,
    /// The branch in the status bar.
    Branch,
}

/// A piece of git chrome in one window.
pub struct GitChrome {
    model: Entity<GitModel>,
    part: ChromePart,
    _observe: Subscription,
}

impl std::fmt::Debug for GitChrome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitChrome")
            .field("part", &self.part)
            .finish_non_exhaustive()
    }
}

/// Each window's chrome views: the badge, then the branch.
#[derive(Default)]
struct ChromeViews(Vec<(AnyWindowHandle, Entity<GitChrome>, Entity<GitChrome>)>);

impl Global for ChromeViews {}

impl GitChrome {
    fn new(model: Entity<GitModel>, part: ChromePart, cx: &mut Context<Self>) -> Self {
        Self {
            _observe: cx.observe(&model, |_, _, cx| cx.notify()),
            model,
            part,
        }
    }

    /// `window`'s view of `part`, made on first use. The views hold the
    /// window's [`GitModel`]; they go when the window does.
    pub fn for_window(part: ChromePart, window: &mut Window, cx: &mut App) -> Entity<GitChrome> {
        let handle = window.window_handle();
        // The window being drawn is not in `cx.windows()` while it draws.
        let open = cx.windows();
        let views = cx.default_global::<ChromeViews>();
        views.0.retain(|(h, ..)| *h == handle || open.contains(h));
        let found = views
            .0
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|(_, b, r)| match part {
                ChromePart::Badge => b.clone(),
                ChromePart::Branch => r.clone(),
            });
        if let Some(view) = found {
            return view;
        }
        let model = GitModel::for_window(window, cx);
        let badge = cx.new(|cx| GitChrome::new(model.clone(), ChromePart::Badge, cx));
        let branch = cx.new(|cx| GitChrome::new(model, ChromePart::Branch, cx));
        cx.default_global::<ChromeViews>()
            .0
            .push((handle, badge.clone(), branch.clone()));
        match part {
            ChromePart::Badge => badge,
            ChromePart::Branch => branch,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn model(&self) -> &Entity<GitModel> {
        &self.model
    }

    /// The badge's number: the shown root's changed files, `None` when
    /// there are none (or no status yet).
    pub fn changed_files(&self, cx: &App) -> Option<usize> {
        let model = self.model.read(cx);
        let root = model.root(model.shown_root()?)?;
        root.status.as_ref()?;
        Some(root.changed_files()).filter(|n| *n > 0)
    }

    /// The status bar's text: the branch (or detached `HEAD`) with `↑n ↓m`
    /// against the upstream, or [`NO_REPOSITORY`].
    pub fn branch_text(&self, cx: &App) -> String {
        let model = self.model.read(cx);
        let Some(root) = model.shown_root().and_then(|r| model.root(r)) else {
            return NO_REPOSITORY.into();
        };
        match (root.head_label(), root.tracking_label()) {
            (Some(head), Some(track)) => format!("{head} {track}"),
            (Some(head), None) => head,
            (None, _) => NO_REPOSITORY.into(),
        }
    }
}

impl Render for GitChrome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.tokens().palette.clone();
        match self.part {
            ChromePart::Badge => match self.changed_files(cx) {
                Some(n) => div()
                    .id("sidebar-git-badge")
                    .test_support()
                    .ml(px(4.))
                    .text_color(p.sand.hsla())
                    .child(n.to_string())
                    .into_any_element(),
                None => Empty.into_any_element(),
            },
            ChromePart::Branch => div()
                .id("status-branch")
                .test_support()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(icon(Icon::GitBranch, px(12.), p.text_muted.hsla()))
                .child(self.branch_text(cx))
                .into_any_element(),
        }
    }
}

/// The Git tab's badge in `window`.
pub fn badge(window: &mut Window, cx: &mut App) -> Option<AnyElement> {
    Some(GitChrome::for_window(ChromePart::Badge, window, cx).into_any_element())
}

/// The status bar's branch in `window`.
pub fn status_branch(window: &mut Window, cx: &mut App) -> AnyElement {
    GitChrome::for_window(ChromePart::Branch, window, cx).into_any_element()
}
