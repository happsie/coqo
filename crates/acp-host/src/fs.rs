// SPDX-License-Identifier: Apache-2.0

//! The agent's view of files: ACP `fs/read_text_file` and
//! `fs/write_text_file`, and the edits agents announce in tool calls.
//!
//! In `coqod` the daemon's replicated buffers implement [`AgentFs`]
//! (ADR 0010): reads see unsaved edits, writes into open buffers land as
//! tracked `agent` changes, and announced edits let the file watcher
//! attribute a disk write to the session. Anything else goes to disk, and
//! only inside the session's workspace roots.

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use coqo_agents::model::SessionId;

/// Open buffers, as the ACP host sees them. Every method has a default that
/// does nothing, so a host without buffers serves files from disk only.
pub trait AgentFs: Send + Sync + 'static {
    /// The agent is about to write `path` (a `diff` in a `tool_call` or
    /// `tool_call_update`, or an `fs/write_text_file` that goes to disk):
    /// `old_text` → `new_text`, either the whole file or the replaced
    /// snippet. Called before the write lands.
    fn expect_write(
        &self,
        session: &SessionId,
        path: &Path,
        old_text: Option<String>,
        new_text: String,
    ) {
        let _ = (session, path, old_text, new_text);
    }

    /// The text of the open buffer for `path`, unsaved edits included.
    /// `None` if it is not open.
    fn open_text(&self, path: &Path) -> Option<String> {
        let _ = path;
        None
    }

    /// Replaces the text of the open buffer for `path` as a tracked change
    /// by `session`. `false` if it is not open (the host then writes the
    /// file).
    fn replace_open(&self, session: &SessionId, path: &Path, text: &str) -> bool {
        let _ = (session, path, text);
        false
    }

    /// Extra directories the workspace `workspace` may be read and written
    /// in, besides the session's working directory (multi-root workspaces).
    fn roots(&self, workspace: Option<&str>) -> Vec<PathBuf> {
        let _ = workspace;
        Vec::new()
    }
}

/// An [`AgentFs`] that can be set after the provider is built: built-in
/// provider mods activate before the daemon's buffers exist. Cheap to clone;
/// clones share the slot. Until it is set, files come from disk.
#[derive(Clone, Default)]
pub struct FsHandle(Arc<RwLock<Option<Arc<dyn AgentFs>>>>);

impl fmt::Debug for FsHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FsHandle")
            .field(&self.get().is_some())
            .finish()
    }
}

impl FsHandle {
    /// An empty handle (disk only).
    pub fn new() -> Self {
        Self::default()
    }

    /// Serves files through `fs` from now on.
    pub fn set(&self, fs: Arc<dyn AgentFs>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(fs);
    }

    /// Empties the slot: files come from disk again. What was set usually
    /// reaches back to the provider holding this handle, an `Arc` cycle
    /// that only this breaks.
    pub fn clear(&self) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub(crate) fn get(&self) -> Option<Arc<dyn AgentFs>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// `path` with `.` and `..` resolved lexically, then through symlinks as
/// far as it exists, so a path into a symlinked directory is checked where
/// it really points.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not an absolute path: {}", path.display()),
        ));
    }
    let mut lexical = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            c => lexical.push(c),
        }
    }
    // Canonicalize the longest existing prefix and keep the rest.
    let mut existing = lexical.clone();
    let mut rest = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(real) => {
                let mut out = real;
                for part in rest.iter().rev() {
                    out.push(part);
                }
                return Ok(out);
            }
            Err(_) => match (
                existing.file_name().map(ToOwned::to_owned),
                existing.parent(),
            ) {
                (Some(name), Some(parent)) => {
                    rest.push(name);
                    existing = parent.to_path_buf();
                }
                _ => return Ok(lexical),
            },
        }
    }
}

/// `path`, resolved, if it lies inside one of `roots`.
pub(crate) fn inside(path: &Path, roots: &[PathBuf]) -> io::Result<PathBuf> {
    let real = resolve(path)?;
    let ok = roots.iter().any(|root| {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        real.starts_with(root)
    });
    if ok {
        Ok(real)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is outside the workspace", path.display()),
        ))
    }
}

/// Lines `line..line + limit` (1-based) of `text`, as ACP
/// `fs/read_text_file` asks.
pub(crate) fn slice(text: String, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return text;
    }
    let skip = line.map_or(0, |l| l.saturating_sub(1) as usize);
    let take = limit.map_or(usize::MAX, |l| l as usize);
    text.split_inclusive('\n').skip(skip).take(take).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_the_handle_lets_go_of_what_was_set() {
        struct Nothing;
        impl AgentFs for Nothing {}
        let fs = Arc::new(Nothing);
        let handle = FsHandle::new();
        handle.set(Arc::clone(&fs) as Arc<dyn AgentFs>);
        let clone = handle.clone();
        assert!(clone.get().is_some());
        assert_eq!(Arc::strong_count(&fs), 2);
        clone.clear();
        assert!(handle.get().is_none());
        assert_eq!(Arc::strong_count(&fs), 1);
    }

    #[test]
    fn slices_lines() {
        let text = "a\nb\nc\n".to_owned();
        assert_eq!(slice(text.clone(), None, None), "a\nb\nc\n");
        assert_eq!(slice(text.clone(), Some(2), None), "b\nc\n");
        assert_eq!(slice(text.clone(), Some(2), Some(1)), "b\n");
        assert_eq!(slice(text, None, Some(1)), "a\n");
    }

    #[test]
    fn only_paths_inside_the_roots_pass() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let roots = vec![root.clone()];
        assert!(inside(&root.join("src/new/file.rs"), &roots).is_ok());
        assert!(inside(&root.join("src/../../escape"), &roots).is_err());
        assert!(inside(&dir.path().join("other"), &roots).is_err());
        assert!(inside(Path::new("relative"), &roots).is_err());

        // A symlink out of the workspace does not count as inside.
        std::os::unix::fs::symlink(dir.path(), root.join("link")).unwrap();
        assert!(inside(&root.join("link/other"), &roots).is_err());
    }
}
