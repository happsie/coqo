// SPDX-License-Identifier: Apache-2.0

//! An in-memory [`ModHost`] for unit-testing mods without the kernel.
//!
//! ```
//! use coqo_mod_api::testing::{TestHost, block_on};
//! use coqo_mod_api::{Mod, ModContext, ModError, BoxFuture, points, contrib::StatusItem};
//!
//! struct Clock;
//! impl Mod for Clock {
//!     fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
//!         Box::pin(async move {
//!             cx.contribute::<points::StatusItems>(StatusItem::new("clock.time".parse()?, "12:00"))
//!         })
//!     }
//! }
//!
//! let host = TestHost::new();
//! block_on(Clock.activate(host.context("clock".parse().unwrap()))).unwrap();
//! let item = host.contribution::<points::StatusItems>("clock.time").unwrap();
//! assert_eq!(item.text, "12:00");
//! ```
//!
//! It resolves services like the kernel does, minus built-in versus mod
//! precedence: per service id and major version, the most recent provider
//! wins.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use serde_json::Value;

use crate::erased::AnyArc;
use crate::error::ModError;
use crate::id::{ContributionId, ContributionPointId, ProviderId, ServiceId, ServiceRef};
use crate::module::{ModContext, ModHost, ResolvedService};
use crate::points::{CommandHandlers, ContributionPoint};

/// One contribution, as [`TestHost::contributions`] returns it.
#[derive(Debug, Clone)]
pub struct Contributed<T: ?Sized> {
    /// Id within the point.
    pub id: ContributionId,
    /// Who contributed it.
    pub provider: ProviderId,
    /// The value.
    pub value: Arc<T>,
}

struct ServiceEntry {
    provider: ProviderId,
    service: ServiceRef,
    seq: u64,
    value: AnyArc,
}

struct ContributionEntry {
    id: ContributionId,
    provider: ProviderId,
    value: AnyArc,
}

#[derive(Default)]
struct State {
    next_seq: u64,
    services: Vec<ServiceEntry>,
    /// Per point, in registration order.
    points: BTreeMap<ContributionPointId, Vec<ContributionEntry>>,
}

/// In-memory [`ModHost`]. Any point accepts contributions; the first one
/// fixes the point's value type.
#[derive(Default)]
pub struct TestHost {
    state: Mutex<State>,
}

impl fmt::Debug for TestHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        let mut map = f.debug_map();
        for (point, entries) in &state.points {
            map.entry(&point.as_str(), &entries.len());
        }
        map.finish()
    }
}

impl TestHost {
    /// An empty host.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A [`ModContext`] for mod `mod_id` on this host.
    pub fn context(self: &Arc<Self>, mod_id: ProviderId) -> ModContext {
        ModContext::new(mod_id, Arc::clone(self) as Arc<dyn ModHost>)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// All contributions to point `P`, in registration order.
    ///
    /// # Panics
    ///
    /// If the point holds values of another type.
    pub fn contributions<P: ContributionPoint>(&self) -> Vec<Contributed<P::Value>> {
        let state = self.lock();
        state
            .points
            .get(&P::id())
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| Contributed {
                        id: e.id.clone(),
                        provider: e.provider.clone(),
                        value: e.value.downcast::<P::Value>().unwrap_or_else(|| {
                            panic!("point `{}` holds `{}`", P::ID, e.value.type_name())
                        }),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The contribution `id` in point `P`, if any.
    ///
    /// # Panics
    ///
    /// If `id` is not a valid contribution id, or the point holds values of
    /// another type.
    pub fn contribution<P: ContributionPoint>(&self, id: &str) -> Option<Arc<P::Value>> {
        let id = ContributionId::new(id).expect("valid contribution id");
        self.contributions::<P>()
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| c.value)
    }

    /// Every service registration as `(service, provider)`, oldest first.
    pub fn services(&self) -> Vec<(ServiceRef, ProviderId)> {
        let state = self.lock();
        state
            .services
            .iter()
            .map(|e| (e.service.clone(), e.provider.clone()))
            .collect()
    }

    /// Runs command `id` through its registered handler and waits for it.
    pub fn run_command(&self, id: &str, args: Value) -> Result<Value, ModError> {
        let id: ContributionId = id.parse()?;
        let handler = self
            .contributions::<CommandHandlers>()
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| Arc::clone(&c.value))
            .ok_or(ModError::UnknownCommand(id))?;
        block_on(handler.run(args))
    }

    /// Removes everything `mod_id` registered, as a host does after
    /// [`Mod::deactivate`](crate::Mod::deactivate). Returns how many
    /// registrations were removed.
    pub fn withdraw_mod(&self, mod_id: &ProviderId) -> usize {
        let mut state = self.lock();
        let before = state.services.len();
        state.services.retain(|e| &e.provider != mod_id);
        let mut removed = before - state.services.len();
        for entries in state.points.values_mut() {
            let before = entries.len();
            entries.retain(|e| &e.provider != mod_id);
            removed += before - entries.len();
        }
        removed
    }
}

impl ModHost for TestHost {
    fn provide_service(
        &self,
        provider: &ProviderId,
        service: ServiceRef,
        value: AnyArc,
    ) -> Result<(), ModError> {
        let mut state = self.lock();
        state.services.retain(|e| {
            !(&e.provider == provider
                && e.service.id == service.id
                && e.service.version.major == service.version.major)
        });
        let seq = state.next_seq;
        state.next_seq += 1;
        state.services.push(ServiceEntry {
            provider: provider.clone(),
            service,
            seq,
            value,
        });
        Ok(())
    }

