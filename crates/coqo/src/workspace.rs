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

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use coqo_protocol::methods::{
    fs::{self, ChangesParams, FsChanges},
    workspace::{
        self, EventsParams, ListResult, OpenParams, SetStateParams, WorkspaceEvent, WorkspaceId,
        WorkspaceInfo, WorkspaceParams,
    },
};
use coqo_ui::{
    gpui::{
        AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter,
        Global, KeyBinding, PathPromptOptions, Subscription, Task, WeakEntity, Window,
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

#[cfg(target_os = "macos")]
const OPEN_FOLDER_KEY: &str = "cmd-o";
#[cfg(not(target_os = "macos"))]
const OPEN_FOLDER_KEY: &str = "ctrl-o";

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
    cx.bind_keys([KeyBinding::new(OPEN_FOLDER_KEY, OpenFolder, None)]);
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

    /// Open workspaces (the rail), most recently opened first.
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
        let handle = window.window_handle();
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

    /// The window showing workspace `id`, if one does.
    #[allow(dead_code)] // Tests use it.
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
            // Subscribe before views list, so no change slips between.
            let changes = cx
                .update(|cx| {
                    daemon.read(cx).subscribe::<fs::Changes>(ChangesParams {
                        workspace: info.id.clone(),
                    })
                })
                .await;
            let state = cx
                .update(|cx| {
                    daemon
                        .read(cx)
                        .request::<workspace::GetState>(WorkspaceParams {
                            id: info.id.clone(),
                        })
                })
                .await;
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
    use super::initials;

    #[test]
    fn initials_from_names() {
        assert_eq!(initials("checkout-service"), "CS");
        assert_eq!(initials("coqo"), "CO");
        assert_eq!(initials("web storefront app"), "WS");
        assert_eq!(initials("x"), "X");
        assert_eq!(initials("--"), "?");
    }
}
