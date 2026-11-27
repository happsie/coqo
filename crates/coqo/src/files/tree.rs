// SPDX-License-Identifier: Apache-2.0

//! The Files sidebar pane: the current workspace's folders as a lazily
//! loaded, virtualised tree (board 1).
//!
//! Directories are listed with `fs/list` the first time they're expanded
//! and re-listed when `fs/changes` reports a path in them. The visible rows
//! are a flat list rebuilt on every change and drawn with `uniform_list`, so
//! only the rows on screen are laid out, however many entries are expanded.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ops::Range,
};

use coqo_protocol::methods::{
    codes,
    fs::{self, DirEntry, EntryKind, FsChanges, PathParams},
    workspace::{RootId, WorkspaceId, WorkspaceInfo},
};
use coqo_ui::{
    ActiveTokens as _,
    dock::{AnyTab, Dock},
    gpui::{
        AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
        FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render,
        ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
        UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
    },
    widgets::{Icon, TestSupportExt as _, icon, kbd, raised, section_label},
};

use super::{
    Collapse, Confirm, Expand, SelectNext, SelectPrev,
    editor::{self, FileEditor},
};
use crate::{
    daemon::{Daemon, DaemonError, DaemonEvent},
    welcome::shortcut,
    workspace::{Workspaces, WorkspacesEvent, prompt_open_folder_in},
};

/// Height of one row.
const ROW_HEIGHT: f32 = 26.;
/// Indent per level.
const INDENT: f32 = 13.;

/// A directory in the tree: a root and a `/`-separated path (`""` is the root).
type DirKey = (RootId, String);

/// One visible row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub root: RootId,
    /// Path relative to the root.
    pub path: String,
    pub name: SharedString,
    pub kind: EntryKind,
    pub depth: usize,
    pub ignored: bool,
    pub expanded: bool,
    pub loading: bool,
}

impl Row {
    fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    fn key(&self) -> DirKey {
        (self.root, self.path.clone())
    }
}

