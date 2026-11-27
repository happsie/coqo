// SPDX-License-Identifier: Apache-2.0

//! Identifiers shared by the service and contribution registries.
//!
//! They live in `coqo-mod-api` (the public mod API), so mods and the kernel
//! use the same types. This module re-exports them for kernel callers.

pub use coqo_mod_api::id::*;
