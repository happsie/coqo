// SPDX-License-Identifier: Apache-2.0

//! The release public key.
//!
//! `crates/updater/release.pub` is compiled into every build: the
//! two-line minisign public key file (`untrusted comment: …` and the
//! base64 key) that `minisign -G` writes. Until the maintainer commits the
//! real one it holds a placeholder (the release workflow, #348, refuses to
//! publish while it says `PLACEHOLDER`); a build with it, or with anything
//! that does not parse as a key, has no release key and refuses every
//! update with [`UpdateError::NoReleaseKey`].
//!
//! There is no other way in: no environment variable, no setting. Tests
//! make their own key and pass it to the updater under `cfg(test)` only.

use std::fmt;

use minisign_verify::PublicKey;

use crate::UpdateError;

/// The release public key file, as committed.
pub const RELEASE_PUB: &str = include_str!("../release.pub");

/// What the placeholder key file says.
pub const PLACEHOLDER: &str = "PLACEHOLDER";

/// A minisign public key that release signatures must verify against.
pub struct TrustedKey(pub(crate) PublicKey);

impl fmt::Debug for TrustedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrustedKey")
    }
}

impl TrustedKey {
    /// The key compiled into this build, or [`UpdateError::NoReleaseKey`]
    /// for the placeholder.
    pub fn release() -> Result<Self, UpdateError> {
        Self::parse(RELEASE_PUB)
    }

    /// Whether this build has a real release key.
    pub fn has_release_key() -> bool {
        Self::release().is_ok()
    }

    /// Parse a minisign public key file. The placeholder, or anything that
    /// is not a key, is [`UpdateError::NoReleaseKey`].
    pub(crate) fn parse(text: &str) -> Result<Self, UpdateError> {
        if text.contains(PLACEHOLDER) {
            return Err(UpdateError::NoReleaseKey);
        }
        PublicKey::decode(text)
            .map(Self)
            .map_err(|_| UpdateError::NoReleaseKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_placeholder_is_no_key() {
        assert!(
            RELEASE_PUB.lines().count() == 2,
            "two lines, as minisign writes"
        );
        // The release workflow's placeholder (#348).
        assert!(matches!(
            TrustedKey::parse(
                "untrusted comment: PLACEHOLDER - not a real key; the maintainer replaces this file (see docs/releasing.md)\n\
                 RWPLACEHOLDERPLACEHOLDERPLACEHOLDERPLACEHOLDERPLACEHOLDERPLACEHOLDER\n"
            ),
            Err(UpdateError::NoReleaseKey)
        ));
        // A well-formed key under a placeholder comment is still refused.
        assert!(matches!(
            TrustedKey::parse(
                "untrusted comment: PLACEHOLDER\n\
                 RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n"
            ),
            Err(UpdateError::NoReleaseKey)
        ));
        assert!(matches!(
            TrustedKey::parse(""),
            Err(UpdateError::NoReleaseKey)
        ));
        assert!(matches!(
            TrustedKey::parse("untrusted comment: x\nnot base64 at all\n"),
            Err(UpdateError::NoReleaseKey)
        ));
    }

    #[test]
    fn a_real_key_parses() {
        // minisign's own test key.
        let key = "untrusted comment: minisign public key E7620F1842B4E81F\n\
                   RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n";
        assert!(TrustedKey::parse(key).is_ok());
    }
}
