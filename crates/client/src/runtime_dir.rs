// SPDX-License-Identifier: Apache-2.0

//! The runtime directory, verified before use (PLAN §9).
//!
//! The directory holds the socket, the lock and the session token, so it
//! must belong to the current user and be closed to everyone else. Both
//! sides check that: the daemon before it locks, writes its token and binds
//! (it would otherwise hand its token to whoever can read the directory),
//! and clients before they connect and send the token (they would otherwise
//! talk to whoever planted a socket there).
//!
//! The checks run on an open file descriptor, not on the path:
//! [`RuntimeDir`] opens the directory with `O_NOFOLLOW | O_DIRECTORY` (a
//! symlink in the last component is refused) and `fstat`s that descriptor,
//! so the directory checked is the directory used. Files inside are opened
//! relative to it (`openat`, again with `O_NOFOLLOW`). Only the socket goes
//! by path, because `bind` and `connect` take one; that is safe because
//! nobody else can create or rename entries in a directory that is ours
//! and `0700`, and [`RuntimeDir::check_still_at_path`] confirms the path
//! still leads to the directory we checked.

use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, Read as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use coqo_protocol::SessionToken;
use nix::errno::Errno;
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;

use crate::paths::{RUNTIME_DIR_ENV, TOKEN_FILE};

/// Mode bits that must be clear: any access for group or others.
const GROUP_OTHER: u32 = 0o077;
/// Upper bound on the token file; real tokens are 64 bytes.
const MAX_TOKEN_LEN: u64 = 1024;

/// Why the runtime directory (or a file in it) cannot be trusted or used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeDirError {
    /// The path is a symbolic link. Following it would let whoever created
    /// it choose where the socket and token go.
    #[error(
        "the Coqo runtime directory {} is a symlink; refusing to use it (remove it, or set {RUNTIME_DIR_ENV})",
        path.display()
    )]
    Symlink {
        /// The path.
        path: PathBuf,
    },
    /// The path exists but is not a directory (or the file is not a
    /// regular file).
    #[error("{} is not a {expected}", path.display())]
    WrongType {
        /// The path.
        path: PathBuf,
        /// `directory` or `regular file`.
        expected: &'static str,
    },
    /// Another user owns it (for example, someone pre-created
    /// `/tmp/coqo-<uid>`).
    #[error(
        "{} belongs to uid {owner}, not to you (uid {uid}); refusing to use it (remove it, or set {RUNTIME_DIR_ENV} or XDG_RUNTIME_DIR)",
        path.display()
    )]
    WrongOwner {
        /// The path.
        path: PathBuf,
        /// Its owner.
        owner: u32,
        /// The current (effective) uid.
        uid: u32,
    },
    /// Group or others have access.
    #[error(
        "{} is accessible to other users (mode {mode:03o}); refusing to use it (run `chmod 700` on it, or remove it)",
        path.display()
    )]
    TooOpen {
        /// The path.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The directory was replaced after it was checked.
    #[error("{} changed while coqo was using it", path.display())]
    Replaced {
        /// The path.
        path: PathBuf,
    },
    /// The token file is empty or too large.
    #[error("{} does not hold a session token", path.display())]
    BadToken {
        /// The path.
        path: PathBuf,
    },
    /// An OS error.
    #[error("cannot use {}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
}

impl RuntimeDirError {
    fn io(path: &Path, source: impl Into<io::Error>) -> Self {
        RuntimeDirError::Io {
            path: path.to_owned(),
            source: source.into(),
        }
    }
}

/// The current effective uid, which owns what this process creates.
pub fn current_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// Check that `meta` is a `expected`-type object owned by `uid` with no
/// group or other access.
fn check_owner_and_mode(
    path: &Path,
    meta: &Metadata,
    uid: u32,
    dir: bool,
) -> Result<(), RuntimeDirError> {
    let right_type = if dir { meta.is_dir() } else { meta.is_file() };
    if !right_type {
        return Err(RuntimeDirError::WrongType {
            path: path.to_owned(),
            expected: if dir { "directory" } else { "regular file" },
        });
    }
    check_ids(path, meta.uid(), meta.mode(), uid)
}

/// The owner and permission check, separate so tests can feed it the
/// owners they cannot create without root.
fn check_ids(path: &Path, owner: u32, mode: u32, uid: u32) -> Result<(), RuntimeDirError> {
    if owner != uid {
        return Err(RuntimeDirError::WrongOwner {
            path: path.to_owned(),
            owner,
            uid,
        });
    }
    if mode & GROUP_OTHER != 0 {
        return Err(RuntimeDirError::TooOpen {
            path: path.to_owned(),
            mode: mode & 0o7777,
        });
    }
    Ok(())
}

