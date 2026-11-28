// SPDX-License-Identifier: Apache-2.0

//! Workspaces in the UI (PLAN §4.2), as the daemon knows them.
//!
//! - [`KnownWorkspaces`] (one per app) follows every workspace with a
//!   [`Resumable`] over `workspace/list` + `workspace/events`, so the rail of
//!   every window stays current, and a reattach replays what changed.
//! - [`Workspaces`] (one per window) is the window's current workspace. It
//!   opens folders through `workspace/open`, fetches the workspace's state
//!   (`workspace/getState`), reopens it after the daemon reconnects, and
//!   forwards `fs/changes` for it as [`WorkspacesEvent::FsChanged`], so the
//!   window's views share one subscription. A workspace is shown in at most
//!   one window: opening one that another window shows focuses that window.
//! - The rail (`shell::rail`) lists [`KnownWorkspaces::projects`]: one
//!   [`RailProject`] per repository, holding the workspaces of all its
//!   worktrees (ADR 0028), and one per workspace outside any repository.
//!   `Ctrl/Cmd+1..9` ([`SwitchToWorkspace`]) switch the window to one of
//!   them, and [`rename`] and [`remove`] change the list for every window.

use coqo_ui::keys;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use coqo_protocol::methods::{
    fs::{self, ChangesParams, FsChanges},
    workspace::{
        self, EventsParams, ListResult, OpenParams, ProjectInfo, RenameParams, SetStateParams,
        WorkspaceEvent, WorkspaceId, WorkspaceInfo, WorkspaceParams,
    },
};
use coqo_ui::{
    gpui::{
        Action, AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, EntityId,
        EventEmitter, Global, PathPromptOptions, Subscription, Task, WeakEntity, Window,
    },
    serde_json::Value,
};

use crate::{
    daemon::{Daemon, DaemonError, DaemonEvent, EventStream},
    resume::{Resumable, Update},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    workspace,
    [
        /// Pick a folder and open it as the workspace.
        OpenFolder,
    ]
);

/// Switch the window to the rail's `index`th workspace (from 0), as
/// `Ctrl/Cmd+1..9` do.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = workspace, no_json)]
pub struct SwitchToWorkspace {
    pub index: usize,
}

#[cfg(target_os = "macos")]
const OPEN_FOLDER_KEY: &str = "cmd-o";
#[cfg(not(target_os = "macos"))]
const OPEN_FOLDER_KEY: &str = "ctrl-o";
#[cfg(target_os = "macos")]
const SWITCH_KEYS: [&str; 9] = [
    "cmd-1", "cmd-2", "cmd-3", "cmd-4", "cmd-5", "cmd-6", "cmd-7", "cmd-8", "cmd-9",
];
#[cfg(not(target_os = "macos"))]
const SWITCH_KEYS: [&str; 9] = [
    "ctrl-1", "ctrl-2", "ctrl-3", "ctrl-4", "ctrl-5", "ctrl-6", "ctrl-7", "ctrl-8", "ctrl-9",
];

/// What to open once the daemon is reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenRequest {
    Path(PathBuf),
    Id(WorkspaceId),
}

/// What the app's first window opens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstWindow {
    Open(PathBuf),
    RestoreLast,
}

/// Emitted by [`Workspaces`].
#[derive(Debug, Clone, PartialEq)]
pub enum WorkspacesEvent {
    /// The current workspace was opened or reopened (also after a daemon
    /// reconnect, or when its roots changed): views re-fetch what they show.
    CurrentChanged,
    /// Paths changed in the current workspace (`fs/changes`).
    FsChanged {
        /// The workspace.
        workspace: WorkspaceId,
        /// What changed.
        changes: FsChanges,
    },
}

// ---------------------------------------------------------------------------
// Projects (ADR 0028)

