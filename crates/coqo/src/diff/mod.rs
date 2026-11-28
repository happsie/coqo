// SPDX-License-Identifier: Apache-2.0

//! The review surface of board 4 (PLAN §7.7, issue #93, ADR 0021): a file
//! list with "viewed" checkboxes, and one file's diff side by side or
//! unified, over any [`DiffSource`].
//!
//! - **Data:** a [`DiffSource`] lists the files and loads each one's
//!   [`FileDiff`] (with full context, so every fold can open). The view
//!   loads them all, so counts, hashes and "viewed" are known up front, and
//!   reloads when the source's `watch` says so.
//! - **Body:** [`coqo_ui::diff`]: syntax and word highlights, folds,
//!   selection, gutter markers and slots.
//! - **Viewed:** per workspace and source identity, keyed by the file's
//!   [content hash](patch::content_hash), so a mark clears itself when the
//!   change does ([`viewed`]).
//! - **Staging:** when the source can stage, each hunk has a gutter button,
//!   and a line selection (drag the line numbers) can be staged or unstaged
//!   from the file header.
//! - **Discard:** when the source can discard (the unstaged diff, #226), the
//!   selection's lines, or a hunk, can be thrown away too, after a
//!   confirmation that says it can't be undone.
//! - **Hooks for later views:** [`DiffOverlay`]s add gutter markers, slots
//!   under lines (comment threads, #94), tints and an "add" affordance;
//!   [`LineRef`] / [`LineRange`] address lines; [`DiffView::scroll_to`]
//!   reveals one (walkthrough steps, #100).

// The review API serves #94, #100 and the PR view; tests use all of it.
#![cfg_attr(not(test), allow(dead_code))]

pub mod patch;
pub mod source;
pub mod viewed;

use std::{collections::HashMap, rc::Rc};

use coqo_protocol::methods::git::{FileChange, FileDiff, LineSelection};
use coqo_ui::{
    ActiveTokens as _,
    diff::{
        DiffBody, DiffMode, DiffViewer, DiffViewerEvent, GutterMarker, LineRange, LineRef,
        LineTint, Patch, Slot,
    },
    gpui::{
        AnyElement, App, AppContext as _, Context, ElementId, EventEmitter, FocusHandle,
        FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window, div,
        px,
    },
    widgets::{FluentBuilder as _, Icon, TestSupportExt as _, icon, raised, section_label},
};

pub use source::{DiffSource, SourceFile, Staging};

use crate::{files::glyph, workspace::Workspaces};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    diff_review,
    [
        /// Scroll to the next run of changes.
        NextHunk,
        /// Scroll to the previous run of changes.
        PreviousHunk,
        /// Show the next file.
        NextFile,
        /// Show the previous file.
        PreviousFile,
        /// Mark the shown file viewed, or not.
        ToggleViewed,
        /// Switch between side by side and unified.
        ToggleMode,
    ]
);

const KEY_CONTEXT: &str = "DiffReview";

/// Changed lines of a file the user asked to discard, before the yes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDiscard {
    pub path: String,
    pub lines: LineSelection,
}

/// Bind the review keys. Plain letters: the diff is read-only, and
/// [`keys::binding`] keeps them out of a text field inside it (a comment).
pub fn init(cx: &mut App) {
    use crate::commands::Command;
    let command = |id: &'static str,
                   title: &'static str,
                   key: &'static str,
                   action: Box<dyn coqo_ui::gpui::Action>| {
        Command::new_boxed(id, title, "Diff", action)
            .key(key)
            .context(KEY_CONTEXT)
    };
    crate::commands::register(
        cx,
        [
            command("diff.next_hunk", "Next change", "j", Box::new(NextHunk)),
            command(
                "diff.previous_hunk",
                "Previous change",
                "k",
                Box::new(PreviousHunk),
            ),
            command("diff.next_file", "Next file", "shift-j", Box::new(NextFile)),
            command(
                "diff.previous_file",
                "Previous file",
                "shift-k",
                Box::new(PreviousFile),
            ),
            command(
                "diff.toggle_viewed",
                "Mark file viewed",
                "v",
                Box::new(ToggleViewed),
            ),
            command(
                "diff.toggle_mode",
                "Toggle side by side / unified",
                "m",
                Box::new(ToggleMode),
            ),
        ],
    );
}