    fn withdraw_service(&self, provider: &ProviderId, service: &ServiceId) -> Result<(), ModError> {
        self.lock()
            .services
            .retain(|e| !(&e.provider == provider && &e.service.id == service));
        Ok(())
    }

    fn consume_service(
        &self,
        _consumer: &ProviderId,
        request: &ServiceRef,
    ) -> Result<ResolvedService, ModError> {
        let state = self.lock();
        let same_id: Vec<&ServiceEntry> = state
            .services
            .iter()
            .filter(|e| e.service.id == request.id)
            .collect();
        if same_id.is_empty() {
            return Err(ModError::ServiceNotFound(request.clone()));
        }
        let active = same_id
            .iter()
            .filter(|e| e.service.version.major == request.version.major)
            .max_by_key(|e| e.seq);
        match active {
            Some(e) if e.service.version.satisfies(request.version) => Ok(ResolvedService {
                service: e.service.clone(),
                provider: e.provider.clone(),
                value: e.value.clone(),
            }),
            _ => {
                let mut available: Vec<_> = same_id.iter().map(|e| e.service.version).collect();
                available.sort();
                available.dedup();
                Err(ModError::ServiceVersionMismatch {
                    requested: request.clone(),
                    available,
                })
            }
        }
    }

    fn contribute(
        &self,
        provider: &ProviderId,
        point: &ContributionPointId,
        id: ContributionId,
        value: AnyArc,
    ) -> Result<(), ModError> {
        let mut state = self.lock();
        let entries = state.points.entry(point.clone()).or_default();
        if let Some(first) = entries.first()
            && !first.value.same_type(&value)
        {
            return Err(ModError::ContributionTypeMismatch {
                point: point.clone(),
                declared: first.value.type_name(),
                requested: value.type_name(),
            });
        }
        match entries.iter_mut().find(|e| e.id == id) {
            Some(existing) if &existing.provider != provider => {
                Err(ModError::DuplicateContribution {
                    point: point.clone(),
                    id,
                    existing: existing.provider.clone(),
                })
            }
            Some(existing) => {
                existing.value = value;
                Ok(())
            }
            None => {
                entries.push(ContributionEntry {
                    id,
                    provider: provider.clone(),
                    value,
                });
                Ok(())
            }
        }
    }

    fn withdraw_contribution(
        &self,
        provider: &ProviderId,
        point: &ContributionPointId,
        id: &ContributionId,
    ) -> Result<bool, ModError> {
        let mut state = self.lock();
        let Some(entries) = state.points.get_mut(point) else {
            return Ok(false);
        };
        let before = entries.len();
        entries.retain(|e| !(&e.id == id && &e.provider == provider));
        Ok(entries.len() != before)
    }
}

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to completion on the current thread. Enough for tests of
/// mods that don't need a runtime such as tokio.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}
