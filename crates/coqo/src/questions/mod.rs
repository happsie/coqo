// SPDX-License-Identifier: Apache-2.0

//! Anchored questions (PLAN §7.5, ADR 0016, design board 2): what the user
//! sees when an agent asks a question pinned to code, or leaves a note there.
//!
//! - [`WorkspaceQuestions`]: a workspace's questions and notes, from
//!   `questions/list` (open ones and the recent history), kept current by
//!   `questions/changes` through a [`Resumable`]. One per workspace, shared by
//!   the windows ([`questions_for`]). It also counts what is open and what an
//!   agent is blocked on, for badges and alerts.
//! - In a file tab ([`crate::files::editor`]): a marker on every anchored
//!   line (sand for a question, blue for a note, rose for a warning, pulsing
//!   while an agent waits) and the [`popover`] anchored to the code, where
//!   the user answers. The tab lists its file's questions again after
//!   `buffer/open`, so the ranges are of the open buffer.
//! - The Questions sidebar pane ([`panel`]): everything open in the
//!   workspace, grouped by file, and the answered history.
//! - [`NextQuestion`] / [`PreviousQuestion`] (`F8` / `Shift+F8`): the next or
//!   previous open question, across files; [`OpenQuestion`] opens one (from
//!   the pane and the chat's question cards). The workbench handles them
//!   ([`crate::shell::Shell`]).
//!
//! Answering calls `questions/answer`, "Dismiss" `questions/dismiss`; the
//! answer reaches the agent as its tool result.
//!
//! **The user asks too** (#229): the ask button in a file tab's gutter, or
//! [`AskAboutCode`] (`Ctrl/Cmd+Shift+/`), opens the [`ask`] popover on the
//! selected lines. Sending pins a thread to them (`questions/ask`): a
//! question with `author: user` whose first message prompts the chosen chat
//! session with the code attached. Its popover shows the agent's replies as
//! they stream ([`thread`]) and takes follow-ups (`questions/reply`). User
//! threads are blue, never block an agent, and F8 skips them.

pub mod ask;
pub mod navigate;
pub mod panel;
pub mod popover;
pub mod thread;

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use coqo_protocol::methods::{
    agents::{self as agm, SessionId},
    questions::{
        self as qm, AnchorStatus, AnswerParams, ChangesParams, ListParams, Question,
        QuestionAnchor, QuestionId, QuestionKind, QuestionList, QuestionParams, QuestionStatus,
        Severity,
    },
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    gpui::{
        Action, App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task,
    },
    slots::{SidebarPane, SidebarPanes},
};

use crate::{
    chat::list::ChatList,
    daemon::{Daemon, DaemonError},
    resume::{Resumable, Update},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derives spell their paths `gpui::…`.

coqo_ui::gpui::actions!(
    questions,
    [
        /// Show the next open question (F8), across files.
        NextQuestion,
        /// Show the previous open question (Shift+F8).
        PreviousQuestion,
        /// Ask an agent about the selected lines (Ctrl/Cmd+Shift+/).
        AskAboutCode,
    ]
);

#[cfg(target_os = "macos")]
const ASK_KEYS: [&str; 2] = ["cmd-shift-/", "cmd-?"];
#[cfg(not(target_os = "macos"))]
const ASK_KEYS: [&str; 2] = ["ctrl-shift-/", "ctrl-?"];
/// Where [`AskAboutCode`] is bound: a file tab.
pub const FILE_KEY_CONTEXT: &str = "FileEditor";

/// Show question `id` in its file, with its popover open and focused.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = questions, no_json)]
pub struct OpenQuestion {
    pub id: u64,
}

/// Bind F8 / Shift+F8 and register the Questions pane.
pub fn init(cx: &mut App) {
    use crate::commands::Command;
    crate::commands::register(
        cx,
        [
            Command::new("questions.next", "Next question", "Questions", NextQuestion).key("f8"),
            Command::new(
                "questions.previous",
                "Previous question",
                "Questions",
                PreviousQuestion,
            )
            .key("shift-f8"),
            Command::new(
                "questions.ask_about_code",
                "Ask an agent about the selection",
                "Questions",
                AskAboutCode,
            )
            .key(ASK_KEYS[0])
            .key(ASK_KEYS[1])
            .context(FILE_KEY_CONTEXT),
        ],
    );
    popover::init(cx);
    SidebarPanes::register(
        cx,
        SidebarPane::new("questions", "Questions", 5, |dock, window, cx| {
            cx.new(|cx| panel::QuestionsPane::new(dock, window, cx))
                .into()
        }),
    );
}

// ---- the store ------------------------------------------------------------

/// What changed in a [`WorkspaceQuestions`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuestionsEvent {
    /// Questions were added, changed or listed again.
    Changed,
    /// An agent started waiting for this open question (live, not from a
    /// snapshot): a visible file tab shows its popover.
    Waiting(QuestionId),
}

