// SPDX-License-Identifier: Apache-2.0

//! `update/*` and `daemon/restartWhenIdle`: the in-app updater (issue
//! #349, ADR 0031).
//!
//! The daemon is the only checker. An installed daemon checks GitHub
//! Releases shortly after start-up and every 6 hours (and on
//! `update/check`), downloads and verifies a newer release with
//! [`coqo_updater::Updater`] and unpacks it beside the install. On
//! `update/install` it swaps it in and arms the restart: once no agent turn
//! runs it shuts down cleanly, and the next client to connect (the UI,
//! relaunched on the new binary) starts the new daemon through the normal
//! spawn and lock path. Sessions resume there (ADR 0026).
//!
//! A development build, a build without a release key, or
//! `updates.check = false` never contacts GitHub.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coqo_protocol::daemon::{RestartParams, RestartResult, RestartWhenIdle};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::update::{
    self as um, InstallParams, OffReason, ReleaseInfo, RestartPending, RolledBack, UpdateState,
    UpdateStatus,
};
use coqo_protocol::{Access, ErrorObject, Router};
use coqo_updater::records::UpdateDir;
use coqo_updater::{Install, Prepared, UpdateError, UpdateSettings, Updater, UpdaterConfig};
use semver::Version;
use tokio_util::sync::CancellationToken;

use crate::daemon::{DaemonContext, VERSION};
use crate::lifecycle::Activity;

/// The first check, this long after start-up (so it never slows a start).
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(5);
/// Then one every 6 hours.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// Whether and what the daemon updates.
#[derive(Debug, Clone)]
pub enum UpdatesConfig {
    /// Never check: development builds and in-process daemons (tests).
    Off(OffReason),
    /// The installed Coqo at `install`, with its settings file.
    Installed {
        /// Where Coqo is installed.
        install: Install,
        /// `settings.toml`, read before every check.
        settings: PathBuf,
    },
}