/// An open runtime directory that belongs to the current user and is closed
/// to everyone else.
#[derive(Debug)]
pub struct RuntimeDir {
    path: PathBuf,
    dir: File,
}

impl RuntimeDir {
    /// Create the directory (`0700`) if it does not exist, then open and
    /// verify it. Missing parents are created with default permissions;
    /// only the last component has to be private. Used by the daemon.
    pub fn create(path: &Path) -> Result<Self, RuntimeDirError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            DirBuilder::new()
                .recursive(true)
                .create(parent)
                .map_err(|e| RuntimeDirError::io(parent, e))?;
        }
        // Not recursive: an existing directory is checked, never adopted
        // with a silent chmod.
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(RuntimeDirError::io(path, e)),
        }
        Self::open_existing(path)
    }

    /// Open and verify an existing directory; `Ok(None)` if there is none.
    /// Used by clients, which never create it.
    pub fn open(path: &Path) -> Result<Option<Self>, RuntimeDirError> {
        match Self::open_existing(path) {
            Err(RuntimeDirError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(None)
            }
            other => other.map(Some),
        }
    }

    fn open_existing(path: &Path) -> Result<Self, RuntimeDirError> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY).bits())
            .open(path)
            .map_err(|e| {
                // ELOOP or ENOTDIR: say which, from the link itself.
                match std::fs::symlink_metadata(path) {
                    Ok(m) if m.file_type().is_symlink() => RuntimeDirError::Symlink {
                        path: path.to_owned(),
                    },
                    Ok(m) if !m.is_dir() => RuntimeDirError::WrongType {
                        path: path.to_owned(),
                        expected: "directory",
                    },
                    _ => RuntimeDirError::io(path, e),
                }
            })?;
        let meta = dir.metadata().map_err(|e| RuntimeDirError::io(path, e))?;
        check_owner_and_mode(path, &meta, current_uid(), true)?;
        Ok(Self {
            path: path.to_owned(),
            dir,
        })
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Confirm the path still leads to the directory that was checked (not
    /// a replacement), before trusting a path-based operation in it.
    pub fn check_still_at_path(&self) -> Result<(), RuntimeDirError> {
        let opened = self
            .dir
            .metadata()
            .map_err(|e| RuntimeDirError::io(&self.path, e))?;
        let now = std::fs::symlink_metadata(&self.path)
            .map_err(|e| RuntimeDirError::io(&self.path, e))?;
        if now.dev() == opened.dev() && now.ino() == opened.ino() {
            Ok(())
        } else {
            Err(RuntimeDirError::Replaced {
                path: self.path.clone(),
            })
        }
    }

    /// Open `name` inside the directory (`openat`, never following a
    /// symlink). `mode` applies when `flags` include `O_CREAT`.
    pub fn open_file(&self, name: &str, flags: OFlag, mode: u32) -> Result<File, RuntimeDirError> {
        let fd = openat(
            self.dir.as_fd(),
            name,
            flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(mode as nix::libc::mode_t),
        )
        .map_err(|e| self.file_error(name, e))?;
        Ok(File::from(fd))
    }

    fn file_error(&self, name: &str, errno: Errno) -> RuntimeDirError {
        let path = self.path.join(name);
        if errno == Errno::ELOOP {
            return RuntimeDirError::WrongType {
                path,
                expected: "regular file",
            };
        }
        RuntimeDirError::io(&path, errno)
    }

    /// Read the daemon's session token (`coqod.token`). `Ok(None)` if there
    /// is none (an older daemon that does not use tokens). The file must be
    /// a regular file of ours with no group or other access.
    pub fn read_token(&self) -> Result<Option<SessionToken>, RuntimeDirError> {
        let path = self.path.join(TOKEN_FILE);
        let mut file = match self.open_file(TOKEN_FILE, OFlag::O_RDONLY, 0) {
            Ok(file) => file,
            Err(RuntimeDirError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata().map_err(|e| RuntimeDirError::io(&path, e))?;
        check_owner_and_mode(&path, &meta, current_uid(), false)?;
        let mut token = String::new();
        (&mut file)
            .take(MAX_TOKEN_LEN + 1)
            .read_to_string(&mut token)
            .map_err(|e| RuntimeDirError::io(&path, e))?;
        let token = token.trim();
        if token.is_empty() || token.len() as u64 > MAX_TOKEN_LEN {
            return Err(RuntimeDirError::BadToken { path });
        }
        Ok(Some(SessionToken::new(token)))
    }
}

impl AsFd for RuntimeDir {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.dir.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn private_tmp() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        tmp
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn create_makes_a_private_directory_and_parents() {
        let tmp = private_tmp();
        let path = tmp.path().join("a").join("run");
        let dir = RuntimeDir::create(&path).unwrap();
        assert_eq!(dir.path(), path);
        assert_eq!(mode(&path), 0o700);
        dir.check_still_at_path().unwrap();
        // Idempotent.
        RuntimeDir::create(&path).unwrap();
    }

    #[test]
    fn open_reports_a_missing_directory_as_none() {
        let tmp = private_tmp();
        assert!(
            RuntimeDir::open(&tmp.path().join("nope"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_too_open_directory_is_refused_not_fixed() {
        let tmp = private_tmp();
        let path = tmp.path().join("run");
        fs::create_dir(&path).unwrap();
        for bits in [0o755, 0o750, 0o701, 0o707] {
            fs::set_permissions(&path, fs::Permissions::from_mode(bits)).unwrap();
            let e = RuntimeDir::create(&path).unwrap_err();
            assert!(
                matches!(e, RuntimeDirError::TooOpen { mode, .. } if mode == bits),
                "{e}"
            );
            assert!(e.to_string().contains("chmod 700"), "{e}");
            assert!(matches!(
                RuntimeDir::open(&path),
                Err(RuntimeDirError::TooOpen { .. })
            ));
            assert_eq!(mode(&path), bits, "left alone");
        }
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let tmp = private_tmp();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = tmp.path().join("run");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = RuntimeDir::create(&link).unwrap_err();
        assert!(matches!(e, RuntimeDirError::Symlink { .. }), "{e}");
        assert!(matches!(
            RuntimeDir::open(&link),
            Err(RuntimeDirError::Symlink { .. })
        ));
        // A dangling link is refused too, not created through.
        fs::remove_dir(&real).unwrap();
        let e = RuntimeDir::create(&link).unwrap_err();
        assert!(matches!(e, RuntimeDirError::Symlink { .. }), "{e}");
        assert!(!real.exists());
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let tmp = private_tmp();
        let path = tmp.path().join("run");
        fs::write(&path, "").unwrap();
        let e = RuntimeDir::create(&path).unwrap_err();
        assert!(matches!(e, RuntimeDirError::WrongType { .. }), "{e}");
    }

    #[test]
    fn another_users_directory_is_refused() {
        // Creating a directory owned by someone else needs root, so feed
        // the check the ids directly.
        let path = Path::new("/tmp/coqo-1000");
        let e = check_ids(path, 0, 0o40700, 1000).unwrap_err();
        assert!(
            matches!(
                e,
                RuntimeDirError::WrongOwner {
                    owner: 0,
                    uid: 1000,
                    ..
                }
            ),
            "{e}"
        );
        assert!(e.to_string().contains("uid 0"), "{e}");
        check_ids(path, 1000, 0o40700, 1000).unwrap();
        assert!(matches!(
            check_ids(path, 1000, 0o40770, 1000),
            Err(RuntimeDirError::TooOpen { mode: 0o770, .. })
        ));
    }

    #[test]
    fn a_replaced_directory_is_noticed() {
        let tmp = private_tmp();
        let path = tmp.path().join("run");
        let dir = RuntimeDir::create(&path).unwrap();
        fs::rename(&path, tmp.path().join("moved")).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            dir.check_still_at_path(),
            Err(RuntimeDirError::Replaced { .. })
        ));
    }

    #[test]
    fn token_file_checks() {
        let tmp = private_tmp();
        let path = tmp.path().join("run");
        let dir = RuntimeDir::create(&path).unwrap();
        let token = path.join(TOKEN_FILE);
        assert!(dir.read_token().unwrap().is_none());

        fs::write(&token, "abc123\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(dir.read_token().unwrap(), Some(SessionToken::new("abc123")));

        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            dir.read_token(),
            Err(RuntimeDirError::TooOpen { .. })
        ));

        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&token, "\n").unwrap();
        assert!(matches!(
            dir.read_token(),
            Err(RuntimeDirError::BadToken { .. })
        ));

        fs::remove_file(&token).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::write(&elsewhere, "planted").unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &token).unwrap();
        assert!(matches!(
            dir.read_token(),
            Err(RuntimeDirError::WrongType { .. })
        ));
    }
}
