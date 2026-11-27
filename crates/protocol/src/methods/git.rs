// SPDX-License-Identifier: Apache-2.0

//! `git/*`: everyday git on a workspace root's repository (PLAN §7.7), from
//! the `coqo.git` mod.
//!
//! Every method names a workspace root; the daemon opens the git
//! repository whose work tree contains it. **Paths are relative to the
//! repository's work tree**, `/`-separated (`git status` paths), not to the
//! root: [`StatusResult::repo`] says where the root is within the work tree
//! ([`RepoInfo::prefix`]), so a file tree can map them.
//!
//! - Reads: [`Status`], [`Diff`] (one file's hunks between two
//!   [`DiffSide`]s), [`DiffFiles`] (which files differ), [`Blame`], [`Log`]
//!   (paged, with graph lanes), [`Branches`], [`StashList`].
//! - Writes, through the user's `git`: [`Stage`] and [`Unstage`] (whole
//!   files, or chosen lines of one file via `git apply --cached`),
//!   [`Commit`], [`SwitchBranch`], [`CreateBranch`], [`StashPush`],
//!   [`StashPop`], [`StashApply`], [`StashDrop`].
//! - [`Changes`]: what may have changed (work tree, index, `HEAD`, refs,
//!   stash), from the workspace watcher and a watch on the git directory.
//!   Events carry no data: fetch again what the view shows.
//!
//! Errors: [`NOT_A_REPOSITORY`](super::codes::NOT_A_REPOSITORY),
//! [`STALE_SELECTION`](super::codes::STALE_SELECTION),
//! [`GIT_FAILED`](super::codes::GIT_FAILED) (with git's own message),
//! [`NOT_FOUND`](super::codes::NOT_FOUND) for unknown paths and
//! revisions, and `INVALID_PARAMS`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, Topic};

/// Params naming a workspace root, whose repository the method acts on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RepoParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root; its repository is the one whose work tree contains it.
    pub root: RootId,
}

/// Where a root's repository is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RepoInfo {
    /// The work tree's top folder, absolute.
    pub workdir: String,
    /// The root's path within the work tree (`""` when the root is the
    /// top). A git path `p` is the root-relative path `p` minus
    /// `prefix + "/"`.
    pub prefix: String,
}

/// How a path changed on one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum FileChange {
    /// New (staged), or added with `--intent-to-add` (unstaged).
    Added,
    /// Contents or the executable bit changed.
    Modified,
    /// Gone.
    Deleted,
    /// It became another kind of entry (a file became a symlink).
    TypeChanged,
    /// Not tracked (unstaged only).
    Untracked,
}

impl FileChange {
    /// The `git status --short` letter: `A`, `M`, `D`, `T` or `?`.
    pub fn letter(self) -> char {
        match self {
            FileChange::Added => 'A',
            FileChange::Modified => 'M',
            FileChange::Deleted => 'D',
            FileChange::TypeChanged => 'T',
            FileChange::Untracked => '?',
        }
    }
}

/// An unmerged path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
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

/// One changed path of [`Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StatusEntry {
    /// Repository-relative. An untracked folder git does not look into (a
    /// nested repository) ends in `/`.
    pub path: String,
    /// `HEAD` → index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<FileChange>,
    /// Index → work tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstaged: Option<FileChange>,
    /// Set for an unmerged path (then `staged` and `unstaged` are absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Conflict>,
}

impl StatusEntry {
    /// One letter for a file tree: `U` (conflict), else the unstaged
    /// change's letter, else the staged one's.
    pub fn letter(&self) -> Option<char> {
        if self.conflict.is_some() {
            return Some('U');
        }
        self.unstaged.or(self.staged).map(FileChange::letter)
    }
}

/// Where `HEAD` is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Head {
    /// The branch's short name; absent when `HEAD` is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit; absent on a branch with no commits yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// The branch's upstream, such as `origin/main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Commits the upstream lacks.
    pub ahead: u32,
    /// Commits on the upstream the branch lacks.
    pub behind: u32,
}

/// Result of [`Status`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StatusResult {
    /// Where the repository is.
    pub repo: RepoInfo,
    /// Where `HEAD` is.
    pub head: Head,
    /// Changed paths, sorted. Untracked files are listed one by one;
    /// ignored files are not listed; renames show as a deletion and an
    /// addition.
    pub entries: Vec<StatusEntry>,
}

/// One side of a diff.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum DiffSide {
    /// The files on disk: `"worktree"`.
    Worktree,
    /// The index (what the next commit gets): `"index"`.
    Index,
    /// A revision: `{"rev": "HEAD"}`, a branch, a commit id, `main~2`, …
    Rev(String),
}

