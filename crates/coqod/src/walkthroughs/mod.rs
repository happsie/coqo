// SPDX-License-Identifier: Apache-2.0

//! `walkthroughs/*`: code walkthroughs agents publish after a change, where
//! they are kept, and the reader's progress (PLAN §7.6, issue #97, ADR
//! 0020).
//!
//! - **Publishing** ([`WalkthroughService::create`], the bridge's
//!   `publish_walkthrough`). The draft is checked in full before anything
//!   is stored: its shape and references ([`validate`]), every anchor
//!   against its file as it is now (open buffers with their unsaved edits),
//!   and the scope's revisions in git ([`git`]). Every problem is reported
//!   at once with its JSON path, so the agent fixes them in one go. Then
//!   each anchor is pinned to its lines with `coqo-anchors` (ADR 0013), the
//!   id, author and time are filled in, and the walkthrough is stored and
//!   recorded in the session's transcript.
//! - **Storage** ([`store`]). Every walkthrough is a row of `walkthroughs`
//!   in `coqo.db`. A workspace whose state has
//!   `walkthroughs.storage = "committed"` also gets each one written to
//!   `.coqo/walkthroughs/<id>.json` in the scope's repository, for the user
//!   to commit; Coqo never commits it.
//! - **Serving.** `walkthroughs/get` resolves every anchor against the files
//!   as they are now (by fingerprint), so a reader lands on the code even
//!   after edits; anchors whose code is gone are orphaned and show their
//!   snippet.
//! - **Coverage** ([`coverage`], issue #99). On publishing, Coqo diffs the
//!   scope itself with the git mod and sorts every changed hunk into covered
//!   by a step, listed as trivial, or uncovered. Uncovered hunks are kept on
//!   the walkthrough and returned to the agent; a draft with
//!   `require_coverage` is refused instead. `get` checks a worktree scope
//!   again (the work tree moves on) with the anchors where they are now.
//!
//! **Access:** a session token may only create walkthroughs, for its own
//! workspace and agent session; everything else takes the daemon token.

mod coverage;
mod git;
mod moves;
pub(crate) mod store;
pub(crate) mod validate;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use coqo_agents::SessionManager;
use coqo_mod_git::Side;
use coqo_protocol::methods::agents::{AcceptanceCriterion, SessionId};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::editor::LineRange;
use coqo_protocol::methods::fs::{MAX_READ_LIMIT, ReadParams};
use coqo_protocol::methods::questions::AnchorSpan;
use coqo_protocol::methods::walkthroughs::{
    self as wtm, COMMITTED_DIR, ChangeKind, ChangesParams, CreateParams, DeleteParams,
    STORAGE_STATE_KEY, SUMMARY_UNCOVERED, ScopeKind, SetStepStateParams, Step, StepAnchor,
    StepState, StorageMode, TrivialHunk, Walkthrough, WalkthroughAuthor, WalkthroughChange,
    WalkthroughCoverage, WalkthroughDraft, WalkthroughId, WalkthroughList, WalkthroughLocation,
    WalkthroughParams, WalkthroughProblems, WalkthroughScope, WalkthroughStorage,
    WalkthroughSummary,
};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId, WorkspaceInfo, WorkspaceParams};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, SeqLog};
use coqo_storage::Timestamp;
use futures_util::StreamExt as _;
use serde_json::Value;

use self::coverage::{AnchorLines, HunksError};
use self::git::RevError;
use self::store::{Store, StoreError};
use self::validate::{Problems, index, key};
use crate::buffers::BufferService;
use crate::git::GitService;
use crate::questions::anchoring::{self, Job, Text};
use crate::workspace::WorkspaceService;

/// Changes kept for slow subscribers.
const CHANGE_LOG: usize = 64;
/// Most problems spelled out in the error message (`data` has them all).
const MESSAGE_PROBLEMS: usize = 50;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn storage_error(e: &StoreError) -> ErrorObject {
    ErrorObject::new(codes::STORAGE_ERROR, format!("walkthroughs store: {e}"))
}

fn unknown(id: &WalkthroughId) -> ErrorObject {
    ErrorObject::new(codes::UNKNOWN_WALKTHROUGH, format!("no walkthrough {id}"))
}

/// The error for a draft with `problems`.
fn rejected(problems: Problems) -> ErrorObject {
    let n = problems.0.len();
    let mut message = format!(
        "the walkthrough was not published: {n} problem{}. Fix every one and call publish_walkthrough again:",
        if n == 1 { "" } else { "s" }
    );
    for p in problems.0.iter().take(MESSAGE_PROBLEMS) {
        message.push_str("\n- ");
        message.push_str(&p.to_string());
    }
    if n > MESSAGE_PROBLEMS {
        message.push_str(&format!("\n- … and {} more", n - MESSAGE_PROBLEMS));
    }
    ErrorObject::new(codes::INVALID_WALKTHROUGH, message).with_data(WalkthroughProblems {
        problems: problems.0,
    })
}

/// `title` as an id: a slug and a random suffix (`volume-discount-3f2a9c1b`).
fn new_id(title: &str) -> WalkthroughId {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "walkthrough" } else { slug };
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        // Unique enough: the clock.
        bytes = (Timestamp::now().0 as u32).to_be_bytes();
    }
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    WalkthroughId(format!("{slug}-{suffix}"))
}

