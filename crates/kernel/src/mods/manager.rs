// SPDX-License-Identifier: Apache-2.0

//! [`ModManager`]: loads mods, activates them on their activation events,
//! disables them, and lists their status.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use coqo_mod_api::manifest::{ActivationEvent, Tier};
use coqo_mod_api::points::CommandHandlers;
use coqo_mod_api::serde_json::Value;
use coqo_mod_api::{BoxFuture, CommandHandler, Manifest, Mod, ModError, api_version, semver};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::builtin::BuiltinMods;
use super::guard::guarded;
use super::host::KernelHost;
use crate::contribution::ContributionRegistry;
use crate::id::{ContributionId, ProviderId};
use crate::service::{ProviderKind, ServiceRegistry};

/// Where a mod is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModState {
    /// Checking the manifest and registering its static contributions.
    Loading,
    /// Loaded, with its static contributions registered; waiting for one of
    /// its activation events.
    Loaded,
    /// [`Mod::activate`] is running.
    Activating,
    /// Activated.
    Active,
    /// [`Mod::deactivate`] is running, or its registrations are being
    /// withdrawn.
    Deactivating,
    /// Disabled: nothing it registered is left.
    Disabled,
    /// Loading or activation failed (see [`ModStatus::error`]); nothing it
    /// registered is left. [`ModManager::enable`] retries.
    Failed,
}

impl ModState {
    /// Whether the mod is between two settled states.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Loading | Self::Activating | Self::Deactivating)
    }
}

impl fmt::Display for ModState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Activating => "activating",
            Self::Active => "active",
            Self::Deactivating => "deactivating",
            Self::Disabled => "disabled",
            Self::Failed => "failed",
        })
    }
}

/// One mod in [`ModManager::status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModStatus {
    /// The mod's id.
    pub id: ProviderId,
    /// Its display name.
    pub name: String,
    /// Its version.
    pub version: semver::Version,
    /// Built-in or installed.
    pub kind: ProviderKind,
    /// Where it is in its lifecycle.
    pub state: ModState,
    /// Why it failed, or why deactivating it went wrong.
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct Phase {
    state: ModState,
    error: Option<String>,
}

struct Slot {
    manifest: Manifest,
    kind: ProviderKind,
    module: Arc<dyn Mod>,
    phase: watch::Sender<Phase>,
}

impl Slot {
    fn id(&self) -> &ProviderId {
        &self.manifest.id
    }

    fn state(&self) -> ModState {
        self.phase.borrow().state
    }

    fn set(&self, state: ModState, error: Option<String>) {
        self.phase.send_replace(Phase { state, error });
    }

    /// Moves to `to` if the current state is one of `from`. Returns the
    /// state found.
    fn claim(&self, from: &[ModState], to: ModState) -> ModState {
        let mut found = ModState::Loading;
        self.phase.send_if_modified(|phase| {
            found = phase.state;
            if from.contains(&phase.state) {
                phase.state = to;
                phase.error = None;
                true
            } else {
                false
            }
        });
        found
    }

    /// Waits until the state is not [transient](ModState::is_transient).
    async fn settled(&self) {
        let mut rx = self.phase.subscribe();
        // The sender lives as long as the slot, so this can't fail.
        let _ = rx.wait_for(|phase| !phase.state.is_transient()).await;
    }

    fn status(&self) -> ModStatus {
        let phase = self.phase.borrow().clone();
        ModStatus {
            id: self.manifest.id.clone(),
            name: self.manifest.display_name().to_owned(),
            version: self.manifest.version.clone(),
            kind: self.kind,
            state: phase.state,
            error: phase.error,
        }
    }
}

/// Loads mods into a [`KernelHost`] and drives their lifecycle (PLAN §5):
///
/// 1. [`load`](Self::load) / [`load_builtins`](Self::load_builtins): check
///    the manifest (a built-in must be a `core` mod, and its `engine` range
///    must match [`coqo_mod_api::api_version`]), admit the mod to the host,
///    and register its static `[contributes]`.
/// 2. [`start`](Self::start) activates every mod with the `startup`
///    activation event; [`run_command`](Self::run_command) activates mods
///    with `command:<id>` before running a command none has registered a
///    handler for; [`fire`](Self::fire) and [`activate`](Self::activate) do
///    it for any other event. Mods activate once.
/// 3. [`disable`](Self::disable) calls [`Mod::deactivate`], revokes the mod
///    from the host and withdraws everything it registered;
///    [`enable`](Self::enable) loads it again.
///
/// Errors are isolated per mod: a mod whose manifest is rejected, whose
/// activation fails or panics, or whose command handler panics is logged
/// and marked [`ModState::Failed`] (handlers just return the error), and
/// everything it registered is withdrawn. Other mods keep running.
pub struct ModManager {
    host: Arc<KernelHost>,
    slots: RwLock<Vec<Arc<Slot>>>,
    started: AtomicBool,
    lifecycle_timeout: Option<Duration>,
}

