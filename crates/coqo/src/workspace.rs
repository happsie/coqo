// SPDX-License-Identifier: Apache-2.0

//! The window's current workspace (PLAN §4.2), as the daemon knows it.
//!
//! [`Workspaces`] opens folders through `workspace/open`, restores the most
//! recently opened workspace on start, reopens the current one after the
//! daemon reconnects, and forwards `fs/changes` for it as
//! [`WorkspacesEvent::FsChanged`], so views share one subscription.

use std::path::{Path, PathBuf};

use coqo_protocol::methods::{
    fs::{self, ChangesParams, FsChanges},
    workspace::{self, OpenParams, WorkspaceId, WorkspaceInfo},
};
use coqo_ui::gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, KeyBinding,
    PathPromptOptions, Subscription, Task,
};

use crate::daemon::{Daemon, DaemonError, DaemonEvent, EventStream};

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

/// Emitted by [`Workspaces`].
#[derive(Debug, Clone, PartialEq)]
pub enum WorkspacesEvent {
    /// The current workspace was opened or reopened (also after a daemon
    /// reconnect): views re-fetch what they show.
    CurrentChanged,
    /// Paths changed in the current workspace (`fs/changes`).
    FsChanged {
        /// The workspace.
        workspace: WorkspaceId,
        /// What changed.
        changes: FsChanges,
    },
}

/// The current workspace and the open ones (for the rail).
pub struct Workspaces {
    current: Option<WorkspaceInfo>,
    open: Vec<WorkspaceInfo>,
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

struct GlobalWorkspaces(Entity<Workspaces>);

impl Global for GlobalWorkspaces {}

/// Create the app's [`Workspaces`]. With `path`, that folder is opened once
/// the daemon is up; without, the most recently opened workspace is
/// restored. Call after [`daemon::init`](crate::daemon::init).
pub fn init(path: Option<PathBuf>, cx: &mut App) -> Entity<Workspaces> {
    cx.bind_keys([KeyBinding::new(OPEN_FOLDER_KEY, OpenFolder, None)]);
    let workspaces = cx.new(|cx| Workspaces::new(path, cx));
    cx.set_global(GlobalWorkspaces(workspaces.clone()));
    workspaces
}

/// Show the platform folder picker and open the chosen folder.
pub fn prompt_open_folder(cx: &mut App) {
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
            cx.update(|cx| {
                if let Some(workspaces) = Workspaces::global(cx) {
                    workspaces.update(cx, |w, cx| w.open_path(path, cx));
                }
            });
        }
    })
    .detach();
}

impl Workspaces {
    fn new(path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        let mut connected = false;
        if let Some(daemon) = Daemon::global(cx) {
            connected = daemon.read(cx).is_connected();
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| match event {
                DaemonEvent::Connected => this.on_connected(cx),
                DaemonEvent::Disconnected => this._changes = None,
            }));
        }
        let mut this = Self {
            current: None,
            open: Vec::new(),
            restore_last: path.is_none(),
            pending: path.map(OpenRequest::Path),
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

    /// The app's workspaces, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<Workspaces>> {
        cx.try_global::<GlobalWorkspaces>().map(|g| g.0.clone())
    }

    /// The workspace shown in the window.
    pub fn current(&self) -> Option<&WorkspaceInfo> {
        self.current.as_ref()
    }

    /// Open workspaces, most recently opened first.
    pub fn open_workspaces(&self) -> &[WorkspaceInfo] {
        &self.open
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
            let Ok(Some(daemon)) = this.update(cx, |_, cx| Daemon::global(cx)) else {
                return;
            };
            let info = match result {
                Ok(info) => info,
                Err(err) => {
                    this.update(cx, |this, cx| this.open_failed(&request, err, cx))
                        .ok();
                    return;
                }
            };
            // Subscribe before views list, so no change slips between.
            let changes = cx
                .update(|cx| {
                    daemon.read(cx).subscribe::<fs::Changes>(ChangesParams {
                        workspace: info.id.clone(),
                    })
                })
                .await;
            let list = cx.update(|cx| daemon.read(cx).request::<workspace::List>(()));
            this.update(cx, |this, cx| this.set_current(info, changes.ok(), cx))
                .ok();
            if let Ok(list) = list.await {
                this.update(cx, |this, cx| {
                    this.open = list.workspaces.into_iter().filter(|w| w.open).collect();
                    cx.notify();
                })
                .ok();
            }
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
        changes: Option<EventStream<FsChanges>>,
        cx: &mut Context<Self>,
    ) {
        self.opening = false;
        self.error = None;
        let id = info.id.clone();
        self.current = Some(info);
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
