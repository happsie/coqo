// SPDX-License-Identifier: Apache-2.0

//! Where the daemon's socket and lock live (PLAN §4.3).
//!
//! ```text
//! <runtime dir>/               $XDG_RUNTIME_DIR/coqo (Linux), ~/Library/Application Support/Coqo/run (macOS)
//!   coqod.sock                 the Coqo Protocol socket
//!   coqod.lock                 single-instance lock (flock), holds the daemon's pid
//! ```
//!
//! `COQO_RUNTIME_DIR` overrides the directory, for tests and for running
//! more than one daemon side by side.

use std::io;
use std::path::{Path, PathBuf};

/// Overrides the runtime directory (socket and lock).
pub const RUNTIME_DIR_ENV: &str = "COQO_RUNTIME_DIR";
/// Overrides the daemon's data directory (`coqo.db` and friends). Read by
/// `coqod`; listed here so every client agrees on the name.
pub const DATA_DIR_ENV: &str = "COQO_DATA_DIR";
/// Path of the `coqod` binary to spawn. See
/// [`SpawnOptions::locate`](crate::SpawnOptions::locate).
pub const DAEMON_BIN_ENV: &str = "COQO_DAEMON";

/// File name of the daemon socket.
pub const SOCKET_FILE: &str = "coqod.sock";
/// File name of the single-instance lock.
pub const LOCK_FILE: &str = "coqod.lock";

/// No runtime directory could be determined.
#[derive(Debug, thiserror::Error)]
#[error("cannot determine the Coqo runtime directory (no home directory?); set {RUNTIME_DIR_ENV}")]
pub struct NoRuntimeDir;

/// The daemon's runtime directory and the paths in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    dir: PathBuf,
}

impl DaemonPaths {
    /// `$COQO_RUNTIME_DIR` if set (and not empty), otherwise
    /// [`DaemonPaths::platform`].
    pub fn from_env() -> Result<Self, NoRuntimeDir> {
        match std::env::var_os(RUNTIME_DIR_ENV) {
            Some(dir) if !dir.is_empty() => Ok(Self::at(dir)),
            _ => Self::platform(),
        }
    }

    /// The per-user platform directory: `$XDG_RUNTIME_DIR/coqo` on Linux
    /// (falling back to `<tmp>/coqo-<uid>` when `XDG_RUNTIME_DIR` is unset),
    /// `~/Library/Application Support/Coqo/run` on macOS.
    pub fn platform() -> Result<Self, NoRuntimeDir> {
        if cfg!(target_os = "macos") {
            let base = dirs::data_dir().ok_or(NoRuntimeDir)?;
            return Ok(Self::at(base.join("Coqo").join("run")));
        }
        match dirs::runtime_dir() {
            Some(base) => Ok(Self::at(base.join("coqo"))),
            None => {
                let uid = nix::unistd::getuid();
                Ok(Self::at(std::env::temp_dir().join(format!("coqo-{uid}"))))
            }
        }
    }

    /// Use `dir` as the runtime directory.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The runtime directory.
    pub fn runtime_dir(&self) -> &Path {
        &self.dir
    }

    /// The daemon socket, `<dir>/coqod.sock`.
    pub fn socket(&self) -> PathBuf {
        self.dir.join(SOCKET_FILE)
    }

    /// The single-instance lock file, `<dir>/coqod.lock`.
    pub fn lock_file(&self) -> PathBuf {
        self.dir.join(LOCK_FILE)
    }

    /// The pid recorded by the daemon that holds (or last held) the lock.
    ///
    /// Informational only: the daemon may have exited since, and the pid may
    /// have been reused. Ask `daemon/status` for the truth.
    pub fn recorded_pid(&self) -> io::Result<Option<u32>> {
        match std::fs::read_to_string(self.lock_file()) {
            Ok(s) => Ok(s.trim().parse().ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_under_dir() {
        let paths = DaemonPaths::at("/run/user/1000/coqo");
        assert_eq!(paths.runtime_dir(), Path::new("/run/user/1000/coqo"));
        assert_eq!(paths.socket(), Path::new("/run/user/1000/coqo/coqod.sock"));
        assert_eq!(
            paths.lock_file(),
            Path::new("/run/user/1000/coqo/coqod.lock")
        );
    }

    #[test]
    fn platform_dir_is_per_app() {
        let paths = DaemonPaths::platform().expect("a runtime dir");
        let dir = paths.runtime_dir();
        if cfg!(target_os = "macos") {
            assert!(dir.ends_with("Coqo/run"), "{}", dir.display());
        } else {
            let name = dir.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("coqo"), "{}", dir.display());
        }
    }

    #[test]
    fn recorded_pid_reads_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::at(dir.path());
        assert_eq!(paths.recorded_pid().unwrap(), None);
        std::fs::write(paths.lock_file(), "4242\n").unwrap();
        assert_eq!(paths.recorded_pid().unwrap(), Some(4242));
        std::fs::write(paths.lock_file(), "").unwrap();
        assert_eq!(paths.recorded_pid().unwrap(), None);
    }
}