/// How a root is named in messages.
fn root_name(ws: &WorkspaceInfo, root: RootId) -> String {
    match ws.roots.iter().find(|r| r.id == root) {
        Some(r) => format!("root {} ({})", root.0, r.path),
        None => format!("root {}", root.0),
    }
}

fn roots_list(ws: &WorkspaceInfo) -> String {
    ws.roots
        .iter()
        .map(|r| format!("{} ({})", r.id.0, r.path))
        .collect::<Vec<_>>()
        .join(", ")
}

fn root_path(ws: &WorkspaceInfo, root: RootId) -> Option<PathBuf> {
    ws.roots
        .iter()
        .find(|r| r.id == root)
        .map(|r| PathBuf::from(&r.path))
}

/// The lines a whole-line anchor covers: 0-based, `end` inclusive.
fn anchor_lines(a: &wtm::StepAnchor) -> LineRange {
    span_lines(a.anchor.range)
}

/// The lines a span of whole lines covers.
fn span_lines(r: AnchorSpan) -> LineRange {
    let end = if r.end.col == 0 && r.end.line > r.start.line {
        r.end.line - 1
    } else {
        r.end.line
    };
    LineRange {
        start: r.start.line,
        end,
    }
}

/// `w` in a list.
pub(crate) fn summary(w: &Walkthrough) -> WalkthroughSummary {
    let count = |state| {
        w.steps
            .iter()
            .filter(|s| w.step_states.get(&s.id) == Some(&state))
            .count() as u32
    };
    WalkthroughSummary {
        id: w.id.clone(),
        workspace: w.workspace.clone(),
        session: w.created_by.session.clone(),
        title: w.title.clone(),
        scope: w.scope.kind,
        steps: w.steps.len() as u32,
        understood: count(StepState::Understood),
        questions: count(StepState::Question),
        start: w
            .steps
            .first()
            .and_then(|s| s.anchors.first())
            .map(|a| WalkthroughLocation {
                root: a.anchor.root,
                path: a.anchor.path.clone(),
                lines: anchor_lines(a),
            }),
        storage: w.storage.mode,
        coverage: w.coverage.as_ref().map(|c| c.truncated(SUMMARY_UNCOVERED)),
        created_at: w.created_at,
        revision: w.revision,
    }
}

/// Whether a draft's resolved `scope` is `old`'s: the same kind, root and
/// revisions as named.
fn same_scope(old: &WalkthroughScope, scope: &WalkthroughScope) -> bool {
    old.kind == scope.kind
        && old.root == scope.root
        && old.base == scope.base
        && old.head == scope.head
}

