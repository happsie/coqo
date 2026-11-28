// SPDX-License-Identifier: Apache-2.0

//! The `walkthrough` tab kind: the player of one walkthrough (board 3). See
//! the [module docs](super).
//!
//! The current step's code is shown in a [`FileEditor`] of the player's own
//! (the file's live buffer, editable, with its questions) or, in diff mode,
//! in a [`DiffView`] over the walkthrough's change. The step popover is
//! anchored at the step's lines in either.
//!
//! Under the header, board 3's **coverage banner** (#264) flags the changed
//! hunks no step explains, from `walkthroughs/get`'s `coverage` (checked
//! again on every load for a worktree scope): "14 of 15 changed hunks
//! covered.", each uncovered hunk as a link (into the diff in diff mode,
//! else a file tab), and "Ask <agent> to explain it", which asks the
//! publishing session in its chat as the chat card's button does.

use std::{collections::HashMap, rc::Rc};

use coqo_protocol::methods::{
    editor::LineRange as ProtocolLines,
    questions::{AnchorStatus, AskParams, NewAnchor, QuestionId},
    walkthroughs::{
        self as wtm, ChangeKind, ChangesParams, SetStepStateParams, StepState, StorageMode,
        UncoveredHunk, Walkthrough, WalkthroughCoverage, WalkthroughId, WalkthroughParams,
        WholeFile,
    },
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    ActiveTokens as _,
    diff::{LineRef, Side},
    dock::{AnyTab, TabItem},
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, FontWeight,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
        prelude::FluentBuilder as _, px,
    },
    serde_json::{Value, json},
    text::markdown,
    widgets::{Icon, TestSupportExt as _, icon, raised, section_label},
};

use super::{
    NextStep, PreviousStep, ToggleDiff, ToggleUnderstood, coverage,
    diff::{DiffMark, StepOverlay, WalkthroughDiffSource},
    popover::{StepData, StepEvent, StepPopover},
};
use crate::{
    chat::{AskAgent, OpenLocation},
    daemon::{Daemon, DaemonError, DaemonEvent},
    diff::DiffView,
    files::editor::{FileEditor, STEP_MARKER_BASE, StepMark, StepMarkerClicked},
    questions,
    workspace::Workspaces,
};

/// The tab kind. Persisted: never change it.
pub const KIND: &str = "walkthrough";

/// Markers per step: one per anchor.
const MARKERS_PER_STEP: u64 = 16;
/// Uncovered hunks listed in the coverage banner.
const BANNER_HUNKS: usize = 8;

/// The de-duplication key of a player tab.
pub fn key(id: &WalkthroughId) -> String {
    id.0.clone()
}

/// Recreate a saved player tab, at the step it showed.
pub fn restore(state: &Value, window: &mut Window, cx: &mut App) -> Option<AnyTab> {
    let workspace = WorkspaceId(state.get("workspace")?.as_str()?.to_owned());
    let id = WalkthroughId(state.get("id")?.as_str()?.to_owned());
    let step = state.get("step").and_then(Value::as_str).map(str::to_owned);
    let diff = state.get("mode").and_then(Value::as_str) == Some("diff");
    Some(AnyTab::new(cx.new(|cx| {
        let mut player = WalkthroughPlayer::new(workspace, id, step, window, cx);
        if diff {
            player.mode = Mode::Diff;
        }
        player
    })))
}

/// Where the step's code is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The file, in an editor.
    Editor,
    /// The walkthrough's diff.
    Diff,
}

/// A step to go to once the walkthrough is loaded.
#[derive(Clone, Debug)]
struct Pending {
    /// By id; the first unread one if `None`.
    step: Option<String>,
    /// Put the keyboard in the popover.
    focus: bool,
}

struct OpenEditor {
    root: RootId,
    path: String,
    editor: Entity<FileEditor>,
    _clicks: Subscription,
}

/// The player. See the module docs.
pub struct WalkthroughPlayer {
    workspace: WorkspaceId,
    id: WalkthroughId,
    walkthrough: Option<Walkthrough>,
    error: Option<String>,
    deleted: bool,
    current: usize,
    anchor: usize,
    mode: Mode,
    pending: Option<Pending>,
    editors: Vec<OpenEditor>,
    /// The editor the popover is in.
    shown: Option<(RootId, String)>,
    diff: Option<(Entity<DiffView>, Rc<StepOverlay>)>,
    popover: Entity<StepPopover>,
    /// The workspace's questions (who asked, asking).
    questions: Option<Entity<questions::WorkspaceQuestions>>,
    /// Questions asked here, by step id.
    asked: HashMap<String, QuestionId>,
    focus: FocusHandle,
    _loading: Option<Task<()>>,
    _changes: Option<Task<()>>,
    _saving: Option<Task<()>>,
    _asking: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for WalkthroughPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalkthroughPlayer")
            .field("id", &self.id)
            .field("current", &self.current)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl WalkthroughPlayer {
    /// A player of `id`, going to `step` (by id; the first unread one if
    /// `None`) once loaded.
    pub fn new(
        workspace: WorkspaceId,
        id: WalkthroughId,
        step: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let popover = cx.new(StepPopover::new);
        let questions = questions::questions_for(&workspace, cx);
        let mut subscriptions = vec![cx.subscribe_in(
            &popover,
            window,
            |this, _, event: &StepEvent, window, cx| this.on_step_event(event, window, cx),
        )];
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(
                cx.subscribe_in(&daemon, window, |this, _, event, window, cx| {
                    if *event == DaemonEvent::Connected {
                        this.follow(window, cx);
                        this.load(window, cx);
                    }
                }),
            );
        }
        // File tabs show its steps while it is open (#260).
        if let Some(store) = super::marks::walkthroughs_for(&workspace, cx) {
            store.update(cx, |store, cx| store.player_opened(&id, cx));
            let held = id.clone();
            subscriptions.push(cx.on_release(move |_, cx| {
                store.update(cx, |store, cx| store.player_closed(&held, cx));
            }));
        }
        let mut this = Self {
            workspace,
            id,
            walkthrough: None,
            error: None,
            deleted: false,
            current: 0,
            anchor: 0,
            mode: Mode::Editor,
            pending: Some(Pending { step, focus: false }),
            editors: Vec::new(),
            shown: None,
            diff: None,
            popover,
            questions,
            asked: HashMap::new(),
            focus: cx.focus_handle(),
            _loading: None,
            _changes: None,
            _saving: None,
            _asking: None,
            _subscriptions: subscriptions,
        };
        this.follow(window, cx);
        this.load(window, cx);
        this
    }

