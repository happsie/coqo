// SPDX-License-Identifier: Apache-2.0

//! `git log`, newest first, with the lanes a graph view draws.

use std::collections::HashMap;

use gix::bstr::ByteSlice as _;

use crate::{ErrorKind, GitError, Repo, Result};

/// Most commits in one page.
pub const MAX_PAGE: usize = 1000;

/// What to list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// Start from these revisions (`git log a b`); empty means `HEAD`.
    pub revs: Vec<String>,
    /// Commits to skip, for paging.
    pub skip: usize,
    /// Commits to return (at most [`MAX_PAGE`]).
    pub limit: usize,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            revs: Vec::new(),
            skip: 0,
            limit: 100,
        }
    }
}

/// How a graph line meets a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// From lane `from` at the row's top to lane `to` at its bottom,
    /// passing the commit by.
    Through,
    /// From lane `from` at the row's top into the commit (`to` is the
    /// commit's lane).
    In,
    /// From the commit (`from` is its lane) to lane `to` at the row's
    /// bottom, towards a parent.
    Out,
}

/// A line of the graph within one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphEdge {
    /// The lane it starts in.
    pub from: u32,
    /// The lane it ends in.
    pub to: u32,
    /// How it meets the row.
    pub kind: EdgeKind,
}

/// The graph at one commit's row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRow {
    /// The commit's lane (0 is leftmost).
    pub column: u32,
    /// The lines crossing the row.
    pub edges: Vec<GraphEdge>,
}

/// One commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogCommit {
    /// Its id.
    pub oid: String,
    /// Its parents' ids, first parent first.
    pub parents: Vec<String>,
    /// The author's name.
    pub author: String,
    /// The author's email.
    pub email: String,
    /// The author time, in Unix seconds.
    pub time: i64,
    /// The message's first line.
    pub summary: String,
    /// Branches and tags pointing at it (short names), and `HEAD`.
    pub refs: Vec<String>,
    /// Its row of the graph.
    pub graph: GraphRow,
}

/// A page of [`Repo::log`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPage {
    /// The commits, newest first.
    pub commits: Vec<LogCommit>,
    /// More commits follow this page.
    pub more: bool,
}

/// Lanes: the commit each lane waits for.
#[derive(Debug, Default)]
struct Lanes(Vec<Option<gix::ObjectId>>);

impl Lanes {
    fn free(&mut self, except: usize) -> usize {
        match self
            .0
            .iter()
            .enumerate()
            .position(|(i, l)| l.is_none() && i != except)
        {
            Some(i) => i,
            None => {
                self.0.push(None);
                self.0.len() - 1
            }
        }
    }

    /// The row of `commit` with `parents`, moving the lanes on.
    fn row(&mut self, commit: gix::ObjectId, parents: &[gix::ObjectId]) -> GraphRow {
        let before = self.0.clone();
        let column = match before.iter().position(|l| *l == Some(commit)) {
            Some(i) => i,
            None => self.free(usize::MAX),
        };
        let mut edges = Vec::new();
        for (i, l) in before.iter().enumerate() {
            match l {
                Some(c) if *c == commit => {
                    edges.push(GraphEdge {
                        from: i as u32,
                        to: column as u32,
                        kind: EdgeKind::In,
                    });
                    self.0[i] = None;
                }
                Some(_) => edges.push(GraphEdge {
                    from: i as u32,
                    to: i as u32,
                    kind: EdgeKind::Through,
                }),
                None => {}
            }
        }
        for (n, p) in parents.iter().enumerate() {
            let lane = match self.0.iter().position(|l| *l == Some(*p)) {
                Some(i) => i,
                None if n == 0 => {
                    self.0[column] = Some(*p);
                    column
                }
                None => {
                    let i = self.free(column);
                    self.0[i] = Some(*p);
                    i
                }
            };
            edges.push(GraphEdge {
                from: column as u32,
                to: lane as u32,
                kind: EdgeKind::Out,
            });
        }
        while self.0.last().is_some_and(Option::is_none) {
            self.0.pop();
        }
        GraphRow {
            column: column as u32,
            edges,
        }
    }
}

