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
//! saving writes the file again.
//!
//! All three take the daemon token only. Agents edit files through their
//! own tools and `buffer/*`; creating, moving and deleting whole entries is
//! the user's, from the tree.

use std::sync::Arc;

use coqo_protocol::methods::fs as fsm;
use coqo_protocol::{Access, Router};

use crate::buffers::BufferService;
use crate::workspace::WorkspaceService;

/// Register `fs/create`, `fs/rename` and `fs/delete` on `router`.
pub fn register(
    router: Router,
    workspaces: Arc<WorkspaceService>,
    buffers: Arc<BufferService>,
) -> Router {
    let ws = Arc::clone(&workspaces);
    let router = router.method_with::<fsm::Create, _, _>(Access::daemon(), move |_ctx, p| {
        let ws = Arc::clone(&ws);
        async move { ws.create_entry(p).await }
    });
    let ws = Arc::clone(&workspaces);
    let router = router.method_with::<fsm::Rename, _, _>(Access::daemon(), move |_ctx, p| {
        let (ws, buffers) = (Arc::clone(&ws), Arc::clone(&buffers));
        async move {
            let from = ws
                .entry_path(p.workspace.clone(), p.root, p.from.clone())
                .await?;
            let workspace = p.workspace.clone();
            let renamed = buffers
                .move_files(&workspace, &from, ws.rename_entry(p))
                .await?;
            Ok(fsm::EntryResult { path: renamed.rel })
        }
    });
    router.method_with::<fsm::Delete, _, _>(Access::daemon(), move |_ctx, p| {
        let ws = Arc::clone(&workspaces);
        async move { ws.delete_entry(p).await }
    })
}

#[cfg(test)]
mod tests;
