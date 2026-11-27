// SPDX-License-Identifier: Apache-2.0

//! Typed, versioned service registry (PLAN §5).
//!
//! Mods [`provide`](ServiceRegistry::provide) and
//! [`consume`](ServiceRegistry::consume) services such as `coqo.git@1`. A
//! service value is any `Arc<T>`, usually `Arc<dyn SomeTrait>`, and consumers
//! name the same `T` to get it back.
//!
//! # Resolution
//!
//! For each service id and **major** version there is at most one *active*
//! provider:
//!
//! 1. a [`ProviderKind::Mod`] provider beats a [`ProviderKind::Builtin`], so any
//!    mod can replace a built-in;
//! 2. within the same kind, the most recent registration wins.
//!
//! A consumer asking for `coqo.git@1.2` gets the active `coqo.git` major-1
//! provider if it is at version `1.2` or later, and a
//! [`ServiceError::VersionMismatch`] otherwise. Withdrawing the active
//! provider (for example when a mod is disabled) brings the next one back.

use std::any::type_name;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use coqo_mod_api::{AnyArc, ModError};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::id::{ProviderId, ServiceId, ServiceRef, ServiceVersion};

/// Whether a provider ships with Coqo or comes from a mod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Built into Coqo. Any mod providing the same service replaces it.
    Builtin,
    /// Installed or user mod.
    Mod,
}

/// Why a service could not be provided or consumed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceError {
    /// Nobody provides a service with this id.
    #[error("no provider for service `{0}`")]
    NotFound(ServiceRef),
    /// The service exists, but no active provider is compatible with the request.
    #[error("service `{requested}` not available; active versions: {}", fmt_versions(.available))]
    VersionMismatch {
        /// What the consumer asked for.
        requested: ServiceRef,
        /// Versions of the active providers, one per major version.
        available: Vec<ServiceVersion>,
    },
    /// The active provider registered a different Rust type than the consumer asked for.
    #[error("service `{service}` from `{provider}` is a `{actual}`, not a `{expected}`")]
    TypeMismatch {
        /// The service as provided.
        service: ServiceRef,
        /// Who provides it.
        provider: ProviderId,
        /// Type the consumer asked for.
        expected: &'static str,
        /// Type the provider registered.
        actual: &'static str,
    },
}

impl From<ServiceError> for ModError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::NotFound(service) => ModError::ServiceNotFound(service),
            ServiceError::VersionMismatch {
                requested,
                available,
            } => ModError::ServiceVersionMismatch {
                requested,
                available,
            },
            ServiceError::TypeMismatch {
                service,
                provider,
                expected,
                actual,
            } => ModError::ServiceTypeMismatch {
                service,
                provider,
                expected,
                actual,
            },
        }
    }
}