/// Titles that name the same walkthrough: equal but for case and the
/// spaces around them.
fn same_title(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// The reader's step states that carry over to `steps` from `old`, the
/// walkthrough they replace: a step keeps its state when its id and title
/// are the same and it anchors the same code (the same files and anchored
/// text, wherever the lines moved). Any other step starts unread.
fn carried_states(old: &Walkthrough, steps: &[Step]) -> BTreeMap<String, StepState> {
    let same_code = |a: &Step, b: &Step| {
        a.anchors.len() == b.anchors.len()
            && a.anchors.iter().zip(&b.anchors).all(|(x, y)| {
                x.anchor.root == y.anchor.root
                    && x.anchor.path == y.anchor.path
                    && x.anchor.text_hash == y.anchor.text_hash
            })
    };
    steps
        .iter()
        .filter_map(|s| {
            let state = *old.step_states.get(&s.id)?;
            let was = old.steps.iter().find(|o| o.id == s.id)?;
            (was.title == s.title && same_code(was, s)).then(|| (s.id.clone(), state))
        })
        .collect()
}

/// The lines of a draft's anchors, for coverage: as the agent gave them,
/// whether or not they check out.
fn draft_anchor_lines(
    ws: &WorkspaceInfo,
    draft: &WalkthroughDraft,
    default_root: RootId,
) -> Vec<AnchorLines> {
    draft
        .steps
        .iter()
        .flat_map(|s| &s.anchors)
        .filter(|a| !a.path.is_empty() && a.start_line >= 1)
        .filter_map(|a| {
            let (root, path) = locate(ws, &a.path, a.root, default_root).ok()?;
            let end = a.end_line.unwrap_or(a.start_line).max(a.start_line);
            Some((
                root,
                path,
                LineRange {
                    start: a.start_line - 1,
                    end: end - 1,
                },
            ))
        })
        .collect()
}

/// The lines of a stored walkthrough's anchors where they are now (their
/// resolution); an orphaned anchor covers nothing.
fn resolved_anchor_lines(w: &Walkthrough) -> Vec<AnchorLines> {
    w.steps
        .iter()
        .flat_map(|s| &s.anchors)
        .filter_map(|a| {
            let lines = match &a.resolution {
                Some(r) => span_lines(r.range?),
                None => anchor_lines(a),
            };
            Some((a.anchor.root, a.anchor.path.clone(), lines))
        })
        .collect()
}

/// A refused `require_coverage` draft's problems: each uncovered hunk.
fn coverage_problems(c: &WalkthroughCoverage, pr: &mut Problems) {
    const AT: &str = "require_coverage";
    if let Some(why) = &c.unavailable {
        pr.add(
            AT,
            format!("coverage cannot be checked: {why}; leave out require_coverage to publish without it"),
        );
        return;
    }
    for h in &c.uncovered {
        let first = h.excerpt.lines().next().unwrap_or_default();
        let quote = if first.is_empty() {
            String::new()
        } else {
            format!(", `{first}`")
        };
        pr.add(
            AT,
            format!(
                "{} ({}{quote}) is not explained by any step and not in trivial_hunks; add it to a step's anchors, or to trivial_hunks with a reason",
                h.location(),
                h.change()
            ),
        );
    }
    let more = c.uncovered_count() as usize - c.uncovered.len();
    if more > 0 {
        pr.add(AT, format!("… and {more} more uncovered hunks"));
    }
}

/// Where a draft's path is: its root and the path relative to it, or why
/// not.
fn locate(
    ws: &WorkspaceInfo,
    path: &str,
    root: Option<RootId>,
    default: RootId,
) -> Result<(RootId, String), (&'static str, String)> {
    if Path::new(path).is_absolute() {
        let p = Path::new(path);
        for r in ws.roots.iter().filter(|r| root.is_none_or(|id| r.id == id)) {
            if let Ok(rel) = p.strip_prefix(&r.path) {
                let rel: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
                return Ok((r.id, rel.join("/")));
            }
        }
        return Err((
            "path",
            format!(
                "{path} is outside the workspace; its roots are {}",
                roots_list(ws)
            ),
        ));
    }
    let root = root.unwrap_or(default);
    if !ws.roots.iter().any(|r| r.id == root) {
        return Err((
            "root",
            format!(
                "the workspace has no root {}; its roots are {}",
                root.0,
                roots_list(ws)
            ),
        ));
    }
    let rel = path.trim_start_matches("./").trim_end_matches('/');
    Ok((root, rel.to_owned()))
}

/// Why a file could not be read, for the agent.
fn file_problem(ws: &WorkspaceInfo, root: RootId, path: &str, e: &ErrorObject) -> String {
    match e.code {
        codes::PATH_OUTSIDE_WORKSPACE => {
            format!(
                "{path} is outside the workspace; use a path inside {}",
                root_name(ws, root)
            )
        }
        codes::NOT_FOUND => format!(
            "there is no file {path} in {}; check the path",
            root_name(ws, root)
        ),
        codes::NOT_A_FILE => format!("{path} is a directory; anchor lines of a file"),
        codes::UNSUPPORTED_ENCODING => format!("{path} is not a text file"),
        codes::FILE_TOO_LARGE => format!("{path} is too large to anchor"),
        _ => format!("{path}: {}", e.message),
    }
}

fn rev_problem(ws: &WorkspaceInfo, root: RootId, rev: &str, e: &RevError) -> String {
    match e {
        RevError::NotARepo => format!(
            "{} is not a git repository, so `{rev}` cannot be resolved; use kind `worktree` without `base`",
            root_name(ws, root)
        ),
        RevError::Unknown => format!(
            "`{rev}` is not a commit in {}; check the name (git branch -a, git log), and fetch a remote branch first",
            root_name(ws, root)
        ),
        RevError::Invalid => format!("`{rev}` is not a revision name"),
        RevError::NoGit(e) => format!("git could not be run to resolve `{rev}`: {e}"),
    }
}

/// The daemon's walkthroughs.
#[derive(Debug)]
pub struct WalkthroughService {
    store: Mutex<Store>,
    changes: Arc<SeqLog<WalkthroughChange>>,
    workspaces: Arc<WorkspaceService>,
    buffers: Arc<BufferService>,
    sessions: Option<SessionManager>,
    git: Option<Arc<GitService>>,
}

/// One anchor of a draft to check and make: `(step, anchor)` indexes and
/// its 1-based lines.
type Wanted = Vec<(usize, usize, wtm::AnchorDraft)>;

impl WalkthroughService {
    /// The service over the walkthroughs in the global database at `db`,
    /// anchoring in `workspaces`' files and `buffers`, recording
    /// publications in `sessions`' transcripts, and diffing scopes with
    /// `git` for coverage.
    pub fn open(
        db: &Path,
        workspaces: Arc<WorkspaceService>,
        buffers: Arc<BufferService>,
        sessions: Option<SessionManager>,
        git: Option<Arc<GitService>>,
    ) -> Result<Arc<Self>, StoreError> {
        Ok(Arc::new(Self {
            store: Mutex::new(Store::open(db)?),
            changes: Arc::new(SeqLog::new(CHANGE_LOG)),
            workspaces,
            buffers,
            sessions,
            git,
        }))
    }

    /// How much of `scope`'s diff `anchors` and `trivial` cover.
    async fn coverage(
        &self,
        ws: &WorkspaceInfo,
        scope: &WalkthroughScope,
        anchors: Vec<AnchorLines>,
        trivial: &[TrivialHunk],
    ) -> WalkthroughCoverage {
        let Some(service) = &self.git else {
            return WalkthroughCoverage::unavailable("git is not available in this daemon");
        };
        let Some(dir) = root_path(ws, scope.root) else {
            return WalkthroughCoverage::unavailable(format!(
                "the workspace has no root {}",
                scope.root.0
            ));
        };
        let rev = |c: &String| Side::Rev(c.clone());
        let (base, head, untracked) = match (scope.kind, &scope.base_commit, &scope.head_commit) {
            (ScopeKind::Worktree, Some(base), _) => (rev(base), Side::Worktree, true),
            (ScopeKind::Worktree, None, _) => {
                let repo = tokio::task::spawn_blocking(move || git::is_repo(&dir)).await;
                return WalkthroughCoverage::unavailable(match repo {
                    Ok(Ok(true)) => {
                        "the repository has no commits yet, so there is no diff to check".to_owned()
                    }
                    _ => format!(
                        "{} is not a git repository, so there is no diff to check",
                        root_name(ws, scope.root)
                    ),
                });
            }
            (ScopeKind::Commits, Some(base), Some(head)) => (rev(base), rev(head), false),
            (ScopeKind::Pr, Some(base), Some(head)) => {
                let (b, h) = (base.clone(), head.clone());
                let merge_base = tokio::task::spawn_blocking(move || git::merge_base(&dir, &b, &h))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| base.clone());
                (Side::Rev(merge_base), rev(head), false)
            }
            _ => {
                return WalkthroughCoverage::unavailable("the scope's revisions did not resolve");
            }
        };
        let hunks = service
            .with_repo(&ws.id, scope.root, move |repo| {
                Ok(coverage::scope_hunks(repo, &base, &head, untracked))
            })
            .await;
        match hunks {
            Ok(Ok(changed)) => coverage::classify(scope.root, &changed, &anchors, trivial),
            Ok(Err(HunksError::TooMany(n))) => WalkthroughCoverage::unavailable(format!(
                "{n} files changed; more than {} are not checked",
                coverage::MAX_FILES
            )),
            Ok(Err(HunksError::Git(e))) => WalkthroughCoverage::unavailable(format!(
                "the diff could not be computed: {}",
                e.message
            )),
            Err(e) => WalkthroughCoverage::unavailable(format!("git: {}", e.message)),
        }
    }

    fn record(&self, id: &WalkthroughId) -> Result<Walkthrough, ErrorObject> {
        lock(&self.store)
            .get(id)
            .map_err(|e| storage_error(&e))?
            .ok_or_else(|| unknown(id))
    }

    fn publish(&self, kind: ChangeKind, w: &Walkthrough) {
        self.changes.publish(WalkthroughChange {
            kind,
            id: w.id.clone(),
            workspace: w.workspace.clone(),
            summary: (kind != ChangeKind::Deleted).then(|| summary(w)),
        });
    }

    /// A file as anchors see it now: the open buffer with its unsaved
    /// edits, else the file on disk.
    async fn text_of(
        &self,
        workspace: &WorkspaceId,
        root: RootId,
        path: &str,
    ) -> Result<Text, ErrorObject> {
        let file = self
            .workspaces
            .resolve_file(workspace.clone(), root, path.to_owned())
            .await?;
        if let Some(buffer) = self.buffers.buffer_at(workspace, &file.real)
            && let Ok(view) = self.buffers.anchor_view(buffer, None)
        {
            return Ok(Text {
                text: view.text,
                buffer: Some((buffer, view.rev)),
                ..Text::default()
            });
        }
        let read = self
            .workspaces
            .read_file(ReadParams {
                workspace: workspace.clone(),
                root,
                path: path.to_owned(),
                max_bytes: Some(MAX_READ_LIMIT),
            })
            .await?;
        match read.text {
            Some(text) => Ok(Text {
                text,
                ..Text::default()
            }),
            None => Err(ErrorObject::new(
                codes::UNSUPPORTED_ENCODING,
                format!("{path} is not a text file"),
            )),
        }
    }

    /// The publishing session's acceptance criteria and provider.
    fn session_facts(&self, session: &str) -> (Vec<AcceptanceCriterion>, String) {
        let snap = self
            .sessions
            .as_ref()
            .and_then(|s| s.snapshot(&SessionId(session.to_owned())).ok());
        match snap {
            Some(s) => (s.info.criteria, s.info.provider),
            None => (Vec::new(), "agent".to_owned()),
        }
    }

    /// Check every anchor of `draft` against its file and pin it there.
    /// Returns the anchors by step, or adds problems.
    async fn anchor_steps(
        &self,
        ws: &WorkspaceInfo,
        draft: &WalkthroughDraft,
        default_root: RootId,
        pr: &mut Problems,
    ) -> Vec<Vec<Option<StepAnchor>>> {
        let mut out: Vec<Vec<Option<StepAnchor>>> = draft
            .steps
            .iter()
            .map(|s| vec![None; s.anchors.len()])
            .collect();
        let mut by_file: BTreeMap<(RootId, String), Wanted> = BTreeMap::new();
        for (i, step) in draft.steps.iter().enumerate() {
            for (k, a) in step.anchors.iter().enumerate() {
                if a.path.is_empty() {
                    // Too broken to look for; its problems are reported.
                    continue;
                }
                let at = index(&key(&index("steps", i), "anchors"), k);
                match locate(ws, &a.path, a.root, default_root) {
                    Ok(file) => by_file.entry(file).or_default().push((i, k, a.clone())),
                    Err((field, message)) => pr.add(key(&at, field), message),
                }
            }
        }
        for ((root, path), wanted) in by_file {
            let at = |i: usize, k: usize, field: &str| {
                key(&index(&key(&index("steps", i), "anchors"), k), field)
            };
            let text = match self.text_of(&ws.id, root, &path).await {
                Ok(text) => text,
                Err(e) => {
                    let message = file_problem(ws, root, &path, &e);
                    for (i, k, _) in &wanted {
                        pr.add(at(*i, *k, "path"), message.clone());
                    }
                    continue;
                }
            };
            let mut ok: Vec<(usize, usize, RangeInclusive<usize>)> = Vec::new();
            for (i, k, a) in &wanted {
                match validate::check_lines(&text.text, a, &path) {
                    Ok(lines) => ok.push((*i, *k, lines)),
                    Err((field, message)) => pr.add(at(*i, *k, field), message),
                }
            }
            if ok.is_empty() || !pr.is_empty() {
                // Nothing will be stored; do not anchor for nothing.
                continue;
            }
            let ranges: Vec<_> = ok.iter().map(|(.., l)| l.clone()).collect();
            let file = path.clone();
            let made =
                tokio::task::spawn_blocking(move || anchoring::make(&file, &text, &ranges)).await;
            match made {
                Ok(Ok(made)) => {
                    for ((i, k, _), (anchor, _)) in ok.into_iter().zip(made) {
                        out[i][k] = Some(StepAnchor {
                            anchor: anchoring::code_anchor(root, &anchor),
                            resolution: None,
                        });
                    }
                }
                Ok(Err(e)) => {
                    for (i, k, _) in ok {
                        pr.add(at(i, k, "start_line"), format!("{path}: {e}"));
                    }
                }
                Err(e) => pr.add("", format!("anchoring failed: {e}")),
            }
        }
        out
    }

    /// Check the trivial hunks' paths: inside the workspace (the file may
    /// be gone, as a deleted file's hunk is).
    async fn trivial_hunks(
        &self,
        ws: &WorkspaceInfo,
        draft: &WalkthroughDraft,
        default_root: RootId,
        pr: &mut Problems,
    ) -> Vec<TrivialHunk> {
        let mut out = Vec::new();
        for (i, h) in draft.trivial_hunks.iter().enumerate() {
            let at = index("trivial_hunks", i);
            let (root, path) = match locate(ws, &h.path, h.root, default_root) {
                Ok(file) => file,
                Err((field, message)) => {
                    pr.add(key(&at, field), message);
                    continue;
                }
            };
            match self
                .workspaces
                .resolve_file(ws.id.clone(), root, path.clone())
                .await
            {
                Ok(_) => {}
                Err(e) if e.code == codes::NOT_FOUND => {}
                Err(e) => {
                    pr.add(key(&at, "path"), file_problem(ws, root, &path, &e));
                    continue;
                }
            }
            let lines = h.start_line.map(|start| LineRange {
                start: start - 1,
                end: h.end_line.unwrap_or(start) - 1,
            });
            out.push(TrivialHunk {
                root,
                path,
                lines,
                reason: h.reason.clone(),
            });
        }
        out
    }

    /// The scope with its revisions resolved in git.
    async fn scope(
        &self,
        ws: &WorkspaceInfo,
        draft: &WalkthroughDraft,
        root: RootId,
        pr: &mut Problems,
    ) -> WalkthroughScope {
        let s = &draft.scope;
        let mut scope = WalkthroughScope {
            kind: s.kind,
            root,
            base: s.base.clone(),
            head: s.head.clone(),
            base_commit: None,
            head_commit: None,
        };
        let Some(dir) = root_path(ws, root) else {
            return scope;
        };
        let base = s.base.clone();
        let head = s.head.clone();
        let kind = s.kind;
        let resolved = tokio::task::spawn_blocking(move || {
            let base = match (&base, kind) {
                (Some(rev), _) => Some(git::rev_parse(&dir, rev).map_err(|e| (rev.clone(), e))),
                // The usual case right after a change: against HEAD, if
                // there is one.
                (None, ScopeKind::Worktree) => git::rev_parse(&dir, "HEAD").ok().map(Ok),
                (None, _) => None,
            };
            let head = head
                .as_ref()
                .filter(|_| kind != ScopeKind::Worktree)
                .map(|rev| git::rev_parse(&dir, rev).map_err(|e| (rev.clone(), e)));
            (base, head)
        })
        .await;
        let Ok((base, head)) = resolved else {
            pr.add("scope", "resolving the scope's revisions failed");
            return scope;
        };
        match base {
            Some(Ok(hash)) => {
                scope.base.get_or_insert_with(|| "HEAD".to_owned());
                scope.base_commit = Some(hash);
            }
            Some(Err((rev, e))) => pr.add("scope.base", rev_problem(ws, root, &rev, &e)),
            None => {}
        }
        match head {
            Some(Ok(hash)) => scope.head_commit = Some(hash),
            Some(Err((rev, e))) => pr.add("scope.head", rev_problem(ws, root, &rev, &e)),
            None => {}
        }
        scope
    }

    /// The walkthrough `draft` replaces: the one its `replaces` names (one
    /// of this session's), else the session's latest one with the same
    /// title and `scope`, if any.
    fn replaced(
        &self,
        p: &CreateParams,
        draft: &WalkthroughDraft,
        scope: &WalkthroughScope,
        pr: &mut Problems,
    ) -> Result<Option<Walkthrough>, ErrorObject> {
        let store = lock(&self.store);
        if let Some(id) = draft.replaces.as_deref() {
            if id.is_empty() {
                return Ok(None);
            }
            let found = store
                .get(&WalkthroughId(id.to_owned()))
                .map_err(|e| storage_error(&e))?;
            return Ok(match found {
                Some(w) if w.workspace == p.workspace && w.created_by.session.0 == p.session => {
                    Some(w)
                }
                Some(_) => {
                    pr.add(
                        "replaces",
                        format!(
                            "`{id}` was published by another session; only this session's walkthroughs can be replaced (leave out replaces to publish a new one)"
                        ),
                    );
                    None
                }
                None => {
                    pr.add(
                        "replaces",
                        format!(
                            "there is no walkthrough `{id}`; use the id publish_walkthrough returned, or leave out replaces to publish a new one"
                        ),
                    );
                    None
                }
            });
        }
        let session = SessionId(p.session.clone());
        let list = store
            .list(&p.workspace, Some(&session))
            .map_err(|e| storage_error(&e))?;
        Ok(list
            .into_iter()
            .find(|w| same_title(&w.title, &draft.title) && same_scope(&w.scope, scope)))
    }

    /// The workspace's storage mode (workspace state).
    async fn storage_mode(&self, workspace: &WorkspaceId) -> StorageMode {
        let state = self
            .workspaces
            .get_state(WorkspaceParams {
                id: workspace.clone(),
            })
            .await;
        match state
            .ok()
            .as_ref()
            .and_then(|s| s.entries.get(STORAGE_STATE_KEY))
        {
            Some(Value::String(mode)) if mode == "committed" => StorageMode::Committed,
            _ => StorageMode::Local,
        }
    }

    // ---- methods --------------------------------------------------------------

    /// `walkthroughs/create`.
    pub async fn create(self: &Arc<Self>, p: CreateParams) -> Result<Walkthrough, ErrorObject> {
        let ws = self
            .workspaces
            .get(WorkspaceParams {
                id: p.workspace.clone(),
            })
            .await?;
        let mut pr = Problems::default();
        let draft = validate::parse(&p.walkthrough, &mut pr);
        let (session_criteria, agent) = self.session_facts(&p.session);
        let criteria = validate::check(&draft, &session_criteria, &mut pr);

        let Some(first) = ws.roots.first() else {
            return Err(ErrorObject::invalid_params(
                "the workspace has no folder roots",
            ));
        };
        let root = match draft.scope.root {
            Some(root) if !ws.roots.iter().any(|r| r.id == root) => {
                pr.add(
                    "scope.root",
                    format!(
                        "the workspace has no root {}; its roots are {}",
                        root.0,
                        roots_list(&ws)
                    ),
                );
                first.id
            }
            Some(root) => root,
            None => first.id,
        };
        let scope = self.scope(&ws, &draft, root, &mut pr).await;
        let previous = self.replaced(&p, &draft, &scope, &mut pr)?;
        let trivial_hunks = self.trivial_hunks(&ws, &draft, root, &mut pr).await;
        let anchors = self.anchor_steps(&ws, &draft, root, &mut pr).await;
        // Only worth the diff if it may be published, or to say what
        // `require_coverage` refuses alongside the rest.
        let coverage = if pr.is_empty() || draft.require_coverage {
            let lines = draft_anchor_lines(&ws, &draft, root);
            let c = self.coverage(&ws, &scope, lines, &trivial_hunks).await;
            if draft.require_coverage
                && !c.is_complete()
                && (pr.is_empty() || c.unavailable.is_none())
            {
                coverage_problems(&c, &mut pr);
            }
            Some(c)
        } else {
            None
        };
        if !pr.is_empty() {
            tracing::debug!(session = %p.session, problems = pr.0.len(), "walkthrough refused");
            return Err(rejected(pr));
        }

        let steps: Vec<Step> = draft
            .steps
            .into_iter()
            .zip(anchors)
            .map(|(s, anchors)| Step {
                id: s.id,
                title: s.title,
                kind: s.kind,
                anchors: anchors.into_iter().flatten().collect(),
                what: s.what,
                why: s.why,
                risks: s.risks,
                criteria_refs: s.criteria_refs,
                depends_on: s.depends_on,
            })
            .collect();
        // Published again: the same walkthrough, in place.
        let (id, revision, step_states) = match &previous {
            Some(old) => (
                old.id.clone(),
                old.revision + 1,
                carried_states(old, &steps),
            ),
            None => (new_id(&draft.title), 0, BTreeMap::new()),
        };
        let mut walkthrough = Walkthrough {
            id: id.clone(),
            workspace: p.workspace.clone(),
            title: draft.title,
            summary: draft.summary,
            scope,
            acceptance_criteria: criteria,
            steps,
            trivial_hunks,
            coverage,
            created_by: WalkthroughAuthor {
                session: SessionId(p.session.clone()),
                agent,
            },
            created_at: Timestamp::now().0,
            revision,
            storage: WalkthroughStorage::default(),
            step_states,
        };
        if self.storage_mode(&p.workspace).await == StorageMode::Committed {
            walkthrough.storage = self.commit_file(&ws, &walkthrough).await;
        }
        let replaced = {
            let store = lock(&self.store);
            let replaced =
                previous.is_some() && store.replace(&walkthrough).map_err(|e| storage_error(&e))?;
            if !replaced {
                // New, or the one it replaces was deleted meanwhile.
                walkthrough.revision = 0;
                walkthrough.step_states.clear();
                store.insert(&walkthrough).map_err(|e| storage_error(&e))?;
            }
            replaced
        };
        self.publish(
            if replaced {
                ChangeKind::Updated
            } else {
                ChangeKind::Published
            },
            &walkthrough,
        );
        if let Some(sessions) = &self.sessions
            && let Err(e) =
                sessions.record_walkthrough(&walkthrough.created_by.session, summary(&walkthrough))
        {
            tracing::debug!(session = %p.session, error = %e, "cannot record a walkthrough");
        }
        tracing::debug!(
            walkthrough = %id,
            steps = walkthrough.steps.len(),
            revision = walkthrough.revision,
            storage = ?walkthrough.storage.mode,
            uncovered = walkthrough.coverage.as_ref().map(WalkthroughCoverage::uncovered_count),
            "walkthrough published"
        );
        Ok(walkthrough)
    }

    /// Write `w` to `.coqo/walkthroughs/<id>.json` in its scope's root.
    async fn commit_file(&self, ws: &WorkspaceInfo, w: &Walkthrough) -> WalkthroughStorage {
        let rel = format!("{COMMITTED_DIR}/{}.json", w.id);
        let mut storage = WalkthroughStorage {
            mode: StorageMode::Committed,
            path: Some(rel.clone()),
            error: None,
        };
        let Some(root) = root_path(ws, w.scope.root) else {
            storage.error = Some("its root is gone".into());
            return storage;
        };
        let mut doc = w.clone();
        doc.storage = storage.clone();
        doc.step_states.clear();
        let written = tokio::task::spawn_blocking(move || write_committed(&root, &rel, &doc)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => storage.error = Some(e),
            Err(e) => storage.error = Some(e.to_string()),
        }
        if let Some(e) = &storage.error {
            tracing::warn!(walkthrough = %w.id, error = %e, "cannot write the committed walkthrough");
        }
        storage
    }

    /// `walkthroughs/list`.
    pub fn list(&self, p: wtm::ListParams) -> Result<WalkthroughList, ErrorObject> {
        let list = lock(&self.store)
            .list(&p.workspace, p.session.as_ref())
            .map_err(|e| storage_error(&e))?;
        Ok(WalkthroughList {
            walkthroughs: list.iter().map(summary).collect(),
        })
    }

    /// `walkthroughs/get`: anchors resolved against the files as they are
    /// now, one read and parse per file.
    pub async fn get(&self, p: WalkthroughParams) -> Result<Walkthrough, ErrorObject> {
        let mut w = self.record(&p.id)?;
        let mut by_file: BTreeMap<(RootId, String), Vec<(usize, usize)>> = BTreeMap::new();
        for (i, s) in w.steps.iter().enumerate() {
            for (k, a) in s.anchors.iter().enumerate() {
                by_file
                    .entry((a.anchor.root, a.anchor.path.clone()))
                    .or_default()
                    .push((i, k));
            }
        }
        for ((root, path), members) in by_file {
            let resolutions = match self.text_of(&w.workspace, root, &path).await {
                Ok(text) => {
                    let jobs: Vec<Job> = members
                        .iter()
                        .map(|&(i, k)| Job {
                            original: anchoring::from_wire(&w.steps[i].anchors[k].anchor),
                            current: None,
                            tracked: None,
                        })
                        .collect();
                    let file = path.clone();
                    tokio::task::spawn_blocking(move || anchoring::resolve(&file, &text, &jobs))
                        .await
                        .map(|outcomes| outcomes.into_iter().map(|o| o.resolution).collect())
                        .unwrap_or_else(|_| vec![anchoring::orphaned(); members.len()])
                }
                Err(e) => {
                    tracing::debug!(walkthrough = %w.id, %path, error = %e.message, "anchored file unreadable");
                    vec![anchoring::orphaned(); members.len()]
                }
            };
            for (&(i, k), r) in members.iter().zip(resolutions) {
                w.steps[i].anchors[k].resolution = Some(r);
            }
        }
        if w.scope.kind == ScopeKind::Worktree {
            self.recheck_coverage(&mut w).await;
        }
        Ok(w)
    }

    /// Check a worktree walkthrough's coverage again, with its anchors
    /// where they are now; keep and announce it if it changed.
    async fn recheck_coverage(&self, w: &mut Walkthrough) {
        let Ok(ws) = self
            .workspaces
            .get(WorkspaceParams {
                id: w.workspace.clone(),
            })
            .await
        else {
            return;
        };
        let anchors = resolved_anchor_lines(w);
        let c = self
            .coverage(&ws, &w.scope, anchors, &w.trivial_hunks)
            .await;
        if w.coverage.as_ref() == Some(&c) {
            return;
        }
        w.coverage = Some(c);
        let stored = lock(&self.store).set_coverage(&w.id, w.coverage.as_ref());
        match stored {
            Ok(true) => self.publish(ChangeKind::Updated, w),
            Ok(false) => {}
            Err(e) => tracing::debug!(walkthrough = %w.id, error = %e, "cannot keep the coverage"),
        }
    }

    /// `walkthroughs/delete`.
    pub async fn delete(&self, p: DeleteParams) -> Result<bool, ErrorObject> {
        let w = match lock(&self.store).get(&p.id) {
            Ok(Some(w)) => w,
            Ok(None) => return Ok(false),
            Err(e) => return Err(storage_error(&e)),
        };
        if p.delete_file
            && let Some(rel) = &w.storage.path
            && let Ok(ws) = self
                .workspaces
                .get(WorkspaceParams {
                    id: w.workspace.clone(),
                })
                .await
            && let Some(root) = root_path(&ws, w.scope.root)
        {
            let file = root.join(rel);
            if let Err(e) = std::fs::remove_file(&file)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(ErrorObject::new(
                    codes::IO_ERROR,
                    format!("cannot delete {}: {e}", file.display()),
                ));
            }
        }
        let existed = lock(&self.store)
            .delete(&p.id)
            .map_err(|e| storage_error(&e))?;
        if existed {
            self.publish(ChangeKind::Deleted, &w);
        }
        Ok(existed)
    }

    /// `walkthroughs/setStepState`.
    pub fn set_step_state(&self, p: SetStepStateParams) -> Result<WalkthroughSummary, ErrorObject> {
        let store = lock(&self.store);
        let mut w = store
            .get(&p.id)
            .map_err(|e| storage_error(&e))?
            .ok_or_else(|| unknown(&p.id))?;
        if !w.steps.iter().any(|s| s.id == p.step) {
            let ids: Vec<_> = w.steps.iter().map(|s| s.id.as_str()).collect();
            return Err(ErrorObject::invalid_params(format!(
                "walkthrough {} has no step `{}` (its steps: {})",
                p.id,
                p.step,
                ids.join(", ")
            )));
        }
        match p.state {
            StepState::Unread => w.step_states.remove(&p.step),
            state => w.step_states.insert(p.step, state),
        };
        store
            .set_states(&p.id, &w.step_states, Timestamp::now().0)
            .map_err(|e| storage_error(&e))?;
        drop(store);
        self.publish(ChangeKind::Updated, &w);
        Ok(summary(&w))
    }

    /// The `walkthroughs/changes` feed, live.
    pub fn changes(&self, p: ChangesParams) -> Result<EventFeed<WalkthroughChange>, ErrorObject> {
        let feed = self.changes.subscribe(None)?;
        let Some(workspace) = p.workspace else {
            return Ok(feed);
        };
        Ok(EventFeed {
            head: feed.head,
            events: feed
                .events
                .filter(move |e| {
                    std::future::ready(e.as_ref().map_or(true, |s| s.event.workspace == workspace))
                })
                .boxed(),
        })
    }
}