    /// Go to `step` (by id; the first unread one if `None`) with the
    /// keyboard in its popover (what opening a walkthrough does).
    pub fn start_at(&mut self, step: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = Some(Pending { step, focus: true });
        if self.walkthrough.is_some() {
            self.take_pending(window, cx);
        }
    }

    // ---- accessors (tests, other views) -------------------------------------

    /// The walkthrough, once loaded.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn walkthrough(&self) -> Option<&Walkthrough> {
        self.walkthrough.as_ref()
    }

    /// The current step's index.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn current(&self) -> usize {
        self.current
    }

    /// The current anchor of the step.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn popover(&self) -> &Entity<StepPopover> {
        &self.popover
    }

    /// The editor of `path` (root-relative), if the player opened it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn editor(&self, path: &str) -> Option<&Entity<FileEditor>> {
        self.editors
            .iter()
            .find(|e| e.path == path)
            .map(|e| &e.editor)
    }

    /// The diff view and its overlay, once diff mode was used.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn diff(&self) -> Option<(&Entity<DiffView>, &Rc<StepOverlay>)> {
        self.diff.as_ref().map(|(v, o)| (v, o))
    }

    // ---- the daemon -----------------------------------------------------------

    /// Follow `walkthroughs/changes` (live only: load again after).
    fn follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let subscribe = daemon.read(cx).subscribe::<wtm::Changes>(ChangesParams {
            workspace: Some(self.workspace.clone()),
        });
        let id = self.id.clone();
        self._changes = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(mut events) = subscribe.await else {
                return;
            };
            while let Some(Ok(change)) = events.next().await {
                if change.id != id {
                    continue;
                }
                let gone = change.kind == ChangeKind::Deleted;
                let ok = this.update_in(cx, |this, window, cx| {
                    if gone {
                        this.deleted = true;
                        cx.notify();
                    } else {
                        this.load(window, cx);
                    }
                });
                if ok.is_err() {
                    return;
                }
            }
        }));
    }

    /// `walkthroughs/get`, its anchors resolved against the files now.
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let get = daemon.read(cx).request::<wtm::Get>(WalkthroughParams {
            id: self.id.clone(),
        });
        self._loading = Some(cx.spawn_in(window, async move |this, cx| {
            let result = get.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(w) => this.loaded(w, window, cx),
                Err(e) => {
                    this.error = Some(match e {
                        DaemonError::NotConnected => "Not connected to coqod".to_owned(),
                        e => e.to_string(),
                    });
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn loaded(&mut self, w: Walkthrough, window: &mut Window, cx: &mut Context<Self>) {
        // Keep the step shown (by id) across reloads.
        let current_id = self
            .walkthrough
            .as_ref()
            .and_then(|old| old.steps.get(self.current))
            .map(|s| s.id.clone());
        let first = self.walkthrough.is_none();
        self.error = None;
        self.deleted = false;
        if let Some(ix) = current_id.and_then(|id| w.steps.iter().position(|s| s.id == id)) {
            self.current = ix;
        }
        self.current = self.current.min(w.steps.len().saturating_sub(1));
        // Published again (#253), the step may have fewer anchors.
        let anchors = w.steps.get(self.current).map_or(1, |s| s.anchors.len());
        self.anchor = self.anchor.min(anchors.saturating_sub(1));
        self.walkthrough = Some(w);
        if self.pending.is_some() {
            self.take_pending(window, cx);
        } else if first {
            self.show(false, window, cx);
        } else {
            // Progress or anchors changed: refresh what is shown in place.
            self.refresh(window, cx);
        }
        cx.notify();
    }

    fn take_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pending), Some(w)) = (self.pending.take(), self.walkthrough.as_ref()) else {
            return;
        };
        let ix = pending
            .step
            .and_then(|id| w.steps.iter().position(|s| s.id == id))
            .unwrap_or_else(|| super::first_unread(w));
        self.go_to(ix, 0, pending.focus, window, cx);
    }

    // ---- navigation -----------------------------------------------------------

    /// Show step `ix` at its anchor `anchor`, with the keyboard in the
    /// popover with `focus`.
    pub fn go_to(
        &mut self,
        ix: usize,
        anchor: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(w) = &self.walkthrough else {
            return;
        };
        if w.steps.is_empty() {
            return;
        }
        self.current = ix.min(w.steps.len() - 1);
        let anchors = w.steps[self.current].anchors.len().max(1);
        self.anchor = anchor.min(anchors - 1);
        self.show(focus, window, cx);
        cx.notify();
    }

    pub fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let last = self
            .walkthrough
            .as_ref()
            .map_or(0, |w| w.steps.len().saturating_sub(1));
        if self.current < last {
            self.go_to(self.current + 1, 0, true, window, cx);
        }
    }

    pub fn previous(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.current > 0 {
            self.go_to(self.current - 1, 0, true, window, cx);
        }
    }

    /// Show the step's next (or previous) anchor, wrapping around.
    fn cycle_anchor(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = self
            .walkthrough
            .as_ref()
            .and_then(|w| w.steps.get(self.current))
            .map(|s| s.anchors.len())
            .filter(|n| *n > 1)
        else {
            return;
        };
        let anchor = if forward {
            (self.anchor + 1) % n
        } else {
            (self.anchor + n - 1) % n
        };
        self.go_to(self.current, anchor, true, window, cx);
    }

    /// Show the code in the editor or in the diff.
    pub fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        if mode == Mode::Diff {
            self.clear_editor_popover(cx);
        } else if let Some((_, overlay)) = &self.diff {
            overlay.set(Vec::new(), None);
        }
        self.mode = mode;
        self.show(true, window, cx);
        cx.notify();
    }

    fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = match self.mode {
            Mode::Editor => Mode::Diff,
            Mode::Diff => Mode::Editor,
        };
        self.set_mode(mode, window, cx);
    }

    fn on_step_event(&mut self, event: &StepEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            StepEvent::Previous => self.previous(window, cx),
            StepEvent::Next => self.next(window, cx),
            StepEvent::ToggleUnderstood => self.toggle_understood(cx),
            StepEvent::GoTo(ix) => self.go_to(*ix, 0, true, window, cx),
            StepEvent::Anchor(forward) => self.cycle_anchor(*forward, window, cx),
            StepEvent::Ask(text) => self.ask(text.clone(), window, cx),
            StepEvent::ViewDiff => self.set_mode(Mode::Diff, window, cx),
            // A file tab's (#260).
            StepEvent::OpenInPlayer | StepEvent::Close => {}
        }
    }

    // ---- showing a step ---------------------------------------------------------

    fn marker_id(step: usize, anchor: usize) -> u64 {
        STEP_MARKER_BASE + step as u64 * MARKERS_PER_STEP + anchor as u64
    }

    /// The step and anchor of marker `id`.
    fn marker_step(id: u64) -> (usize, usize) {
        let rel = id - STEP_MARKER_BASE;
        (
            (rel / MARKERS_PER_STEP) as usize,
            (rel % MARKERS_PER_STEP) as usize,
        )
    }

    /// What the popover shows for the current step.
    fn step_data(&self, cx: &App) -> Option<StepData> {
        let w = self.walkthrough.as_ref()?;
        let asked = w
            .steps
            .get(self.current)
            .and_then(|s| self.asked.get(&s.id).copied());
        let agent = super::agent_name(w, self.questions.as_ref(), cx);
        super::popover::step_data(w, self.current, self.anchor, agent, asked)
    }

    /// Who wrote the walkthrough: "Claude".
    fn agent_name(&self, cx: &App) -> String {
        match &self.walkthrough {
            Some(w) => super::agent_name(w, self.questions.as_ref(), cx),
            None => "the agent".to_owned(),
        }
    }

    /// The step markers of `root`/`path`.
    fn marks_in(&self, root: RootId, path: &str, cx: &App) -> Vec<StepMark> {
        let Some(w) = &self.walkthrough else {
            return Vec::new();
        };
        let sage = cx.tokens().palette.sage.hsla();
        let mut marks = Vec::new();
        for (i, step) in w.steps.iter().enumerate() {
            for (j, a) in step.anchors.iter().enumerate() {
                if a.anchor.root != root
                    || a.anchor.path != path
                    || super::anchor_status(a) == AnchorStatus::Orphaned
                {
                    continue;
                }
                marks.push(StepMark {
                    id: Self::marker_id(i, j),
                    lines: super::anchor_lines(a),
                    color: if i == self.current {
                        sage
                    } else {
                        sage.opacity(0.5)
                    },
                    label: (i + 1).to_string().into(),
                });
            }
        }
        marks
    }

    /// The step anchors in the diff.
    fn diff_marks(&self) -> Vec<DiffMark> {
        let Some(w) = &self.walkthrough else {
            return Vec::new();
        };
        let mut marks = Vec::new();
        for (i, step) in w.steps.iter().enumerate() {
            for (j, a) in step.anchors.iter().enumerate() {
                if a.anchor.root != w.scope.root
                    || super::anchor_status(a) == AnchorStatus::Orphaned
                {
                    continue;
                }
                let lines = super::anchor_lines(a);
                marks.push(DiffMark {
                    step: i,
                    path: a.anchor.path.clone(),
                    start: lines.start + 1,
                    end: lines.end.max(lines.start + 1),
                    current: i == self.current && j == self.anchor,
                });
            }
        }
        marks
    }

    /// Update the popover and the markers without moving.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(data) = self.step_data(cx) {
            self.popover.update(cx, |p, cx| p.set(data, cx));
        }
        self.sync_editor_marks(cx);
        if self.mode == Mode::Diff {
            self.sync_diff(cx);
        } else if let Some((root, path)) = self.shown.clone()
            && let Some(editor) = self.find_editor(root, &path)
        {
            // An anchor may have appeared or gone.
            let marker = self.current_marker();
            let view = self.popover.clone();
            let shows = editor.read(cx).shows_step();
            if shows {
                editor.update(cx, |e, cx| {
                    e.show_step(view.into(), marker, false, None, window, cx)
                });
            }
        }
    }

    fn sync_editor_marks(&self, cx: &mut Context<Self>) {
        for open in &self.editors {
            let marks = self.marks_in(open.root, &open.path, cx);
            open.editor.update(cx, |e, cx| e.set_step_marks(marks, cx));
        }
    }

    /// The marker of the current anchor, unless its code is gone.
    fn current_marker(&self) -> Option<u64> {
        let a = self
            .walkthrough
            .as_ref()?
            .steps
            .get(self.current)?
            .anchors
            .get(self.anchor)?;
        (super::anchor_status(a) != AnchorStatus::Orphaned)
            .then(|| Self::marker_id(self.current, self.anchor))
    }

    fn find_editor(&self, root: RootId, path: &str) -> Option<Entity<FileEditor>> {
        self.editors
            .iter()
            .find(|e| e.root == root && e.path == path)
            .map(|e| e.editor.clone())
    }

    fn editor_for(
        &mut self,
        root: RootId,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<FileEditor> {
        if let Some(editor) = self.find_editor(root, path) {
            return editor;
        }
        let workspace = self.workspace.clone();
        let owned = path.to_owned();
        let editor = cx.new(|cx| {
            let mut editor = FileEditor::new(workspace, root, owned, window, cx);
            editor.set_walkthrough_host(cx);
            editor
        });
        let clicks = cx.subscribe_in(
            &editor,
            window,
            |this, _, clicked: &StepMarkerClicked, window, cx| {
                let (step, anchor) = Self::marker_step(clicked.0);
                this.go_to(step, anchor, true, window, cx);
            },
        );
        self.editors.push(OpenEditor {
            root,
            path: path.to_owned(),
            editor: editor.clone(),
            _clicks: clicks,
        });
        editor
    }

    fn clear_editor_popover(&mut self, cx: &mut Context<Self>) {
        if let Some((root, path)) = self.shown.take()
            && let Some(editor) = self.find_editor(root, &path)
        {
            editor.update(cx, |e, cx| e.clear_step(cx));
        }
    }

    /// Show the current step: its code, its markers and the popover.
    fn show(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.step_data(cx) else {
            return;
        };
        self.popover.update(cx, |p, cx| p.set(data, cx));
        let Some(a) = self
            .walkthrough
            .as_ref()
            .and_then(|w| w.steps.get(self.current))
            .and_then(|s| s.anchors.get(self.anchor))
            .cloned()
        else {
            return;
        };
        let (root, path) = (a.anchor.root, a.anchor.path.clone());
        match self.mode {
            Mode::Editor => {
                if self.shown.as_ref() != Some(&(root, path.clone())) {
                    self.clear_editor_popover(cx);
                }
                self.editor_for(root, &path, window, cx);
                self.sync_editor_marks(cx);
                let marker = self.current_marker();
                let view = self.popover.clone();
                let focus = focus.then(|| self.popover.focus_handle(cx));
                if let Some(editor) = self.find_editor(root, &path) {
                    let host = Some(editor.downgrade());
                    self.popover.update(cx, |p, cx| p.set_host(host, cx));
                    editor.update(cx, |e, cx| {
                        e.show_step(view.into(), marker, true, focus, window, cx)
                    });
                }
                self.shown = Some((root, path));
            }
            Mode::Diff => {
                self.popover.update(cx, |p, cx| p.set_host(None, cx));
                let (view, _) = self.diff_view(window, cx);
                self.sync_diff(cx);
                let lines = super::anchor_lines(&a);
                let at = LineRef::new(path, Side::New, lines.start + 1);
                view.update(cx, |v, cx| v.scroll_to(&at, cx));
                if focus {
                    let handle = self.popover.focus_handle(cx);
                    window.focus(&handle, cx);
                }
            }
        }
        cx.notify();
    }

    fn diff_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<DiffView>, Rc<StepOverlay>) {
        if let Some((view, overlay)) = &self.diff {
            return (view.clone(), overlay.clone());
        }
        let source = match &self.walkthrough {
            Some(w) => WalkthroughDiffSource::new(w, window, cx),
            None => unreachable!("diff mode needs the walkthrough"),
        };
        let select = self
            .walkthrough
            .as_ref()
            .and_then(|w| w.steps.get(self.current))
            .and_then(|s| s.anchors.get(self.anchor))
            .map(|a| a.anchor.path.clone());
        let overlay = Rc::new(StepOverlay::new(cx.entity().downgrade()));
        let view = cx.new(|cx| {
            let mut view = DiffView::new(Rc::new(source), select, window, cx);
            view.add_overlay(overlay.clone(), cx);
            // Unified, so the step popover has the width beside the step
            // list (`m` switches, as anywhere).
            view.viewer().set_mode(coqo_ui::diff::DiffMode::Unified, cx);
            view
        });
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        self.diff = Some((view.clone(), overlay.clone()));
        (view, overlay)
    }

    fn sync_diff(&self, cx: &mut Context<Self>) {
        let Some((view, overlay)) = &self.diff else {
            return;
        };
        let popover = (self.mode == Mode::Diff).then(|| self.popover.clone().into());
        overlay.set(self.diff_marks(), popover);
        view.update(cx, |_, cx| cx.notify());
    }

    // ---- progress and questions -----------------------------------------------

    fn toggle_understood(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self
            .walkthrough
            .as_ref()
            .and_then(|w| w.steps.get(self.current).map(|s| super::step_state(w, s)))
        else {
            return;
        };
        let next = if state == StepState::Understood {
            StepState::Unread
        } else {
            StepState::Understood
        };
        self.set_state(self.current, next, cx);
    }

    /// Mark step `ix` (`walkthroughs/setStepState`); shown at once.
    pub fn set_state(&mut self, ix: usize, state: StepState, cx: &mut Context<Self>) {
        let Some(w) = self.walkthrough.as_mut() else {
            return;
        };
        let Some(step) = w.steps.get(ix).map(|s| s.id.clone()) else {
            return;
        };
        if state == StepState::Unread {
            w.step_states.remove(&step);
        } else {
            w.step_states.insert(step.clone(), state);
        }
        if let Some(data) = self.step_data(cx) {
            self.popover.update(cx, |p, cx| p.set(data, cx));
        }
        cx.notify();
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let call = daemon
            .read(cx)
            .request::<wtm::SetStepState>(SetStepStateParams {
                id: self.id.clone(),
                step,
                state,
            });
        self._saving = Some(cx.spawn(async move |this, cx| {
            if let Err(e) = call.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Couldn't save the progress: {e}"));
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Ask the agent that wrote the walkthrough about the current step: a
    /// thread pinned to the step's lines (`questions/ask`), and the step
    /// marked `question`.
    fn ask(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = &self.walkthrough else {
            return;
        };
        let ix = self.current;
        let Some(step) = w.steps.get(ix) else {
            return;
        };
        let a = step.anchors.get(self.anchor).or(step.anchors.first());
        let anchors: Vec<NewAnchor> = a
            .map(|a| {
                let lines = super::anchor_lines(a);
                NewAnchor {
                    root: a.anchor.root,
                    path: a.anchor.path.clone(),
                    lines: ProtocolLines {
                        start: lines.start,
                        end: lines.end.max(lines.start + 1) - 1,
                    },
                }
            })
            .into_iter()
            .collect();
        let params = AskParams {
            workspace: self.workspace.clone(),
            session: w.created_by.session.clone(),
            body: super::question_body(w, ix, &text),
            anchors,
        };
        let step_id = step.id.clone();
        let Some(store) = self.questions.clone() else {
            self.popover.update(cx, |p, cx| {
                p.sent(Err("Not connected to coqod".to_owned()), window, cx)
            });
            return;
        };
        let asked = questions::ask(&store, params, cx);
        self._asking = Some(cx.spawn_in(window, async move |this, cx| {
            let result = asked.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(q) => {
                    this.asked.insert(step_id, q.id);
                    this.set_state(ix, StepState::Question, cx);
                    this.popover.update(cx, |p, cx| p.sent(Ok(()), window, cx));
                }
                Err(e) => {
                    let message = match e {
                        DaemonError::Rpc(e) => e.message,
                        e => e.to_string(),
                    };
                    this.popover.update(cx, |p, cx| {
                        p.sent(Err(format!("Couldn't ask: {message}")), window, cx)
                    });
                }
            })
            .ok();
        }));
    }

    /// Show uncovered hunk `h`: in the diff in diff mode (and for a deleted
    /// file, which only the diff shows), else in a file tab.
    fn open_hunk(&mut self, h: &UncoveredHunk, window: &mut Window, cx: &mut Context<Self>) {
        let deleted = h.whole_file == Some(WholeFile::Deleted);
        if self.mode == Mode::Editor
            && let Some(at) = coverage::hunk_location(h)
        {
            coverage::open_location(&at, window, cx);
            return;
        }
        if deleted && self.mode == Mode::Editor {
            self.set_mode(Mode::Diff, window, cx);
        }
        let (view, _) = self.diff_view(window, cx);
        let at = match (deleted, h.old_range) {
            (true, Some(old)) => LineRef::new(h.path.clone(), Side::Old, old.start + 1),
            _ => LineRef::new(h.path.clone(), Side::New, h.new_range.start + 1),
        };
        view.update(cx, |v, cx| v.scroll_to(&at, cx));
        cx.notify();
    }

    /// "Ask <agent> to explain it": the uncovered hunks to the session that
    /// published the walkthrough, in its chat.
    fn ask_to_explain(&self, window: &mut Window, cx: &mut App) {
        let Some(w) = &self.walkthrough else {
            return;
        };
        let Some(c) = &w.coverage else {
            return;
        };
        let action = AskAgent {
            session: w.created_by.session.0.clone(),
            prompt: coverage::explain_prompt(&w.title, &w.id, c),
        };
        window.dispatch_action(Box::new(action), cx);
    }

    /// Open the current anchor in a file tab of its own.
    fn open_file(&self, window: &mut Window, cx: &mut App) {
        let Some(a) = self
            .walkthrough
            .as_ref()
            .and_then(|w| w.steps.get(self.current))
            .and_then(|s| s.anchors.get(self.anchor))
        else {
            return;
        };
        let workspaces = Workspaces::for_window(window, cx).or_else(|| Workspaces::global(cx));
        let Some(root) = workspaces.as_ref().and_then(|w| {
            w.read(cx)
                .current()?
                .roots
                .iter()
                .find(|r| r.id == a.anchor.root)
                .map(|r| r.path.clone())
        }) else {
            return;
        };
        let lines = super::anchor_lines(a);
        window.dispatch_action(
            Box::new(OpenLocation {
                path: format!("{}/{}", root.trim_end_matches('/'), a.anchor.path),
                line: Some(lines.start),
                end: Some(lines.end.max(lines.start + 1) - 1),
            }),
            cx,
        );
    }

    // ---- rendering ------------------------------------------------------------

    fn render_header(&self, w: &Walkthrough, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let total = w.steps.len();
        let done = super::understood_count(w);
        let pct = if total == 0 {
            0.
        } else {
            done as f32 / total as f32
        };
        let storage = match (&w.storage.mode, &w.storage.error) {
            (StorageMode::Committed, None) => "In .coqo/walkthroughs".to_owned(),
            (StorageMode::Committed, Some(_)) => "Stored locally (commit failed)".to_owned(),
            (StorageMode::Local, _) => "Stored locally".to_owned(),
        };
        let subtitle = format!(
            "{} · {} · written by {}",
            super::scope_label(w),
            super::files_label(super::files(w).len()),
            self.agent_name(cx),
        );
        let bar_width = 140.;
        // Not a repository, no git: say so quietly, with why on hover.
        let unchecked = w
            .coverage
            .as_ref()
            .and_then(|c| c.unavailable.clone())
            .map(|why| {
                div()
                    .id("walkthrough-coverage")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(10.))
                    .bg(p.surface.hsla())
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .child(icon(Icon::CircleAlert, px(12.), p.text_subtle.hsla()))
                    .child("Coverage not checked")
                    .tooltip(move |_, cx| {
                        let why = SharedString::from(format!("{why}."));
                        cx.new(|_| Tip(why)).into()
                    })
            });
        div()
            .id("walkthrough-header")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(16.))
            .py(px(12.))
            .rounded(px(16.))
            .bg(p.panel_raised.hsla())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(14.))
                    .child(icon(Icon::FileCode, px(20.), p.sage.hsla()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(240.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .id("walkthrough-title")
                                    .test_support()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(format!(
                                        "Walkthrough · {}",
                                        w.title
                                    ))),
                            )
                            .child(
                                div()
                                    .text_size(m.text_sm)
                                    .text_color(p.text_muted.hsla())
                                    .child(subtitle),
                            ),
                    )
                    .children(unchecked)
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(10.))
                            .bg(p.surface.hsla())
                            .text_size(m.text_sm)
                            .text_color(p.text_muted.hsla())
                            .child(storage),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .w(px(bar_width))
                                    .h(px(8.))
                                    .rounded(px(4.))
                                    .bg(p.surface_selected.hsla())
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .h_full()
                                            .w(px(bar_width * pct))
                                            .rounded(px(4.))
                                            .bg(p.sage.hsla()),
                                    ),
                            )
                            .child(
                                div()
                                    .id("walkthrough-progress")
                                    .test_support()
                                    .text_size(m.text_sm)
                                    .text_color(p.text_muted.hsla())
                                    .whitespace_nowrap()
                                    .child(format!("{done} of {total} understood")),
                            ),
                    ),
            )
            .when(!w.summary.trim().is_empty(), |d| {
                d.child(
                    div()
                        .id("walkthrough-summary")
                        .test_support()
                        .text_color(p.text_muted.hsla())
                        .line_height(px(20.))
                        .child(markdown(
                            "walkthrough-summary-text",
                            w.summary.trim().to_owned(),
                        )),
                )
            })
            .into_any_element()
    }

    /// Board 3's coverage banner, under the header, while hunks are
    /// uncovered. Coverage that could not be checked is a chip in the
    /// header; complete coverage shows nothing.
    fn render_coverage(&self, w: &Walkthrough, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c: &WalkthroughCoverage = w.coverage.as_ref()?;
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        if !coverage::incomplete(c) {
            return None;
        }
        let (head, body) = coverage::banner_label(c);
        let ask = div()
            .id("walkthrough-coverage-ask")
            .test_support()
            .flex_none()
            .px(px(12.))
            .py(px(6.))
            .rounded(px(10.))
            .bg(p.rose.alpha(0.18))
            .border_1()
            .border_color(p.rose.alpha(0.35))
            .text_size(m.text_sm)
            .text_color(p.text.hsla())
            .cursor_pointer()
            .child(SharedString::from(format!(
                "Ask {} to explain it",
                self.agent_name(cx)
            )))
            .on_click(cx.listener(|this, _, window, cx| this.ask_to_explain(window, cx)));
        let mut links = div()
            .flex()
            .flex_wrap()
            .gap_x(px(16.))
            .gap_y(px(4.))
            .pl(px(28.));
        for (k, h) in c.uncovered.iter().take(BANNER_HUNKS).enumerate() {
            let hunk = h.clone();
            links = links.child(
                div()
                    .id(("walkthrough-coverage-hunk", k))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(m.text_label)
                    .child(
                        div()
                            .font_family(m.font_mono.clone())
                            .text_color(p.blue.hsla())
                            .child(SharedString::from(h.location())),
                    )
                    .child(
                        div()
                            .text_color(p.text_subtle.hsla())
                            .child(SharedString::from(h.change())),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_hunk(&hunk, window, cx)),
                    ),
            );
        }
        let listed = c.uncovered.len().min(BANNER_HUNKS);
        if c.uncovered_count() as usize > listed {
            links = links.child(
                div()
                    .text_size(m.text_label)
                    .text_color(p.text_subtle.hsla())
                    .child(SharedString::from(format!(
                        "and {} more",
                        c.uncovered_count() as usize - listed
                    ))),
            );
        }
        Some(
            div()
                .id("walkthrough-coverage")
                .test_support()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(16.))
                .py(px(10.))
                .rounded(px(14.))
                .bg(p.rose.alpha(0.12))
                .border_1()
                .border_color(p.rose.alpha(0.3))
                .text_size(m.text_sm)
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(12.))
                        .child(icon(Icon::CircleAlert, px(16.), p.rose.hsla()))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(300.))
                                .flex()
                                .flex_wrap()
                                .gap_x(px(4.))
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(p.rose.hsla())
                                        .child(SharedString::from(head)),
                                )
                                .child(
                                    div()
                                        .text_color(p.text_muted.hsla())
                                        .child(SharedString::from(body)),
                                ),
                        )
                        .child(ask),
                )
                .child(links)
                .into_any_element(),
        )
    }

    fn render_steps(&self, w: &Walkthrough, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let mut list = div()
            .id("walkthrough-steps")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(4.));
        for (ix, step) in w.steps.iter().enumerate() {
            let selected = ix == self.current;
            let state = super::step_state(w, step);
            let file = step
                .anchors
                .first()
                .map(|a| super::file_name(&a.anchor.path).to_owned())
                .unwrap_or_default();
            let worst = step.risks.iter().map(|r| r.severity).max();
            let (mark, mark_color) = match state {
                StepState::Understood => ("✓", p.sage.hsla()),
                StepState::Question => ("?", p.sand.hsla()),
                StepState::Unread => ("", p.text_subtle.hsla()),
            };
            list = list.child(
                div()
                    .id(("walkthrough-step", ix))
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(12.))
                    .cursor_pointer()
                    .border_1()
                    .map(|d| {
                        if selected {
                            d.bg(p.surface_selected.hsla())
                                .border_color(p.sage.hsla().opacity(0.45))
                        } else {
                            d.border_color(coqo_ui::gpui::transparent_black())
                                .hover(|d| d.bg(p.surface.hsla()))
                        }
                    })
                    .child(
                        div()
                            .flex_none()
                            .size(px(24.))
                            .rounded(px(8.))
                            .bg(p.surface_selected.hsla())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(m.text_sm)
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.text_muted.hsla())
                            .child((ix + 1).to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(step.title.clone())),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .text_size(m.text_xs)
                                    .text_color(p.text_subtle.hsla())
                                    .child(div().min_w_0().truncate().child(format!(
                                        "{} · {file}",
                                        super::kind_label(step.kind)
                                    )))
                                    .when_some(
                                        worst.filter(|s| *s > wtm::RiskSeverity::Low),
                                        |d, s| {
                                            let color = super::severity_color(s, cx).hsla();
                                            d.child(
                                                div()
                                                    .flex_none()
                                                    .px(px(5.))
                                                    .rounded(px(5.))
                                                    .bg(color.opacity(0.22))
                                                    .text_color(color)
                                                    .text_size(px(9.5))
                                                    .font_weight(FontWeight::BOLD)
                                                    .child(super::severity_label(s)),
                                            )
                                        },
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id(("walkthrough-step-state", ix))
                            .test_support()
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .text_color(mark_color)
                            .child(mark),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.go_to(ix, 0, true, window, cx)),
                    ),
            );
        }
        let criteria = (!w.acceptance_criteria.is_empty()).then(|| {
            let mut block = div()
                .id("walkthrough-criteria")
                .test_support()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(12.))
                .py(px(10.))
                .rounded(px(12.))
                .bg(p.surface.hsla())
                .text_size(m.text_sm)
                .child(
                    div()
                        .text_size(m.text_label)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text_subtle.hsla())
                        .child("ACCEPTANCE CRITERIA"),
                );
            for c in &w.acceptance_criteria {
                use coqo_protocol::methods::agents::CriterionStatus as S;
                let (glyph, color) = match c.status {
                    Some(S::Met) => (Icon::Check, p.sage.hsla()),
                    Some(S::Failed) => (Icon::CircleX, p.rose.hsla()),
                    Some(S::Pending) => (Icon::Circle, p.sand.hsla()),
                    Some(S::Unknown) | None => (Icon::CircleAlert, p.text_subtle.hsla()),
                };
                let steps = super::steps_list(&super::criterion_steps(w, &c.id));
                block = block.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .child(div().pt(px(2.)).child(icon(glyph, px(12.), color)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(p.blue.hsla())
                                                .child(SharedString::from(c.id.clone())),
                                        )
                                        .child(div().text_color(p.text_subtle.hsla()).child(
                                            format!(
                                                "{} · {steps}",
                                                super::criterion_label(c.status)
                                            ),
                                        )),
                                )
                                .child(
                                    div()
                                        .text_color(p.text_muted.hsla())
                                        .child(SharedString::from(c.text.clone())),
                                ),
                        ),
                );
            }
            block
        });
        div()
            .id("walkthrough-nav")
            .test_support()
            .flex_none()
            .w(px(320.))
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(12.))
            .rounded(px(16.))
            .bg(p.panel_raised.hsla())
            .text_size(m.text_md)
            .child(section_label("Steps · reading order", cx))
            .child(list)
            .children(criteria)
            .into_any_element()
    }

    fn render_main(&self, w: &Walkthrough, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let step = w.steps.get(self.current);
        let place = step
            .and_then(|s| s.anchors.get(self.anchor))
            .map(|a| format!("{}:{}", a.anchor.path, super::anchor_range_label(a)));
        let toggle = |id: &'static str,
                      label: &'static str,
                      on: bool,
                      mode: Mode,
                      cx: &mut Context<Self>| {
            div()
                .id(id)
                .test_support()
                .px(px(10.))
                .py(px(4.))
                .rounded(px(8.))
                .cursor_pointer()
                .text_size(m.text_sm)
                .map(|d| {
                    if on {
                        d.bg(p.surface_selected.hsla()).text_color(p.text.hsla())
                    } else {
                        d.text_color(p.text_muted.hsla())
                    }
                })
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| this.set_mode(mode, window, cx)))
        };
        let bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .px(px(16.))
            .py(px(10.))
            .child(
                div()
                    .text_size(m.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child(format!("Step {} of {}", self.current + 1, w.steps.len())),
            )
            .when_some(place, |d, place| {
                d.child(
                    div()
                        .id("walkthrough-place")
                        .test_support()
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(px(9.))
                        .bg(p.surface.hsla())
                        .font_family(m.font_mono.clone())
                        .text_size(px(12.5))
                        .child(place),
                )
            })
            .child(
                div()
                    .id("walkthrough-open-file")
                    .test_support()
                    .text_size(m.text_sm)
                    .text_color(p.blue.hsla())
                    .cursor_pointer()
                    .child("Open file")
                    .on_click(cx.listener(|this, _, window, cx| this.open_file(window, cx))),
            )
            .child(div().flex_1())
            .child(
                raised(div(), cx)
                    .flex()
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded(px(10.))
                    .child(toggle(
                        "walkthrough-mode-editor",
                        "Editor",
                        self.mode == Mode::Editor,
                        Mode::Editor,
                        cx,
                    ))
                    .child(toggle(
                        "walkthrough-mode-diff",
                        "Diff view",
                        self.mode == Mode::Diff,
                        Mode::Diff,
                        cx,
                    )),
            );
        let body = match self.mode {
            Mode::Editor => self
                .shown
                .as_ref()
                .and_then(|(root, path)| self.find_editor(*root, path))
                .map(|e| e.into_any_element()),
            Mode::Diff => self
                .diff
                .as_ref()
                .map(|(v, _)| v.clone().into_any_element()),
        };
        div()
            .id("walkthrough-main")
            .test_support()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(px(16.))
            .border_1()
            .border_color(p.border.hsla())
            .overflow_hidden()
            .child(bar)
            .child(div().flex_1().min_h_0().flex().flex_col().children(body))
            .into_any_element()
    }
}

