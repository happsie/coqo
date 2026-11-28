// SPDX-License-Identifier: Apache-2.0

//! The in-app updater's UI (issue #349, ADR 0031): the status bar's
//! "Update to vX.Y.Z", its popover with the release notes and **Update and
//! restart**, the palette commands, the relaunch, and the launcher's
//! rollback.
//!
//! The daemon does the checking, downloading and verifying (`update/*`);
//! [`Updates`] follows its status. **Update and restart** asks the daemon
//! to swap the verified release in (`update/install`, which also arms
//! `daemon/restartWhenIdle`), then starts the new `coqo` detached and
//! quits. The new UI attaches to the old daemon while agents finish their
//! turns ("Restarting the daemon when agents are idle (N running)", with a
//! **Restart now** that warns first); when the daemon exits, the UI
//! reconnects and starts the new `coqod` through the normal spawn path.
//!
//! If that new daemon exits during start-up, [`after_connect_failure`]
//! puts the previous version back (once), records why, and relaunches it;
//! the restored version shows the reason.

pub mod view;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use coqo_protocol::daemon::{RestartParams, RestartWhenIdle};
use coqo_protocol::methods::update::{self as um, InstallParams, UpdateState, UpdateStatus};
use coqo_ui::{
    gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Subscription, Task},
    slots::{Slot, Slots},
};
use coqo_updater::Install;
use coqo_updater::records::{UpdateDir, roll_back_failed_start};

use crate::daemon::{Daemon, DaemonEvent};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    update,
    [
        /// Ask the daemon to check GitHub Releases now, and show the result.
        CheckForUpdates,
        /// Install the downloaded update and relaunch on it.
        UpdateAndRestart,
        /// Restart the daemon now instead of when agents are idle (asks
        /// first while turns run).
        RestartDaemonNow,
        /// Show the update popover.
        ShowUpdate,
    ]
);

/// This UI's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the status is fetched again (the daemon checks GitHub only
/// every 6 hours; this only reads its answer).
const POLL_EVERY: Duration = Duration::from_secs(60);

/// A click on the item this soon after the popover closed by a click
/// outside it is that same click.
const REOPEN_GUARD: Duration = Duration::from_millis(250);

/// What the UI is doing about the update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Checking,
    Installing,
    Restarting,
}