/// A workspace's questions and notes. See the module docs.
pub struct WorkspaceQuestions {
    workspace: WorkspaceId,
    questions: HashMap<QuestionId, Question>,
    loaded: bool,
    /// Provider names by id, for the popover's "Claude · …".
    providers: HashMap<String, String>,
    _feed: Entity<Resumable<QuestionList, Question>>,
    _providers: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for WorkspaceQuestions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceQuestions")
            .field("workspace", &self.workspace)
            .field("questions", &self.questions.len())
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<QuestionsEvent> for WorkspaceQuestions {}

#[derive(Default)]
struct QuestionHub(HashMap<WorkspaceId, Entity<WorkspaceQuestions>>);

impl Global for QuestionHub {}

/// The questions of `workspace`, followed from the first call on. `None`
/// without a daemon (some tests).
pub fn questions_for(workspace: &WorkspaceId, cx: &mut App) -> Option<Entity<WorkspaceQuestions>> {
    Daemon::global(cx)?;
    if let Some(store) = cx.default_global::<QuestionHub>().0.get(workspace) {
        return Some(store.clone());
    }
    let id = workspace.clone();
    let store = cx.new(|cx| WorkspaceQuestions::new(id, cx));
    cx.global_mut::<QuestionHub>()
        .0
        .insert(workspace.clone(), store.clone());
    Some(store)
}

impl WorkspaceQuestions {
    fn new(workspace: WorkspaceId, cx: &mut Context<Self>) -> Self {
        let list = ListParams {
            workspace: workspace.clone(),
            root: None,
            path: None,
            session: None,
            include_closed: true,
        };
        let topic = ChangesParams {
            workspace: Some(workspace.clone()),
        };
        let feed =
            cx.new(|cx| Resumable::follow::<qm::List, qm::Changes>(list, |l| l.seq, topic, cx));
        let subscriptions = vec![cx.subscribe(&feed, |this, _, update, cx| match update {
            Update::Snapshot(list) => this.replace(list.questions.clone(), cx),
            Update::Event(event) => this.received(event.event.clone(), cx),
        })];
        Self {
            workspace,
            questions: HashMap::new(),
            loaded: false,
            providers: HashMap::new(),
            _feed: feed,
            _providers: None,
            _subscriptions: subscriptions,
        }
    }

    /// The workspace.
    #[allow(dead_code)] // For badges and alerts (#76).
    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// Whether the first list arrived.
    #[allow(dead_code)]
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// One question or note.
    pub fn get(&self, id: QuestionId) -> Option<&Question> {
        self.questions.get(&id)
    }

    /// Open questions and notes, oldest first.
    pub fn open(&self) -> Vec<&Question> {
        let mut open: Vec<_> = self
            .questions
            .values()
            .filter(|q| q.status == QuestionStatus::Open)
            .collect();
        open.sort_by_key(|q| (q.created_at, q.id));
        open
    }

    /// Answered and dismissed ones, most recently closed first.
    pub fn closed(&self) -> Vec<&Question> {
        let mut closed: Vec<_> = self
            .questions
            .values()
            .filter(|q| q.status != QuestionStatus::Open)
            .collect();
        closed.sort_by_key(|q| std::cmp::Reverse((q.closed_at.unwrap_or(q.created_at), q.id)));
        closed
    }

