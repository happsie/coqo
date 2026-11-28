// SPDX-License-Identifier: Apache-2.0

//! `update/*`: the in-app updater (issue #349, ADR 0031).
//!
//! The daemon is the only checker, so several windows never each poll
//! GitHub. It checks at start-up and every 6 hours, downloads and verifies
//! a newer release and unpacks it beside the install; the UI shows it and
//! asks to install it.
//!
//! - [`Status`] (`update/status`): where the updater is.
//! - [`Check`] (`update/check`): check now ("Check for updates"); answers
//!   once the check (and a download) is done.
//! - [`Install`] (`update/install`): swap the verified release in and
//!   restart the daemon once no agent turn runs
//!   ([`RestartWhenIdle`](crate::daemon::RestartWhenIdle)). The UI then
//!   relaunches itself on the new binary.

use serde::{Deserialize, Serialize};

use crate::method::Method;

/// `update/status`: where the updater is.
#[derive(Debug)]
pub enum Status {}
impl Method for Status {
    const NAME: &'static str = "update/status";
    type Params = ();
    type Result = UpdateStatus;
}

/// `update/check`: check for an update now, and download and verify it if
/// there is one. Answers when that is done, with the new status.
#[derive(Debug)]
pub enum Check {}
impl Method for Check {
    const NAME: &'static str = "update/check";
    type Params = ();
    type Result = UpdateStatus;
}

/// `update/install`: swap the downloaded release in, and restart the
/// daemon once no agent turn is running. Fails with
/// [`UPDATE_FAILED`](super::codes::UPDATE_FAILED) when no release is ready
/// or the swap fails.
#[derive(Debug)]
pub enum Install {}
impl Method for Install {
    const NAME: &'static str = "update/install";
    type Params = InstallParams;
    type Result = UpdateStatus;
}

/// Params of `update/install`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct InstallParams {
    /// The install folder the UI runs from (`…/Coqo.app`, or the bin
    /// folder). The daemon refuses if it runs from another one, so a UI
    /// never swaps an install it does not relaunch from.
    pub install: String,
}

/// Result of `update/status`, `update/check` and `update/install`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    /// The daemon's version.
    pub current: String,
    /// Where the updater is.
    pub state: UpdateState,
    /// Set while the daemon waits to restart (after an install, or
    /// `daemon/restartWhenIdle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartPending>,
    /// The last update that was rolled back, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<RolledBack>,
}

/// Where the updater is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateState {
    /// Not checking, and why.
    #[serde(rename_all = "camelCase")]
    Off {
        /// Why.
        reason: OffReason,
        /// The same, for people.
        message: String,
    },
    /// Up to date (or not checked yet).
    #[serde(rename_all = "camelCase")]
    UpToDate {
        /// When the last check answered, in ms since the Unix epoch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checked_at_ms: Option<u64>,
    },
    /// Asking GitHub.
    Checking,
    /// Downloading and verifying a release.
    #[serde(rename_all = "camelCase")]
    Downloading {
        /// Its version.
        version: String,
    },
    /// A verified release is unpacked beside the install, ready to swap in.
    #[serde(rename_all = "camelCase")]
    Ready {
        /// The release.
        release: ReleaseInfo,
    },
    /// Swapped in; the daemon restarts once agents are idle.
    #[serde(rename_all = "camelCase")]
    Installed {
        /// The version installed.
        version: String,
    },
    /// The last check, download or verification failed.
    #[serde(rename_all = "camelCase")]
    Failed {
        /// Why, for people.
        message: String,
        /// The release it was about, if one was found.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// When, in ms since the Unix epoch.
        checked_at_ms: u64,
    },
}

/// Why the updater is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum OffReason {
    /// `updates.check = false` in `settings.toml`.
    Setting,
    /// Coqo does not run from an install location.
    DevBuild,
    /// The build has no release key, so it cannot verify updates.
    NoReleaseKey,
    /// There are no releases for this platform.
    Unsupported,
}

/// A release, as the updater offers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    /// Its version, `X.Y.Z`.
    pub version: String,
    /// The release notes (Markdown).
    pub notes: String,
    /// Its page on GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When it was published (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

/// The daemon is waiting to restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RestartPending {
    /// Agent turns still running; it restarts when this reaches 0.
    pub running: u32,
}

/// An update that was rolled back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RolledBack {
    /// The version that did not start.
    pub version: String,
    /// The version restored.
    pub restored: String,
    /// Why: what its daemon wrote before it exited.
    pub reason: String,
}
