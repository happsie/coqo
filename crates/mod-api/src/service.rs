// SPDX-License-Identifier: Apache-2.0

//! Typed service handles.
//!
//! A service is a Rust trait (or type) with an id and an interface version.
//! Implement [`Service`] for the trait object to give it both, then provide
//! and consume it through [`ModContext`](crate::ModContext):
//!
//! ```
//! use std::sync::Arc;
//! use coqo_mod_api::{ModContext, ModError, Service, ServiceVersion};
//!
//! /// `coqo.greeter@1`.
//! pub trait Greeter: Send + Sync {
//!     fn greet(&self, name: &str) -> String;
//! }
//!
//! impl Service for dyn Greeter {
//!     const ID: &'static str = "coqo.greeter";
//!     const VERSION: ServiceVersion = ServiceVersion::new(1, 0);
//! }
//!
//! struct English;
//! impl Greeter for English {
//!     fn greet(&self, name: &str) -> String {
//!         format!("Hello, {name}!")
//!     }
//! }
//!
//! fn activate(cx: &ModContext) -> Result<(), ModError> {
//!     cx.provide::<dyn Greeter>(Arc::new(English))?;
//!     let greeter = cx.consume::<dyn Greeter>()?;
//!     assert_eq!(greeter.greet("Ada"), "Hello, Ada!");
//!     Ok(())
//! }
//! # let host = coqo_mod_api::testing::TestHost::new();
//! # activate(&host.context("coqo.greeter".parse().unwrap())).unwrap();
//! ```
//!
//! Bump the minor version when you add methods with defaults (consumers of an
//! older minor still work) and the major version for anything incompatible:
//! that is a new interface, and both majors can be provided side by side.
//!
//! A service has at most one active provider per major version, and a mod
//! can replace a built-in provider. When several implementations must
//! coexist (agent providers, say), contribute each one to a contribution
//! point instead.

use crate::id::{ServiceId, ServiceRef, ServiceVersion};

/// A service interface with an id and version. Usually implemented for
/// `dyn Trait`.
pub trait Service: Send + Sync + 'static {
    /// The service id, such as `coqo.git`. Must be a valid [`ServiceId`].
    const ID: &'static str;

    /// The interface version this Rust type describes. Providers offer it;
    /// consumers ask for at least it.
    const VERSION: ServiceVersion;

    /// `ID@VERSION` as a [`ServiceRef`].
    ///
    /// # Panics
    ///
    /// If `ID` is not a valid service id, which is a bug in the service's
    /// definition.
    fn service_ref() -> ServiceRef {
        let id = ServiceId::new(Self::ID)
            .unwrap_or_else(|e| panic!("Service::ID of {}: {e}", std::any::type_name::<Self>()));
        ServiceRef::new(id, Self::VERSION)
    }
}
