// SPDX-License-Identifier: Apache-2.0

//! `coqo-updater`: Coqo's in-app updater (issue #349, ADR 0031).
//!
//! The daemon is the only checker: it asks GitHub Releases for the newest
//! release ([`Updater::check`]), downloads its signed `manifest.json` and
//! this target's archive into `<data>/updates/<version>/`, verifies both
//! minisign signatures against the public key compiled into this build
//! ([`TrustedKey::release`]) and the archive's sha256 and size against the
//! signed manifest, and unpacks it next to the install as `Coqo.app.new`
//! (macOS) or `bin.new` (Linux). [`swap`] then renames the install to
//! `.old` and the new one into place, never writing over a running binary,
//! and [`roll_back`] undoes it once if the new daemon does not start.
//!
//! - [`key`]: the release public key, and the placeholder that refuses all.
//! - [`verify`]: minisign signatures and sha256.
//! - [`manifest`]: the signed `manifest.json`.
//! - [`release`]: GitHub's release JSON, versions and channels.
//! - [`http`]: the HTTPS client (only a `User-Agent` and, when polling, an
//!   `If-None-Match`).
//! - [`install`]: where Coqo is installed (or that it is a development
//!   build), unpacking, the swap and the rollback.
//! - [`records`]: the `pending.json` and `rolled-back.json` records the UI
//!   and the daemon share.
//! - [`settings`]: `[updates]` in `settings.toml`.

pub mod check;
pub mod http;
pub mod install;
pub mod key;
pub mod manifest;
pub mod records;
pub mod release;
pub mod settings;
pub mod verify;

#[cfg(test)]
mod tests;

pub use check::{Prepared, Updater, UpdaterConfig};
pub use install::{Install, Os, roll_back, swap};
pub use key::TrustedKey;
pub use release::Channel;
pub use settings::UpdateSettings;

/// Where the releases are: `happsie/coqo` on GitHub.
pub const REPOSITORY: &str = "happsie/coqo";

/// The release target this build was compiled for, as the release assets
/// name it (`coqo-X.Y.Z-<target>.tar.gz`). `None` on targets with no
/// release.
pub const TARGET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu"
)) {
    Some("x86_64-unknown-linux-gnu")
} else if cfg!(all(
    target_os = "linux",
    target_arch = "aarch64",
    target_env = "gnu"
)) {
    Some("aarch64-unknown-linux-gnu")
} else {
    None
};

/// Why an update could not be checked, downloaded, verified or installed.
/// The messages are shown to the user as they are.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UpdateError {
    /// This build has no release key (the placeholder), so nothing can be
    /// verified and nothing is installed.
    #[error("this build has no release key, so it can't verify updates")]
    NoReleaseKey,
    /// The request failed (offline, DNS, TLS, timeout).
    #[error("cannot reach {what}: {message}")]
    Network {
        /// What was being fetched.
        what: String,
        /// Why.
        message: String,
    },
    /// The server answered with an unexpected status.
    #[error("{what}: the server answered {status}")]
    Status {
        /// What was being fetched.
        what: String,
        /// The HTTP status.
        status: u16,
    },
    /// The response or the release is not what the release format says.
    #[error("{0}")]
    Format(String),
    /// A signature did not verify against the release key.
    #[error(
        "the signature of {what} does not verify against Coqo's release key; not installing it"
    )]
    BadSignature {
        /// What was signed.
        what: String,
    },
    /// The archive is not the one the signed manifest describes.
    #[error("{what} does not match the signed manifest ({detail}); not installing it")]
    Mismatch {
        /// The file.
        what: String,
        /// sha256 or size.
        detail: String,
    },
    /// The release is older than (or the same as) the running version.
    #[error("v{offered} is not newer than v{current}; not installing it")]
    NotNewer {
        /// The release's version.
        offered: String,
        /// The running version.
        current: String,
    },
    /// The archive holds something an install must not (a link, a path
    /// outside its folder).
    #[error("the archive is unsafe to unpack: {0}")]
    UnsafeArchive(String),
    /// Coqo does not run from an install location.
    #[error("running a development build: updates install only into an installed Coqo")]
    DevBuild,
    /// A file operation failed.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// The OS error.
        source: std::io::Error,
    },
}

impl UpdateError {
    pub(crate) fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let what = what.into();
        move |source| Self::Io { what, source }
    }
}