/// One rail button: a repository with the workspaces of its worktrees, or
/// one workspace in no repository.
#[derive(Debug, Clone, PartialEq)]
pub struct RailProject {
    /// The project, or `None` for a workspace in no repository.
    pub project: Option<ProjectInfo>,
    /// Its name on the rail: the main worktree's workspace's name if there
    /// is one, else the repository's; the workspace's own without one.
    pub name: String,
    /// Its workspaces, most recently opened first. Never empty.
    pub workspaces: Vec<WorkspaceInfo>,
}

impl RailProject {
    /// What groups workspaces: the project's id, or the workspace's own.
    pub fn key_of(info: &WorkspaceInfo) -> String {
        match &info.project {
            Some(p) => format!("project:{}", p.id),
            None => format!("workspace:{}", info.id),
        }
    }

    /// Group `list` (most recently opened first) into projects, in the
    /// order they were first added, so the buttons and their
    /// `Ctrl/Cmd+1..9` stay put while switching.
    pub fn group(list: &[WorkspaceInfo]) -> Vec<RailProject> {
        let mut groups: Vec<(String, RailProject)> = Vec::new();
        for info in list {
            let key = Self::key_of(info);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, group)) => group.workspaces.push(info.clone()),
                None => groups.push((
                    key,
                    RailProject {
                        project: info.project.clone(),
                        name: String::new(),
                        workspaces: vec![info.clone()],
                    },
                )),
            }
        }
        let mut groups: Vec<RailProject> = groups.into_iter().map(|(_, g)| g).collect();
        for group in &mut groups {
            group.name = match (&group.project, group.main()) {
                (_, Some(main)) => main.name.clone(),
                (Some(project), None) => project.name.clone(),
                (None, None) => group.workspaces[0].name.clone(),
            };
        }
        groups.sort_by_cached_key(|g| {
            g.workspaces
                .iter()
                .map(|w| (w.created_at, w.id.0.clone()))
                .min()
        });
        groups
    }

    /// The workspace the button switches to: the most recently opened.
    pub fn latest(&self) -> &WorkspaceInfo {
        &self.workspaces[0]
    }

    /// The workspace on the main worktree, or the only workspace of one in
    /// no repository.
    pub fn main(&self) -> Option<&WorkspaceInfo> {
        match &self.project {
            Some(project) => {
                let main = project.main_worktree.as_deref()?;
                self.workspaces
                    .iter()
                    .find(|w| w.roots.first().map(|r| r.path.as_str()) == Some(main))
            }
            None => self.workspaces.first(),
        }
    }

    /// Whether workspace `id` is one of its worktrees'.
    pub fn contains(&self, id: &WorkspaceId) -> bool {
        self.workspaces.iter().any(|w| &w.id == id)
    }

    /// The folder to reveal: the main worktree, else the latest workspace's
    /// first root.
    pub fn folder(&self) -> Option<PathBuf> {
        self.project
            .as_ref()
            .and_then(|p| p.main_worktree.clone())
            .or_else(|| self.latest().roots.first().map(|r| r.path.clone()))
            .map(PathBuf::from)
    }
}

// ---------------------------------------------------------------------------
// Every workspace (the app)

/// Every workspace the daemon knows, kept current through `workspace/events`.
pub struct KnownWorkspaces {
    list: Vec<WorkspaceInfo>,
    loaded: bool,
    first_window: Option<FirstWindow>,
    feed: Entity<Resumable<ListResult, WorkspaceEvent>>,
    _feed: Subscription,
}

impl std::fmt::Debug for KnownWorkspaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnownWorkspaces")
            .field("list", &self.list.len())
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

/// Re-emits each `workspace/events` event, after applying it.
impl EventEmitter<WorkspaceEvent> for KnownWorkspaces {}

/// Emitted by [`KnownWorkspaces`] when a window's current workspace was
/// opened or reopened, for app-wide views that follow the focused window
/// ([`Workspaces::global`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowWorkspaceChanged;

