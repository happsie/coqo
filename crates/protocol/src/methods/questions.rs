// SPDX-License-Identifier: Apache-2.0

//! `questions/*`: questions and notes agents pin to code (PLAN §7.5, issue
//! #73).
//!
//! An agent asks through the `coqo-editor` MCP tools (`ask_question`,
//! `await_answer`, `annotate`); the user answers in the editor. The daemon
//! stores every question (in `coqo.db`, so they survive restarts and queue
//! while no UI is open) with its [`CodeAnchor`]s, and keeps each anchor's
//! [`AnchorResolution`] current: through the buffer's changes while the
//! file is open, by its fingerprint after a gap (a restart, a change on
//! disk). A question whose code is gone keeps its original snippet and
//! resolves as [`AnchorStatus::Orphaned`]; it can still be answered.
//!
//! # For the UI
//!
//! 1. [`List`] (`questions/list`) for a workspace, optionally one file: the
//!    questions with their anchors resolved now, and `seq`. List after
//!    `buffer/open`, so the ranges are of the open buffer (`buffer` and
//!    `rev` in each resolution say which text a range is in).
//! 2. Follow [`Changes`] (`questions/changes`) with `since: seq`: each
//!    question whose state changed (asked, answered, dismissed, an anchor
//!    moved, an agent started or stopped waiting), as its latest state.
//!    Resuming replays only each question's newest state, so seqs skip
//!    values. On `RESUME_GAP`, list again.
//! 3. [`Answer`] (`questions/answer`) or [`Dismiss`] (`questions/dismiss`).
//!
//! # For the bridge
//!
//! # Threads the user starts
//!
//! The other way round, the user asks an agent about code (#229):
//! [`Ask`] (`questions/ask`) pins a thread to the lines like an agent's
//! question ([`QuestionAuthor::User`]) and prompts the chosen session with
//! the question and the code; [`Reply`] (`questions/reply`) adds a
//! follow-up. Each [`ThreadMessage`] carries the session turn that answers
//! it, so the agent's reply is that turn of `sessions/updates`. Nobody
//! waits on a thread: it stays open until the user dismisses it.
//!
//! [`Create`] (`questions/create`) asks a question or adds a note, and
//! [`Watch`] (`questions/watch`) follows one question: its first event is
//! the question as it is now, then one per change, so a waiting agent never
//! misses its answer. Dropping the subscription stops the wait.
//!
//! **Access:** listing, answering and following changes take the daemon
//! token (the UI). A session token (an agent's `coqod mcp`) may create and
//! watch questions only for its own workspace and agent session.

use serde::{Deserialize, Serialize};

use super::agents::SessionId;
use super::buffer::BufferId;
use super::editor::LineRange;
use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, Topic};

/// Longest question or note body, in bytes.
pub const MAX_BODY: usize = 16 * 1024;
/// Most options a decision can offer.
pub const MAX_OPTIONS: usize = 16;
/// Most code locations one question can be pinned to.
pub const MAX_ANCHORS: usize = 8;
/// Longest anchored range, in lines.
pub const MAX_ANCHOR_LINES: u32 = 2000;
/// Longest free-text answer, in bytes.
pub const MAX_ANSWER: usize = 16 * 1024;

/// Who started a question.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum QuestionAuthor {
    /// An agent asked the user (`ask_question`, `annotate`).
    #[default]
    Agent,
    /// The user asked an agent ([`Ask`]): a thread the agent answers in its
    /// session.
    User,
}

impl QuestionAuthor {
    /// Whether an agent asked (the default, left out on the wire).
    pub fn is_agent(&self) -> bool {
        *self == Self::Agent
    }
}

/// One message the user sent in a thread ([`Ask`], [`Reply`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ThreadMessage {
    /// The message, markdown.
    pub text: String,
    /// The session turn it started: the agent's reply is that turn.
    pub turn: u64,
    /// Sent, Unix milliseconds.
    pub at: i64,
}

/// Identifies a question or note. Assigned by the daemon, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct QuestionId(pub u64);