/// Extra content for a [`DiffView`]'s body, for the file shown. Comment
/// threads (#94) and walkthrough steps (#100) plug in here.
///
/// Markers share one gutter lane with the view's own staging buttons,
/// which come first: a line shows the first marker given for it. Ids must
/// be unique across overlays.
pub trait DiffOverlay: 'static {
    /// Markers in the gutter lane.
    fn markers(&self, _path: &str, _diff: &FileDiff, _cx: &App) -> Vec<GutterMarker> {
        Vec::new()
    }

    /// Draw one of this overlay's markers (a small focusable control).
    fn render_marker(
        &self,
        _marker: &GutterMarker,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        div().into_any_element()
    }

    /// Content under lines or the file header.
    fn slots(&self, _path: &str, _diff: &FileDiff, _cx: &App) -> Vec<Slot> {
        Vec::new()
    }

    /// Draw one of this overlay's slots.
    fn render_slot(&self, _slot: &Slot, _window: &mut Window, _cx: &mut App) -> AnyElement {
        div().into_any_element()
    }

    /// Backgrounds over lines.
    fn tints(&self, _path: &str, _diff: &FileDiff, _cx: &App) -> Vec<LineTint> {
        Vec::new()
    }

    /// Whether lines can be "added to" (a "+" beside the hovered line and
    /// the selection); [`on_add`](Self::on_add) then gets the lines. The
    /// first overlay that wants it gets it.
    fn wants_add(&self) -> bool {
        false
    }

    fn on_add(&self, _range: &LineRange, _window: &mut Window, _cx: &mut App) {}

    /// A phrase at the end of the header's summary line, after the totals
    /// ("made by Claude in \"Volume discount\"", #286).
    fn summary(&self, _cx: &App) -> Option<SharedString> {
        None
    }

    /// Content at the right of the header card, after the layout toggle (a
    /// review's pending count and its send button).
    fn header(&self, _window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        None
    }

    /// A badge on a file's row in the file list, before its counts
    /// ("1 pending").
    fn file_badge(&self, _path: &str, _cx: &App) -> Option<AnyElement> {
        None
    }

    /// A note at the bottom of the file list, above the key hints.
    fn files_note(&self, _cx: &App) -> Option<AnyElement> {
        None
    }

    /// A line for the hint under the body ("Press + beside a line to
    /// comment").
    fn hint(&self, _cx: &App) -> Option<SharedString> {
        None
    }
}

/// What a [`DiffView`] reports.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffViewEvent {
    /// Another file is shown.
    FileSelected(String),
    /// The selected lines changed.
    SelectionChanged(Option<LineRange>),
    /// A file was marked viewed or not.
    ViewedChanged,
    /// The files were (re)loaded from the source.
    Loaded,
}

/// Where the file list is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Loading,
    Ready,
    Failed(String),
}

/// A loaded file.
#[derive(Clone, Debug, PartialEq)]
pub enum Loaded {
    Diff {
        diff: FileDiff,
        hash: String,
        hunks: Vec<patch::ReviewHunk>,
    },
    Failed(String),
}

impl Loaded {
    fn new(diff: FileDiff) -> Self {
        Loaded::Diff {
            hash: patch::content_hash(&diff),
            hunks: patch::hunks(&diff),
            diff,
        }
    }

    fn hash(&self) -> Option<&str> {
        match self {
            Loaded::Diff { hash, .. } => Some(hash),
            Loaded::Failed(_) => None,
        }
    }
}

