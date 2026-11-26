// SPDX-License-Identifier: Apache-2.0

//! Coqo kernel: registry, services, supervisor and event log.
//!
//! Placeholder crate: see `docs/PLAN.md` §10. Real code lands in later milestones.

/// Name of this crate, used by the workspace smoke tests.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_set() {
        assert_eq!(CRATE_NAME, "coqo-kernel");
    }
}
