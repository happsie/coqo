// SPDX-License-Identifier: Apache-2.0

//! Reading and atomically writing buffer files. Blocking; called on
//! blocking threads.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a file could not be read.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// Larger than the limit.
    TooLarge { size: u64 },
    /// Anything else.
    Io(io::Error),
}

/// Read `path`, failing if it is larger than `limit` bytes.
pub(crate) fn read(path: &Path, limit: u64) -> Result<Vec<u8>, ReadError> {
    let file = File::open(path).map_err(ReadError::Io)?;
    let meta = file.metadata().map_err(ReadError::Io)?;
    if !meta.is_file() {
        return Err(ReadError::Io(io::Error::other("not a regular file")));
    }
    if meta.len() > limit {
        return Err(ReadError::TooLarge { size: meta.len() });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    // One byte past the limit, in case the file grew since the stat.
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(ReadError::TooLarge {
            size: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

/// Replace `path` with `bytes` atomically: write a temporary file next to
/// it (same permissions), flush it to disk, then rename it over `path`.
/// Readers see the old or the new file, never a partial one. `path` must be
/// the real (symlink-free) path, so a symlink pointing at it stays a link.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the file has no parent directory"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("the file name is not UTF-8"))?;
    let mode = fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    let tmp = dir.join(format!(
        ".{name}.coqo-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(bytes)?;
        // `mode` is masked by the umask at creation; set it exactly.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        // Make the rename itself durable.
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_keeps_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("script.sh");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        write_atomic(&path, b"new contents").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new contents");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
        let leftovers: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file left behind");
    }

    #[test]
    fn read_enforces_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big");
        fs::write(&path, vec![b'x'; 100]).unwrap();
        assert_eq!(read(&path, 100).unwrap().len(), 100);
        assert!(matches!(
            read(&path, 99),
            Err(ReadError::TooLarge { size: 100 })
        ));
        assert!(matches!(
            read(&tmp.path().join("missing"), 10),
            Err(ReadError::Io(e)) if e.kind() == io::ErrorKind::NotFound
        ));
    }
}