/// The review surface. See the module docs.
pub struct DiffView {
    source: Rc<dyn DiffSource>,
    workspaces: Option<WeakEntity<Workspaces>>,
    viewer: DiffViewer,
    status: Status,
    files: Vec<SourceFile>,
    loaded: HashMap<String, Loaded>,
    selected: Option<String>,
    /// The path and hash installed in the viewer.
    shown: Option<(String, String)>,
    viewed: viewed::Viewed,
    overlays: Vec<Rc<dyn DiffOverlay>>,
    selection: Option<LineRange>,
    /// A failed stage, shown until dismissed.
    error: Option<String>,
    /// Changed lines of the shown file waiting for a yes to be discarded.
    pending_discard: Option<PendingDiscard>,
    staging: bool,
    /// The hunk to reveal after the next reload (the one after a staged hunk).
    reveal_hunk: Option<usize>,
    focus: FocusHandle,
    _loading: Option<Task<()>>,
    _staging: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for DiffView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffView")
            .field("identity", &self.source.identity())
            .field("status", &self.status)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    /// A view over `source`, showing `select` (or the first file) once
    /// loaded.
    pub fn new(
        source: Rc<dyn DiffSource>,
        select: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspaces = Workspaces::for_window(window, cx);
        let (mode, viewed) = workspaces
            .as_ref()
            .map(|w| {
                let w = w.read(cx);
                (
                    w.state(viewed::MODE_KEY)
                        .and_then(|v| v.as_str())
                        .and_then(DiffMode::parse),
                    viewed::Viewed::from_value(w.state(&viewed::viewed_key(&source.identity()))),
                )
            })
            .unwrap_or_default();
        let viewer = DiffViewer::new(mode.unwrap_or_default(), cx);
        let mut subscriptions = vec![viewer.subscribe(cx, |this, event, cx| {
            if let DiffViewerEvent::SelectionChanged(range) = event {
                this.selection = range.clone();
                cx.emit(DiffViewEvent::SelectionChanged(range.clone()));
                cx.notify();
            }
        })];
        let weak = cx.entity().downgrade();
        if let Some(watch) = source.watch(
            Rc::new(move |cx| {
                weak.update(cx, |this, cx| this.refresh(cx)).ok();
            }),
            window,
            cx,
        ) {
            subscriptions.push(watch);
        }
        let mut this = Self {
            source,
            workspaces: workspaces.map(|w| w.downgrade()),
            viewer,
            status: Status::Loading,
            files: Vec::new(),
            loaded: HashMap::new(),
            selected: select,
            shown: None,
            viewed,
            overlays: Vec::new(),
            selection: None,
            error: None,
            staging: false,
            pending_discard: None,
            reveal_hunk: None,
            focus: cx.focus_handle(),
            _loading: None,
            _staging: None,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    /// The source shown.
    pub fn source(&self) -> &Rc<dyn DiffSource> {
        &self.source
    }

    /// The diff body's state, for views that drive it directly.
    pub fn viewer(&self) -> &DiffViewer {
        &self.viewer
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The files, in list order.
    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }

    /// A file's diff, once loaded.
    pub fn file_diff(&self, path: &str) -> Option<&FileDiff> {
        match self.loaded.get(path)? {
            Loaded::Diff { diff, .. } => Some(diff),
            Loaded::Failed(_) => None,
        }
    }

    /// The hunks of the shown file.
    pub fn hunks(&self) -> &[patch::ReviewHunk] {
        match self.selected.as_ref().and_then(|p| self.loaded.get(p)) {
            Some(Loaded::Diff { hunks, .. }) => hunks,
            _ => &[],
        }
    }

    /// The shown file.
    pub fn selected_file(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected lines of the shown file.
    pub fn selection(&self) -> Option<&LineRange> {
        self.selection.as_ref()
    }

    /// A failed stage.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn mode(&self, cx: &App) -> DiffMode {
        self.viewer.mode(cx)
    }

    /// Add content to the body. See [`DiffOverlay`].
    pub fn add_overlay(&mut self, overlay: Rc<dyn DiffOverlay>, cx: &mut Context<Self>) {
        self.overlays.push(overlay);
        cx.notify();
    }

    /// Reload the file list and every file.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let files = self.source.files(cx);
        self._loading = Some(cx.spawn(async move |this, cx| {
            let files = files.await;
            let files = match files {
                Ok(files) => files,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.status = Status::Failed(err);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let Ok(calls) = this.update(cx, |this, cx| {
                files
                    .iter()
                    .map(|f| this.source.file(f, cx))
                    .collect::<Vec<_>>()
            }) else {
                return;
            };
            let diffs = futures::future::join_all(calls).await;
            this.update(cx, |this, cx| this.loaded(files, diffs, cx))
                .ok();
        }));
    }

    pub(crate) fn loaded(
        &mut self,
        files: Vec<SourceFile>,
        diffs: Vec<Result<FileDiff, String>>,
        cx: &mut Context<Self>,
    ) {
        let previous: Vec<String> = self.files.iter().map(|f| f.path.clone()).collect();
        self.loaded = files
            .iter()
            .zip(diffs)
            .map(|(f, d)| {
                let loaded = match d {
                    Ok(diff) => Loaded::new(diff),
                    Err(err) => Loaded::Failed(err),
                };
                (f.path.clone(), loaded)
            })
            .collect();
        self.files = files;
        self.status = Status::Ready;
        let forgot = self.viewed.retain_current(self.files.iter().map(|f| {
            (
                f.path.as_str(),
                self.loaded.get(&f.path).and_then(Loaded::hash),
            )
        }));
        if forgot {
            self.save_viewed(cx);
        }
        // Keep the shown file; if it went away, show its neighbour.
        let keep = self
            .selected
            .as_ref()
            .is_some_and(|p| self.files.iter().any(|f| &f.path == p));
        if !keep {
            let at = self
                .selected
                .as_ref()
                .and_then(|p| previous.iter().position(|q| q == p))
                .unwrap_or(0);
            self.selected = self
                .files
                .get(at.min(self.files.len().saturating_sub(1)))
                .map(|f| f.path.clone());
            if let Some(path) = &self.selected {
                cx.emit(DiffViewEvent::FileSelected(path.clone()));
            }
        }
        self.install(cx);
        cx.emit(DiffViewEvent::Loaded);
        cx.notify();
    }

    /// Put the shown file's diff in the body, if it changed.
    fn install(&mut self, cx: &mut Context<Self>) {
        let current = self
            .selected
            .as_ref()
            .and_then(|p| match self.loaded.get(p) {
                Some(Loaded::Diff { diff, hash, .. }) => Some((p.clone(), hash.clone(), diff)),
                _ => None,
            });
        let Some((path, hash, diff)) = current else {
            if self.shown.take().is_some() {
                self.viewer.set_patches(&[], cx).ok();
            }
            return;
        };
        if self.shown.as_ref() == Some(&(path.clone(), hash.clone())) {
            return;
        }
        let patch = Patch {
            text: patch::to_patch(diff),
            language: coqo_ui::editor::language_for_path(&path).map(SharedString::from),
        };
        if let Err(err) = self.viewer.set_patches(&[patch], cx) {
            self.loaded.insert(path, Loaded::Failed(err.to_string()));
            self.shown = None;
            return;
        }
        self.shown = Some((path, hash));
        self.selection = None;
        if let Some(ix) = self.reveal_hunk.take() {
            let hunks = self.hunks();
            if let Some(h) = hunks.get(ix.min(hunks.len().saturating_sub(1))) {
                let at = h.first.clone();
                self.viewer.scroll_to(&at, cx);
            }
        }
    }

    /// Show `path`.
    pub fn select_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(path) {
            return;
        }
        self.selected = Some(path.to_owned());
        self.reveal_hunk = None;
        self.pending_discard = None;
        self.install(cx);
        cx.emit(DiffViewEvent::FileSelected(path.to_owned()));
        cx.notify();
    }

    fn step_file(&mut self, by: isize, cx: &mut Context<Self>) {
        let at = self
            .selected
            .as_ref()
            .and_then(|p| self.files.iter().position(|f| &f.path == p));
        let next = match at {
            Some(at) => at
                .saturating_add_signed(by)
                .min(self.files.len().saturating_sub(1)),
            None => 0,
        };
        if let Some(path) = self.files.get(next).map(|f| f.path.clone()) {
            self.select_file(&path, cx);
        }
    }

    pub fn next_file(&mut self, cx: &mut Context<Self>) {
        self.step_file(1, cx);
    }

    pub fn previous_file(&mut self, cx: &mut Context<Self>) {
        self.step_file(-1, cx);
    }

    pub fn next_hunk(&mut self, cx: &mut Context<Self>) {
        self.viewer.next_change(cx);
    }

    pub fn previous_hunk(&mut self, cx: &mut Context<Self>) {
        self.viewer.previous_change(cx);
    }

    /// Show `at`, selecting its file and opening its fold.
    pub fn scroll_to(&mut self, at: &LineRef, cx: &mut Context<Self>) {
        self.select_file(&at.path, cx);
        self.viewer.scroll_to(at, cx);
    }

    /// Select lines of the shown file (or clear the selection).
    pub fn select_lines(&mut self, range: Option<&LineRange>, cx: &mut Context<Self>) {
        self.viewer.set_selection(range, cx);
        self.selection = self.viewer.selection(cx);
        cx.notify();
    }

    pub fn set_mode(&mut self, mode: DiffMode, cx: &mut Context<Self>) {
        if self.viewer.mode(cx) == mode {
            return;
        }
        self.viewer.set_mode(mode, cx);
        self.write_state(viewed::MODE_KEY, mode.as_str().into(), cx);
        cx.notify();
    }

    pub fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        let mode = self.viewer.mode(cx).toggled();
        self.set_mode(mode, cx);
    }

