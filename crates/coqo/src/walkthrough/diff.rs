// SPDX-License-Identifier: Apache-2.0

//! The walkthrough in the review surface (ADR 0021): its change as a
//! [`DiffSource`], and its steps as a [`DiffOverlay`].
//!
//! - [`WalkthroughDiffSource`]: the scope's base (the commit it resolved to
//!   when published) against its head: the working tree for a `worktree`
//!   scope (untracked files included, as in the Git pane), else the head
//!   commit. So "View as diff" shows exactly the walkthrough's change.
//! - [`StepOverlay`]: a numbered marker on each step's first line (a click
//!   goes to the step), a tint over the current step's lines, and the step
//!   popover in a slot under them.
//!
//! Anchors are lines of the files as they are now, so they are placed on
//! the new side.

use std::{cell::RefCell, rc::Rc};

use coqo_protocol::methods::{
    git::{self as gm, DiffSide, FileChange, FileDiff, GitChangeKind},
    walkthroughs::{ScopeKind, Walkthrough},
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    ActiveTokens as _,
    diff::{GutterMarker, LineRange, LineRef, LineTint, Side, Slot, SlotTarget},
    gpui::{
        AnyElement, AnyView, App, ElementId, Entity, FontWeight, InteractiveElement as _,
        IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _,
        Styled as _, Subscription, WeakEntity, Window, div, px,
    },
    widgets::TestSupportExt as _,
};
use futures::future::BoxFuture;

use super::player::WalkthroughPlayer;
use crate::{
    daemon::{Daemon, DaemonError},
    diff::{
        DiffOverlay, DiffSource, SourceFile,
        source::{FULL_CONTEXT, SourceError},
    },
    git::model::{GitModel, GitModelEvent},
};

fn message(err: DaemonError) -> SourceError {
    match err {
        DaemonError::NotConnected => "Not connected to coqod".to_owned(),
        err => err.to_string(),
    }
}

/// A walkthrough's change. See the module docs.
pub struct WalkthroughDiffSource {
    id: String,
    workspace: WorkspaceId,
    root: RootId,
    base: DiffSide,
    head: DiffSide,
    base_label: SharedString,
    head_label: SharedString,
    worktree: bool,
    model: Entity<GitModel>,
}

impl std::fmt::Debug for WalkthroughDiffSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalkthroughDiffSource")
            .field("id", &self.id)
            .field("base", &self.base)
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

impl WalkthroughDiffSource {
    pub fn new(w: &Walkthrough, window: &mut Window, cx: &mut App) -> Self {
        let s = &w.scope;
        let base_rev = s
            .base_commit
            .clone()
            .or_else(|| s.base.clone())
            .unwrap_or_else(|| "HEAD".to_owned());
        let worktree = s.kind == ScopeKind::Worktree;
        let head = if worktree {
            DiffSide::Worktree
        } else {
            DiffSide::Rev(
                s.head_commit
                    .clone()
                    .or_else(|| s.head.clone())
                    .unwrap_or_else(|| "HEAD".to_owned()),
            )
        };
        let base_label = s.base.clone().unwrap_or_else(|| "HEAD".to_owned());
        let head_label = if worktree {
            "working tree".to_owned()
        } else {
            s.head.clone().unwrap_or_else(|| "HEAD".to_owned())
        };
        Self {
            id: w.id.0.clone(),
            workspace: w.workspace.clone(),
            root: s.root,
            base: DiffSide::Rev(base_rev),
            head,
            base_label: base_label.into(),
            head_label: head_label.into(),
            worktree,
            model: GitModel::for_window(window, cx),
        }
    }
}

impl DiffSource for WalkthroughDiffSource {
    fn identity(&self) -> String {
        format!("walkthrough:{}", self.id)
    }

    fn base_label(&self) -> SharedString {
        self.base_label.clone()
    }

    fn head_label(&self) -> SharedString {
        self.head_label.clone()
    }

    fn files(&self, cx: &mut App) -> BoxFuture<'static, Result<Vec<SourceFile>, SourceError>> {
        let Some(daemon) = Daemon::global(cx) else {
            return Box::pin(async { Err(message(DaemonError::NotConnected)) });
        };
        let call = daemon
            .read(cx)
            .request::<gm::DiffFiles>(gm::DiffFilesParams {
                workspace: self.workspace.clone(),
                root: self.root,
                base: self.base.clone(),
                head: self.head.clone(),
            });
        // `git diff` leaves untracked files out; a change in the working
        // tree includes the files it added.
        let status = self.worktree.then(|| {
            daemon.read(cx).request::<gm::Status>(gm::RepoParams {
                workspace: self.workspace.clone(),
                root: self.root,
            })
        });
        Box::pin(async move {
            let untracked: Vec<String> = match status {
                Some(status) => status
                    .await
                    .map_err(message)?
                    .entries
                    .into_iter()
                    .filter(|e| e.unstaged == Some(FileChange::Untracked) && !e.path.ends_with('/'))
                    .map(|e| e.path)
                    .collect(),
                None => Vec::new(),
            };
            let mut files: Vec<SourceFile> = call
                .await
                .map_err(message)?
                .files
                .into_iter()
                .map(|f| SourceFile {
                    path: f.path,
                    change: f.change,
                    additions: f.additions,
                    deletions: f.deletions,
                    binary: f.binary,
                })
                .collect();
            for path in untracked {
                if !files.iter().any(|f| f.path == path) {
                    files.push(SourceFile {
                        path,
                        change: FileChange::Untracked,
                        additions: 0,
                        deletions: 0,
                        binary: false,
                    });
                }
            }
            files.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(files)
        })
    }

    fn file(&self, path: &str, cx: &mut App) -> BoxFuture<'static, Result<FileDiff, SourceError>> {
        let Some(daemon) = Daemon::global(cx) else {
            return Box::pin(async { Err(message(DaemonError::NotConnected)) });
        };
        let call = daemon.read(cx).request::<gm::Diff>(gm::DiffParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: path.to_owned(),
            base: self.base.clone(),
            head: self.head.clone(),
            context: Some(FULL_CONTEXT),
        });
        Box::pin(async move { call.await.map_err(message) })
    }

    fn watch(
        &self,
        changed: Rc<dyn Fn(&mut App)>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Subscription> {
        if !self.worktree {
            return None; // Commits do not change.
        }
        let root = self.root;
        Some(
            cx.subscribe(&self.model, move |_, event: &GitModelEvent, cx| {
                let GitModelEvent::Changed { root: r, kinds } = event else {
                    return;
                };
                let ours = [
                    GitChangeKind::Worktree,
                    GitChangeKind::Index,
                    GitChangeKind::Head,
                ];
                if *r == root && kinds.iter().any(|k| ours.contains(k)) {
                    changed(cx);
                }
            }),
        )
    }
}

