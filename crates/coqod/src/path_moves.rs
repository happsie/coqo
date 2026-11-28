// SPDX-License-Identifier: Apache-2.0

//! Things anchored to a file follow it when it is renamed or moved (issue
//! #288).
//!
//! Questions and the user's threads, review threads, walkthrough steps and
//! their coverage, and the editor context UI windows published are all
//! keyed by a root-relative path. Each of those services implements
//! [`FollowsMoves`] and is subscribed to the daemon's [`PathMoves`]. A
//! [`PathMove`] reaches them from two places:
//!
//! - **`fs/rename`** ([`crate::file_ops`]), after the entry moved and its
//!   open buffers followed: [`PathMoves::moved`] runs every hook before the
//!   call returns, so a client that hears the rename's result already finds
//!   the items on the new path.
//! - **The file watcher**, for a rename made outside Coqo that the platform
//!   reports as one (inotify pairs the two halves of a rename; so does a
//!   `From` event followed directly by its `To`): [`PathMoves::seen`]. The
//!   watcher also reports Coqo's own renames; those were applied already
//!   and are skipped.
//!
//! A folder's move is one [`PathMove`]: everything under `from` goes under
//! `to`. Each hook stores the new paths and publishes the changed items on
//! its changes topic, so open UIs follow live. A hook never fails the
//! rename: problems are logged, and an item that could not be moved keeps
//! its old path, where it shows as orphaned (its code is not found), never
//! lost.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use futures_util::future::BoxFuture;
use tokio::sync::broadcast::error::RecvError;

use crate::workspace::WorkspaceService;

/// How long a rename made by `fs/rename` is remembered, so the watcher's
/// report of it is not applied twice.
const OWN_RENAME_WINDOW: Duration = Duration::from_secs(10);
/// Own renames remembered at most.
const OWN_RENAMES: usize = 256;

/// A file or folder that moved within one root. Paths are root-relative,
/// `/`-separated and normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMove {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root both paths are relative to.
    pub root: RootId,
    /// Where it was.
    pub from: String,
    /// Where it is now.
    pub to: String,
}

impl PathMove {
    /// Where `path` (relative to `root`) is after this move: `None` if the
    /// move did not touch it (another root, or not at or under `from`).
    pub fn apply(&self, root: RootId, path: &str) -> Option<String> {
        if root != self.root || self.from.is_empty() || self.from == self.to {
            return None;
        }
        let rest = path.strip_prefix(self.from.as_str())?;
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        Some(format!("{}{rest}", self.to))
    }

    /// Move `path` in place if this move touches it. Returns whether it did.
    pub fn rekey(&self, root: RootId, path: &mut String) -> bool {
        match self.apply(root, path) {
            Some(moved) => {
                *path = moved;
                true
            }
            None => false,
        }
    }

    /// The same move, undone.
    #[must_use]
    pub fn reversed(&self) -> Self {
        Self {
            workspace: self.workspace.clone(),
            root: self.root,
            from: self.to.clone(),
            to: self.from.clone(),
        }
    }
}

/// A service whose records are keyed by path and follow a [`PathMove`].
pub trait FollowsMoves: Send + Sync + std::fmt::Debug {
    /// Re-key everything at or under `mv.from` to `mv.to`, store it and
    /// publish what changed. Must not fail: log problems instead.
    fn path_moved(self: Arc<Self>, mv: PathMove) -> BoxFuture<'static, ()>;
}

/// The daemon's path-move hooks. See the module docs.
#[derive(Debug, Default)]
pub struct PathMoves {
    hooks: Mutex<Vec<Arc<dyn FollowsMoves>>>,
    /// Renames `fs/rename` applied, for the watcher to skip.
    own: Mutex<VecDeque<(PathMove, Instant)>>,
}

impl PathMoves {
    /// An empty set of hooks.
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Subscribe `hook` to every move.
    pub fn follow(&self, hook: Arc<dyn FollowsMoves>) {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hook);
    }

    async fn run(&self, mv: &PathMove) {
        let hooks = self
            .hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for hook in hooks {
            hook.path_moved(mv.clone()).await;
        }
    }

    /// `fs/rename` moved `mv.from` to `mv.to`: run every hook.
    pub async fn moved(&self, mv: PathMove) {
        if mv.from == mv.to {
            return;
        }
        {
            let mut own = self.own.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            own.retain(|(_, at)| now.duration_since(*at) < OWN_RENAME_WINDOW);
            if own.len() >= OWN_RENAMES {
                own.pop_front();
            }
            own.push_back((mv.clone(), now));
        }
        tracing::debug!(workspace = %mv.workspace, from = %mv.from, to = %mv.to, "anchored items follow a rename");
        self.run(&mv).await;
    }

    /// The watcher saw `mv.from` renamed to `mv.to`: run every hook, unless
    /// it was `fs/rename`'s own (applied already).
    pub async fn seen(&self, mv: PathMove) {
        if mv.from == mv.to {
            return;
        }
        {
            let mut own = self.own.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            own.retain(|(_, at)| now.duration_since(*at) < OWN_RENAME_WINDOW);
            if let Some(i) = own.iter().position(|(m, _)| *m == mv) {
                own.remove(i);
                return;
            }
        }
        tracing::debug!(workspace = %mv.workspace, from = %mv.from, to = %mv.to, "anchored items follow a rename made outside Coqo");
        self.run(&mv).await;
    }
}

/// Apply the renames `workspaces`' file watchers see (made outside Coqo)
/// through `moves`, for as long as the daemon runs. Must run inside a Tokio
/// runtime.
pub fn follow_outside_renames(workspaces: &WorkspaceService, moves: Arc<PathMoves>) {
    let mut seen = workspaces.renames_seen();
    tokio::spawn(async move {
        loop {
            match seen.recv().await {
                Ok(mv) => moves.seen(mv).await,
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "renames seen by the watcher were dropped");
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests;