/// The update state the UI shows. See the module docs.
pub struct Updates {
    install: Install,
    status: Option<UpdateStatus>,
    /// The window whose popover is open.
    open_in: Option<AnyWindowHandle>,
    closed_at: Option<Instant>,
    /// The popover asks before **Restart now** stops running turns.
    confirm_restart: bool,
    busy: Option<Busy>,
    /// What went wrong with the last action, shown in the popover.
    error: Option<String>,
    /// The rollback notice was dismissed.
    rollback_seen: bool,
    _poll: Option<Task<()>>,
    _request: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for Updates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Updates")
            .field("install", &self.install)
            .field("status", &self.status)
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

struct GlobalUpdates(Entity<Updates>);

impl Global for GlobalUpdates {}

/// Register the status-bar item, the palette commands and their actions.
pub fn init(cx: &mut App) {
    let updates = cx.new(|cx| Updates::new(Install::detect(), cx));
    cx.set_global(GlobalUpdates(updates));
    Slots::register(cx, Slot::StatusBarEnd, "update", 10, view::status_item);
    crate::commands::register(
        cx,
        [
            crate::commands::Command::new(
                "update.check",
                "Check for updates",
                "Coqo",
                CheckForUpdates,
            ),
            crate::commands::Command::new(
                "update.install",
                "Update and restart",
                "Coqo",
                UpdateAndRestart,
            ),
            crate::commands::Command::new(
                "update.restart_daemon",
                "Restart the daemon now",
                "Coqo",
                RestartDaemonNow,
            ),
            crate::commands::Command::new("update.show", "Show update", "Coqo", ShowUpdate),
        ],
    );
    cx.on_action(|_: &CheckForUpdates, cx| {
        with_updates(cx, |u, cx| {
            u.open_in_active(cx);
            u.check(cx);
        });
    });
    cx.on_action(|_: &UpdateAndRestart, cx| {
        with_updates(cx, |u, cx| {
            u.open_in_active(cx);
            u.install(cx);
        });
    });
    cx.on_action(|_: &RestartDaemonNow, cx| {
        with_updates(cx, |u, cx| {
            u.open_in_active(cx);
            u.restart_now(cx);
        });
    });
    cx.on_action(|_: &ShowUpdate, cx| with_updates(cx, |u, cx| u.open_in_active(cx)));
}

fn with_updates(cx: &mut App, f: impl FnOnce(&mut Updates, &mut Context<Updates>)) {
    if let Some(updates) = Updates::global(cx) {
        updates.update(cx, f);
    }
}

/// `<data>/updates`, where the daemon keeps the downloads and the records.
fn update_dir() -> Option<UpdateDir> {
    let data = coqo_client::data_dir(std::env::var_os(coqo_client::DATA_DIR_ENV).as_deref())?;
    Some(UpdateDir::new(&data))
}

impl Updates {
    fn new(install: Install, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            install,
            status: None,
            open_in: None,
            closed_at: None,
            confirm_restart: false,
            busy: None,
            error: None,
            rollback_seen: false,
            _poll: None,
            _request: None,
            _subscriptions: Vec::new(),
        };
        if let Some(daemon) = Daemon::global(cx) {
            this._subscriptions
                .push(cx.subscribe(&daemon, |this, _, event, cx| {
                    if *event == DaemonEvent::Connected {
                        this.refresh(cx);
                    }
                }));
            if daemon.read(cx).is_connected() {
                this.refresh(cx);
            }
            this._poll = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(POLL_EVERY).await;
                    if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                        return;
                    }
                }
            }));
        }
        this
    }

    /// The app's update state, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<Updates>> {
        cx.try_global::<GlobalUpdates>().map(|g| g.0.clone())
    }

    /// Where this UI is installed.
    pub fn location(&self) -> &Install {
        &self.install
    }

    /// The daemon's last answer.
    pub fn status(&self) -> Option<&UpdateStatus> {
        self.status.as_ref()
    }

    pub fn busy(&self) -> Option<Busy> {
        self.busy
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn confirm_restart(&self) -> bool {
        self.confirm_restart
    }

    pub fn rollback_seen(&self) -> bool {
        self.rollback_seen
    }

    /// Whether the popover is open in `window`.
    pub fn is_open_in(&self, window: AnyWindowHandle) -> bool {
        self.open_in == Some(window)
    }

    /// Set what the daemon said, for tests and screenshots.
    #[cfg(test)]
    pub fn set_for_tests(
        &mut self,
        install: Install,
        status: Option<UpdateStatus>,
        cx: &mut Context<Self>,
    ) {
        self.install = install;
        self.status = status;
        cx.notify();
    }

    fn daemon(cx: &App) -> Option<Entity<Daemon>> {
        Daemon::global(cx).filter(|d| d.read(cx).is_connected())
    }

    /// Fetch the daemon's status.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let status = daemon.read(cx).request::<um::Status>(());
        cx.spawn(async move |this, cx| {
            if let Ok(status) = status.await {
                this.update(cx, |this, cx| this.set_status(status, cx)).ok();
            }
        })
        .detach();
    }

    fn set_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        if self.status.as_ref() != Some(&status) {
            if status.restart.is_none() {
                self.confirm_restart = false;
            }
            self.status = Some(status);
            cx.notify();
        }
    }

    /// Open the popover in `window`, or close it.
    pub fn toggle(&mut self, window: AnyWindowHandle, cx: &mut Context<Self>) {
        if self.open_in == Some(window) {
            self.close(cx);
            return;
        }
        if self.closed_at.is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
            return;
        }
        self.open_in = Some(window);
        cx.notify();
    }

    fn open_in_active(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = cx.active_window() {
            self.open_in = Some(window);
            cx.notify();
        }
    }

    /// Close the popover.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open_in.take().is_some() {
            self.closed_at = Some(Instant::now());
            self.confirm_restart = false;
            if self
                .status
                .as_ref()
                .is_some_and(|s| s.rolled_back.is_some())
                && !matches!(
                    self.status.as_ref().map(|s| &s.state),
                    Some(UpdateState::Ready { .. })
                )
            {
                self.rollback_seen = true;
            }
            cx.notify();
        }
    }

    /// "Check for updates": ask the daemon to check now.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            self.error = Some("Not connected to coqod".into());
            cx.notify();
            return;
        };
        self.busy = Some(Busy::Checking);
        self.error = None;
        cx.notify();
        let check = daemon.read(cx).request::<um::Check>(());
        self._request = Some(cx.spawn(async move |this, cx| {
            let result = check.await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(status) => this.set_status(status, cx),
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// "Update and restart": swap the update in through the daemon, then
    /// relaunch on it.
    pub fn install(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.install.root().map(|r| r.display().to_string()) else {
            self.error = Some("Running a development build: nothing to install into".into());
            cx.notify();
            return;
        };
        let ready = matches!(
            self.status.as_ref().map(|s| &s.state),
            Some(UpdateState::Ready { .. })
        );
        if !ready {
            self.error = Some("No update is ready yet".into());
            cx.notify();
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            self.error = Some("Not connected to coqod".into());
            cx.notify();
            return;
        };
        self.busy = Some(Busy::Installing);
        self.error = None;
        cx.notify();
        let install = daemon
            .read(cx)
            .request::<um::Install>(InstallParams { install: root });
        let location = self.install.clone();
        self._request = Some(cx.spawn(async move |this, cx| {
            let result = install.await;
            this.update(cx, |this, cx| match result {
                Ok(status) => {
                    this.set_status(status, cx);
                    match relaunch(&location) {
                        Ok(()) => cx.quit(),
                        Err(e) => {
                            this.busy = None;
                            this.error = Some(format!(
                                "Installed, but the new Coqo did not start ({e}). Quit and open Coqo again."
                            ));
                            cx.notify();
                        }
                    }
                }
                Err(e) => {
                    this.busy = None;
                    this.error = Some(e.to_string());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// **Restart now**: the first time while turns run it only asks; then
    /// it restarts the daemon at once.
    pub fn restart_now(&mut self, cx: &mut Context<Self>) {
        let running = self
            .status
            .as_ref()
            .and_then(|s| s.restart)
            .map_or(0, |r| r.running);
        if running > 0 && !self.confirm_restart {
            self.confirm_restart = true;
            cx.notify();
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        self.confirm_restart = false;
        self.busy = Some(Busy::Restarting);
        cx.notify();
        let restart = daemon
            .read(cx)
            .request::<RestartWhenIdle>(RestartParams { now: true });
        self._request = Some(cx.spawn(async move |this, cx| {
            let result = restart.await;
            this.update(cx, |this, cx| {
                this.busy = None;
                if let Err(e) = result {
                    this.error = Some(e.to_string());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Cancel the **Restart now** question.
    pub fn cancel_restart(&mut self, cx: &mut Context<Self>) {
        self.confirm_restart = false;
        cx.notify();
    }
}

/// Start the installed UI detached: `open -n Coqo.app` on macOS (a new
/// instance, through Launch Services), the bin folder's `coqo` in its own
/// process group on Linux.
pub fn relaunch(install: &Install) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let mut command = match install {
        Install::App { app } => {
            let mut c = Command::new("/usr/bin/open");
            c.arg("-n").arg(app);
            c
        }
        Install::BinDir { .. } => {
            let ui: PathBuf = install.ui_binary().unwrap_or_default();
            Command::new(ui)
        }
        Install::Dev { .. } => {
            return Err(std::io::Error::other(
                "a development build is not relaunched",
            ));
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
}

/// Why a connection attempt failed, for the updater.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectFailure {
    /// The daemon this UI started exited during start-up, with this
    /// reason (#328).
    pub daemon_exited: Option<String>,
    /// The daemon speaks an incompatible protocol.
    pub incompatible: bool,
}

/// What the UI does after a failed connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterFailure {
    /// Nothing special: show it, wait for Retry.
    Nothing,
    /// An older daemon with another protocol is still finishing its
    /// agents' turns before it restarts: try again shortly.
    RetrySoon,
    /// Rolled back and relaunching the previous version: this UI quits.
    RolledBack,
}

/// After a failed connection attempt: roll back once if this version was
/// just installed and its daemon exited during start-up; wait for the old
/// daemon if it speaks another protocol.
pub fn after_connect_failure(failure: &ConnectFailure, cx: &mut App) -> AfterFailure {
    let Some(dir) = update_dir() else {
        return AfterFailure::Nothing;
    };
    let install = Updates::global(cx)
        .map(|u| u.read(cx).install.clone())
        .unwrap_or_else(Install::detect);
    let after = decide(&dir, &install, VERSION, failure, |install| {
        relaunch(install).map_err(|e| e.to_string())
    });
    if after == AfterFailure::RolledBack {
        cx.quit();
    }
    after
}

/// [`after_connect_failure`] without the app: decide, roll back and
/// relaunch through `relaunch`.
fn decide(
    dir: &UpdateDir,
    install: &Install,
    version: &str,
    failure: &ConnectFailure,
    relaunch: impl FnOnce(&Install) -> Result<(), String>,
) -> AfterFailure {
    if let Some(reason) = &failure.daemon_exited {
        match roll_back_failed_start(dir, install, version, reason) {
            Ok(Some(record)) => {
                eprintln!(
                    "coqo: v{} did not start its daemon ({reason}); rolled back to v{}",
                    record.version, record.restored
                );
                return match relaunch(install) {
                    Ok(()) => AfterFailure::RolledBack,
                    Err(e) => {
                        eprintln!("coqo: cannot start the restored Coqo: {e}");
                        AfterFailure::Nothing
                    }
                };
            }
            Ok(None) => {}
            Err(e) => eprintln!("coqo: cannot roll back the update: {e}"),
        }
    }
    let waiting = failure.incompatible && dir.pending().is_some_and(|p| p.to == version);
    if waiting {
        AfterFailure::RetrySoon
    } else {
        AfterFailure::Nothing
    }
}