impl std::fmt::Display for QuestionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of answer a question wants (PLAN §7.5), or a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Pick one of the options (or reply in free text).
    Decision,
    /// A free-text reply.
    Clarification,
    /// Yes or no (options `yes` and `no`), with an optional comment.
    Confirmation,
    /// Something the agent wants the user to see; acknowledge it (option
    /// `ok`).
    Info,
    /// A non-blocking note pinned to code (`annotate`). Nobody waits for
    /// it; dismiss it when done.
    Annotation,
}

impl QuestionKind {
    /// Whether the agent wants an answer (everything but an annotation).
    pub fn is_question(self) -> bool {
        self != Self::Annotation
    }
}

/// How important a note is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing.
    #[default]
    Info,
    /// Probably a problem.
    Warning,
}

/// How strongly a finding written as text is worded (ADR 0030): the
/// severity words of its line or heading. It sets the note's colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FindingLevel {
    /// critical, bug, error: shown in rose.
    Critical,
    /// warning, should: shown in sand.
    Warning,
    /// nit, suggestion, or no severity word: shown in the subtle colour.
    #[default]
    Note,
}

/// The chat message a note was made from (#345, ADR 0030): findings an
/// agent wrote as text (`file:line - issue`), pinned by the UI as notes
/// that link back to the message. Only the UI sets it; the message is of
/// the note's `session`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NoteOrigin {
    /// The message: the seq of its first `sessions/updates` chunk.
    pub message: u64,
    /// How its text words the finding.
    #[serde(default)]
    pub level: FindingLevel,
}

/// Where a question is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    /// Waiting for the user (a note: shown until dismissed).
    #[default]
    Open,
    /// The user answered.
    Answered,
    /// The user dismissed it without an answer.
    Dismissed,
}

/// One answer a question offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct QuestionOption {
    /// What [`AnswerParams::option`] names: `1`, `2`, … for a decision,
    /// `yes` / `no` for a confirmation, `ok` for info.
    pub id: String,
    /// The button text.
    pub label: String,
    /// More detail, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An option as the agent offers it; the daemon numbers them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NewOption {
    /// The button text.
    pub label: String,
    /// More detail, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ---- anchors (mirrors `coqo-anchors`) -------------------------------------------

/// A spot in a text: 0-based line, byte column within the line and byte
/// offset from the start (UTF-8, as in `buffer/*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AnchorPoint {
    /// 0-based line.
    pub line: u32,
    /// Byte column within the line.
    pub col: u32,
    /// Byte offset from the start of the text.
    pub offset: u64,
}

/// A range of a text, `end` exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AnchorSpan {
    /// Where it starts.
    pub start: AnchorPoint,
    /// Where it ends (exclusive).
    pub end: AnchorPoint,
}

/// The innermost symbol an anchor was in (tree-sitter, `coqo-syntax`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AnchorSymbol {
    /// Its containers and name, outermost first: `["shapes", "Circle",
    /// "area"]`.
    pub path: Vec<String>,
    /// Its kind, as `syntax/*` names kinds (`method`, `function`, …).
    pub kind: String,
    /// How many lines below the symbol's first line the anchor started.
    #[serde(default)]
    pub line_offset: u32,
}

/// Code a question is pinned to, as it was when the agent asked: where,
/// and a fingerprint to find it again (ADR 0013).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CodeAnchor {
    /// The root `path` is relative to.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// The buffer revision `range` was taken at; absent for a file read
    /// from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
    /// Where the anchored text was.
    pub range: AnchorSpan,
    /// FNV-1a 64 hash of `text`, in hex.
    pub text_hash: String,
    /// The anchored text as it was: what to show when it is orphaned.
    pub text: String,
    /// The start of the first line, up to the range.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// The rest of the last line after the range.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub suffix: String,
    /// Up to 3 lines before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<String>,
    /// Up to 3 lines after.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_after: Vec<String>,
    /// The enclosing symbol, if the file's language is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<AnchorSymbol>,
}

