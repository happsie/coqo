// SPDX-License-Identifier: Apache-2.0

//! How providers register, and how the session manager finds them.
//!
//! A provider mod contributes two things under the same id:
//!
//! - its metadata to [`points::AgentProviders`] (`agent_providers`, also
//!   declarable statically in `coqo.mod.toml`), for the provider picker;
//! - its runtime, an `Arc<dyn AgentProvider>`, to [`AgentProviderImpls`].
//!
//! [`register_provider`] does both. Each provider is a separate
//! contribution, not a service: the service registry keeps one active
//! provider per service id, and Coqo runs several agent providers at once.
//!
//! The session manager reads providers through [`ProviderRegistry`], which
//! the kernel's contribution registry implements in `coqod` (#147), and
//! which [`StaticProviders`] and [`TestHost`] implement here.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_mod_api::testing::TestHost;
use coqo_mod_api::{ContributionPoint, ModContext, ModError, points};
use coqo_protocol::methods::agents::{ProviderInfo, ProviderKind};

use crate::provider::AgentProvider;

/// Runtimes of in-process agent providers, keyed by provider id (the same id
/// as their [`AgentProviderInfo`] in `agent_providers`):
/// `agent_provider_impls` takes `Arc<dyn AgentProvider>`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentProviderImpls;

impl ContributionPoint for AgentProviderImpls {
    const ID: &'static str = "agent_provider_impls";
    type Value = Arc<dyn AgentProvider>;
}

/// Registers an in-process provider from a mod's `activate`: its metadata
/// in `agent_providers` and its runtime in [`AgentProviderImpls`].
pub fn register_provider(
    cx: &ModContext,
    info: AgentProviderInfo,
    provider: Arc<dyn AgentProvider>,
) -> Result<(), ModError> {
    let id = info.id.clone();
    cx.contribute_as::<AgentProviderImpls>(id.clone(), provider)?;
    if let Err(e) = cx.contribute::<points::AgentProviders>(info) {
        let _ = cx.withdraw::<AgentProviderImpls>(&id);
        return Err(e);
    }
    Ok(())
}

/// Where the session manager finds providers.
pub trait ProviderRegistry: Send + Sync + 'static {
    /// Every known provider. Those without a runtime are listed as not
    /// available.
    fn providers(&self) -> Vec<ProviderInfo>;

    /// The runtime of provider `id`, if it is available.
    fn provider(&self, id: &str) -> Option<Arc<dyn AgentProvider>>;
}

fn kind(kind: AgentProviderKind) -> ProviderKind {
    match kind {
        AgentProviderKind::Acp => ProviderKind::Acp,
        AgentProviderKind::Terminal => ProviderKind::Terminal,
        AgentProviderKind::Native => ProviderKind::Native,
    }
}

/// [`ProviderInfo`] for `info`, with `provider` its runtime if available.
pub fn provider_info(
    info: &AgentProviderInfo,
    provider: Option<&Arc<dyn AgentProvider>>,
) -> ProviderInfo {
    let status = provider.and_then(|p| p.status());
    ProviderInfo {
        id: info.id.as_str().to_owned(),
        name: info.name.clone(),
        kind: kind(info.kind),
        description: info.description.clone(),
        available: provider.is_some() && status.as_ref().is_none_or(|s| s.ready),
        capabilities: provider.map(|p| p.capabilities()),
        status,
    }
}

type Entry = (AgentProviderInfo, Arc<dyn AgentProvider>);

/// A fixed set of providers, for tests and for wiring providers by hand.
#[derive(Default)]
pub struct StaticProviders {
    providers: RwLock<BTreeMap<String, Entry>>,
}

impl fmt::Debug for StaticProviders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let providers = self
            .providers
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        f.debug_list().entries(providers.keys()).finish()
    }
}

impl StaticProviders {
    /// No providers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a provider.
    pub fn add(&self, info: AgentProviderInfo, provider: Arc<dyn AgentProvider>) {
        let mut providers = self
            .providers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        providers.insert(info.id.as_str().to_owned(), (info, provider));
    }

    /// Builder-style [`add`](Self::add).
    pub fn with(self, info: AgentProviderInfo, provider: Arc<dyn AgentProvider>) -> Self {
        self.add(info, provider);
        self
    }
}

impl ProviderRegistry for StaticProviders {
    fn providers(&self) -> Vec<ProviderInfo> {
        let providers = self
            .providers
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        providers
            .values()
            .map(|(info, p)| provider_info(info, Some(p)))
            .collect()
    }

    fn provider(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        let providers = self
            .providers
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        providers.get(id).map(|(_, p)| Arc::clone(p))
    }
}

/// Reads what mods registered on a [`TestHost`] with [`register_provider`].
impl ProviderRegistry for TestHost {
    fn providers(&self) -> Vec<ProviderInfo> {
        self.contributions::<points::AgentProviders>()
            .iter()
            .map(|c| {
                let runtime = self.contribution::<AgentProviderImpls>(c.id.as_str());
                provider_info(&c.value, runtime.as_deref())
            })
            .collect()
    }

    fn provider(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        coqo_mod_api::ContributionId::new(id).ok()?;
        self.contribution::<AgentProviderImpls>(id)
            .map(|p| Arc::clone(&*p))
    }
}