    /// Open questions and notes with an anchor in `root`/`path`, oldest first.
    pub fn open_in(&self, root: RootId, path: &str) -> Vec<&Question> {
        self.open()
            .into_iter()
            .filter(|q| {
                q.anchors
                    .iter()
                    .any(|a| a.anchor.root == root && a.anchor.path == path)
            })
            .collect()
    }

    /// Open questions (not notes, not the user's threads): what needs the
    /// user. For badges and alerts (#76).
    #[allow(dead_code)]
    pub fn open_count(&self) -> usize {
        self.open()
            .iter()
            .filter(|q| q.kind.is_question() && q.author.is_agent())
            .count()
    }

    /// Open questions an agent is blocked on right now.
    #[allow(dead_code)]
    pub fn waiting_count(&self) -> usize {
        self.open().iter().filter(|q| q.waiting).count()
    }

    /// Open notes.
    #[allow(dead_code)]
    pub fn note_count(&self) -> usize {
        self.open().iter().filter(|q| !q.kind.is_question()).count()
    }

    /// The asking agent and session: "Claude · Volume discount".
    pub fn asker(&self, session: &SessionId, cx: &App) -> String {
        let info = ChatList::global(cx).and_then(|l| l.read(cx).info(session).cloned());
        let provider = info
            .as_ref()
            .map(|i| {
                self.providers
                    .get(&i.provider)
                    .cloned()
                    .unwrap_or_else(|| i.provider.clone())
            })
            .unwrap_or_else(|| "Agent".to_owned());
        match info.and_then(|i| i.title) {
            Some(title) => format!("{provider} · {title}"),
            None => provider,
        }
    }

    /// Take in questions listed or returned elsewhere (a file's list after
    /// `buffer/open`, an answer): each replaces what is known unless that is
    /// a newer state.
    pub fn merge(&mut self, questions: Vec<Question>, cx: &mut Context<Self>) {
        let mut changed = false;
        for q in questions {
            if q.workspace != self.workspace {
                continue;
            }
            let newer = self.questions.get(&q.id).is_none_or(|old| q.seq >= old.seq);
            if newer && self.questions.get(&q.id) != Some(&q) {
                self.questions.insert(q.id, q);
                changed = true;
            }
        }
        if changed {
            cx.emit(QuestionsEvent::Changed);
            cx.notify();
        }
    }

    fn replace(&mut self, questions: Vec<Question>, cx: &mut Context<Self>) {
        self.questions = questions.into_iter().map(|q| (q.id, q)).collect();
        self.loaded = true;
        if self.providers.is_empty() {
            self.load_providers(cx);
        }
        cx.emit(QuestionsEvent::Changed);
        cx.notify();
    }

    fn received(&mut self, q: Question, cx: &mut Context<Self>) {
        let was_waiting = self.questions.get(&q.id).is_some_and(|old| old.waiting);
        if self.questions.get(&q.id).is_some_and(|old| old.seq > q.seq) {
            return;
        }
        let waiting = q.waiting && q.status == QuestionStatus::Open && !was_waiting;
        let id = q.id;
        self.questions.insert(id, q);
        cx.emit(QuestionsEvent::Changed);
        if waiting {
            cx.emit(QuestionsEvent::Waiting(id));
        }
        cx.notify();
    }