impl DiffSide {
    /// `HEAD`.
    pub fn head() -> Self {
        DiffSide::Rev("HEAD".to_owned())
    }
}

/// Params of [`Diff`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiffParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// The file, repository-relative.
    pub path: String,
    /// The old side. Unstaged changes: `index` → `worktree`; staged ones:
    /// `{"rev": "HEAD"}` → `index`.
    pub base: DiffSide,
    /// The new side.
    pub head: DiffSide,
    /// Lines of context around changes (default 3, at most 10 000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
}

/// A file as one diff side has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileSide {
    /// Git's mode: `33188` (`0o100644`), `33261` (`0o100755`), `40960`
    /// (`0o120000`, a symlink) or `57344` (`0o160000`, a submodule).
    pub mode: u32,
    /// The blob id; absent for a work-tree file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
}

/// What a diff line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum LineKind {
    /// On both sides.
    Context,
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Removed,
}

/// One line of a [`Hunk`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    /// What it is.
    pub kind: LineKind,
    /// 1-based line on the old side (context and removed lines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_line: Option<u32>,
    /// 1-based line on the new side (context and added lines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_line: Option<u32>,
    /// The text, without its line break.
    pub text: String,
    /// `false` for a last line without a line break ("\ No newline at end
    /// of file").
    pub newline: bool,
}

/// A run of changes with context, as `git diff` groups them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    /// First old line (the one before, when `old_lines` is 0).
    pub old_start: u32,
    /// Old lines in the hunk.
    pub old_lines: u32,
    /// First new line (the one before, when `new_lines` is 0).
    pub new_start: u32,
    /// New lines in the hunk.
    pub new_lines: u32,
    /// The heading after `@@` (the enclosing function, roughly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// The lines.
    pub lines: Vec<DiffLine>,
}

/// Result of [`Diff`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileDiff {
    /// The file, repository-relative.
    pub path: String,
    /// The old side's file; absent for an added file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<FileSide>,
    /// The new side's file; absent for a deleted file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<FileSide>,
    /// Either side is binary (then there are no hunks).
    pub binary: bool,
    /// The changes; empty when the contents are equal.
    pub hunks: Vec<Hunk>,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
}

/// Params of [`DiffFiles`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiffFilesParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// The old side.
    pub base: DiffSide,
    /// The new side.
    pub head: DiffSide,
}

/// A file that differs, in [`DiffFilesResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangedFile {
    /// Repository-relative.
    pub path: String,
    /// `added`, `modified`, `deleted` or `typeChanged`.
    pub change: FileChange,
    /// Added lines (0 for binary files).
    pub additions: u32,
    /// Removed lines (0 for binary files).
    pub deletions: u32,
    /// Either side is binary.
    pub binary: bool,
}

/// Result of [`DiffFiles`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiffFilesResult {
    /// The files, sorted by path (`git diff --numstat`). Untracked files
    /// are not on the `worktree` side.
    pub files: Vec<ChangedFile>,
}

/// Lines of one file's diff, by their numbers in it. Choosing a whole hunk
/// is choosing all its removed and added lines.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct LineSelection {
    /// Old-side numbers of removed lines.
    #[serde(default)]
    pub removed: Vec<u32>,
    /// New-side numbers of added lines.
    #[serde(default)]
    pub added: Vec<u32>,
}

/// Params of [`Stage`] and [`Unstage`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StageParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// Repository-relative paths. With `lines`, exactly one.
    pub paths: Vec<String>,
    /// Only these lines of the file's diff: for [`Stage`] the unstaged
    /// diff (`index` → `worktree`), for [`Unstage`] the staged one
    /// (`HEAD` → `index`), both with 3 lines of context. Lines that are not
    /// in the current diff fail with
    /// [`STALE_SELECTION`](super::codes::STALE_SELECTION).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineSelection>,
}

/// Params of [`Commit`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CommitParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// The message. May be empty only when amending (keeps the old one).
    pub message: String,
    /// Amend the last commit instead.
    #[serde(default)]
    pub amend: bool,
}

/// Result of [`Commit`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CommitResult {
    /// The new commit.
    pub oid: String,
    /// Its message's first line.
    pub summary: String,
}

/// A branch, in [`BranchesResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Branch {
    /// Short name: `main`, or `origin/main` for a remote branch.
    pub name: String,
    /// A remote-tracking branch.
    pub remote: bool,
    /// `HEAD` is on it.
    pub current: bool,
    /// The commit it points at.
    pub oid: String,
    /// Its upstream (local branches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The commit's message's first line.
    pub summary: String,
    /// The commit's time, in Unix seconds.
    pub time: i64,
}

