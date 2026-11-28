// SPDX-License-Identifier: Apache-2.0

//! The Files sidebar pane: the current workspace's folders as a lazily
//! loaded, virtualised tree (board 1).
//!
//! Directories are listed with `fs/list` the first time they're expanded
//! and re-listed when `fs/changes` reports a path in them. The visible rows
//! are a flat list rebuilt on every change and drawn with `uniform_list`, so
//! only the rows on screen are laid out, however many entries are expanded.
//!
//! **File operations** (issue #161): a row's context menu creates, renames
//! (in place, in the row), deletes (after a confirmation that names the
//! entry), copies paths and reveals the entry in the OS file manager;
//! dragging a row onto a folder moves it. The daemon does the work
//! (`fs/create`, `fs/rename`, `fs/delete`); the tree selects the result,
//! and [`FileEvents`] tells open file tabs to follow.
//!
//! **Undo** (issue #291, [`undo`]): `Ctrl Z` / `Cmd Z` in the focused tree
//! undoes the window's last create, rename, move or delete, and `Ctrl Shift
//! Z` / `Cmd Shift Z` (and `Ctrl Y` off macOS) redoes it ([`History`]).
//!
//! **Decorations** (issue #163, [`RowDecorations`]): rows end in what
//! providers say about them (git's status letters), and a folder's name is
//! tinted when something inside is. **Auto-reveal** (`files.auto_reveal`):
//! switching to a file tab reveals its file here, without taking the focus.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ops::Range,
    path::PathBuf,
};

use coqo_protocol::methods::{
    codes,
    fs::{
        self, CopyParams, CreateParams, DeleteParams, DirEntry, EntryKind, FsChanges, PathParams,
        RenameParams,
    },
    workspace::{RootId, WorkspaceId, WorkspaceInfo},
};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    dock::{AnyTab, Dock},
    gpui::{
        AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FocusHandle,
        Focusable, FontWeight, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
        ParentElement as _, Pixels, Point, Render, ScrollStrategy, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
        WeakEntity, Window, anchored, deferred, div, px, uniform_list,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{Icon, TestSupportExt as _, icon, kbd, popover, raised, section_label},
};

use super::{
    Collapse, Confirm, CopyEntry, CopyPath, CopyRelativePath, DeleteEntry, Dismiss, DuplicateEntry,
    Expand, NewFile, NewFolder, PasteEntry, RenameEntry, RevealInFileManager, SelectNext,
    SelectPrev,
    decorations::{RowDecoration, RowDecorations},
    editor::{self, FileEditor},
    history::{FileOp, History},
    ops::{FileEvent, FileEvents, is_at_or_under, moved_path},
};

mod undo;
use crate::{
    daemon::{Daemon, DaemonError, DaemonEvent},
    settings::{self, Settings},
    welcome::shortcut,
    workspace::{Workspaces, WorkspacesEvent, prompt_open_folder_in},
};

/// Height of one row.
const ROW_HEIGHT: f32 = 26.;
/// Indent per level.
const INDENT: f32 = 13.;
/// The path of the row a new entry's name is typed in: no real name has a
/// NUL.
const NEW_ENTRY: &str = "\0new";

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
    /// The row shows a name field: a rename in place, or a new entry.
    pub editing: bool,
}

impl Row {
    fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    fn key(&self) -> DirKey {
        (self.root, self.path.clone())
    }

    fn target(&self) -> Target {
        Target {
            root: self.root,
            path: self.path.clone(),
            kind: self.kind,
        }
    }
}

/// An entry an operation applies to. `path` `""` is a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub root: RootId,
    pub path: String,
    pub kind: EntryKind,
}

impl Target {
    /// The folder new entries go in: this one, or a file's folder.
    fn folder(&self) -> DirKey {
        if self.kind == EntryKind::Dir {
            (self.root, self.path.clone())
        } else {
            (self.root, parent(&self.path).to_owned())
        }
    }
}

/// An open context menu.
struct Menu {
    target: Target,
    at: Point<Pixels>,
}

/// What a name field in a row is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditKind {
    /// Renaming an entry in place.
    Rename { root: RootId, path: String },
    /// Naming a new entry in folder `dir`.
    Create {
        root: RootId,
        dir: String,
        kind: EntryKind,
    },
}

/// A name being typed in a row.
struct Editing {
    kind: EditKind,
    input: Entity<PromptInput>,
    /// The request is in flight.
    busy: bool,
    _events: Subscription,
}

/// A delete waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelete {
    pub target: Target,
    /// Names of the open tabs with unsaved edits in what would be deleted.
    pub dirty: Vec<String>,
    /// The trash refused it: offer to delete it for good.
    pub permanent: bool,
    pub error: Option<String>,
    /// Undoing this creation: the entry changed since, so the user is asked.
    pub undo: Option<FileOp>,
    busy: bool,
}

/// The drag payload of a tree row.
#[derive(Debug, Clone)]
pub struct DraggedEntry {
    pub root: RootId,
    pub path: String,
    pub name: SharedString,
    kind: EntryKind,
}

impl Render for DraggedEntry {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let lead = if self.kind == EntryKind::Dir {
            icon(Icon::Folder, px(14.), p.text_muted.hsla()).into_any_element()
        } else {
            super::glyph::render(&self.name, cx)
        };
        popover(cx)
            .flex_row()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(9.))
            .text_size(m.text_sm)
            .text_color(p.text.hsla())
            .child(lead)
            .child(self.name.clone())
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
    /// Select this entry once its row shows up (a new or renamed entry).
    pending_select: Option<DirKey>,
    menu: Option<Menu>,
    editing: Option<Editing>,
    pending_delete: Option<PendingDelete>,
    /// The file or folder Copy remembered, for Paste.
    copied: Option<Target>,
    /// The last operation's error, above the tree.
    op_error: Option<String>,
    /// The window's file operations, to undo and redo.
    history: Entity<History>,
    /// An undo or redo is in flight.
    undoing: bool,
    /// What the last undo or redo did ("Undid rename of …"), shown briefly.
    toast: Option<SharedString>,
    /// What the rows show besides their names (git's letters).
    decorations: Entity<RowDecorations>,
    /// The file tab auto-reveal last showed, so it reveals each switch once.
    followed: Option<(WorkspaceId, RootId, String)>,
    _toast_timer: Option<Task<()>>,
    _op: Option<Task<()>>,
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

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether `path` is strictly inside directory `dir`.
fn is_inside(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        !path.is_empty()
    } else {
        path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
    }
}