    fn load_providers(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let providers = daemon.read(cx).request::<agm::Providers>(());
        self._providers = Some(cx.spawn(async move |this, cx| {
            if let Ok(list) = providers.await {
                this.update(cx, |this, cx| {
                    this.providers = list.into_iter().map(|p| (p.id, p.name)).collect();
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// List `root`/`path`'s questions again (after `buffer/open`, so their
    /// ranges are of the open buffer) and take them in.
    pub fn relist_file(&mut self, root: RootId, path: &str, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let list = daemon.read(cx).request::<qm::List>(ListParams {
            workspace: self.workspace.clone(),
            root: Some(root),
            path: Some(path.to_owned()),
            session: None,
            include_closed: false,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(list) = list.await {
                this.update(cx, |this, cx| this.merge(list.questions, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// The open questions and notes in the order F8 visits them: by file,
    /// then line.
    pub fn navigation(&self) -> Vec<NavItem> {
        let mut items: Vec<NavItem> = self
            .open()
            .into_iter()
            .filter(|q| q.author.is_agent())
            .map(|q| {
                let anchor = primary_anchor(q);
                NavItem {
                    id: q.id,
                    root: anchor.map_or(RootId(0), |a| a.anchor.root),
                    path: anchor.map(|a| a.anchor.path.clone()).unwrap_or_default(),
                    line: anchor.map_or(0, anchor_line),
                }
            })
            .collect();
        items.sort_by(|a, b| a.key().cmp(&b.key()).then(a.id.cmp(&b.id)));
        items
    }
}

/// One stop of F8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavItem {
    pub id: QuestionId,
    pub root: RootId,
    pub path: String,
    /// 0-based.
    pub line: u32,
}

impl NavItem {
    fn key(&self) -> (u32, &str, u32) {
        (self.root.0, self.path.as_str(), self.line)
    }
}

/// Where F8 is now: a question whose popover is open, else a file and line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavFrom {
    Question(QuestionId),
    Place(RootId, String, u32),
    Nowhere,
}

/// The question F8 (`forward`) or Shift+F8 goes to from `from`, wrapping
/// around.
pub fn step(items: &[NavItem], from: &NavFrom, forward: bool) -> Option<QuestionId> {
    if items.is_empty() {
        return None;
    }
    let n = items.len();
    let ix = match from {
        NavFrom::Question(id) => match items.iter().position(|i| i.id == *id) {
            Some(at) if forward => (at + 1) % n,
            Some(at) => (at + n - 1) % n,
            None if forward => 0,
            None => n - 1,
        },
        NavFrom::Place(root, path, line) => {
            let here = (root.0, path.as_str(), *line);
            if forward {
                items.iter().position(|i| i.key() > here).unwrap_or(0)
            } else {
                items.iter().rposition(|i| i.key() < here).unwrap_or(n - 1)
            }
        }
        NavFrom::Nowhere if forward => 0,
        NavFrom::Nowhere => n - 1,
    };
    Some(items[ix].id)
}

// ---- helpers ----------------------------------------------------------------

/// The anchor a question is shown at: the first whose code is still there,
/// else the first.
pub fn primary_anchor(q: &Question) -> Option<&QuestionAnchor> {
    q.anchors
        .iter()
        .find(|a| a.resolution.status != AnchorStatus::Orphaned)
        .or_else(|| q.anchors.first())
}

/// The 0-based line an anchor starts on now (as asked, if orphaned).
pub fn anchor_line(a: &QuestionAnchor) -> u32 {
    a.resolution
        .range
        .map_or(a.anchor.range.start.line, |r| r.start.line)
}

/// The lines (0-based, end exclusive) of a range of whole lines: its end is
/// the start of the line after.
pub fn span_lines(span: &qm::AnchorSpan) -> std::ops::Range<u32> {
    let start = span.start.line;
    let end = if span.end.col == 0 && span.end.line > start {
        span.end.line
    } else {
        span.end.line + 1
    };
    start..end.max(start + 1)
}

/// "src/cart.rs:42" (1-based), "…:42–48" for several lines.
pub fn location(a: &QuestionAnchor) -> String {
    let span = a.resolution.range.unwrap_or(a.anchor.range);
    let lines = span_lines(&span);
    if lines.len() > 1 {
        format!("{}:{}–{}", a.anchor.path, lines.start + 1, lines.end)
    } else {
        format!("{}:{}", a.anchor.path, lines.start + 1)
    }
}

/// The badge text of a question's kind.
pub fn kind_label(q: &Question) -> &'static str {
    if !q.author.is_agent() {
        return "YOU ASKED";
    }
    match q.kind {
        QuestionKind::Decision => "DECISION",
        QuestionKind::Clarification => "CLARIFICATION",
        QuestionKind::Confirmation => "CONFIRM",
        QuestionKind::Info => "INFO",
        QuestionKind::Annotation => match q.severity.unwrap_or_default() {
            Severity::Info => "NOTE",
            Severity::Warning => "WARNING",
        },
    }
}

/// The colour of a question's kind badge (board 2: decision sand,
/// clarification blue, confirm sage).
pub fn kind_color(q: &Question, cx: &App) -> Rgb {
    let p = &cx.tokens().palette;
    if !q.author.is_agent() {
        return p.blue;
    }
    match q.kind {
        QuestionKind::Decision => p.sand,
        QuestionKind::Clarification => p.blue,
        QuestionKind::Confirmation => p.sage,
        QuestionKind::Info => p.lavender,
        QuestionKind::Annotation => note_color(q, cx),
    }
}

/// The colour of a question's markers: sand for a question that needs the
/// user, blue for a note or a thread the user started, rose for a warning.
pub fn marker_color(q: &Question, cx: &App) -> Rgb {
    if !q.author.is_agent() {
        cx.tokens().palette.blue
    } else if q.kind.is_question() {
        cx.tokens().palette.sand
    } else {
        note_color(q, cx)
    }
}

fn note_color(q: &Question, cx: &App) -> Rgb {
    let p = &cx.tokens().palette;
    match q.severity.unwrap_or_default() {
        Severity::Info => p.blue,
        Severity::Warning => p.rose,
    }
}

/// "3 min ago" for Unix milliseconds.
pub fn ago(ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let secs = (now - ms).max(0) / 1000;
    match secs {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// "3m" for the pane's cards.
pub fn ago_short(ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let secs = (now - ms).max(0) / 1000;
    match secs {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The text of an answer: the chosen option's label and the reply.
pub fn answer_text(q: &Question) -> Option<String> {
    let answer = q.answer.as_ref()?;
    let option = answer
        .option
        .as_ref()
        .map(|id| {
            q.options
                .iter()
                .find(|o| &o.id == id)
                .map_or_else(|| id.clone(), |o| o.label.clone())
        })
        .filter(|_| q.kind != QuestionKind::Info);
    match (
        option,
        answer.text.as_ref().filter(|t| !t.trim().is_empty()),
    ) {
        (Some(o), Some(t)) => Some(format!("{o}: {t}")),
        (Some(o), None) => Some(o),
        (None, Some(t)) => Some(t.clone()),
        (None, None) => (q.kind == QuestionKind::Info).then(|| "OK".to_owned()),
    }
}

/// Send an answer; the store takes in the answered question.
pub fn answer(
    store: &Entity<WorkspaceQuestions>,
    params: AnswerParams,
    cx: &mut App,
) -> Task<Result<Question, DaemonError>> {
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let call = daemon.read(cx).request::<qm::Answer>(params);
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let q = call.await?;
        store
            .update(cx, |store, cx| store.merge(vec![q.clone()], cx))
            .ok();
        Ok(q)
    })
}

/// Ask `session` about code: pin a thread (`questions/ask`); the store
/// takes it in.
pub fn ask(
    store: &Entity<WorkspaceQuestions>,
    params: qm::AskParams,
    cx: &mut App,
) -> Task<Result<Question, DaemonError>> {
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let call = daemon.read(cx).request::<qm::Ask>(params);
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let q = call.await?;
        store
            .update(cx, |store, cx| store.merge(vec![q.clone()], cx))
            .ok();
        Ok(q)
    })
}

/// Follow up in a thread the user started (`questions/reply`).
pub fn reply(
    store: &Entity<WorkspaceQuestions>,
    params: qm::ReplyParams,
    cx: &mut App,
) -> Task<Result<Question, DaemonError>> {
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let call = daemon.read(cx).request::<qm::Reply>(params);
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let q = call.await?;
        store
            .update(cx, |store, cx| store.merge(vec![q.clone()], cx))
            .ok();
        Ok(q)
    })
}

/// Dismiss a question or note; the store takes in the dismissed one.
pub fn dismiss(
    store: &Entity<WorkspaceQuestions>,
    id: QuestionId,
    cx: &mut App,
) -> Task<Result<Question, DaemonError>> {
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let call = daemon
        .read(cx)
        .request::<qm::Dismiss>(QuestionParams { id });
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let q = call.await?;
        store
            .update(cx, |store, cx| store.merge(vec![q.clone()], cx))
            .ok();
        Ok(q)
    })
}

#[cfg(test)]
mod tests;