/// How well an anchor was found in the current text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnchorStatus {
    /// The anchored text, unchanged, in the same place.
    #[default]
    Exact,
    /// The anchored text, unchanged, somewhere else (lines added above, or
    /// the code moved).
    Moved,
    /// Probably the anchored code after an edit; `confidence` in
    /// `0.6..1`.
    Fuzzy {
        /// How sure the match is.
        confidence: f32,
    },
    /// The code is gone, or no match was good enough. Show the anchor's
    /// own `text`.
    Orphaned,
}

/// Where an anchor is now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AnchorResolution {
    /// How well it was found.
    pub status: AnchorStatus,
    /// Its range in the current text; absent when orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<AnchorSpan>,
    /// The open buffer `range` is in, if the file is open; else `range` is
    /// in the file on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer: Option<BufferId>,
    /// That buffer's revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
}

/// One code location of a question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct QuestionAnchor {
    /// The code as it was asked about.
    pub anchor: CodeAnchor,
    /// Where it is now.
    pub resolution: AnchorResolution,
}

/// The user's answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct QuestionAnswer {
    /// The chosen option's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option: Option<String>,
    /// A free-text reply or comment (markdown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A question or note, as the daemon keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Question {
    /// Id.
    pub id: QuestionId,
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The agent session that asked (or, for a thread the user started,
    /// the session asked).
    pub session: SessionId,
    /// Who asked: an agent, or the user ([`Ask`]).
    #[serde(default, skip_serializing_if = "QuestionAuthor::is_agent")]
    pub author: QuestionAuthor,
    /// What kind of answer it wants (a thread the user started is a
    /// `clarification`).
    pub kind: QuestionKind,
    /// How important a note is (annotations only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// The question or note, in markdown.
    pub body: String,
    /// The answers it offers, in order (empty for a clarification or a
    /// note).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
    /// The code it is about, at least one.
    pub anchors: Vec<QuestionAnchor>,
    /// Open, answered or dismissed.
    pub status: QuestionStatus,
    /// The answer, once answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<QuestionAnswer>,
    /// Whether the agent is blocked on it right now (a waiting
    /// `ask_question` or `await_answer`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waiting: bool,
    /// A thread the user started: their messages, oldest first, each with
    /// the turn that answers it. Empty for an agent's question.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread: Vec<ThreadMessage>,
    /// Asked, Unix milliseconds.
    pub created_at: i64,
    /// Answered or dismissed, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<i64>,
    /// The chat message the note was made from, for a finding written as
    /// text; absent for what an agent pinned itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<NoteOrigin>,
    /// The `questions/changes` seq of this state.
    pub seq: u64,
}

// ---- params and results ---------------------------------------------------------

/// Params of [`List`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Only questions with an anchor in this root…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootId>,
    /// …and this file (`/`-separated, relative to `root`; in any root if
    /// `root` is omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Only this session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// Answered and dismissed ones too (most recent first, up to 200).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_closed: bool,
}

/// Result of [`List`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct QuestionList {
    /// Open questions oldest first, then (with `includeClosed`) closed
    /// ones, most recently closed first.
    pub questions: Vec<Question>,
    /// The `questions/changes` seq this list reflects; subscribe with
    /// `since: seq` to keep it current.
    pub seq: u64,
}

/// Params of [`Get`] and [`Dismiss`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct QuestionParams {
    /// The question.
    pub id: QuestionId,
}

/// Params of [`Answer`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AnswerParams {
    /// The question.
    pub id: QuestionId,
    /// The chosen option's id. Required for a confirmation and info; a
    /// decision takes an option, `text`, or both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option: Option<String>,
    /// A free-text reply (markdown). Required for a clarification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Params of the [`Changes`] topic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// Only this workspace's questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// A code location to pin a question to: whole lines of a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct NewAnchor {
    /// The root `path` is relative to.
    pub root: RootId,
    /// `/`-separated path of a file relative to the root.
    pub path: String,
    /// The lines, 0-based, `end` inclusive.
    pub lines: LineRange,
}