impl Repo {
    /// `git log revs…`: commits newest first (by committer time), a page at
    /// a time, with graph lanes.
    pub fn log(&self, options: &LogOptions) -> Result<LogPage> {
        let repo = self.gix();
        let limit = options.limit.clamp(1, MAX_PAGE);
        let revs: Vec<&str> = if options.revs.is_empty() {
            vec!["HEAD"]
        } else {
            options.revs.iter().map(String::as_str).collect()
        };
        let mut tips = Vec::new();
        for rev in revs {
            match repo.rev_parse_single(rev) {
                Ok(id) => {
                    let commit = id
                        .object()
                        .and_then(|o| o.peel_to_commit())
                        .map_err(|e| GitError::new(ErrorKind::NotFound, format!("{rev:?}: {e}")))?;
                    tips.push(commit.id);
                }
                // An unborn HEAD has no history yet.
                Err(_) if rev == "HEAD" && repo.head_id().is_err() => {}
                Err(e) => {
                    return Err(GitError::new(
                        ErrorKind::NotFound,
                        format!("unknown revision {rev:?}: {e}"),
                    ));
                }
            }
        }
        if tips.is_empty() {
            return Ok(LogPage {
                commits: Vec::new(),
                more: false,
            });
        }
        let decorations = self.decorations()?;
        let walk = repo
            .rev_walk(tips)
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ))
            .all()
            .map_err(GitError::other)?;
        let mut lanes = Lanes::default();
        let mut commits = Vec::new();
        let mut more = false;
        for (i, info) in walk.enumerate() {
            let info = info.map_err(GitError::other)?;
            if i >= options.skip + limit {
                more = true;
                break;
            }
            let parents: Vec<gix::ObjectId> = info.parent_ids.iter().copied().collect();
            let graph = lanes.row(info.id, &parents);
            if i < options.skip {
                continue;
            }
            let commit = info.object().map_err(GitError::other)?;
            let (author, email, time) = match commit.author() {
                Ok(a) => (
                    a.name.to_str_lossy().trim().to_owned(),
                    a.email.to_str_lossy().trim().to_owned(),
                    a.seconds(),
                ),
                Err(_) => Default::default(),
            };
            let summary = commit
                .message()
                .map(|m| m.summary().to_string())
                .unwrap_or_default();
            commits.push(LogCommit {
                oid: info.id.to_string(),
                parents: parents.iter().map(ToString::to_string).collect(),
                author,
                email,
                time,
                summary,
                refs: decorations.get(&info.id).cloned().unwrap_or_default(),
                graph,
            });
        }
        Ok(LogPage { commits, more })
    }

    /// Short names of the branches and tags at each commit, with `HEAD`
    /// first where it is.
    fn decorations(&self) -> Result<HashMap<gix::ObjectId, Vec<String>>> {
        let repo = self.gix();
        let mut out: HashMap<gix::ObjectId, Vec<String>> = HashMap::new();
        if let Ok(head) = repo.head_id() {
            out.entry(head.detach())
                .or_default()
                .push("HEAD".to_owned());
        }
        let refs = repo.references().map_err(GitError::other)?;
        for iter in [
            refs.local_branches().map_err(GitError::other)?,
            refs.remote_branches().map_err(GitError::other)?,
            refs.tags().map_err(GitError::other)?,
        ] {
            for r in iter {
                let mut r = r.map_err(GitError::other)?;
                if r.target().try_name().is_some() {
                    continue;
                }
                let name = r.name().shorten().to_str_lossy().into_owned();
                let Ok(id) = r.peel_to_id() else { continue };
                out.entry(id.detach()).or_default().push(name);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> gix::ObjectId {
        gix::ObjectId::from_bytes_or_panic(&[n; 20])
    }

    #[test]
    fn lanes_fork_and_merge() {
        // 4 merges 3 (second parent) into 2; 3 and 2 both come from 1.
        let mut lanes = Lanes::default();
        let r4 = lanes.row(id(4), &[id(2), id(3)]);
        assert_eq!(r4.column, 0);
        assert_eq!(
            r4.edges,
            vec![
                GraphEdge {
                    from: 0,
                    to: 0,
                    kind: EdgeKind::Out
                },
                GraphEdge {
                    from: 0,
                    to: 1,
                    kind: EdgeKind::Out
                },
            ]
        );
        let r3 = lanes.row(id(3), &[id(1)]);
        assert_eq!(r3.column, 1);
        assert!(r3.edges.contains(&GraphEdge {
            from: 0,
            to: 0,
            kind: EdgeKind::Through
        }));
        assert!(r3.edges.contains(&GraphEdge {
            from: 1,
            to: 1,
            kind: EdgeKind::In
        }));
        let r2 = lanes.row(id(2), &[id(1)]);
        assert_eq!(r2.column, 0);
        // 2's parent 1 is already awaited in lane 1: the line goes there.
        assert!(r2.edges.contains(&GraphEdge {
            from: 0,
            to: 1,
            kind: EdgeKind::Out
        }));
        let r1 = lanes.row(id(1), &[]);
        assert_eq!(r1.column, 1);
        assert_eq!(
            r1.edges,
            vec![GraphEdge {
                from: 1,
                to: 1,
                kind: EdgeKind::In
            }]
        );
        assert!(lanes.0.is_empty());
    }
}
