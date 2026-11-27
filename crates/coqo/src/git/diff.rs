// SPDX-License-Identifier: Apache-2.0

//! The `git-diff` tab kind: a root's unstaged (index → work tree) or staged
//! (`HEAD` → index) changes in the review surface ([`DiffView`], #93), with
//! hunk and line staging.
//!
//! One tab per root and side. Opening a file's diff from the Git pane shows
//! that file in the tab. Saved state from before #93 (one tab per file)
//! restores as the side's tab showing that file.

use std::rc::Rc;

use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_ui::{
    ActiveTokens as _,
    dock::{AnyTab, TabItem},
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
        IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, div, px,
    },
    serde_json::{Value, json},
    widgets::{Icon, icon},
};

use crate::{
    diff::{DiffView, source::GitDiffSource},
    threads::overlay::{ReviewState, ThreadsOverlay},
};

/// The tab kind. Persisted: never change it.
pub const KIND: &str = "git-diff";

/// The de-duplication key of a diff tab.
pub fn key(workspace: &WorkspaceId, root: RootId, staged: bool) -> String {
    let side = if staged { "staged" } else { "unstaged" };
    format!("{workspace}/{root}/{side}")
}

/// Recreate a saved diff tab.
pub fn restore(state: &Value, window: &mut Window, cx: &mut App) -> Option<AnyTab> {
    let workspace = WorkspaceId(state.get("workspace")?.as_str()?.to_owned());
    let root = RootId(u32::try_from(state.get("root")?.as_u64()?).ok()?);
    let path = state.get("path").and_then(Value::as_str).map(str::to_owned);
    let staged = state.get("staged")?.as_bool()?;
    Some(AnyTab::new(cx.new(|cx| {
        DiffTab::new(workspace, root, staged, path, window, cx)
    })))
}

/// A root's changes, reviewed.
pub struct DiffTab {
    workspace: WorkspaceId,
    root: RootId,
    staged: bool,
    view: Entity<DiffView>,
    /// The comment threads in the view (#94), with a daemon. The view keeps
    /// the overlay alive; tests reach it here.
    #[cfg_attr(not(test), allow(dead_code))]
    review: Option<Entity<ReviewState>>,
}

impl std::fmt::Debug for DiffTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffTab")
            .field("root", &self.root)
            .field("staged", &self.staged)
            .finish_non_exhaustive()
    }
}

impl DiffTab {
    pub fn new(
        workspace: WorkspaceId,
        root: RootId,
        staged: bool,
        select: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = GitDiffSource::new(workspace.clone(), root, staged, window, cx);
        let view = cx.new(|cx| DiffView::new(Rc::new(source), select, window, cx));
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        let review = ThreadsOverlay::attach(&view, window, cx);
        Self {
            workspace,
            root,
            staged,
            view,
            review,
        }
    }

    /// A tab over a fixed set of diffs, without a daemon (screenshots).
    #[cfg(test)]
    pub fn with_view(
        workspace: WorkspaceId,
        root: RootId,
        staged: bool,
        view: Entity<DiffView>,
    ) -> Self {
        Self {
            workspace,
            root,
            staged,
            view,
            review: None,
        }
    }

    pub fn view(&self) -> &Entity<DiffView> {
        &self.view
    }

    /// The comment threads' state, when the view shows them.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn review(&self) -> Option<&Entity<ReviewState>> {
        self.review.as_ref()
    }

    fn title_text(&self) -> &'static str {
        if self.staged {
            "Staged changes"
        } else {
            "Changes"
        }
    }
}

impl TabItem for DiffTab {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        self.title_text().into()
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(key(&self.workspace, self.root, self.staged).into())
    }

    fn save(&self, cx: &App) -> Value {
        json!({
            "workspace": self.workspace.0,
            "root": self.root.0,
            "staged": self.staged,
            "path": self.view.read(cx).selected_file(),
        })
    }

    fn render_label(&self, _: bool, _: &mut Window, cx: &App) -> Option<AnyElement> {
        let t = cx.tokens();
        let p = &t.palette;
        let (viewed, total) = self.view.read(cx).viewed_count();
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(icon(Icon::GitBranch, px(12.), p.text_muted.hsla()))
                .child(self.title_text())
                .child(
                    div()
                        .text_size(t.metrics.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .child(format!("{viewed}/{total}")),
                )
                .into_any_element(),
        )
    }

    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        Some(self.view.read(cx).focus_handle(cx))
    }
}

impl Render for DiffTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}