impl UpdatesConfig {
    /// For the running executable: [`UpdatesConfig::Installed`] when it
    /// runs from an install location, else a development build.
    pub fn detect() -> Self {
        match Install::detect() {
            Install::Dev { .. } => Self::Off(OffReason::DevBuild),
            install => Self::Installed {
                install,
                settings: coqo_client::config_dir().join("settings.toml"),
            },
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn off_message(reason: OffReason) -> &'static str {
    match reason {
        OffReason::Setting => "Update checks are off (updates.check = false)",
        OffReason::DevBuild => "Running a development build",
        OffReason::NoReleaseKey => "This build has no release key, so it can't verify updates",
        OffReason::Unsupported => "There are no Coqo releases for this platform",
    }
}

fn off(reason: OffReason) -> UpdateState {
    UpdateState::Off {
        reason,
        message: off_message(reason).to_owned(),
    }
}

struct Inner {
    updater: Result<Updater, OffReason>,
    settings: Option<PathBuf>,
    dir: Option<UpdateDir>,
    state: Mutex<UpdateState>,
    prepared: Mutex<Option<Prepared>>,
    /// One check at a time.
    checking: tokio::sync::Mutex<()>,
    restart_armed: AtomicBool,
    activity: Activity,
    shutdown: CancellationToken,
}

/// The update service. Cheap to clone.
#[derive(Clone)]
pub struct UpdateService(Arc<Inner>);

impl std::fmt::Debug for UpdateService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateService").finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl UpdateService {
    /// The service for `ctx`; [`Self::start`] begins checking.
    pub fn new(ctx: &DaemonContext) -> Self {
        let current = Version::parse(VERSION).unwrap_or_else(|_| Version::new(0, 0, 0));
        let (updater, settings, dir) = match &ctx.updates {
            UpdatesConfig::Off(reason) => (Err(*reason), None, None),
            UpdatesConfig::Installed { install, settings } => {
                let updater = Updater::new(UpdaterConfig {
                    current,
                    data_dir: ctx.data.root().to_owned(),
                    install: install.clone(),
                })
                .map_err(|e| match e {
                    UpdateError::DevBuild => OffReason::DevBuild,
                    UpdateError::NoReleaseKey => OffReason::NoReleaseKey,
                    other => {
                        tracing::warn!(error = %other, "the updater is off");
                        OffReason::Unsupported
                    }
                });
                let dir = UpdateDir::new(ctx.data.root());
                // This daemon is up: an update to this version started.
                if dir.pending().is_some_and(|p| p.to == VERSION) {
                    tracing::info!(version = VERSION, "the update's daemon started");
                    dir.clear_pending();
                }
                (updater, Some(settings.clone()), Some(dir))
            }
        };
        Self(Arc::new(Inner {
            updater,
            settings,
            dir,
            state: Mutex::new(UpdateState::UpToDate {
                checked_at_ms: None,
            }),
            prepared: Mutex::new(None),
            checking: tokio::sync::Mutex::new(()),
            restart_armed: AtomicBool::new(false),
            activity: ctx.activity.clone(),
            shutdown: ctx.shutdown.clone(),
        }))
    }

    /// Check shortly after start-up and then every 6 hours, until the
    /// daemon shuts down. Does nothing when the updater is off.
    pub fn start(&self) {
        if self.0.updater.is_err() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut wait = FIRST_CHECK_AFTER;
            loop {
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = this.0.shutdown.cancelled() => return,
                }
                this.check().await;
                wait = CHECK_EVERY;
            }
        });
    }

    fn settings(&self) -> UpdateSettings {
        let Some(path) = &self.0.settings else {
            return UpdateSettings::default();
        };
        let (settings, problems) = UpdateSettings::read(path);
        for p in problems {
            tracing::warn!("settings: {p}");
        }
        settings
    }

    fn rolled_back(&self) -> Option<coqo_updater::records::RolledBack> {
        self.0.dir.as_ref()?.rolled_back()
    }

    /// The current status.
    pub fn status(&self) -> UpdateStatus {
        let state = match &self.0.updater {
            Err(reason) => off(*reason),
            Ok(_) if !self.settings().check => off(OffReason::Setting),
            Ok(_) => lock(&self.0.state).clone(),
        };
        UpdateStatus {
            current: VERSION.to_owned(),
            state,
            restart: self
                .0
                .restart_armed
                .load(Ordering::SeqCst)
                .then(|| RestartPending {
                    running: self.0.activity.running_turns(),
                }),
            rolled_back: self.rolled_back().map(|r| RolledBack {
                version: r.version,
                restored: r.restored,
                reason: r.reason,
            }),
        }
    }

    fn set(&self, state: UpdateState) {
        *lock(&self.0.state) = state;
    }

    /// Check now, and download and verify a newer release. Returns the new
    /// status.
    pub async fn check(&self) -> UpdateStatus {
        let Ok(updater) = &self.0.updater else {
            return self.status();
        };
        let _one = self.0.checking.lock().await;
        let settings = self.settings();
        let installed = matches!(*lock(&self.0.state), UpdateState::Installed { .. });
        if !settings.check || installed || self.0.restart_armed.load(Ordering::SeqCst) {
            return self.status();
        }
        let was = lock(&self.0.state).clone();
        self.set(UpdateState::Checking);
        let offer = match updater.check(settings.channel).await {
            Ok(offer) => offer,
            Err(e) => {
                tracing::warn!(error = %e, "update check failed");
                // A hiccup does not lose a release that is ready.
                self.set(match was {
                    ready @ UpdateState::Ready { .. } => ready,
                    _ => UpdateState::Failed {
                        message: e.to_string(),
                        version: None,
                        checked_at_ms: now_ms(),
                    },
                });
                return self.status();
            }
        };
        let skipped = self.rolled_back().map(|r| r.version);
        let Some(offer) = offer.filter(|o| skipped.as_deref() != Some(&o.version.to_string()))
        else {
            *lock(&self.0.prepared) = None;
            self.set(UpdateState::UpToDate {
                checked_at_ms: Some(now_ms()),
            });
            return self.status();
        };
        let ready = |p: &Prepared| UpdateState::Ready {
            release: ReleaseInfo {
                version: p.version.to_string(),
                notes: p.notes.clone(),
                url: p.url.clone(),
                published_at: p.published_at.clone(),
            },
        };
        let staged = updater
            .install_location()
            .staged()
            .is_some_and(|s| s.exists());
        if let Some(p) = lock(&self.0.prepared)
            .as_ref()
            .filter(|p| p.version == offer.version && staged)
        {
            self.set(ready(p));
            return self.status();
        }
        let version = offer.version.to_string();
        self.set(UpdateState::Downloading {
            version: version.clone(),
        });
        match updater.prepare(&offer).await {
            Ok(prepared) => {
                self.set(ready(&prepared));
                *lock(&self.0.prepared) = Some(prepared);
            }
            Err(e) => {
                tracing::warn!(%version, error = %e, "update refused");
                *lock(&self.0.prepared) = None;
                self.set(UpdateState::Failed {
                    message: e.to_string(),
                    version: Some(version),
                    checked_at_ms: now_ms(),
                });
            }
        }
        self.status()
    }

    /// Swap the ready release in and arm the restart.
    pub async fn install(&self, params: InstallParams) -> Result<UpdateStatus, ErrorObject> {
        let failed = |m: String| ErrorObject::new(codes::UPDATE_FAILED, m);
        let updater = match &self.0.updater {
            Ok(updater) => updater,
            Err(reason) => return Err(failed(off_message(*reason).to_owned())),
        };
        let _one = self.0.checking.lock().await;
        let root = updater
            .install_location()
            .root()
            .map(|r| r.display().to_string());
        if root.as_deref() != Some(params.install.as_str()) {
            return Err(failed(format!(
                "this daemon runs from {}, not from {}",
                root.unwrap_or_default(),
                params.install
            )));
        }
        let Some(prepared) = lock(&self.0.prepared).clone() else {
            return Err(failed("no update is ready to install".into()));
        };
        let service = self.clone();
        let p = prepared.clone();
        let swapped = tokio::task::spawn_blocking(move || match &service.0.updater {
            Ok(updater) => updater.install(&p),
            Err(_) => Err(UpdateError::DevBuild),
        })
        .await
        .map_err(|e| failed(format!("the install stopped: {e}")))?;
        if let Err(e) = swapped {
            tracing::error!(error = %e, "update install failed");
            return Err(failed(e.to_string()));
        }
        tracing::info!(version = %prepared.version, "update installed");
        *lock(&self.0.prepared) = None;
        self.set(UpdateState::Installed {
            version: prepared.version.to_string(),
        });
        self.restart_when_idle(false);
        Ok(self.status())
    }

    /// Restart once no agent turn runs, or `now`.
    pub fn restart_when_idle(&self, now: bool) -> RestartResult {
        let running = self.0.activity.running_turns();
        if now {
            tracing::info!(running, "restarting now");
            self.0.restart_armed.store(true, Ordering::SeqCst);
            self.0.shutdown.cancel();
            return RestartResult {
                running,
                restarting: true,
            };
        }
        if !self.0.restart_armed.swap(true, Ordering::SeqCst) {
            tracing::info!(running, "restarting once no agent turn runs");
            let activity = self.0.activity.clone();
            let shutdown = self.0.shutdown.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = activity.until_no_turns() => {
                        tracing::info!("no agent turn runs; restarting");
                        shutdown.cancel();
                    }
                    () = shutdown.cancelled() => {}
                }
            });
        }
        RestartResult {
            running,
            restarting: running == 0,
        }
    }
}

/// Register `update/*` and `daemon/restartWhenIdle`, and start checking.
/// All need the daemon token: agents never update or restart Coqo.
pub fn register(router: Router, ctx: &DaemonContext) -> Router {
    let service = UpdateService::new(ctx);
    service.start();
    let (status, check, install, restart) =
        (service.clone(), service.clone(), service.clone(), service);
    router
        .method_with::<um::Status, _, _>(Access::daemon(), move |_, ()| {
            std::future::ready(Ok(status.status()))
        })
        .method_with::<um::Check, _, _>(Access::daemon(), move |_, ()| {
            let check = check.clone();
            async move { Ok(check.check().await) }
        })
        .method_with::<um::Install, _, _>(Access::daemon(), move |_, params| {
            let install = install.clone();
            async move { install.install(params).await }
        })
        .method_with::<RestartWhenIdle, _, _>(
            Access::daemon(),
            move |req, params: RestartParams| {
                tracing::info!(
                    connection = req.peer.connection(),
                    client = %req.hello.client,
                    now = params.now,
                    "daemon/restartWhenIdle"
                );
                std::future::ready(Ok(restart.restart_when_idle(params.now)))
            },
        )
}

#[cfg(test)]
mod tests;