/// Write `doc` to `root/rel` (atomically), refusing to follow a symlinked
/// `.coqo` out of the repository.
fn write_committed(root: &Path, rel: &str, doc: &Walkthrough) -> Result<(), String> {
    let dir = root.join(COMMITTED_DIR);
    let coqo = root.join(".coqo");
    if std::fs::symlink_metadata(&coqo).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!(
            "{} is a symlink; not writing through it",
            coqo.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut json =
        serde_json::to_string_pretty(doc).map_err(|e| format!("cannot encode it: {e}"))?;
    json.push('\n');
    let file = root.join(rel);
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// Register `walkthroughs/*` on `router`.
pub fn register(router: Router, svc: Arc<WalkthroughService>) -> Router {
    let list = Arc::clone(&svc);
    let get = Arc::clone(&svc);
    let delete = Arc::clone(&svc);
    let step = Arc::clone(&svc);
    let create = Arc::clone(&svc);
    let changes = svc;
    router
        .method_with::<wtm::List, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(list.list(p))
        })
        .method_with::<wtm::Get, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&get);
            async move { s.get(p).await }
        })
        .method_with::<wtm::Delete, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&delete);
            async move { s.delete(p).await }
        })
        .method_with::<wtm::SetStepState, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(step.set_step_state(p))
        })
        .method_with::<wtm::Create, _, _>(
            Access::check(|scope, p: &CreateParams| {
                scope.require_workspace(&p.workspace.0)?;
                scope.require_session(&p.session)
            }),
            move |_ctx, p| {
                let s = Arc::clone(&create);
                async move { s.create(p).await }
            },
        )
        .topic_with::<wtm::Changes, _, _>(Access::daemon(), move |_ctx, p, _since| {
            std::future::ready(changes.changes(p))
        })
}
