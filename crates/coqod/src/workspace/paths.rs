// SPDX-License-Identifier: Apache-2.0

//! Workspace-relative paths and confinement to the workspace's roots.
//!
//! A client path is `/`-separated and relative to one root. It is checked
//! twice: lexically (no absolute paths, no `..`, no NUL) before touching the
//! disk, and after resolving symlinks (the real path must lie inside one of
//! the workspace's roots), so neither `..` nor a symlink can escape.
//!
//! There is an unavoidable window between the check and the read in which a
//! local process could swap a directory for a symlink. The daemon only ever
//! reads on behalf of the same user, so this is accepted.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use coqo_protocol::ErrorObject;

use super::error;

/// A client path split into its normal components (`""` and `.` dropped).
pub(crate) fn parse_rel(path: &str) -> Result<Vec<&str>, ErrorObject> {
    if path.starts_with('/') || path.contains('\0') {
        return Err(error::outside(path));
    }
    let mut out = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => return Err(error::outside(path)),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Joins components onto `root` (no disk access).
pub(crate) fn join(root: &Path, comps: &[&str]) -> PathBuf {
    let mut p = root.to_path_buf();
    p.extend(comps);
    p
}

/// Whether `real` (a canonical path) lies inside one of `roots` (canonical).
pub(crate) fn inside(real: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| real.starts_with(r))
}

/// A client path, checked and resolved.
#[derive(Debug)]
pub(crate) struct Resolved {
    /// The path as named (root joined with the components), used for ignore
    /// rules and for display.
    pub lexical: PathBuf,
    /// The canonical path, inside the workspace.
    pub real: PathBuf,
    /// Whether the named path itself is a symlink.
    pub symlink: bool,
    /// Metadata of the target.
    pub meta: fs::Metadata,
}

/// Resolve `rel` under `root`, requiring the result to be inside `roots`.
///
/// Fails with `NOT_FOUND` if it does not exist (or is a broken symlink) and
/// `PATH_OUTSIDE_WORKSPACE` if it resolves outside the roots.
pub(crate) fn resolve(
    root: &Path,
    roots: &[PathBuf],
    rel: &str,
) -> Result<(Vec<String>, Resolved), ErrorObject> {
    let comps = parse_rel(rel)?;
    let lexical = join(root, &comps);
    let link = fs::symlink_metadata(&lexical).map_err(|e| error::io(rel, &e))?;
    let real = fs::canonicalize(&lexical).map_err(|e| error::io(rel, &e))?;
    if !inside(&real, roots) {
        return Err(error::outside(rel));
    }
    let meta = if link.file_type().is_symlink() {
        fs::metadata(&real).map_err(|e| error::io(rel, &e))?
    } else {
        link.clone()
    };
    Ok((
        comps.into_iter().map(str::to_owned).collect(),
        Resolved {
            lexical,
            real,
            symlink: link.file_type().is_symlink(),
            meta,
        },
    ))
}

/// Canonicalise a folder the client wants as a root: it must exist and be a
/// directory.
pub(crate) fn canonical_dir(path: &str) -> Result<PathBuf, ErrorObject> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(ErrorObject::invalid_params(format!(
            "workspace folder {path:?} must be an absolute path"
        )));
    }
    let real = fs::canonicalize(p).map_err(|e| error::io(path, &e))?;
    if !real.is_dir() {
        return Err(error::io(
            path,
            &io::Error::from(io::ErrorKind::NotADirectory),
        ));
    }
    if real.to_str().is_none() {
        return Err(ErrorObject::invalid_params(format!(
            "workspace folder {path:?} is not valid UTF-8"
        )));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_protocol::methods::codes;

    #[test]
    fn parse_rel_normalises_and_rejects_escapes() {
        assert_eq!(parse_rel("").unwrap(), Vec::<&str>::new());
        assert_eq!(parse_rel("./a//b/.").unwrap(), vec!["a", "b"]);
        for bad in ["/etc", "..", "a/../b", "a/..", "../x", "a\0b"] {
            assert_eq!(
                parse_rel(bad).unwrap_err().code,
                codes::PATH_OUTSIDE_WORKSPACE,
                "{bad:?}"
            );
        }
        // `..` inside a name is fine.
        assert_eq!(parse_rel("a..b/..c").unwrap(), vec!["a..b", "..c"]);
    }

    #[test]
    fn canonical_dir_requires_an_existing_absolute_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        fs::write(&file, "x").unwrap();
        assert_eq!(
            canonical_dir(tmp.path().to_str().unwrap()).unwrap(),
            fs::canonicalize(tmp.path()).unwrap()
        );
        assert_eq!(
            canonical_dir("relative/dir").unwrap_err().code,
            coqo_protocol::codes::INVALID_PARAMS
        );
        assert_eq!(
            canonical_dir(file.to_str().unwrap()).unwrap_err().code,
            codes::NOT_A_DIRECTORY
        );
        assert_eq!(
            canonical_dir(tmp.path().join("missing").to_str().unwrap())
                .unwrap_err()
                .code,
            codes::NOT_FOUND
        );
    }
}