    /// Whether `path` is marked viewed with its current change.
    pub fn is_viewed(&self, path: &str) -> bool {
        self.loaded
            .get(path)
            .and_then(Loaded::hash)
            .is_some_and(|h| self.viewed.is_viewed(path, h))
    }

    /// Viewed files and all files.
    pub fn viewed_count(&self) -> (usize, usize) {
        let viewed = self
            .files
            .iter()
            .filter(|f| self.is_viewed(&f.path))
            .count();
        (viewed, self.files.len())
    }

    /// Mark `path` viewed (at its current change) or not.
    pub fn set_viewed(&mut self, path: &str, viewed: bool, cx: &mut Context<Self>) {
        let Some(hash) = self
            .loaded
            .get(path)
            .and_then(Loaded::hash)
            .map(str::to_owned)
        else {
            return;
        };
        if self.is_viewed(path) == viewed {
            return;
        }
        self.viewed.set(path, &hash, viewed);
        self.save_viewed(cx);
        cx.emit(DiffViewEvent::ViewedChanged);
        cx.notify();
    }

    pub fn toggle_viewed(&mut self, path: &str, cx: &mut Context<Self>) {
        let viewed = self.is_viewed(path);
        self.set_viewed(path, !viewed, cx);
    }

    fn save_viewed(&mut self, cx: &mut Context<Self>) {
        let key = viewed::viewed_key(&self.source.identity());
        self.write_state(&key, self.viewed.to_value(), cx);
    }

    fn write_state(&self, key: &str, value: coqo_ui::serde_json::Value, cx: &mut Context<Self>) {
        let Some(workspaces) = self.workspaces.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let write = workspaces.update(cx, |w, cx| w.set_state(key, value, cx));
        cx.background_spawn(async move {
            if let Err(err) = write.await {
                eprintln!("coqo: couldn't save the diff view's state: {err}");
            }
        })
        .detach();
    }

    /// Stage (or unstage) hunk `ix` of the shown file.
    pub fn stage_hunk(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let Some(Loaded::Diff { diff, hunks, .. }) = self.loaded.get(&path) else {
            return;
        };
        let Some(hunk) = hunks.get(ix) else {
            return;
        };
        // A file's only hunk, a binary file, or a file one side lacks goes
        // whole: `git add` / `git restore --staged` handle what a patch of
        // lines cannot (an added or deleted file).
        let whole = hunks.len() == 1 || diff.binary || diff.old.is_none() || diff.new.is_none();
        let lines = (!whole).then(|| hunk.lines.clone());
        self.reveal_hunk = Some(ix);
        self.run_stage(path, lines, cx);
    }