/// Params of [`Create`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CreateParams {
    /// The asking session's workspace.
    pub workspace: WorkspaceId,
    /// The asking agent session.
    pub session: String,
    /// What kind of answer it wants; `annotation` for a note.
    pub kind: QuestionKind,
    /// The question or note, in markdown.
    pub body: String,
    /// A decision's options (at least 2). Confirmation and info get theirs
    /// from the daemon; the other kinds take none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<NewOption>,
    /// A note's severity (default `info`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// The code it is about: 1 to [`MAX_ANCHORS`] locations, the files
    /// as they are now (open buffers with their unsaved edits).
    pub anchors: Vec<NewAnchor>,
    /// The chat message a finding was made from (the UI's findings as
    /// notes, #345); only for an annotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<NoteOrigin>,
}

/// Params of [`Ask`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AskParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The agent session to ask; it must not be running a turn.
    pub session: SessionId,
    /// The question, markdown.
    pub body: String,
    /// The code it is about: 1 to [`MAX_ANCHORS`] locations, as for
    /// [`Create`].
    pub anchors: Vec<NewAnchor>,
}

/// Params of [`Reply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ReplyParams {
    /// The thread.
    pub id: QuestionId,
    /// The follow-up, markdown.
    pub body: String,
}

/// The text a message of a user's thread prompts its chat with ([`Ask`],
/// [`Reply`]): a line naming the thread and the code, then the message.
/// The daemon writes it; a chat parses it back to show the message in short
/// (#248). The code itself follows a first message as embedded resources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadPrompt {
    /// The thread.
    pub thread: QuestionId,
    /// Where its code is, e.g. `src/cart.rs:16-17` (1-based lines).
    pub places: Vec<String>,
    /// The thread's first message ([`Ask`]), else a follow-up.
    pub first: bool,
    /// The user's message, markdown.
    pub body: String,
}

impl ThreadPrompt {
    const ASK: &'static str = "I'm asking about ";
    const PINNED: &'static str = " (attached; pinned in my editor as thread #";
    const ASK_END: &'static str = "). Please answer here. If a note on the code itself would \
        help, you can also `annotate` the same lines.\n\n";
    const FOLLOW_UP: &'static str = "Follow-up on thread #";
    const ABOUT: &'static str = " about ";
    const FOLLOW_UP_END: &'static str = ":\n\n";

    /// The prompt's text.
    pub fn to_text(&self) -> String {
        let (places, id) = (self.places.join(", "), self.thread);
        if self.first {
            format!(
                "{}{places}{}{id}{}{}",
                Self::ASK,
                Self::PINNED,
                Self::ASK_END,
                self.body
            )
        } else {
            format!(
                "{}{id}{}{places}{}{}",
                Self::FOLLOW_UP,
                Self::ABOUT,
                Self::FOLLOW_UP_END,
                self.body
            )
        }
    }

    /// The thread prompt `text` is, if it is one.
    pub fn parse(text: &str) -> Option<Self> {
        let places = |s: &str| s.split(", ").map(str::to_owned).collect();
        if let Some(rest) = text.strip_prefix(Self::ASK) {
            let (at, rest) = rest.split_once(Self::PINNED)?;
            let (id, body) = rest.split_once(Self::ASK_END)?;
            return Some(Self {
                thread: QuestionId(id.parse().ok()?),
                places: places(at),
                first: true,
                body: body.to_owned(),
            });
        }
        let rest = text.strip_prefix(Self::FOLLOW_UP)?;
        let (id, rest) = rest.split_once(Self::ABOUT)?;
        let (at, body) = rest.split_once(Self::FOLLOW_UP_END)?;
        Some(Self {
            thread: QuestionId(id.parse().ok()?),
            places: places(at),
            first: false,
            body: body.to_owned(),
        })
    }
}

/// Params of the [`Watch`] topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WatchParams {
    /// The watching session's workspace.
    pub workspace: WorkspaceId,
    /// The watching agent session: the one that asked.
    pub session: String,
    /// The question.
    pub id: QuestionId,
}