impl TabItem for WalkthroughPlayer {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        self.walkthrough
            .as_ref()
            .map_or_else(|| "Walkthrough".into(), |w| w.title.clone().into())
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(key(&self.id).into())
    }

    fn save(&self, _: &App) -> Value {
        let step = self
            .walkthrough
            .as_ref()
            .and_then(|w| w.steps.get(self.current))
            .map(|s| s.id.clone());
        json!({
            "workspace": self.workspace.0,
            "id": self.id.0,
            "step": step,
            "mode": if self.mode == Mode::Diff { "diff" } else { "editor" },
        })
    }

    fn render_label(&self, _: bool, _: &mut Window, cx: &App) -> Option<AnyElement> {
        let t = cx.tokens();
        let p = &t.palette;
        let (title, progress) = match &self.walkthrough {
            Some(w) => (
                w.title.clone(),
                format!("{}/{}", super::understood_count(w), w.steps.len()),
            ),
            None => ("Walkthrough".to_owned(), String::new()),
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(icon(Icon::FileCode, px(12.), p.sage.hsla()))
                .child(div().max_w(px(220.)).truncate().child(title))
                .child(
                    div()
                        .text_size(t.metrics.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .child(progress),
                )
                .into_any_element(),
        )
    }

    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        Some(self.popover.focus_handle(cx))
    }
}

