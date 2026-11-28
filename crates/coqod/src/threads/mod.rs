// SPDX-License-Identifier: Apache-2.0

//! `threads/*` and `review/*`: comment threads on diff lines and the user's
//! pending review (PLAN §7.7, issue #94, ADR 0022).
//!
//! - **Store.** Threads and comments are rows of `review_threads` and
//!   `review_comments` in `coqo.db` ([`store`]), so they survive restarts.
//!   Each change gets the next `threads/changes` seq, written with it, so a
//!   UI resumes from the seq of its last list. A deleted thread stays as a
//!   tombstone, so the replay can tell.
//! - **Anchors.** A thread is pinned to whole lines of one *version* of a
//!   file, the one the diff side shows: the work tree (the file on disk, as
//!   `git diff` reads it), the index or a revision (read through the git
//!   service). The anchor is made with `coqo-anchors` (the questions'
//!   [`anchoring`]) and resolved against that version as it is now every
//!   time a thread is served, by its fingerprint, so lines added above move
//!   it and deleted code orphans it. A moved anchor is rebased and stored
//!   (`current`); the original stays for the snippet an orphan shows. There
//!   is no live tracking of unsaved edits: a diff shows what is saved, and
//!   the UI lists again when `git/changes` says the files changed.
//! - **Comments.** By the user, an agent session or (M8) a GitHub login.
//!   The user's comments may be *pending*: the pending review, sent to the
//!   agent in one prompt by `review/send` ([`review`], #95). An agent's
//!   reply is a comment by [`Author::Agent`], added by `review/reply` (the
//!   `reply_to_review` tool), which may also resolve the thread.
//! - **Backends** ([`backend`]): every thread is in the store; its backend
//!   mirrors changes elsewhere. Only [`LocalBackend`] exists now.
//!
//! **Access:** every route takes the daemon token, except `review/reply`:
//! a session token may reply as its own session in its own workspace.

pub(crate) mod backend;
pub(crate) mod review;
pub(crate) mod store;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use coqo_agents::SessionManager;
use coqo_anchors::Anchor;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{MAX_READ_LIMIT, ReadParams};
use coqo_protocol::methods::git::DiffSide;
use coqo_protocol::methods::questions::{AnchorResolution, AnchorStatus};
use coqo_protocol::methods::threads::{
    self as tm, Author, Backend, ChangesParams, CommentId, CommentParams, CreateParams,
    DeleteCommentResult, DiscardResult, EditCommentParams, ListParams, MAX_COMMENT,
    MAX_THREAD_LINES, PendingComment, PendingReview, ReplyParams, ReviewParams, Thread,
    ThreadAnchor, ThreadEvent, ThreadId, ThreadLines, ThreadList, ThreadParams, ThreadStatus,
};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, Sequenced};
use coqo_storage::Timestamp;
use futures_util::{StreamExt as _, stream};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

pub use self::backend::{LocalBackend, Mirror, ThreadBackend};
use self::store::{NewComment, NewThread, Record, Store, StoreError, StoredAnchor};
use crate::git::GitService;
use crate::questions::anchoring::{self, Job, Text};
use crate::workspace::WorkspaceService;

/// Live changes buffered before a slow subscriber falls back to the store.
const CHANGE_CHANNEL: usize = 256;
/// Threads read per replay page.
const PAGE: usize = 64;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn storage_error(e: &StoreError) -> ErrorObject {
    ErrorObject::new(codes::STORAGE_ERROR, format!("threads store: {e}"))
}

fn unknown(id: ThreadId) -> ErrorObject {
    ErrorObject::new(codes::UNKNOWN_THREAD, format!("no thread {id}"))
}

fn unknown_comment(id: CommentId) -> ErrorObject {
    ErrorObject::new(codes::UNKNOWN_COMMENT, format!("no comment {id}"))
}

fn not_yours(id: CommentId) -> ErrorObject {
    ErrorObject::new(
        codes::NOT_YOUR_COMMENT,
        format!("comment {id} is not yours to change"),
    )
}

fn invalid(message: impl Into<String>) -> ErrorObject {
    ErrorObject::invalid_params(message.into())
}

