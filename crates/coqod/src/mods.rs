// SPDX-License-Identifier: Apache-2.0

//! The daemon's mods (PLAN §5): the built-in mod table, loaded and
//! activated at startup and deactivated at shutdown.
//!
//! **Adding a built-in mod** is one line in [`builtins`]:
//! `.with(my_mod::MANIFEST, my_mod::MyMod::new())`. A crate that owns a
//! contribution point (agent providers, say) adds `.with_point::<P>()`
//! there too, so the point exists before any mod contributes to it.
//!
//! Services read what mods registered from [`Mods::services`] and
//! [`Mods::contributions`] (via [`DaemonContext::mods`](crate::DaemonContext)).

use std::sync::Arc;
use std::time::Duration;

use coqo_kernel::mods::{BuiltinMods, ModManager, ModState};
use coqo_kernel::{ContributionRegistry, ServiceRegistry};

/// How long a mod may take to activate or deactivate before it is failed.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(10);

/// The core mods linked into the daemon, and the contribution points their
/// crates own.
pub fn builtins() -> BuiltinMods {
    BuiltinMods::new().with(coqo_mod_hello::MANIFEST, coqo_mod_hello::Hello::new())
}

/// The kernel registries and the mods running on them. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Mods {
    /// Services mods provide, by id and version.
    pub services: Arc<ServiceRegistry>,
    /// Contributions, by contribution point.
    pub contributions: Arc<ContributionRegistry>,
    /// The mods: status, commands, enable and disable.
    pub manager: Arc<ModManager>,
}

impl Mods {
    /// Fresh registries with `builtins` loaded (not yet activated). A mod
    /// that fails to load is logged and listed as failed; it never stops
    /// the daemon.
    pub fn load(builtins: BuiltinMods) -> Self {
        let services = Arc::new(ServiceRegistry::new());
        let contributions = Arc::new(ContributionRegistry::new());
        let manager =
            ModManager::with_registries(Arc::clone(&services), Arc::clone(&contributions))
                .with_lifecycle_timeout(LIFECYCLE_TIMEOUT);
        // Failures are logged by the manager and kept in its status.
        let failed = manager.load_builtins(builtins).len();
        if failed > 0 {
            tracing::warn!(failed, "some built-in mods did not load");
        }
        Self {
            services,
            contributions,
            manager: Arc::new(manager),
        }
    }

    /// Activates the mods that activate at startup. The manager logs each
    /// mod; this logs the totals.
    pub async fn start(&self) {
        self.manager.start().await;
        let status = self.manager.status();
        let count = |state| status.iter().filter(|s| s.state == state).count();
        tracing::info!(
            total = status.len(),
            active = count(ModState::Active),
            waiting = count(ModState::Loaded),
            failed = count(ModState::Failed),
            "mods started"
        );
    }

    /// Deactivates every mod and withdraws what they registered.
    pub async fn shutdown(&self) {
        self.manager.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use coqo_mod_api::points;
    use coqo_mod_api::serde_json::Value;

    use super::*;

    #[tokio::test]
    async fn hello_runs_in_the_daemon() {
        let mods = Mods::load(builtins());
        mods.start().await;
        let status = mods.manager.status();
        assert!(
            status
                .iter()
                .any(|s| s.id.as_str() == "coqo.hello" && s.state == ModState::Active),
            "{status:?}"
        );
        let greet = coqo_mod_hello::GREET.parse().unwrap();
        assert!(
            mods.contributions
                .get_of::<points::Commands>(&greet)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            mods.manager.run_command(&greet, Value::Null).await.unwrap(),
            "Hello, world!"
        );

        mods.shutdown().await;
        assert!(
            mods.contributions
                .contributions_of::<points::Commands>()
                .unwrap()
                .is_empty()
        );
    }
}