    /// Stage (or unstage) the changed lines of the selection.
    pub fn stage_selection(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(range)) = (self.selected.clone(), self.selection.clone()) else {
            return;
        };
        let Some(diff) = self.file_diff(&path) else {
            return;
        };
        let lines = patch::selection(diff, &range);
        if lines.removed.is_empty() && lines.added.is_empty() {
            self.error = Some("The selection has no changed lines".into());
            cx.notify();
            return;
        }
        self.run_stage(path, Some(lines), cx);
    }

    fn run_stage(
        &mut self,
        path: String,
        lines: Option<coqo_protocol::methods::git::LineSelection>,
        cx: &mut Context<Self>,
    ) {
        if self.staging || self.source.staging().is_none() {
            return;
        }
        self.staging = true;
        self.error = None;
        let call = self.source.stage(&path, lines, cx);
        self._staging = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.staging = false;
                if let Err(err) = result {
                    this.error = Some(err);
                    this.reveal_hunk = None;
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Whether a stage is in flight.
    pub fn is_staging(&self) -> bool {
        self.staging
    }

    /// The discard waiting for a yes, if any.
    pub fn pending_discard(&self) -> Option<&PendingDiscard> {
        self.pending_discard.as_ref()
    }

    /// Ask before discarding the selection's changed lines.
    pub fn request_discard_selection(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(range)) = (self.selected.clone(), self.selection.clone()) else {
            return;
        };
        let Some(diff) = self.file_diff(&path) else {
            return;
        };
        let lines = patch::selection(diff, &range);
        self.ask_discard(path, lines, cx);
    }

    /// Ask before discarding hunk `ix` of the shown file.
    pub fn request_discard_hunk(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let Some(Loaded::Diff { hunks, .. }) = self.loaded.get(&path) else {
            return;
        };
        let Some(hunk) = hunks.get(ix) else {
            return;
        };
        let lines = hunk.lines.clone();
        self.ask_discard(path, lines, cx);
    }

    fn ask_discard(&mut self, path: String, lines: LineSelection, cx: &mut Context<Self>) {
        if !self.source.can_discard() {
            return;
        }
        if lines.removed.is_empty() && lines.added.is_empty() {
            self.error = Some("The selection has no changed lines".into());
        } else {
            self.pending_discard = Some(PendingDiscard { path, lines });
        }
        cx.notify();
    }

    pub fn cancel_discard(&mut self, cx: &mut Context<Self>) {
        if self.pending_discard.take().is_some() {
            cx.notify();
        }
    }

    /// The user said yes: discard the lines.
    pub fn confirm_discard(&mut self, cx: &mut Context<Self>) {
        if self.staging {
            return;
        }
        let Some(PendingDiscard { path, lines }) = self.pending_discard.take() else {
            return;
        };
        self.staging = true;
        self.error = None;
        self.selection = None;
        let call = self.source.discard(&path, lines, cx);
        self._staging = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                this.staging = false;
                if let Err(err) = result {
                    this.error = Some(err);
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_discard(&self, pending: &PendingDiscard, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let n = pending.lines.removed.len() + pending.lines.added.len();
        let name = pending.path.rsplit('/').next().unwrap_or(&pending.path);
        let lines = if n == 1 { "line" } else { "lines" };
        let added = pending.lines.added.len();
        let removed = pending.lines.removed.len();
        let what = match (added, removed) {
            (_, 0) => format!(
                "{added} added {} go",
                if added == 1 { "line" } else { "lines" }
            ),
            (0, _) => format!(
                "{removed} removed {} come back",
                if removed == 1 { "line" } else { "lines" }
            ),
            _ => format!("{added} added lines go and {removed} removed lines come back"),
        };
        let button = |id: &'static str, label: String, fill: Option<coqo_ui::Rgb>, cx: &App| {
            let el = div()
                .px(px(12.))
                .py(px(5.))
                .rounded(m.radius_control)
                .text_size(px(12.))
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
        div()
            .id("diff-discard-confirm")
            .test_support()
            .occlude()
            .flex()
            .items_center()
            .gap(px(10.))
            .mx_3()
            .mb_2()
            .px_3()
            .py_2()
            .rounded(px(10.))
            .bg(p.surface.hsla())
            .border_1()
            .border_color(p.border_strong.hsla())
            .text_size(m.text_sm)
            .child(icon(Icon::Undo, px(14.), p.rose.hsla()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child(format!("Discard {n} changed {lines} in “{name}”?")),
                    )
                    .child(
                        div()
                            .text_size(m.text_xs)
                            .text_color(p.text_muted.hsla())
                            .child(format!(
                                "{what}: the file goes back to its staged version there. This can't be undone."
                            )),
                    ),
            )
            .child(
                button("diff-discard-cancel", "Cancel".into(), None, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_discard(cx))),
            )
            .child(
                button("diff-discard-ok", format!("Discard {n} {lines}"), Some(p.rose), cx)
                    .when(self.staging, |b| b.opacity(0.6))
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_discard(cx))),
            )
            .into_any_element()
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let extra: Vec<AnyElement> = self
            .overlays
            .iter()
            .filter_map(|o| o.header(window, cx))
            .collect();
        let summaries: Vec<SharedString> =
            self.overlays.iter().filter_map(|o| o.summary(cx)).collect();
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let (adds, dels) = self
            .files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        let n = self.files.len();
        let mode = self.viewer.mode(cx);
        let segment = |id: &'static str, label: &'static str, this_mode: DiffMode| {
            let base = div()
                .px_3()
                .py(px(6.))
                .rounded(px(9.))
                .text_size(px(12.))
                .cursor_pointer();
            let base = if mode == this_mode {
                raised(base, cx).text_color(p.text.hsla())
            } else {
                base.border_1()
                    .border_color(coqo_ui::gpui::transparent_black())
                    .text_color(p.text_muted.hsla())
            };
            base.id(id)
                .test_support()
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(this_mode, cx)))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .rounded(m.radius_panel)
            .bg(p.panel_raised.hsla())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child("Review changes"),
                    )
                    .child(
                        div()
                            .id("diff-summary")
                            .test_support()
                            .mt(px(2.))
                            .flex()
                            .gap(px(4.))
                            .text_size(px(12.))
                            .text_color(p.text_muted.hsla())
                            .child(format!(
                                "{} vs {} · {n} {} ·",
                                self.source.head_label(),
                                self.source.base_label(),
                                if n == 1 { "file" } else { "files" }
                            ))
                            .child(div().text_color(p.green.hsla()).child(format!("+{adds}")))
                            .child(div().text_color(p.rose.hsla()).child(format!("−{dels}")))
                            .children(summaries.into_iter().enumerate().map(|(i, s)| {
                                div()
                                    .id(("diff-summary-extra", i))
                                    .test_support()
                                    .min_w_0()
                                    .truncate()
                                    .child(format!("· {s}"))
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .p(px(3.))
                    .rounded(px(12.))
                    .bg(p.ground.hsla())
                    .child(segment("diff-mode-split", "Side by side", DiffMode::Split))
                    .child(segment("diff-mode-unified", "Unified", DiffMode::Unified)),
            )
            .children(extra)
    }

    fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let (viewed, total) = self.viewed_count();
        let rows: Vec<AnyElement> = self
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let selected = self.selected.as_deref() == Some(f.path.as_str());
                let is_viewed = self.is_viewed(&f.path);
                let (folder, name) = f.path.rsplit_once('/').unwrap_or(("", &f.path));
                // Names only, as on the board; a name the list has twice
                // gets its folder's name (`pricing/mod.rs`).
                let twice = self
                    .files
                    .iter()
                    .filter(|g| g.path.rsplit('/').next() == Some(name))
                    .count()
                    > 1;
                let parent = folder.rsplit('/').next().unwrap_or("");
                let label = if twice && !parent.is_empty() {
                    format!("{parent}/{name}")
                } else {
                    name.to_owned()
                };
                let badges: Vec<AnyElement> = self
                    .overlays
                    .iter()
                    .filter_map(|o| o.file_badge(&f.path, cx))
                    .collect();
                let path = f.path.clone();
                let toggle_path = f.path.clone();
                let letter = f.change.letter();
                let letter_color = match f.change {
                    FileChange::Added => p.green,
                    FileChange::Deleted => p.rose,
                    FileChange::Untracked => p.text_subtle,
                    FileChange::TypeChanged => p.lavender,
                    _ => p.sand,
                };
                let checkbox = div()
                    .id(("diff-viewed", i))
                    .test_support()
                    .size(px(16.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .map(|d| {
                        if is_viewed {
                            d.bg(p.sage.hsla())
                                .child(icon(Icon::Check, px(12.), p.ground.hsla()))
                        } else {
                            d.border_1().border_color(p.border_strong.hsla())
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_viewed(&toggle_path, cx);
                    }));
                let stats = if f.binary {
                    div().text_color(p.text_subtle.hsla()).child("binary")
                } else {
                    div()
                        .flex()
                        .gap(px(4.))
                        .when(f.additions > 0, |d| {
                            d.child(
                                div()
                                    .text_color(p.green.hsla())
                                    .child(format!("+{}", f.additions)),
                            )
                        })
                        .when(f.deletions > 0, |d| {
                            d.child(
                                div()
                                    .text_color(p.rose.hsla())
                                    .child(format!("−{}", f.deletions)),
                            )
                        })
                };
                div()
                    .id(("diff-file", i))
                    .test_support()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(11.))
                    .text_size(px(13.))
                    .cursor_pointer()
                    .map(|d| {
                        if selected {
                            d.bg(p.surface_selected.hsla())
                                .border_1()
                                .border_color(p.sage.alpha(0.4))
                        } else {
                            d.border_1()
                                .border_color(coqo_ui::gpui::transparent_black())
                                .hover(|d| d.bg(p.surface.hsla()))
                        }
                    })
                    .child(checkbox)
                    .child(glyph::render(name, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(if is_viewed {
                                p.text_muted.hsla()
                            } else {
                                p.text.hsla()
                            })
                            .child(label),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .children(badges)
                            .child(stats)
                            .child(
                                div()
                                    .w(px(10.))
                                    .font_family(m.font_mono.clone())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(letter_color.hsla())
                                    .child(letter.to_string()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_file(&path, cx);
                        window.focus(&this.viewer.focus_handle(cx), cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let notes: Vec<AnyElement> = self
            .overlays
            .iter()
            .filter_map(|o| o.files_note(cx))
            .collect();
        div()
            .id("diff-files")
            .test_support()
            .w(px(280.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p_3()
            .rounded(m.radius_panel)
            .bg(p.panel_raised.hsla())
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .pr_2()
                    .child(section_label("Files", cx))
                    .child(
                        div()
                            .id("diff-viewed-count")
                            .test_support()
                            .text_size(m.text_label)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_subtle.hsla())
                            .child(format!("{viewed} of {total} viewed")),
                    ),
            )
            .child(
                div()
                    .id("diff-file-list")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .children(notes)
    }

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let message = |id: &'static str, text: String, color: coqo_ui::Rgb| {
            div()
                .id(id)
                .test_support()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(m.text_sm)
                .text_color(color.hsla())
                .child(text)
                .into_any_element()
        };
        let body = match &self.status {
            Status::Loading => message("diff-loading", "Loading…".into(), p.text_subtle),
            Status::Failed(err) => message("diff-failed", err.clone(), p.rose),
            Status::Ready if self.files.is_empty() => {
                message("diff-empty", "No changes".into(), p.text_subtle)
            }
            Status::Ready => match self.selected.as_ref().and_then(|s| self.loaded.get(s)) {
                None => message("diff-loading", "Loading…".into(), p.text_subtle),
                Some(Loaded::Failed(err)) => message("diff-failed", err.clone(), p.rose),
                Some(Loaded::Diff { diff, .. }) if diff.binary => {
                    message("diff-binary", "Binary file differs".into(), p.text_subtle)
                }
                Some(Loaded::Diff { diff, .. }) if diff.hunks.is_empty() => {
                    message("diff-no-lines", "No line changes".into(), p.text_subtle)
                }
                Some(Loaded::Diff { diff, .. }) => self.render_body(diff, window, cx),
            },
        };
        let file = self
            .selected
            .as_ref()
            .and_then(|s| self.files.iter().find(|f| &f.path == s));
        let header = file.map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path);
            let staging = self.source.staging();
            let stage_lines = self
                .selection
                .as_ref()
                .zip(self.file_diff(&f.path))
                .map(|(r, d)| patch::selection(d, r))
                .filter(|s| !s.removed.is_empty() || !s.added.is_empty());
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .px_4()
                .py_3()
                .text_size(m.text_md)
                .child(glyph::render(name, cx))
                .child(
                    div()
                        .id("diff-path")
                        .test_support()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text.hsla())
                        .child(match &f.orig_path {
                            Some(orig) => format!("{orig} → {}", f.path),
                            None => f.path.clone(),
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .gap(px(4.))
                        .text_size(px(12.))
                        .child(
                            div()
                                .text_color(p.green.hsla())
                                .child(format!("+{}", f.additions)),
                        )
                        .child(
                            div()
                                .text_color(p.rose.hsla())
                                .child(format!("−{}", f.deletions)),
                        ),
                )
                .child(div().flex_1())
                .children(
                    stage_lines
                        .clone()
                        .filter(|_| self.source.can_discard())
                        .map(|lines| {
                            let n = lines.removed.len() + lines.added.len();
                            raised(div(), cx)
                                .id("diff-discard-lines")
                                .test_support()
                                .flex_none()
                                .px_3()
                                .py(px(5.))
                                .rounded(px(9.))
                                .text_size(px(12.))
                                .text_color(p.rose.hsla())
                                .cursor_pointer()
                                .child(format!(
                                    "Discard {n} {}…",
                                    if n == 1 { "line" } else { "lines" }
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.request_discard_selection(cx);
                                }))
                        }),
                )
                .children(staging.zip(stage_lines).map(|(staging, lines)| {
                    let n = lines.removed.len() + lines.added.len();
                    let verb = match staging {
                        Staging::Stage => "Stage",
                        Staging::Unstage => "Unstage",
                    };
                    raised(div(), cx)
                        .id("diff-stage-lines")
                        .test_support()
                        .flex_none()
                        .px_3()
                        .py(px(5.))
                        .rounded(px(9.))
                        .text_size(px(12.))
                        .text_color(p.text.hsla())
                        .cursor_pointer()
                        .child(format!(
                            "{verb} {n} {}",
                            if n == 1 { "line" } else { "lines" }
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.stage_selection(cx)))
                }))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .child(
                            div()
                                .text_color(p.text_subtle.hsla())
                                .child(self.source.base_label()),
                        )
                        .child(div().text_color(p.text_subtle.hsla()).child("→"))
                        .child(
                            div()
                                .text_color(p.text_muted.hsla())
                                .child(self.source.head_label()),
                        ),
                )
        });
        let error = self.error.clone().map(|err| {
            div()
                .id("diff-error")
                .test_support()
                .mx_3()
                .mb_2()
                .px_3()
                .py_2()
                .rounded(px(10.))
                .bg(p.rose.alpha(0.12))
                .text_size(m.text_sm)
                .text_color(p.rose.hsla())
                .cursor_pointer()
                .child(err)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.error = None;
                    cx.notify();
                }))
        });
        let mut hints: Vec<SharedString> = Vec::new();
        if let Some(s) = self.source.staging() {
            let verb = match s {
                Staging::Stage => "stage",
                Staging::Unstage => "unstage",
            };
            hints.push(
                if self.source.can_discard() {
                    format!(
                        "Drag line numbers to select lines to {verb} or discard, or use a hunk's button to {verb}"
                    )
                } else {
                    format!("Drag line numbers to select lines, or use a hunk's button, to {verb}")
                }
                .into(),
            );
        }
        hints.extend(self.overlays.iter().filter_map(|o| o.hint(cx)));
        let hint = (file.is_some() && !hints.is_empty()).then(|| {
            div()
                .id("diff-hint")
                .test_support()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .mx_4()
                .my_2()
                .text_size(px(12.))
                .text_color(p.text_subtle.hsla())
                .child(div().flex_1().h(px(1.)).bg(p.surface.hsla()))
                .child(hints.join(" · "))
                .child(div().flex_1().h(px(1.)).bg(p.surface.hsla()))
        });
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .rounded(m.radius_panel)
            .bg(p.panel.hsla())
            .overflow_hidden()
            .children(header)
            .children(
                self.pending_discard
                    .as_ref()
                    .filter(|d| self.selected.as_ref() == Some(&d.path))
                    .map(|d| self.render_discard(d, cx)),
            )
            .children(error)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .mx_3()
                    .when(hint.is_none(), |d| d.mb_3())
                    .flex()
                    .flex_col()
                    .rounded(px(14.))
                    .border_1()
                    .border_color(p.border.hsla())
                    .overflow_hidden()
                    .child(body),
            )
            .children(hint)
            .into_any_element()
    }

    fn render_body(&self, diff: &FileDiff, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let path = diff.path.as_str();
        // Who draws each marker: the view's staging buttons, or an overlay.
        #[derive(Clone, Copy)]
        enum Owner {
            Stage(usize),
            Overlay(usize),
        }
        let mut owners: HashMap<ElementId, Owner> = HashMap::new();
        let mut markers = Vec::new();
        if let Some(staging) = self.source.staging() {
            let label = match staging {
                Staging::Stage => "Stage hunk",
                Staging::Unstage => "Unstage hunk",
            };
            for (i, h) in self.hunks().iter().enumerate() {
                let id = ElementId::from(("diff-stage-hunk", i));
                owners.insert(id.clone(), Owner::Stage(i));
                markers.push(GutterMarker {
                    id,
                    at: h.first.clone(),
                    label: label.into(),
                });
            }
        }
        let mut slots = Vec::new();
        let mut slot_owners: HashMap<ElementId, usize> = HashMap::new();
        let mut tints = Vec::new();
        for (i, overlay) in self.overlays.iter().enumerate() {
            for marker in overlay.markers(path, diff, cx) {
                owners.insert(marker.id.clone(), Owner::Overlay(i));
                markers.push(marker);
            }
            for slot in overlay.slots(path, diff, cx) {
                slot_owners.insert(slot.id.clone(), i);
                slots.push(slot);
            }
            tints.extend(overlay.tints(path, diff, cx));
        }
        let overlays: Rc<[Rc<dyn DiffOverlay>]> = self.overlays.clone().into();
        let view = cx.entity().downgrade();
        let staging = self.source.staging();
        let marker_overlays = overlays.clone();
        let mut body = DiffBody::new(&self.viewer)
            .size_full()
            .tints(tints)
            .markers(markers, move |marker, window, cx| {
                match owners.get(&marker.id).copied() {
                    Some(Owner::Stage(ix)) => {
                        stage_button(&marker.id, ix, staging, view.clone(), cx)
                    }
                    Some(Owner::Overlay(i)) => marker_overlays[i].render_marker(marker, window, cx),
                    None => div().into_any_element(),
                }
            });
        if !slots.is_empty() {
            let slot_overlays = overlays.clone();
            body = body.slots(slots, move |slot, window, cx| {
                match slot_owners.get(&slot.id) {
                    Some(i) => slot_overlays[*i].render_slot(slot, window, cx),
                    None => div().into_any_element(),
                }
            });
        }
        if let Some(adder) = overlays.iter().find(|o| o.wants_add()).cloned() {
            body = body.on_add(move |range, window, cx| adder.on_add(range, window, cx));
        }
        body.into_any_element()
    }
}

