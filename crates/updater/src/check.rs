// SPDX-License-Identifier: Apache-2.0

//! The update check and the download: [`Updater`].
//!
//! [`Updater::check`] asks GitHub for the release to offer (polling with
//! `If-None-Match`). [`Updater::prepare`] downloads its `manifest.json`
//! and signature, verifies the signature against the release key, takes
//! the version and this target's archive from the now trusted manifest,
//! downloads the archive and its signature, checks the archive's sha256
//! and size against the manifest and its signature against the key, and
//! unpacks it beside the install. Anything that fails is refused with the
//! reason, and nothing is swapped in. [`Updater::install`] swaps it in.

use std::sync::Mutex;

use semver::Version;

use crate::http::Http;
use crate::install::{self, Install};
use crate::manifest::{MAX_MANIFEST_BYTES, Manifest, archive_name};
use crate::records::{Pending, UpdateDir};
use crate::release::{self, Channel, GithubRelease, MAX_RELEASE_BYTES};
use crate::verify;
use crate::{REPOSITORY, TARGET, TrustedKey, UpdateError};

/// The largest `.minisig` the updater reads.
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
/// Recent releases read on [`Channel::Pre`].
const PRE_RELEASES: usize = 20;

/// What the updater works with.
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// The running version.
    pub current: Version,
    /// The data directory: downloads go to `<data>/updates/<version>/`.
    pub data_dir: std::path::PathBuf,
    /// Where Coqo is installed.
    pub install: Install,
}

/// A release worth offering, before it is downloaded.
#[derive(Debug, Clone)]
pub struct Offer {
    /// Its version (the tag's; the signed manifest must agree).
    pub version: Version,
    /// GitHub's description of it.
    pub release: GithubRelease,
}

/// A downloaded, verified release unpacked beside the install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// Its version, from the signed manifest.
    pub version: Version,
    /// The release notes.
    pub notes: String,
    /// The release's page.
    pub url: Option<String>,
    /// When it was published.
    pub published_at: Option<String>,
}

#[derive(Debug)]
struct Cache {
    channel: Channel,
    etag: String,
    releases: Vec<GithubRelease>,
}

/// Checks for, downloads, verifies and installs updates. See the module
/// docs.
#[derive(Debug)]
pub struct Updater {
    http: Http,
    key: TrustedKey,
    current: Version,
    target: &'static str,
    install: Install,
    dir: UpdateDir,
    /// The repository's API URL.
    api: String,
    /// What every asset URL must start with.
    downloads: String,
    cache: Mutex<Option<Cache>>,
}

impl Updater {
    /// An updater for the installed Coqo, with the release key compiled in
    /// and GitHub as the source. Refused for a development build, a build
    /// without a release key, and a target with no releases.
    pub fn new(config: UpdaterConfig) -> Result<Self, UpdateError> {
        if config.install.is_dev() {
            return Err(UpdateError::DevBuild);
        }
        let key = TrustedKey::release()?;
        let target = TARGET.ok_or_else(|| {
            UpdateError::Format("there are no Coqo releases for this platform".into())
        })?;
        let http = Http::new(&config.current.to_string())?;
        Ok(Self::with(
            config,
            http,
            key,
            target,
            format!("https://api.github.com/repos/{REPOSITORY}"),
            format!("https://github.com/{REPOSITORY}/releases/download/"),
        ))
    }

    /// An updater against a test release server, with a test key.
    #[cfg(test)]
    pub(crate) fn for_tests(
        config: UpdaterConfig,
        key: TrustedKey,
        target: &'static str,
        server: &str,
    ) -> Self {
        let http = Http::for_tests(&config.current.to_string());
        Self::with(
            config,
            http,
            key,
            target,
            format!("{server}/repos/{REPOSITORY}"),
            format!("{server}/{REPOSITORY}/releases/download/"),
        )
    }

