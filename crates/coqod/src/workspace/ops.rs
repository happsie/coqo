// SPDX-License-Identifier: Apache-2.0

//! `fs/create`, `fs/rename`, `fs/delete`: the file tree's operations.
//! Synchronous; the service calls these on a blocking thread.
//!
//! An entry is named by its parent folder and its name. The parent is
//! resolved like any other path (lexically checked, symlinks followed, and
//! the real folder required to be inside the roots), but the entry itself is
//! not followed: renaming or deleting a symlink acts on the link, never on
//! what it points at, so a link cannot carry an operation outside the
//! workspace. New entries are created with `create_new` / `mkdir`, which
//! never follow a symlink at the final name either.
//!
//! The checks and the operation are separate steps, so a local process that
//! swaps a folder for a symlink in between could redirect one. The daemon
//! only ever acts for the same user, so this is accepted (as for reads).

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use coqo_protocol::ErrorObject;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::EntryKind;

use super::error;
use super::fs::Roots;
use super::paths;

/// Where `fs/delete` puts what it deletes, unless asked to delete for good.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Trash {
    /// The operating system's trash (the freedesktop.org trash on Linux,
    /// the Finder's on macOS), through the `trash` crate.
    #[default]
    System,
    /// Move deleted entries into this folder instead (tests, so they never
    /// touch the user's trash).
    Folder(PathBuf),
}

/// An entry named by a client path: its parent's real folder (inside the
/// roots) and its name.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The path's components, normalised.
    pub comps: Vec<String>,
    /// The entry: the parent's real folder joined with the name. Not
    /// resolved itself (it may be a symlink, or not exist yet).
    pub path: PathBuf,
}

impl Entry {
    /// The client path, normalised.
    pub fn rel(&self) -> String {
        self.comps.join("/")
    }
}

/// Resolve the parent of `rel` and name the entry in it. `rel` must not be
/// the root itself.
pub(crate) fn entry(roots: &Roots<'_>, rel: &str) -> Result<Entry, ErrorObject> {
    let comps = paths::parse_rel(rel)?;
    let Some((name, parents)) = comps.split_last() else {
        return Err(ErrorObject::invalid_params(
            "a workspace root cannot be created, renamed or deleted here",
        ));
    };
    let parent_rel = parents.join("/");
    let (_, parent) = paths::resolve(roots.root, roots.all, &parent_rel)?;
    if !parent.meta.is_dir() {
        return Err(error::not_a_directory(&parent_rel));
    }
    Ok(Entry {
        path: parent.real.join(name),
        comps: comps.into_iter().map(str::to_owned).collect(),
    })
}

/// Refuse to move or delete a workspace root, or a folder holding one.
fn not_a_root(roots: &Roots<'_>, entry: &Entry) -> Result<(), ErrorObject> {
    if roots.all.iter().any(|r| r.starts_with(&entry.path)) {
        return Err(ErrorObject::invalid_params(format!(
            "{:?} is (or holds) a root of the workspace; remove the root instead",
            entry.rel()
        )));
    }
    Ok(())
}

/// The entry's own metadata (not its target's), or `NOT_FOUND`.
fn existing(entry: &Entry) -> Result<fs::Metadata, ErrorObject> {
    fs::symlink_metadata(&entry.path).map_err(|e| error::io(entry.rel(), &e))
}

/// `fs/create`. Returns the normalised path.
pub(crate) fn create(roots: &Roots<'_>, rel: &str, kind: EntryKind) -> Result<String, ErrorObject> {
    if kind == EntryKind::Other {
        return Err(ErrorObject::invalid_params(
            "fs/create makes a `file` or a `dir`",
        ));
    }
    let comps = paths::parse_rel(rel)?;
    if comps.is_empty() {
        return Err(ErrorObject::invalid_params(
            "fs/create needs the path of the new entry",
        ));
    }
    // The deepest folder on the way that exists already; the rest is made.
    let mut have = comps.len() - 1;
    while have > 0 && fs::symlink_metadata(paths::join(roots.root, &comps[..have])).is_err() {
        have -= 1;
    }
    let base_rel = comps[..have].join("/");
    let (_, base) = paths::resolve(roots.root, roots.all, &base_rel)?;
    if !base.meta.is_dir() {
        return Err(error::not_a_directory(&base_rel));
    }
    let mut dir = base.real;
    for (i, name) in comps.iter().enumerate().skip(have) {
        let path = dir.join(name);
        let shown = comps[..=i].join("/");
        let last = i + 1 == comps.len();
        let made = if last && kind == EntryKind::File {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map(drop)
        } else {
            fs::create_dir(&path)
        };
        match made {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(error::exists(&shown));
            }
            Err(e) => return Err(error::io(&shown, &e)),
        }
        dir = path;
    }
    Ok(comps.join("/"))
}

