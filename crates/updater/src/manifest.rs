// SPDX-License-Identifier: Apache-2.0

//! The signed `manifest.json` of a release (issue #349's release format):
//!
//! ```json
//! { "version": "X.Y.Z", "protocol": "<major>.<minor>",
//!   "assets": { "<target>": { "file": "...", "sha256": "...", "size": N } } }
//! ```
//!
//! It is trusted only after `manifest.json.minisig` verified; then its
//! version decides whether this is an update, and its sha256 and size
//! decide whether the downloaded archive is the one that was signed.

use std::collections::BTreeMap;

use semver::Version;
use serde::Deserialize;

use crate::UpdateError;

/// The largest `manifest.json` (and its signature) the updater reads.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// The largest archive the updater downloads.
pub const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

/// A release's `manifest.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// The release's version, without a `v`.
    pub version: String,
    /// The protocol version the release speaks, `"<major>.<minor>"`
    /// (`PROTOCOL_VERSION`). Informational: a new daemon is started after
    /// every update either way.
    #[serde(default)]
    pub protocol: Option<String>,
    /// One archive per target.
    pub assets: BTreeMap<String, ManifestAsset>,
}

/// One target's archive in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestAsset {
    /// The asset's file name: `coqo-X.Y.Z-<target>.tar.gz`.
    pub file: String,
    /// Its sha256, in hex.
    pub sha256: String,
    /// Its size in bytes.
    pub size: u64,
}

/// The archive's file name for `version` and `target`.
pub fn archive_name(version: &Version, target: &str) -> String {
    format!("coqo-{version}-{target}.tar.gz")
}

impl Manifest {
    /// Parse a manifest (only once its signature verified).
    pub fn parse(bytes: &[u8]) -> Result<Self, UpdateError> {
        serde_json::from_slice(bytes)
            .map_err(|e| UpdateError::Format(format!("manifest.json is not valid: {e}")))
    }

    /// The manifest's version, which must be the release tag's.
    pub fn version(&self, tag: &Version) -> Result<Version, UpdateError> {
        let version = Version::parse(&self.version).map_err(|e| {
            UpdateError::Format(format!(
                "manifest.json has no valid version ({:?}: {e})",
                self.version
            ))
        })?;
        if &version != tag {
            return Err(UpdateError::Format(format!(
                "manifest.json is for v{version}, but the release is v{tag}"
            )));
        }
        Ok(version)
    }

    /// This target's archive, checked: named as the release format says
    /// (so it is a plain file name), with a well-formed sha256 and a size
    /// the updater accepts.
    pub fn asset(&self, version: &Version, target: &str) -> Result<&ManifestAsset, UpdateError> {
        let asset = self
            .assets
            .get(target)
            .ok_or_else(|| UpdateError::Format(format!("v{version} has no build for {target}")))?;
        let expected = archive_name(version, target);
        if asset.file != expected {
            return Err(UpdateError::Format(format!(
                "manifest.json names the {target} archive {:?}, not {expected:?}",
                asset.file
            )));
        }
        let hex = asset.sha256.len() == 64
            && asset
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(UpdateError::Format(format!(
                "manifest.json's sha256 of {expected} is not 64 lowercase hex digits"
            )));
        }
        if asset.size == 0 || asset.size > MAX_ARCHIVE_BYTES {
            return Err(UpdateError::Format(format!(
                "manifest.json gives {expected} a size of {} bytes",
                asset.size
            )));
        }
        Ok(asset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(json: &str) -> Manifest {
        Manifest::parse(json.as_bytes()).unwrap()
    }

    const SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn the_version_must_be_the_tags_and_the_asset_named_as_the_format_says() {
        let v = Version::parse("0.2.0").unwrap();
        let m = manifest(&format!(
            r#"{{"version":"0.2.0","protocol":"0.9",
                "assets":{{"aarch64-apple-darwin":{{"file":"coqo-0.2.0-aarch64-apple-darwin.tar.gz","sha256":"{SHA}","size":10}},
                           "x86_64-apple-darwin":{{"file":"../evil.tar.gz","sha256":"{SHA}","size":10}},
                           "x86_64-unknown-linux-gnu":{{"file":"coqo-0.2.0-x86_64-unknown-linux-gnu.tar.gz","sha256":"ABC","size":10}}}}}}"#
        ));
        assert_eq!(m.version(&v).unwrap(), v);
        assert_eq!(m.protocol.as_deref(), Some("0.9"));
        assert!(m.version(&Version::parse("0.3.0").unwrap()).is_err());
        assert_eq!(m.asset(&v, "aarch64-apple-darwin").unwrap().size, 10);
        let e = m.asset(&v, "x86_64-apple-darwin").unwrap_err();
        assert!(e.to_string().contains("evil"), "{e}");
        assert!(m.asset(&v, "x86_64-unknown-linux-gnu").is_err());
        let e = m.asset(&v, "riscv64").unwrap_err();
        assert_eq!(e.to_string(), "v0.2.0 has no build for riscv64");
    }

    #[test]
    fn junk_is_refused() {
        assert!(Manifest::parse(b"not json").is_err());
        assert!(Manifest::parse(br#"{"version":"0.2.0"}"#).is_err());
        let m = manifest(r#"{"version":"two","assets":{}}"#);
        assert!(m.version(&Version::parse("0.2.0").unwrap()).is_err());
    }
}
