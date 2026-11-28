// SPDX-License-Identifier: Apache-2.0

//! Undoing `fs/delete` (issue #291): what a deleted entry can be brought
//! back from, and `fs/restore`.
//!
//! Every delete that can be undone gets a token (random, valid while this
//! daemon runs) naming where the entry went:
//!
//! - **The OS trash**, where the platform can restore from it: the
//!   freedesktop.org trash on Linux and the Recycle Bin on Windows, through
//!   `trash::os_limited`. The entry is found again by its original path
//!   and the time it was deleted.
//! - **The trash folder** of [`Trash::Folder`] (tests): moved back.
//! - **A copy Coqo kept** in `<data>/undo/` at delete time, where the trash
//!   can't give entries back (macOS's Finder trash), for entries up to
//!   [`COPY_LIMIT`] in all. Copies are removed when restored, when their
//!   token is evicted ([`MAX_RESTORES`], [`MAX_AGE`]), and when the daemon
//!   starts (a previous run's are unreachable).
//!
//! Anything else (deleted permanently, too large for a copy) has no token,
//! and the delete's result says why.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use coqo_protocol::ErrorObject;
use coqo_protocol::methods::codes;

use super::error;
use super::fs::Roots;
use super::ops::{self, Trash};

/// The largest entry (all its files) Coqo keeps a copy of.
pub const COPY_LIMIT: u64 = 16 * 1024 * 1024;
/// Most entries in a folder Coqo keeps a copy of.
const COPY_ENTRIES: usize = 10_000;
/// Tokens kept at most; the oldest go first.
const MAX_RESTORES: usize = 128;
/// How long a token stays valid.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether `trash::os_limited` can restore from this platform's trash.
const OS_RESTORE: bool = cfg!(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
));

/// Where a deleted entry can be restored from.
#[derive(Debug)]
enum Source {
    /// The OS trash, deleted at or after this time (Unix seconds).
    Os { after: i64 },
    /// [`Trash::Folder`]: where it was moved.
    Folder(PathBuf),
    /// Coqo's copy.
    Copy(PathBuf),
}

#[derive(Debug)]
struct Restorable {
    token: String,
    /// The entry's real path when deleted.
    original: PathBuf,
    source: Source,
    at: Instant,
}

/// The delete-undo state of a workspace service.
#[derive(Debug)]
pub(crate) struct Restores {
    /// Where copies go.
    dir: PathBuf,
    /// Whether to keep copies (where the OS trash can't restore).
    copies: bool,
    entries: Mutex<VecDeque<Restorable>>,
}

impl Drop for Restores {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
            .to_be_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove a file, a folder or a link.
fn remove(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copy `from` to `to` (not following links), giving up past `budget`
/// bytes or [`COPY_ENTRIES`] entries (`Ok(false)`).
fn copy_tree(from: &Path, to: &Path, budget: &mut u64, entries: &mut usize) -> io::Result<bool> {
    *entries += 1;
    if *entries > COPY_ENTRIES {
        return Ok(false);
    }
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, to)?;
        #[cfg(not(unix))]
        {
            let _ = target;
            return Ok(false);
        }
        return Ok(true);
    }
    if meta.is_dir() {
        fs::create_dir(to)?;
        for child in fs::read_dir(from)? {
            let child = child?;
            if !copy_tree(&child.path(), &to.join(child.file_name()), budget, entries)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if meta.len() > *budget {
        return Ok(false);
    }
    *budget -= meta.len();
    fs::copy(from, to)?;
    Ok(true)
}

/// Move `from` to `to`, copying when they are on different file systems.
fn move_back(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            let (mut budget, mut entries) = (u64::MAX, 0);
            if !copy_tree(from, to, &mut budget, &mut entries)? {
                let _ = remove(to);
                return Err(io::Error::other("too many entries to copy back"));
            }
            remove(from)
        }
    }
}

impl Restores {
    /// Copies go in `dir` (emptied now), kept when `copies` is set.
    pub fn new(dir: PathBuf, copies: bool) -> Self {
        let _ = fs::remove_dir_all(&dir);
        Self {
            dir,
            copies,
            entries: Mutex::default(),
        }
    }

    /// Whether copies are kept: by default only where the OS trash can't
    /// restore.
    pub fn copies_by_default() -> bool {
        !OS_RESTORE
    }

    /// Keep copies of deleted entries (or not).
    pub fn set_copies(&mut self, copies: bool) {
        self.copies = copies;
    }

    fn remember(&self, original: PathBuf, source: Source) -> String {
        let token = token();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        while entries
            .front()
            .is_some_and(|e| entries.len() >= MAX_RESTORES || now.duration_since(e.at) > MAX_AGE)
        {
            if let Some(old) = entries.pop_front()
                && let Source::Copy(copy) = &old.source
            {
                let _ = remove(copy);
            }
        }
        entries.push_back(Restorable {
            token: token.clone(),
            original,
            source,
            at: now,
        });
        token
    }

