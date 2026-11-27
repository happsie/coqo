// SPDX-License-Identifier: Apache-2.0

//! Where the daemon keeps its data (PLAN §8).
//!
//! ```text
//! <root>/                      ~/Library/Application Support/Coqo, $XDG_DATA_HOME/coqo
//!   coqo.db                    global DB: workspaces, sessions, transcripts, event log, alerts
//!   workspaces/<id>/
//!     workspace.db             per-workspace DB: annotations, local walkthroughs, …
//!     traces.db                (later) trace explorer data
//! ```

use std::path::{Path, PathBuf};

use crate::error::{Result, StorageError};

/// File name of the global database inside the data root.
pub const GLOBAL_DB_FILE: &str = "coqo.db";
/// Directory under the data root that holds one directory per workspace.
pub const WORKSPACES_DIR: &str = "workspaces";
/// File name of a workspace's database inside its directory.
pub const WORKSPACE_DB_FILE: &str = "workspace.db";

/// The daemon data root and the paths derived from it.
///
/// Use [`DataDirs::platform`] in the daemon and [`DataDirs::at`] in tests (or
/// when the user overrides the location).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    root: PathBuf,
}

impl DataDirs {
    /// The platform data directory from PLAN §8:
    /// `~/Library/Application Support/Coqo` on macOS, `$XDG_DATA_HOME/coqo`
    /// (default `~/.local/share/coqo`) on Linux, `%APPDATA%\Coqo` on Windows.
    pub fn platform() -> Result<Self> {
        let base = dirs::data_dir().ok_or(StorageError::NoDataDir)?;
        let name = if cfg!(target_os = "linux") {
            "coqo"
        } else {
            "Coqo"
        };
        Ok(Self::at(base.join(name)))
    }

    /// Use `root` as the data root. Nothing is created until a database is opened.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The data root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the global database, `<root>/coqo.db`.
    pub fn global_db(&self) -> PathBuf {
        self.root.join(GLOBAL_DB_FILE)
    }

    /// Directory of one workspace, `<root>/workspaces/<id>`.
    ///
    /// `id` must be a single, non-empty path component (no `/`, `\`, `.`, `..`).
    pub fn workspace_dir(&self, id: &str) -> Result<PathBuf> {
        validate_workspace_id(id)?;
        Ok(self.root.join(WORKSPACES_DIR).join(id))
    }

    /// Path of a workspace's database, `<root>/workspaces/<id>/workspace.db`.
    pub fn workspace_db(&self, id: &str) -> Result<PathBuf> {
        Ok(self.workspace_dir(id)?.join(WORKSPACE_DB_FILE))
    }
}

fn validate_workspace_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains(['/', '\\', '\0'])
        && !id.contains(':');
    if ok {
        Ok(())
    } else {
        Err(StorageError::InvalidWorkspaceId(id.to_owned()))
    }
}

/// Create `dir` and its parents if needed.
pub(crate) fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| StorageError::Io {
        path: dir.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_under_root() {
        let dirs = DataDirs::at("/data");
        assert_eq!(dirs.root(), Path::new("/data"));
        assert_eq!(dirs.global_db(), Path::new("/data/coqo.db"));
        assert_eq!(
            dirs.workspace_db("abc123").unwrap(),
            Path::new("/data/workspaces/abc123/workspace.db")
        );
    }

    #[test]
    fn rejects_path_like_workspace_ids() {
        let dirs = DataDirs::at("/data");
        for bad in ["", ".", "..", "a/b", "a\\b", "../x", "c:", "a\0b"] {
            assert!(
                matches!(
                    dirs.workspace_dir(bad),
                    Err(StorageError::InvalidWorkspaceId(_))
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn platform_dir_ends_with_app_name() {
        // Not every CI sandbox has a home directory; only check when it does.
        if let Ok(dirs) = DataDirs::platform() {
            let name = dirs.root().file_name().unwrap().to_string_lossy();
            assert!(name.eq_ignore_ascii_case("coqo"), "{name}");
        }
    }
}
