// SPDX-License-Identifier: Apache-2.0

//! Review threads follow a rename or move of their file or a folder above
//! it (issue #288, [`crate::path_moves`]).
//!
//! Only threads on the work tree's version of a file follow: a rename moves
//! the file on disk, while the index and every revision still have it at
//! its old path, so a thread on one of those keeps resolving there. A moved
//! thread is stored and published again on `threads/changes`.

use std::sync::Arc;

use coqo_protocol::methods::git::DiffSide;
use coqo_protocol::methods::threads::ThreadId;
use futures_util::future::BoxFuture;

use super::{ThreadService, lock};
use crate::path_moves::{FollowsMoves, PathMove};

impl ThreadService {
    /// Re-key the stored work-tree threads `mv` touches. Returns them.
    fn rekey(&self, mv: &PathMove) -> Vec<ThreadId> {
        let inner = lock(&self.inner);
        let records = match inner.store.list(&mv.workspace) {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!(workspace = %mv.workspace, error = %e, "cannot list threads to follow a rename");
                return Vec::new();
            }
        };
        let mut moved = Vec::new();
        for mut r in records {
            let a = &mut r.anchor;
            if a.version != DiffSide::Worktree || !mv.rekey(a.root, &mut a.path) {
                continue;
            }
            let path = a.path.clone();
            for anchor in [&mut a.anchor, &mut a.current].into_iter().flatten() {
                anchor.path.clone_from(&path);
            }
            match inner.store.save_anchor(&r) {
                Ok(()) => moved.push(r.id),
                Err(e) => tracing::warn!(thread = %r.id, error = %e, "cannot move a thread"),
            }
        }
        moved
    }
}

impl FollowsMoves for ThreadService {
    fn path_moved(self: Arc<Self>, mv: PathMove) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for id in self.rekey(&mv) {
                if let Err(e) = self.change(id, |_, _| Ok(())).await {
                    tracing::warn!(thread = %id, error = %e.message, "cannot publish a moved thread");
                }
            }
        })
    }
}
