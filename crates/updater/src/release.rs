// SPDX-License-Identifier: Apache-2.0

//! GitHub's release JSON, versions and update channels.
//!
//! Only what the updater needs is read: the tag, the notes, whether it is a
//! draft or a pre-release, and the download URLs of its assets. None of it
//! is trusted for integrity: the version that counts is the signed
//! manifest's, and the files are checked against it.

use semver::Version;
use serde::Deserialize;

/// The largest release JSON the updater reads (GitHub's are a few KiB;
/// the notes are the bulk).
pub const MAX_RELEASE_BYTES: usize = 2 * 1024 * 1024;

/// Which releases are offered (`updates.channel`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Channel {
    /// Releases only (`"stable"`, the default): GitHub's latest release,
    /// never a pre-release.
    #[default]
    Stable,
    /// Pre-releases too (`"pre"`): the newest of the recent releases.
    Pre,
}

/// One GitHub release, as `releases/latest` and `releases` return it.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubRelease {
    /// The tag, `vX.Y.Z`.
    pub tag_name: String,
    /// Its title.
    #[serde(default)]
    pub name: Option<String>,
    /// The release notes (Markdown).
    #[serde(default)]
    pub body: Option<String>,
    /// Drafts are never offered.
    #[serde(default)]
    pub draft: bool,
    /// Pre-releases are offered only on [`Channel::Pre`].
    #[serde(default)]
    pub prerelease: bool,
    /// Its page on GitHub.
    #[serde(default)]
    pub html_url: Option<String>,
    /// When it was published (RFC 3339).
    #[serde(default)]
    pub published_at: Option<String>,
    /// Its files.
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

/// A release file.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubAsset {
    /// The file name.
    pub name: String,
    /// Where to download it.
    pub browser_download_url: String,
    /// Its size in bytes.
    #[serde(default)]
    pub size: u64,
}

impl GithubRelease {
    /// The version in the tag (`v` + semver), if it is one.
    pub fn version(&self) -> Option<Version> {
        Version::parse(self.tag_name.strip_prefix('v')?).ok()
    }

    /// The download URL of the asset named `name`.
    pub fn asset_url(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.as_str())
    }
}

/// Whether `offered` is an update from `current` on `channel`: strictly
/// newer, and not a pre-release unless the channel takes them. Never a
/// downgrade, never the same version again.
pub fn is_update(offered: &Version, current: &Version, channel: Channel) -> bool {
    if !offered.pre.is_empty() && channel == Channel::Stable {
        return false;
    }
    offered > current
}

/// The release to offer from `releases` on `channel`: the newest by
/// version that is not a draft and whose tag is a version, if it is an
/// update from `current`.
pub fn pick(
    releases: Vec<GithubRelease>,
    current: &Version,
    channel: Channel,
) -> Option<(Version, GithubRelease)> {
    releases
        .into_iter()
        .filter(|r| !r.draft && (channel == Channel::Pre || !r.prerelease))
        .filter_map(|r| Some((r.version()?, r)))
        .filter(|(v, _)| is_update(v, current, channel))
        .max_by(|(a, _), (b, _)| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn release(tag: &str, prerelease: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            name: None,
            body: None,
            draft: false,
            prerelease,
            html_url: None,
            published_at: None,
            assets: Vec::new(),
        }
    }

    #[test]
    fn only_newer_versions_and_pre_releases_only_on_pre() {
        let current = v("0.2.0");
        assert!(is_update(&v("0.2.1"), &current, Channel::Stable));
        assert!(!is_update(&v("0.2.0"), &current, Channel::Stable));
        assert!(
            !is_update(&v("0.1.9"), &current, Channel::Pre),
            "no downgrades"
        );
        assert!(!is_update(&v("0.3.0-rc.1"), &current, Channel::Stable));
        assert!(is_update(&v("0.3.0-rc.1"), &current, Channel::Pre));
        assert!(!is_update(&v("0.2.0-rc.1"), &current, Channel::Pre));
        // From a pre-release, its release is newer.
        assert!(is_update(&v("0.3.0"), &v("0.3.0-rc.2"), Channel::Stable));
    }

    #[test]
    fn picks_the_newest_offer() {
        let releases = || {
            vec![
                release("v0.2.1", false),
                release("v0.3.0-rc.1", true),
                release("not-a-version", false),
                GithubRelease {
                    draft: true,
                    ..release("v9.0.0", false)
                },
                release("v0.1.0", false),
            ]
        };
        let current = v("0.2.0");
        let (version, _) = pick(releases(), &current, Channel::Stable).unwrap();
        assert_eq!(version, v("0.2.1"));
        let (version, _) = pick(releases(), &current, Channel::Pre).unwrap();
        assert_eq!(version, v("0.3.0-rc.1"));
        assert!(pick(releases(), &v("0.3.0"), Channel::Pre).is_none());
        // A pre-release that GitHub calls a release is still refused on
        // stable: the version decides.
        assert!(
            pick(
                vec![release("v0.4.0-rc.1", false)],
                &current,
                Channel::Stable
            )
            .is_none()
        );
    }
}
