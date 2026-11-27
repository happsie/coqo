// SPDX-License-Identifier: Apache-2.0

//! [`KernelHost`]: `coqo_mod_api::ModHost` over the kernel registries.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use coqo_mod_api::{AnyArc, ModContext, ModError, ModHost, ResolvedService};

use crate::contribution::ContributionRegistry;
use crate::id::{ContributionId, ContributionPointId, ProviderId, ServiceId, ServiceRef};
use crate::service::{ProviderKind, ServiceRegistry};

/// The kernel's [`ModHost`]: mods' calls land in the shared
/// [`ServiceRegistry`] and [`ContributionRegistry`], so point owners and
/// service consumers in the daemon read the mods' typed values back.
///
/// Only *admitted* mods may call it (the [`ModManager`](super::ModManager)
/// admits a mod when it loads it and revokes it when it disables it), so a
/// disabled mod that kept its [`ModContext`] can't register anything again.
/// Each admitted mod has a [`ProviderKind`]: built-ins provide services as
/// [`ProviderKind::Builtin`], so any other mod can replace them.
pub struct KernelHost {
    services: Arc<ServiceRegistry>,
    contributions: Arc<ContributionRegistry>,
    admitted: RwLock<HashMap<ProviderId, ProviderKind>>,
}

impl fmt::Debug for KernelHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let admitted = self.admitted.read().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("KernelHost")
            .field("admitted", &*admitted)
            .finish_non_exhaustive()
    }
}

impl KernelHost {
    /// A host over `services` and `contributions`. Declares every mod API
    /// contribution point ([`coqo_mod_api::points::ALL`]) with its value
    /// type; crates that own further points declare them on `contributions`
    /// before the mods that use them activate.
    pub fn new(services: Arc<ServiceRegistry>, contributions: Arc<ContributionRegistry>) -> Self {
        contributions
            .declare_mod_api_points()
            .expect("the mod API points are declared with their own types");
        Self {
            services,
            contributions,
            admitted: RwLock::default(),
        }
    }

    /// The service registry.
    pub fn services(&self) -> &Arc<ServiceRegistry> {
        &self.services
    }

    /// The contribution registry.
    pub fn contributions(&self) -> &Arc<ContributionRegistry> {
        &self.contributions
    }

    /// Lets `mod_id` call the host, providing services as `kind`.
    pub fn admit(&self, mod_id: ProviderId, kind: ProviderKind) {
        self.admitted
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(mod_id, kind);
    }

    /// Stops `mod_id` from calling the host. Calls already in progress
    /// finish first, so nothing it registers slips past a following
    /// [`withdraw_all`](Self::withdraw_all). Returns whether it was admitted.
    pub fn revoke(&self, mod_id: &ProviderId) -> bool {
        self.admitted
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(mod_id)
            .is_some()
    }

    /// Whether `mod_id` is admitted.
    pub fn is_admitted(&self, mod_id: &ProviderId) -> bool {
        self.admitted
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(mod_id)
    }

    /// Removes every service and contribution `mod_id` registered. Returns
    /// how many registrations were removed.
    pub fn withdraw_all(&self, mod_id: &ProviderId) -> usize {
        self.services.withdraw_provider(mod_id) + self.contributions.withdraw_provider(mod_id)
    }

    /// A [`ModContext`] for `mod_id` on this host.
    pub fn context(self: &Arc<Self>, mod_id: ProviderId) -> ModContext {
        ModContext::new(mod_id, Arc::clone(self) as Arc<dyn ModHost>)
    }

    /// Runs `f` with the caller's provider kind while holding the admission
    /// lock, so [`revoke`](Self::revoke) waits for it.
    fn admitted<T>(
        &self,
        mod_id: &ProviderId,
        f: impl FnOnce(ProviderKind) -> Result<T, ModError>,
    ) -> Result<T, ModError> {
        let admitted = self.admitted.read().unwrap_or_else(PoisonError::into_inner);
        let kind = *admitted.get(mod_id).ok_or_else(|| {
            ModError::PermissionDenied(format!("`{mod_id}` is not an enabled mod"))
        })?;
        f(kind)
    }
}

impl ModHost for KernelHost {
    fn provide_service(
        &self,
        provider: &ProviderId,
        service: ServiceRef,
        value: AnyArc,
    ) -> Result<(), ModError> {
        self.admitted(provider, |kind| {
            self.services
                .provide_any(provider.clone(), kind, service, value);
            Ok(())
        })
    }

    fn withdraw_service(&self, provider: &ProviderId, service: &ServiceId) -> Result<(), ModError> {
        self.admitted(provider, |_| {
            self.services.withdraw(provider, service);
            Ok(())
        })
    }

    fn consume_service(
        &self,
        consumer: &ProviderId,
        request: &ServiceRef,
    ) -> Result<ResolvedService, ModError> {
        // Checking `request` against the consumer's manifest `consumes` is
        // left to typed services (#32).
        self.admitted(consumer, |_| {
            let (descriptor, value) = self.services.consume_any(request)?;
            Ok(ResolvedService {
                service: descriptor.service,
                provider: descriptor.provider,
                value,
            })
        })
    }

    fn contribute(
        &self,
        provider: &ProviderId,
        point: &ContributionPointId,
        id: ContributionId,
        value: AnyArc,
    ) -> Result<(), ModError> {
        self.admitted(provider, |_| {
            Ok(self
                .contributions
                .contribute_any(point, provider.clone(), id, &value)?)
        })
    }

    fn withdraw_contribution(
        &self,
        provider: &ProviderId,
        point: &ContributionPointId,
        id: &ContributionId,
    ) -> Result<bool, ModError> {
        self.admitted(provider, |_| {
            Ok(self.contributions.withdraw(point, provider, id))
        })
    }
}