fn check_body(body: &str) -> Result<String, ErrorObject> {
    let body = body.trim();
    if body.is_empty() {
        return Err(invalid("the comment is empty"));
    }
    if body.len() > MAX_COMMENT {
        return Err(invalid(format!(
            "the comment is longer than {MAX_COMMENT} bytes"
        )));
    }
    Ok(body.to_owned())
}

/// The name of a version, for messages.
fn version_name(v: &DiffSide) -> String {
    match v {
        DiffSide::Worktree => "the working tree".to_owned(),
        DiffSide::Index => "the index".to_owned(),
        DiffSide::Rev(rev) => format!("`{rev}`"),
    }
}

/// What the store and the change seq share a lock for.
#[derive(Debug)]
struct Inner {
    store: Store,
    head: u64,
}

/// A file version's anchors to resolve together: `record index`.
type Groups = BTreeMap<(WorkspaceId, RootId, String, String), Vec<usize>>;

/// The daemon's comment threads. See the module docs.
#[derive(Debug)]
pub struct ThreadService {
    inner: Mutex<Inner>,
    changes: broadcast::Sender<Sequenced<ThreadEvent>>,
    workspaces: Arc<WorkspaceService>,
    git: Arc<GitService>,
    /// The agent sessions a review is sent to.
    sessions: Option<SessionManager>,
    backends: Vec<Arc<dyn ThreadBackend>>,
}

