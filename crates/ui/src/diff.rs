// SPDX-License-Identifier: Apache-2.0

//! The diff body: gpui-component's read-only, virtualized `Diff` (ADR 0002),
//! behind Coqo's own types (ADR 0021).
//!
//! - **Input:** unified patches ([`Patch`]), one or more files each, from
//!   any provider: `git/diff` (converted by the app), a GitHub pull request,
//!   a walkthrough.
//! - **Modes:** side by side ([`DiffMode::Split`]) and [`DiffMode::Unified`].
//! - **Emphasis:** syntax highlighting of both sides with the editor's
//!   highlighter and theme, and word-level highlights between paired lines.
//! - **Context:** unchanged runs fold to [`CONTEXT_LINES`] lines around each
//!   change, with disclosure rows that reveal more (all, or
//!   [`EXPANSION_LINES`] at a time). Feed patches made with full context so
//!   every fold can open.
//! - **Addressing:** a source line is a [`LineRef`] (path, [`Side`], 1-based
//!   line); a selection is a [`LineRange`]. They do not depend on folding or
//!   mode.
//! - **Hooks:** [`DiffBody`] takes gutter markers (one lane, one marker per
//!   line), slots (content under a line or a file header, such as comment
//!   threads), line tints and an "add" affordance on hovered or selected
//!   lines.
//!
//! [`DiffViewer`] is the retained state (create once per view);
//! [`DiffBody`] is the element built from it on every render.

use std::rc::Rc;

use gpui_kit::component::diff::{
    Diff, DiffAnnotation, DiffChangeIndicator, DiffDecoration, DiffEvent, DiffFile,
    DiffGutterMarker, DiffHunkSeparator, DiffInlineUnit, DiffLinePosition, DiffLineRange,
    DiffMode as KitMode, DiffSide, DiffState,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, ElementId, Entity, FocusHandle,
    Focusable as _, Hsla, IntoElement, RenderOnce, SharedString, StyleRefinement, Styled,
    Subscription, Window,
};

/// Unchanged lines kept around each change before the rest folds.
pub const CONTEXT_LINES: usize = 3;
/// Lines a partial disclosure reveals.
pub const EXPANSION_LINES: usize = 20;

/// How the two sides are laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DiffMode {
    /// Old and new side by side.
    #[default]
    Split,
    /// One column, removed lines before added ones.
    Unified,
}

impl DiffMode {
    /// The other mode.
    pub fn toggled(self) -> Self {
        match self {
            DiffMode::Split => DiffMode::Unified,
            DiffMode::Unified => DiffMode::Split,
        }
    }

    /// `"split"` or `"unified"`, for persisted settings.
    pub fn as_str(self) -> &'static str {
        match self {
            DiffMode::Split => "split",
            DiffMode::Unified => "unified",
        }
    }

    /// The inverse of [`as_str`](Self::as_str).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "split" => Some(DiffMode::Split),
            "unified" => Some(DiffMode::Unified),
            _ => None,
        }
    }

    fn kit(self) -> KitMode {
        match self {
            DiffMode::Split => KitMode::Split,
            DiffMode::Unified => KitMode::Unified,
        }
    }
}

/// Which version a line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    /// The base (`-` lines and the old line numbers).
    Old,
    /// The head (`+` lines and the new line numbers).
    New,
}

impl Side {
    fn kit(self) -> DiffSide {
        match self {
            Side::Old => DiffSide::Original,
            Side::New => DiffSide::Modified,
        }
    }

    fn from_kit(side: DiffSide) -> Self {
        match side {
            DiffSide::Original => Side::Old,
            DiffSide::Modified => Side::New,
        }
    }
}

/// One source line of one side of a file, independent of folding and mode.
/// Context lines exist on both sides; either address shows them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LineRef {
    /// The file's path, as in its patch's `+++` (or `---` for a deletion).
    pub path: SharedString,
    pub side: Side,
    /// 1-based.
    pub line: u32,
}

impl LineRef {
    pub fn new(path: impl Into<SharedString>, side: Side, line: u32) -> Self {
        Self {
            path: path.into(),
            side,
            line,
        }
    }

    fn kit(&self) -> DiffLinePosition {
        DiffLinePosition::new(self.path.clone(), self.side.kit(), self.line as usize)
    }

