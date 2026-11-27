// SPDX-License-Identifier: Apache-2.0

//! Public Coqo mod API: the only Coqo crate that mods may depend on (PLAN §5).
//!
//! Everything in Coqo is a mod, and every mod (built-in core mods included)
//! goes through this API:
//!
//! - [`Mod`]: a mod's code, with [`activate`](Mod::activate) and
//!   [`deactivate`](Mod::deactivate).
//! - [`ModContext`]: what a mod talks to. It provides and consumes typed
//!   [`Service`]s and contributes to typed [`points`].
//! - [`manifest`]: the `coqo.mod.toml` every mod ships: id, version, engine
//!   range, tier and entry, activation events, provides / consumes,
//!   permissions, and static contributions.
//! - [`contrib`]: the value types of the v1 contribution points.
//! - [`id`]: validated ids for mods, services and contributions.
//! - [`ModHost`]: the object-safe interface hosts implement (the kernel for
//!   core mods; the WASM and process hosts forward to their guests), and
//!   [`testing::TestHost`], an in-memory one for tests.
//!
//! # A core mod
//!
//! ```
//! use coqo_mod_api::contrib::{Command, StatusItem};
//! use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError, points};
//! use serde_json::Value;
//!
//! struct Hello;
//!
//! impl Mod for Hello {
//!     fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
//!         Box::pin(async move {
//!             let greet = "hello.greet".parse()?;
//!             cx.register_command(Command::new(greet, "Say hello"), |_args: Value| async {
//!                 Ok(Value::from("Hello!"))
//!             })?;
//!             cx.contribute::<points::StatusItems>(StatusItem::new("hello.status".parse()?, "Hello"))
//!         })
//!     }
//! }
//!
//! # use coqo_mod_api::testing::{TestHost, block_on};
//! # let host = TestHost::new();
//! # block_on(Hello.activate(host.context("hello".parse().unwrap()))).unwrap();
//! # assert_eq!(host.run_command("hello.greet", Value::Null).unwrap(), "Hello!");
//! ```
//!
//! This crate must not depend on any other Coqo crate; the kernel depends on
//! it instead and re-exports [`id`].

#![warn(missing_docs)]

pub mod contrib;
mod erased;
mod error;
pub mod id;
pub mod manifest;
mod module;
pub mod points;
mod service;
pub mod testing;

pub use erased::{AnyArc, BoxFuture};
pub use error::ModError;
pub use id::{
    ContributionId, ContributionPointId, IdError, ProviderId, ServiceId, ServiceRef, ServiceVersion,
};
pub use manifest::Manifest;
pub use module::{
    CommandHandler, McpToolCall, McpToolHandler, McpToolOutput, Mod, ModContext, ModHost,
    ResolvedService,
};
pub use points::ContributionPoint;
pub use service::Service;

/// Re-exported so mods use the same `semver` as [`Manifest`].
pub use semver;
/// Re-exported so mods use the same `serde_json` as command arguments and
/// schemas.
pub use serde_json;

/// Name of this crate.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Version of this mod API, which a manifest's `engine` range is checked
/// against. It follows the Coqo release version.
pub const API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// [`API_VERSION`] parsed.
pub fn api_version() -> semver::Version {
    semver::Version::parse(API_VERSION).expect("crate version is valid semver")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_set() {
        assert_eq!(CRATE_NAME, "coqo-mod-api");
    }

    #[test]
    fn api_version_parses() {
        assert_eq!(api_version().to_string(), API_VERSION);
    }
}
