// SPDX-License-Identifier: Apache-2.0

//! Contribution registry keyed by contribution point (PLAN §5).
//!
//! The owner of a contribution point (the UI for `views.sidebar`, the command
//! system for `commands`, …) [declares](ContributionRegistry::declare_point) it
//! with the Rust type its contributions carry. Mods then
//! [contribute](ContributionRegistry::contribute) values under an id that is
//! unique within the point, and the owner reads them back with
//! [`contributions`](ContributionRegistry::contributions).
//!
//! Contributions from process and WASM mods arrive as data, so their points
//! will typically use a serde type (or a JSON value); core mods can use any
//! `Send + Sync` type.
//!
//! Points with a [`ContributionPoint`] marker type (every point in
//! [`coqo_mod_api::points`], and any a crate defines the same way) have typed
//! shorthands: [`declare`](ContributionRegistry::declare),
//! [`contributions_of`](ContributionRegistry::contributions_of) and
//! [`get_of`](ContributionRegistry::get_of).
//! [`declare_mod_api_points`](ContributionRegistry::declare_mod_api_points)
//! declares all of `coqo_mod_api::points`. Mods contribute through the
//! type-erased [`contribute_any`](ContributionRegistry::contribute_any),
//! which the kernel's [`ModHost`](coqo_mod_api::ModHost) uses.

use std::any::{Any, TypeId, type_name};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use coqo_mod_api::points::{self, ContributionPoint};
use coqo_mod_api::{AnyArc, ModError};
use tokio::sync::watch;

use crate::id::{ContributionId, ContributionPointId, ProviderId};

/// Why a contribution could not be declared, made or read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ContributionError {
    /// The point hasn't been declared.
    #[error("unknown contribution point `{0}`")]
    UnknownPoint(ContributionPointId),
    /// The point was declared with a different contribution type.
    #[error("contribution point `{point}` takes `{declared}`, not `{requested}`")]
    TypeMismatch {
        /// The point.
        point: ContributionPointId,
        /// Type the point was declared with.
        declared: &'static str,
        /// Type used in this call.
        requested: &'static str,
    },
    /// Another provider already contributed this id to the point.
    #[error("`{id}` is already contributed to `{point}` by `{existing}`")]
    Duplicate {
        /// The point.
        point: ContributionPointId,
        /// The contested id.
        id: ContributionId,
        /// Who contributed it first.
        existing: ProviderId,
    },
}

impl From<ContributionError> for ModError {
    fn from(e: ContributionError) -> Self {
        match e {
            ContributionError::UnknownPoint(point) => ModError::UnknownPoint(point),
            ContributionError::TypeMismatch {
                point,
                declared,
                requested,
            } => ModError::ContributionTypeMismatch {
                point,
                declared,
                requested,
            },
            ContributionError::Duplicate {
                point,
                id,
                existing,
            } => ModError::DuplicateContribution {
                point,
                id,
                existing,
            },
        }
    }
}

/// One contribution, as returned to the point's owner.
#[derive(Debug)]
pub struct Contribution<T> {
    /// Id within the point.
    pub id: ContributionId,
    /// Who contributed it.
    pub provider: ProviderId,
    /// The contributed value.
    pub value: Arc<T>,
}

impl<T> Clone for Contribution<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            provider: self.provider.clone(),
            value: Arc::clone(&self.value),
        }
    }
}

struct Entry {
    id: ContributionId,
    provider: ProviderId,
    value: Arc<dyn Any + Send + Sync>,
}

/// Turns an erased `Arc<T>` into the registry's `Arc<dyn Any>` holding a
/// `T`, if it holds the point's type.
type Unerase = fn(&AnyArc) -> Option<Arc<dyn Any + Send + Sync>>;

fn unerase<T: Send + Sync + 'static>(value: &AnyArc) -> Option<Arc<dyn Any + Send + Sync>> {
    value
        .downcast::<T>()
        .map(|v| v as Arc<dyn Any + Send + Sync>)
}

