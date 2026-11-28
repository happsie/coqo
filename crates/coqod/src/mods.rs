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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use coqo_acp_host::{FsHandle, TerminalHandle};
use coqo_kernel::mods::{BuiltinMods, ModManager, ModState};
use coqo_kernel::{ContributionRegistry, ServiceRegistry};
use coqo_storage::DataDirs;
use serde_json::Value;

/// How long a mod may take to activate or deactivate before it is failed.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Turns the setting "Use Claude plan logins through ACP"
/// (`claude.acp.plan-logins`) off with `0`, `false` or `off`, whatever
/// `settings.toml` says.
pub const CLAUDE_PLAN_LOGINS_ENV: &str = "COQO_CLAUDE_PLAN_LOGINS";

/// Turns the setting "Append Coqo's operating guide to Claude's system
/// prompt" (`claude.guide`) off with `0`, `false` or `off`, whatever
/// `settings.toml` says.
pub const CLAUDE_GUIDE_ENV: &str = "COQO_CLAUDE_GUIDE";

/// Turns the setting "Load Coqo's Claude Code plugin" (`claude.plugin`) off
/// with `0`, `false` or `off`, whatever `settings.toml` says.
pub const CLAUDE_PLUGIN_ENV: &str = "COQO_CLAUDE_PLUGIN";

/// Whether an override variable's value turns its setting off.
fn env_off(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.trim(), "0" | "false" | "off"))
}

/// A switch read at startup: on unless the user's `settings.toml` sets `key`
/// to `false` or the override variable `env` turns it off.
fn startup_switch(settings: &BTreeMap<String, Value>, key: &str, env: &str) -> bool {
    !env_off(env) && settings.get(key).and_then(Value::as_bool).unwrap_or(true)
}

/// The core mods linked into the daemon, and the contribution points their
/// crates own. `data` is the daemon's data directory (Claude's ACP adapter
/// is installed under it); agents read and write files through `agent_fs`
/// and run commands they hand the client through `agent_terminals`.
/// `settings` are the user's settings as the daemon started
/// ([`startup_values`](crate::settings::startup_values)), for the options
/// mods are built with.
pub fn builtins(
    data: &DataDirs,
    agent_fs: &FsHandle,
    agent_terminals: &TerminalHandle,
    settings: &BTreeMap<String, Value>,
) -> BuiltinMods {
    let mut claude = coqo_agent_claude::ClaudeOptions::new(data.root());
    claude.fs = agent_fs.clone();
    // The adapter (0.113.0) runs its Bash tool itself and never asks for a
    // client terminal; other ACP agents built on these options would.
    claude.acp.terminals = agent_terminals.clone();
    claude.plan_logins = startup_switch(
        settings,
        coqo_agent_claude::PLAN_LOGINS_SETTING,
        CLAUDE_PLAN_LOGINS_ENV,
    );
    claude.guide = startup_switch(settings, coqo_agent_claude::GUIDE_SETTING, CLAUDE_GUIDE_ENV);
    claude.plugin = startup_switch(
        settings,
        coqo_agent_claude::PLUGIN_SETTING,
        CLAUDE_PLUGIN_ENV,
    );
    BuiltinMods::new()
        .with_point::<coqo_agents::AgentProviderImpls>()
        .with(
            crate::settings::workbench::MANIFEST,
            crate::settings::workbench::WorkbenchSettings,
        )
        .with(coqo_mod_hello::MANIFEST, coqo_mod_hello::Hello::new())
        .with(
            coqo_syntax::builtin::MANIFEST,
            coqo_syntax::builtin::SyntaxMod::new(),
        )
        .with(coqo_mod_git::MANIFEST, coqo_mod_git::GitMod::default())
        .with(coqo_mod_search::MANIFEST, coqo_mod_search::SearchMod::new())
        .with(
            coqo_agent_claude::MANIFEST,
            coqo_agent_claude::ClaudeMod::new(claude),
        )
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
pub(crate) mod tests {
    use coqo_mod_api::points;
    use coqo_mod_api::serde_json::Value;

    use super::*;

    /// The built-in mods, with data in `dir`.
    pub(crate) fn test_builtins(dir: &std::path::Path) -> BuiltinMods {
        builtins(
            &DataDirs::at(dir),
            &FsHandle::new(),
            &TerminalHandle::new(),
            &BTreeMap::new(),
        )
    }

    #[tokio::test]
    async fn hello_runs_in_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let mods = Mods::load(test_builtins(dir.path()));
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
        // The syntax mod is always on and makes the bundled languages known.
        assert!(
            status
                .iter()
                .any(|s| s.id.as_str() == "coqo.syntax" && s.state == ModState::Active),
            "{status:?}"
        );
        let languages = mods
            .contributions
            .contributions_of::<points::Languages>()
            .unwrap();
        assert!(
            languages.iter().any(|l| l.id.as_str() == "kotlin"),
            "{languages:?}"
        );
        // Claude Code is a built-in provider, whether or not it is set up.
        assert!(
            status
                .iter()
                .any(|s| s.id.as_str() == "coqo.claude" && s.state == ModState::Active),
            "{status:?}"
        );
        let providers = mods
            .contributions
            .contributions_of::<points::AgentProviders>()
            .unwrap();
        assert!(
            providers
                .iter()
                .any(|p| p.id.as_str() == coqo_agent_claude::PROVIDER_ID),
            "{providers:?}"
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
