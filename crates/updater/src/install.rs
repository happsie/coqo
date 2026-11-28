// SPDX-License-Identifier: Apache-2.0

//! Where Coqo is installed, unpacking a release next to it, the swap and
//! the rollback.
//!
//! An install is one folder holding both binaries, so the UI and the
//! daemon always update together:
//!
//! - **macOS:** `Coqo.app` in `~/Applications`, or in `/Applications` if
//!   the user can write there; the binaries are in `Contents/MacOS/`.
//! - **Linux:** `~/.local/share/coqo/bin` (with symlinks from
//!   `~/.local/bin`).
//!
//! Anything else (a `cargo run`, a test binary, a copy somewhere) is a
//! development build, which never installs ([`Install::Dev`]).
//!
//! A release is unpacked into `<install>.staging` and moved to
//! `<install>.new`, beside the install on the same file system. [`swap`]
//! renames the install to `<install>.old` and `.new` into its place: two
//! renames, so no running binary is ever written to (its file lives on
//! under `.old` until it exits). One previous version is kept, for
//! [`roll_back`].

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use crate::UpdateError;

/// The app bundle's name on macOS.
pub const APP_NAME: &str = "Coqo.app";
/// The UI binary.
pub const UI_BIN: &str = "coqo";
/// The daemon binary.
pub const DAEMON_BIN: &str = "coqod";

/// The most entries an archive may hold.
const MAX_ENTRIES: usize = 10_000;
/// The most bytes an archive may unpack to.
const MAX_UNPACKED: u64 = 2 * 1024 * 1024 * 1024;

/// The operating systems Coqo installs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Linux,
}

impl Os {
    /// The one this build runs on.
    pub const CURRENT: Option<Os> = if cfg!(target_os = "macos") {
        Some(Os::Macos)
    } else if cfg!(target_os = "linux") {
        Some(Os::Linux)
    } else {
        None
    };
}

/// Where the running Coqo is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// macOS: the `Coqo.app` bundle.
    App {
        /// `…/Applications/Coqo.app`.
        app: PathBuf,
    },
    /// Linux: the folder with `coqo` and `coqod`.
    BinDir {
        /// `~/.local/share/coqo/bin`.
        bin: PathBuf,
    },
    /// Not an install location: a development build.
    Dev {
        /// The running executable.
        exe: PathBuf,
    },
}

/// `path`, with symlinks resolved when it exists.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

fn writable(dir: &Path) -> bool {
    nix::unistd::access(dir, nix::unistd::AccessFlags::W_OK).is_ok()
}

impl Install {
    /// The running executable's install.
    pub fn detect() -> Install {
        let Ok(exe) = std::env::current_exe() else {
            return Install::Dev {
                exe: PathBuf::new(),
            };
        };
        match (Os::CURRENT, dirs::home_dir()) {
            (Some(os), Some(home)) => Install::detect_at(os, &exe, &home),
            _ => Install::Dev { exe },
        }
    }

    /// The install of `exe` on `os` for the user whose home is `home`.
    pub fn detect_at(os: Os, exe: &Path, home: &Path) -> Install {
        let exe = canonical(exe);
        let dev = || Install::Dev { exe: exe.clone() };
        let Some(dir) = exe.parent() else {
            return dev();
        };
        match os {
            Os::Macos => {
                let contents = dir.parent();
                let app = contents.and_then(Path::parent);
                let (Some(contents), Some(app)) = (contents, app) else {
                    return dev();
                };
                let shaped = dir.file_name().is_some_and(|n| n == "MacOS")
                    && contents.file_name().is_some_and(|n| n == "Contents")
                    && app.file_name().is_some_and(|n| n == APP_NAME);
                let Some(parent) = app.parent() else {
                    return dev();
                };
                let allowed = parent == canonical(&home.join("Applications"))
                    || parent == canonical(Path::new("/Applications"));
                if shaped && allowed && writable(parent) {
                    Install::App {
                        app: app.to_owned(),
                    }
                } else {
                    dev()
                }
            }
            Os::Linux => {
                let bin = canonical(&home.join(".local/share/coqo/bin"));
                match dir.parent() {
                    Some(parent) if dir == bin && writable(parent) => Install::BinDir { bin },
                    _ => dev(),
                }
            }
        }
    }

    /// Whether this is a development build.
    pub fn is_dev(&self) -> bool {
        matches!(self, Install::Dev { .. })
    }

    /// The install's folder: `Coqo.app` or the bin folder.
    pub fn root(&self) -> Option<&Path> {
        match self {
            Install::App { app } => Some(app),
            Install::BinDir { bin } => Some(bin),
            Install::Dev { .. } => None,
        }
    }

    /// The install's folder with `suffix` appended, beside it.
    fn beside(&self, suffix: &str) -> Option<PathBuf> {
        let root = self.root()?;
        let mut name = root.file_name()?.to_owned();
        name.push(suffix);
        Some(root.with_file_name(name))
    }

