// SPDX-License-Identifier: Apache-2.0

//! `fs/list`, `fs/stat`, `fs/read`: read-only access inside a workspace.
//! Synchronous; the service calls these on a blocking thread.

use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use coqo_protocol::ErrorObject;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{
    DEFAULT_READ_LIMIT, DirEntry, EntryKind, FileStat, FileTooLarge, ListDirResult, MAX_READ_LIMIT,
    ReadResult,
};

use super::error;
use super::ignores::IgnoreCache;
use super::paths::{self, Resolved};

/// What the fs functions need to know about the workspace.
#[derive(Debug)]
pub(crate) struct Roots<'a> {
    /// The root the client path is relative to (canonical).
    pub root: &'a Path,
    /// Every root of the workspace (canonical), for confinement.
    pub all: &'a [PathBuf],
}

pub(crate) fn mtime(meta: &Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(d.as_millis()).ok()
}

fn kind(meta: &Metadata) -> EntryKind {
    if meta.is_dir() {
        EntryKind::Dir
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn size(meta: &Metadata) -> u64 {
    if meta.is_dir() { 0 } else { meta.len() }
}

/// Resolves the parent chain for ignore rules of `comps` (a path's components).
fn ignored(cache: &IgnoreCache, root: &Path, comps: &[String], r: &Resolved) -> bool {
    cache.is_ignored(root, comps, r.meta.is_dir())
}

pub(crate) fn stat(
    cache: &IgnoreCache,
    roots: &Roots<'_>,
    rel: &str,
) -> Result<FileStat, ErrorObject> {
    let (comps, r) = paths::resolve(roots.root, roots.all, rel)?;
    Ok(FileStat {
        kind: kind(&r.meta),
        symlink: r.symlink,
        size: size(&r.meta),
        mtime: mtime(&r.meta),
        ignored: ignored(cache, roots.root, &comps, &r),
    })
}

pub(crate) fn list(
    cache: &IgnoreCache,
    roots: &Roots<'_>,
    rel: &str,
) -> Result<ListDirResult, ErrorObject> {
    let (comps, r) = paths::resolve(roots.root, roots.all, rel)?;
    if !r.meta.is_dir() {
        return Err(error::not_a_directory(rel));
    }
    let chain = cache.chain(roots.root, &comps);
    let read = fs::read_dir(&r.real).map_err(|e| error::io(rel, &e))?;
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| error::io(rel, &e))?;
        let Ok(name) = entry.file_name().into_string() else {
            // Paths in the protocol are strings; a name that is not UTF-8
            // could not be addressed anyway.
            tracing::debug!(dir = %r.real.display(), "skipping a non-UTF-8 file name");
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let symlink = file_type.is_symlink();
        let meta = if symlink {
            // Follow it only if it stays inside the workspace.
            match fs::canonicalize(&path) {
                Ok(real) if paths::inside(&real, roots.all) => fs::metadata(&real).ok(),
                _ => None,
            }
        } else {
            entry.metadata().ok()
        };
        let (kind, size, mtime) = match &meta {
            Some(m) => (kind(m), size(m), mtime(m)),
            None => (
                EntryKind::Other,
                0,
                fs::symlink_metadata(&path).ok().as_ref().and_then(mtime),
            ),
        };
        let is_dir = kind == EntryKind::Dir;
        entries.push(DirEntry {
            ignored: chain.is_ignored(&paths::join(&r.lexical, &[&name]), is_dir),
            name,
            kind,
            symlink,
            size,
            mtime,
        });
    }
    entries.sort_by(|a, b| {
        let dir = |e: &DirEntry| e.kind != EntryKind::Dir;
        dir(a)
            .cmp(&dir(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(ListDirResult { entries })
}

pub(crate) fn read(
    roots: &Roots<'_>,
    rel: &str,
    max_bytes: Option<u64>,
) -> Result<ReadResult, ErrorObject> {
    let limit = max_bytes.unwrap_or(DEFAULT_READ_LIMIT);
    if limit > MAX_READ_LIMIT {
        return Err(ErrorObject::invalid_params(format!(
            "maxBytes {limit} is above the maximum of {MAX_READ_LIMIT}"
        )));
    }
    let (_, r) = paths::resolve(roots.root, roots.all, rel)?;
    if !r.meta.is_file() {
        return Err(error::not_a_file(rel));
    }
    let too_large = |size| {
        ErrorObject::new(
            codes::FILE_TOO_LARGE,
            format!("{rel:?} is {size} bytes, more than the limit of {limit}"),
        )
        .with_data(FileTooLarge { size, limit })
    };
    if r.meta.len() > limit {
        return Err(too_large(r.meta.len()));
    }
    let file = File::open(&r.real).map_err(|e| error::io(rel, &e))?;
    let meta = file.metadata().map_err(|e| error::io(rel, &e))?;
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    // Read one byte past the limit, in case the file grew since the stat.
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| error::io(rel, &e))?;
    let size = bytes.len() as u64;
    if size > limit {
        return Err(too_large(meta.len().max(size)));
    }
    let (text, binary) = match String::from_utf8(bytes) {
        Ok(t) => (Some(t), false),
        Err(_) => (None, true),
    };
    Ok(ReadResult {
        text,
        binary,
        size,
        mtime: mtime(&meta),
    })
}
