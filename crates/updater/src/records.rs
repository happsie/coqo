// SPDX-License-Identifier: Apache-2.0

//! `<data>/updates/`: the downloads, and the two small records the UI and
//! the daemon share across a restart.
//!
//! - `pending.json` ([`Pending`]): written by the swap. While it is there
//!   the new version has not yet started its daemon; the UI rolls back
//!   once if that daemon exits during start-up, and clears it when the new
//!   daemon answers.
//! - `rolled-back.json` ([`RolledBack`]): written by the rollback, so the
//!   restored version can say why and does not offer the same update again.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The folder's name in the data directory.
pub const DIR: &str = "updates";
const PENDING: &str = "pending.json";
const ROLLED_BACK: &str = "rolled-back.json";

/// An installed update whose daemon has not started yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The version swapped out (now `<install>.old`).
    pub from: String,
    /// The version swapped in.
    pub to: String,
    /// The install folder.
    pub install: PathBuf,
}

/// An update that was rolled back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolledBack {
    /// The version that did not start.
    pub version: String,
    /// The version restored.
    pub restored: String,
    /// Why: what the new daemon wrote before it exited.
    pub reason: String,
}

/// `<data>/updates`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateDir {
    root: PathBuf,
}

impl UpdateDir {
    /// The folder under the data directory `data`.
    pub fn new(data: &Path) -> Self {
        Self {
            root: data.join(DIR),
        }
    }

    /// The folder itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `version`'s downloads go. A [`Version`] prints as plain
    /// semver, so the name never leaves the folder.
    pub fn version_dir(&self, version: &Version) -> PathBuf {
        self.root.join(version.to_string())
    }

    /// Remove every version's downloads but `keep`'s.
    pub fn prune(&self, keep: &Version) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        let keep = keep.to_string();
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && entry.file_name() != keep.as_str() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        let text = fs::read(self.root.join(name)).ok()?;
        serde_json::from_slice(&text).ok()
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let tmp = self.root.join(format!(".{name}.tmp"));
        let text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            io::Write::write_all(&mut file, &text)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, self.root.join(name))
    }

    fn remove(&self, name: &str) {
        let _ = fs::remove_file(self.root.join(name));
    }

    /// The pending update, if any.
    pub fn pending(&self) -> Option<Pending> {
        self.read(PENDING)
    }

    /// Record an installed update.
    pub fn set_pending(&self, pending: &Pending) -> io::Result<()> {
        self.write(PENDING, pending)
    }

    /// Forget the pending update.
    pub fn clear_pending(&self) {
        self.remove(PENDING);
    }

    /// The last rollback, if any.
    pub fn rolled_back(&self) -> Option<RolledBack> {
        self.read(ROLLED_BACK)
    }

    /// Record a rollback.
    pub fn set_rolled_back(&self, rolled_back: &RolledBack) -> io::Result<()> {
        self.write(ROLLED_BACK, rolled_back)
    }

    /// Forget the last rollback.
    pub fn clear_rolled_back(&self) {
        self.remove(ROLLED_BACK);
    }
}

/// The launcher's rollback (issue #349): the daemon of the version that is
/// `running` exited during start-up, with `reason`. If that version was
/// just swapped in (a pending update to it, for this install), put the
/// previous version back, once, and record why. Returns the record when it
/// rolled back; the caller then relaunches the restored UI.
///
/// "Once": the pending record is removed before anything else, so a
/// second failure (of the restored version, say) never rolls back again.
pub fn roll_back_failed_start(
    dir: &UpdateDir,
    install: &crate::Install,
    running: &str,
    reason: &str,
) -> Result<Option<RolledBack>, crate::UpdateError> {
    let Some(pending) = dir.pending() else {
        return Ok(None);
    };
    if pending.to != running || install.root() != Some(pending.install.as_path()) {
        return Ok(None);
    }
    dir.clear_pending();
    crate::install::roll_back(install)?;
    let record = RolledBack {
        version: pending.to,
        restored: pending.from,
        reason: reason.to_owned(),
    };
    if let Err(e) = dir.set_rolled_back(&record) {
        tracing::warn!(error = %e, "cannot record the rollback");
    }
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_prune_keeps_one_version() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = UpdateDir::new(tmp.path());
        assert_eq!(dir.pending(), None);
        let pending = Pending {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            install: "/x/Coqo.app".into(),
        };
        dir.set_pending(&pending).unwrap();
        assert_eq!(dir.pending(), Some(pending));
        dir.clear_pending();
        assert_eq!(dir.pending(), None);
        let rb = RolledBack {
            version: "0.2.0".into(),
            restored: "0.1.0".into(),
            reason: "boom".into(),
        };
        dir.set_rolled_back(&rb).unwrap();
        assert_eq!(dir.rolled_back(), Some(rb));

        let v1 = Version::parse("0.1.0").unwrap();
        let v2 = Version::parse("0.2.0").unwrap();
        fs::create_dir_all(dir.version_dir(&v1)).unwrap();
        fs::create_dir_all(dir.version_dir(&v2)).unwrap();
        dir.prune(&v2);
        assert!(!dir.version_dir(&v1).exists());
        assert!(dir.version_dir(&v2).exists());
        assert!(dir.rolled_back().is_some(), "records stay");
    }
}
