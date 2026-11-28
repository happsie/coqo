// SPDX-License-Identifier: Apache-2.0

//! The file tree's undo history (issue #291): the operations a window's
//! tree did, newest last, bounded to [`LIMIT`], and those undone, for redo.
//!
//! Only what the tree did itself is recorded, never changes on disk from
//! elsewhere, plus the untracked files the Git pane's Discard moved to the
//! trash (#226), which undo brings back like a delete. A new operation
//! clears what was undone, as in an editor. The history is the window's
//! ([`History::for_window`]) and starts empty with each workspace.

use coqo_protocol::methods::{fs::EntryKind, workspace::RootId};
use coqo_ui::gpui::{AnyWindowHandle, App, AppContext as _, Entity, Global, WeakEntity, Window};

/// Operations kept at most; the oldest are forgotten.
pub const LIMIT: usize = 100;

/// One file operation, as done (forward).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// `path` was created.
    Create {
        root: RootId,
        path: String,
        kind: EntryKind,
    },
    /// `from` was copied (or duplicated) to `path`. Redo copies it again.
    Copy {
        root: RootId,
        from: String,
        path: String,
        kind: EntryKind,
    },
    /// `from` was renamed or moved to `to`.
    Move {
        root: RootId,
        from: String,
        to: String,
    },
    /// `path` was deleted. `restore` is the daemon's token to bring it back
    /// (`fs/restore`), or why it can't be.
    Delete {
        root: RootId,
        path: String,
        kind: EntryKind,
        restore: Result<String, String>,
    },
}

fn name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

impl FileOp {
    /// The root it was in.
    pub fn root(&self) -> RootId {
        match self {
            Self::Create { root, .. }
            | Self::Copy { root, .. }
            | Self::Move { root, .. }
            | Self::Delete { root, .. } => *root,
        }
    }

    /// The entry an undo of this operation would delete: what a creation or
    /// a copy made.
    pub fn made(&self) -> Option<(RootId, &str, EntryKind)> {
        match self {
            Self::Create { root, path, kind }
            | Self::Copy {
                root, path, kind, ..
            } => Some((*root, path, *kind)),
            _ => None,
        }
    }

    /// What it was, for "Undid …" and the menu: `rename of “a.rs” →
    /// “b.rs”`, `move of “a.rs” to “src”`, `creation of “a.rs”`, `delete of
    /// “a.rs”`.
    pub fn describe(&self) -> String {
        match self {
            Self::Create { path, .. } => format!("creation of “{}”", name(path)),
            Self::Copy { path, .. } => format!("copy of “{}”", name(path)),
            Self::Delete { path, .. } => format!("delete of “{}”", name(path)),
            Self::Move { from, to, .. } if parent(from) == parent(to) => {
                format!("rename of “{}” → “{}”", name(from), name(to))
            }
            Self::Move { from, to, .. } => {
                let dir = parent(to);
                let dir = if dir.is_empty() {
                    "the top folder"
                } else {
                    name(dir)
                };
                format!("move of “{}” to “{dir}”", name(from))
            }
        }
    }

    /// Why it can't be undone, if it can't.
    pub fn cannot_undo(&self) -> Option<&str> {
        match self {
            Self::Delete {
                restore: Err(why), ..
            } => Some(why),
            _ => None,
        }
    }
}

/// A window's tree history. See the module docs.
#[derive(Debug, Default)]
pub struct History {
    done: Vec<FileOp>,
    undone: Vec<FileOp>,
}

/// The histories, by window.
#[derive(Default)]
struct Histories(Vec<(AnyWindowHandle, WeakEntity<History>)>);

impl Global for Histories {}

impl History {
    /// `window`'s history, made on first use. The views that hold it (the
    /// file tree, the Git pane) keep it alive.
    pub fn for_window(window: &mut Window, cx: &mut App) -> Entity<History> {
        let handle = window.window_handle();
        let all = cx.default_global::<Histories>();
        all.0.retain(|(_, h)| h.upgrade().is_some());
        if let Some(history) = all
            .0
            .iter()
            .find(|(w, _)| *w == handle)
            .and_then(|(_, h)| h.upgrade())
        {
            return history;
        }
        let history = cx.new(|_| History::default());
        cx.default_global::<Histories>()
            .0
            .push((handle, history.downgrade()));
        history
    }