fn fmt_versions(versions: &[ServiceVersion]) -> String {
    if versions.is_empty() {
        return "none".to_owned();
    }
    versions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One registered provider of a service, as reported by [`ServiceRegistry::services`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDescriptor {
    /// Service id and the exact version on offer.
    pub service: ServiceRef,
    /// Who provides it.
    pub provider: ProviderId,
    /// Built-in or mod.
    pub kind: ProviderKind,
    /// Whether this provider currently serves its id and major version.
    pub active: bool,
}

struct Entry {
    provider: ProviderId,
    kind: ProviderKind,
    version: ServiceVersion,
    seq: u64,
    /// An erased `Arc<T>`, so unsized `T` works too.
    value: AnyArc,
}

impl Entry {
    fn rank(&self) -> (ProviderKind, u64) {
        (self.kind, self.seq)
    }
}

#[derive(Default)]
struct Inner {
    services: HashMap<ServiceId, Vec<Entry>>,
    next_seq: u64,
}

impl Inner {
    /// The active entry for `id` at `major`, if any.
    fn active(&self, id: &ServiceId, major: u32) -> Option<&Entry> {
        self.services
            .get(id)?
            .iter()
            .filter(|e| e.version.major == major)
            .max_by_key(|e| e.rank())
    }

    /// Versions of the active providers of `id`, one per major, ascending.
    fn active_versions(&self, id: &ServiceId) -> Vec<ServiceVersion> {
        let Some(entries) = self.services.get(id) else {
            return Vec::new();
        };
        let mut majors: Vec<u32> = entries.iter().map(|e| e.version.major).collect();
        majors.sort_unstable();
        majors.dedup();
        majors
            .into_iter()
            .filter_map(|major| self.active(id, major).map(|e| e.version))
            .collect()
    }
}

/// Typed, versioned service registry. Cheap to share behind an `Arc`; all
/// methods take `&self`.
pub struct ServiceRegistry {
    inner: RwLock<Inner>,
    generation: watch::Sender<u64>,
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ServiceRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceRegistry")
            .field("services", &self.services())
            .field("generation", &self.generation())
            .finish()
    }
}

impl ServiceRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            inner: RwLock::default(),
            generation: watch::Sender::new(0),
        }
    }

    /// Registers `value` as `provider`'s implementation of `service`
    /// (`service.version` is the exact version on offer).
    ///
    /// If `provider` already provides the same id and major version, that
    /// registration is replaced (for hot reload); it then counts as the most
    /// recent one.
    pub fn provide<T>(
        &self,
        provider: ProviderId,
        kind: ProviderKind,
        service: ServiceRef,
        value: Arc<T>,
    ) where
        T: ?Sized + Send + Sync + 'static,
    {
        self.provide_any(provider, kind, service, AnyArc::new(value));
    }

    /// [`provide`](Self::provide) with a type-erased value, as mods hand it
    /// to the [`ModHost`](coqo_mod_api::ModHost). Consumers name the erased
    /// type to get it back.
    pub fn provide_any(
        &self,
        provider: ProviderId,
        kind: ProviderKind,
        service: ServiceRef,
        value: AnyArc,
    ) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let seq = inner.next_seq;
        inner.next_seq += 1;
        let entries = inner.services.entry(service.id.clone()).or_default();
        entries.retain(|e| !(e.provider == provider && e.version.major == service.version.major));
        tracing::debug!(%service, %provider, ?kind, "service provided");
        entries.push(Entry {
            provider,
            kind,
            version: service.version,
            seq,
            value,
        });
        drop(inner);
        self.bump();
    }

    /// Resolves `request` and returns the active provider's value.
    ///
    /// `T` must be the type the provider registered.
    pub fn consume<T>(&self, request: &ServiceRef) -> Result<Arc<T>, ServiceError>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let (descriptor, value) = self.consume_any(request)?;
        value.downcast::<T>().ok_or(ServiceError::TypeMismatch {
            service: descriptor.service,
            provider: descriptor.provider,
            expected: type_name::<T>(),
            actual: value.type_name(),
        })
    }

    /// Resolves `request` and returns the active provider with its erased
    /// value, for hosts that hand it on without knowing its type.
    pub fn consume_any(
        &self,
        request: &ServiceRef,
    ) -> Result<(ServiceDescriptor, AnyArc), ServiceError> {
        let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::resolve_entry(&inner, request)?;
        Ok((
            ServiceDescriptor {
                service: ServiceRef::new(request.id.clone(), entry.version),
                provider: entry.provider.clone(),
                kind: entry.kind,
                active: true,
            },
            entry.value.clone(),
        ))
    }

    /// Resolves `request` without fetching the value: which provider would serve it.
    pub fn resolve(&self, request: &ServiceRef) -> Result<ServiceDescriptor, ServiceError> {
        let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let entry = Self::resolve_entry(&inner, request)?;
        Ok(ServiceDescriptor {
            service: ServiceRef::new(request.id.clone(), entry.version),
            provider: entry.provider.clone(),
            kind: entry.kind,
            active: true,
        })
    }

    fn resolve_entry<'a>(
        inner: &'a Inner,
        request: &ServiceRef,
    ) -> Result<&'a Entry, ServiceError> {
        if !inner
            .services
            .get(&request.id)
            .is_some_and(|e| !e.is_empty())
        {
            return Err(ServiceError::NotFound(request.clone()));
        }
        match inner.active(&request.id, request.version.major) {
            Some(entry) if entry.version.satisfies(request.version) => Ok(entry),
            _ => Err(ServiceError::VersionMismatch {
                requested: request.clone(),
                available: inner.active_versions(&request.id),
            }),
        }
    }

    /// Removes `provider`'s registrations of `service` (all major versions).
    /// Returns how many were removed.
    pub fn withdraw(&self, provider: &ProviderId, service: &ServiceId) -> usize {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let removed = match inner.services.get_mut(service) {
            Some(entries) => {
                let before = entries.len();
                entries.retain(|e| &e.provider != provider);
                before - entries.len()
            }
            None => 0,
        };
        inner.services.retain(|_, entries| !entries.is_empty());
        drop(inner);
        if removed > 0 {
            tracing::debug!(%service, %provider, "service withdrawn");
            self.bump();
        }
        removed
    }

    /// Removes every service `provider` registered, e.g. when its mod is
    /// disabled. Returns how many registrations were removed.
    pub fn withdraw_provider(&self, provider: &ProviderId) -> usize {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let mut removed = 0;
        for entries in inner.services.values_mut() {
            let before = entries.len();
            entries.retain(|e| &e.provider != provider);
            removed += before - entries.len();
        }
        inner.services.retain(|_, entries| !entries.is_empty());
        drop(inner);
        if removed > 0 {
            tracing::debug!(%provider, removed, "provider's services withdrawn");
            self.bump();
        }
        removed
    }

    /// Every registration, sorted by service id, version and then provider.
    pub fn services(&self) -> Vec<ServiceDescriptor> {
        let inner = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<ServiceDescriptor> = inner
            .services
            .iter()
            .flat_map(|(id, entries)| {
                let inner = &inner;
                entries.iter().map(move |e| {
                    let active = inner
                        .active(id, e.version.major)
                        .is_some_and(|a| a.seq == e.seq);
                    ServiceDescriptor {
                        service: ServiceRef::new(id.clone(), e.version),
                        provider: e.provider.clone(),
                        kind: e.kind,
                        active,
                    }
                })
            })
            .collect();
        out.sort_by(|a, b| (&a.service, &a.provider).cmp(&(&b.service, &b.provider)));
        out
    }

    /// A counter that increases on every change.
    pub fn generation(&self) -> u64 {
        *self.generation.borrow()
    }

    /// Watches [`generation`](Self::generation), to re-resolve services after a change.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    fn bump(&self) {
        self.generation.send_modify(|g| *g += 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Git: Send + Sync {
        fn name(&self) -> &'static str;
    }

    struct BuiltinGit;
    impl Git for BuiltinGit {
        fn name(&self) -> &'static str {
            "builtin"
        }
    }

    struct ModGit;
    impl Git for ModGit {
        fn name(&self) -> &'static str {
            "mod"
        }
    }

    fn sref(s: &str) -> ServiceRef {
        s.parse().unwrap()
    }

    fn pid(s: &str) -> ProviderId {
        ProviderId::new(s).unwrap()
    }

    fn git(registry: &ServiceRegistry, request: &str) -> Result<&'static str, ServiceError> {
        registry
            .consume::<dyn Git>(&sref(request))
            .map(|g| g.name())
    }

    #[test]
    fn resolves_a_provided_service() {
        let registry = ServiceRegistry::new();
        let value: Arc<dyn Git> = Arc::new(BuiltinGit);
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1.2"),
            value,
        );

        assert_eq!(git(&registry, "coqo.git@1"), Ok("builtin"));
        assert_eq!(git(&registry, "coqo.git@1.2"), Ok("builtin"));
        let resolved = registry.resolve(&sref("coqo.git@1")).unwrap();
        assert_eq!(resolved.service, sref("coqo.git@1.2"));
        assert_eq!(resolved.provider, pid("coqo.git"));
    }

    #[test]
    fn unknown_service_is_not_found() {
        let registry = ServiceRegistry::new();
        assert_eq!(
            git(&registry, "coqo.git@1"),
            Err(ServiceError::NotFound(sref("coqo.git@1")))
        );
    }

    #[test]
    fn version_mismatch() {
        let registry = ServiceRegistry::new();
        let value: Arc<dyn Git> = Arc::new(BuiltinGit);
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1.1"),
            value,
        );

        let expected = |requested: &str| ServiceError::VersionMismatch {
            requested: sref(requested),
            available: vec![ServiceVersion::new(1, 1)],
        };
        // Newer minor than provided.
        assert_eq!(
            git(&registry, "coqo.git@1.2"),
            Err(expected("coqo.git@1.2"))
        );
        // Different major.
        assert_eq!(git(&registry, "coqo.git@2"), Err(expected("coqo.git@2")));
        let message = git(&registry, "coqo.git@2").unwrap_err().to_string();
        assert!(
            message.contains("coqo.git@2") && message.contains("1.1"),
            "{message}"
        );
    }

    #[test]
    fn majors_coexist() {
        let registry = ServiceRegistry::new();
        let v1: Arc<dyn Git> = Arc::new(BuiltinGit);
        let v2: Arc<dyn Git> = Arc::new(ModGit);
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1"),
            v1,
        );
        registry.provide(pid("acme.git"), ProviderKind::Mod, sref("coqo.git@2"), v2);

        assert_eq!(git(&registry, "coqo.git@1"), Ok("builtin"));
        assert_eq!(git(&registry, "coqo.git@2"), Ok("mod"));
    }

    #[test]
    fn mod_replaces_builtin_and_withdrawal_restores_it() {
        let registry = ServiceRegistry::new();
        let builtin: Arc<dyn Git> = Arc::new(BuiltinGit);
        let replacement: Arc<dyn Git> = Arc::new(ModGit);
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1"),
            builtin,
        );
        registry.provide(
            pid("acme.git"),
            ProviderKind::Mod,
            sref("coqo.git@1"),
            replacement,
        );
        assert_eq!(git(&registry, "coqo.git@1"), Ok("mod"));

        let listed = registry.services();
        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .any(|d| d.provider == pid("acme.git") && d.active)
        );
        assert!(
            listed
                .iter()
                .any(|d| d.provider == pid("coqo.git") && !d.active)
        );

        assert_eq!(registry.withdraw_provider(&pid("acme.git")), 1);
        assert_eq!(git(&registry, "coqo.git@1"), Ok("builtin"));
    }

    #[test]
    fn builtin_registered_later_does_not_displace_a_mod() {
        let registry = ServiceRegistry::new();
        let replacement: Arc<dyn Git> = Arc::new(ModGit);
        let builtin: Arc<dyn Git> = Arc::new(BuiltinGit);
        registry.provide(
            pid("acme.git"),
            ProviderKind::Mod,
            sref("coqo.git@1"),
            replacement,
        );
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1"),
            builtin,
        );
        assert_eq!(git(&registry, "coqo.git@1"), Ok("mod"));
    }

    #[test]
    fn later_mod_replaces_earlier_mod() {
        let registry = ServiceRegistry::new();
        let first: Arc<dyn Git> = Arc::new(BuiltinGit);
        let second: Arc<dyn Git> = Arc::new(ModGit);
        registry.provide(pid("one.git"), ProviderKind::Mod, sref("coqo.git@1"), first);
        registry.provide(
            pid("two.git"),
            ProviderKind::Mod,
            sref("coqo.git@1"),
            second,
        );
        assert_eq!(git(&registry, "coqo.git@1"), Ok("mod"));
        assert_eq!(
            registry.withdraw(&pid("two.git"), &ServiceId::new("coqo.git").unwrap()),
            1
        );
        assert_eq!(git(&registry, "coqo.git@1"), Ok("builtin"));
    }

    #[test]
    fn replacement_with_too_old_minor_is_a_mismatch() {
        let registry = ServiceRegistry::new();
        let builtin: Arc<dyn Git> = Arc::new(BuiltinGit);
        let replacement: Arc<dyn Git> = Arc::new(ModGit);
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1.3"),
            builtin,
        );
        registry.provide(
            pid("acme.git"),
            ProviderKind::Mod,
            sref("coqo.git@1.1"),
            replacement,
        );
        assert_eq!(git(&registry, "coqo.git@1.1"), Ok("mod"));
        assert!(matches!(
            git(&registry, "coqo.git@1.3"),
            Err(ServiceError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn reproviding_replaces_own_registration() {
        let registry = ServiceRegistry::new();
        let first: Arc<dyn Git> = Arc::new(BuiltinGit);
        let second: Arc<dyn Git> = Arc::new(ModGit);
        registry.provide(
            pid("acme.git"),
            ProviderKind::Mod,
            sref("coqo.git@1"),
            first,
        );
        registry.provide(
            pid("acme.git"),
            ProviderKind::Mod,
            sref("coqo.git@1.1"),
            second,
        );
        assert_eq!(registry.services().len(), 1);
        assert_eq!(git(&registry, "coqo.git@1.1"), Ok("mod"));
    }

    #[test]
    fn type_mismatch() {
        let registry = ServiceRegistry::new();
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1"),
            Arc::new(42_u32),
        );
        assert_eq!(*registry.consume::<u32>(&sref("coqo.git@1")).unwrap(), 42);
        assert!(matches!(
            git(&registry, "coqo.git@1"),
            Err(ServiceError::TypeMismatch { actual: "u32", .. })
        ));
    }

    #[test]
    fn generation_tracks_changes() {
        let registry = ServiceRegistry::new();
        let mut rx = registry.subscribe();
        assert_eq!(registry.generation(), 0);
        registry.provide(
            pid("coqo.git"),
            ProviderKind::Builtin,
            sref("coqo.git@1"),
            Arc::new(()),
        );
        assert_eq!(registry.generation(), 1);
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        assert_eq!(registry.withdraw_provider(&pid("nobody")), 0);
        assert!(!rx.has_changed().unwrap());
        assert_eq!(registry.withdraw_provider(&pid("coqo.git")), 1);
        assert_eq!(registry.generation(), 2);
        assert!(registry.services().is_empty());
    }
}
