// SPDX-License-Identifier: Apache-2.0

//! `git status`: the branch and every changed path, from gix.

use std::collections::BTreeMap;

use gix::bstr::ByteSlice as _;
use gix::status::plumbing::index_as_worktree::{self as iw, EntryStatus};

use crate::{GitError, Repo, Result};

/// How a path changed on one side (staged: `HEAD` → index; unstaged:
/// index → work tree).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Change {
    /// New (staged), or `git add --intent-to-add`ed (unstaged).
    Added,
    /// Contents or the executable bit changed.
    Modified,
    /// Gone.
    Deleted,
    /// It became another kind of entry (a file became a symlink, say).
    TypeChanged,
    /// Not tracked (unstaged only).
    Untracked,
}

impl Change {
    /// The `git status --short` letter.
    pub fn letter(self) -> char {
        match self {
            Change::Added => 'A',
            Change::Modified => 'M',
            Change::Deleted => 'D',
            Change::TypeChanged => 'T',
            Change::Untracked => '?',
        }
    }
}

/// An unmerged path, as `git status` describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Conflict {
    /// `DD`.
    BothDeleted,
    /// `AU`.
    AddedByUs,
    /// `UD`.
    DeletedByThem,
    /// `UA`.
    AddedByThem,
    /// `DU`.
    DeletedByUs,
    /// `AA`.
    BothAdded,
    /// `UU`.
    BothModified,
}

impl Conflict {
    /// The two letters of `git status --short`.
    pub fn letters(self) -> &'static str {
        match self {
            Conflict::BothDeleted => "DD",
            Conflict::AddedByUs => "AU",
            Conflict::DeletedByThem => "UD",
            Conflict::AddedByThem => "UA",
            Conflict::DeletedByUs => "DU",
            Conflict::BothAdded => "AA",
            Conflict::BothModified => "UU",
        }
    }
}

/// One changed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    /// Repository-relative. An untracked directory git does not descend
    /// into (a nested repository) ends in `/`.
    pub path: String,
    /// `HEAD` → index.
    pub staged: Option<Change>,
    /// Index → work tree.
    pub unstaged: Option<Change>,
    /// Set for an unmerged path; `staged` and `unstaged` are `None` then.
    pub conflict: Option<Conflict>,
}

/// Where `HEAD` is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
    /// The branch's short name; `None` when detached.
    pub branch: Option<String>,
    /// The commit; `None` on an unborn branch (no commits yet).
    pub oid: Option<String>,
    /// The branch's upstream, such as `origin/main`.
    pub upstream: Option<String>,
    /// Commits on the branch that the upstream lacks.
    pub ahead: u32,
    /// Commits on the upstream that the branch lacks.
    pub behind: u32,
}

/// The repository's status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Where `HEAD` is.
    pub head: Head,
    /// Changed paths, sorted by path. Ignored files are not listed.
    pub entries: Vec<StatusEntry>,
}

fn conflict(c: iw::Conflict) -> Conflict {
    match c {
        iw::Conflict::BothDeleted => Conflict::BothDeleted,
        iw::Conflict::AddedByUs => Conflict::AddedByUs,
        iw::Conflict::DeletedByThem => Conflict::DeletedByThem,
        iw::Conflict::AddedByThem => Conflict::AddedByThem,
        iw::Conflict::DeletedByUs => Conflict::DeletedByUs,
        iw::Conflict::BothAdded => Conflict::BothAdded,
        iw::Conflict::BothModified => Conflict::BothModified,
    }
}

/// Whether the object type (file, symlink, submodule) differs; the
/// executable bit is a modification.
fn kind_changed(a: gix::index::entry::Mode, b: gix::index::entry::Mode) -> bool {
    (a.bits() & 0o170000) != (b.bits() & 0o170000)
}

/// The entry for `path`, made if needed.
fn at<'a>(entries: &'a mut BTreeMap<String, StatusEntry>, path: &[u8]) -> &'a mut StatusEntry {
    let path = path.to_str_lossy().into_owned();
    entries.entry(path.clone()).or_insert_with(|| StatusEntry {
        path,
        staged: None,
        unstaged: None,
        conflict: None,
    })
}