    /// Whether `ops` are the newest operations, in order (the last of them
    /// on top): what one undo of them all would undo.
    pub fn ends_with(&self, ops: &[FileOp]) -> bool {
        !ops.is_empty() && self.done.ends_with(ops)
    }

    /// The tree did `op`: it can be undone, and nothing undone can be redone.
    pub fn record(&mut self, op: FileOp) {
        self.undone.clear();
        self.push_done(op);
    }

    fn push_done(&mut self, op: FileOp) {
        if self.done.len() >= LIMIT {
            self.done.remove(0);
        }
        self.done.push(op);
    }

    /// What undo would undo.
    pub fn next_undo(&self) -> Option<&FileOp> {
        self.done.last()
    }

    /// What redo would redo.
    pub fn next_redo(&self) -> Option<&FileOp> {
        self.undone.last()
    }

    /// Take the operation to undo.
    pub fn take_undo(&mut self) -> Option<FileOp> {
        self.done.pop()
    }

    /// Take the operation to redo.
    pub fn take_redo(&mut self) -> Option<FileOp> {
        self.undone.pop()
    }

    /// `op` was undone: it can be redone.
    pub fn undone(&mut self, op: FileOp) {
        if self.undone.len() >= LIMIT {
            self.undone.remove(0);
        }
        self.undone.push(op);
    }

    /// `op` was redone (as given, with a fresh restore token for a delete):
    /// it can be undone again, and what else was undone stays redoable.
    pub fn redone(&mut self, op: FileOp) {
        self.push_done(op);
    }

    /// An undo or redo failed: put `op` back where it was taken from.
    pub fn put_back(&mut self, op: FileOp, was_undo: bool) {
        if was_undo {
            self.push_done(op);
        } else {
            self.undone.push(op);
        }
    }

    /// Forget everything (another workspace).
    pub fn clear(&mut self) {
        self.done.clear();
        self.undone.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(from: &str, to: &str) -> FileOp {
        FileOp::Move {
            root: RootId(0),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn undo_redo_and_a_new_operation() {
        let mut h = History::default();
        h.record(mv("a", "b"));
        h.record(mv("b", "c"));
        let op = h.take_undo().unwrap();
        assert_eq!(op, mv("b", "c"));
        h.undone(op);
        assert_eq!(h.next_redo(), Some(&mv("b", "c")));
        let op = h.take_redo().unwrap();
        h.redone(op);
        assert_eq!(h.next_undo(), Some(&mv("b", "c")));
        let op = h.take_undo().unwrap();
        h.undone(op);
        // A new operation drops what was undone.
        h.record(mv("b", "d"));
        assert_eq!(h.next_redo(), None);
        for i in 0..LIMIT + 5 {
            h.record(mv(&i.to_string(), "x"));
        }
        assert_eq!(h.done.len(), LIMIT, "bounded");
    }

    #[test]
    fn descriptions() {
        assert_eq!(
            mv("src/a.rs", "src/b.rs").describe(),
            "rename of “a.rs” → “b.rs”"
        );
        assert_eq!(
            mv("src/a.rs", "lib/a.rs").describe(),
            "move of “a.rs” to “lib”"
        );
        assert_eq!(
            mv("src/a.rs", "a.rs").describe(),
            "move of “a.rs” to “the top folder”"
        );
        let gone = FileOp::Delete {
            root: RootId(0),
            path: "a.rs".into(),
            kind: EntryKind::File,
            restore: Err("“a.rs” was deleted permanently.".into()),
        };
        assert_eq!(gone.describe(), "delete of “a.rs”");
        assert_eq!(gone.cannot_undo(), Some("“a.rs” was deleted permanently."));
        let made = FileOp::Create {
            root: RootId(0),
            path: "docs/a.md".into(),
            kind: EntryKind::File,
        };
        assert_eq!(made.describe(), "creation of “a.md”");
        let copied = FileOp::Copy {
            root: RootId(0),
            from: "docs/a.md".into(),
            path: "docs/a copy.md".into(),
            kind: EntryKind::File,
        };
        assert_eq!(copied.describe(), "copy of “a copy.md”");
        assert_eq!(copied.made().map(|m| m.1), Some("docs/a copy.md"));
        assert_eq!(copied.cannot_undo(), None);
        assert_eq!(made.cannot_undo(), None);
    }
}