impl EventEmitter<WindowWorkspaceChanged> for KnownWorkspaces {}

struct GlobalKnown(Entity<KnownWorkspaces>);

impl Global for GlobalKnown {}

/// The windows' [`Workspaces`], in the order the windows opened.
#[derive(Default)]
struct WindowWorkspaces(Vec<(AnyWindowHandle, WeakEntity<Workspaces>)>);

impl Global for WindowWorkspaces {}

/// Create the app's [`KnownWorkspaces`] and bind the workspace keys. The
/// first window opens `path` once the daemon is up, or, without one,
/// restores the most recently opened workspace. Call after
/// [`daemon::init`](crate::daemon::init).
pub fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<KnownWorkspaces> {
    crate::commands::register(
        cx,
        [crate::commands::Command::new(
            "workspace.open_folder",
            "Open folder…",
            "Workspace",
            OpenFolder,
        )
        .key(OPEN_FOLDER_KEY)],
    );
    cx.bind_keys(
        SWITCH_KEYS
            .iter()
            .enumerate()
            .map(|(index, key)| keys::binding(key, SwitchToWorkspace { index }, None)),
    );
    let first = match path {
        Some(path) => FirstWindow::Open(path),
        None => FirstWindow::RestoreLast,
    };
    let known = cx.new(|cx| KnownWorkspaces::new(first, cx));
    cx.set_global(GlobalKnown(known.clone()));
    known
}

impl KnownWorkspaces {
    fn new(first_window: FirstWindow, cx: &mut Context<Self>) -> Self {
        let feed = cx.new(|cx| {
            Resumable::follow::<workspace::List, workspace::Events>(
                (),
                |list| list.seq,
                EventsParams::default(),
                cx,
            )
        });
        let subscription = cx.subscribe(&feed, |this, _, update, cx| this.update(update, cx));
        Self {
            list: Vec::new(),
            loaded: false,
            first_window: Some(first_window),
            feed,
            _feed: subscription,
        }
    }

    /// The app's workspaces, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<KnownWorkspaces>> {
        cx.try_global::<GlobalKnown>().map(|g| g.0.clone())
    }

    /// Every known workspace, most recently opened first.
    #[allow(dead_code)] // For the workspace switcher; tests use it.
    pub fn all(&self) -> &[WorkspaceInfo] {
        &self.list
    }

    /// The workspace whose first root is the folder `path`.
    pub fn on_folder(&self, path: &str) -> Option<&WorkspaceInfo> {
        self.list
            .iter()
            .find(|w| w.roots.first().map(|r| r.path.as_str()) == Some(path))
    }

    /// Replace the list, for screenshots without a daemon.
    #[cfg(test)]
    pub fn set_for_tests(&mut self, list: Vec<WorkspaceInfo>, cx: &mut Context<Self>) {
        self.list = list;
        self.loaded = true;
        cx.notify();
    }

    /// A known workspace.
    pub fn get(&self, id: &WorkspaceId) -> Option<&WorkspaceInfo> {
        self.list.iter().find(|w| &w.id == id)
    }

    /// Open workspaces (watched by the daemon), most recently opened first.
    #[allow(dead_code)] // Tests use it.
    pub fn open_workspaces(&self) -> impl Iterator<Item = &WorkspaceInfo> {
        self.list.iter().filter(|w| w.open)
    }

    /// Whether the first snapshot arrived.
    #[allow(dead_code)] // Tests use it.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// The feed behind the list, for its `seq` and snapshot count.
    #[allow(dead_code)] // Tests use it.
    pub fn feed(&self) -> &Entity<Resumable<ListResult, WorkspaceEvent>> {
        &self.feed
    }

    fn update(&mut self, update: &Update<ListResult, WorkspaceEvent>, cx: &mut Context<Self>) {
        match update {
            Update::Snapshot(list) => {
                self.list = list.workspaces.clone();
                self.loaded = true;
            }
            Update::Event(event) => {
                self.apply(&event.event);
                cx.emit(event.event.clone());
            }
        }
        cx.notify();
    }

    fn apply(&mut self, event: &WorkspaceEvent) {
        match event {
            WorkspaceEvent::Opened { workspace }
            | WorkspaceEvent::Closed { workspace }
            | WorkspaceEvent::Changed { workspace } => {
                match self.list.iter_mut().find(|w| w.id == workspace.id) {
                    Some(known) => *known = workspace.clone(),
                    None => self.list.push(workspace.clone()),
                }
                self.list
                    .sort_by_key(|w| std::cmp::Reverse(w.last_opened_at));
            }
            WorkspaceEvent::Removed { id } => self.list.retain(|w| &w.id != id),
            WorkspaceEvent::StateChanged { .. } => {}
        }
    }

    fn take_first_window(&mut self) -> Option<FirstWindow> {
        self.first_window.take()
    }
}