/// What the user reads when an operation fails.
fn op_message(error: &DaemonError, name: &str) -> String {
    match error.code() {
        Some(codes::ALREADY_EXISTS) => {
            format!("A file or folder named “{name}” already exists here.")
        }
        Some(codes::PERMISSION_DENIED) => format!("Permission denied for “{name}”."),
        Some(codes::NOT_FOUND) => format!("“{name}” is no longer there."),
        _ => error.to_string(),
    }
}

/// "Reveal in Finder" and the like.
#[cfg(target_os = "macos")]
pub(crate) const REVEAL_LABEL: &str = "Reveal in Finder";
#[cfg(not(target_os = "macos"))]
pub(crate) const REVEAL_LABEL: &str = "Reveal in File Manager";

#[cfg(target_os = "macos")]
const DELETE_KEY_LABEL: &str = "⌘⌫";
#[cfg(not(target_os = "macos"))]
const DELETE_KEY_LABEL: &str = "Del";

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
        if let Some(events) = FileEvents::global(cx) {
            // Another window's tree moved something this one shows.
            subscriptions.push(cx.subscribe(&events, |this, _, event: &FileEvent, cx| {
                if let FileEvent::Moved {
                    workspace,
                    root,
                    from,
                    to,
                } = event
                    && this.workspace.as_ref().map(|w| &w.id) == Some(workspace)
                {
                    this.rekey(*root, from, to);
                    this.rebuild(cx);
                }
            }));
        }
        let decorations = RowDecorations::for_window(window, cx);
        subscriptions.push(cx.observe(&decorations, |_, _, cx| cx.notify()));
        if let Some(dock) = dock.upgrade() {
            subscriptions.push(cx.observe(&dock, |this, _, cx| this.follow_active_tab(cx)));
        }
        subscriptions.push(cx.observe_global::<Settings>(|this, cx| {
            this.followed = None;
            this.follow_active_tab(cx);
        }));
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
            pending_select: None,
            menu: None,
            editing: None,
            pending_delete: None,
            copied: None,
            op_error: None,
            history: History::for_window(window, cx),
            undoing: false,
            toast: None,
            decorations,
            followed: None,
            _toast_timer: None,
            _op: None,
            _subscriptions: subscriptions,
        };
        this.set_workspace(current, cx);
        this.follow_active_tab(cx);
        this
    }

    /// What row `ix` shows besides its name.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn decoration(&self, ix: usize, cx: &App) -> RowDecoration {
        self.rows
            .get(ix)
            .map(|row| self.decoration_of(row, cx))
            .unwrap_or_default()
    }

    fn decoration_of(&self, row: &Row, cx: &App) -> RowDecoration {
        if row.ignored || row.editing {
            return RowDecoration::default();
        }
        let mut d = self.decorations.read(cx).row(row.root, &row.path);
        if !row.is_dir() {
            // Only folders are tinted; a file has its letter.
            d.tint = None;
        }
        d
    }

    /// Scroll so that row `ix` is at the top (benchmarks).
    #[cfg(test)]
    pub fn scroll_to_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
        cx.notify();
    }

    /// With `files.auto_reveal` on, reveal the active file tab's file once
    /// per switch, leaving the focus where it is. Not while a name is typed
    /// or a delete waits in the tree.
    fn follow_active_tab(&mut self, cx: &mut Context<Self>) {
        let workspace = self.workspace.as_ref().map(|w| &w.id);
        if !settings::get(cx).bool(settings::AUTO_REVEAL, workspace) {
            self.followed = None;
            return;
        }
        let Some(dock) = self.dock.upgrade() else {
            return;
        };
        let active = {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
                .map(|tab| {
                    let tab = tab.read(cx);
                    (tab.workspace().clone(), tab.root(), tab.path().to_owned())
                })
        };
        let Some(active) = active else {
            self.followed = None;
            return;
        };
        if self.followed.as_ref() == Some(&active)
            || self.workspace.as_ref().map(|w| &w.id) != Some(&active.0)
            || self.editing.is_some()
            || self.pending_delete.is_some()
        {
            return;
        }
        self.reveal_entry(active.1, &active.2, cx);
        self.followed = Some(active);
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

    /// What the open context menu is for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn menu_target(&self) -> Option<&Target> {
        self.menu.as_ref().map(|m| &m.target)
    }

    /// The name field in a row, and what it is for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn name_field(&self) -> Option<(&EditKind, &Entity<PromptInput>)> {
        self.editing.as_ref().map(|e| (&e.kind, &e.input))
    }

    /// The delete waiting for confirmation.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pending_delete(&self) -> Option<&PendingDelete> {
        self.pending_delete.as_ref()
    }

    /// The last operation's error.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn op_error(&self) -> Option<&str> {
        self.op_error.as_deref()
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
            self.pending_select = None;
            self.menu = None;
            self.editing = None;
            self.pending_delete = None;
            self.op_error = None;
            self.history.update(cx, |h, _| h.clear());
            self.toast = None;
            self.followed = None;
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

    /// List `key` again if the tree has it (or shows it expanded).
    fn relist(&mut self, key: DirKey, cx: &mut Context<Self>) {
        if self.dirs.contains_key(&key) || self.expanded.contains(&key) {
            self.load(key, cx);
        }
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

    /// `from` moved to `to` under `root`: carry what was expanded and loaded
    /// there along, so a moved folder looks the same at its new place.
    fn rekey(&mut self, root: RootId, from: &str, to: &str) {
        let moved = |key: &DirKey| {
            (key.0 == root)
                .then(|| moved_path(&key.1, from, to))
                .flatten()
                .map(|path| (root, path))
        };
        self.dirs = std::mem::take(&mut self.dirs)
            .into_iter()
            .map(|(k, v)| (moved(&k).unwrap_or(k), v))
            .collect();
        self.expanded = std::mem::take(&mut self.expanded)
            .into_iter()
            .map(|k| moved(&k).unwrap_or(k))
            .collect();
        self.errors.retain(|k, _| moved(k).is_none());
        if let Some(selected) = self.selected.as_ref().and_then(moved) {
            self.selected = Some(selected);
        }
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
                        editing: false,
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
        if let Some(pending) = &self.pending_select
            && let Some(ix) = self
                .rows
                .iter()
                .position(|r| r.root == pending.0 && r.path == pending.1)
        {
            self.selected = self.pending_select.take();
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        if self.selected.is_some() && self.selected_index().is_none() {
            self.selected = None;
        }
        cx.notify();
    }

    fn push_children(&self, rows: &mut Vec<Row>, root: RootId, dir: &str, depth: usize) {
        if let Some(Editing {
            kind:
                EditKind::Create {
                    root: r,
                    dir: d,
                    kind,
                },
            ..
        }) = &self.editing
            && *r == root
            && d == dir
        {
            // The new entry's name is typed at the top of its folder.
            rows.push(Row {
                root,
                path: join(dir, NEW_ENTRY),
                name: SharedString::default(),
                kind: *kind,
                depth,
                ignored: false,
                expanded: false,
                loading: false,
                editing: true,
            });
        }
        let Some(entries) = self.dirs.get(&(root, dir.to_owned())) else {
            return;
        };
        let renaming = match &self.editing {
            Some(Editing {
                kind: EditKind::Rename { root: r, path },
                ..
            }) if *r == root => Some(path.as_str()),
            _ => None,
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
                editing: renaming == Some(key.1.as_str()),
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
        if self.pending_delete.is_some() {
            self.confirm_delete(window, cx);
        } else if let Some(ix) = self.selected_index() {
            self.activate(ix, window, cx);
        }
    }

    /// What a click or Enter does: toggle a directory, open a file.
    pub(crate) fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        if row.editing {
            return;
        }
        self.selected = Some(row.key());
        if row.is_dir() {
            self.toggle(row.root, &row.path, cx);
        } else if row.kind == EntryKind::File {
            self.open_path(row.root, &row.path, window, cx);
        }
        cx.notify();
    }

    fn open_path(&mut self, root: RootId, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(workspace), Some(dock)) = (self.workspace.as_ref(), self.dock.upgrade()) else {
            return;
        };
        let (workspace, path) = (workspace.id.clone(), path.to_owned());
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
        if self.rows.get(ix).is_some_and(|r| r.editing) {
            return;
        }
        window.focus(&self.focus, cx);
        self.activate(ix, window, cx);
    }

    // ---- file operations ----------------------------------------------------

    /// The workspace and a connected daemon.
    fn session(&self, cx: &App) -> Option<(WorkspaceId, Entity<Daemon>)> {
        let workspace = self.workspace.as_ref()?.id.clone();
        let daemon = Daemon::global(cx).filter(|d| d.read(cx).is_connected())?;
        Some((workspace, daemon))
    }

    /// The absolute path of `path` under `root`.
    fn absolute(&self, root: RootId, path: &str) -> Option<PathBuf> {
        let root = self
            .workspace
            .as_ref()?
            .roots
            .iter()
            .find(|r| r.id == root)?;
        let mut abs = PathBuf::from(&root.path);
        abs.extend(path.split('/').filter(|c| !c.is_empty()));
        Some(abs)
    }

    /// The selected entry, or the (single) root when nothing is selected.
    fn selected_target(&self) -> Option<Target> {
        if let Some(ix) = self.selected_index() {
            return Some(self.rows[ix].target());
        }
        let root = self.workspace.as_ref()?.roots.first()?;
        Some(Target {
            root: root.id,
            path: String::new(),
            kind: EntryKind::Dir,
        })
    }

    /// Open the context menu for `target` at `at` (window coordinates).
    pub(crate) fn open_menu(&mut self, target: Target, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.pending_delete = None;
        self.op_error = None;
        if !target.path.is_empty() || self.multi_root() {
            self.selected = Some((target.root, target.path.clone()));
        }
        self.menu = Some(Menu { target, at });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        // An undo of a creation the user didn't confirm stays undoable.
        if let Some(op) = self.pending_delete.take().and_then(|p| p.undo) {
            self.history.update(cx, |h, _| h.put_back(op, true));
        }
        self.op_error = None;
        cx.notify();
    }

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.selected_target() {
            self.start_create(target.folder(), EntryKind::File, window, cx);
        }
    }

    fn new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.selected_target() {
            self.start_create(target.folder(), EntryKind::Dir, window, cx);
        }
    }

    fn rename_selected(&mut self, _: &RenameEntry, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        if let Some(ix) = self.selected_index() {
            let row = self.rows[ix].clone();
            self.start_rename(row.root, &row.path, window, cx);
        }
    }

    fn duplicate_selected(
        &mut self,
        _: &DuplicateEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_some() {
            return;
        }
        if let Some(ix) = self.selected_index() {
            let target = self.rows[ix].target();
            self.duplicate(target, window, cx);
        }
    }

    fn copy_selected_entry(&mut self, _: &CopyEntry, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected_index() {
            let target = self.rows[ix].target();
            self.copy_entry(target, cx);
        }
    }

    fn paste_selected(&mut self, _: &PasteEntry, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        if let Some(target) = self.selected_target() {
            self.paste(target.folder(), window, cx);
        }
    }

    fn delete_selected(&mut self, _: &DeleteEntry, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        if let Some(ix) = self.selected_index() {
            let target = self.rows[ix].target();
            self.request_delete(target, window, cx);
        }
    }

    /// A name field for a row, focused, starting with `text`.
    fn name_input(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<PromptInput>, Subscription) {
        let input = cx.new(|cx| {
            let mut input = PromptInput::new("Name", 1, 1, window, cx);
            input.set_text(text.to_owned(), window, cx);
            input
        });
        let events = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            PromptInputEvent::Submit => this.finish_edit(true, window, cx),
            PromptInputEvent::Cancel => this.finish_edit(false, window, cx),
            PromptInputEvent::Changed => {
                if this.op_error.take().is_some() {
                    cx.notify();
                }
            }
            PromptInputEvent::Key(_) => {}
        });
        let focus = Focusable::focus_handle(input.read(cx), cx);
        window.focus(&focus, cx);
        (input, events)
    }

    /// Start naming a new file or folder in `dir`.
    pub(crate) fn start_create(
        &mut self,
        dir: DirKey,
        kind: EntryKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.pending_delete = None;
        self.op_error = None;
        if self.expanded.insert(dir.clone()) && !self.dirs.contains_key(&dir) {
            self.load(dir.clone(), cx);
        }
        let (input, events) = self.name_input("", window, cx);
        let (root, dir) = dir;
        self.editing = Some(Editing {
            kind: EditKind::Create { root, dir, kind },
            input,
            busy: false,
            _events: events,
        });
        self.rebuild(cx);
        if let Some(ix) = self.rows.iter().position(|r| r.editing) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    /// Start renaming `path` in its row.
    pub(crate) fn start_rename(
        &mut self,
        root: RootId,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.pending_delete = None;
        self.op_error = None;
        if path.is_empty() {
            return; // a root: renamed with the workspace, not here
        }
        let (input, events) = self.name_input(base_name(path), window, cx);
        self.selected = Some((root, path.to_owned()));
        self.editing = Some(Editing {
            kind: EditKind::Rename {
                root,
                path: path.to_owned(),
            },
            input,
            busy: false,
            _events: events,
        });
        self.rebuild(cx);
    }

    /// End the name field: send the name if `accept`.
    fn finish_edit(&mut self, accept: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.as_mut() else {
            return;
        };
        if editing.busy {
            return;
        }
        let name = editing.input.read(cx).text(cx).trim().to_owned();
        let kind = editing.kind.clone();
        let unchanged = match &kind {
            EditKind::Rename { path, .. } => name == base_name(path),
            EditKind::Create { .. } => false,
        };
        if !accept || name.is_empty() || unchanged {
            self.editing = None;
            self.op_error = None;
            window.focus(&self.focus, cx);
            self.rebuild(cx);
            return;
        }
        editing.busy = true;
        match kind {
            EditKind::Rename { root, path } => {
                let to = join(parent(&path), &name);
                self.rename_entry(root, path, to, window, cx);
            }
            EditKind::Create { root, dir, kind } => {
                self.create_entry(root, join(&dir, &name), kind, window, cx);
            }
        }
    }

    fn create_entry(
        &mut self,
        root: RootId,
        path: String,
        kind: EntryKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace, daemon)) = self.session(cx) else {
            self.op_failed(&DaemonError::NotConnected, &path, cx);
            return;
        };
        let create = daemon.read(cx).request::<fs::Create>(CreateParams {
            workspace,
            root,
            path: path.clone(),
            kind,
        });
        self._op = Some(cx.spawn_in(window, async move |this, cx| {
            let result = create.await;
            this.update_in(cx, |this, window, cx| {
                this.created(root, &path, kind, result, window, cx)
            })
            .ok();
        }));
    }

    fn created(
        &mut self,
        root: RootId,
        asked: &str,
        kind: EntryKind,
        result: Result<fs::EntryResult, DaemonError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => return self.op_failed(&e, base_name(asked), cx),
        };
        self.history.update(cx, |h, _| {
            h.record(FileOp::Create {
                root,
                path: entry.path.clone(),
                kind,
            })
        });
        let dir = match &self.editing {
            Some(Editing {
                kind: EditKind::Create { dir, .. },
                ..
            }) => dir.clone(),
            _ => parent(&entry.path).to_owned(),
        };
        self.editing = None;
        self.op_error = None;
        // Folders made on the way are shown open.
        let mut folder = parent(&entry.path);
        while is_inside(folder, &dir) {
            self.expanded.insert((root, folder.to_owned()));
            self.relist((root, folder.to_owned()), cx);
            folder = parent(folder);
        }
        self.relist((root, dir), cx);
        self.pending_select = Some((root, entry.path.clone()));
        window.focus(&self.focus, cx);
        self.rebuild(cx);
        if kind == EntryKind::File {
            self.open_path(root, &entry.path, window, cx);
        }
    }

    /// Rename or move `from` to `to` (both under `root`).
    fn rename_entry(
        &mut self,
        root: RootId,
        from: String,
        to: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((workspace, daemon)) = self.session(cx) else {
            self.op_failed(&DaemonError::NotConnected, &to, cx);
            return;
        };
        let rename = daemon.read(cx).request::<fs::Rename>(RenameParams {
            workspace: workspace.clone(),
            root,
            from: from.clone(),
            to: to.clone(),
        });
        self._op = Some(cx.spawn_in(window, async move |this, cx| {
            let result = rename.await;
            this.update_in(cx, |this, window, cx| {
                this.renamed(workspace, root, &from, &to, result, window, cx)
            })
            .ok();
        }));
    }

    #[allow(clippy::too_many_arguments)]
    fn renamed(
        &mut self,
        workspace: WorkspaceId,
        root: RootId,
        from: &str,
        asked: &str,
        result: Result<fs::EntryResult, DaemonError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => return self.op_failed(&e, base_name(asked), cx),
        };
        if matches!(
            self.editing,
            Some(Editing {
                kind: EditKind::Rename { .. },
                ..
            })
        ) {
            self.editing = None;
        }
        if from != entry.path {
            self.history.update(cx, |h, _| {
                h.record(FileOp::Move {
                    root,
                    from: from.to_owned(),
                    to: entry.path.clone(),
                })
            });
        }
        self.apply_move(workspace, root, from, entry.path, window, cx);
    }

    /// `from` is now `to`: show it there, selected, and tell the tabs.
    fn apply_move(
        &mut self,
        workspace: WorkspaceId,
        root: RootId,
        from: &str,
        to: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = fs::EntryResult { path: to };
        self.op_error = None;
        self.rekey(root, from, &entry.path);
        self.relist((root, parent(from).to_owned()), cx);
        let to_dir = (root, parent(&entry.path).to_owned());
        if !self.expanded.contains(&to_dir) {
            // Moved into a closed folder: open it to show where it went.
            self.expanded.insert(to_dir.clone());
            self.load(to_dir.clone(), cx);
        } else {
            self.relist(to_dir, cx);
        }
        self.pending_select = Some((root, entry.path.clone()));
        FileEvents::announce(
            FileEvent::Moved {
                workspace,
                root,
                from: from.to_owned(),
                to: entry.path,
            },
            cx,
        );
        window.focus(&self.focus, cx);
        self.rebuild(cx);
    }

    /// Remember `target` (a file or a folder) for [`paste`](Self::paste).
    pub(crate) fn copy_entry(&mut self, target: Target, cx: &mut Context<Self>) {
        self.menu = None;
        if !target.path.is_empty() {
            self.copied = Some(target);
        }
        cx.notify();
    }

    /// The file or folder Copy remembered.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn copied(&self) -> Option<&Target> {
        self.copied.as_ref()
    }

    /// Copy `target` next to itself (`name copy.ext`) and start renaming
    /// the copy.
    pub(crate) fn duplicate(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.pending_delete = None;
        if target.path.is_empty() {
            return; // a root is not copied
        }
        let dir = (target.root, parent(&target.path).to_owned());
        self.copy_into(target, dir, true, window, cx);
    }

    /// Paste what Copy remembered into folder `dir`; a name taken there
    /// gets ` copy`.
    pub(crate) fn paste(&mut self, dir: DirKey, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.pending_delete = None;
        let Some(from) = self.copied.clone() else {
            return;
        };
        if from.root != dir.0 {
            self.op_error = Some("Copying between workspace folders isn't supported yet.".into());
            cx.notify();
            return;
        }
        self.copy_into(from, dir, false, window, cx);
    }

    /// Send `fs/copy` of `from` into `dir`; `rename` starts a rename of the
    /// copy.
    fn copy_into(
        &mut self,
        from: Target,
        dir: DirKey,
        rename: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.op_error = None;
        let Some((workspace, daemon)) = self.session(cx) else {
            self.op_failed(&DaemonError::NotConnected, base_name(&from.path), cx);
            return;
        };
        let copy = daemon.read(cx).request::<fs::CopyEntry>(CopyParams {
            workspace,
            root: from.root,
            from: from.path.clone(),
            dir: dir.1,
        });
        self._op = Some(cx.spawn_in(window, async move |this, cx| {
            let result = copy.await;
            this.update_in(cx, |this, window, cx| {
                this.copied_to(from, rename, result, window, cx)
            })
            .ok();
        }));
    }

    fn copied_to(
        &mut self,
        from: Target,
        rename: bool,
        result: Result<fs::EntryResult, DaemonError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => return self.op_failed(&e, base_name(&from.path), cx),
        };
        let root = from.root;
        self.history.update(cx, |h, _| {
            h.record(FileOp::Copy {
                root,
                from: from.path.clone(),
                path: entry.path.clone(),
                kind: from.kind,
            })
        });
        // Shown in its folder, opened if it was closed.
        let dir = (root, parent(&entry.path).to_owned());
        self.expanded.insert(dir.clone());
        self.relist(dir, cx);
        self.pending_select = Some((root, entry.path.clone()));
        window.focus(&self.focus, cx);
        self.rebuild(cx);
        if rename {
            self.start_rename(root, &entry.path, window, cx);
        }
    }

    /// An operation failed: say why, and let a name field be edited again.
    fn op_failed(&mut self, error: &DaemonError, name: &str, cx: &mut Context<Self>) {
        if let Some(editing) = self.editing.as_mut() {
            editing.busy = false;
        }
        self.op_error = Some(op_message(error, name));
        cx.notify();
    }

    /// The names of this window's file tabs with unsaved edits in `target`.
    fn dirty_tabs(&self, target: &Target, cx: &App) -> Vec<String> {
        let (Some(workspace), Some(dock)) = (self.workspace.as_ref(), self.dock.upgrade()) else {
            return Vec::new();
        };
        let dock = dock.read(cx);
        dock.tree()
            .tabs()
            .into_iter()
            .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
            .filter_map(|tab| {
                let tab = tab.read(cx);
                (tab.workspace() == &workspace.id
                    && tab.root() == target.root
                    && is_at_or_under(tab.path(), &target.path)
                    && tab.is_dirty())
                .then(|| base_name(tab.path()).to_owned())
            })
            .collect()
    }

    /// Ask before deleting `target`.
    pub(crate) fn request_delete(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.op_error = None;
        if target.path.is_empty() {
            return; // a root: removed from the workspace, not deleted
        }
        let mut dirty = self.dirty_tabs(&target, cx);
        dirty.dedup();
        self.selected = Some((target.root, target.path.clone()));
        self.pending_delete = Some(PendingDelete {
            target,
            dirty,
            permanent: false,
            error: None,
            undo: None,
            busy: false,
        });
        // Enter confirms, Escape cancels.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((workspace, daemon)) = self.session(cx) else {
            return;
        };
        let Some(pending) = self.pending_delete.as_mut() else {
            return;
        };
        if pending.busy {
            return;
        }
        pending.busy = true;
        let target = pending.target.clone();
        let delete = daemon.read(cx).request::<fs::Delete>(DeleteParams {
            workspace: workspace.clone(),
            root: target.root,
            path: target.path.clone(),
            permanent: pending.permanent,
        });
        self._op = Some(cx.spawn_in(window, async move |this, cx| {
            let result = delete.await;
            this.update_in(cx, |this, window, cx| {
                this.deleted(workspace, target, result, window, cx)
            })
            .ok();
        }));
        cx.notify();
    }

    fn deleted(
        &mut self,
        workspace: WorkspaceId,
        target: Target,
        result: Result<fs::DeleteResult, DaemonError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = base_name(&target.path).to_owned();
        match result {
            Ok(result) => {
                let undo = self.pending_delete.take().and_then(|p| p.undo);
                match undo {
                    // Undoing a creation, confirmed: it can be made again.
                    Some(op) => self.undone(op, cx),
                    None => {
                        let op = FileOp::Delete {
                            root: target.root,
                            path: target.path.clone(),
                            kind: target.kind,
                            restore: result.restore.ok_or_else(|| {
                                result
                                    .no_restore
                                    .unwrap_or_else(|| format!("“{name}” can't be restored."))
                            }),
                        };
                        self.history.update(cx, |h, _| h.record(op));
                    }
                }
                self.apply_delete(workspace, target, window, cx);
            }
            Err(e) => {
                if let Some(pending) = self.pending_delete.as_mut() {
                    pending.busy = false;
                    if e.code() == Some(codes::TRASH_FAILED) {
                        pending.permanent = true;
                        pending.error = Some(format!(
                            "“{name}” can't be moved to the trash. Delete it permanently instead?"
                        ));
                    } else {
                        pending.error = Some(op_message(&e, &name));
                    }
                }
                cx.notify();
            }
        }
    }

    /// `target` is gone: drop it from the tree and tell the tabs.
    fn apply_delete(
        &mut self,
        workspace: WorkspaceId,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if target.kind == EntryKind::Dir {
            self.forget(&(target.root, target.path.clone()));
        }
        self.relist((target.root, parent(&target.path).to_owned()), cx);
        FileEvents::announce(
            FileEvent::Deleted {
                workspace,
                root: target.root,
                path: target.path,
            },
            cx,
        );
        window.focus(&self.focus, cx);
        self.rebuild(cx);
    }

    /// A row was dropped on folder `dir`: move it there.
    fn drop_on(
        &mut self,
        root: RootId,
        dir: String,
        entry: &DraggedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if entry.root != root {
            self.op_error = Some("Moving between workspace folders isn't supported yet.".into());
            cx.notify();
            return;
        }
        if parent(&entry.path) == dir || is_at_or_under(&dir, &entry.path) {
            return; // already there, or into itself
        }
        let to = join(&dir, base_name(&entry.path));
        self.rename_entry(root, entry.path.clone(), to, window, cx);
    }

    fn copy_path(&mut self, target: &Target, relative: bool, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let text = if relative {
            Some(target.path.clone())
        } else {
            self.absolute(target.root, &target.path)
                .map(|p| p.to_string_lossy().into_owned())
        };
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy_selected(&mut self, relative: bool, cx: &mut Context<Self>) {
        if let Some(target) = self.selected_target() {
            self.copy_path(&target, relative, cx);
        }
    }

    /// Show `path` under `root`: expand the folders it is in (listing the
    /// ones not listed yet), then select it and scroll to it, now or once
    /// it's listed. Whether it shows now.
    pub(crate) fn reveal_entry(
        &mut self,
        root: RootId,
        path: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        self.menu = None;
        let folders = std::iter::once("").chain(path.match_indices('/').map(|(ix, _)| &path[..ix]));
        for folder in folders {
            let key = (root, folder.to_owned());
            if self.expanded.insert(key.clone()) && !self.dirs.contains_key(&key) {
                self.load(key, cx);
            }
        }
        self.pending_select = Some((root, path.to_owned()));
        self.rebuild(cx);
        self.pending_select.is_none()
    }

    fn reveal(&mut self, target: &Target, cx: &mut Context<Self>) {
        self.close_menu(cx);
        if let Some(path) = self.absolute(target.root, &target.path) {
            cx.reveal_path(&path);
        }
    }

    // ---- rendering ------------------------------------------------------------

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let selected = self.selected_index();
        let multi_root = self.multi_root();
        let decorations: Vec<RowDecoration> =
            range.clone().map(|ix| self.decoration(ix, cx)).collect();
        range
            .zip(decorations)
            .filter_map(|(ix, decoration)| {
                let row = self.rows.get(ix)?;
                let is_selected = selected == Some(ix);
                let name_color = decoration.tint.map(|tone| tone.rgb(&p));
                let color = if row.ignored {
                    p.text_subtle
                } else if is_selected {
                    p.text
                } else {
                    p.text_muted
                };
                let lead = if row.is_dir() {
                    let glyph = if row.editing {
                        icon(Icon::Folder, px(13.), p.text_subtle.hsla())
                    } else {
                        let chevron = if row.expanded {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        };
                        icon(chevron, px(12.), p.text_subtle.hsla())
                    };
                    div()
                        .w(px(16.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(glyph)
                        .into_any_element()
                } else if row.editing && row.name.is_empty() {
                    div()
                        .w(px(16.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(Icon::File, px(13.), p.text_subtle.hsla()))
                        .into_any_element()
                } else {
                    super::glyph::render(&row.name, cx)
                };
                let base = div()
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
                    .child(lead);
                if row.editing {
                    let field = self.render_name_field(&p, cx)?;
                    return Some(
                        base.bg(p.surface_selected.hsla())
                            .text_color(p.text.hsla())
                            .child(field)
                            .into_any_element(),
                    );
                }
                let target = row.target();
                let drop_dir = if row.is_dir() {
                    row.path.clone()
                } else {
                    parent(&row.path).to_owned()
                };
                let root = row.root;
                let mut el = base
                    .cursor_pointer()
                    .when(is_selected, |d| d.bg(p.surface_selected.hsla()))
                    .when(!is_selected, |d| d.hover(|s| s.bg(p.surface.hsla())))
                    .when(row.ignored, |d| d.opacity(0.75))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .when_some(name_color, |d, c| d.text_color(c.hsla()))
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
                    .when_some(decoration.badge, |d, badge| {
                        d.child(
                            div()
                                .id(("file-row-badge", ix))
                                .test_support()
                                .flex_none()
                                .min_w(px(10.))
                                .flex()
                                .justify_end()
                                .text_color(badge.tone.rgb(&p).hsla())
                                .child(badge.letter.to_string()),
                        )
                    })
                    .on_click(
                        cx.listener(move |this, e, window, cx| this.click_row(ix, e, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            this.open_menu(target.clone(), e.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .drag_over::<DraggedEntry>({
                        let bg = p.blue.alpha(0.16);
                        move |s, _, _, _| s.bg(bg)
                    })
                    .on_drop(cx.listener(move |this, entry: &DraggedEntry, window, cx| {
                        this.drop_on(root, drop_dir.clone(), entry, window, cx);
                    }));
                // A root's own row stays put.
                if !(row.path.is_empty() && multi_root) {
                    el = el.on_drag(
                        DraggedEntry {
                            root: row.root,
                            path: row.path.clone(),
                            name: row.name.clone(),
                            kind: row.kind,
                        },
                        |entry, _, _, cx| cx.new(|_| entry.clone()),
                    );
                }
                Some(el.into_any_element())
            })
            .collect()
    }

    /// The row a name is typed in.
    fn render_name_field(
        &self,
        p: &coqo_ui::Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editing = self.editing.as_ref()?;
        let border = if self.op_error.is_some() {
            p.rose.hsla()
        } else {
            p.blue.alpha(0.6)
        };
        Some(
            div()
                .id("file-name-field")
                .test_support()
                .flex_1()
                .min_w_0()
                .h(px(ROW_HEIGHT - 4.))
                .flex()
                .items_center()
                .px(px(6.))
                .rounded(px(6.))
                .bg(p.panel_raised.hsla())
                .border_1()
                .border_color(border)
                .overflow_hidden()
                .on_mouse_down_out(
                    cx.listener(|this, _, window, cx| this.finish_edit(true, window, cx)),
                )
                .child(editing.input.clone())
                .into_any_element(),
        )
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

    /// The delete confirmation, or the last operation's error, above the
    /// rows.
    fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        if let Some(pending) = &self.pending_delete {
            return Some(self.render_delete(pending, cx));
        }
        let error = self.op_error.clone()?;
        Some(
            div()
                .id("file-op-error")
                .test_support()
                .flex()
                .items_start()
                .gap(px(8.))
                .mx(px(2.))
                .px(px(10.))
                .py(px(7.))
                .rounded(m.radius_field)
                .bg(p.surface.hsla())
                .text_size(m.text_xs)
                .text_color(p.text.hsla())
                .child(icon(Icon::CircleAlert, px(13.), p.rose.hsla()))
                .child(div().flex_1().min_w_0().child(error))
                .into_any_element(),
        )
    }

    fn render_delete(&self, pending: &PendingDelete, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let name = base_name(&pending.target.path).to_owned();
        let title = if pending.undo.is_some() {
            format!("Undo the creation of “{name}”?")
        } else if pending.target.kind == EntryKind::Dir {
            format!("Delete the folder “{name}”?")
        } else {
            format!("Delete “{name}”?")
        };
        let detail = match (&pending.error, pending.permanent, pending.target.kind) {
            (Some(error), _, _) => error.clone(),
            (None, _, _) if pending.undo.is_some() => {
                "It has changed since it was created. It moves to the trash.".to_owned()
            }
            (None, _, EntryKind::Dir) => "It moves to the trash with everything in it.".to_owned(),
            (None, _, _) => "It moves to the trash.".to_owned(),
        };
        let unsaved = (!pending.dirty.is_empty()).then(|| {
            format!(
                "Unsaved edits in {} stay in the open tab; save to write the file again.",
                pending.dirty.join(", ")
            )
        });
        let button = |id: &'static str, label: &'static str, fill: Option<Rgb>, cx: &App| {
            let el = div()
                .px(px(12.))
                .py(px(5.))
                .rounded(m.radius_control)
                .text_size(m.text_sm)
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
        let confirm_label = if pending.permanent {
            "Delete Permanently"
        } else {
            "Move to Trash"
        };
        div()
            .id("file-delete-confirm")
            .test_support()
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.))
            .mx(px(2.))
            .p(px(12.))
            .rounded(m.radius_field)
            .bg(p.surface.hsla())
            .border_1()
            .border_color(p.border_strong.hsla())
            .text_size(m.text_sm)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(icon(Icon::Trash, px(14.), p.rose.hsla()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child(title),
                    ),
            )
            .child(
                div()
                    .text_size(m.text_xs)
                    .text_color(p.text_muted.hsla())
                    .child(detail),
            )
            .children(unsaved.map(|text| {
                div()
                    .id("file-delete-unsaved")
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .text_size(m.text_xs)
                    .text_color(p.text_muted.hsla())
                    .child(icon(Icon::CircleAlert, px(12.), p.sand.hsla()))
                    .child(div().flex_1().min_w_0().child(text))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(6.))
                    .child(button("file-delete-cancel", "Cancel", None, cx).on_click(
                        cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
                    ))
                    .child(
                        button("file-delete-ok", confirm_label, Some(p.rose), cx)
                            .when(pending.busy, |b| b.opacity(0.6))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_menu(&self, menu: &Menu, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let target = menu.target.clone();
        let is_root = target.path.is_empty();
        let item =
            |id: &'static str, glyph: Icon, label: &'static str, key: Option<&'static str>| {
                div()
                    .id(id)
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .mx(px(6.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_color(p.text.hsla())
                    .hover(|s| s.bg(p.surface_selected.hsla()))
                    .child(icon(glyph, px(14.), p.text_muted.hsla()))
                    .child(div().flex_1().child(label))
                    .children(key.map(|key| {
                        div()
                            .text_size(m.text_xs)
                            .text_color(p.text_subtle.hsla())
                            .child(key)
                    }))
            };
        let separator = || {
            div()
                .h(px(1.))
                .mx(px(12.))
                .my(px(4.))
                .bg(p.border_strong.hsla())
        };
        let folder = target.folder();
        let (new_file, new_folder) = (folder.clone(), folder);
        let (rename, delete, copy, copy_rel, reveal) = (
            target.clone(),
            target.clone(),
            target.clone(),
            target.clone(),
            target.clone(),
        );
        let (duplicate, copy_entry, paste_into) = (target.clone(), target.clone(), target.folder());
        let can_paste = self.copied.is_some();
        let undo_rows = self.render_undo_items(cx);
        let mut card = popover(cx)
            .id("file-menu")
            .occlude()
            .test_support()
            .w(px(230.))
            .py(px(6.))
            .text_size(m.text_sm)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)));
        if !undo_rows.is_empty() {
            card = card.children(undo_rows).child(separator());
        }
        card = card
            .child(
                item("file-menu-new-file", Icon::Plus, "New File…", None).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.start_create(new_file.clone(), EntryKind::File, window, cx)
                    },
                )),
            )
            .child(
                item("file-menu-new-folder", Icon::Folder, "New Folder…", None).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.start_create(new_folder.clone(), EntryKind::Dir, window, cx)
                    }),
                ),
            );
        if !is_root {
            card = card.child(separator()).child(
                item("file-menu-rename", Icon::FilePen, "Rename…", Some("F2")).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.start_rename(rename.root, &rename.path, window, cx)
                    }),
                ),
            );
        }
        if !is_root || can_paste {
            card = card.child(separator());
        }
        if !is_root {
            card = card
                .child(
                    item("file-menu-duplicate", Icon::File, "Duplicate", None).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.duplicate(duplicate.clone(), window, cx)
                        }),
                    ),
                )
                .child(item("file-menu-copy", Icon::File, "Copy", None).on_click(
                    cx.listener(move |this, _, _, cx| this.copy_entry(copy_entry.clone(), cx)),
                ));
        }
        if can_paste {
            card = card.child(item("file-menu-paste", Icon::File, "Paste", None).on_click(
                cx.listener(move |this, _, window, cx| this.paste(paste_into.clone(), window, cx)),
            ));
        }
        card = card
            .child(separator())
            .child(
                item("file-menu-copy-path", Icon::File, "Copy Path", None)
                    .on_click(cx.listener(move |this, _, _, cx| this.copy_path(&copy, false, cx))),
            )
            .child(
                item(
                    "file-menu-copy-relative-path",
                    Icon::File,
                    "Copy Relative Path",
                    None,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.copy_path(&copy_rel, true, cx))),
            )
            .child(
                item("file-menu-reveal", Icon::FolderOpen, REVEAL_LABEL, None)
                    .on_click(cx.listener(move |this, _, _, cx| this.reveal(&reveal, cx))),
            );
        if !is_root {
            card = card.child(separator()).child(
                div()
                    .id("file-menu-delete")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .mx(px(6.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_color(p.rose.hsla())
                    .hover(|s| s.bg(p.surface_selected.hsla()))
                    .child(icon(Icon::Trash, px(14.), p.rose.hsla()))
                    .child(div().flex_1().child("Delete…"))
                    .child(
                        div()
                            .text_size(m.text_xs)
                            .text_color(p.text_subtle.hsla())
                            .child(DELETE_KEY_LABEL),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_delete(delete.clone(), window, cx)
                    })),
            );
        }
        deferred(anchored().position(menu.at).snap_to_window().child(card))
            .with_priority(3)
            .into_any_element()
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
        let banner = self.render_banner(cx);
        let menu = self.menu.as_ref().map(|menu| self.render_menu(menu, cx));
        let toast = self.render_toast(cx);
        // The space below the rows: right-click and drops reach the root.
        let root = self
            .workspace
            .as_ref()
            .filter(|w| w.roots.len() == 1)
            .map(|w| w.roots[0].id);
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
        let mut body = div()
            .id("file-tree-body")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .children(list);
        if let Some(root) = root {
            let bg = cx.tokens().palette.blue.alpha(0.06);
            body = body
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                        let target = Target {
                            root,
                            path: String::new(),
                            kind: EntryKind::Dir,
                        };
                        this.open_menu(target, e.position, cx);
                    }),
                )
                .drag_over::<DraggedEntry>(move |s, _, _, _| s.bg(bg))
                .on_drop(cx.listener(move |this, entry: &DraggedEntry, window, cx| {
                    this.drop_on(root, String::new(), entry, window, cx);
                }));
        }
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
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::rename_selected))
            .on_action(cx.listener(Self::delete_selected))
            .on_action(cx.listener(Self::duplicate_selected))
            .on_action(cx.listener(Self::copy_selected_entry))
            .on_action(cx.listener(Self::paste_selected))
            .on_action(cx.listener(|this, _: &CopyPath, _, cx| this.copy_selected(false, cx)))
            .on_action(
                cx.listener(|this, _: &CopyRelativePath, _, cx| this.copy_selected(true, cx)),
            )
            .on_action(cx.listener(|this, _: &RevealInFileManager, _, cx| {
                if let Some(target) = this.selected_target() {
                    this.reveal(&target, cx);
                }
            }))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_label(label, cx))
            .children(banner)
            .children(note)
            .child(body)
            .children(toast)
            .children(menu)
    }
}

#[cfg(test)]
mod tests {
    use super::{base_name, is_inside, join, parent};

    #[test]
    fn path_helpers() {
        assert_eq!(join("", "src"), "src");
        assert_eq!(join("src", "main.rs"), "src/main.rs");
        assert_eq!(parent("src/main.rs"), "src");
        assert_eq!(parent("README.md"), "");
        assert_eq!(base_name("src/main.rs"), "main.rs");
        assert_eq!(base_name("README.md"), "README.md");
        assert!(is_inside("src/main.rs", "src"));
        assert!(is_inside("src", ""));
        assert!(!is_inside("src", "src"));
        assert!(!is_inside("srcx/a", "src"));
        assert!(!is_inside("", ""));
    }
}