impl ThreadService {
    /// The service over the threads in the global database at `db`,
    /// reading file versions from `workspaces` (the work tree) and `git`
    /// (the index and revisions), sending reviews to `sessions`.
    pub fn open(
        db: &std::path::Path,
        workspaces: Arc<WorkspaceService>,
        git: Arc<GitService>,
        sessions: Option<SessionManager>,
    ) -> Result<Arc<Self>, StoreError> {
        let store = Store::open(db)?;
        let head = store.head()?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner { store, head }),
            changes: broadcast::channel(CHANGE_CHANNEL).0,
            workspaces,
            git,
            sessions,
            backends: vec![Arc::new(LocalBackend)],
        }))
    }

    fn backend(&self, kind: Backend) -> Result<Arc<dyn ThreadBackend>, ErrorObject> {
        self.backends
            .iter()
            .find(|b| b.kind() == kind)
            .cloned()
            .ok_or_else(|| {
                ErrorObject::new(
                    codes::UNSUPPORTED,
                    "GitHub threads come with the PR inbox (M8)",
                )
            })
    }

    async fn mirror(&self, kind: Backend, change: Mirror<'_>) -> Result<(), ErrorObject> {
        self.backend(kind)?.mirror(change).await
    }

    /// A live (not deleted) thread.
    fn record(&self, id: ThreadId) -> Result<Record, ErrorObject> {
        lock(&self.inner)
            .store
            .get(id)
            .map_err(|e| storage_error(&e))?
            .filter(|r| !r.deleted)
            .ok_or_else(|| unknown(id))
    }

    // ---- anchors -----------------------------------------------------------

    /// A file's text as `version` has it now; `None` when that version has
    /// no such file (or it is not text).
    async fn text_of(
        &self,
        workspace: &WorkspaceId,
        root: RootId,
        path: &str,
        version: &DiffSide,
    ) -> Result<Option<String>, ErrorObject> {
        match version {
            DiffSide::Worktree => {
                let read = self
                    .workspaces
                    .read_file(ReadParams {
                        workspace: workspace.clone(),
                        root,
                        path: path.to_owned(),
                        max_bytes: Some(MAX_READ_LIMIT),
                    })
                    .await;
                match read {
                    Ok(read) => Ok(read.text),
                    Err(e) if e.code == codes::NOT_FOUND => Ok(None),
                    Err(e) => Err(e),
                }
            }
            v => self.git.read_text(workspace, root, path, v.clone()).await,
        }
    }

    /// Resolve the anchors of `records` against their versions as they are
    /// now, one read and parse per file version, and store those that moved.
    /// `None` for a thread on a whole file.
    async fn resolve(&self, records: &mut [Record]) -> Vec<Option<AnchorResolution>> {
        let mut out: Vec<Option<AnchorResolution>> = records
            .iter()
            .map(|r| r.anchor.anchor.as_ref().map(|_| anchoring::orphaned()))
            .collect();
        let mut groups = Groups::new();
        for (i, r) in records.iter().enumerate() {
            if r.anchor.anchor.is_some() {
                let version = serde_json::to_string(&r.anchor.version).unwrap_or_default();
                groups
                    .entry((
                        r.workspace.clone(),
                        r.anchor.root,
                        r.anchor.path.clone(),
                        version,
                    ))
                    .or_default()
                    .push(i);
            }
        }
        let mut moved = HashSet::new();
        for ((workspace, root, path, _), members) in groups {
            let version = records[members[0]].anchor.version.clone();
            let text = match self.text_of(&workspace, root, &path, &version).await {
                Ok(Some(text)) => text,
                Ok(None) => continue,
                Err(e) => {
                    tracing::debug!(%workspace, %path, error = %e.message, "thread's file unreadable");
                    continue;
                }
            };
            let jobs: Vec<Job> = members
                .iter()
                .filter_map(|&i| {
                    let a = &records[i].anchor;
                    Some(Job {
                        original: a.anchor.clone()?,
                        current: a.current.clone(),
                        tracked: None,
                    })
                })
                .collect();
            let file = path.clone();
            let outcomes = tokio::task::spawn_blocking(move || {
                let text = Text {
                    text,
                    ..Text::default()
                };
                anchoring::resolve(&file, &text, &jobs)
            })
            .await;
            let Ok(outcomes) = outcomes else {
                tracing::warn!("thread anchor resolution failed");
                continue;
            };
            for (&i, outcome) in members.iter().zip(outcomes) {
                let r = &mut records[i];
                if let Some(anchor) = outcome.moved_to
                    && r.anchor.current.as_ref() != Some(&anchor)
                {
                    r.anchor.current = Some(anchor);
                    moved.insert(i);
                }
                out[i] = Some(outcome.resolution);
            }
        }
        if !moved.is_empty() {
            let inner = lock(&self.inner);
            for i in moved {
                if let Err(e) = inner.store.save_anchor(&records[i]) {
                    tracing::warn!(thread = %records[i].id, error = %e, "cannot store a rebased anchor");
                }
            }
        }
        out
    }

    /// `records` as served now.
    async fn threads(&self, mut records: Vec<Record>) -> Vec<Thread> {
        let resolutions = self.resolve(&mut records).await;
        records
            .iter()
            .zip(resolutions)
            .map(|(r, res)| wire(r, res))
            .collect()
    }

    // ---- changes -----------------------------------------------------------

    /// Change thread `id` under the lock: `f` gets the store and the stored
    /// row (with its anchor rebased where it is now) and may refuse, change
    /// its comments in the store, or change the row; the row is then written
    /// as the next change and published. Returns the thread (or `None` if
    /// `f` deleted it) and what `f` returned.
    async fn change<T>(
        &self,
        id: ThreadId,
        f: impl FnOnce(&Store, &mut Record) -> Result<T, ErrorObject>,
    ) -> Result<(Option<Thread>, T), ErrorObject> {
        let mut records = vec![self.record(id)?];
        let resolution = self.resolve(&mut records).await.remove(0);
        let mut inner = lock(&self.inner);
        let mut record = inner
            .store
            .get(id)
            .map_err(|e| storage_error(&e))?
            .filter(|r| !r.deleted)
            .ok_or_else(|| unknown(id))?;
        let out = f(&inner.store, &mut record)?;
        if let Some(fresh) = inner.store.get(id).map_err(|e| storage_error(&e))? {
            record.comments = fresh.comments;
        }
        if record.comments.is_empty() {
            record.deleted = true;
        }
        record.updated_at = Timestamp::now().0;
        let seq = inner.head + 1;
        inner
            .store
            .bump(&record, seq)
            .map_err(|e| storage_error(&e))?;
        inner.head = seq;
        record.seq = seq;
        let (thread, event) = if record.deleted {
            let event = ThreadEvent::Deleted {
                id,
                workspace: record.workspace.clone(),
            };
            (None, event)
        } else {
            let thread = wire(&record, resolution);
            (
                Some(thread.clone()),
                ThreadEvent::Changed {
                    thread: Box::new(thread),
                },
            )
        };
        let _ = self.changes.send(Sequenced { seq, event });
        Ok((thread, out))
    }

    // ---- methods --------------------------------------------------------------

    /// `threads/create`, by `author` (the user, from the route).
    pub async fn create(&self, p: CreateParams, author: Author) -> Result<Thread, ErrorObject> {
        let body = check_body(&p.body)?;
        let path = p.path.trim_start_matches("./").to_owned();
        if path.is_empty() {
            return Err(invalid("the path is empty"));
        }
        let backend = self.backend(Backend::Local)?;
        let (anchor, resolution) = match p.lines {
            None => (None, None),
            Some(ThreadLines { start, end }) => {
                if start == 0 || end < start {
                    return Err(invalid("lines are 1-based and end after they start"));
                }
                if end - start >= MAX_THREAD_LINES {
                    return Err(invalid(format!(
                        "comment on at most {MAX_THREAD_LINES} lines"
                    )));
                }
                let text = self
                    .text_of(&p.workspace, p.root, &path, &p.version)
                    .await?
                    .ok_or_else(|| {
                        invalid(format!(
                            "{path} is not a text file in {}",
                            version_name(&p.version)
                        ))
                    })?;
                let count = anchoring::line_count(&text);
                if end as usize > count {
                    return Err(invalid(format!(
                        "{path} has {count} lines in {}; line {end} is past its end",
                        version_name(&p.version)
                    )));
                }
                let file = path.clone();
                let made = tokio::task::spawn_blocking(move || {
                    let text = Text {
                        text,
                        ..Text::default()
                    };
                    let lines = (start as usize - 1)..=(end as usize - 1);
                    anchoring::make(&file, &text, std::slice::from_ref(&lines))
                })
                .await
                .map_err(|e| ErrorObject::internal(format!("anchoring failed: {e}")))?
                .map_err(|e| invalid(format!("{path}: {e}")))?;
                let anchor = made.into_iter().next().map(|(a, _)| a);
                let resolution = anchor.as_ref().map(|a| AnchorResolution {
                    status: AnchorStatus::Exact,
                    range: Some(anchoring::code_anchor(p.root, a).range),
                    buffer: None,
                    rev: None,
                });
                (anchor, resolution)
            }
        };
        let now = Timestamp::now().0;
        let new = NewThread {
            workspace: p.workspace,
            backend: backend.kind(),
            diff: p.diff.filter(|d| !d.is_empty()),
            anchor: StoredAnchor {
                root: p.root,
                path,
                side: p.side,
                version: p.version,
                anchor,
                current: None,
            },
            created_at: now,
        };
        let first = NewComment {
            author,
            body,
            pending: p.pending,
            created_at: now,
        };
        let thread = {
            let mut inner = lock(&self.inner);
            let seq = inner.head + 1;
            let record = inner
                .store
                .create(&new, &first, seq)
                .map_err(|e| storage_error(&e))?;
            inner.head = seq;
            let thread = wire(&record, resolution);
            let _ = self.changes.send(Sequenced {
                seq,
                event: ThreadEvent::Changed {
                    thread: Box::new(thread.clone()),
                },
            });
            thread
        };
        tracing::debug!(thread = %thread.id, path = %thread.anchor.path, "thread started");
        self.mirror(thread.backend, Mirror::Created(&thread))
            .await?;
        Ok(thread)
    }

    /// `threads/reply`, by `author` (the user, from the route; agents reply
    /// with [`Self::agent_reply`]).
    pub async fn reply(&self, p: ReplyParams, author: Author) -> Result<Thread, ErrorObject> {
        let body = check_body(&p.body)?;
        // Only the user's comments wait in a review.
        let pending = p.pending && author == Author::User;
        let (thread, comment) = self
            .change(p.thread, |store, _| {
                store
                    .add_comment(
                        p.thread,
                        &NewComment {
                            author,
                            body,
                            pending,
                            created_at: Timestamp::now().0,
                        },
                    )
                    .map_err(|e| storage_error(&e))
            })
            .await?;
        let thread = thread.ok_or_else(|| unknown(p.thread))?;
        self.mirror(thread.backend, Mirror::Commented(&thread, &comment))
            .await?;
        Ok(thread)
    }

    /// The thread `comment` is in, and the comment, if it is the user's.
    fn own_comment(&self, comment: CommentId) -> Result<ThreadId, ErrorObject> {
        let thread = lock(&self.inner)
            .store
            .thread_of(comment)
            .map_err(|e| storage_error(&e))?
            .ok_or_else(|| unknown_comment(comment))?;
        Ok(thread)
    }

    /// `threads/editComment`.
    pub async fn edit_comment(&self, p: EditCommentParams) -> Result<Thread, ErrorObject> {
        let body = check_body(&p.body)?;
        let id = self.own_comment(p.comment)?;
        let (thread, ()) = self
            .change(id, |store, record| {
                let c = record
                    .comments
                    .iter()
                    .find(|c| c.id == p.comment)
                    .ok_or_else(|| unknown_comment(p.comment))?;
                if c.author != Author::User {
                    return Err(not_yours(p.comment));
                }
                store
                    .edit_comment(p.comment, &body, Timestamp::now().0)
                    .map_err(|e| storage_error(&e))
            })
            .await?;
        let thread = thread.ok_or_else(|| unknown(id))?;
        if let Some(c) = thread.comments.iter().find(|c| c.id == p.comment) {
            self.mirror(thread.backend, Mirror::Edited(&thread, c))
                .await?;
        }
        Ok(thread)
    }

    /// `threads/deleteComment`.
    pub async fn delete_comment(
        &self,
        p: CommentParams,
    ) -> Result<DeleteCommentResult, ErrorObject> {
        let id = self.own_comment(p.comment)?;
        let (thread, backend) = self
            .change(id, |store, record| {
                let c = record
                    .comments
                    .iter()
                    .find(|c| c.id == p.comment)
                    .ok_or_else(|| unknown_comment(p.comment))?;
                if c.author != Author::User {
                    return Err(not_yours(p.comment));
                }
                store
                    .delete_comments(&[p.comment])
                    .map_err(|e| storage_error(&e))?;
                Ok(record.backend)
            })
            .await?;
        self.mirror(backend, Mirror::Deleted(thread.as_ref(), p.comment))
            .await?;
        Ok(DeleteCommentResult { thread })
    }

    /// `threads/resolve` and `threads/reopen`.
    pub async fn set_status(
        &self,
        p: ThreadParams,
        status: ThreadStatus,
    ) -> Result<Thread, ErrorObject> {
        if self.record(p.id)?.status == status {
            return self.get(p).await;
        }
        let (thread, ()) = self
            .change(p.id, |_, record| {
                record.status = status;
                record.resolved_at = (status == ThreadStatus::Resolved).then(|| Timestamp::now().0);
                Ok(())
            })
            .await?;
        let thread = thread.ok_or_else(|| unknown(p.id))?;
        self.mirror(thread.backend, Mirror::Status(&thread)).await?;
        Ok(thread)
    }

    /// `threads/list`.
    pub async fn list(&self, p: ListParams) -> Result<ThreadList, ErrorObject> {
        let (records, seq) = {
            let inner = lock(&self.inner);
            let records = inner
                .store
                .list(&p.workspace)
                .map_err(|e| storage_error(&e))?;
            (records, inner.head)
        };
        let path = p.path.as_deref().map(|s| s.trim_start_matches("./"));
        let records = records
            .into_iter()
            .filter(|r| {
                p.root.is_none_or(|root| root == r.anchor.root)
                    && path.is_none_or(|path| path == r.anchor.path)
                    && p.diff.as_ref().is_none_or(|d| r.diff.as_ref() == Some(d))
            })
            .collect();
        Ok(ThreadList {
            threads: self.threads(records).await,
            seq,
        })
    }

    /// `threads/get`.
    pub async fn get(&self, p: ThreadParams) -> Result<Thread, ErrorObject> {
        let record = self.record(p.id)?;
        Ok(self.threads(vec![record]).await.remove(0))
    }

    /// The threads of `p.workspace` (in `p.diff`) with pending comments.
    fn pending_records(&self, p: &ReviewParams) -> Result<Vec<Record>, ErrorObject> {
        let records = lock(&self.inner)
            .store
            .list(&p.workspace)
            .map_err(|e| storage_error(&e))?;
        Ok(records
            .into_iter()
            .filter(|r| p.diff.as_ref().is_none_or(|d| r.diff.as_ref() == Some(d)))
            .filter(|r| r.comments.iter().any(|c| c.pending))
            .collect())
    }

    /// `review/pending`.
    pub async fn pending(&self, p: ReviewParams) -> Result<PendingReview, ErrorObject> {
        let threads = self.threads(self.pending_records(&p)?).await;
        let comments: Vec<PendingComment> = threads
            .into_iter()
            .flat_map(|t| {
                let (thread, diff, anchor) = (t.id, t.diff, t.anchor);
                t.comments
                    .into_iter()
                    .filter(|c| c.pending)
                    .map(move |comment| PendingComment {
                        thread,
                        diff: diff.clone(),
                        anchor: anchor.clone(),
                        comment,
                    })
            })
            .collect();
        Ok(PendingReview {
            count: u32::try_from(comments.len()).unwrap_or(u32::MAX),
            comments,
        })
    }

    /// `review/discard`.
    pub async fn discard(&self, p: ReviewParams) -> Result<DiscardResult, ErrorObject> {
        let mut discarded = 0u32;
        for record in self.pending_records(&p)? {
            let ids: Vec<CommentId> = record
                .comments
                .iter()
                .filter(|c| c.pending)
                .map(|c| c.id)
                .collect();
            let result = self
                .change(record.id, |store, _| {
                    store.delete_comments(&ids).map_err(|e| storage_error(&e))
                })
                .await;
            match result {
                Ok((thread, ())) => {
                    discarded += u32::try_from(ids.len()).unwrap_or(u32::MAX);
                    for id in &ids {
                        self.mirror(record.backend, Mirror::Deleted(thread.as_ref(), *id))
                            .await?;
                    }
                }
                // Deleted meanwhile.
                Err(e) if e.code == codes::UNKNOWN_THREAD => {}
                Err(e) => return Err(e),
            }
        }
        Ok(DiscardResult { discarded })
    }

    /// The `threads/changes` feed: replay from the store after `since`,
    /// then live changes.
    pub fn changes(
        self: &Arc<Self>,
        p: ChangesParams,
        since: Option<u64>,
    ) -> Result<EventFeed<ThreadEvent>, ErrorObject> {
        let rx = self.changes.subscribe();
        let head = lock(&self.inner).head;
        let (after, replaying) = match since {
            None => (head, false),
            Some(n) if n > head => {
                return Err(coqo_protocol::subscription::ResumeGap {
                    needed: n.saturating_add(1),
                    oldest: 1,
                    head,
                }
                .to_error());
            }
            Some(n) => (n, n < head),
        };
        struct Cursor {
            svc: Weak<ThreadService>,
            workspace: Option<WorkspaceId>,
            after: u64,
            replaying: bool,
            buf: VecDeque<Sequenced<ThreadEvent>>,
            rx: broadcast::Receiver<Sequenced<ThreadEvent>>,
        }
        let cursor = Cursor {
            svc: Arc::downgrade(self),
            workspace: p.workspace,
            after,
            replaying,
            buf: VecDeque::new(),
            rx,
        };
        let events = stream::unfold(cursor, |mut c| async move {
            loop {
                if let Some(e) = c.buf.pop_front() {
                    if e.seq > c.after {
                        c.after = e.seq;
                        return Some((Ok(e), c));
                    }
                    continue;
                }
                if c.replaying {
                    let svc = c.svc.upgrade()?;
                    let page =
                        lock(&svc.inner)
                            .store
                            .changed_since(c.after, c.workspace.as_ref(), PAGE);
                    match page {
                        Ok(page) if page.is_empty() => c.replaying = false,
                        Ok(page) => {
                            let (gone, live): (Vec<_>, Vec<_>) =
                                page.into_iter().partition(|r| r.deleted);
                            let mut events: Vec<Sequenced<ThreadEvent>> = gone
                                .into_iter()
                                .map(|r| Sequenced {
                                    seq: r.seq,
                                    event: ThreadEvent::Deleted {
                                        id: r.id,
                                        workspace: r.workspace,
                                    },
                                })
                                .collect();
                            events.extend(svc.threads(live).await.into_iter().map(|t| Sequenced {
                                seq: t.seq,
                                event: ThreadEvent::Changed {
                                    thread: Box::new(t),
                                },
                            }));
                            events.sort_by_key(|e| e.seq);
                            c.buf.extend(events);
                        }
                        Err(e) => return Some((Err(storage_error(&e)), c)),
                    }
                    continue;
                }
                match c.rx.recv().await {
                    Ok(s) => {
                        if s.seq > c.after
                            && c.workspace
                                .as_ref()
                                .is_none_or(|w| w == s.event.workspace())
                        {
                            c.after = s.seq;
                            return Some((Ok(s), c));
                        }
                    }
                    Err(RecvError::Lagged(_)) => c.replaying = true,
                    Err(RecvError::Closed) => return None,
                }
            }
        })
        .boxed();
        Ok(EventFeed { head, events })
    }
}