    fn from_kit(p: &DiffLinePosition) -> Self {
        Self::new(
            p.path().clone(),
            Side::from_kit(p.side()),
            u32::try_from(p.line()).unwrap_or(u32::MAX),
        )
    }
}

/// Selected lines of one file, from `start` to `end` inclusive in display
/// order. In split mode both ends are on one side; in unified mode the range
/// can start on an old line and end on a new one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LineRange {
    pub path: SharedString,
    pub start: (Side, u32),
    pub end: (Side, u32),
}

impl LineRange {
    /// A range of lines on one side.
    pub fn lines(path: impl Into<SharedString>, side: Side, start: u32, end: u32) -> Self {
        Self {
            path: path.into(),
            start: (side, start),
            end: (side, end),
        }
    }

    /// The first line.
    pub fn first(&self) -> LineRef {
        LineRef::new(self.path.clone(), self.start.0, self.start.1)
    }

    /// The last line.
    pub fn last(&self) -> LineRef {
        LineRef::new(self.path.clone(), self.end.0, self.end.1)
    }

    fn kit(&self) -> DiffLineRange {
        DiffLineRange::new(
            self.path.clone(),
            self.start.0.kit(),
            self.start.1 as usize,
            self.end.1 as usize,
        )
        .with_end_side(self.end.0.kit())
    }

    fn from_kit(r: &DiffLineRange) -> Self {
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Self {
            path: r.path().clone(),
            start: (Side::from_kit(r.side()), n(r.start())),
            end: (Side::from_kit(r.end_side()), n(r.end())),
        }
    }
}

/// One file's unified patch (`---`/`+++` headers and `@@` hunks; a
/// `diff --git` header is optional), with an optional highlighter language
/// id (`coqo-syntax`'s) that overrides detection by path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    pub text: String,
    pub language: Option<SharedString>,
}

/// A marker in the gutter's marker lane, one row high and wide. A line shows
/// the first marker given for it.
#[derive(Clone, Debug, PartialEq)]
pub struct GutterMarker {
    /// Stable and unique among the markers of one render.
    pub id: ElementId,
    pub at: LineRef,
    /// Accessible name, e.g. "Stage hunk" or "Step 3".
    pub label: SharedString,
}

/// Where a [`Slot`] goes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SlotTarget {
    /// Under a source line (in its column, in split mode).
    Line(LineRef),
    /// Under the file's header.
    File(SharedString),
}

/// Content under a line or a file header, such as a comment thread. Rows
/// re-measure when slots change, so content may change height freely.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// Stable for the slot's lifetime.
    pub id: ElementId,
    pub at: SlotTarget,
}

/// A background over a range of lines (a walkthrough step, a thread's
/// lines), drawn over the change tint.
#[derive(Clone, Debug, PartialEq)]
pub struct LineTint {
    pub range: LineRange,
    pub color: Hsla,
}

/// What the viewer reports.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffViewerEvent {
    /// The selected lines changed (gutter drag, shift-click, keyboard).
    SelectionChanged(Option<LineRange>),
    /// A selection gesture finished.
    SelectionEnded(LineRange),
}

/// A patch that could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchError(pub String);

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PatchError {}

/// The retained state of a diff body: files, mode, folds, selection and
/// scroll. Cheap to clone (a handle).
#[derive(Clone)]
pub struct DiffViewer {
    state: Entity<DiffState>,
}

impl std::fmt::Debug for DiffViewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffViewer").finish_non_exhaustive()
    }
}

impl DiffViewer {
    /// An empty viewer in `mode`, with word-level emphasis and
    /// [`CONTEXT_LINES`] of context.
    pub fn new(mode: DiffMode, cx: &mut App) -> Self {
        crate::editor::register_languages();
        let state = cx.new(|cx| {
            DiffState::new([], cx)
                .with_mode(mode.kit())
                .with_context_lines(Some(CONTEXT_LINES))
                .with_expansion_lines(EXPANSION_LINES)
                .with_inline_unit(Some(DiffInlineUnit::Word))
        });
        Self { state }
    }

    /// Show `patches`, replacing what was shown. Resets folds, selection and
    /// scroll; callers that refresh should do it only when the patch changed.
    pub fn set_patches(&self, patches: &[Patch], cx: &mut App) -> Result<(), PatchError> {
        let mut files = Vec::new();
        for patch in patches {
            let parsed = DiffFile::parse(&patch.text).map_err(|e| PatchError(e.to_string()))?;
            files.extend(parsed.into_iter().map(|f| match &patch.language {
                Some(lang) => f.with_language(lang.clone()),
                None => f,
            }));
        }
        self.state.update(cx, |s, cx| s.set_files(files, cx));
        Ok(())
    }

