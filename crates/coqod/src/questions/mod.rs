// SPDX-License-Identifier: Apache-2.0

//! `questions/*`: questions and notes agents pin to code, the anchors
//! service that keeps them on the code, and the user's answers (PLAN §7.5,
//! issue #73, ADR 0016).
//!
//! - **Store.** Every question is a row of `questions` in `coqo.db`
//!   ([`store`]), so questions survive restarts and queue while no UI is
//!   open. Each change gets the next `questions/changes` seq, written with
//!   it, so a UI resumes from the seq of its last list.
//! - **Anchors** ([`anchoring`]). A question is pinned to whole lines of
//!   files as they are when asked: the open buffer with its unsaved edits,
//!   else the file on disk. Every time a question is served its anchors are
//!   resolved against the file as it is now. An anchor made or rebased in an
//!   open buffer is mapped exactly through that buffer's changes; any other
//!   (after a restart, a reopen, or a change on disk) is found again by its
//!   fingerprint. Anchors that moved are rebased and stored (`current`); the
//!   original stays for the snippet an orphan shows.
//! - **Live.** While a file with open questions is open, the service
//!   follows its buffer's changes (without keeping it loaded), re-resolves
//!   its anchors after each burst and publishes the questions whose
//!   resolution changed on `questions/changes`.
//! - **Waiting.** `questions/watch` is how an agent blocks on an answer: the
//!   question now, then every change. While one is followed the question is
//!   `waiting`. Dropping the subscription (the agent cancelled its tool
//!   call) ends the wait.
//! - **Sessions.** While a session has an open question its state is
//!   `needs_input`; when the last is answered or dismissed it goes back
//!   (`working` while a turn runs, else `done`). Questions and their
//!   answers are recorded in the session's transcript.
//!
//! **Access:** listing, answering, dismissing and following changes take
//! the daemon token. A session token may create and watch questions of its
//! own workspace and agent session only.

pub(crate) mod anchoring;
pub(crate) mod store;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use coqo_agents::SessionManager;
use coqo_protocol::methods::agents::{AgentState, SessionId};
use coqo_protocol::methods::buffer::BufferId;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{MAX_READ_LIMIT, ReadParams};
use coqo_protocol::methods::questions::{
    self as qm, AnchorResolution, AnchorStatus, AnswerParams, ChangesParams, CreateParams,
    ListParams, MAX_ANCHOR_LINES, MAX_ANCHORS, MAX_ANSWER, MAX_BODY, MAX_OPTIONS, Question,
    QuestionAnchor, QuestionAnswer, QuestionId, QuestionKind, QuestionList, QuestionOption,
    QuestionParams, QuestionStatus, WatchParams,
};
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, Sequenced};
use coqo_storage::Timestamp;
use futures_util::{FutureExt as _, StreamExt as _, stream};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use self::anchoring::{Job, Outcome, Text, Tracked};
use self::store::{NewRecord, Record, Store, StoreError, StoredAnchor};
use crate::buffers::BufferService;
use crate::workspace::WorkspaceService;

/// Live changes buffered before a slow subscriber falls back to the store.
const CHANGE_CHANNEL: usize = 256;
/// Questions read per replay page.
const PAGE: usize = 64;
/// Closed questions `questions/list` returns at most.
const CLOSED_LIMIT: usize = 200;
/// How long the live tracker lets a burst of edits settle.
const SETTLE: Duration = Duration::from_millis(120);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn storage_error(e: &StoreError) -> ErrorObject {
    ErrorObject::new(codes::STORAGE_ERROR, format!("questions store: {e}"))
}

fn unknown(id: QuestionId) -> ErrorObject {
    ErrorObject::new(codes::UNKNOWN_QUESTION, format!("no question {id}"))
}

fn closed(id: QuestionId, status: QuestionStatus) -> ErrorObject {
    let what = match status {
        QuestionStatus::Answered => "answered",
        _ => "dismissed",
    };
    ErrorObject::new(
        codes::QUESTION_CLOSED,
        format!("question {id} was already {what}"),
    )
}

fn invalid(message: impl Into<String>) -> ErrorObject {
    ErrorObject::invalid_params(message.into())
}

/// What the store and the change seq share a lock for.
#[derive(Debug)]
struct Inner {
    store: Store,
    head: u64,
}

/// Anchors tracked in open buffers, the buffers followed, what was last
/// published, and who waits.
#[derive(Debug, Default)]
struct Live {
    /// By question and anchor index.
    tracked: HashMap<(QuestionId, usize), Tracked>,
    /// Buffers whose changes are followed.
    followed: HashSet<BufferId>,
    /// The resolutions each open question was last published with.
    published: HashMap<QuestionId, Vec<AnchorResolution>>,
    /// Agents waiting on each question.
    watchers: HashMap<QuestionId, usize>,
}