    /// Where a verified release waits to be swapped in: `<install>.new`.
    pub fn staged(&self) -> Option<PathBuf> {
        self.beside(".new")
    }

    /// The previous version, kept for a rollback: `<install>.old`.
    pub fn previous(&self) -> Option<PathBuf> {
        self.beside(".old")
    }

    /// Where the binaries are inside an install folder like this one.
    fn bin_dir(&self, root: &Path) -> PathBuf {
        match self {
            Install::App { .. } => root.join("Contents/MacOS"),
            _ => root.to_owned(),
        }
    }

    /// The UI binary of the install (after a swap, the new one).
    pub fn ui_binary(&self) -> Option<PathBuf> {
        self.root().map(|r| self.bin_dir(r).join(UI_BIN))
    }

    /// The daemon binary of the install.
    pub fn daemon_binary(&self) -> Option<PathBuf> {
        self.root().map(|r| self.bin_dir(r).join(DAEMON_BIN))
    }

    /// The path the archive's tree starts with: `Coqo.app` or `coqo/bin`.
    fn archive_root(&self) -> Option<&'static [&'static str]> {
        match self {
            Install::App { .. } => Some(&[APP_NAME]),
            Install::BinDir { .. } => Some(&["coqo", "bin"]),
            Install::Dev { .. } => None,
        }
    }

    /// Check that `root` is a complete install folder of this kind: both
    /// binaries are executable files (and, for an app, it has its
    /// `Info.plist`).
    pub fn check_folder(&self, root: &Path) -> Result<(), UpdateError> {
        let bin = self.bin_dir(root);
        let mut required = vec![bin.join(UI_BIN), bin.join(DAEMON_BIN)];
        if matches!(self, Install::App { .. }) {
            required.push(root.join("Contents/Info.plist"));
        }
        for (i, path) in required.iter().enumerate() {
            let meta = fs::symlink_metadata(path).map_err(|_| {
                UpdateError::Format(format!("the release has no {}", path.display()))
            })?;
            let executable = i == 2 || meta.permissions().mode() & 0o111 != 0;
            if !meta.file_type().is_file() || !executable {
                return Err(UpdateError::Format(format!(
                    "{} in the release is not an executable file",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

/// Remove `path` and everything in it, if it exists.
fn remove_all(path: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
    .map_err(UpdateError::io(format!("cannot remove {}", path.display())))
}

/// Unpack the verified `archive` beside `install` as `<install>.new`,
/// replacing an older one, and check it is complete. Returns its path.
pub fn unpack(install: &Install, archive: &Path) -> Result<PathBuf, UpdateError> {
    let (Some(staged), Some(prefix), Some(staging)) = (
        install.staged(),
        install.archive_root(),
        install.beside(".staging"),
    ) else {
        return Err(UpdateError::DevBuild);
    };
    remove_all(&staging)?;
    fs::create_dir(&staging).map_err(UpdateError::io(format!(
        "cannot create {}",
        staging.display()
    )))?;
    let result = (|| {
        extract(archive, &staging, prefix)?;
        let tree = prefix.iter().fold(staging.clone(), |p, c| p.join(c));
        install.check_folder(&tree)?;
        remove_all(&staged)?;
        fs::rename(&tree, &staged).map_err(UpdateError::io(format!(
            "cannot move to {}",
            staged.display()
        )))?;
        clear_quarantine(&staged);
        Ok(staged.clone())
    })();
    let _ = remove_all(&staging);
    result
}

/// The relative, plain path of an archive entry: no `..`, no root, no
/// drive; `.` components are dropped.
fn plain_path(path: &Path) -> Result<PathBuf, UpdateError> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => {
                return Err(UpdateError::UnsafeArchive(format!(
                    "{} leaves the install folder",
                    path.display()
                )));
            }
        }
    }
    Ok(out)
}

/// Unpack `archive` (a `.tar.gz`) into the empty folder `into`. Only
/// folders and regular files under `prefix` are allowed; files get mode
/// 0755 or 0644 (no set-id bits, nothing writable by others), whatever the
/// archive says, and nothing is written twice.
fn extract(archive: &Path, into: &Path, prefix: &[&str]) -> Result<(), UpdateError> {
    let bad = |e: io::Error| UpdateError::UnsafeArchive(format!("cannot read it: {e}"));
    let file = File::open(archive).map_err(UpdateError::io(format!(
        "cannot open {}",
        archive.display()
    )))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file)));
    let mut unpacked = 0u64;
    for (count, entry) in tar.entries().map_err(bad)?.enumerate() {
        if count >= MAX_ENTRIES {
            return Err(UpdateError::UnsafeArchive(format!(
                "more than {MAX_ENTRIES} entries"
            )));
        }
        let mut entry = entry.map_err(bad)?;
        let path = plain_path(&entry.path().map_err(bad)?)?;
        if path.as_os_str().is_empty() {
            continue;
        }
        let kind = entry.header().entry_type();
        let names: Vec<_> = path.iter().collect();
        // Under the prefix, or a folder on the way to it (`coqo/`).
        let under = names.len() > prefix.len()
            && names
                .iter()
                .zip(prefix)
                .all(|(n, p)| *n == std::ffi::OsStr::new(p));
        let on_the_way = kind.is_dir()
            && names.len() <= prefix.len()
            && names
                .iter()
                .zip(prefix)
                .all(|(n, p)| *n == std::ffi::OsStr::new(p));
        if !under && !on_the_way {
            return Err(UpdateError::UnsafeArchive(format!(
                "{} is outside {}",
                path.display(),
                prefix.join("/")
            )));
        }
        let dest = into.join(&path);
        if kind.is_dir() {
            fs::create_dir_all(&dest)
                .and_then(|()| fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)))
                .map_err(UpdateError::io(format!("cannot create {}", dest.display())))?;
        } else if kind.is_file() {
            let mode = entry.header().mode().map_err(bad)?;
            let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(UpdateError::io(format!(
                    "cannot create {}",
                    parent.display()
                )))?;
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&dest)
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        UpdateError::UnsafeArchive(format!("{} is in it twice", path.display()))
                    } else {
                        UpdateError::io(format!("cannot write {}", dest.display()))(e)
                    }
                })?;
            let mut buf = vec![0; 64 * 1024];
            loop {
                let n = entry.read(&mut buf).map_err(bad)?;
                if n == 0 {
                    break;
                }
                unpacked += n as u64;
                if unpacked > MAX_UNPACKED {
                    return Err(UpdateError::UnsafeArchive(format!(
                        "it unpacks to more than {MAX_UNPACKED} bytes"
                    )));
                }
                out.write_all(&buf[..n])
                    .map_err(UpdateError::io(format!("cannot write {}", dest.display())))?;
            }
            out.sync_all()
                .map_err(UpdateError::io(format!("cannot write {}", dest.display())))?;
        } else {
            return Err(UpdateError::UnsafeArchive(format!(
                "{} is a {:?}, not a file or a folder",
                path.display(),
                kind
            )));
        }
    }
    Ok(())
}

