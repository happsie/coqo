// SPDX-License-Identifier: Apache-2.0

//! Questions, notes and the user's threads follow a rename or move of their
//! file or a folder above it (issue #288, [`crate::path_moves`]).
//!
//! Every question of the workspace with an anchor at or under the old path
//! (open or closed) gets the new path in its anchor as asked and as last
//! rebased, is stored, and is published again on `questions/changes` with
//! its anchors resolved where they are now.

use std::sync::Arc;

use coqo_protocol::methods::questions::QuestionId;
use futures_util::future::BoxFuture;

use super::{QuestionService, lock};
use crate::path_moves::{FollowsMoves, PathMove};

impl QuestionService {
    /// Re-key the stored and tracked anchors `mv` touches. Returns the
    /// questions it changed.
    fn rekey(&self, mv: &PathMove) -> Vec<QuestionId> {
        let mut touched = Vec::new();
        let mut moved = Vec::new();
        {
            let inner = lock(&self.inner);
            let records = match inner.store.all_of(&mv.workspace) {
                Ok(records) => records,
                Err(e) => {
                    tracing::warn!(workspace = %mv.workspace, error = %e, "cannot list questions to follow a rename");
                    return Vec::new();
                }
            };
            for mut r in records {
                let mut changed = false;
                for (ai, a) in r.anchors.iter_mut().enumerate() {
                    let mut hit = mv.rekey(a.root, &mut a.anchor.path);
                    if let Some(current) = &mut a.current {
                        hit |= mv.rekey(a.root, &mut current.path);
                    }
                    if hit {
                        touched.push((r.id, ai));
                        changed = true;
                    }
                }
                if !changed {
                    continue;
                }
                match inner.store.save_anchors(&r) {
                    Ok(()) => moved.push(r.id),
                    Err(e) => {
                        tracing::warn!(question = %r.id, error = %e, "cannot move a question's anchor");
                    }
                }
            }
        }
        let mut live = lock(&self.live);
        for key in &touched {
            if moved.contains(&key.0)
                && let Some(t) = live.tracked.get_mut(key)
            {
                mv.rekey(mv.root, &mut t.anchor.path);
            }
        }
        moved
    }

    /// Publish question `id` again as stored, its anchors resolved now.
    async fn republish(self: &Arc<Self>, id: QuestionId) {
        let Ok(record) = self.record(id) else {
            return;
        };
        let mut records = vec![record];
        let resolutions = self.resolve(&mut records).await.remove(0);
        if let Err(e) = self.publish(id, resolutions, |_| Ok(())) {
            tracing::warn!(question = %id, error = %e.message, "cannot publish a moved question");
        }
    }
}

impl FollowsMoves for QuestionService {
    fn path_moved(self: Arc<Self>, mv: PathMove) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            for id in self.rekey(&mv) {
                self.republish(id).await;
            }
        })
    }
}