    /// `fs/delete`: delete `rel` and say how it can be restored.
    pub fn delete(
        &self,
        roots: &Roots<'_>,
        rel: &str,
        permanent: bool,
        trash: &Trash,
    ) -> Result<coqo_protocol::methods::fs::DeleteResult, ErrorObject> {
        use coqo_protocol::methods::fs::DeleteResult;
        let entry = ops::entry(roots, rel)?;
        let name = entry.comps.last().cloned().unwrap_or_default();
        if permanent {
            ops::delete(roots, rel, true, trash)?;
            return Ok(DeleteResult {
                trashed: false,
                restore: None,
                no_restore: Some(format!("“{name}” was deleted permanently.")),
            });
        }
        // A copy first, where the trash can't give it back.
        let copy = if self.copies && fs::symlink_metadata(&entry.path).is_ok() {
            let copy = self.dir.join(token());
            let (mut budget, mut entries) = (COPY_LIMIT, 0);
            let made = fs::create_dir_all(&self.dir)
                .and_then(|()| copy_tree(&entry.path, &copy, &mut budget, &mut entries));
            match made {
                Ok(true) => Some(copy),
                Ok(false) => {
                    let _ = remove(&copy);
                    None
                }
                Err(e) => {
                    tracing::debug!(path = %entry.path.display(), error = %e, "cannot keep a copy to undo a delete");
                    let _ = remove(&copy);
                    None
                }
            }
        } else {
            None
        };
        let after = unix_now() - 1;
        let deleted = match ops::delete(roots, rel, false, trash) {
            Ok(d) => d,
            Err(e) => {
                if let Some(copy) = &copy {
                    let _ = remove(copy);
                }
                return Err(e);
            }
        };
        // Copies are kept where the trash can't give entries back: then
        // the copy is the only way.
        let source = match (self.copies, copy, trash, deleted.binned) {
            (_, Some(copy), _, _) => Some(Source::Copy(copy)),
            (true, None, _, _) => None,
            (false, None, Trash::Folder(_), Some(binned)) => Some(Source::Folder(binned)),
            (false, None, Trash::System, _) if OS_RESTORE => Some(Source::Os { after }),
            _ => None,
        };
        Ok(match source {
            Some(source) => DeleteResult {
                trashed: true,
                restore: Some(self.remember(entry.path, source)),
                no_restore: None,
            },
            None => DeleteResult {
                trashed: true,
                restore: None,
                no_restore: Some(if self.copies {
                    format!(
                        "“{name}” is too large for Coqo to keep a copy; restore it from the Trash."
                    )
                } else {
                    format!("The Trash can't give “{name}” back here; restore it from the Trash.")
                }),
            },
        })
    }

    /// `fs/restore`: bring `rel` back from what `token` names.
    pub fn restore(
        &self,
        roots: &Roots<'_>,
        rel: &str,
        token: &str,
    ) -> Result<String, ErrorObject> {
        let entry = ops::entry(roots, rel)?;
        let name = entry.comps.last().cloned().unwrap_or_default();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(i) = entries.iter().position(|e| e.token == token) else {
            return Err(ErrorObject::new(
                codes::NOT_FOUND,
                format!(
                    "“{name}” can no longer be restored (the daemon restarted, or too much was deleted since)"
                ),
            ));
        };
        if entries[i].original != entry.path {
            return Err(ErrorObject::invalid_params(format!(
                "that restore is for another path than {rel:?}"
            )));
        }
        if fs::symlink_metadata(&entry.path).is_ok() {
            return Err(error::exists(&entry.rel()));
        }
        let restored = match &entries[i].source {
            Source::Copy(from) | Source::Folder(from) => {
                move_back(from, &entry.path).map_err(|e| error::io(entry.rel(), &e))
            }
            Source::Os { after } => os_restore(&entry.path, *after, &name),
        };
        restored?;
        entries.remove(i);
        Ok(entry.rel())
    }
}

/// Restore `original` from the OS trash: the item deleted from there last,
/// at or after `after`.
#[cfg(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
))]
fn os_restore(original: &Path, after: i64, name: &str) -> Result<(), ErrorObject> {
    use trash::os_limited;
    let failed = |e: &dyn std::fmt::Display| {
        ErrorObject::new(
            codes::TRASH_FAILED,
            format!("cannot restore “{name}” from the trash: {e}"),
        )
    };
    let items = os_limited::list().map_err(|e| failed(&e))?;
    let Some(item) = items
        .into_iter()
        .filter(|i| i.original_path() == original && i.time_deleted >= after)
        .max_by_key(|i| i.time_deleted)
    else {
        return Err(ErrorObject::new(
            codes::NOT_FOUND,
            format!("“{name}” is no longer in the trash"),
        ));
    };
    match os_limited::restore_all([item]) {
        Ok(()) => Ok(()),
        Err(trash::Error::RestoreCollision { .. }) => Err(ErrorObject::new(
            codes::ALREADY_EXISTS,
            format!("something is already at “{name}”"),
        )),
        Err(e) => Err(failed(&e)),
    }
}

#[cfg(not(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
)))]
fn os_restore(_: &Path, _: i64, name: &str) -> Result<(), ErrorObject> {
    Err(ErrorObject::new(
        codes::UNSUPPORTED,
        format!("the Trash can't give “{name}” back here"),
    ))
}
