// SPDX-License-Identifier: Apache-2.0

//! The in-app updater (issue #349). Placeholder crate created by the release
//! workflow (issue #348) so that `release.pub` has a home; #349 replaces it.

/// The minisign public key that release archives and manifests are verified
/// against. A placeholder until the maintainer commits the real key
/// (docs/releasing.md), and verification must refuse it while it is one.
pub const RELEASE_PUBLIC_KEY: &str = include_str!("../release.pub");

/// True while `release.pub` is still the placeholder.
pub fn release_key_is_placeholder() -> bool {
    RELEASE_PUBLIC_KEY.contains("PLACEHOLDER")
}

#[cfg(test)]
mod tests {
    #[test]
    fn key_file_is_two_lines() {
        assert_eq!(super::RELEASE_PUBLIC_KEY.lines().count(), 2);
    }
}