/// What [`rename`] moved, as real paths.
#[derive(Debug)]
pub(crate) struct Renamed {
    pub from: PathBuf,
    pub to: PathBuf,
    /// The new client path, normalised.
    pub rel: String,
}

/// Whether two entries are the same file (a case-only rename on a
/// case-insensitive file system).
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// `fs/rename`.
pub(crate) fn rename(roots: &Roots<'_>, from: &str, to: &str) -> Result<Renamed, ErrorObject> {
    let source = entry(roots, from)?;
    let meta = existing(&source)?;
    not_a_root(roots, &source)?;
    let target = entry(roots, to)?;
    let renamed = Renamed {
        from: source.path.clone(),
        to: target.path.clone(),
        rel: target.rel(),
    };
    if source.path == target.path {
        return Ok(renamed);
    }
    if meta.is_dir() && target.path.starts_with(&source.path) {
        return Err(ErrorObject::invalid_params(format!(
            "cannot move {:?} into itself",
            source.rel()
        )));
    }
    if let Ok(there) = fs::symlink_metadata(&target.path)
        && !same_file(&meta, &there)
    {
        return Err(error::exists(&target.rel()));
    }
    fs::rename(&source.path, &target.path).map_err(|e| error::io(source.rel(), &e))?;
    Ok(renamed)
}

/// What [`delete`] did.
#[derive(Debug)]
pub(crate) struct Deleted {
    /// Where [`Trash::Folder`] put it.
    pub binned: Option<PathBuf>,
}

/// `fs/delete`.
pub(crate) fn delete(
    roots: &Roots<'_>,
    rel: &str,
    permanent: bool,
    trash: &Trash,
) -> Result<Deleted, ErrorObject> {
    let entry = entry(roots, rel)?;
    let meta = existing(&entry)?;
    not_a_root(roots, &entry)?;
    if permanent {
        let removed = if meta.is_dir() {
            fs::remove_dir_all(&entry.path)
        } else {
            fs::remove_file(&entry.path)
        };
        removed.map_err(|e| error::io(entry.rel(), &e))?;
        return Ok(Deleted { binned: None });
    }
    let binned = match trash {
        Trash::System => {
            trash::delete(&entry.path).map_err(|e| trash_failed(&entry, &e))?;
            None
        }
        Trash::Folder(folder) => Some(to_folder(&entry, folder)?),
    };
    Ok(Deleted { binned })
}

fn trash_failed(entry: &Entry, e: &dyn std::fmt::Display) -> ErrorObject {
    ErrorObject::new(
        codes::TRASH_FAILED,
        format!(
            "cannot move {:?} to the trash ({e}); it was left alone. Delete it permanently instead",
            entry.rel()
        ),
    )
}

/// [`Trash::Folder`]: move the entry in under a name not taken yet, and
/// say where.
fn to_folder(entry: &Entry, folder: &Path) -> Result<PathBuf, ErrorObject> {
    fs::create_dir_all(folder).map_err(|e| trash_failed(entry, &e))?;
    let name = entry.comps.last().map_or("entry", String::as_str);
    let mut n = 0u32;
    loop {
        let to = if n == 0 {
            folder.join(name)
        } else {
            folder.join(format!("{name}.{n}"))
        };
        if fs::symlink_metadata(&to).is_err() {
            return fs::rename(&entry.path, &to)
                .map(|()| to)
                .map_err(|e| trash_failed(entry, &e));
        }
        n += 1;
    }
}
