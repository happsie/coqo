// SPDX-License-Identifier: Apache-2.0

//! Branches and the stash list, from gix.

use gix::bstr::ByteSlice as _;

use crate::{GitError, Repo, Result};

/// A branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// The short name: `main`, or `origin/main` for a remote branch.
    pub name: String,
    /// A remote-tracking branch (`refs/remotes/…`).
    pub remote: bool,
    /// `HEAD` is on it.
    pub current: bool,
    /// The commit it points at.
    pub oid: String,
    /// Its upstream (local branches), such as `origin/main`.
    pub upstream: Option<String>,
    /// The commit's message's first line.
    pub summary: String,
    /// The commit's committer time, in Unix seconds.
    pub time: i64,
}

/// A stash entry, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    /// `n` of `stash@{n}`.
    pub index: usize,
    /// What `git stash list` shows: `On main: message` or `WIP on main: …`.
    pub message: String,
    /// The stash commit.
    pub oid: String,
    /// When it was made, in Unix seconds.
    pub time: i64,
}

impl Repo {
    /// Local branches, then remote ones, each sorted by name. Symbolic refs
    /// (`origin/HEAD`) are left out.
    pub fn branches(&self) -> Result<Vec<Branch>> {
        let repo = self.gix();
        let head = repo.head_name().map_err(GitError::other)?;
        let refs = repo.references().map_err(GitError::other)?;
        let mut out = Vec::new();
        for (remote, iter) in [
            (false, refs.local_branches().map_err(GitError::other)?),
            (true, refs.remote_branches().map_err(GitError::other)?),
        ] {
            let mut group = Vec::new();
            for r in iter {
                let mut r = r.map_err(GitError::other)?;
                if r.target().try_name().is_some() {
                    continue;
                }
                let name = r.name().shorten().to_str_lossy().into_owned();
                let current = head.as_ref().is_some_and(|h| h.as_ref() == r.name());
                let upstream = if remote {
                    None
                } else {
                    repo.branch_remote_tracking_ref_name(r.name(), gix::remote::Direction::Fetch)
                        .and_then(Result::ok)
                        .map(|u| u.shorten().to_str_lossy().into_owned())
                };
                let id = r.peel_to_id().map_err(GitError::other)?.detach();
                let (summary, time) = match repo.find_commit(id) {
                    Ok(c) => (
                        c.message()
                            .map(|m| m.summary().to_string())
                            .unwrap_or_default(),
                        c.time().map(|t| t.seconds).unwrap_or_default(),
                    ),
                    Err(_) => (String::new(), 0),
                };
                group.push(Branch {
                    name,
                    remote,
                    current,
                    oid: id.to_string(),
                    upstream,
                    summary,
                    time,
                });
            }
            group.sort_by(|a, b| a.name.cmp(&b.name));
            out.extend(group);
        }
        Ok(out)
    }

    /// The stash list, newest (`stash@{0}`) first.
    pub fn stashes(&self) -> Result<Vec<Stash>> {
        let repo = self.gix();
        let Ok(reference) = repo.find_reference("refs/stash") else {
            return Ok(Vec::new());
        };
        let mut platform = reference.log_iter();
        let Some(lines) = platform.all().map_err(GitError::other)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for line in lines {
            let line = line.map_err(GitError::other)?;
            out.push(Stash {
                index: 0,
                message: line.message.to_str_lossy().into_owned(),
                oid: line.new_oid.to_str_lossy().into_owned(),
                time: line.signature.seconds(),
            });
        }
        out.reverse();
        for (i, s) in out.iter_mut().enumerate() {
            s.index = i;
        }
        Ok(out)
    }
}
