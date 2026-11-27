// SPDX-License-Identifier: Apache-2.0

//! Where a [`DiffView`](super::DiffView) gets its files: the [`DiffSource`]
//! trait, and [`GitDiffSource`] over `git/diffFiles` and `git/diff`.
//! Walkthroughs (#100) and pull requests (#103) bring their own sources.

use std::rc::Rc;

use coqo_protocol::methods::git::{
    self as gm, DiffSide, FileChange, FileDiff, GitChangeKind, LineSelection,
};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_ui::gpui::{App, Entity, SharedString, Subscription, Window};
use futures::future::BoxFuture;

use crate::daemon::{Daemon, DaemonError};
use crate::git::model::{GitModel, GitModelEvent};

/// Context lines a source should fetch with: enough that every fold in a
/// file of up to 20,000 lines can open (`git/diff` allows at most 10,000).
pub const FULL_CONTEXT: u32 = 10_000;

/// One file of a diff, as the file list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    /// The path [`DiffSource::file`] takes, and the diff's `path`.
    pub path: String,
    pub change: FileChange,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
}

/// What a source's line staging does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staging {
    /// The source is index → work tree: lines go into the index.
    Stage,
    /// The source is `HEAD` → index: lines come out of it.
    Unstage,
}

/// A failure, as the view shows it.
pub type SourceError = String;

/// A provider of file diffs.
pub trait DiffSource: 'static {
    /// What is compared, stable across sessions (`git:<root>:index..worktree`).
    /// "Viewed" marks are kept per workspace and identity.
    fn identity(&self) -> String;

    /// The old side, for the header (`index`, `main`).
    fn base_label(&self) -> SharedString;

    /// The new side (`working tree`, `feature/volume-discount`).
    fn head_label(&self) -> SharedString;

    /// The changed files, in display order.
    fn files(&self, cx: &mut App) -> BoxFuture<'static, Result<Vec<SourceFile>, SourceError>>;

    /// One file's diff, ideally with [`FULL_CONTEXT`] so folds can open.
    fn file(&self, path: &str, cx: &mut App) -> BoxFuture<'static, Result<FileDiff, SourceError>>;

    /// Whether hunks and lines can be staged from this diff, and which way.
    fn staging(&self) -> Option<Staging> {
        None
    }

    /// Stage (or unstage, per [`staging`](Self::staging)) `lines` of
    /// `path`, or the whole file with `None`.
    fn stage(
        &self,
        _path: &str,
        _lines: Option<LineSelection>,
        _cx: &mut App,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        Box::pin(async { Err("this diff cannot be staged".to_owned()) })
    }

    /// Call `changed` whenever the files may have changed, for as long as
    /// the subscription lives. `None` for a source that never changes.
    fn watch(
        &self,
        _changed: Rc<dyn Fn(&mut App)>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Subscription> {
        None
    }
}

/// Unstaged (index → work tree, untracked files included) or staged
/// (`HEAD` → index) changes of one root.
pub struct GitDiffSource {
    workspace: WorkspaceId,
    root: RootId,
    staged: bool,
    model: Entity<GitModel>,
}

impl std::fmt::Debug for GitDiffSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitDiffSource")
            .field("workspace", &self.workspace)
            .field("root", &self.root)
            .field("staged", &self.staged)
            .finish_non_exhaustive()
    }
}

impl GitDiffSource {
    pub fn new(
        workspace: WorkspaceId,
        root: RootId,
        staged: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self {
            workspace,
            root,
            staged,
            model: GitModel::for_window(window, cx),
        }
    }

    fn sides(&self) -> (DiffSide, DiffSide) {
        if self.staged {
            (DiffSide::head(), DiffSide::Index)
        } else {
            (DiffSide::Index, DiffSide::Worktree)
        }
    }
}

fn message(err: DaemonError) -> SourceError {
    match err {
        DaemonError::NotConnected => "Not connected to coqod".to_owned(),
        err => err.to_string(),
    }
}

impl DiffSource for GitDiffSource {
    fn identity(&self) -> String {
        let side = if self.staged {
            "HEAD..index"
        } else {
            "index..worktree"
        };
        format!("git:{}:{side}", self.root.0)
    }

    fn base_label(&self) -> SharedString {
        if self.staged { "HEAD" } else { "index" }.into()
    }

    fn head_label(&self) -> SharedString {
        if self.staged { "index" } else { "working tree" }.into()
    }