/// Result of [`Branches`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct BranchesResult {
    /// Local branches, then remote ones, each by name.
    pub branches: Vec<Branch>,
}

/// Params of [`SwitchBranch`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SwitchBranchParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// A local branch, or a remote branch's name without the remote (git
    /// then creates a tracking branch).
    pub name: String,
}

/// Params of [`CreateBranch`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CreateBranchParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// The new branch.
    pub name: String,
    /// Where it starts (default `HEAD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Switch to it.
    #[serde(default)]
    pub switch: bool,
}

/// A stash entry, in [`StashListResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Stash {
    /// `n` of `stash@{n}`.
    pub index: u32,
    /// As `git stash list` shows it: `On main: message`.
    pub message: String,
    /// The stash commit.
    pub oid: String,
    /// When it was made, in Unix seconds.
    pub time: i64,
}

/// Result of [`StashList`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StashListResult {
    /// Newest (`stash@{0}`) first.
    pub stashes: Vec<Stash>,
}

/// Params of [`StashPush`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct StashPushParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// A message (default: git's `WIP on …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Stash untracked files too.
    #[serde(default)]
    pub include_untracked: bool,
}

/// Result of [`StashPush`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StashPushResult {
    /// `false` if there was nothing to stash.
    pub created: bool,
}

/// Params of [`StashPop`], [`StashApply`] and [`StashDrop`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StashParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// `n` of `stash@{n}`.
    pub index: u32,
}

/// Params of [`Blame`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct BlameParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// The file, repository-relative.
    pub path: String,
    /// The revision to blame (default `HEAD`). Uncommitted changes are not
    /// blamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

/// Consecutive lines last changed by one commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BlameHunk {
    /// 1-based first line in the file.
    pub start: u32,
    /// Lines in the hunk.
    pub lines: u32,
    /// 1-based first line in the file as that commit left it.
    pub orig_start: u32,
    /// The commit (a key of [`BlameResult::commits`]).
    pub oid: String,
    /// The file's path in that commit, if it was renamed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
}

/// A commit blame points at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CommitInfo {
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

/// Result of [`Blame`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct BlameResult {
    /// The commit blamed.
    pub rev: String,
    /// Lines in the file at that commit.
    pub line_count: u32,
    /// Hunks in line order, covering every line.
    pub hunks: Vec<BlameHunk>,
    /// The commits the hunks name, by id.
    pub commits: BTreeMap<String, CommitInfo>,
}

/// Params of [`Log`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct LogParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root.
    pub root: RootId,
    /// Start from these revisions (`git log a b`); empty for `HEAD`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revs: Vec<String>,
    /// Commits to skip (paging).
    #[serde(default)]
    pub skip: u32,
    /// Commits to return (default 100, at most 1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// How a graph line meets a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum EdgeKind {
    /// From lane `from` at the row's top to lane `to` at its bottom,
    /// passing the commit by.
    Through,
    /// From lane `from` at the row's top into the commit (in lane `to`).
    In,
    /// From the commit (in lane `from`) to lane `to` at the row's bottom,
    /// towards a parent.
    Out,
}

/// A graph line within one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct GraphEdge {
    /// The lane it starts in.
    pub from: u32,
    /// The lane it ends in.
    pub to: u32,
    /// How it meets the row.
    pub kind: EdgeKind,
}

/// The graph at one commit's row. Rows are the same whatever the page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct GraphRow {
    /// The commit's lane (0 is leftmost).
    pub column: u32,
    /// The lines crossing the row.
    pub edges: Vec<GraphEdge>,
}

/// One commit of [`LogResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct LogCommit {
    /// Its id.
    pub oid: String,
    /// Its parents, first parent first.
    pub parents: Vec<String>,
    /// The author's name.
    pub author: String,
    /// The author's email.
    pub email: String,
    /// The author time, in Unix seconds.
    pub time: i64,
    /// The message's first line.
    pub summary: String,
    /// Branches and tags at it (short names), and `HEAD`.
    pub refs: Vec<String>,
    /// Its row of the graph.
    pub graph: GraphRow,
}

/// Result of [`Log`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct LogResult {
    /// Newest first (by committer time, as `git log`).
    pub commits: Vec<LogCommit>,
    /// More commits follow: ask again with `skip` past these.
    pub more: bool,
}

/// Params of the [`Changes`] topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// The workspace to watch. It must be open.
    pub workspace: WorkspaceId,
}

/// What may have changed, in a [`GitChanges`] event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum GitChangeKind {
    /// Files in the work tree: unstaged changes and untracked files.
    Worktree,
    /// The index: staged changes.
    Index,
    /// `HEAD`: a commit, a branch switch.
    Head,
    /// Branches and tags.
    Refs,
    /// The stash.
    Stash,
}