// ---------------------------------------------------------------------------
// One window's workspace

/// A window's current workspace.
pub struct Workspaces {
    window: Option<AnyWindowHandle>,
    current: Option<WorkspaceInfo>,
    /// The current workspace's state document (`workspace/getState`), kept
    /// current through `workspace/events`.
    state: BTreeMap<String, Value>,
    pending: Option<OpenRequest>,
    restore_last: bool,
    opening: bool,
    error: Option<String>,
    _opening: Option<Task<()>>,
    _changes: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for Workspaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspaces")
            .field("current", &self.current.as_ref().map(|w| &w.id))
            .field("pending", &self.pending)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<WorkspacesEvent> for Workspaces {}

/// Show the platform folder picker and open the chosen folder in the
/// focused window.
#[allow(dead_code)] // For callers without a window at hand.
pub fn prompt_open_folder(cx: &mut App) {
    if let Some(workspaces) = Workspaces::global(cx) {
        prompt_open_folder_in(workspaces.downgrade(), cx);
    }
}

/// Show the platform folder picker and open the chosen folder in the window
/// of `workspaces`.
pub fn prompt_open_folder_in(workspaces: WeakEntity<Workspaces>, cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn(async move |cx: &mut AsyncApp| {
        let path = match picked.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            Ok(Err(err)) => {
                eprintln!("coqo: couldn't show the folder picker: {err:#}");
                None
            }
            _ => None,
        };
        if let Some(path) = path {
            workspaces.update(cx, |w, cx| w.open_path(path, cx)).ok();
        }
    })
    .detach();
}

