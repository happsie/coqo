// SPDX-License-Identifier: Apache-2.0

//! What the file tree's operations did (issue #161), for the views that
//! follow files around.
//!
//! The tree sends `fs/create`, `fs/rename` and `fs/delete` and, when one
//! succeeds, announces it on the app's [`FileEvents`]. File tabs in every
//! window follow a move (their path, title and key change; the daemon moved
//! their buffer already) and react to a delete (a clean tab shows "File not
//! found", a tab with unsaved edits keeps them). Each window saves its
//! layout and republishes the editor context agents see.

use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_ui::gpui::{App, AppContext as _, Entity, EventEmitter, Global};

/// A file operation that succeeded. Paths are root-relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// `from` (a file or a folder) is now `to`.
    Moved {
        workspace: WorkspaceId,
        root: RootId,
        from: String,
        to: String,
    },
    /// `path` (a file or a folder) was deleted.
    Deleted {
        workspace: WorkspaceId,
        root: RootId,
        path: String,
    },
    /// `path` (a file or a folder) was restored: a delete was undone.
    Restored {
        workspace: WorkspaceId,
        root: RootId,
        path: String,
    },
}

/// The app's file-operation announcements. See the module docs.
#[derive(Debug, Default)]
pub struct FileEvents;

impl EventEmitter<FileEvent> for FileEvents {}

struct GlobalFileEvents(Entity<FileEvents>);

impl Global for GlobalFileEvents {}

impl FileEvents {
    /// Set up the app's announcements. Called by [`super::init`].
    pub fn init(cx: &mut App) {
        let events = cx.new(|_| FileEvents);
        cx.set_global(GlobalFileEvents(events));
    }

    /// The app's announcements, once [`init`](Self::init) ran.
    pub fn global(cx: &App) -> Option<Entity<FileEvents>> {
        cx.try_global::<GlobalFileEvents>().map(|g| g.0.clone())
    }

    /// Announce `event`.
    pub fn announce(event: FileEvent, cx: &mut App) {
        if let Some(events) = Self::global(cx) {
            events.update(cx, |_, cx| cx.emit(event));
        }
    }
}

/// Whether `path` is `entry` or inside it (`""` is the root, which holds
/// everything).
pub fn is_at_or_under(path: &str, entry: &str) -> bool {
    entry.is_empty()
        || path == entry
        || (path.len() > entry.len()
            && path.starts_with(entry)
            && path.as_bytes()[entry.len()] == b'/')
}

/// Where `path` is after `from` moved to `to`: `None` if the move didn't
/// touch it.
pub fn moved_path(path: &str, from: &str, to: &str) -> Option<String> {
    if from.is_empty() || !is_at_or_under(path, from) {
        return None;
    }
    Some(format!("{to}{}", &path[from.len()..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_a_move() {
        assert_eq!(moved_path("a.rs", "a.rs", "b.rs").as_deref(), Some("b.rs"));
        assert_eq!(
            moved_path("src/x/y.rs", "src", "lib").as_deref(),
            Some("lib/x/y.rs")
        );
        assert_eq!(moved_path("srcx/y.rs", "src", "lib"), None);
        assert_eq!(moved_path("other.rs", "src", "lib"), None);
        assert_eq!(moved_path("a.rs", "", "x"), None);
        assert!(is_at_or_under("src/a", "src"));
        assert!(is_at_or_under("src", "src"));
        assert!(is_at_or_under("anything", ""));
        assert!(!is_at_or_under("srcx", "src"));
    }
}
