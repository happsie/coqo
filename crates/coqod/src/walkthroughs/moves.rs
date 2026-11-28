// SPDX-License-Identifier: Apache-2.0

//! Walkthroughs follow a rename or move of a file they anchor or a folder
//! above it (issue #288, [`crate::path_moves`]).
//!
//! Step anchors, trivial hunks and the uncovered hunks of the coverage get
//! the new path; the walkthrough is stored, written again to its committed
//! file if it has one, and announced on `walkthroughs/changes`.

use std::sync::Arc;

use coqo_protocol::methods::walkthroughs::{ChangeKind, StorageMode, Walkthrough};
use coqo_protocol::methods::workspace::WorkspaceParams;
use futures_util::future::BoxFuture;

use super::{WalkthroughService, lock};
use crate::path_moves::{FollowsMoves, PathMove};

/// Re-key every path of `w` that `mv` touches. Returns whether any moved.
fn rekey(w: &mut Walkthrough, mv: &PathMove) -> bool {
    let mut hit = false;
    for a in w.steps.iter_mut().flat_map(|s| s.anchors.iter_mut()) {
        hit |= mv.rekey(a.anchor.root, &mut a.anchor.path);
    }
    for t in &mut w.trivial_hunks {
        hit |= mv.rekey(t.root, &mut t.path);
    }
    if let Some(c) = &mut w.coverage {
        for u in &mut c.uncovered {
            hit |= mv.rekey(u.root, &mut u.path);
        }
    }
    hit
}

impl WalkthroughService {
    /// Re-key and store the walkthroughs `mv` touches. Returns them.
    fn rekey(&self, mv: &PathMove) -> Vec<Walkthrough> {
        let store = lock(&self.store);
        let all = match store.all_of(&mv.workspace) {
            Ok(all) => all,
            Err(e) => {
                tracing::warn!(workspace = %mv.workspace, error = %e, "cannot list walkthroughs to follow a rename");
                return Vec::new();
            }
        };
        all.into_iter()
            .filter_map(|mut w| {
                if !rekey(&mut w, mv) {
                    return None;
                }
                match store.replace_document(&w) {
                    Ok(true) => Some(w),
                    Ok(false) => None,
                    Err(e) => {
                        tracing::warn!(walkthrough = %w.id, error = %e, "cannot move a walkthrough");
                        None
                    }
                }
            })
            .collect()
    }
}

impl FollowsMoves for WalkthroughService {
    fn path_moved(self: Arc<Self>, mv: PathMove) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for w in self.rekey(&mv) {
                if w.storage.mode == StorageMode::Committed
                    && w.storage.path.is_some()
                    && let Ok(ws) = self
                        .workspaces
                        .get(WorkspaceParams {
                            id: w.workspace.clone(),
                        })
                        .await
                {
                    self.commit_file(&ws, &w).await;
                }
                self.publish(ChangeKind::Updated, &w);
            }
        })
    }
}