/// One event of [`Changes`]: a root's repository may have changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct GitChanges {
    /// The root whose repository changed.
    pub root: RootId,
    /// What may have changed, sorted. Status depends on `worktree`,
    /// `index` and `head`; branches on `head` and `refs`; the log on
    /// `head` and `refs`; the stash list on `stash`.
    pub kinds: Vec<GitChangeKind>,
}

/// `git/status`: the branch and every changed path.
#[derive(Debug)]
pub enum Status {}
impl Method for Status {
    const NAME: &'static str = "git/status";
    type Params = RepoParams;
    type Result = StatusResult;
}

/// `git/diff`: one file's hunks between two sides.
#[derive(Debug)]
pub enum Diff {}
impl Method for Diff {
    const NAME: &'static str = "git/diff";
    type Params = DiffParams;
    type Result = FileDiff;
}

/// `git/diffFiles`: the files that differ between two sides.
#[derive(Debug)]
pub enum DiffFiles {}
impl Method for DiffFiles {
    const NAME: &'static str = "git/diffFiles";
    type Params = DiffFilesParams;
    type Result = DiffFilesResult;
}

/// `git/stage`: stage whole files (`git add -A`), or chosen lines of one.
#[derive(Debug)]
pub enum Stage {}
impl Method for Stage {
    const NAME: &'static str = "git/stage";
    type Params = StageParams;
    type Result = ();
}

/// `git/unstage`: unstage whole files (`git restore --staged`), or chosen
/// lines of one.
#[derive(Debug)]
pub enum Unstage {}
impl Method for Unstage {
    const NAME: &'static str = "git/unstage";
    type Params = StageParams;
    type Result = ();
}

/// `git/commit`: commit what is staged (hooks run), or amend.
#[derive(Debug)]
pub enum Commit {}
impl Method for Commit {
    const NAME: &'static str = "git/commit";
    type Params = CommitParams;
    type Result = CommitResult;
}

/// `git/branches`: local and remote branches.
#[derive(Debug)]
pub enum Branches {}
impl Method for Branches {
    const NAME: &'static str = "git/branches";
    type Params = RepoParams;
    type Result = BranchesResult;
}

/// `git/switchBranch`: `git switch`.
#[derive(Debug)]
pub enum SwitchBranch {}
impl Method for SwitchBranch {
    const NAME: &'static str = "git/switchBranch";
    type Params = SwitchBranchParams;
    type Result = ();
}

/// `git/createBranch`: `git branch`, or `git switch -c`.
#[derive(Debug)]
pub enum CreateBranch {}
impl Method for CreateBranch {
    const NAME: &'static str = "git/createBranch";
    type Params = CreateBranchParams;
    type Result = ();
}

/// `git/stashList`: the stash, newest first.
#[derive(Debug)]
pub enum StashList {}
impl Method for StashList {
    const NAME: &'static str = "git/stashList";
    type Params = RepoParams;
    type Result = StashListResult;
}

/// `git/stashPush`: `git stash push`.
#[derive(Debug)]
pub enum StashPush {}
impl Method for StashPush {
    const NAME: &'static str = "git/stashPush";
    type Params = StashPushParams;
    type Result = StashPushResult;
}

/// `git/stashPop`: apply a stash and drop it.
#[derive(Debug)]
pub enum StashPop {}
impl Method for StashPop {
    const NAME: &'static str = "git/stashPop";
    type Params = StashParams;
    type Result = ();
}

/// `git/stashApply`: apply a stash and keep it.
#[derive(Debug)]
pub enum StashApply {}
impl Method for StashApply {
    const NAME: &'static str = "git/stashApply";
    type Params = StashParams;
    type Result = ();
}

/// `git/stashDrop`: drop a stash.
#[derive(Debug)]
pub enum StashDrop {}
impl Method for StashDrop {
    const NAME: &'static str = "git/stashDrop";
    type Params = StashParams;
    type Result = ();
}

/// `git/blame`: which commit last changed each line of a file.
#[derive(Debug)]
pub enum Blame {}
impl Method for Blame {
    const NAME: &'static str = "git/blame";
    type Params = BlameParams;
    type Result = BlameResult;
}

/// `git/log`: commits newest first, a page at a time, with graph lanes.
#[derive(Debug)]
pub enum Log {}
impl Method for Log {
    const NAME: &'static str = "git/log";
    type Params = LogParams;
    type Result = LogResult;
}

/// `git/changes`: a root's repository may have changed (debounced, live
/// only: after a reconnect, resubscribe and fetch again).
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "git/changes";
    type Params = ChangesParams;
    type Event = GitChanges;
}