    fn files(&self, cx: &mut App) -> BoxFuture<'static, Result<Vec<SourceFile>, SourceError>> {
        let Some(daemon) = Daemon::global(cx) else {
            return Box::pin(async { Err(message(DaemonError::NotConnected)) });
        };
        let (base, head) = self.sides();
        let call = daemon
            .read(cx)
            .request::<gm::DiffFiles>(gm::DiffFilesParams {
                workspace: self.workspace.clone(),
                root: self.root,
                base,
                head,
            });
        // `git diff` leaves untracked files out; the Git pane lists them
        // with the changes, and so does this view.
        let untracked: Vec<String> = if self.staged {
            Vec::new()
        } else {
            self.model
                .read(cx)
                .root(self.root)
                .map(|r| {
                    r.unstaged()
                        .filter(|e| e.unstaged == Some(FileChange::Untracked))
                        .map(|e| e.path.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        Box::pin(async move {
            let mut files: Vec<SourceFile> = call
                .await
                .map_err(message)?
                .files
                .into_iter()
                .map(|f| SourceFile {
                    path: f.path,
                    change: f.change,
                    additions: f.additions,
                    deletions: f.deletions,
                    binary: f.binary,
                })
                .collect();
            for path in untracked {
                if !files.iter().any(|f| f.path == path) {
                    files.push(SourceFile {
                        path,
                        change: FileChange::Untracked,
                        additions: 0,
                        deletions: 0,
                        binary: false,
                    });
                }
            }
            files.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(files)
        })
    }

    fn file(&self, path: &str, cx: &mut App) -> BoxFuture<'static, Result<FileDiff, SourceError>> {
        let Some(daemon) = Daemon::global(cx) else {
            return Box::pin(async { Err(message(DaemonError::NotConnected)) });
        };
        let (base, head) = self.sides();
        let call = daemon.read(cx).request::<gm::Diff>(gm::DiffParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: path.to_owned(),
            base,
            head,
            context: Some(FULL_CONTEXT),
        });
        Box::pin(async move { call.await.map_err(message) })
    }

    fn staging(&self) -> Option<Staging> {
        Some(if self.staged {
            Staging::Unstage
        } else {
            Staging::Stage
        })
    }

    fn stage(
        &self,
        path: &str,
        lines: Option<LineSelection>,
        cx: &mut App,
    ) -> BoxFuture<'static, Result<(), SourceError>> {
        let Some(daemon) = Daemon::global(cx) else {
            return Box::pin(async { Err(message(DaemonError::NotConnected)) });
        };
        let params = gm::StageParams {
            workspace: self.workspace.clone(),
            root: self.root,
            paths: vec![path.to_owned()],
            lines,
        };
        let daemon = daemon.read(cx);
        if self.staged {
            let call = daemon.request::<gm::Unstage>(params);
            Box::pin(async move { call.await.map_err(message) })
        } else {
            let call = daemon.request::<gm::Stage>(params);
            Box::pin(async move { call.await.map_err(message) })
        }
    }

    fn watch(
        &self,
        changed: Rc<dyn Fn(&mut App)>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Subscription> {
        let (root, staged) = (self.root, self.staged);
        Some(
            cx.subscribe(&self.model, move |_, event: &GitModelEvent, cx| {
                let GitModelEvent::Changed { root: r, kinds } = event else {
                    return;
                };
                use GitChangeKind as K;
                let ours = if staged {
                    [K::Index, K::Head].as_slice()
                } else {
                    [K::Index, K::Worktree].as_slice()
                };
                if *r == root && kinds.iter().any(|k| ours.contains(k)) {
                    changed(cx);
                }
            }),
        )
    }
}

/// A fixed set of diffs, for tests and screenshots.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct StaticSource {
    pub identity: String,
    pub base: SharedString,
    pub head: SharedString,
    pub files: Vec<FileDiff>,
}

#[cfg(test)]
impl DiffSource for StaticSource {
    fn identity(&self) -> String {
        self.identity.clone()
    }

    fn base_label(&self) -> SharedString {
        self.base.clone()
    }

    fn head_label(&self) -> SharedString {
        self.head.clone()
    }

    fn files(&self, _: &mut App) -> BoxFuture<'static, Result<Vec<SourceFile>, SourceError>> {
        let files = self.list();
        Box::pin(async move { Ok(files) })
    }

    fn file(&self, path: &str, _: &mut App) -> BoxFuture<'static, Result<FileDiff, SourceError>> {
        let diff = self.files.iter().find(|d| d.path == path).cloned();
        let path = path.to_owned();
        Box::pin(async move { diff.ok_or_else(|| format!("{path} is not in this diff")) })
    }
}

#[cfg(test)]
impl StaticSource {
    /// The file list, at once.
    pub fn list(&self) -> Vec<SourceFile> {
        self.files
            .iter()
            .map(|d| SourceFile {
                path: d.path.clone(),
                change: match (&d.old, &d.new) {
                    (None, _) => FileChange::Added,
                    (_, None) => FileChange::Deleted,
                    _ => FileChange::Modified,
                },
                additions: d.additions,
                deletions: d.deletions,
                binary: d.binary,
            })
            .collect()
    }
}
