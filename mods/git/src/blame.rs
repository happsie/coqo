// SPDX-License-Identifier: Apache-2.0

//! `git blame`: which commit last changed each line, from gix.

use std::collections::BTreeMap;

use gix::bstr::{BStr, ByteSlice as _};

use crate::{ErrorKind, GitError, Repo, Result, check_path};

/// Consecutive lines last changed by one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameHunk {
    /// 1-based first line in the blamed file.
    pub start: u32,
    /// Lines in the hunk.
    pub lines: u32,
    /// 1-based first line in the file as that commit left it.
    pub orig_start: u32,
    /// The commit.
    pub oid: String,
    /// The file's path in that commit, if it was renamed since.
    pub orig_path: Option<String>,
}

/// A commit that blame points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameCommit {
    /// Its id.
    pub oid: String,
    /// The author's name.
    pub author: String,
    /// The author's email.
    pub email: String,
    /// The author time, in Unix seconds.
    pub time: i64,
    /// The message's first line.
    pub summary: String,
}

/// A file's blame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blame {
    /// The revision blamed (its commit id).
    pub rev: String,
    /// Lines in the file.
    pub line_count: u32,
    /// Hunks in line order, covering every line.
    pub hunks: Vec<BlameHunk>,
    /// The commits the hunks name, by id.
    pub commits: BTreeMap<String, BlameCommit>,
}

impl Repo {
    /// `git blame rev -- path` (`rev` defaults to `HEAD`), following
    /// renames. Uncommitted changes are not blamed.
    pub fn blame(&self, path: &str, rev: Option<&str>) -> Result<Blame> {
        check_path(path)?;
        let repo = self.gix();
        let rev = rev.unwrap_or("HEAD");
        let commit = repo
            .rev_parse_single(rev)
            .map_err(GitError::other)
            .and_then(|id| id.object().map_err(GitError::other))
            .and_then(|o| o.peel_to_commit().map_err(GitError::other))
            .map_err(|e| {
                GitError::new(
                    ErrorKind::NotFound,
                    format!("unknown revision {rev:?}: {e}"),
                )
            })?;
        let options = gix::repository::blame_file::Options {
            // git's default; the configured `diff.algorithm` does not apply
            // to blame.
            diff_algorithm: Some(gix::diff::blob::Algorithm::Myers),
            rewrites: Some(gix::diff::Rewrites::default()),
            ..Default::default()
        };
        let outcome = repo
            .blame_file(BStr::new(path), commit.id, options)
            .map_err(|e| {
                let msg = e.to_string();
                if commit
                    .tree()
                    .ok()
                    .and_then(|t| t.lookup_entry_by_path(path).ok().flatten())
                    .is_none()
                {
                    GitError::new(ErrorKind::NotFound, format!("{path} is not in {rev}"))
                } else {
                    GitError::other(msg)
                }
            })?;
        let line_count = outcome.blob.lines_with_terminator().count() as u32;
        let mut commits = BTreeMap::new();
        let mut hunks = Vec::new();
        for e in outcome.entries {
            let oid = e.commit_id.to_string();
            if !commits.contains_key(&oid) {
                let c = repo.find_commit(e.commit_id).map_err(GitError::other)?;
                let (author, email, time) = match c.author() {
                    Ok(a) => (
                        a.name.to_str_lossy().trim().to_owned(),
                        a.email.to_str_lossy().trim().to_owned(),
                        a.seconds(),
                    ),
                    Err(_) => Default::default(),
                };
                let summary = c
                    .message()
                    .map(|m| m.summary().to_string())
                    .unwrap_or_default();
                commits.insert(
                    oid.clone(),
                    BlameCommit {
                        oid: oid.clone(),
                        author,
                        email,
                        time,
                        summary,
                    },
                );
            }
            hunks.push(BlameHunk {
                start: e.start_in_blamed_file + 1,
                lines: e.len.get(),
                orig_start: e.start_in_source_file + 1,
                oid,
                orig_path: e.source_file_name.map(|p| p.to_str_lossy().into_owned()),
            });
        }
        Ok(Blame {
            rev: commit.id.to_string(),
            line_count,
            hunks,
            commits,
        })
    }
}
