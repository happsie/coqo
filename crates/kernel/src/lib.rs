// SPDX-License-Identifier: Apache-2.0

//! Coqo kernel: the small core everything else plugs into (PLAN §5, §10).
//!
//! - [`service`]: typed, versioned service registry (`provide` / `consume`
//!   services such as `coqo.git@1`), where a mod can replace a built-in.
//! - [`contribution`]: contribution registry keyed by contribution point
//!   (`commands`, `views.sidebar`, …).
//! - [`supervisor`]: supervises child processes (language servers, agents,
//!   process mods): process groups, restart with backoff, output capture and
//!   RSS per child.
//! - [`logging`]: `tracing` subscriber setup for the binaries.
//! - [`mods`]: the kernel's `ModHost`, the built-in mod table and the mod
//!   lifecycle (load, activate on activation events, disable, status).
//! - [`id`]: the identifiers the registries use, re-exported from
//!   `coqo-mod-api` so mods and the kernel share them.

pub mod contribution;
pub mod id;
pub mod logging;
pub mod mods;
pub mod service;
pub mod supervisor;

pub use contribution::{Contribution, ContributionError, ContributionRegistry};
pub use id::{
    ContributionId, ContributionPointId, IdError, ProviderId, ServiceId, ServiceRef, ServiceVersion,
};
pub use service::{ProviderKind, ServiceDescriptor, ServiceError, ServiceRegistry};
pub use supervisor::{
    Backoff, ChildEvent, ChildExit, ChildHandle, ChildId, ChildIo, ChildSnapshot, ChildSpec,
    ChildState, ChildStatus, ChildStdio, OutputLine, OutputStream, ResourceUsage, RestartMode,
    RestartPolicy, Supervisor,
};