/// Swap the staged release in: `<install>` → `<install>.old` (replacing an
/// older one), then `<install>.new` → `<install>`. If the second rename
/// fails the first is undone.
pub fn swap(install: &Install) -> Result<(), UpdateError> {
    let (Some(root), Some(new), Some(old)) = (install.root(), install.staged(), install.previous())
    else {
        return Err(UpdateError::DevBuild);
    };
    install.check_folder(&new)?;
    remove_all(&old)?;
    fs::rename(root, &old).map_err(UpdateError::io(format!(
        "cannot move {} aside",
        root.display()
    )))?;
    if let Err(e) = fs::rename(&new, root) {
        let _ = fs::rename(&old, root);
        return Err(UpdateError::io(format!(
            "cannot move the new version to {}",
            root.display()
        ))(e));
    }
    tracing::info!(install = %root.display(), "swapped in the new version");
    Ok(())
}

/// Undo a [`swap`]: the previous version goes back into place and the
/// failed one is removed. Works once: afterwards there is no previous
/// version.
pub fn roll_back(install: &Install) -> Result<(), UpdateError> {
    let (Some(root), Some(old), Some(failed)) = (
        install.root(),
        install.previous(),
        install.beside(".failed"),
    ) else {
        return Err(UpdateError::DevBuild);
    };
    install.check_folder(&old).map_err(|_| {
        UpdateError::Format(format!("there is no previous version at {}", old.display()))
    })?;
    remove_all(&failed)?;
    fs::rename(root, &failed).map_err(UpdateError::io(format!(
        "cannot move {} aside",
        root.display()
    )))?;
    if let Err(e) = fs::rename(&old, root) {
        let _ = fs::rename(&failed, root);
        return Err(UpdateError::io(format!(
            "cannot move the previous version back to {}",
            root.display()
        ))(e));
    }
    let _ = remove_all(&failed);
    tracing::warn!(install = %root.display(), "rolled back to the previous version");
    Ok(())
}

/// Files Coqo downloads itself are not quarantined, but if the bundle got
/// the quarantine attribute anyway (a browser-downloaded archive put in the
/// update folder, say), clear it so the verified release opens.
#[cfg(target_os = "macos")]
fn clear_quarantine(app: &Path) {
    use std::process::{Command, Stdio};
    let Some(ui) = Install::App {
        app: app.to_owned(),
    }
    .ui_binary() else {
        return;
    };
    let quarantined = Command::new("/usr/bin/xattr")
        .args(["-p", "com.apple.quarantine"])
        .arg(&ui)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if quarantined {
        let _ = Command::new("/usr/bin/xattr")
            .args(["-d", "-r", "com.apple.quarantine"])
            .arg(app)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(not(target_os = "macos"))]
fn clear_quarantine(_: &Path) {}
