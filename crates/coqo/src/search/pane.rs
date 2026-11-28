// SPDX-License-Identifier: Apache-2.0

//! The Search sidebar pane: a query with match case, whole word and regex
//! toggles, a replacement, include and exclude globs (behind "Filters"), and
//! the results grouped by file.
//!
//! - **Searching** starts [`DEBOUNCE`] after the last keystroke (at once on
//!   Enter or a toggle) and streams in: files appear sorted by path as the
//!   daemon finds them. A new query drops the old subscription, which
//!   cancels that search in the daemon.
//! - **Results:** a file row (chevron, glyph, name, folder, count) folds its
//!   matches; a match row shows the line with the match highlighted, and,
//!   with a replacement typed, struck through with the replacement after it.
//!   Clicking a match opens the file with the match selected. `F4` /
//!   `Shift+F4` step through the matches from anywhere.
//! - **Replacing:** "Replace all" asks first, with the number of matches
//!   and files; the buttons on a file or match row replace just that. Files
//!   open in a tab of this window take the edits as one undoable edit of the
//!   tab (`search/replace` holds them for it); the daemon edits other open
//!   buffers and writes files that are not open. The search runs again
//!   after.

use std::{collections::HashMap, ops::Range, time::Duration};

use coqo_protocol::methods::{
    buffer::BufferId,
    editor::{Position, Range as EditorRange},
    search::{
        self as sm, FileMatches, FileReplaced, ReplaceFile, ReplaceOutcome, ReplaceParams,
        ReplaceResult, RunParams, SearchEvent, SearchMatch, SearchQuery, SearchSummary,
    },
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    ActiveTokens as _,
    dock::{AnyTab, Dock},
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
        HighlightStyle, InteractiveElement as _, IntoElement, ParentElement as _, Render,
        StatefulInteractiveElement as _, StrikethroughStyle, Styled as _, StyledText, Subscription,
        Task, UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{
        FluentBuilder as _, Icon, TestSupportExt as _, icon, primary, raised, section_label,
    },
};

use super::{KEY_CONTEXT, ReplaceAll, ToggleCase, ToggleRegex, ToggleWord};
use crate::{
    daemon::{Daemon, DaemonError},
    files::{
        editor::{self as file_editor, FileEditor},
        glyph,
    },
    questions::navigate,
    workspace::{Workspaces, WorkspacesEvent},
};

/// How long after the last keystroke the search starts.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
const ROW_HEIGHT: f32 = 24.;
const MATCH_INDENT: f32 = 26.;

/// A file with matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileResult {
    pub root: RootId,
    pub path: String,
    pub matches: Vec<SearchMatch>,
    pub collapsed: bool,
}

/// A row of the result list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A file (index into the results).
    File(usize),
    /// A match: file index, match index.
    Match(usize, usize),
}

/// Where the search is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// No query.
    Idle,
    /// Results are streaming in.
    Running,
    /// Done.
    Done(SearchSummary),
    /// It failed (a bad regex, no daemon).
    Failed(String),
}

/// The selected match, by file so it survives results arriving.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Selected {
    root: RootId,
    path: String,
    index: usize,
}

/// A replace waiting for its confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub files: usize,
    pub matches: usize,
}

/// The Search pane's view.
pub struct SearchPane {
    dock: WeakEntity<Dock>,
    workspaces: Option<Entity<Workspaces>>,
    focus: FocusHandle,
    query: Entity<PromptInput>,
    replace: Entity<PromptInput>,
    include: Entity<PromptInput>,
    exclude: Entity<PromptInput>,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    filters_open: bool,
    results: Vec<FileResult>,
    rows: Vec<Row>,
    status: Status,
    selected: Option<Selected>,
    confirm: Option<Confirm>,
    note: Option<String>,
    replacing: bool,
    scroll: UniformListScrollHandle,
    _debounce: Option<Task<()>>,
    _run: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for SearchPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchPane")
            .field("status", &self.status)
            .field("results", &self.results.len())
            .finish_non_exhaustive()
    }
}