// ---- methods ---------------------------------------------------------------------

/// `questions/list`: a workspace's questions, anchors resolved now.
#[derive(Debug)]
pub enum List {}
impl Method for List {
    const NAME: &'static str = "questions/list";
    type Params = ListParams;
    type Result = QuestionList;
}

/// `questions/get`: one question, anchors resolved now.
#[derive(Debug)]
pub enum Get {}
impl Method for Get {
    const NAME: &'static str = "questions/get";
    type Params = QuestionParams;
    type Result = Question;
}

/// `questions/answer`: the user's answer. An answered or dismissed question
/// is [`QUESTION_CLOSED`](super::codes::QUESTION_CLOSED); an answer that
/// does not fit the kind (no option for a confirmation, an unknown option)
/// is invalid params.
#[derive(Debug)]
pub enum Answer {}
impl Method for Answer {
    const NAME: &'static str = "questions/answer";
    type Params = AnswerParams;
    type Result = Question;
}

/// `questions/dismiss`: close a question without an answer, or a note. A
/// waiting agent is told it was dismissed.
#[derive(Debug)]
pub enum Dismiss {}
impl Method for Dismiss {
    const NAME: &'static str = "questions/dismiss";
    type Params = QuestionParams;
    type Result = Question;
}

/// `questions/create`: an agent asks a question or adds a note (the
/// bridge's `ask_question` and `annotate`).
#[derive(Debug)]
pub enum Create {}
impl Method for Create {
    const NAME: &'static str = "questions/create";
    type Params = CreateParams;
    type Result = Question;
}

/// `questions/ask`: the user asks an agent session about code. The daemon
/// pins a thread to the code (like [`Create`], authored by the user), then
/// prompts the session with the question and the anchored code as embedded
/// resources. Fails like `sessions/prompt` (`SESSION_BUSY` while a turn
/// runs), and then no thread is left behind.
#[derive(Debug)]
pub enum Ask {}
impl Method for Ask {
    const NAME: &'static str = "questions/ask";
    type Params = AskParams;
    type Result = Question;
}

/// `questions/reply`: a follow-up in a thread the user started, prompted to
/// the same session with the code again. An agent's question is invalid
/// params (answer it instead); a dismissed thread is
/// [`QUESTION_CLOSED`](super::codes::QUESTION_CLOSED).
#[derive(Debug)]
pub enum Reply {}
impl Method for Reply {
    const NAME: &'static str = "questions/reply";
    type Params = ReplyParams;
    type Result = Question;
}

/// `questions/changes`: questions whose state changed, as their latest
/// state. Resumable from [`QuestionList::seq`]; resuming replays each
/// question's newest state only.
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "questions/changes";
    type Params = ChangesParams;
    type Event = Question;
}