    /// The paths shown, in order.
    pub fn paths(&self, cx: &App) -> Vec<SharedString> {
        self.state
            .read(cx)
            .files()
            .iter()
            .map(|f| f.path().clone())
            .collect()
    }

    pub fn mode(&self, cx: &App) -> DiffMode {
        match self.state.read(cx).mode() {
            KitMode::Split => DiffMode::Split,
            _ => DiffMode::Unified,
        }
    }

    /// Change the layout, keeping the selection and a nearby row in view.
    pub fn set_mode(&self, mode: DiffMode, cx: &mut App) {
        self.state.update(cx, |s, cx| s.set_mode(mode.kit(), cx));
    }

    pub fn selection(&self, cx: &App) -> Option<LineRange> {
        self.state
            .read(cx)
            .selected_lines()
            .as_ref()
            .map(LineRange::from_kit)
    }

    /// Select lines (or clear the selection), as a gutter drag would.
    pub fn set_selection(&self, range: Option<&LineRange>, cx: &mut App) {
        let range = range.map(LineRange::kit);
        self.state
            .update(cx, |s, cx| s.set_selected_lines(range, cx));
    }

    /// Whether the patch has `at` (shown or folded).
    pub fn has_line(&self, at: &LineRef, cx: &App) -> bool {
        self.state.read(cx).has_line(&at.kit())
    }

    /// Scroll to `at`, opening its fold if it is folded.
    pub fn scroll_to(&self, at: &LineRef, cx: &mut App) {
        let at = at.kit();
        self.state.update(cx, |s, cx| s.scroll_to_line(at, cx));
    }

    /// Scroll to the header of `path`.
    pub fn scroll_to_file(&self, path: &str, cx: &mut App) {
        self.state.update(cx, |s, cx| s.scroll_to_file(path, cx));
    }

    /// Scroll to the next run of changes after the viewport (wrapping).
    pub fn next_change(&self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.next_change(cx));
    }

    /// Scroll to the previous run of changes before the viewport.
    pub fn previous_change(&self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.previous_change(cx));
    }

    /// Open every fold.
    pub fn expand_all(&self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.expand_unchanged(cx));
    }

    /// Close every fold again.
    pub fn collapse_all(&self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.collapse_unchanged(cx));
    }

    /// The focus of the code body (arrow keys scroll, `Ctrl C` copies).
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.read(cx).focus_handle(cx)
    }

    /// Call `f` with the viewer's events, for as long as the subscription
    /// lives.
    pub fn subscribe<V: 'static>(
        &self,
        cx: &mut Context<V>,
        mut f: impl FnMut(&mut V, &DiffViewerEvent, &mut Context<V>) + 'static,
    ) -> Subscription {
        cx.subscribe(&self.state, move |this, _, event: &DiffEvent, cx| {
            let event = match event {
                DiffEvent::SelectionChanged(range) => {
                    DiffViewerEvent::SelectionChanged(range.as_ref().map(LineRange::from_kit))
                }
                DiffEvent::SelectionEnded(range) => {
                    DiffViewerEvent::SelectionEnded(LineRange::from_kit(range))
                }
                _ => return,
            };
            f(this, &event, cx);
        })
    }
}

type MarkerRender = Rc<dyn Fn(&GutterMarker, &mut Window, &mut App) -> AnyElement>;
type SlotRender = Rc<dyn Fn(&Slot, &mut Window, &mut App) -> AnyElement>;
type RangeHandler = Rc<dyn Fn(&LineRange, &mut Window, &mut App)>;
type LineHandler = Rc<dyn Fn(&LineRef, &ClickEvent, &mut Window, &mut App)>;

/// The diff body element. It owns its scrolling: give it a bounded height.
#[derive(IntoElement)]
pub struct DiffBody {
    viewer: DiffViewer,
    style: StyleRefinement,
    headers: bool,
    markers: Vec<GutterMarker>,
    render_marker: Option<MarkerRender>,
    slots: Vec<Slot>,
    render_slot: Option<SlotRender>,
    tints: Vec<LineTint>,
    on_add: Option<RangeHandler>,
    on_line_click: Option<LineHandler>,
}

