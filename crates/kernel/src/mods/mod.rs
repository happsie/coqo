// SPDX-License-Identifier: Apache-2.0

//! Running mods in the kernel (PLAN §5).
//!
//! - [`KernelHost`] implements [`coqo_mod_api::ModHost`] over the kernel's
//!   [`ServiceRegistry`](crate::ServiceRegistry) and
//!   [`ContributionRegistry`](crate::ContributionRegistry), so a mod's
//!   services and contributions are ordinary typed registry entries: a
//!   point's owner reads `P::Value`s back with
//!   [`contributions_of::<P>`](crate::ContributionRegistry::contributions_of),
//!   and kernel code consumes services with
//!   [`ServiceRegistry::consume`](crate::ServiceRegistry::consume).
//! - [`BuiltinMods`] is the explicit table of core mods linked into the
//!   daemon, plus the contribution points their crates own.
//! - [`ModManager`] loads mods, activates them on their activation events,
//!   disables them and reports their [`ModStatus`].
//!
//! ```
//! use std::sync::Arc;
//! use coqo_kernel::mods::{BuiltinMods, ModManager, ModState};
//! use coqo_kernel::{ContributionRegistry, ServiceRegistry};
//! use coqo_mod_api::contrib::StatusItem;
//! use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError, points};
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
//! const MANIFEST: &str = r#"
//! id = "coqo.clock"
//! version = "0.1.0"
//! engine = "*"
//! tier = "core"
//! activation = ["startup"]
//! "#;
//!
//! # tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
//! let contributions = Arc::new(ContributionRegistry::new());
//! let mods = ModManager::with_registries(Arc::new(ServiceRegistry::new()), contributions.clone());
//! assert!(mods.load_builtins(BuiltinMods::new().with(MANIFEST, Clock)).is_empty());
//! mods.start().await;
//!
//! assert_eq!(mods.status()[0].state, ModState::Active);
//! let items = contributions.contributions_of::<points::StatusItems>().unwrap();
//! assert_eq!(items[0].value.text, "12:00");
//! # });
//! ```

mod builtin;
mod guard;
mod host;
mod manager;

pub use builtin::BuiltinMods;
pub use host::KernelHost;
pub use manager::{ModManager, ModState, ModStatus};