/// The daemon's question store and anchors service.
#[derive(Debug)]
pub struct QuestionService {
    inner: Mutex<Inner>,
    changes: broadcast::Sender<Sequenced<Question>>,
    workspaces: Arc<WorkspaceService>,
    buffers: Arc<BufferService>,
    sessions: Option<SessionManager>,
    live: Mutex<Live>,
}

/// An open question, for alerts ([`QuestionService::open_questions_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQuestion {
    /// Its id.
    pub id: QuestionId,
    /// The question, markdown.
    pub body: String,
    /// Its first code location: root, path and 0-based line.
    pub location: Option<(RootId, String, u32)>,
}

/// The lines asked about in one file: `(anchor index, lines)`.
type Lines = Vec<(usize, RangeInclusive<usize>)>;

/// A file's anchors to resolve together: `(record index, anchor index)`.
type Groups = BTreeMap<(WorkspaceId, RootId, String), Vec<(usize, usize)>>;

impl QuestionService {
    /// The service over the questions in the global database at `db`,
    /// resolving anchors in `workspaces`' files and `buffers`, and telling
    /// `sessions` when one waits for an answer.
    pub fn open(
        db: &Path,
        workspaces: Arc<WorkspaceService>,
        buffers: Arc<BufferService>,
        sessions: Option<SessionManager>,
    ) -> Result<Arc<Self>, StoreError> {
        let store = Store::open(db)?;
        let head = store.head()?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner { store, head }),
            changes: broadcast::channel(CHANGE_CHANNEL).0,
            workspaces,
            buffers,
            sessions,
            live: Mutex::default(),
        }))
    }

    fn record(&self, id: QuestionId) -> Result<Record, ErrorObject> {
        lock(&self.inner)
            .store
            .get(id)
            .map_err(|e| storage_error(&e))?
            .ok_or_else(|| unknown(id))
    }

    // ---- anchors -----------------------------------------------------------

    /// The text of a file as anchors see it now, with the changes since
    /// `since(buffer)` if it is open.
    async fn text_of(
        &self,
        workspace: &WorkspaceId,
        root: RootId,
        path: &str,
        since: impl FnOnce(BufferId) -> Option<u64>,
    ) -> Result<Text, ErrorObject> {
        let file = self
            .workspaces
            .resolve_file(workspace.clone(), root, path.to_owned())
            .await?;
        if let Some(buffer) = self.buffers.buffer_at(workspace, &file.real) {
            match self.buffers.anchor_view(buffer, since(buffer)) {
                Ok(view) => {
                    return Ok(Text {
                        text: view.text,
                        buffer: Some((buffer, view.rev)),
                        changes: view.changes,
                        since: view.since,
                    });
                }
                // Unloaded meanwhile: read the file.
                Err(e) => tracing::debug!(%buffer, error = %e.message, "buffer gone"),
            }
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

    /// Resolve every anchor of `records` against the files as they are now,
    /// one read and parse per file. Tracks anchors found in open buffers,
    /// follows those buffers, and stores anchors that moved.
    async fn resolve(self: &Arc<Self>, records: &mut [Record]) -> Vec<Vec<AnchorResolution>> {
        let mut out: Vec<Vec<AnchorResolution>> = records
            .iter()
            .map(|r| vec![anchoring::orphaned(); r.anchors.len()])
            .collect();
        let mut groups = Groups::new();
        for (ri, r) in records.iter().enumerate() {
            for (ai, a) in r.anchors.iter().enumerate() {
                groups
                    .entry((r.workspace.clone(), a.root, a.anchor.path.clone()))
                    .or_default()
                    .push((ri, ai));
            }
        }
        let mut moved = HashSet::new();
        for ((workspace, root, path), members) in groups {
            let jobs: Vec<Job> = {
                let live = lock(&self.live);
                members
                    .iter()
                    .map(|&(ri, ai)| {
                        let r = &records[ri];
                        Job {
                            original: r.anchors[ai].anchor.clone(),
                            current: r.anchors[ai].current.clone(),
                            tracked: live.tracked.get(&(r.id, ai)).cloned(),
                        }
                    })
                    .collect()
            };
            let since = |buffer: BufferId| {
                jobs.iter()
                    .filter_map(|j| j.tracked.as_ref())
                    .filter(|t| t.buffer == buffer)
                    .filter_map(|t| t.anchor.rev)
                    .min()
            };
            let text = match self.text_of(&workspace, root, &path, since).await {
                Ok(text) => text,
                Err(e) => {
                    tracing::debug!(%workspace, %path, error = %e.message, "anchored file unreadable");
                    let mut live = lock(&self.live);
                    for &(ri, ai) in &members {
                        live.tracked.remove(&(records[ri].id, ai));
                    }
                    continue;
                }
            };
            let buffer = text.buffer.map(|(id, _)| id);
            let file = path.clone();
            let outcomes: Vec<Outcome> =
                match tokio::task::spawn_blocking(move || anchoring::resolve(&file, &text, &jobs))
                    .await
                {
                    Ok(outcomes) => outcomes,
                    Err(e) => {
                        tracing::warn!(error = %e, "anchor resolution failed");
                        continue;
                    }
                };
            let mut follow = false;
            {
                let mut live = lock(&self.live);
                for (&(ri, ai), outcome) in members.iter().zip(outcomes) {
                    let r = &mut records[ri];
                    let key = (r.id, ai);
                    match outcome.tracked {
                        Some(t) if r.status == QuestionStatus::Open => {
                            live.tracked.insert(key, t);
                            follow = true;
                        }
                        _ => {
                            live.tracked.remove(&key);
                        }
                    }
                    if let Some(anchor) = outcome.moved_to
                        && r.anchors[ai].current.as_ref() != Some(&anchor)
                    {
                        r.anchors[ai].current = Some(anchor);
                        moved.insert(ri);
                    }
                    out[ri][ai] = outcome.resolution;
                }
            }
            if follow && let Some(buffer) = buffer {
                self.follow(buffer);
            }
        }
        if !moved.is_empty() {
            let inner = lock(&self.inner);
            for ri in moved {
                if let Err(e) = inner.store.save_anchors(&records[ri]) {
                    tracing::warn!(question = %records[ri].id, error = %e, "cannot store a rebased anchor");
                }
            }
        }
        out
    }

    /// The wire form of `record` with `resolutions`.
    fn question(&self, record: &Record, resolutions: Vec<AnchorResolution>) -> Question {
        let waiting = record.status == QuestionStatus::Open
            && lock(&self.live)
                .watchers
                .get(&record.id)
                .is_some_and(|n| *n > 0);
        Question {
            id: record.id,
            workspace: record.workspace.clone(),
            session: record.session.clone(),
            kind: record.kind,
            severity: record.severity,
            body: record.body.clone(),
            options: record.options.clone(),
            anchors: record
                .anchors
                .iter()
                .zip(resolutions)
                .map(|(a, resolution)| QuestionAnchor {
                    anchor: anchoring::code_anchor(a.root, &a.anchor),
                    resolution,
                })
                .collect(),
            status: record.status,
            answer: record.answer.clone(),
            waiting,
            created_at: record.created_at,
            closed_at: record.closed_at,
            seq: record.seq,
        }
    }

    /// `records` as served now.
    async fn questions(self: &Arc<Self>, mut records: Vec<Record>) -> Vec<Question> {
        let resolutions = self.resolve(&mut records).await;
        records
            .iter()
            .zip(resolutions)
            .map(|(r, res)| self.question(r, res))
            .collect()
    }

    /// Write `record` as the next change and publish it. `check` runs under
    /// the lock on the stored row first, which it may refuse or change.
    fn publish(
        &self,
        id: QuestionId,
        resolutions: Vec<AnchorResolution>,
        check: impl FnOnce(&mut Record) -> Result<(), ErrorObject>,
    ) -> Result<Question, ErrorObject> {
        let mut inner = lock(&self.inner);
        let mut record = inner
            .store
            .get(id)
            .map_err(|e| storage_error(&e))?
            .ok_or_else(|| unknown(id))?;
        check(&mut record)?;
        let seq = inner.head + 1;
        inner
            .store
            .update(&record, seq)
            .map_err(|e| storage_error(&e))?;
        inner.head = seq;
        record.seq = seq;
        let question = self.question(&record, resolutions);
        self.remember(&question);
        let _ = self.changes.send(Sequenced {
            seq,
            event: question.clone(),
        });
        Ok(question)
    }

    /// Keep what an open question was published with; forget a closed one.
    fn remember(&self, q: &Question) {
        let mut live = lock(&self.live);
        if q.status == QuestionStatus::Open {
            live.published
                .insert(q.id, q.anchors.iter().map(|a| a.resolution).collect());
        } else {
            live.published.remove(&q.id);
            live.tracked.retain(|(id, _), _| *id != q.id);
        }
    }

    /// Publish question `id` again as it is now (it started or stopped
    /// waiting), if it is open.
    async fn touch(self: &Arc<Self>, id: QuestionId) {
        let Ok(record) = self.record(id) else {
            return;
        };
        if record.status != QuestionStatus::Open {
            return;
        }
        let mut records = vec![record];
        let resolutions = self.resolve(&mut records).await.remove(0);
        let result = self.publish(id, resolutions, |r| {
            if r.status == QuestionStatus::Open {
                Ok(())
            } else {
                Err(closed(id, r.status))
            }
        });
        if let Err(e) = result
            && e.code != codes::QUESTION_CLOSED
        {
            tracing::warn!(question = %id, error = %e.message, "cannot publish a question");
        }
    }

    // ---- live tracking ---------------------------------------------------------

    /// Follow `buffer`'s changes while it is open, republishing the open
    /// questions whose anchors in it moved.
    fn follow(self: &Arc<Self>, buffer: BufferId) {
        if !lock(&self.live).followed.insert(buffer) {
            return;
        }
        let Ok(mut changes) = self.buffers.follow_changes(buffer) else {
            lock(&self.live).followed.remove(&buffer);
            return;
        };
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut open = true;
            while open {
                match changes.next().await {
                    Some(Ok(_)) => {}
                    // Fell behind the buffer's log: follow it afresh; the
                    // refresh resolves without the lost changes.
                    Some(Err(_)) => {
                        match weak.upgrade().map(|s| s.buffers.follow_changes(buffer)) {
                            Some(Ok(again)) => changes = again,
                            _ => break,
                        }
                    }
                    None => break,
                }
                tokio::time::sleep(SETTLE).await;
                loop {
                    match changes.next().now_or_never() {
                        Some(Some(_)) => {}
                        Some(None) => {
                            open = false;
                            break;
                        }
                        None => break,
                    }
                }
                let Some(svc) = weak.upgrade() else {
                    return;
                };
                if !svc.refresh(buffer).await {
                    // Nothing left to track here, unless a question was
                    // asked meanwhile: then go on.
                    let mut live = lock(&svc.live);
                    if !live.tracked.values().any(|t| t.buffer == buffer) {
                        live.followed.remove(&buffer);
                        return;
                    }
                }
            }
            // The buffer was unloaded: its revisions mean nothing any more.
            if let Some(svc) = weak.upgrade() {
                let mut live = lock(&svc.live);
                live.followed.remove(&buffer);
                live.tracked.retain(|_, t| t.buffer != buffer);
            }
        });
    }

    /// Re-resolve the open questions tracked in `buffer` and publish those
    /// whose resolution changed. Returns whether any is still tracked there.
    async fn refresh(self: &Arc<Self>, buffer: BufferId) -> bool {
        let ids: Vec<QuestionId> = {
            let live = lock(&self.live);
            let mut ids: Vec<_> = live
                .tracked
                .iter()
                .filter(|(_, t)| t.buffer == buffer)
                .map(|((id, _), _)| *id)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        if ids.is_empty() {
            return false;
        }
        let records: Vec<Record> = {
            let inner = lock(&self.inner);
            ids.iter()
                .filter_map(|id| inner.store.get(*id).ok().flatten())
                .filter(|r| r.status == QuestionStatus::Open)
                .collect()
        };
        let mut records = records;
        let resolutions = self.resolve(&mut records).await;
        for (record, res) in records.iter().zip(resolutions) {
            let changed = lock(&self.live).published.get(&record.id) != Some(&res);
            if changed {
                let id = record.id;
                let result = self.publish(id, res, |r| {
                    if r.status == QuestionStatus::Open {
                        Ok(())
                    } else {
                        Err(closed(id, r.status))
                    }
                });
                if let Err(e) = result
                    && e.code != codes::QUESTION_CLOSED
                {
                    tracing::warn!(question = %id, error = %e.message, "cannot publish a moved anchor");
                }
            }
        }
        lock(&self.live)
            .tracked
            .values()
            .any(|t| t.buffer == buffer)
    }

    // ---- sessions -------------------------------------------------------------

    /// Tell the asking session: record the question in its transcript and
    /// set or clear `needs_input`.
    fn tell_session(&self, q: &Question) {
        let Some(sessions) = &self.sessions else {
            return;
        };
        if let Err(e) = sessions.record_question(&q.session, q.clone()) {
            tracing::debug!(session = %q.session, error = %e, "cannot record a question");
        }
        if !q.kind.is_question() {
            return;
        }
        let open = lock(&self.inner).store.open_questions_of(&q.session);
        let reason = match open {
            Ok(open) => open.first().map(|r| waiting_reason(&r.body)),
            Err(e) => {
                tracing::warn!(session = %q.session, error = %e, "cannot count open questions");
                return;
            }
        };
        // While a question waits the session needs the user's input; once
        // the last one is closed it goes back to work, or is done if its
        // turn has ended meanwhile. A state that is not ours to end (an
        // error, `blocked`, a permission request) is left alone.
        let reported = match reason {
            Some(reason) => sessions
                .report_state(&q.session, AgentState::NeedsInput, Some(reason))
                .map(drop),
            None => match sessions.snapshot(&q.session) {
                Ok(s) if s.info.state == AgentState::NeedsInput && !s.info.closed => {
                    let state = if s.running {
                        AgentState::Working
                    } else {
                        AgentState::Done
                    };
                    sessions.report_state(&q.session, state, None).map(drop)
                }
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = reported {
            tracing::debug!(session = %q.session, error = %e, "cannot report the session's state");
        }
    }

    /// The open questions (not notes) of `session`, oldest first, with
    /// where each was first pinned (its first anchor, where it was last
    /// found). Cheap: no anchor is resolved. For alerts.
    pub fn open_questions_of(&self, session: &SessionId) -> Vec<OpenQuestion> {
        let open = lock(&self.inner).store.open_questions_of(session);
        match open {
            Ok(records) => records
                .into_iter()
                .map(|r| OpenQuestion {
                    id: r.id,
                    body: r.body,
                    location: r.anchors.first().map(|a| {
                        let at =
                            anchoring::code_anchor(a.root, a.current.as_ref().unwrap_or(&a.anchor));
                        (a.root, at.path, at.range.start.line)
                    }),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(%session, error = %e, "cannot list open questions");
                Vec::new()
            }
        }
    }

    // ---- methods --------------------------------------------------------------

    /// `questions/create`.
    pub async fn create(self: &Arc<Self>, p: CreateParams) -> Result<Question, ErrorObject> {
        let body = p.body.trim();
        if body.is_empty() {
            return Err(invalid("the question is empty"));
        }
        if body.len() > MAX_BODY {
            return Err(invalid(format!(
                "the question is longer than {MAX_BODY} bytes"
            )));
        }
        let options = options(p.kind, p.options)?;
        if p.anchors.is_empty() || p.anchors.len() > MAX_ANCHORS {
            return Err(invalid(format!(
                "pin it to 1 to {MAX_ANCHORS} code locations"
            )));
        }
        let mut by_file: BTreeMap<(RootId, String), Lines> = BTreeMap::new();
        for (i, a) in p.anchors.iter().enumerate() {
            let (start, end) = (a.lines.start, a.lines.end);
            if end < start {
                return Err(invalid("the lines end before they start"));
            }
            if end - start >= MAX_ANCHOR_LINES {
                return Err(invalid(format!(
                    "pin it to at most {MAX_ANCHOR_LINES} lines"
                )));
            }
            let path = a.path.trim_start_matches("./").to_owned();
            by_file
                .entry((a.root, path))
                .or_default()
                .push((i, start as usize..=end as usize));
        }

        let mut anchors: Vec<Option<(StoredAnchor, Option<Tracked>, AnchorResolution)>> =
            vec![None; p.anchors.len()];
        let mut buffers = Vec::new();
        for ((root, path), lines) in by_file {
            let text = self.text_of(&p.workspace, root, &path, |_| None).await?;
            let count = anchoring::line_count(&text.text);
            if let Some((_, past)) = lines.iter().find(|(_, l)| *l.end() >= count) {
                return Err(invalid(format!(
                    "{path} has {count} lines; line {} is past its end",
                    past.end() + 1
                )));
            }
            let ranges: Vec<_> = lines.iter().map(|(_, l)| l.clone()).collect();
            let file = path.clone();
            let (made, buffer) = tokio::task::spawn_blocking(move || {
                (anchoring::make(&file, &text, &ranges), text.buffer)
            })
            .await
            .map_err(|e| ErrorObject::internal(format!("anchoring failed: {e}")))?;
            let made = made.map_err(|e| invalid(format!("{path}: {e}")))?;
            if let Some((id, _)) = buffer {
                buffers.push(id);
            }
            for ((i, _), (anchor, tracked)) in lines.into_iter().zip(made) {
                let resolution = AnchorResolution {
                    status: AnchorStatus::Exact,
                    range: Some(anchoring::code_anchor(root, &anchor).range),
                    buffer: buffer.map(|(id, _)| id),
                    rev: buffer.map(|(_, rev)| rev),
                };
                anchors[i] = Some((
                    StoredAnchor {
                        root,
                        anchor,
                        current: None,
                    },
                    tracked,
                    resolution,
                ));
            }
        }
        let anchors: Vec<_> = anchors.into_iter().flatten().collect();
        let new = NewRecord {
            workspace: p.workspace,
            session: SessionId(p.session),
            kind: p.kind,
            severity: (p.kind == QuestionKind::Annotation).then(|| p.severity.unwrap_or_default()),
            body: body.to_owned(),
            options,
            anchors: anchors.iter().map(|(a, ..)| a.clone()).collect(),
            created_at: Timestamp::now().0,
        };
        let resolutions: Vec<_> = anchors.iter().map(|(.., r)| *r).collect();
        let question = {
            let mut inner = lock(&self.inner);
            let seq = inner.head + 1;
            let record = inner
                .store
                .insert(&new, seq)
                .map_err(|e| storage_error(&e))?;
            inner.head = seq;
            {
                let mut live = lock(&self.live);
                for (i, (_, tracked, _)) in anchors.into_iter().enumerate() {
                    if let Some(t) = tracked {
                        live.tracked.insert((record.id, i), t);
                    }
                }
            }
            let question = self.question(&record, resolutions);
            self.remember(&question);
            let _ = self.changes.send(Sequenced {
                seq,
                event: question.clone(),
            });
            question
        };
        for buffer in buffers {
            self.follow(buffer);
        }
        tracing::debug!(question = %question.id, session = %question.session, kind = ?question.kind, "question asked");
        self.tell_session(&question);
        Ok(question)
    }

    /// `questions/list`.
    pub async fn list(self: &Arc<Self>, p: ListParams) -> Result<QuestionList, ErrorObject> {
        let (records, seq) = {
            let inner = lock(&self.inner);
            let records = inner
                .store
                .list(
                    &p.workspace,
                    p.session.as_ref(),
                    p.include_closed,
                    CLOSED_LIMIT,
                )
                .map_err(|e| storage_error(&e))?;
            (records, inner.head)
        };
        let path = p.path.as_deref().map(|s| s.trim_start_matches("./"));
        let records = records
            .into_iter()
            .filter(|r| {
                path.is_none_or(|path| {
                    r.anchors
                        .iter()
                        .any(|a| a.anchor.path == path && p.root.is_none_or(|root| root == a.root))
                })
            })
            .collect();
        Ok(QuestionList {
            questions: self.questions(records).await,
            seq,
        })
    }

    /// `questions/get`.
    pub async fn get(self: &Arc<Self>, p: QuestionParams) -> Result<Question, ErrorObject> {
        let record = self.record(p.id)?;
        Ok(self.questions(vec![record]).await.remove(0))
    }

    /// `questions/answer`.
    pub async fn answer(self: &Arc<Self>, p: AnswerParams) -> Result<Question, ErrorObject> {
        let record = self.record(p.id)?;
        if record.status != QuestionStatus::Open {
            return Err(closed(p.id, record.status));
        }
        let answer = check_answer(&record, p.option, p.text)?;
        let mut records = vec![record];
        let resolutions = self.resolve(&mut records).await.remove(0);
        let id = p.id;
        let question = self.publish(id, resolutions, move |r| {
            if r.status != QuestionStatus::Open {
                return Err(closed(id, r.status));
            }
            r.status = QuestionStatus::Answered;
            r.answer = Some(answer);
            r.closed_at = Some(Timestamp::now().0);
            Ok(())
        })?;
        tracing::debug!(question = %id, "question answered");
        self.tell_session(&question);
        Ok(question)
    }

    /// `questions/dismiss`.
    pub async fn dismiss(self: &Arc<Self>, p: QuestionParams) -> Result<Question, ErrorObject> {
        let record = self.record(p.id)?;
        if record.status != QuestionStatus::Open {
            return Err(closed(p.id, record.status));
        }
        let mut records = vec![record];
        let resolutions = self.resolve(&mut records).await.remove(0);
        let id = p.id;
        let question = self.publish(id, resolutions, move |r| {
            if r.status != QuestionStatus::Open {
                return Err(closed(id, r.status));
            }
            r.status = QuestionStatus::Dismissed;
            r.closed_at = Some(Timestamp::now().0);
            Ok(())
        })?;
        self.tell_session(&question);
        Ok(question)
    }

    /// The `questions/changes` feed: replay from the store after `since`,
    /// then live changes.
    pub fn changes(
        self: &Arc<Self>,
        p: ChangesParams,
        since: Option<u64>,
    ) -> Result<EventFeed<Question>, ErrorObject> {
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
            svc: Weak<QuestionService>,
            workspace: Option<WorkspaceId>,
            after: u64,
            replaying: bool,
            buf: std::collections::VecDeque<Question>,
            rx: broadcast::Receiver<Sequenced<Question>>,
        }
        let cursor = Cursor {
            svc: Arc::downgrade(self),
            workspace: p.workspace,
            after,
            replaying,
            buf: std::collections::VecDeque::new(),
            rx,
        };
        let events = stream::unfold(cursor, |mut c| async move {
            loop {
                if let Some(q) = c.buf.pop_front() {
                    if q.seq > c.after {
                        c.after = q.seq;
                        return Some((
                            Ok(Sequenced {
                                seq: q.seq,
                                event: q,
                            }),
                            c,
                        ));
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
                        Ok(page) => c.buf.extend(svc.questions(page).await),
                        Err(e) => return Some((Err(storage_error(&e)), c)),
                    }
                    continue;
                }
                match c.rx.recv().await {
                    Ok(s) => {
                        if s.seq > c.after
                            && c.workspace.as_ref().is_none_or(|w| *w == s.event.workspace)
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

    /// The `questions/watch` feed: the question now, then its changes. The
    /// question is `waiting` until the feed is dropped.
    pub async fn watch(
        self: &Arc<Self>,
        p: WatchParams,
    ) -> Result<EventFeed<Question>, ErrorObject> {
        let record = self.record(p.id)?;
        if record.session.0 != p.session || record.workspace != p.workspace {
            // Not this session's: as if it did not exist.
            return Err(unknown(p.id));
        }
        let rx = self.changes.subscribe();
        let guard = Waiting::start(self, p.id);
        if guard.first {
            self.touch(p.id).await;
        }
        let now = self.get(QuestionParams { id: p.id }).await?;
        let id = p.id;
        let weak = Arc::downgrade(self);
        let head = now.seq;
        let live = stream::unfold(
            (rx, head, weak),
            move |(mut rx, mut last, weak)| async move {
                loop {
                    match rx.recv().await {
                        Ok(s) if s.event.id == id && s.seq > last => {
                            last = s.seq;
                            return Some((Ok(s), (rx, last, weak)));
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {
                            let svc = weak.upgrade()?;
                            if let Ok(q) = svc.get(QuestionParams { id }).await
                                && q.seq > last
                            {
                                last = q.seq;
                                return Some((
                                    Ok(Sequenced {
                                        seq: q.seq,
                                        event: q,
                                    }),
                                    (rx, last, weak),
                                ));
                            }
                        }
                        Err(RecvError::Closed) => return None,
                    }
                }
            },
        );
        let events = stream::once(std::future::ready(Ok(Sequenced {
            seq: now.seq,
            event: now,
        })))
        .chain(live)
        .map(move |e| {
            let _waiting = &guard;
            e
        })
        .boxed();
        Ok(EventFeed { head, events })
    }
}

/// Counts an agent waiting on a question; the last one leaving publishes
/// that nobody waits any more.
#[derive(Debug)]
struct Waiting {
    svc: Weak<QuestionService>,
    id: QuestionId,
    first: bool,
}

impl Waiting {
    fn start(svc: &Arc<QuestionService>, id: QuestionId) -> Self {
        let mut live = lock(&svc.live);
        let n = live.watchers.entry(id).or_default();
        *n += 1;
        Self {
            svc: Arc::downgrade(svc),
            id,
            first: *n == 1,
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let Some(svc) = self.svc.upgrade() else {
            return;
        };
        let last = {
            let mut live = lock(&svc.live);
            let n = live.watchers.entry(self.id).or_default();
            *n = n.saturating_sub(1);
            let last = *n == 0;
            if last {
                live.watchers.remove(&self.id);
            }
            last
        };
        if last && let Ok(rt) = tokio::runtime::Handle::try_current() {
            let id = self.id;
            rt.spawn(async move { svc.touch(id).await });
        }
    }
}

/// The options a question of `kind` offers.
fn options(
    kind: QuestionKind,
    offered: Vec<qm::NewOption>,
) -> Result<Vec<QuestionOption>, ErrorObject> {
    let fixed = |opts: &[(&str, &str)]| {
        opts.iter()
            .map(|(id, label)| QuestionOption {
                id: (*id).to_owned(),
                label: (*label).to_owned(),
                description: None,
            })
            .collect()
    };
    match kind {
        QuestionKind::Decision => {
            if offered.len() < 2 || offered.len() > MAX_OPTIONS {
                return Err(invalid(format!(
                    "a decision offers 2 to {MAX_OPTIONS} options"
                )));
            }
            let mut seen = HashSet::new();
            offered
                .into_iter()
                .enumerate()
                .map(|(i, o)| {
                    let label = o.label.trim().to_owned();
                    if label.is_empty() {
                        return Err(invalid("an option has no label"));
                    }
                    if !seen.insert(label.clone()) {
                        return Err(invalid(format!("option `{label}` is offered twice")));
                    }
                    Ok(QuestionOption {
                        id: (i + 1).to_string(),
                        label,
                        description: o.description.filter(|d| !d.trim().is_empty()),
                    })
                })
                .collect()
        }
        _ if !offered.is_empty() => Err(invalid(
            "only a decision takes options (a confirmation is yes or no)",
        )),
        QuestionKind::Confirmation => Ok(fixed(&[("yes", "Yes"), ("no", "No")])),
        QuestionKind::Info => Ok(fixed(&[("ok", "OK")])),
        QuestionKind::Clarification | QuestionKind::Annotation => Ok(Vec::new()),
    }
}

/// The answer to `record`, if it fits its kind.
fn check_answer(
    record: &Record,
    option: Option<String>,
    text: Option<String>,
) -> Result<QuestionAnswer, ErrorObject> {
    let text = text.filter(|t| !t.trim().is_empty());
    if text.as_ref().is_some_and(|t| t.len() > MAX_ANSWER) {
        return Err(invalid(format!(
            "the answer is longer than {MAX_ANSWER} bytes"
        )));
    }
    if let Some(option) = &option
        && !record.options.iter().any(|o| o.id == *option)
    {
        let ids: Vec<_> = record.options.iter().map(|o| o.id.as_str()).collect();
        return Err(invalid(format!(
            "question {} has no option `{option}` (options: {ids:?})",
            record.id
        )));
    }
    match record.kind {
        QuestionKind::Annotation => Err(invalid("a note is dismissed, not answered")),
        QuestionKind::Clarification if text.is_none() => Err(invalid("the answer is empty")),
        QuestionKind::Confirmation if option.is_none() => Err(invalid("answer `yes` or `no`")),
        QuestionKind::Decision if option.is_none() && text.is_none() => {
            Err(invalid("choose an option or reply"))
        }
        QuestionKind::Info => Ok(QuestionAnswer {
            option: Some(option.unwrap_or_else(|| "ok".to_owned())),
            text,
        }),
        _ => Ok(QuestionAnswer { option, text }),
    }
}

/// The session state's reason while it waits: the question's first line.
fn waiting_reason(body: &str) -> String {
    const MAX: usize = 120;
    let first = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut line: String = first.chars().take(MAX).collect();
    if first.chars().count() > MAX {
        line.push('…');
    }
    format!("Waiting for your answer: {line}")
}

/// Register `questions/*` on `router`.
pub fn register(router: Router, svc: Arc<QuestionService>) -> Router {
    let list = Arc::clone(&svc);
    let get = Arc::clone(&svc);
    let answer = Arc::clone(&svc);
    let dismiss = Arc::clone(&svc);
    let create = Arc::clone(&svc);
    let changes = Arc::clone(&svc);
    let watch = svc;
    router
        .method_with::<qm::List, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&list);
            async move { s.list(p).await }
        })
        .method_with::<qm::Get, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&get);
            async move { s.get(p).await }
        })
        .method_with::<qm::Answer, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&answer);
            async move { s.answer(p).await }
        })
        .method_with::<qm::Dismiss, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&dismiss);
            async move { s.dismiss(p).await }
        })
        .method_with::<qm::Create, _, _>(
            Access::check(|scope, p: &CreateParams| {
                scope.require_workspace(&p.workspace.0)?;
                scope.require_session(&p.session)
            }),
            move |_ctx, p| {
                let s = Arc::clone(&create);
                async move { s.create(p).await }
            },
        )
        .topic_with::<qm::Changes, _, _>(Access::daemon(), move |_ctx, p, since| {
            std::future::ready(changes.changes(p, since))
        })
        .topic_with::<qm::Watch, _, _>(
            Access::check(|scope, p: &WatchParams| {
                scope.require_workspace(&p.workspace.0)?;
                scope.require_session(&p.session)
            }),
            move |_ctx, p, _since| {
                let s = Arc::clone(&watch);
                async move { s.watch(p).await }
            },
        )
}