impl fmt::Debug for ModManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModManager")
            .field("mods", &self.status())
            .field("started", &self.started.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

fn unknown(id: &ProviderId) -> ModError {
    ModError::failed(format!("unknown mod `{id}`"))
}

impl ModManager {
    /// A manager for mods on `host`.
    pub fn new(host: Arc<KernelHost>) -> Self {
        Self {
            host,
            slots: RwLock::default(),
            started: AtomicBool::new(false),
            lifecycle_timeout: None,
        }
    }

    /// A manager with a new [`KernelHost`] over `services` and
    /// `contributions`.
    pub fn with_registries(
        services: Arc<ServiceRegistry>,
        contributions: Arc<ContributionRegistry>,
    ) -> Self {
        Self::new(Arc::new(KernelHost::new(services, contributions)))
    }

    /// Fails a mod whose [`Mod::activate`] or [`Mod::deactivate`] takes
    /// longer than `timeout`, so one stuck mod can't hold up the others. The
    /// default is no limit. Needs a tokio runtime with timers.
    #[must_use]
    pub fn with_lifecycle_timeout(mut self, timeout: Duration) -> Self {
        self.lifecycle_timeout = Some(timeout);
        self
    }

    /// Runs a mod's activate or deactivate, catching panics and enforcing
    /// the lifecycle timeout.
    async fn lifecycle<'a>(
        &self,
        what: String,
        start: impl FnOnce() -> BoxFuture<'a, Result<(), ModError>>,
    ) -> Result<(), ModError> {
        let run = guarded(|| what.clone(), start);
        match self.lifecycle_timeout {
            None => run.await,
            Some(limit) => tokio::time::timeout(limit, run).await.unwrap_or_else(|_| {
                Err(ModError::failed(format!(
                    "{what} timed out after {limit:?}"
                )))
            }),
        }
    }

    /// The host mods talk to.
    pub fn host(&self) -> &Arc<KernelHost> {
        &self.host
    }

    fn slots(&self) -> Vec<Arc<Slot>> {
        self.slots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn slot(&self, id: &ProviderId) -> Result<Arc<Slot>, ModError> {
        self.slots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|s| s.id() == id)
            .cloned()
            .ok_or_else(|| unknown(id))
    }

    /// Declares the points `builtins` carries, then
    /// [loads](Self::load) each of its mods as [`ProviderKind::Builtin`].
    /// Returns the failures, which are also logged; those of mods with a
    /// readable manifest show up in [`status`](Self::status) too.
    pub fn load_builtins(&self, builtins: BuiltinMods) -> Vec<ModError> {
        let mut errors = Vec::new();
        for declare in builtins.points {
            if let Err(e) = declare(self.host.contributions()) {
                tracing::error!(error = %e, "cannot declare a built-in contribution point");
                errors.push(e.into());
            }
        }
        for builtin in builtins.mods {
            let result = match builtin.manifest {
                Ok(manifest) => self.load(manifest, builtin.module, ProviderKind::Builtin),
                Err(e) => {
                    tracing::error!(error = %e, "a built-in mod has an invalid manifest");
                    Err(ModError::failed(format!("invalid manifest: {e}")))
                }
            };
            if let Err(e) = result {
                errors.push(e);
            }
        }
        errors
    }

    /// Loads `module` with `manifest`: checks the manifest, admits the mod
    /// to the host as `kind`, and registers its static contributions. It
    /// activates on its activation events (for `startup`, at the next
    /// [`start`](Self::start)).
    ///
    /// A mod whose id is already loaded is refused. Any other failure leaves
    /// the mod in [`ModState::Failed`] and is returned and logged.
    pub fn load(
        &self,
        manifest: Manifest,
        module: Box<dyn Mod>,
        kind: ProviderKind,
    ) -> Result<(), ModError> {
        let slot = {
            let mut slots = self.slots.write().unwrap_or_else(PoisonError::into_inner);
            if slots.iter().any(|s| s.id() == &manifest.id) {
                let e = ModError::failed(format!("mod `{}` is already loaded", manifest.id));
                tracing::error!(mod_id = %manifest.id, "{e}");
                return Err(e);
            }
            let slot = Arc::new(Slot {
                manifest,
                kind,
                module: Arc::from(module),
                phase: watch::Sender::new(Phase {
                    state: ModState::Loading,
                    error: None,
                }),
            });
            slots.push(Arc::clone(&slot));
            slot
        };
        self.prepare(&slot)
    }

    /// Checks the manifest, admits the mod and registers its static
    /// contributions. The slot must be [`ModState::Loading`].
    fn prepare(&self, slot: &Slot) -> Result<(), ModError> {
        let id = slot.id();
        let result = self.check(slot).and_then(|()| {
            self.host.admit(id.clone(), slot.kind);
            slot.manifest
                .contributes
                .register(&self.host.context(id.clone()))
        });
        match result {
            Ok(()) => {
                tracing::debug!(mod_id = %id, version = %slot.manifest.version, "mod loaded");
                slot.set(ModState::Loaded, None);
                Ok(())
            }
            Err(e) => {
                tracing::error!(mod_id = %id, error = %e, "cannot load mod");
                self.withdraw(id);
                slot.set(ModState::Failed, Some(e.to_string()));
                Err(e)
            }
        }
    }

    fn check(&self, slot: &Slot) -> Result<(), ModError> {
        let manifest = &slot.manifest;
        manifest
            .validate()
            .map_err(|e| ModError::failed(format!("invalid manifest: {e}")))?;
        if slot.kind == ProviderKind::Builtin && manifest.tier != Tier::Core {
            return Err(ModError::failed(format!(
                "a built-in mod must be a `core` mod, not `{}`",
                manifest.tier
            )));
        }
        let api = api_version();
        if !manifest.supports_api(&api) {
            return Err(ModError::failed(format!(
                "needs mod API `{}`, but this Coqo has {api}",
                manifest.engine
            )));
        }
        Ok(())
    }

    /// Revokes `id` from the host and withdraws everything it registered.
    fn withdraw(&self, id: &ProviderId) -> usize {
        self.host.revoke(id);
        self.host.withdraw_all(id)
    }

    /// Activates every loaded mod with the `startup` activation event, and
    /// from now on activates `startup` mods when they are
    /// [enabled](Self::enable). Call it again after loading more mods.
    pub async fn start(&self) {
        self.started.store(true, Ordering::SeqCst);
        self.fire(&ActivationEvent::Startup).await;
    }

    /// Activates every loaded mod that has `event` among its activation
    /// events, one after another in load order, and waits for mods already
    /// activating. Returns how many of them are active afterwards.
    pub async fn fire(&self, event: &ActivationEvent) -> usize {
        let mut active = 0;
        for slot in self.slots() {
            if slot.manifest.activates_on(event)
                && matches!(
                    slot.state(),
                    ModState::Loaded | ModState::Loading | ModState::Activating | ModState::Active
                )
                && self.activate_slot(&slot).await.is_ok()
            {
                active += 1;
            }
        }
        active
    }

    /// Activates mod `id` now, whatever its activation events. Succeeds at
    /// once if it is already active.
    pub async fn activate(&self, id: &ProviderId) -> Result<(), ModError> {
        let slot = self.slot(id)?;
        self.activate_slot(&slot).await
    }

    async fn activate_slot(&self, slot: &Slot) -> Result<(), ModError> {
        let id = slot.id().clone();
        loop {
            match slot.claim(&[ModState::Loaded], ModState::Activating) {
                ModState::Loaded => break,
                ModState::Active => return Ok(()),
                ModState::Disabled => {
                    return Err(ModError::failed(format!("mod `{id}` is disabled")));
                }
                ModState::Failed => {
                    let error = slot.phase.borrow().error.clone().unwrap_or_default();
                    return Err(ModError::failed(format!("mod `{id}` failed: {error}")));
                }
                ModState::Loading | ModState::Activating | ModState::Deactivating => {
                    slot.settled().await;
                }
            }
        }

        tracing::debug!(mod_id = %id, "activating mod");
        let cx = self.host.context(id.clone());
        let module = Arc::clone(&slot.module);
        match self
            .lifecycle(format!("activating `{id}`"), || module.activate(cx))
            .await
        {
            Ok(()) => {
                tracing::info!(mod_id = %id, version = %slot.manifest.version, "mod active");
                slot.set(ModState::Active, None);
                Ok(())
            }
            Err(e) => {
                tracing::error!(mod_id = %id, error = %e, "mod failed to activate");
                // Let it stop whatever it started before failing.
                if let Err(de) = self
                    .lifecycle(format!("deactivating `{id}`"), || module.deactivate())
                    .await
                {
                    tracing::warn!(mod_id = %id, error = %de, "mod failed to deactivate");
                }
                self.withdraw(&id);
                slot.set(ModState::Failed, Some(e.to_string()));
                Err(e)
            }
        }
    }

    /// Runs command `id` with `args` through its registered handler.
    ///
    /// If no handler is registered, first activates the mods with the
    /// `command:<id>` activation event and waits for mods that are still
    /// activating. A panicking handler returns [`ModError::Failed`].
    pub async fn run_command(&self, id: &ContributionId, args: Value) -> Result<Value, ModError> {
        let handler = match self.handler(id)? {
            Some(handler) => handler,
            None => {
                self.fire(&ActivationEvent::Command(id.clone())).await;
                for slot in self.slots() {
                    slot.settled().await;
                }
                self.handler(id)?
                    .ok_or_else(|| ModError::UnknownCommand(id.clone()))?
            }
        };
        guarded(|| format!("command `{id}`"), || handler.run(args)).await
    }

    fn handler(&self, id: &ContributionId) -> Result<Option<Arc<dyn CommandHandler>>, ModError> {
        Ok(self
            .host
            .contributions()
            .get_of::<CommandHandlers>(id)?
            .map(|c| Arc::clone(&*c.value)))
    }

    /// Disables mod `id`: calls [`Mod::deactivate`] if it is active, then
    /// revokes it from the host and withdraws every service and contribution
    /// it registered, static ones included. A failing deactivation is logged
    /// and kept in [`ModStatus::error`]; the mod is disabled anyway.
    pub async fn disable(&self, id: &ProviderId) -> Result<(), ModError> {
        let slot = self.slot(id)?;
        let previous = loop {
            match slot.claim(
                &[ModState::Loaded, ModState::Active, ModState::Failed],
                ModState::Deactivating,
            ) {
                ModState::Disabled => return Ok(()),
                state if state.is_transient() => slot.settled().await,
                state => break state,
            }
        };

        let mut error = None;
        if previous == ModState::Active {
            let module = Arc::clone(&slot.module);
            if let Err(e) = self
                .lifecycle(format!("deactivating `{id}`"), || module.deactivate())
                .await
            {
                tracing::warn!(mod_id = %id, error = %e, "mod failed to deactivate");
                error = Some(format!("deactivate: {e}"));
            }
        }
        let withdrawn = self.withdraw(id);
        tracing::info!(mod_id = %id, withdrawn, "mod disabled");
        slot.set(ModState::Disabled, error);
        Ok(())
    }

    /// Enables a disabled or failed mod `id`: loads it again and, once
    /// [started](Self::start), activates it if it has the `startup` event.
    pub async fn enable(&self, id: &ProviderId) -> Result<(), ModError> {
        let slot = self.slot(id)?;
        loop {
            match slot.claim(&[ModState::Disabled, ModState::Failed], ModState::Loading) {
                ModState::Disabled | ModState::Failed => break,
                ModState::Loaded | ModState::Active => return Ok(()),
                _ => slot.settled().await,
            }
        }
        self.prepare(&slot)?;
        if self.started.load(Ordering::SeqCst)
            && slot.manifest.activates_on(&ActivationEvent::Startup)
        {
            self.activate_slot(&slot).await?;
        }
        Ok(())
    }

    /// Disables every mod, in reverse load order, e.g. when the daemon
    /// shuts down.
    pub async fn shutdown(&self) {
        for slot in self.slots().into_iter().rev() {
            // Only fails for unknown ids.
            let _ = self.disable(slot.id()).await;
        }
    }

    /// Every mod in load order.
    pub fn status(&self) -> Vec<ModStatus> {
        self.slots().iter().map(|s| s.status()).collect()
    }

    /// Mod `id`'s status, if it is loaded.
    pub fn status_of(&self, id: &ProviderId) -> Option<ModStatus> {
        self.slot(id).ok().map(|s| s.status())
    }
}