struct Point {
    type_id: TypeId,
    type_name: &'static str,
    unerase: Unerase,
    /// In registration order.
    entries: Vec<Entry>,
}

impl Point {
    fn check<T: 'static>(&self, point: &ContributionPointId) -> Result<(), ContributionError> {
        if self.type_id == TypeId::of::<T>() {
            Ok(())
        } else {
            Err(ContributionError::TypeMismatch {
                point: point.clone(),
                declared: self.type_name,
                requested: type_name::<T>(),
            })
        }
    }
}

/// Registry of contribution points and their contributions. Share it behind
/// an `Arc`; all methods take `&self`.
pub struct ContributionRegistry {
    points: RwLock<BTreeMap<ContributionPointId, Point>>,
    generation: watch::Sender<u64>,
}

impl Default for ContributionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ContributionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let points = self.points.read().unwrap_or_else(PoisonError::into_inner);
        let mut map = f.debug_map();
        for (id, point) in points.iter() {
            map.entry(&id.as_str(), &point.entries.len());
        }
        map.finish()
    }
}

impl ContributionRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            points: RwLock::default(),
            generation: watch::Sender::new(0),
        }
    }

    /// Declares `point`, whose contributions are `T`s. Declaring the same
    /// point again with the same type is a no-op.
    pub fn declare_point<T>(&self, point: ContributionPointId) -> Result<(), ContributionError>
    where
        T: Send + Sync + 'static,
    {
        let mut points = self.points.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = points.get(&point) {
            return existing.check::<T>(&point);
        }
        tracing::debug!(%point, ty = type_name::<T>(), "contribution point declared");
        points.insert(
            point,
            Point {
                type_id: TypeId::of::<T>(),
                type_name: type_name::<T>(),
                unerase: unerase::<T>,
                entries: Vec::new(),
            },
        );
        drop(points);
        self.bump();
        Ok(())
    }

    /// Declares point `P` with its value type. Declaring it again is a no-op.
    pub fn declare<P: ContributionPoint>(&self) -> Result<(), ContributionError> {
        self.declare_point::<P::Value>(P::id())
    }

    /// Declares every contribution point of the mod API
    /// ([`coqo_mod_api::points::ALL`]) with its value type. The kernel's mod
    /// host does this when it is created. Idempotent.
    pub fn declare_mod_api_points(&self) -> Result<(), ContributionError> {
        // Keep in step with `coqo_mod_api::points`; a test checks the ids
        // against `points::ALL`.
        self.declare::<points::Commands>()?;
        self.declare::<points::CommandHandlers>()?;
        self.declare::<points::Keybindings>()?;
        self.declare::<points::Menus>()?;
        self.declare::<points::Settings>()?;
        self.declare::<points::SettingsSections>()?;
        self.declare::<points::SidebarViews>()?;
        self.declare::<points::PanelViews>()?;
        self.declare::<points::TabViews>()?;
        self.declare::<points::RailViews>()?;
        self.declare::<points::StatusItems>()?;
        self.declare::<points::Languages>()?;
        self.declare::<points::LspServers>()?;
        self.declare::<points::DebugAdapters>()?;
        self.declare::<points::McpTools>()?;
        self.declare::<points::McpToolHandlers>()?;
        self.declare::<points::AgentProviders>()?;
        self.declare::<points::SlashCommands>()?;
        self.declare::<points::ContextProviders>()?;
        self.declare::<points::AlertSources>()?;
        self.declare::<points::Themes>()?;
        Ok(())
    }

    /// Adds `provider`'s contribution `id` to `point`.
    ///
    /// If `provider` already contributed `id` there, the value is replaced in
    /// place (for hot reload). If another provider did, this fails with
    /// [`ContributionError::Duplicate`].
    pub fn contribute<T>(
        &self,
        point: &ContributionPointId,
        provider: ProviderId,
        id: ContributionId,
        value: T,
    ) -> Result<(), ContributionError>
    where
        T: Send + Sync + 'static,
    {
        self.insert(point, provider, id, |p| {
            p.check::<T>(point)?;
            Ok(Arc::new(value))
        })
    }

    /// [`contribute`](Self::contribute) with a type-erased value: an
    /// [`AnyArc`] holding an `Arc` of the point's value type, as mods hand it
    /// to the [`ModHost`](coqo_mod_api::ModHost). The registry keeps that
    /// `Arc`, so readers get the mod's own value back.
    pub fn contribute_any(
        &self,
        point: &ContributionPointId,
        provider: ProviderId,
        id: ContributionId,
        value: &AnyArc,
    ) -> Result<(), ContributionError> {
        self.insert(point, provider, id, |p| {
            (p.unerase)(value).ok_or_else(|| ContributionError::TypeMismatch {
                point: point.clone(),
                declared: p.type_name,
                requested: value.type_name(),
            })
        })
    }

    fn insert(
        &self,
        point: &ContributionPointId,
        provider: ProviderId,
        id: ContributionId,
        value: impl FnOnce(&Point) -> Result<Arc<dyn Any + Send + Sync>, ContributionError>,
    ) -> Result<(), ContributionError> {
        let mut points = self.points.write().unwrap_or_else(PoisonError::into_inner);
        let entry_point = points
            .get_mut(point)
            .ok_or_else(|| ContributionError::UnknownPoint(point.clone()))?;
        let value = value(entry_point)?;
        match entry_point.entries.iter_mut().find(|e| e.id == id) {
            Some(existing) if existing.provider != provider => {
                return Err(ContributionError::Duplicate {
                    point: point.clone(),
                    id,
                    existing: existing.provider.clone(),
                });
            }
            Some(existing) => existing.value = value,
            None => {
                tracing::debug!(%point, %id, %provider, "contribution added");
                entry_point.entries.push(Entry {
                    id,
                    provider,
                    value,
                });
            }
        }
        drop(points);
        self.bump();
        Ok(())
    }

    /// All contributions to point `P`, in registration order.
    pub fn contributions_of<P: ContributionPoint>(
        &self,
    ) -> Result<Vec<Contribution<P::Value>>, ContributionError> {
        self.contributions::<P::Value>(&P::id())
    }

    /// The contribution `id` in point `P`, if any.
    pub fn get_of<P: ContributionPoint>(
        &self,
        id: &ContributionId,
    ) -> Result<Option<Contribution<P::Value>>, ContributionError> {
        self.get::<P::Value>(&P::id(), id)
    }

    /// All contributions to `point`, in registration order.
    pub fn contributions<T>(
        &self,
        point: &ContributionPointId,
    ) -> Result<Vec<Contribution<T>>, ContributionError>
    where
        T: Send + Sync + 'static,
    {
        let points = self.points.read().unwrap_or_else(PoisonError::into_inner);
        let p = points
            .get(point)
            .ok_or_else(|| ContributionError::UnknownPoint(point.clone()))?;
        p.check::<T>(point)?;
        Ok(p.entries.iter().map(Self::typed).collect())
    }

    /// The contribution `id` in `point`, if any.
    pub fn get<T>(
        &self,
        point: &ContributionPointId,
        id: &ContributionId,
    ) -> Result<Option<Contribution<T>>, ContributionError>
    where
        T: Send + Sync + 'static,
    {
        let points = self.points.read().unwrap_or_else(PoisonError::into_inner);
        let p = points
            .get(point)
            .ok_or_else(|| ContributionError::UnknownPoint(point.clone()))?;
        p.check::<T>(point)?;
        Ok(p.entries.iter().find(|e| &e.id == id).map(Self::typed))
    }

    fn typed<T: Send + Sync + 'static>(entry: &Entry) -> Contribution<T> {
        Contribution {
            id: entry.id.clone(),
            provider: entry.provider.clone(),
            value: Arc::clone(&entry.value)
                .downcast::<T>()
                .expect("point type checked by caller"),
        }
    }

    /// Removes `provider`'s contribution `id` from `point`. Returns whether
    /// anything was removed.
    pub fn withdraw(
        &self,
        point: &ContributionPointId,
        provider: &ProviderId,
        id: &ContributionId,
    ) -> bool {
        let mut points = self.points.write().unwrap_or_else(PoisonError::into_inner);
        let removed = points.get_mut(point).is_some_and(|p| {
            let before = p.entries.len();
            p.entries
                .retain(|e| !(&e.id == id && &e.provider == provider));
            p.entries.len() != before
        });
        drop(points);
        if removed {
            self.bump();
        }
        removed
    }

    /// Removes every contribution `provider` made, e.g. when its mod is
    /// disabled. Declared points stay. Returns how many were removed.
    pub fn withdraw_provider(&self, provider: &ProviderId) -> usize {
        let mut points = self.points.write().unwrap_or_else(PoisonError::into_inner);
        let mut removed = 0;
        for p in points.values_mut() {
            let before = p.entries.len();
            p.entries.retain(|e| &e.provider != provider);
            removed += before - p.entries.len();
        }
        drop(points);
        if removed > 0 {
            tracing::debug!(%provider, removed, "provider's contributions withdrawn");
            self.bump();
        }
        removed
    }

    /// Declared points, sorted.
    pub fn points(&self) -> Vec<ContributionPointId> {
        let points = self.points.read().unwrap_or_else(PoisonError::into_inner);
        points.keys().cloned().collect()
    }

    /// A counter that increases on every change.
    pub fn generation(&self) -> u64 {
        *self.generation.borrow()
    }

    /// Watches [`generation`](Self::generation), to re-read contributions after a change.
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

    #[derive(Debug, PartialEq)]
    struct Command {
        title: &'static str,
    }

    fn point(s: &str) -> ContributionPointId {
        ContributionPointId::new(s).unwrap()
    }
    fn pid(s: &str) -> ProviderId {
        ProviderId::new(s).unwrap()
    }
    fn cid(s: &str) -> ContributionId {
        ContributionId::new(s).unwrap()
    }

    fn titles(registry: &ContributionRegistry) -> Vec<&'static str> {
        registry
            .contributions::<Command>(&point("commands"))
            .unwrap()
            .iter()
            .map(|c| c.value.title)
            .collect()
    }

    #[test]
    fn contributions_in_registration_order() {
        let registry = ContributionRegistry::new();
        registry
            .declare_point::<Command>(point("commands"))
            .unwrap();
        registry
            .contribute(
                &point("commands"),
                pid("coqo.git"),
                cid("git.commit"),
                Command { title: "Commit" },
            )
            .unwrap();
        registry
            .contribute(
                &point("commands"),
                pid("coqo.editor"),
                cid("editor.save"),
                Command { title: "Save" },
            )
            .unwrap();
        assert_eq!(titles(&registry), ["Commit", "Save"]);

        let save = registry
            .get::<Command>(&point("commands"), &cid("editor.save"))
            .unwrap()
            .unwrap();
        assert_eq!(save.provider, pid("coqo.editor"));
        assert!(
            registry
                .get::<Command>(&point("commands"), &cid("nope"))
                .unwrap()
                .is_none()
        );
        assert_eq!(registry.points(), [point("commands")]);
    }

    #[test]
    fn unknown_point_and_type_mismatch() {
        let registry = ContributionRegistry::new();
        assert_eq!(
            registry.contribute(
                &point("commands"),
                pid("a"),
                cid("x"),
                Command { title: "X" }
            ),
            Err(ContributionError::UnknownPoint(point("commands")))
        );
        registry
            .declare_point::<Command>(point("commands"))
            .unwrap();
        // Same type again is fine; a different one isn't.
        registry
            .declare_point::<Command>(point("commands"))
            .unwrap();
        assert!(matches!(
            registry.declare_point::<String>(point("commands")),
            Err(ContributionError::TypeMismatch { .. })
        ));
        assert!(matches!(
            registry.contribute(
                &point("commands"),
                pid("a"),
                cid("x"),
                "not a command".to_owned()
            ),
            Err(ContributionError::TypeMismatch { .. })
        ));
        assert!(matches!(
            registry.contributions::<String>(&point("commands")),
            Err(ContributionError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn duplicates_and_replacement() {
        let registry = ContributionRegistry::new();
        registry
            .declare_point::<Command>(point("commands"))
            .unwrap();
        registry
            .contribute(
                &point("commands"),
                pid("a"),
                cid("x"),
                Command { title: "A" },
            )
            .unwrap();
        assert_eq!(
            registry.contribute(
                &point("commands"),
                pid("b"),
                cid("x"),
                Command { title: "B" }
            ),
            Err(ContributionError::Duplicate {
                point: point("commands"),
                id: cid("x"),
                existing: pid("a"),
            })
        );
        // Same provider re-contributing replaces its value.
        registry
            .contribute(
                &point("commands"),
                pid("a"),
                cid("x"),
                Command { title: "A2" },
            )
            .unwrap();
        assert_eq!(titles(&registry), ["A2"]);
    }

    #[test]
    fn withdrawal() {
        let registry = ContributionRegistry::new();
        registry
            .declare_point::<Command>(point("commands"))
            .unwrap();
        registry.declare_point::<u32>(point("numbers")).unwrap();
        registry
            .contribute(
                &point("commands"),
                pid("a"),
                cid("x"),
                Command { title: "X" },
            )
            .unwrap();
        registry
            .contribute(
                &point("commands"),
                pid("b"),
                cid("y"),
                Command { title: "Y" },
            )
            .unwrap();
        registry
            .contribute(&point("numbers"), pid("a"), cid("one"), 1_u32)
            .unwrap();

        assert!(!registry.withdraw(&point("commands"), &pid("b"), &cid("x")));
        assert!(registry.withdraw(&point("commands"), &pid("b"), &cid("y")));
        assert_eq!(titles(&registry), ["X"]);

        let generation = registry.generation();
        assert_eq!(registry.withdraw_provider(&pid("a")), 2);
        assert!(registry.generation() > generation);
        assert!(titles(&registry).is_empty());
        assert_eq!(registry.points().len(), 2);
    }

    #[test]
    fn declares_every_mod_api_point() {
        let registry = ContributionRegistry::new();
        registry.declare_mod_api_points().unwrap();
        registry.declare_mod_api_points().unwrap();
        let mut declared: Vec<String> = registry.points().iter().map(|p| p.to_string()).collect();
        let mut all: Vec<String> = points::ALL.iter().map(|p| (*p).to_owned()).collect();
        declared.sort();
        all.sort();
        assert_eq!(declared, all);
    }

    #[test]
    fn erased_contributions_keep_the_arc_and_check_the_type() {
        let registry = ContributionRegistry::new();
        registry.declare::<points::Commands>().unwrap();
        let command = Arc::new(coqo_mod_api::contrib::Command::new(cid("x"), "X"));
        registry
            .contribute_any(
                &point("commands"),
                pid("a"),
                cid("x"),
                &AnyArc::new(Arc::clone(&command)),
            )
            .unwrap();
        let got = registry
            .get_of::<points::Commands>(&cid("x"))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&got.value, &command));

        let err = registry
            .contribute_any(
                &point("commands"),
                pid("a"),
                cid("y"),
                &AnyArc::from_value(7_u32),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ContributionError::TypeMismatch {
                requested: "u32",
                ..
            }
        ));
        assert!(matches!(
            ModError::from(err),
            ModError::ContributionTypeMismatch { .. }
        ));
        assert_eq!(
            registry.contribute_any(
                &point("nope"),
                pid("a"),
                cid("x"),
                &AnyArc::from_value(1_u8)
            ),
            Err(ContributionError::UnknownPoint(point("nope")))
        );
    }
}