/// The lines (1-based, inclusive) of a range of whole lines: its end is the
/// start of the line after, unless it ends the text.
fn lines_of(span: &coqo_protocol::methods::questions::AnchorSpan) -> ThreadLines {
    let start = span.start.line;
    let end = if span.end.col == 0 && span.end.line > start {
        span.end.line
    } else {
        span.end.line + 1
    };
    ThreadLines {
        start: start + 1,
        end: end.max(start + 1),
    }
}

/// The wire form of `record`, its anchor resolved as `resolution`.
fn wire(record: &Record, resolution: Option<AnchorResolution>) -> Thread {
    let a = &record.anchor;
    let root = a.root;
    let code = a.anchor.as_ref().map(|x| anchoring::code_anchor(root, x));
    let lines = a.anchor.as_ref().map(|original| {
        let last: &Anchor = a.current.as_ref().unwrap_or(original);
        let span = resolution
            .and_then(|r| r.range)
            .unwrap_or_else(|| anchoring::code_anchor(root, last).range);
        lines_of(&span)
    });
    Thread {
        id: record.id,
        workspace: record.workspace.clone(),
        backend: record.backend,
        diff: record.diff.clone(),
        anchor: ThreadAnchor {
            root,
            path: a.path.clone(),
            side: a.side,
            version: a.version.clone(),
            code,
            resolution,
            lines,
        },
        status: record.status,
        comments: record.comments.clone(),
        created_at: record.created_at,
        updated_at: record.updated_at,
        resolved_at: record.resolved_at,
        seq: record.seq,
    }
}