impl std::fmt::Debug for DiffBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffBody")
            .field("markers", &self.markers)
            .field("slots", &self.slots)
            .finish_non_exhaustive()
    }
}

impl DiffBody {
    pub fn new(viewer: &DiffViewer) -> Self {
        Self {
            viewer: viewer.clone(),
            style: StyleRefinement::default(),
            headers: false,
            markers: Vec::new(),
            render_marker: None,
            slots: Vec::new(),
            render_slot: None,
            tints: Vec::new(),
            on_add: None,
            on_line_click: None,
        }
    }

    /// Show the component's own header above each file (path, counts,
    /// collapse). Off by default: Coqo's views draw their own.
    pub fn file_headers(mut self, show: bool) -> Self {
        self.headers = show;
        self
    }

    /// Gutter markers and how to draw one (a small focusable control).
    pub fn markers(
        mut self,
        markers: Vec<GutterMarker>,
        render: impl Fn(&GutterMarker, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.markers = markers;
        self.render_marker = Some(Rc::new(render));
        self
    }

    /// Slots and how to draw one.
    pub fn slots(
        mut self,
        slots: Vec<Slot>,
        render: impl Fn(&Slot, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.slots = slots;
        self.render_slot = Some(Rc::new(render));
        self
    }

    /// Backgrounds over ranges of lines.
    pub fn tints(mut self, tints: Vec<LineTint>) -> Self {
        self.tints = tints;
        self
    }

    /// Show an add button beside the hovered line and at the end of the
    /// selection; `f` gets the selection, or the hovered line alone.
    pub fn on_add(mut self, f: impl Fn(&LineRange, &mut Window, &mut App) + 'static) -> Self {
        self.on_add = Some(Rc::new(f));
        self
    }

    /// A click on a line's code (not a text-selection drag).
    pub fn on_line_click(
        mut self,
        f: impl Fn(&LineRef, &ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_line_click = Some(Rc::new(f));
        self
    }
}

impl Styled for DiffBody {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for DiffBody {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let split = self.viewer.mode(cx) == DiffMode::Split;
        let mut diff = Diff::new(&self.viewer.state)
            .header_visible(self.headers)
            // Patches carry full context, so a hunk header would only say
            // `@@ -1,n +1,m @@`; show the hidden count when there is one.
            .hunk_separator(DiffHunkSeparator::LineInfo)
            .change_indicator(if split {
                DiffChangeIndicator::None
            } else {
                DiffChangeIndicator::Signs
            })
            .decorations(
                self.tints
                    .into_iter()
                    .map(|t| DiffDecoration::new(t.range.kit()).with_background(t.color)),
            );
        *diff.style() = self.style;
        if let Some(render) = self.render_marker {
            diff = diff
                .gutter_markers(
                    self.markers
                        .into_iter()
                        .map(|m| DiffGutterMarker::new(m.id, m.at.kit()).with_label(m.label)),
                )
                .render_gutter_marker(move |m, window, cx| {
                    let marker = GutterMarker {
                        id: m.id().clone(),
                        at: LineRef::from_kit(m.position()),
                        label: m.label().clone(),
                    };
                    render(&marker, window, cx)
                });
        }
        if let Some(render) = self.render_slot {
            diff = diff
                .annotations(self.slots.into_iter().map(|s| match s.at {
                    SlotTarget::Line(at) => DiffAnnotation::line(s.id, at.kit()),
                    SlotTarget::File(path) => DiffAnnotation::file(s.id, path),
                }))
                .render_annotation(move |a, window, cx| {
                    let slot = Slot {
                        id: a.id().clone(),
                        at: match a.position() {
                            Some(p) => SlotTarget::Line(LineRef::from_kit(p)),
                            None => SlotTarget::File(a.path().clone()),
                        },
                    };
                    render(&slot, window, cx)
                });
        }
        if let Some(f) = self.on_add {
            diff =
                diff.on_add_annotation(move |r, window, cx| f(&LineRange::from_kit(r), window, cx));
        }
        if let Some(f) = self.on_line_click {
            diff =
                diff.on_line_click(move |p, e, window, cx| f(&LineRef::from_kit(p), e, window, cx));
        }
        diff
    }
}

#[cfg(test)]
mod tests;
