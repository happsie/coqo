// SPDX-License-Identifier: Apache-2.0

//! The file tree's operations (issue #161): `fs/create`, `fs/rename` and
//! `fs/delete`.
//!
//! The disk side is the workspace service's (same confinement as `fs/list`:
//! paths stay inside the roots, through neither `..` nor a symlink); this
//! module adds what the buffers need. A rename moves the open buffers of the
//! files it moves ([`BufferService::move_files`]), so an editor tab keeps
//! its buffer, text and unsaved edits under the new name. A delete leaves
//! open buffers alone: they keep their text (unsaved edits included), and
//! saving writes the file again. After a rename, everything else anchored
//! to the moved paths follows it too ([`PathMoves`], issue #288).
//!
//! A restored file's open tab finds it again through `fs/changes`, like a
//! file that reappears on disk.
//!
//! All of them take the daemon token only. Agents edit files through their
//! own tools and `buffer/*`; creating, moving and deleting whole entries is
//! the user's, from the tree.

use std::sync::Arc;

use coqo_protocol::methods::fs as fsm;
use coqo_protocol::{Access, Router};

use crate::buffers::BufferService;
use crate::path_moves::{PathMove, PathMoves};
use crate::workspace::WorkspaceService;

/// Register `fs/create`, `fs/rename`, `fs/delete` and `fs/restore` on
/// `router`.
pub fn register(
    router: Router,
    workspaces: Arc<WorkspaceService>,
    buffers: Arc<BufferService>,
    moves: Arc<PathMoves>,
) -> Router {
    let ws = Arc::clone(&workspaces);
    let router = router.method_with::<fsm::Create, _, _>(Access::daemon(), move |_ctx, p| {
        let ws = Arc::clone(&ws);
        async move { ws.create_entry(p).await }
    });
    let ws = Arc::clone(&workspaces);
    let router = router.method_with::<fsm::Rename, _, _>(Access::daemon(), move |_ctx, p| {
        let (ws, buffers, moves) = (Arc::clone(&ws), Arc::clone(&buffers), Arc::clone(&moves));
        async move {
            let (from, old) = ws
                .entry_path(p.workspace.clone(), p.root, p.from.clone())
                .await?;
            let workspace = p.workspace.clone();
            let root = p.root;
            let renamed = buffers
                .move_files(&workspace, &from, ws.rename_entry(p))
                .await?;
            moves
                .moved(PathMove {
                    workspace,
                    root,
                    from: old,
                    to: renamed.rel.clone(),
                })
                .await;
            Ok(fsm::EntryResult { path: renamed.rel })
        }
    });
    let ws = Arc::clone(&workspaces);
    let router = router.method_with::<fsm::CopyEntry, _, _>(Access::daemon(), move |_ctx, p| {
        let ws = Arc::clone(&ws);
        async move { ws.copy_entry(p).await }
    });
    let ws = Arc::clone(&workspaces);
    let router = router.method_with::<fsm::Delete, _, _>(Access::daemon(), move |_ctx, p| {
        let ws = Arc::clone(&ws);
        async move { ws.delete_entry(p).await }
    });
    router.method_with::<fsm::Restore, _, _>(Access::daemon(), move |_ctx, p| {
        let ws = Arc::clone(&workspaces);
        async move { ws.restore_entry(p).await }
    })
}

#[cfg(test)]
mod tests;