/// Register `threads/*` and `review/*` on `router`.
pub fn register(router: Router, svc: Arc<ThreadService>) -> Router {
    let list = Arc::clone(&svc);
    let get = Arc::clone(&svc);
    let create = Arc::clone(&svc);
    let reply = Arc::clone(&svc);
    let edit = Arc::clone(&svc);
    let delete = Arc::clone(&svc);
    let resolve = Arc::clone(&svc);
    let reopen = Arc::clone(&svc);
    let pending = Arc::clone(&svc);
    let discard = Arc::clone(&svc);
    let send = Arc::clone(&svc);
    let agent_reply = Arc::clone(&svc);
    let changes = svc;
    router
        .method_with::<tm::List, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&list);
            async move { s.list(p).await }
        })
        .method_with::<tm::Get, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&get);
            async move { s.get(p).await }
        })
        .method_with::<tm::Create, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&create);
            async move { s.create(p, Author::User).await }
        })
        .method_with::<tm::Reply, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&reply);
            async move { s.reply(p, Author::User).await }
        })
        .method_with::<tm::EditComment, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&edit);
            async move { s.edit_comment(p).await }
        })
        .method_with::<tm::DeleteComment, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&delete);
            async move { s.delete_comment(p).await }
        })
        .method_with::<tm::Resolve, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&resolve);
            async move { s.set_status(p, ThreadStatus::Resolved).await }
        })
        .method_with::<tm::Reopen, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&reopen);
            async move { s.set_status(p, ThreadStatus::Open).await }
        })
        .method_with::<tm::Pending, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&pending);
            async move { s.pending(p).await }
        })
        .method_with::<tm::Discard, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&discard);
            async move { s.discard(p).await }
        })
        .method_with::<tm::SendReview, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&send);
            async move { s.send(p).await }
        })
        .method_with::<tm::AgentReply, _, _>(
            Access::check(|scope, p: &tm::AgentReplyParams| {
                scope.require_workspace(&p.workspace.0)?;
                scope.require_session(&p.session.0)
            }),
            move |_ctx, p| {
                let s = Arc::clone(&agent_reply);
                async move { s.agent_reply(p).await }
            },
        )
        .topic_with::<tm::Changes, _, _>(Access::daemon(), move |_ctx, p, since| {
            std::future::ready(changes.changes(p, since))
        })
}
