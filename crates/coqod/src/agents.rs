// SPDX-License-Identifier: Apache-2.0

//! Agent sessions (`coqo.agents@1`, PLAN §7.3): the `coqo-agents` session
//! manager, wired to the daemon's storage and busy guards.
//!
//! Sessions are daemon-owned: their turns run here whether or not a UI is
//! attached, each running turn holds a busy guard so the daemon does not
//! idle-exit mid-turn, and transcripts are persisted in `coqo.db`.

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use coqo_acp_host::AgentFs;
use coqo_agents::model::{ProviderInfo, SessionId};
use coqo_agents::{
    AgentProvider, AgentProviderImpls, KeepAlive, ManagerOptions, ProviderRegistry, SessionManager,
    SessionStore, provider_info,
};
use coqo_kernel::ContributionRegistry;
use coqo_mod_api::points;
use coqo_protocol::Router;

use crate::buffers::BufferService;
use crate::daemon::DaemonContext;
use crate::editor_mcp;
use crate::lifecycle::Activity;

/// Busy guards for agent work.
#[derive(Debug, Clone)]
pub struct ActivityKeepAlive(pub Activity);

impl KeepAlive for ActivityKeepAlive {
    fn hold(&self, reason: String) -> Box<dyn Any + Send + Sync> {
        Box::new(self.0.busy(reason))
    }
}

/// ACP agents' file access through the daemon's replicated buffers (ADR
/// 0010): `fs/read_text_file` sees unsaved edits, `fs/write_text_file` into
/// an open buffer is a tracked `agent` change, and tool-call diffs mark the
/// disk write that follows as the session's.
#[derive(Debug, Clone)]
pub struct BufferFs(pub Arc<BufferService>);

impl AgentFs for BufferFs {
    fn expect_write(
        &self,
        session: &SessionId,
        path: &Path,
        old_text: Option<String>,
        new_text: String,
    ) {
        self.0
            .expect_agent_write(&session.0, path, old_text, new_text);
    }

    fn open_text(&self, path: &Path) -> Option<String> {
        self.0.open_text(path)
    }

    fn replace_open(&self, session: &SessionId, path: &Path, text: &str) -> bool {
        self.0.agent_replace(&session.0, path, text).is_some()
    }
}

/// Agent providers registered by mods: metadata in `agent_providers`,
/// runtimes in `agent_provider_impls` (see `coqo_agents::register_provider`).
/// Read on every lookup, so a mod enabled, disabled or reloaded takes
/// effect for the next session.
#[derive(Debug, Clone)]
pub struct ModProviders(pub Arc<ContributionRegistry>);

impl ModProviders {
    fn runtime(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        let id = id.parse().ok()?;
        let c = self.0.get_of::<AgentProviderImpls>(&id).ok()??;
        Some(Arc::clone(&*c.value))
    }
}

impl ProviderRegistry for ModProviders {
    fn providers(&self) -> Vec<ProviderInfo> {
        let infos = self
            .0
            .contributions_of::<points::AgentProviders>()
            .unwrap_or_default();
        infos
            .iter()
            .map(|c| provider_info(&c.value, self.runtime(c.id.as_str()).as_ref()))
            .collect()
    }

    fn provider(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        self.runtime(id)
    }
}

/// Opens the session manager on the daemon's global database. Running turns
/// are ended as interrupted when the daemon shuts down (and, after a crash,
/// when it next starts).
pub fn open(ctx: &DaemonContext) -> Result<SessionManager> {
    let path = ctx.data.global_db();
    let store = SessionStore::open(&path)
        .with_context(|| format!("cannot open agent sessions in {}", path.display()))?;
    let providers = Arc::new(ModProviders(Arc::clone(&ctx.mods.contributions)));
    // The `coqo-editor` MCP server (`coqod mcp`), behind a switch until #71
    // injects it into every session.
    let editor = if editor_mcp::enabled() {
        Some(editor_mcp::EditorMcp::for_daemon(ctx)?.source())
    } else {
        None
    };
    let options = ManagerOptions {
        keep_alive: Some(Arc::new(ActivityKeepAlive(ctx.activity.clone()))),
        mcp_servers: editor,
    };
    let manager = SessionManager::open(store, providers, options)?;
    editor_mcp::revoke_on_close(&manager, ctx.auth.clone(), ctx.shutdown.clone());
    let on_shutdown = manager.clone();
    let shutdown = ctx.shutdown.clone();
    tokio::spawn(async move {
        shutdown.cancelled().await;
        on_shutdown.shutdown().await;
    });
    Ok(manager)
}

/// Registers `agents/*` and `sessions/*`.
pub fn register(router: Router, manager: SessionManager) -> Router {
    coqo_agents::rpc::register(router, manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::Mods;
    use crate::mods::tests::test_builtins;
    use coqo_agents::register_provider;
    use coqo_agents::testing::ScriptedProvider;
    use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
    use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError};

    struct FakeAgentMod;

    impl Mod for FakeAgentMod {
        fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
            Box::pin(async move {
                let info = AgentProviderInfo {
                    id: "fake".parse()?,
                    name: "Fake agent".into(),
                    kind: AgentProviderKind::Native,
                    description: None,
                };
                register_provider(&cx, info, Arc::new(ScriptedProvider::new()))
            })
        }
    }

    const FAKE_MANIFEST: &str = r#"
        id = "test.fake-agent"
        name = "Fake agent"
        version = "0.1.0"
        engine = "^0.0"
        tier = "core"
        activation = ["startup"]
    "#;

    #[tokio::test]
    async fn providers_come_from_mods() {
        let dir = tempfile::tempdir().unwrap();
        let mods = Mods::load(test_builtins(dir.path()).with(FAKE_MANIFEST, FakeAgentMod));
        mods.start().await;
        let providers = ModProviders(Arc::clone(&mods.contributions));
        let listed = providers.providers();
        let fake = listed.iter().find(|p| p.id == "fake").expect("{listed:?}");
        assert!(fake.available);
        assert!(fake.capabilities.is_some_and(|c| c.resume));
        assert!(fake.status.is_none());
        // The built-in Claude provider reports its setup.
        let claude = listed
            .iter()
            .find(|p| p.id == coqo_agent_claude::PROVIDER_ID)
            .expect("{listed:?}");
        assert!(claude.status.is_some());
        assert!(providers.provider("fake").is_some());
        assert!(providers.provider("other").is_none());

        mods.shutdown().await;
        assert!(providers.provider("fake").is_none());
        assert!(providers.providers().is_empty());
    }

    #[test]
    fn keep_alive_holds_a_busy_guard() {
        let activity = Activity::new();
        let keep_alive = ActivityKeepAlive(activity.clone());
        let guard = keep_alive.hold("agent session s-1".into());
        assert_eq!(activity.snapshot().busy, ["agent session s-1"]);
        drop(guard);
        assert!(activity.snapshot().busy.is_empty());
    }
}