/// A hunk's stage (or unstage) button in the gutter lane.
fn stage_button(
    id: &ElementId,
    ix: usize,
    staging: Option<Staging>,
    view: WeakEntity<DiffView>,
    cx: &App,
) -> AnyElement {
    let p = &cx.tokens().palette;
    let sign = match staging {
        Some(Staging::Unstage) => "−",
        _ => "+",
    };
    div()
        .id(id.clone())
        .test_support()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(p.sage.hsla())
        .cursor_pointer()
        .hover(|d| d.bg(p.surface_strong.hsla()))
        .child(sign)
        .on_click(move |_, _, cx| {
            view.update(cx, |this, cx| this.stage_hunk(ix, cx)).ok();
        })
        .into_any_element()
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.selected.clone();
        div()
            .id("diff-view")
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| this.next_hunk(cx)))
            .on_action(cx.listener(|this, _: &PreviousHunk, _, cx| this.previous_hunk(cx)))
            .on_action(cx.listener(|this, _: &NextFile, _, cx| this.next_file(cx)))
            .on_action(cx.listener(|this, _: &PreviousFile, _, cx| this.previous_file(cx)))
            .on_action(cx.listener(move |this, _: &ToggleViewed, _, cx| {
                if let Some(path) = &path {
                    this.toggle_viewed(path, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleMode, _, cx| this.toggle_mode(cx)))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(10.))
            .child(self.render_header(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(10.))
                    .child(self.render_files(cx))
                    .child(self.render_main(window, cx)),
            )
    }
}

impl coqo_ui::gpui::Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests;