/// The tooltip of the "Coverage not checked" chip: why.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        coqo_ui::widgets::popover(cx)
            .px(px(10.))
            .py(px(6.))
            .text_size(t.metrics.text_sm)
            .text_color(t.palette.text.hsla())
            .font_family(t.metrics.font_ui.clone())
            .child(self.0.clone())
    }
}

impl Render for WalkthroughPlayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let root = div()
            .id("walkthrough-player")
            .test_support()
            .key_context(super::KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NextStep, window, cx| this.next(window, cx)))
            .on_action(cx.listener(|this, _: &PreviousStep, window, cx| this.previous(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleUnderstood, _, cx| this.toggle_understood(cx)))
            .on_action(cx.listener(|this, _: &ToggleDiff, window, cx| this.toggle_mode(window, cx)))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(10.))
            .font_family(m.font_ui.clone())
            .text_size(m.text_md)
            .text_color(p.text.hsla());
        let message = |text: String| {
            div()
                .id("walkthrough-message")
                .test_support()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(p.text_muted.hsla())
                .child(text)
        };
        if self.deleted {
            return root.child(message("This walkthrough was deleted.".to_owned()));
        }
        let Some(w) = self.walkthrough.clone() else {
            let text = match &self.error {
                Some(e) => format!("Couldn't load the walkthrough: {e}"),
                None => "Loading the walkthrough…".to_owned(),
            };
            return root.child(message(text));
        };
        let error = self.error.clone().map(|e| {
            div()
                .id("walkthrough-error")
                .test_support()
                .px(px(16.))
                .text_size(m.text_sm)
                .text_color(p.rose.hsla())
                .child(SharedString::from(e))
        });
        let coverage = self.render_coverage(&w, cx);
        root.child(self.render_header(&w, cx))
            .children(coverage)
            .children(error)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(10.))
                    .child(self.render_steps(&w, cx))
                    .child(self.render_main(&w, cx)),
            )
    }
}