/// The Files pane's view.
pub struct FileTree {
    dock: WeakEntity<Dock>,
    /// The window's workspace.
    workspaces: Option<Entity<Workspaces>>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    workspace: Option<WorkspaceInfo>,
    /// Listings of the directories loaded so far.
    dirs: HashMap<DirKey, Vec<DirEntry>>,
    /// Listings in flight.
    loading: HashSet<DirKey>,
    /// Listings that changed while in flight: listed again when they land.
    stale: HashSet<DirKey>,
    errors: HashMap<DirKey, String>,
    expanded: HashSet<DirKey>,
    rows: Vec<Row>,
    selected: Option<DirKey>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for FileTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTree")
            .field("workspace", &self.workspace.as_ref().map(|w| &w.id))
            .field("rows", &self.rows.len())
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl Focusable for FileTree {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Whether `path` is strictly inside directory `dir`.
fn is_inside(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        !path.is_empty()
    } else {
        path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
    }
}

impl FileTree {
    pub fn new(dock: WeakEntity<Dock>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        let workspaces = Workspaces::for_window(window, cx);
        if let Some(workspaces) = &workspaces {
            subscriptions.push(cx.subscribe(
                workspaces,
                |this, workspaces, event, cx| match event {
                    WorkspacesEvent::CurrentChanged => {
                        let info = workspaces.read(cx).current().cloned();
                        this.set_workspace(info, cx);
                    }
                    WorkspacesEvent::FsChanged { workspace, changes } => {
                        this.apply_changes(workspace, changes, cx);
                    }
                },
            ));
            subscriptions.push(cx.observe(workspaces, |_, _, cx| cx.notify()));
        }
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| {
                if *event == DaemonEvent::Disconnected {
                    this.loading.clear();
                    this.stale.clear();
                    this.rebuild(cx);
                }
            }));
            subscriptions.push(cx.observe(&daemon, |_, _, cx| cx.notify()));
        }
        let current = workspaces
            .as_ref()
            .and_then(|w| w.read(cx).current().cloned());
        let mut this = Self {
            dock,
            workspaces,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            workspace: None,
            dirs: HashMap::new(),
            loading: HashSet::new(),
            stale: HashSet::new(),
            errors: HashMap::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            _subscriptions: subscriptions,
        };
        this.set_workspace(current, cx);
        this
    }

    /// The visible rows, top to bottom.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The selected row's root and path.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn selected(&self) -> Option<(RootId, &str)> {
        self.selected.as_ref().map(|(r, p)| (*r, p.as_str()))
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows
            .iter()
            .position(|r| r.root == selected.0 && r.path == selected.1)
    }

    fn multi_root(&self) -> bool {
        self.workspace.as_ref().is_some_and(|w| w.roots.len() > 1)
    }

    /// Show `info` (or nothing). The same workspace again (a reconnect) keeps
    /// what's expanded and lists it again.
    fn set_workspace(&mut self, info: Option<WorkspaceInfo>, cx: &mut Context<Self>) {
        let same = self.workspace.as_ref().map(|w| &w.id) == info.as_ref().map(|w| &w.id);
        if !same {
            self.dirs.clear();
            self.expanded.clear();
            self.errors.clear();
            self.selected = None;
        }
        self.loading.clear();
        self.stale.clear();
        self.workspace = info;
        let Some(info) = self.workspace.clone() else {
            self.rebuild(cx);
            return;
        };
        for root in &info.roots {
            self.expanded.insert((root.id, String::new()));
        }
        let mut reload: Vec<DirKey> = self.dirs.keys().cloned().collect();
        reload.extend(info.roots.iter().map(|r| (r.id, String::new())));
        reload.sort();
        reload.dedup();
        for key in reload {
            if self.expanded.contains(&key) || self.dirs.contains_key(&key) {
                self.load(key, cx);
            }
        }
        self.rebuild(cx);
    }

    fn load(&mut self, key: DirKey, cx: &mut Context<Self>) {
        let (Some(workspace), Some(daemon)) = (self.workspace.as_ref(), Daemon::global(cx)) else {
            return;
        };
        if !self.loading.insert(key.clone()) {
            self.stale.insert(key);
            return;
        }
        let workspace_id = workspace.id.clone();
        let list = daemon.read(cx).request::<fs::ListDir>(PathParams {
            workspace: workspace_id.clone(),
            root: key.0,
            path: key.1.clone(),
        });
        cx.spawn(async move |this, cx| {
            let result = list.await;
            this.update(cx, |this, cx| this.loaded(workspace_id, key, result, cx))
                .ok();
        })
        .detach();
    }

    fn loaded(
        &mut self,
        workspace: WorkspaceId,
        key: DirKey,
        result: Result<fs::ListDirResult, DaemonError>,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.as_ref().map(|w| &w.id) != Some(&workspace) {
            return;
        }
        self.loading.remove(&key);
        match result {
            Ok(list) => {
                self.errors.remove(&key);
                self.prune_below(&key, &list.entries);
                self.dirs.insert(key.clone(), list.entries);
            }
            Err(err) if matches!(err.code(), Some(codes::NOT_FOUND | codes::NOT_A_DIRECTORY)) => {
                self.forget(&key);
            }
            Err(DaemonError::NotConnected) => {}
            Err(err) => {
                self.errors.insert(key.clone(), err.to_string());
            }
        }
        if self.stale.remove(&key) {
            self.load(key, cx);
        }
        self.rebuild(cx);
    }

    /// Drop cached listings under `dir` for children that are gone or no
    /// longer directories.
    fn prune_below(&mut self, dir: &DirKey, entries: &[DirEntry]) {
        let (root, dir_path) = (dir.0, dir.1.as_str());
        let alive: HashSet<String> = entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir)
            .map(|e| join(dir_path, &e.name))
            .collect();
        let gone = |key: &DirKey| {
            key.0 == root
                && is_inside(&key.1, dir_path)
                && !alive
                    .iter()
                    .any(|child| key.1 == *child || is_inside(&key.1, child))
        };
        self.dirs.retain(|k, _| !gone(k));
        self.expanded.retain(|k| !gone(k));
    }

    /// Forget a directory that no longer exists, and everything under it.
    fn forget(&mut self, dir: &DirKey) {
        let gone = |key: &DirKey| key.0 == dir.0 && (key.1 == dir.1 || is_inside(&key.1, &dir.1));
        self.dirs.retain(|k, _| !gone(k));
        self.expanded.retain(|k| !gone(k) || k.1.is_empty());
        self.errors.retain(|k, _| !gone(k));
    }

    /// Re-list the loaded directories that `changes` touches.
    fn apply_changes(
        &mut self,
        workspace: &WorkspaceId,
        changes: &FsChanges,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.as_ref().map(|w| &w.id) != Some(workspace) {
            return;
        }
        let root = changes.root;
        let mut reload = BTreeSet::new();
        if changes.rescan {
            reload.extend(self.dirs.keys().filter(|k| k.0 == root).cloned());
        }
        for path in &changes.paths {
            for dir in [parent(path), path.as_str()] {
                let key = (root, dir.to_owned());
                if self.dirs.contains_key(&key) || self.loading.contains(&key) {
                    reload.insert(key);
                }
            }
        }
        for key in reload {
            self.load(key, cx);
        }
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        if let Some(info) = &self.workspace {
            if info.roots.len() > 1 {
                for root in &info.roots {
                    let key = (root.id, String::new());
                    let expanded = self.expanded.contains(&key);
                    rows.push(Row {
                        root: root.id,
                        path: String::new(),
                        name: root.name.clone().into(),
                        kind: EntryKind::Dir,
                        depth: 0,
                        ignored: false,
                        expanded,
                        loading: self.loading.contains(&key) && !self.dirs.contains_key(&key),
                    });
                    if expanded {
                        self.push_children(&mut rows, root.id, "", 1);
                    }
                }
            } else if let Some(root) = info.roots.first() {
                self.push_children(&mut rows, root.id, "", 0);
            }
        }
        self.rows = rows;
        if self.selected.is_some() && self.selected_index().is_none() {
            self.selected = None;
        }
        cx.notify();
    }

    fn push_children(&self, rows: &mut Vec<Row>, root: RootId, dir: &str, depth: usize) {
        let Some(entries) = self.dirs.get(&(root, dir.to_owned())) else {
            return;
        };
        for entry in entries {
            let path = join(dir, &entry.name);
            let key = (root, path);
            let is_dir = entry.kind == EntryKind::Dir;
            let expanded = is_dir && self.expanded.contains(&key);
            let loading = expanded && !self.dirs.contains_key(&key);
            rows.push(Row {
                root,
                path: key.1.clone(),
                name: entry.name.clone().into(),
                kind: entry.kind,
                depth,
                ignored: entry.ignored,
                expanded,
                loading,
            });
            if expanded {
                self.push_children(rows, root, &key.1, depth + 1);
            }
        }
    }

    /// Expand or collapse a directory.
    pub fn toggle(&mut self, root: RootId, path: &str, cx: &mut Context<Self>) {
        let key = (root, path.to_owned());
        if self.expanded.contains(&key) {
            self.collapse_dir(key, cx);
        } else {
            self.expand_dir(key, cx);
        }
    }

    fn expand_dir(&mut self, key: DirKey, cx: &mut Context<Self>) {
        if self.expanded.insert(key.clone()) && !self.dirs.contains_key(&key) {
            self.load(key, cx);
        }
        self.rebuild(cx);
    }

    fn collapse_dir(&mut self, key: DirKey, cx: &mut Context<Self>) {
        // A single root's own folder stays open: it's the tree itself.
        if key.1.is_empty() && !self.multi_root() {
            return;
        }
        self.expanded.remove(&key);
        self.rebuild(cx);
    }

    fn select_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(ix) {
            self.selected = Some(row.key());
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index().map_or(0, |ix| ix + 1);
        self.select_index(ix.min(self.rows.len().saturating_sub(1)), cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index().map_or(0, |ix| ix.saturating_sub(1));
        self.select_index(ix, cx);
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else {
            return self.select_index(0, cx);
        };
        let row = self.rows[ix].clone();
        if !row.is_dir() {
            return;
        }
        if !row.expanded {
            self.expand_dir(row.key(), cx);
        } else if self.rows.get(ix + 1).is_some_and(|r| r.depth > row.depth) {
            self.select_index(ix + 1, cx);
        }
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else {
            return;
        };
        let row = self.rows[ix].clone();
        if row.is_dir() && row.expanded {
            self.collapse_dir(row.key(), cx);
        } else if let Some(parent) = self.rows[..ix].iter().rposition(|r| r.depth < row.depth) {
            self.select_index(parent, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected_index() {
            self.activate(ix, window, cx);
        }
    }

    /// What a click or Enter does: toggle a directory, open a file.
    pub(crate) fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        self.selected = Some(row.key());
        if row.is_dir() {
            self.toggle(row.root, &row.path, cx);
        } else if row.kind == EntryKind::File {
            self.open_file(&row, window, cx);
        }
        cx.notify();
    }

    fn open_file(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(workspace), Some(dock)) = (self.workspace.as_ref(), self.dock.upgrade()) else {
            return;
        };
        let (workspace, root, path) = (workspace.id.clone(), row.root, row.path.clone());
        // Show an open tab instead of opening the buffer a second time.
        let key = editor::key(&workspace, root, &path);
        if let Some(open) = dock.read(cx).find_tab(editor::KIND, &key, cx) {
            dock.update(cx, |dock, cx| dock.activate_tab(open, window, cx));
            return;
        }
        let tab: Entity<FileEditor> =
            cx.new(|cx| FileEditor::new(workspace, root, path, window, cx));
        dock.update(cx, |dock, cx| {
            dock.open_tab(AnyTab::new(tab), window, cx);
        });
    }

    fn click_row(
        &mut self,
        ix: usize,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.activate(ix, window, cx);
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let selected = self.selected_index();
        range
            .filter_map(|ix| {
                let row = self.rows.get(ix)?;
                let is_selected = selected == Some(ix);
                let color = if row.ignored {
                    p.text_subtle
                } else if is_selected {
                    p.text
                } else {
                    p.text_muted
                };
                let lead = if row.is_dir() {
                    let chevron = if row.expanded {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    };
                    div()
                        .w(px(16.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(chevron, px(12.), p.text_subtle.hsla()))
                        .into_any_element()
                } else {
                    super::glyph::render(&row.name, cx)
                };
                let el = div()
                    .id(("file-row", ix))
                    .test_support()
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(8. + INDENT * row.depth as f32))
                    .pr(px(8.))
                    .rounded(px(9.))
                    .text_size(m.text_sm)
                    .text_color(color.hsla())
                    .cursor_pointer()
                    .when(is_selected, |d| d.bg(p.surface_selected.hsla()))
                    .when(!is_selected, |d| d.hover(|s| s.bg(p.surface.hsla())))
                    .when(row.ignored, |d| d.opacity(0.75))
                    .child(lead)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(row.name.clone()),
                    )
                    .when(row.loading, |d| {
                        d.child(
                            div()
                                .text_size(m.text_xs)
                                .text_color(p.text_subtle.hsla())
                                .child("…"),
                        )
                    })
                    .on_click(
                        cx.listener(move |this, e, window, cx| this.click_row(ix, e, window, cx)),
                    );
                Some(el.into_any_element())
            })
            .collect()
    }

    fn render_note(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let note = |text: SharedString| {
            div()
                .px_2()
                .py_1()
                .text_size(m.text_sm)
                .text_color(p.text_subtle.hsla())
                .child(text)
        };
        let daemon = Daemon::global(cx);
        let connected = daemon.as_ref().is_some_and(|d| d.read(cx).is_connected());
        let workspaces = self.workspaces.clone();
        if let Some(info) = &self.workspace {
            let single_root = (!self.multi_root()).then(|| (info.roots[0].id, String::new()));
            if let Some(err) = single_root.as_ref().and_then(|k| self.errors.get(k)) {
                return Some(note(err.clone().into()).into_any_element());
            }
            if self.rows.is_empty() {
                let loading = single_root
                    .as_ref()
                    .is_some_and(|k| !self.dirs.contains_key(k));
                let text = match (loading, connected) {
                    (true, true) => "Loading…",
                    (true, false) => "Waiting for coqod…",
                    (false, _) => "This folder is empty",
                };
                return Some(note(text.into()).into_any_element());
            }
            return None;
        }
        if workspaces.as_ref().is_some_and(|w| w.read(cx).is_opening()) {
            return Some(note("Opening…".into()).into_any_element());
        }
        let error = workspaces
            .as_ref()
            .and_then(|w| w.read(cx).error().map(|e| SharedString::from(e.to_owned())));
        let open_button = raised(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(12.))
                .py(px(6.))
                .rounded(m.radius_control)
                .text_size(m.text_sm)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text.hsla()),
            cx,
        )
        .id("files-open-folder")
        .test_support()
        .cursor_pointer()
        .child(icon(Icon::FolderOpen, px(14.), p.text_muted.hsla()))
        .child("Open Folder…")
        .child(kbd(shortcut("Ctrl O"), cx))
        .on_click({
            let workspaces = workspaces.as_ref().map(|w| w.downgrade());
            move |_, _, cx| {
                if let Some(workspaces) = workspaces.clone() {
                    prompt_open_folder_in(workspaces, cx);
                }
            }
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(note("No folder open".into()))
                .children(error.map(|e| {
                    div()
                        .px_2()
                        .text_size(m.text_xs)
                        .text_color(p.rose.hsla())
                        .child(e)
                }))
                .child(div().px_2().flex().child(open_button))
                .into_any_element(),
        )
    }
}

use coqo_ui::widgets::FluentBuilder as _;

impl Render for FileTree {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label: SharedString = self
            .workspace
            .as_ref()
            .map_or_else(|| "Explorer".into(), |w| w.name.clone().into());
        let note = self.render_note(cx);
        let list = (note.is_none()).then(|| {
            uniform_list(
                "file-tree-list",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
        });
        div()
            .id("sidebar-files-pane")
            .test_support()
            .key_context("FileTree")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::collapse))
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_label(label, cx))
            .children(note)
            .children(list)
    }
}

#[cfg(test)]
mod tests {
    use super::{is_inside, join, parent};

    #[test]
    fn path_helpers() {
        assert_eq!(join("", "src"), "src");
        assert_eq!(join("src", "main.rs"), "src/main.rs");
        assert_eq!(parent("src/main.rs"), "src");
        assert_eq!(parent("README.md"), "");
        assert!(is_inside("src/main.rs", "src"));
        assert!(is_inside("src", ""));
        assert!(!is_inside("src", "src"));
        assert!(!is_inside("srcx/a", "src"));
        assert!(!is_inside("", ""));
    }
}