    fn with(
        config: UpdaterConfig,
        http: Http,
        key: TrustedKey,
        target: &'static str,
        api: String,
        downloads: String,
    ) -> Self {
        Self {
            http,
            key,
            target,
            current: config.current,
            dir: UpdateDir::new(&config.data_dir),
            install: config.install,
            api,
            downloads,
            cache: Mutex::new(None),
        }
    }

    /// The running version.
    pub fn current(&self) -> &Version {
        &self.current
    }

    /// The install it updates.
    pub fn install_location(&self) -> &Install {
        &self.install
    }

    /// `<data>/updates`.
    pub fn dir(&self) -> &UpdateDir {
        &self.dir
    }

    /// The release to offer on `channel`, if there is a newer one. Polls
    /// with the last `ETag`, so an unchanged answer costs a `304`.
    pub async fn check(&self, channel: Channel) -> Result<Option<Offer>, UpdateError> {
        let url = match channel {
            Channel::Stable => format!("{}/releases/latest", self.api),
            Channel::Pre => format!("{}/releases?per_page={PRE_RELEASES}", self.api),
        };
        let etag = self
            .lock_cache()
            .as_ref()
            .filter(|c| c.channel == channel)
            .map(|c| c.etag.clone());
        let fetched = match self
            .http
            .get(&url, etag.as_deref(), MAX_RELEASE_BYTES, "GitHub Releases")
            .await
        {
            Ok(fetched) => fetched,
            // No release yet (`releases/latest` is a 404 then).
            Err(UpdateError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let releases = if fetched.status == http::StatusCode::NOT_MODIFIED {
            match self.lock_cache().as_ref().filter(|c| c.channel == channel) {
                Some(cache) => cache.releases.clone(),
                None => {
                    return Err(UpdateError::Format(
                        "GitHub answered 304 to a first request".into(),
                    ));
                }
            }
        } else {
            let releases = match channel {
                Channel::Stable => {
                    serde_json::from_slice::<GithubRelease>(&fetched.body).map(|r| vec![r])
                }
                Channel::Pre => serde_json::from_slice::<Vec<GithubRelease>>(&fetched.body),
            }
            .map_err(|e| UpdateError::Format(format!("GitHub's release JSON is not valid: {e}")))?;
            *self.lock_cache() = fetched.etag.map(|etag| Cache {
                channel,
                etag,
                releases: releases.clone(),
            });
            releases
        };
        Ok(release::pick(releases, &self.current, channel)
            .map(|(version, release)| Offer { version, release }))
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, Option<Cache>> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The download URL of `name` in `offer`, checked to be one of this
    /// repository's release downloads.
    fn asset_url<'a>(&self, offer: &'a Offer, name: &str) -> Result<&'a str, UpdateError> {
        let url = offer.release.asset_url(name).ok_or_else(|| {
            UpdateError::Format(format!("release v{} has no {name}", offer.version))
        })?;
        if !url.starts_with(&self.downloads) {
            return Err(UpdateError::Format(format!(
                "refusing to download {name} from {url}: not a {REPOSITORY} release download"
            )));
        }
        self.http.check_url(url)?;
        Ok(url)
    }

    async fn small(&self, url: &str, limit: usize, what: &str) -> Result<Vec<u8>, UpdateError> {
        Ok(self.http.get(url, None, limit, what).await?.body)
    }

    /// Download, verify and unpack `offer` beside the install. See the
    /// module docs for the checks.
    pub async fn prepare(&self, offer: &Offer) -> Result<Prepared, UpdateError> {
        if !release::is_update(&offer.version, &self.current, Channel::Pre) {
            return Err(UpdateError::NotNewer {
                offered: offer.version.to_string(),
                current: self.current.to_string(),
            });
        }
        // The manifest, trusted once its signature verifies.
        let manifest_url = self.asset_url(offer, "manifest.json")?;
        let manifest_sig_url = self.asset_url(offer, "manifest.json.minisig")?;
        let manifest_bytes = self
            .small(manifest_url, MAX_MANIFEST_BYTES, "manifest.json")
            .await?;
        let manifest_sig = self
            .small(
                manifest_sig_url,
                MAX_SIGNATURE_BYTES,
                "manifest.json.minisig",
            )
            .await?;
        let manifest_sig =
            String::from_utf8(manifest_sig).map_err(|_| UpdateError::BadSignature {
                what: "manifest.json".into(),
            })?;
        verify::signature(&self.key, &manifest_bytes, &manifest_sig, "manifest.json")?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        let version = manifest.version(&offer.version)?;
        if !release::is_update(&version, &self.current, Channel::Pre) {
            return Err(UpdateError::NotNewer {
                offered: version.to_string(),
                current: self.current.to_string(),
            });
        }
        let asset = manifest.asset(&version, self.target)?.clone();

        let dir = self.dir.version_dir(&version);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(UpdateError::io(format!("cannot create {}", dir.display())))?;
        let _ = tokio::fs::write(dir.join("manifest.json"), &manifest_bytes).await;
        let _ = tokio::fs::write(dir.join("manifest.json.minisig"), &manifest_sig).await;

        // The archive: its sha256 and size from the manifest, its own
        // signature from the key.
        let name = archive_name(&version, self.target);
        let archive_url = self.asset_url(offer, &name)?;
        let sig_name = format!("{name}.minisig");
        let sig_url = self.asset_url(offer, &sig_name)?;
        let archive = dir.join(&name);
        let have = {
            let archive = archive.clone();
            tokio::task::spawn_blocking(move || verify::sha256_file(&archive).ok())
                .await
                .ok()
                .flatten()
        };
        if have.as_ref() != Some(&(asset.sha256.clone(), asset.size)) {
            let (sha, size) = self
                .http
                .download(archive_url, &archive, asset.size, &name)
                .await?;
            let mismatch = if size != asset.size {
                Some(format!("{size} bytes, the manifest says {}", asset.size))
            } else if sha != asset.sha256 {
                Some(format!("sha256 {sha}, the manifest says {}", asset.sha256))
            } else {
                None
            };
            if let Some(detail) = mismatch {
                let _ = tokio::fs::remove_file(&archive).await;
                return Err(UpdateError::Mismatch { what: name, detail });
            }
        }
        let signature = self.small(sig_url, MAX_SIGNATURE_BYTES, &sig_name).await?;
        let signature = String::from_utf8(signature)
            .map_err(|_| UpdateError::BadSignature { what: name.clone() })?;
        let key_check = {
            let archive = archive.clone();
            let name = name.clone();
            let key = TrustedKey(self.key.0.clone());
            tokio::task::spawn_blocking(move || {
                let data = std::fs::read(&archive).map_err(UpdateError::io(format!(
                    "cannot read {}",
                    archive.display()
                )))?;
                verify::signature(&key, &data, &signature, &name)
            })
        };
        if let Err(e) = key_check
            .await
            .map_err(|e| UpdateError::Format(format!("verification stopped: {e}")))?
        {
            let _ = tokio::fs::remove_file(&archive).await;
            return Err(e);
        }

        // Unpack beside the install.
        let install = self.install.clone();
        tokio::task::spawn_blocking(move || install::unpack(&install, &archive))
            .await
            .map_err(|e| UpdateError::Format(format!("unpacking stopped: {e}")))??;
        self.dir.prune(&version);
        tracing::info!(%version, target = self.target, "update downloaded and verified");
        Ok(Prepared {
            version,
            notes: offer.release.body.clone().unwrap_or_default(),
            url: offer.release.html_url.clone(),
            published_at: offer.release.published_at.clone(),
        })
    }

    /// Swap `prepared` in and record it as pending, so the UI can roll
    /// back once if its daemon does not start.
    pub fn install(&self, prepared: &Prepared) -> Result<(), UpdateError> {
        let root = self.install.root().ok_or(UpdateError::DevBuild)?.to_owned();
        install::swap(&self.install)?;
        self.dir
            .set_pending(&Pending {
                from: self.current.to_string(),
                to: prepared.version.to_string(),
                install: root,
            })
            .map_err(UpdateError::io("cannot record the update"))?;
        self.dir.clear_rolled_back();
        Ok(())
    }
}