impl Focusable for SearchPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// `text` split on commas, trimmed, empty parts dropped.
fn globs(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A match's range for `FileEditor::reveal`.
fn editor_range(m: &SearchMatch) -> EditorRange {
    EditorRange::new(Position::new(m.line, m.start), Position::new(m.line, m.end))
}

impl SearchPane {
    pub fn new(dock: WeakEntity<Dock>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| PromptInput::new(placeholder, 1, 1, window, cx))
        };
        let query = field("Search", window, cx);
        let replace = field("Replace", window, cx);
        let include = field("Files to include (*.rs, src)", window, cx);
        let exclude = field("Files to exclude", window, cx);
        let mut subscriptions = Vec::new();
        for input in [&query, &include, &exclude] {
            subscriptions.push(cx.subscribe(input, |this, _, event, cx| match event {
                PromptInputEvent::Changed => this.schedule(cx),
                PromptInputEvent::Submit => this.run(cx),
                _ => {}
            }));
        }
        subscriptions.push(
            cx.subscribe_in(&replace, window, |this, _, event, window, cx| match event {
                PromptInputEvent::Changed => cx.notify(),
                PromptInputEvent::Submit => this.ask_replace_all(window, cx),
                _ => {}
            }),
        );
        let workspaces = Workspaces::for_window(window, cx);
        if let Some(workspaces) = &workspaces {
            subscriptions.push(cx.subscribe(workspaces, |this, _, event, cx| {
                if *event == WorkspacesEvent::CurrentChanged {
                    this.run(cx);
                }
            }));
        }
        Self {
            dock,
            workspaces,
            focus: cx.focus_handle(),
            query,
            replace,
            include,
            exclude,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            filters_open: false,
            results: Vec::new(),
            rows: Vec::new(),
            status: Status::Idle,
            selected: None,
            confirm: None,
            note: None,
            replacing: false,
            scroll: UniformListScrollHandle::new(),
            _debounce: None,
            _run: None,
            _subscriptions: subscriptions,
        }
    }

    // ---- accessors (tests and the shell) -------------------------------------

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn query_input(&self) -> &Entity<PromptInput> {
        &self.query
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn replace_input(&self) -> &Entity<PromptInput> {
        &self.replace
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn results(&self) -> &[FileResult] {
        &self.results
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn status(&self) -> &Status {
        &self.status
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn confirm(&self) -> Option<Confirm> {
        self.confirm
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_replacing(&self) -> bool {
        self.replacing
    }

    /// The selected match: file index and match index.
    pub fn selected(&self) -> Option<(usize, usize)> {
        let s = self.selected.as_ref()?;
        let file = self
            .results
            .iter()
            .position(|f| f.root == s.root && f.path == s.path)?;
        (s.index < self.results[file].matches.len()).then_some((file, s.index))
    }

    /// Show `results` and `status` without a daemon, for screenshots.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_for_screenshot(
        &mut self,
        query: &str,
        replacement: &str,
        results: Vec<FileResult>,
        status: Status,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.query
            .update(cx, |q, cx| q.set_text(query.to_owned(), window, cx));
        self.replace
            .update(cx, |r, cx| r.set_text(replacement.to_owned(), window, cx));
        self._debounce = None;
        self.results = results;
        self.status = status;
        self.selected = self.results.first().map(|f| Selected {
            root: f.root,
            path: f.path.clone(),
            index: 0,
        });
        self.rebuild_rows();
        cx.notify();
    }

    /// Focus the query with its text selected, starting from `text` if
    /// given (the editor's selection).
    pub fn focus_query(
        &mut self,
        text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = text.filter(|t| !t.contains('\n') && t.len() <= 200) {
            if text != self.query.read(cx).text(cx) {
                self.query.update(cx, |q, cx| q.set_text(text, window, cx));
                self.run(cx);
            }
        }
        let handle = self.query.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    // ---- searching -------------------------------------------------------------

    fn workspace(&self, cx: &App) -> Option<WorkspaceId> {
        self.workspaces
            .as_ref()?
            .read(cx)
            .current()
            .map(|w| w.id.clone())
    }

    fn search_query(&self, cx: &App) -> SearchQuery {
        SearchQuery {
            pattern: self.query.read(cx).text(cx),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            include: globs(&self.include.read(cx).text(cx)),
            exclude: globs(&self.exclude.read(cx).text(cx)),
        }
    }

    /// Search after [`DEBOUNCE`].
    fn schedule(&mut self, cx: &mut Context<Self>) {
        self.note = None;
        self.confirm = None;
        self._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, cx| this.run(cx)).ok();
        }));
        cx.notify();
    }

    /// Search now, dropping (so cancelling) the search in progress.
    pub fn run(&mut self, cx: &mut Context<Self>) {
        self._debounce = None;
        self._run = None;
        self.confirm = None;
        self.results.clear();
        self.rows.clear();
        self.selected = None;
        let query = self.search_query(cx);
        if query.pattern.is_empty() {
            self.status = Status::Idle;
            cx.notify();
            return;
        }
        let Some(workspace) = self.workspace(cx) else {
            self.status = Status::Failed("No folder open".into());
            cx.notify();
            return;
        };
        let Some(daemon) = Daemon::global(cx) else {
            self.status = Status::Failed("Not connected to coqod".into());
            cx.notify();
            return;
        };
        let subscribe = daemon.read(cx).subscribe::<sm::Run>(RunParams {
            workspace,
            query,
            roots: None,
            max_matches: None,
        });
        self.status = Status::Running;
        self._run = Some(cx.spawn(async move |this, cx| {
            let mut events = match subscribe.await {
                Ok(events) => events,
                Err(e) => {
                    this.update(cx, |this, cx| this.failed(&e, cx)).ok();
                    return;
                }
            };
            while let Some(event) = events.next().await {
                let alive = this
                    .update(cx, |this, cx| match event {
                        Ok(SearchEvent::Files { files }) => this.add(files, cx),
                        Ok(SearchEvent::Done(summary)) => {
                            this.status = Status::Done(summary);
                            cx.notify();
                        }
                        Err(e) => this.failed(&e, cx),
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn failed(&mut self, e: &DaemonError, cx: &mut Context<Self>) {
        self.status = Status::Failed(e.to_string());
        cx.notify();
    }

    /// Insert `files`, keeping the list sorted by root and path.
    fn add(&mut self, files: Vec<FileMatches>, cx: &mut Context<Self>) {
        for f in files {
            let key = (f.root, f.path.as_str());
            let at = self
                .results
                .binary_search_by(|r| (r.root, r.path.as_str()).cmp(&key))
                .unwrap_or_else(|i| i);
            self.results.insert(
                at,
                FileResult {
                    root: f.root,
                    path: f.path,
                    matches: f.matches,
                    collapsed: false,
                },
            );
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (i, f) in self.results.iter().enumerate() {
            self.rows.push(Row::File(i));
            if !f.collapsed {
                self.rows
                    .extend((0..f.matches.len()).map(|j| Row::Match(i, j)));
            }
        }
    }

    fn total_matches(&self) -> usize {
        self.results.iter().map(|f| f.matches.len()).sum()
    }

    fn toggle(&mut self, which: fn(&mut Self) -> &mut bool, cx: &mut Context<Self>) {
        let flag = which(self);
        *flag = !*flag;
        self.run(cx);
    }

    fn toggle_case(&mut self, _: &ToggleCase, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle(|s| &mut s.case_sensitive, cx);
    }

    fn toggle_word(&mut self, _: &ToggleWord, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle(|s| &mut s.whole_word, cx);
    }

    fn toggle_regex(&mut self, _: &ToggleRegex, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle(|s| &mut s.regex, cx);
    }

    fn toggle_file(&mut self, file: usize, cx: &mut Context<Self>) {
        if let Some(f) = self.results.get_mut(file) {
            f.collapsed = !f.collapsed;
            self.rebuild_rows();
            cx.notify();
        }
    }

    // ---- opening ---------------------------------------------------------------

    /// Open match `index` of file `file`, selected in its editor.
    pub fn open(&mut self, file: usize, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(dock), Some(workspace)) = (self.dock.upgrade(), self.workspace(cx)) else {
            return;
        };
        let Some(f) = self.results.get(file) else {
            return;
        };
        let Some(m) = f.matches.get(index) else {
            return;
        };
        let (root, path, range) = (f.root, f.path.clone(), editor_range(m));
        if f.collapsed {
            self.results[file].collapsed = false;
            self.rebuild_rows();
        }
        self.selected = Some(Selected {
            root,
            path: path.clone(),
            index,
        });
        if let Some(row) = self.rows.iter().position(|r| *r == Row::Match(file, index)) {
            self.scroll
                .scroll_to_item(row, coqo_ui::gpui::ScrollStrategy::Center);
        }
        cx.notify();
        if let Some(tab) = navigate::open_file(&dock, &workspace, root, &path, window, cx) {
            tab.update(cx, |tab, cx| tab.reveal(Some(range), window, cx));
        }
    }

    /// Open the next (`forward`) or previous match, wrapping around.
    pub fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let all: Vec<(usize, usize)> = self
            .results
            .iter()
            .enumerate()
            .flat_map(|(i, f)| (0..f.matches.len()).map(move |j| (i, j)))
            .collect();
        if all.is_empty() {
            return;
        }
        let at = self
            .selected()
            .and_then(|s| all.iter().position(|a| *a == s));
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => all.len() - 1,
            (Some(i), true) => (i + 1) % all.len(),
            (Some(i), false) => (i + all.len() - 1) % all.len(),
        };
        let (file, index) = all[next];
        self.open(file, index, window, cx);
    }

    // ---- replacing -------------------------------------------------------------

    /// "Replace all": ask first.
    pub fn ask_replace_all(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.results.is_empty() || self.replacing {
            return;
        }
        self.confirm = Some(Confirm {
            files: self.results.len(),
            matches: self.total_matches(),
        });
        cx.notify();
    }

    fn replace_all_action(&mut self, _: &ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_replace_all(window, cx);
    }

    /// Replace every match (the confirmation said yes).
    pub fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        let files = self
            .results
            .iter()
            .map(|f| ReplaceFile {
                root: f.root,
                path: f.path.clone(),
                matches: None,
            })
            .collect();
        self.replace(files, window, cx);
    }

    /// Replace the matches of file `file`.
    pub fn replace_file(&mut self, file: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.results.get(file) else {
            return;
        };
        let files = vec![ReplaceFile {
            root: f.root,
            path: f.path.clone(),
            matches: None,
        }];
        self.replace(files, window, cx);
    }

    /// Replace match `index` of file `file`.
    pub fn replace_match(
        &mut self,
        file: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.results.get(file) else {
            return;
        };
        let Some(m) = f.matches.get(index) else {
            return;
        };
        let files = vec![ReplaceFile {
            root: f.root,
            path: f.path.clone(),
            matches: Some(vec![m.at()]),
        }];
        self.replace(files, window, cx);
    }

    /// This window's file tabs on `files`, by their buffer.
    fn tabs_for(
        &self,
        workspace: &WorkspaceId,
        files: &[ReplaceFile],
        cx: &App,
    ) -> HashMap<BufferId, Entity<FileEditor>> {
        let Some(dock) = self.dock.upgrade() else {
            return HashMap::new();
        };
        let dock = dock.read(cx);
        files
            .iter()
            .filter_map(|f| {
                let key = file_editor::key(workspace, f.root, &f.path);
                let id = dock.find_tab(file_editor::KIND, &key, cx)?;
                let tab = dock.tab(id).and_then(AnyTab::downcast::<FileEditor>)?;
                let buffer = tab.read(cx).buffer()?;
                Some((buffer, tab))
            })
            .collect()
    }

    fn replace(&mut self, files: Vec<ReplaceFile>, window: &mut Window, cx: &mut Context<Self>) {
        if files.is_empty() || self.replacing {
            return;
        }
        let (Some(workspace), Some(daemon)) = (self.workspace(cx), Daemon::global(cx)) else {
            return;
        };
        let tabs = self.tabs_for(&workspace, &files, cx);
        let params = ReplaceParams {
            workspace,
            query: self.search_query(cx),
            replacement: self.replace.read(cx).text(cx),
            files,
            hold: tabs.keys().copied().collect(),
        };
        let request = daemon.read(cx).request::<sm::Replace>(params.clone());
        self.replacing = true;
        self.note = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let retry = this
                .update_in(cx, |this, window, cx| {
                    this.replaced(result, &params, &tabs, window, cx)
                })
                .ok()
                .flatten();
            // Held edits a tab could not take (it moved on meanwhile): the
            // daemon applies them to the buffer instead.
            if let Some((retry, mut tally)) = retry {
                let request = this
                    .update(cx, |_, cx| {
                        Daemon::global(cx).map(|d| d.read(cx).request::<sm::Replace>(retry))
                    })
                    .ok()
                    .flatten();
                if let Some(request) = request {
                    match request.await {
                        Ok(r) => tally.add(&r.files),
                        Err(e) => tally.errors.push(e.to_string()),
                    }
                }
                this.update(cx, |this, cx| this.finish_replace(tally, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// The daemon answered: apply the held edits in their tabs. Returns a
    /// second request for the edits a tab could not take, if any (else the
    /// replace is finished here).
    fn replaced(
        &mut self,
        result: Result<ReplaceResult, DaemonError>,
        params: &ReplaceParams,
        tabs: &HashMap<BufferId, Entity<FileEditor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(ReplaceParams, Tally)> {
        let mut tally = Tally::default();
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                tally.errors.push(e.to_string());
                self.finish_replace(tally, cx);
                return None;
            }
        };
        let mut retry = Vec::new();
        for (file, asked) in result.files.iter().zip(&params.files) {
            if file.outcome != ReplaceOutcome::Held {
                tally.add(std::slice::from_ref(file));
                continue;
            }
            let applied = match (file.buffer.and_then(|b| tabs.get(&b)), file.rev) {
                (Some(tab), Some(rev)) => tab.update(cx, |tab, cx| {
                    tab.apply_replace(rev, &file.edits, window, cx)
                }),
                _ => false,
            };
            if applied {
                tally.files += 1;
                tally.matches += file.replaced as usize;
            } else {
                retry.push(asked.clone());
            }
        }
        if retry.is_empty() {
            self.finish_replace(tally, cx);
            return None;
        }
        let mut again = params.clone();
        again.files = retry;
        again.hold = Vec::new();
        Some((again, tally))
    }

    fn finish_replace(&mut self, tally: Tally, cx: &mut Context<Self>) {
        self.replacing = false;
        let mut note = match (tally.matches, tally.files) {
            (0, _) => "Nothing was replaced".to_owned(),
            (1, _) => "Replaced 1 match".to_owned(),
            (m, 1) => format!("Replaced {m} matches in 1 file"),
            (m, f) => format!("Replaced {m} matches in {f} files"),
        };
        if let Some(first) = tally.errors.first() {
            note.push_str(&format!(" · {} failed: {first}", tally.errors.len()));
        }
        self.run(cx);
        self.note = Some(note);
        cx.notify();
    }
}

/// What a replace did, across its requests.
#[derive(Debug, Default)]
struct Tally {
    files: usize,
    matches: usize,
    errors: Vec<String>,
}

impl Tally {
    fn add(&mut self, files: &[FileReplaced]) {
        for f in files {
            match f.outcome {
                ReplaceOutcome::Buffer | ReplaceOutcome::Written => {
                    self.files += 1;
                    self.matches += f.replaced as usize;
                }
                ReplaceOutcome::Failed => self.errors.push(format!(
                    "{}: {}",
                    f.path,
                    f.error.as_deref().unwrap_or("failed")
                )),
                ReplaceOutcome::Held | ReplaceOutcome::Unchanged => {}
            }
        }
    }
}

// ---- rendering ------------------------------------------------------------------

impl SearchPane {
    fn render_field(
        &self,
        id: &'static str,
        input: &Entity<PromptInput>,
        trailing: Option<AnyElement>,
        cx: &App,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        div()
            .id(id)
            .test_support()
            .flex()
            .items_center()
            .gap_1()
            .pl_2()
            .pr_1()
            .py(px(3.))
            .rounded(m.radius_field)
            .bg(p.surface.hsla())
            .border_1()
            .border_color(p.border.hsla())
            .text_size(m.text_sm)
            .child(div().flex_1().min_w_0().child(input.clone()))
            .children(trailing)
            .into_any_element()
    }

    fn render_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        flip: fn(&mut Self) -> &mut bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let chip = div()
            .min_w(px(22.))
            .h(px(20.))
            .px(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .font_family(m.font_mono.clone())
            .text_size(m.text_xs);
        let chip = if on {
            raised(chip, cx).text_color(p.text.hsla())
        } else {
            chip.text_color(p.text_subtle.hsla())
                .hover(|s| s.text_color(p.text_muted.hsla()))
        };
        chip.id(id)
            .test_support()
            .cursor_pointer()
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(flip, cx)))
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let label = match &self.status {
            Status::Idle => None,
            Status::Running if self.results.is_empty() => Some("Searching…".to_owned()),
            Status::Done(s) if s.matches == 0 => Some("No results".to_owned()),
            Status::Failed(_) => None,
            _ => {
                let (n, f) = (self.total_matches(), self.results.len());
                let r = if n == 1 { "result" } else { "results" };
                let fl = if f == 1 { "file" } else { "files" };
                Some(format!("{n} {r} in {f} {fl}"))
            }
        };
        let limit = matches!(&self.status, Status::Done(s) if s.limit_hit);
        let can_replace = !self.results.is_empty() && !self.replacing;
        let replace_all = can_replace.then(|| {
            div()
                .id("search-replace-all")
                .test_support()
                .px_2()
                .text_size(m.text_xs)
                .text_color(p.text_subtle.hsla())
                .cursor_pointer()
                .hover(|s| s.text_color(p.text.hsla()))
                .child("Replace all")
                .on_click(cx.listener(|this, _, window, cx| this.ask_replace_all(window, cx)))
        });
        div()
            .flex()
            .items_center()
            .children(label.map(|l| section_label(l, cx)))
            .when(limit, |d| {
                d.child(
                    div()
                        .text_size(m.text_xs)
                        .text_color(p.sand.hsla())
                        .child("· more not shown"),
                )
            })
            .child(div().flex_1())
            .children(replace_all)
            .into_any_element()
    }

    fn render_confirm(&self, confirm: Confirm, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let replacement = self.replace.read(cx).text(cx);
        let what = match (confirm.matches, confirm.files) {
            (1, _) => "1 match".to_owned(),
            (n, 1) => format!("{n} matches in 1 file"),
            (n, f) => format!("{n} matches in {f} files"),
        };
        let text = if replacement.is_empty() {
            format!("Delete {what}?")
        } else {
            format!("Replace {what} with “{replacement}”?")
        };
        let button = primary(
            div()
                .px(px(10.))
                .py(px(3.))
                .rounded(m.radius_control)
                .text_size(m.text_xs),
            cx,
        )
        .id("search-confirm-replace")
        .test_support()
        .cursor_pointer()
        .child("Replace")
        .on_click(cx.listener(|this, _, window, cx| this.replace_all(window, cx)));
        let cancel = div()
            .id("search-cancel-replace")
            .test_support()
            .px_2()
            .text_size(m.text_xs)
            .text_color(p.text_subtle.hsla())
            .cursor_pointer()
            .hover(|s| s.text_color(p.text.hsla()))
            .child("Cancel")
            .on_click(cx.listener(|this, _, _, cx| {
                this.confirm = None;
                cx.notify();
            }));
        div()
            .id("search-confirm")
            .test_support()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py(px(6.))
            .rounded(m.radius_control)
            .bg(p.sand.alpha(0.12))
            .text_size(m.text_xs)
            .text_color(p.text.hsla())
            .child(text)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(button)
                    .child(cancel),
            )
            .into_any_element()
    }

    fn replace_button(
        id: (&'static str, usize),
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let p = cx.tokens().palette.clone();
        div()
            .id(id)
            .test_support()
            .size(px(18.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(p.surface_strong.hsla()))
            .child(icon(Icon::ArrowRightLeft, px(11.), p.text_subtle.hsla()))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                f(this, window, cx);
            }))
            .into_any_element()
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let selected = self.selected();
        let replacement = self.replace.read(cx).text(cx);
        let replacing = !replacement.is_empty();
        range
            .filter_map(|ix| {
                let row = *self.rows.get(ix)?;
                let el = match row {
                    Row::File(i) => {
                        let f = self.results.get(i)?;
                        let (dir, name) = match f.path.rsplit_once('/') {
                            Some((d, n)) => (Some(d.to_owned()), n.to_owned()),
                            None => (None, f.path.clone()),
                        };
                        let chevron = if f.collapsed {
                            Icon::ChevronRight
                        } else {
                            Icon::ChevronDown
                        };
                        let count = f.matches.len();
                        div()
                            .id(("search-file", i))
                            .test_support()
                            .w_full()
                            .h(px(ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .pl(px(4.))
                            .pr(px(4.))
                            .rounded(px(9.))
                            .text_size(m.text_sm)
                            .cursor_pointer()
                            .hover(|s| s.bg(p.surface.hsla()))
                            .child(icon(chevron, px(12.), p.text_subtle.hsla()))
                            .child(glyph::render(&name, cx))
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
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(6.))
                                    .rounded(px(8.))
                                    .bg(p.surface_strong.hsla())
                                    .text_size(m.text_xs)
                                    .text_color(p.text_muted.hsla())
                                    .child(count.to_string()),
                            )
                            .child(Self::replace_button(
                                ("search-replace-file", i),
                                cx,
                                move |this, window, cx| this.replace_file(i, window, cx),
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(i, cx)))
                            .into_any_element()
                    }
                    Row::Match(i, j) => {
                        let mat = self.results.get(i)?.matches.get(j)?;
                        let is_selected = selected == Some((i, j));
                        let (start, end) = (mat.preview_start as usize, mat.preview_end as usize);
                        let ok = start <= end
                            && end <= mat.preview.len()
                            && mat.preview.is_char_boundary(start)
                            && mat.preview.is_char_boundary(end);
                        let (start, end) = if ok { (start, end) } else { (0, 0) };
                        let mut text = String::with_capacity(mat.preview.len() + replacement.len());
                        text.push_str(&mat.preview[..end]);
                        let mut highlights = Vec::new();
                        if replacing {
                            highlights.push((
                                start..end,
                                HighlightStyle {
                                    background_color: Some(p.rose.alpha(0.18)),
                                    strikethrough: Some(StrikethroughStyle {
                                        thickness: px(1.),
                                        color: Some(p.rose.hsla()),
                                    }),
                                    ..Default::default()
                                },
                            ));
                            text.push_str(&replacement);
                            highlights.push((
                                end..end + replacement.len(),
                                HighlightStyle {
                                    background_color: Some(p.green.alpha(0.2)),
                                    color: Some(p.text.hsla()),
                                    ..Default::default()
                                },
                            ));
                        } else {
                            highlights.push((
                                start..end,
                                HighlightStyle {
                                    background_color: Some(p.sand.alpha(0.28)),
                                    color: Some(p.text.hsla()),
                                    font_weight: Some(FontWeight::SEMIBOLD),
                                    ..Default::default()
                                },
                            ));
                        }
                        text.push_str(&mat.preview[end..]);
                        let highlights: Vec<_> = highlights
                            .into_iter()
                            .filter(|(r, _)| !r.is_empty())
                            .collect();
                        div()
                            .id(("search-match", ix))
                            .test_support()
                            .w_full()
                            .h(px(ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .pl(px(MATCH_INDENT))
                            .pr(px(4.))
                            .rounded(px(9.))
                            .text_size(m.text_xs)
                            .font_family(m.font_mono.clone())
                            .text_color(p.text_muted.hsla())
                            .cursor_pointer()
                            .when(is_selected, |d| d.bg(p.surface_selected.hsla()))
                            .when(!is_selected, |d| d.hover(|s| s.bg(p.surface.hsla())))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(StyledText::new(text).with_highlights(highlights)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(p.text_subtle.hsla())
                                    .child((mat.line + 1).to_string()),
                            )
                            .child(Self::replace_button(
                                ("search-replace-match", ix),
                                cx,
                                move |this, window, cx| this.replace_match(i, j, window, cx),
                            ))
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.open(i, j, window, cx)),
                            )
                            .into_any_element()
                    }
                };
                Some(el)
            })
            .collect()
    }
}

impl Render for SearchPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let toggles = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(self.render_toggle(
                "search-case",
                "Aa",
                self.case_sensitive,
                |s| &mut s.case_sensitive,
                cx,
            ))
            .child(self.render_toggle(
                "search-word",
                "ab",
                self.whole_word,
                |s| &mut s.whole_word,
                cx,
            ))
            .child(self.render_toggle("search-regex", ".*", self.regex, |s| &mut s.regex, cx))
            .into_any_element();
        let query = self.render_field("search-query", &self.query, Some(toggles), cx);
        let replace = self.render_field("search-replace", &self.replace, None, cx);
        let filters_toggle = div()
            .id("search-filters")
            .test_support()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .text_size(m.text_xs)
            .text_color(p.text_subtle.hsla())
            .cursor_pointer()
            .hover(|s| s.text_color(p.text.hsla()))
            .child(icon(
                if self.filters_open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                px(11.),
                p.text_subtle.hsla(),
            ))
            .child("Filters")
            .on_click(cx.listener(|this, _, _, cx| {
                this.filters_open = !this.filters_open;
                cx.notify();
            }));
        let filters = self.filters_open.then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(self.render_field("search-include", &self.include, None, cx))
                .child(self.render_field("search-exclude", &self.exclude, None, cx))
        });
        let error = match &self.status {
            Status::Failed(e) => Some(
                div()
                    .id("search-error")
                    .test_support()
                    .px_2()
                    .py_1()
                    .rounded(m.radius_control)
                    .bg(p.rose.alpha(0.12))
                    .text_size(m.text_xs)
                    .text_color(p.rose.hsla())
                    .child(e.clone()),
            ),
            _ => None,
        };
        let note = self.note.clone().map(|n| {
            div()
                .id("search-note")
                .test_support()
                .px_2()
                .text_size(m.text_xs)
                .text_color(p.sage.hsla())
                .child(n)
        });
        let confirm = self.confirm.map(|c| self.render_confirm(c, cx));
        let header = self.render_header(cx);
        let list = uniform_list(
            "search-results",
            self.rows.len(),
            cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
        )
        .track_scroll(&self.scroll)
        .flex_1()
        .min_h_0();
        div()
            .id("sidebar-search-pane")
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::toggle_case))
            .on_action(cx.listener(Self::toggle_word))
            .on_action(cx.listener(Self::toggle_regex))
            .on_action(cx.listener(Self::replace_all_action))
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_1()
                    .child(query)
                    .child(replace)
                    .child(filters_toggle)
                    .children(filters)
                    .children(error)
                    .children(confirm)
                    .children(note)
                    .child(header),
            )
            .child(list)
    }
}