impl Workspaces {
    /// The workspace of `window`. The first window opens what [`init`] was
    /// given; later ones start empty.
    pub fn new(window: &Window, cx: &mut Context<Self>) -> Self {
        let handle = window.window_handle();
        let me = cx.weak_entity();
        let registry = cx.default_global::<WindowWorkspaces>();
        registry.0.retain(|(_, w)| w.upgrade().is_some());
        registry.0.push((handle, me));

        let mut subscriptions = Vec::new();
        let mut connected = false;
        if let Some(daemon) = Daemon::global(cx) {
            connected = daemon.read(cx).is_connected();
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| match event {
                DaemonEvent::Connected => this.on_connected(cx),
                DaemonEvent::Disconnected => this._changes = None,
            }));
        }
        let mut first = None;
        if let Some(known) = KnownWorkspaces::global(cx) {
            first = known.update(cx, |known, _| known.take_first_window());
            subscriptions.push(cx.subscribe(&known, |this, _, event, cx| {
                this.on_workspace_event(event, cx);
            }));
            subscriptions.push(cx.observe(&known, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            window: Some(handle),
            current: None,
            state: BTreeMap::new(),
            restore_last: first == Some(FirstWindow::RestoreLast),
            pending: match first {
                Some(FirstWindow::Open(path)) => Some(OpenRequest::Path(path)),
                _ => None,
            },
            opening: false,
            error: None,
            _opening: None,
            _changes: None,
            _subscriptions: subscriptions,
        };
        if connected {
            this.on_connected(cx);
        }
        this
    }

    /// The workspace of `window`.
    pub fn for_window(window: &Window, cx: &App) -> Option<Entity<Workspaces>> {
        Self::for_handle(window.window_handle(), cx)
    }

    /// The workspace of the window `handle`.
    pub fn for_handle(handle: AnyWindowHandle, cx: &App) -> Option<Entity<Workspaces>> {
        cx.try_global::<WindowWorkspaces>()?
            .0
            .iter()
            .find(|(h, _)| *h == handle)
            .and_then(|(_, w)| w.upgrade())
    }

    /// The focused window's workspace (or the last opened window's). Views
    /// inside a window use [`Workspaces::for_window`].
    #[allow(dead_code)] // For callers without a window at hand; tests use it.
    pub fn global(cx: &App) -> Option<Entity<Workspaces>> {
        let windows = &cx.try_global::<WindowWorkspaces>()?.0;
        let active = cx.active_window();
        windows
            .iter()
            .find(|(h, w)| Some(*h) == active && w.upgrade().is_some())
            .or_else(|| windows.iter().rev().find(|(_, w)| w.upgrade().is_some()))
            .and_then(|(_, w)| w.upgrade())
    }

    /// Every window showing workspace `id`, in the order they opened.
    pub fn windows_showing(id: &WorkspaceId, cx: &App) -> Vec<AnyWindowHandle> {
        cx.try_global::<WindowWorkspaces>()
            .map(|windows| {
                windows
                    .0
                    .iter()
                    .filter(|(_, w)| {
                        w.upgrade()
                            .is_some_and(|w| w.read(cx).current().map(|c| &c.id) == Some(id))
                    })
                    .map(|(h, _)| *h)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The window showing workspace `id`, if one does.
    pub fn window_showing(id: &WorkspaceId, cx: &App) -> Option<AnyWindowHandle> {
        Self::window_showing_except(id, None, cx)
    }

    /// The window showing `id`, not counting the [`Workspaces`] `except`
    /// (which may be mid-update, so it can't be read).
    fn window_showing_except(
        id: &WorkspaceId,
        except: Option<EntityId>,
        cx: &App,
    ) -> Option<AnyWindowHandle> {
        cx.try_global::<WindowWorkspaces>()?
            .0
            .iter()
            .filter(|(_, w)| Some(w.entity_id()) != except)
            .find(|(_, w)| {
                w.upgrade()
                    .is_some_and(|w| w.read(cx).current().map(|c| &c.id) == Some(id))
            })
            .map(|(h, _)| *h)
    }

    /// The workspace shown in the window.
    pub fn current(&self) -> Option<&WorkspaceInfo> {
        self.current.as_ref()
    }

    /// Show `info` without asking the daemon, for screenshots.
    #[cfg(test)]
    pub fn set_current_for_tests(&mut self, info: WorkspaceInfo, cx: &mut Context<Self>) {
        self.current = Some(info);
        cx.notify();
    }

    /// An entry of the current workspace's state document.
    pub fn state(&self, key: &str) -> Option<&Value> {
        self.state.get(key)
    }

    /// Whether a `workspace/open` is in flight.
    pub fn is_opening(&self) -> bool {
        self.opening
    }

    /// Why the last open failed, if it did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Open `path` as the current workspace (now, or once connected).
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.open(OpenRequest::Path(path), cx);
    }

    /// Switch to a known workspace.
    pub fn switch_to(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        if self.current.as_ref().map(|w| &w.id) != Some(&id) {
            self.open(OpenRequest::Id(id), cx);
        }
    }

    /// Set (or, with `null`, remove) an entry of the current workspace's
    /// state document. No-op without a current workspace.
    #[allow(dead_code)] // For tabs and settings that live with the workspace.
    pub fn set_state(
        &mut self,
        key: &str,
        value: Value,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), DaemonError>> {
        let Some(current) = &self.current else {
            return Task::ready(Ok(()));
        };
        if value.is_null() {
            self.state.remove(key);
        } else {
            self.state.insert(key.to_owned(), value.clone());
        }
        let Some(daemon) = Self::daemon(cx) else {
            return Task::ready(Err(DaemonError::NotConnected));
        };
        let set = daemon
            .read(cx)
            .request::<workspace::SetState>(SetStateParams {
                id: current.id.clone(),
                key: key.to_owned(),
                value,
            });
        cx.background_spawn(set)
    }

    fn daemon(cx: &App) -> Option<Entity<Daemon>> {
        Daemon::global(cx).filter(|d| d.read(cx).is_connected())
    }

    fn on_connected(&mut self, cx: &mut Context<Self>) {
        let request = self
            .pending
            .take()
            .or_else(|| self.current.as_ref().map(|w| OpenRequest::Id(w.id.clone())));
        match request {
            Some(request) => self.open(request, cx),
            None if self.restore_last => self.restore(cx),
            None => {}
        }
    }

    fn on_workspace_event(&mut self, event: &WorkspaceEvent, cx: &mut Context<Self>) {
        let Some(current) = &self.current else {
            return;
        };
        match event {
            // Forgotten (here or by another client): show no workspace.
            WorkspaceEvent::Removed { id } if *id == current.id => {
                self.current = None;
                self.state.clear();
                self._changes = None;
                cx.emit(WorkspacesEvent::CurrentChanged);
                if let Some(known) = KnownWorkspaces::global(cx) {
                    known.update(cx, |_, cx| cx.emit(WindowWorkspaceChanged));
                }
            }
            WorkspaceEvent::Changed { workspace } if workspace.id == current.id => {
                self.current = Some(workspace.clone());
                cx.emit(WorkspacesEvent::CurrentChanged);
            }
            WorkspaceEvent::Opened { workspace } | WorkspaceEvent::Closed { workspace }
                if workspace.id == current.id =>
            {
                self.current = Some(workspace.clone());
            }
            WorkspaceEvent::StateChanged { id, key, value } if *id == current.id => {
                if value.is_null() {
                    self.state.remove(key);
                } else {
                    self.state.insert(key.clone(), value.clone());
                }
            }
            _ => return,
        }
        cx.notify();
    }

    /// If another window shows `id`, focus it and return `true`.
    fn focus_other_window(&self, id: &WorkspaceId, cx: &mut Context<Self>) -> bool {
        let me = cx.entity_id();
        let Some(other) =
            Self::window_showing_except(id, Some(me), cx).filter(|h| Some(*h) != self.window)
        else {
            return false;
        };
        other
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    }

    /// Reopen the most recently opened workspace.
    fn restore(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let list = daemon.read(cx).request::<workspace::List>(());
        self.opening = true;
        self._opening = Some(cx.spawn(async move |this, cx| {
            let last = match list.await {
                Ok(list) => list.workspaces.into_iter().next(),
                Err(err) => {
                    eprintln!("coqo: couldn't list workspaces: {err}");
                    None
                }
            };
            this.update(cx, |this, cx| {
                this.opening = false;
                if this.current.is_none()
                    && this.pending.is_none()
                    && let Some(last) = last
                {
                    this.open(OpenRequest::Id(last.id), cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open(&mut self, request: OpenRequest, cx: &mut Context<Self>) {
        self.restore_last = false;
        if let OpenRequest::Id(id) = &request
            && self.current.as_ref().map(|w| &w.id) != Some(id)
            && self.focus_other_window(id, cx)
        {
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            self.pending = Some(request);
            return;
        };
        let params = match &request {
            OpenRequest::Path(path) => OpenParams {
                path: Some(absolute(path).to_string_lossy().into_owned()),
                ..OpenParams::default()
            },
            OpenRequest::Id(id) => OpenParams {
                id: Some(id.clone()),
                ..OpenParams::default()
            },
        };
        let opened = daemon.read(cx).request::<workspace::Open>(params);
        self.opening = true;
        cx.notify();
        self._opening = Some(cx.spawn(async move |this, cx| {
            let result = opened.await;
            let info = match result {
                Ok(info) => info,
                Err(err) => {
                    this.update(cx, |this, cx| this.open_failed(&request, err, cx))
                        .ok();
                    return;
                }
            };
            // Shown in another window: focus that one instead.
            let elsewhere = this.update(cx, |this, cx| {
                let elsewhere = this.current.as_ref().map(|w| &w.id) != Some(&info.id)
                    && this.focus_other_window(&info.id, cx);
                if elsewhere {
                    this.opening = false;
                    cx.notify();
                }
                elsewhere
            });
            if elsewhere.unwrap_or(true) {
                return;
            }
            let Ok(Some(daemon)) = this.update(cx, |_, cx| Daemon::global(cx)) else {
                return;
            };
            // Subscribe before views list, so no change slips between. The
            // state comes at the same time: one round trip, not two.
            let (changes, state) = cx.update(|cx| {
                let daemon = daemon.read(cx);
                let changes = daemon.subscribe::<fs::Changes>(ChangesParams {
                    workspace: info.id.clone(),
                });
                let state = daemon.request::<workspace::GetState>(WorkspaceParams {
                    id: info.id.clone(),
                });
                (changes, state)
            });
            let (changes, state) = futures_util::future::join(changes, state).await;
            let state = match state {
                Ok(state) => state.entries,
                Err(err) => {
                    eprintln!("coqo: couldn't read the state of {}: {err}", info.id);
                    BTreeMap::new()
                }
            };
            this.update(cx, |this, cx| {
                this.set_current(info, state, changes.ok(), cx)
            })
            .ok();
        }));
    }

    fn open_failed(&mut self, request: &OpenRequest, err: DaemonError, cx: &mut Context<Self>) {
        self.opening = false;
        let what = match request {
            OpenRequest::Path(path) => path.display().to_string(),
            OpenRequest::Id(id) => id.to_string(),
        };
        eprintln!("coqo: couldn't open {what}: {err}");
        if err == DaemonError::NotConnected {
            self.pending = Some(request.clone());
        } else {
            self.error = Some(format!("Couldn't open {what}: {err}"));
        }
        cx.notify();
    }

    fn set_current(
        &mut self,
        info: WorkspaceInfo,
        state: BTreeMap<String, Value>,
        changes: Option<EventStream<FsChanges>>,
        cx: &mut Context<Self>,
    ) {
        self.opening = false;
        self.error = None;
        let id = info.id.clone();
        self.current = Some(info);
        self.state = state;
        self._changes = changes.map(|mut stream| {
            cx.spawn(async move |this, cx| {
                while let Some(Ok(changes)) = stream.next().await {
                    let event = WorkspacesEvent::FsChanged {
                        workspace: id.clone(),
                        changes,
                    };
                    if this.update(cx, |_, cx| cx.emit(event)).is_err() {
                        break;
                    }
                }
            })
        });
        cx.emit(WorkspacesEvent::CurrentChanged);
        if let Some(known) = KnownWorkspaces::global(cx) {
            known.update(cx, |_, cx| cx.emit(WindowWorkspaceChanged));
        }
        cx.notify();
    }
}

/// Rename workspace `id` (`workspace/rename`). Every rail follows through
/// `workspace/events`. A blank name is ignored.
pub fn rename(id: WorkspaceId, name: &str, cx: &mut App) {
    let name = name.trim().to_owned();
    let Some(daemon) = Workspaces::daemon(cx) else {
        return;
    };
    if name.is_empty() {
        return;
    }
    let renamed = daemon
        .read(cx)
        .request::<workspace::Rename>(RenameParams { id, name });
    cx.background_spawn(async move {
        if let Err(err) = renamed.await {
            eprintln!("coqo: couldn't rename the workspace: {err}");
        }
    })
    .detach();
}

/// Forget workspace `id` (`workspace/remove`): it leaves every rail, and a
/// window showing it shows no workspace. Its folders are never touched.
pub fn remove(id: WorkspaceId, cx: &mut App) {
    let Some(daemon) = Workspaces::daemon(cx) else {
        return;
    };
    let removed = daemon
        .read(cx)
        .request::<workspace::Remove>(WorkspaceParams { id });
    cx.background_spawn(async move {
        if let Err(err) = removed.await {
            eprintln!("coqo: couldn't remove the workspace: {err}");
        }
    })
    .detach();
}

/// `path` made absolute against the working directory: the daemon runs
/// elsewhere, so relative paths mean nothing to it.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Short initials for a workspace's rail button: `checkout-service` → `CS`,
/// `coqo` → `CO`.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let letters: String = match words.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect(),
        [first, second, ..] => first
            .chars()
            .take(1)
            .chain(second.chars().take(1))
            .collect(),
    };
    letters.to_uppercase()
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::workspace::{
        ProjectInfo, RootId, WorkspaceId, WorkspaceInfo, WorkspaceRoot,
    };

    use super::{RailProject, initials};

    fn ws(id: &str, folder: &str, created: i64, opened: i64, git: Option<&str>) -> WorkspaceInfo {
        WorkspaceInfo {
            id: WorkspaceId(id.into()),
            name: folder.rsplit('/').next().unwrap().into(),
            roots: vec![WorkspaceRoot {
                id: RootId(1),
                path: folder.into(),
                name: folder.into(),
            }],
            created_at: created,
            last_opened_at: opened,
            open: true,
            project: git.map(ProjectInfo::from_common_dir),
        }
    }

    #[test]
    fn worktrees_of_a_repository_share_one_rail_button() {
        let shop = Some("/code/shop/.git");
        // Most recently opened first, as the daemon lists them.
        let list = vec![
            ws("ws-hotfix", "/code/shop-hotfix", 4, 40, shop),
            ws("ws-notes", "/code/notes", 2, 30, None),
            ws("ws-main", "/code/shop", 1, 20, shop),
            ws("ws-feature", "/code/shop-worktrees/feature", 3, 10, shop),
            ws("ws-api", "/code/api", 5, 5, Some("/code/api.git")),
        ];
        let groups = RailProject::group(&list);
        let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
        // In the order first added; the main worktree names the project.
        assert_eq!(names, ["shop", "notes", "api"]);
        let shop = &groups[0];
        let ids: Vec<_> = shop.workspaces.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["ws-hotfix", "ws-main", "ws-feature"]);
        assert_eq!(shop.latest().id.0, "ws-hotfix");
        assert_eq!(shop.main().unwrap().id.0, "ws-main");
        assert_eq!(shop.folder().unwrap().to_str(), Some("/code/shop"));
        assert!(shop.contains(&WorkspaceId("ws-feature".into())));
        // A bare repository's project is named without `.git`, and has no
        // main worktree to rename.
        assert!(groups[2].main().is_none());
        assert_eq!(groups[2].name, "api");
        // Without the main worktree, the repository names it.
        let groups = RailProject::group(&list[..1]);
        assert_eq!(groups[0].name, "shop");
        assert!(groups[0].main().is_none());
    }

    #[test]
    fn initials_from_names() {
        assert_eq!(initials("checkout-service"), "CS");
        assert_eq!(initials("coqo"), "CO");
        assert_eq!(initials("web storefront app"), "WS");
        assert_eq!(initials("x"), "X");
        assert_eq!(initials("--"), "?");
    }
}