/// `questions/watch`: one question, for the agent that asked it: its state
/// now, then every change. Live only (`since` is ignored).
#[derive(Debug)]
pub enum Watch {}
impl Topic for Watch {
    const NAME: &'static str = "questions/watch";
    type Params = WatchParams;
    type Event = Question;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn question_wire_shape() {
        let q = Question {
            id: QuestionId(7),
            workspace: WorkspaceId("ws-1".into()),
            session: SessionId("s-1".into()),
            author: Default::default(),
            kind: QuestionKind::Decision,
            severity: None,
            body: "Which **map**?".into(),
            options: vec![QuestionOption {
                id: "1".into(),
                label: "HashMap".into(),
                description: None,
            }],
            anchors: vec![QuestionAnchor {
                anchor: CodeAnchor {
                    root: RootId(0),
                    path: "src/main.rs".into(),
                    rev: Some(3),
                    range: AnchorSpan {
                        start: AnchorPoint {
                            line: 1,
                            col: 0,
                            offset: 12,
                        },
                        end: AnchorPoint {
                            line: 2,
                            col: 0,
                            offset: 27,
                        },
                    },
                    text_hash: "00ff".into(),
                    text: "    let x = 1;\n".into(),
                    prefix: String::new(),
                    suffix: String::new(),
                    context_before: vec!["fn main() {".into()],
                    context_after: vec![],
                    symbol: Some(AnchorSymbol {
                        path: vec!["main".into()],
                        kind: "function".into(),
                        line_offset: 1,
                    }),
                },
                resolution: AnchorResolution {
                    status: AnchorStatus::Fuzzy { confidence: 0.75 },
                    range: None,
                    buffer: Some(BufferId(4)),
                    rev: Some(5),
                },
            }],
            status: QuestionStatus::Answered,
            answer: Some(QuestionAnswer {
                option: Some("1".into()),
                text: None,
            }),
            waiting: true,
            thread: vec![],
            created_at: 1,
            closed_at: Some(2),
            origin: None,
            seq: 9,
        };
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(
            v,
            json!({
                "id": 7,
                "workspace": "ws-1",
                "session": "s-1",
                "kind": "decision",
                "body": "Which **map**?",
                "options": [{"id": "1", "label": "HashMap"}],
                "anchors": [{
                    "anchor": {
                        "root": 0,
                        "path": "src/main.rs",
                        "rev": 3,
                        "range": {
                            "start": {"line": 1, "col": 0, "offset": 12},
                            "end": {"line": 2, "col": 0, "offset": 27},
                        },
                        "textHash": "00ff",
                        "text": "    let x = 1;\n",
                        "contextBefore": ["fn main() {"],
                        "symbol": {"path": ["main"], "kind": "function", "lineOffset": 1},
                    },
                    "resolution": {
                        "status": {"kind": "fuzzy", "confidence": 0.75},
                        "buffer": 4,
                        "rev": 5,
                    },
                }],
                "status": "answered",
                "answer": {"option": "1"},
                "waiting": true,
                "createdAt": 1,
                "closedAt": 2,
                "seq": 9,
            })
        );
        let back: Question = serde_json::from_value(v).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn thread_prompts_parse_back() {
        for first in [true, false] {
            let prompt = ThreadPrompt {
                thread: QuestionId(12),
                places: vec!["src/cart.rs:16-17".into(), "src/a, b.rs:3".into()],
                first,
                body: "Why cap it?\n\nAnd where: (here).".into(),
            };
            let text = prompt.to_text();
            assert_eq!(
                ThreadPrompt::parse(&text).map(|p| p.body),
                Some(prompt.body)
            );
            let back = ThreadPrompt::parse(&text).unwrap();
            assert_eq!((back.thread, back.first), (QuestionId(12), first));
            assert_eq!(back.places[0], "src/cart.rs:16-17");
        }
        let text = ThreadPrompt {
            thread: QuestionId(3),
            places: vec!["src/cart.rs:6-7".into()],
            first: true,
            body: "Why?".into(),
        }
        .to_text();
        assert_eq!(
            text,
            "I'm asking about src/cart.rs:6-7 (attached; pinned in my editor as thread #3). \
             Please answer here. If a note on the code itself would help, you can also \
             `annotate` the same lines.\n\nWhy?"
        );
        assert_eq!(ThreadPrompt::parse("I'm asking about the weather"), None);
        assert_eq!(
            ThreadPrompt::parse("Follow-up on thread #x about a:\n\nb"),
            None
        );
    }

    #[test]
    fn create_params_wire_shape() {
        let p: CreateParams = serde_json::from_value(json!({
            "workspace": "ws-1",
            "session": "s-1",
            "kind": "annotation",
            "body": "careful",
            "severity": "warning",
            "anchors": [{"root": 0, "path": "a.rs", "lines": {"start": 2, "end": 4}}],
        }))
        .unwrap();
        assert_eq!(p.kind, QuestionKind::Annotation);
        assert_eq!(p.severity, Some(Severity::Warning));
        assert!(p.options.is_empty());
        assert_eq!(p.anchors[0].lines, LineRange { start: 2, end: 4 });
    }
}