impl Repo {
    /// `git status`: the branch and every changed path.
    pub fn status(&self) -> Result<Status> {
        use gix::status::{UntrackedFiles, tree_index::TrackRenames};
        let repo = self.gix();
        let iter = repo
            .status(gix::progress::Discard)
            .map_err(GitError::other)?
            .untracked_files(UntrackedFiles::Files)
            .index_worktree_rewrites(None)
            .tree_index_track_renames(TrackRenames::Disabled)
            .into_iter(None)
            .map_err(GitError::other)?;
        let mut entries: BTreeMap<String, StatusEntry> = BTreeMap::new();
        for item in iter {
            match item.map_err(GitError::other)? {
                gix::status::Item::TreeIndex(change) => {
                    use gix::diff::index::ChangeRef;
                    match change {
                        ChangeRef::Addition { location, .. } => {
                            at(&mut entries, &location).staged = Some(Change::Added);
                        }
                        ChangeRef::Deletion { location, .. } => {
                            at(&mut entries, &location).staged = Some(Change::Deleted);
                        }
                        ChangeRef::Modification {
                            location,
                            previous_entry_mode,
                            entry_mode,
                            ..
                        } => {
                            at(&mut entries, &location).staged =
                                Some(if kind_changed(previous_entry_mode, entry_mode) {
                                    Change::TypeChanged
                                } else {
                                    Change::Modified
                                });
                        }
                        ChangeRef::Rewrite {
                            source_location,
                            location,
                            ..
                        } => {
                            // Rename tracking is off; be safe anyway.
                            at(&mut entries, &source_location).staged = Some(Change::Deleted);
                            at(&mut entries, &location).staged = Some(Change::Added);
                        }
                    }
                }
                gix::status::Item::IndexWorktree(item) => {
                    use gix::status::index_worktree::Item;
                    match item {
                        Item::Modification {
                            rela_path, status, ..
                        } => match status {
                            EntryStatus::Conflict { summary, .. } => {
                                at(&mut entries, &rela_path).conflict = Some(conflict(summary));
                            }
                            EntryStatus::Change(change) => {
                                at(&mut entries, &rela_path).unstaged = Some(match change {
                                    iw::Change::Removed => Change::Deleted,
                                    iw::Change::Type { .. } => Change::TypeChanged,
                                    iw::Change::Modification { .. }
                                    | iw::Change::SubmoduleModification(_) => Change::Modified,
                                });
                            }
                            EntryStatus::IntentToAdd => {
                                at(&mut entries, &rela_path).unstaged = Some(Change::Added);
                            }
                            EntryStatus::NeedsUpdate(_) => {}
                        },
                        Item::DirectoryContents { entry, .. } => {
                            if entry.status == gix::dir::entry::Status::Untracked {
                                let mut path = entry.rela_path.to_vec();
                                if entry.disk_kind.is_some_and(|k| k.is_dir()) {
                                    path.push(b'/');
                                }
                                at(&mut entries, &path).unstaged = Some(Change::Untracked);
                            }
                        }
                        Item::Rewrite { dirwalk_entry, .. } => {
                            at(&mut entries, &dirwalk_entry.rela_path).unstaged =
                                Some(Change::Untracked);
                        }
                    }
                }
            }
        }
        let entries = entries
            .into_values()
            .map(|mut e| {
                if e.conflict.is_some() {
                    e.staged = None;
                    e.unstaged = None;
                }
                e
            })
            .filter(|e| e.conflict.is_some() || e.staged.is_some() || e.unstaged.is_some())
            .collect();
        Ok(Status {
            head: self.head()?,
            entries,
        })
    }

    /// Where `HEAD` is, with the branch's upstream and how far apart they
    /// are.
    pub fn head(&self) -> Result<Head> {
        let repo = self.gix();
        let name = repo.head_name().map_err(GitError::other)?;
        let oid = repo.head_id().ok().map(|id| id.detach());
        let mut head = Head {
            branch: name
                .as_ref()
                .map(|n| n.shorten().to_str_lossy().into_owned()),
            oid: oid.map(|o| o.to_string()),
            ..Head::default()
        };
        if let Some(name) = &name
            && let Some(Ok(upstream)) =
                repo.branch_remote_tracking_ref_name(name.as_ref(), gix::remote::Direction::Fetch)
            && let Ok(mut reference) = repo.find_reference(upstream.as_ref())
        {
            head.upstream = Some(upstream.shorten().to_str_lossy().into_owned());
            if let (Some(local), Ok(remote)) = (oid, reference.peel_to_id()) {
                let remote = remote.detach();
                head.ahead = count(repo, local, remote)?;
                head.behind = count(repo, remote, local)?;
            }
        }
        Ok(head)
    }
}

/// Commits reachable from `tip` but not from `hidden`.
fn count(repo: &gix::Repository, tip: gix::ObjectId, hidden: gix::ObjectId) -> Result<u32> {
    if tip == hidden {
        return Ok(0);
    }
    let walk = repo
        .rev_walk([tip])
        .with_hidden([hidden])
        .all()
        .map_err(GitError::other)?;
    let mut n = 0u32;
    for info in walk {
        info.map_err(GitError::other)?;
        n = n.saturating_add(1);
    }
    Ok(n)
}
