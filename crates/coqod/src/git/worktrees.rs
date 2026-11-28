// SPDX-License-Identifier: Apache-2.0

//! `git/worktrees`, `git/addWorktree` and `git/removeWorktree` (#92, ADR
//! 0028): a root's repository's worktrees, through the git mod (which
//! runs the user's `git worktree`).
//!
//! Each worktree is its own workspace: adding one opens a workspace on its
//! folder (`workspace/open`, so it joins its project on the rail), and
//! removing one forgets the workspaces on it. Removal is never forced and
//! never takes the main worktree.

use std::path::{Path, PathBuf};

use coqo_mod_git::{Removal, Worktree};
use coqo_protocol::ErrorObject;
use coqo_protocol::methods::git::{self as gm, GitChangeKind};
use coqo_protocol::methods::workspace::{OpenParams, WorkspaceId, WorkspaceParams};

use super::GitService;

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn worktree(w: Worktree, changes: Option<usize>) -> gm::Worktree {
    gm::Worktree {
        path: w.path.to_string_lossy().into_owned(),
        branch: w.branch,
        head: w.head,
        main: w.main,
        locked: w.locked,
        prunable: w.prunable,
        changes: changes.map(count),
    }
}

impl GitService {
    /// `git/worktrees`.
    pub async fn worktrees(&self, p: gm::RepoParams) -> Result<gm::WorktreesResult, ErrorObject> {
        self.with_repo(&p.workspace, p.root, |repo| {
            let list = repo.worktrees()?;
            let worktrees = list
                .into_iter()
                .map(|w| {
                    let changes = (!w.prunable)
                        .then(|| repo.worktree_changes(&w.path).ok())
                        .flatten();
                    worktree(w, changes)
                })
                .collect();
            Ok(gm::WorktreesResult {
                worktrees,
                worktrees_dir: repo.worktrees_dir()?.to_string_lossy().into_owned(),
            })
        })
        .await
    }

    /// `git/addWorktree`.
    pub async fn add_worktree(
        &self,
        p: gm::AddWorktreeParams,
    ) -> Result<gm::AddWorktreeResult, ErrorObject> {
        let (branch, base, path) = (p.branch, p.base, p.path.map(PathBuf::from));
        let wt = self
            .with_repo(&p.workspace, p.root, move |repo| {
                repo.add_worktree(&branch, base.as_deref(), path.as_deref())
            })
            .await?;
        self.changed(&p.workspace, p.root, &[GitChangeKind::Refs]);
        let workspace = self
            .workspaces
            .open_workspace(OpenParams {
                path: Some(wt.path.to_string_lossy().into_owned()),
                ..OpenParams::default()
            })
            .await?;
        Ok(gm::AddWorktreeResult {
            worktree: worktree(wt, Some(0)),
            workspace,
        })
    }

    /// `git/removeWorktree`.
    pub async fn remove_worktree(
        &self,
        p: gm::RemoveWorktreeParams,
    ) -> Result<gm::RemoveWorktreeResult, ErrorObject> {
        let (path, confirm) = (PathBuf::from(&p.path), p.confirm);
        let target = path.clone();
        let removal = self
            .with_repo(&p.workspace, p.root, move |repo| {
                repo.remove_worktree(&target, confirm)
            })
            .await?;
        if let Removal::HasWork(work) = removal {
            return Ok(gm::RemoveWorktreeResult {
                removed: false,
                changes: count(work.changes),
                unpushed: count(work.unpushed),
                forgotten: Vec::new(),
            });
        }
        // The folder is gone: so are the workspaces on it.
        let forgotten = self.workspaces_in(&path).await?;
        for id in &forgotten {
            self.workspaces
                .remove(WorkspaceParams { id: id.clone() })
                .await?;
        }
        if !forgotten.contains(&p.workspace) {
            self.changed(&p.workspace, p.root, &[GitChangeKind::Refs]);
        }
        Ok(gm::RemoveWorktreeResult {
            removed: true,
            changes: 0,
            unpushed: 0,
            forgotten,
        })
    }

    /// The workspaces whose first root is in the folder `dir`.
    async fn workspaces_in(&self, dir: &Path) -> Result<Vec<WorkspaceId>, ErrorObject> {
        // The folder may be gone already; compare as git named it.
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        Ok(self
            .workspaces
            .list()
            .await?
            .workspaces
            .into_iter()
            .filter(|w| {
                w.roots
                    .first()
                    .is_some_and(|r| Path::new(&r.path).starts_with(&dir))
            })
            .map(|w| w.id)
            .collect())
    }
}