/// One step anchor in the diff.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffMark {
    /// The step's index.
    pub step: usize,
    /// Repository-relative, as the diff names it.
    pub path: String,
    /// 1-based, inclusive, on the new side.
    pub start: u32,
    pub end: u32,
    /// The anchor the popover is at.
    pub current: bool,
}

#[derive(Default)]
struct OverlayState {
    marks: Vec<DiffMark>,
    popover: Option<AnyView>,
}

/// The steps over a [`crate::diff::DiffView`]. See the module docs.
pub struct StepOverlay {
    state: RefCell<OverlayState>,
    player: WeakEntity<WalkthroughPlayer>,
}

impl StepOverlay {
    pub fn new(player: WeakEntity<WalkthroughPlayer>) -> Self {
        Self {
            state: RefCell::default(),
            player,
        }
    }

    /// Show these marks, and `popover` under the current one.
    pub fn set(&self, marks: Vec<DiffMark>, popover: Option<AnyView>) {
        let mut state = self.state.borrow_mut();
        state.marks = marks;
        state.popover = popover;
    }

    /// The marks shown.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn marks(&self) -> Vec<DiffMark> {
        self.state.borrow().marks.clone()
    }

    fn marker_id(ix: usize) -> ElementId {
        ElementId::NamedInteger("walkthrough-diff-step".into(), ix as u64)
    }
}

impl DiffOverlay for StepOverlay {
    fn markers(&self, path: &str, _diff: &FileDiff, _cx: &App) -> Vec<GutterMarker> {
        let state = self.state.borrow();
        let mut seen = Vec::new();
        let mut out = Vec::new();
        for (ix, m) in state.marks.iter().enumerate() {
            // One marker per line: the current step's wins.
            if m.path != path || seen.contains(&m.start) {
                continue;
            }
            let first = state
                .marks
                .iter()
                .position(|o| o.path == path && o.start == m.start && o.current)
                .unwrap_or(ix);
            seen.push(m.start);
            out.push(GutterMarker {
                id: Self::marker_id(first),
                at: LineRef::new(path.to_owned(), Side::New, m.start),
                label: format!("Step {}", state.marks[first].step + 1).into(),
            });
        }
        out
    }

    fn render_marker(
        &self,
        marker: &GutterMarker,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let state = self.state.borrow();
        let Some(mark) = state
            .marks
            .iter()
            .enumerate()
            .find(|(ix, _)| Self::marker_id(*ix) == marker.id)
            .map(|(_, m)| m.clone())
        else {
            return div().into_any_element();
        };
        let p = &cx.tokens().palette;
        let color = if mark.current {
            p.sage.hsla()
        } else {
            p.sage.hsla().opacity(0.55)
        };
        let player = self.player.clone();
        let step = mark.step;
        div()
            .id(marker.id.clone())
            .test_support()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .child(
                div()
                    .min_w(px(16.))
                    .h(px(16.))
                    .px(px(2.))
                    .rounded(px(5.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(color)
                    .text_color(p.panel.hsla())
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .child((step + 1).to_string()),
            )
            .on_click(move |_, window, cx| {
                player
                    .update(cx, |player, cx| player.go_to(step, 0, true, window, cx))
                    .ok();
            })
            .into_any_element()
    }

    fn slots(&self, path: &str, _diff: &FileDiff, _cx: &App) -> Vec<Slot> {
        let state = self.state.borrow();
        if state.popover.is_none() {
            return Vec::new();
        }
        state
            .marks
            .iter()
            .filter(|m| m.current && m.path == path)
            .take(1)
            .map(|m| Slot {
                id: "walkthrough-diff-popover".into(),
                at: SlotTarget::Line(LineRef::new(path.to_owned(), Side::New, m.end)),
            })
            .collect()
    }

    fn render_slot(&self, _slot: &Slot, _window: &mut Window, _cx: &mut App) -> AnyElement {
        let state = self.state.borrow();
        match &state.popover {
            Some(view) => div()
                .px(px(16.))
                .py(px(10.))
                .max_w(px(760.))
                .child(view.clone())
                .into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn tints(&self, path: &str, _diff: &FileDiff, cx: &App) -> Vec<LineTint> {
        let color = cx.tokens().palette.sage.hsla().opacity(0.13);
        self.state
            .borrow()
            .marks
            .iter()
            .filter(|m| m.current && m.path == path)
            .map(|m| LineTint {
                range: LineRange::lines(path.to_owned(), Side::New, m.start, m.end),
                color,
            })
            .collect()
    }
}
