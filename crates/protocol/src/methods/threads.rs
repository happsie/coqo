// SPDX-License-Identifier: Apache-2.0

//! `threads/*` and `review/*`: comment threads on diff lines and the user's
//! pending review (PLAN §7.7, issue #94, ADR 0022).
//!
//! A [`Thread`] is pinned to lines of one file **as one version of it has
//! them** ([`ThreadAnchor::version`]: the work tree, the index or a
//! revision, whichever a diff side shows), or to the whole file. The daemon
//! keeps a [`CodeAnchor`] of those lines (ADR 0013) and resolves it against
//! that version as it is now every time it serves the thread, so a thread
//! stays on its code when lines are added above it, and is
//! [`Orphaned`](AnchorStatus::Orphaned) when the code is gone.
//!
//! A thread holds [`Comment`]s by the user, an agent session, or (with the
//! GitHub backend, M8) a GitHub login. The user's comments start
//! **pending**: together they are the *pending review* ([`Pending`]), which
//! the user sends to the agent in one message (#95) or [`Discard`]s. An
//! agent's reply is just another comment, by [`Author::Agent`].
//!
//! # For the UI
//!
//! 1. [`List`] (`threads/list`) a workspace's threads (optionally one file
//!    or one diff) with their anchors resolved now, and `seq`.
//! 2. Follow [`Changes`] (`threads/changes`) with `since: seq`: each thread
//!    created, commented on, edited, resolved or reopened as its latest
//!    state, and each deleted one. Resuming replays each thread's newest
//!    state only, so seqs skip values. On `RESUME_GAP`, list again. Where
//!    an anchor is now comes with each list: list again when the files
//!    change (`git/changes`).
//! 3. [`Create`], [`Reply`], [`EditComment`], [`DeleteComment`],
//!    [`Resolve`], [`Reopen`]; [`Pending`] and [`Discard`] for the review.
//! 4. [`SendReview`] (`review/send`, #95) sends the pending review to an
//!    agent session in one prompt and clears `pending`; the agent answers
//!    each thread with the `coqo-editor` tool [`REPLY_TOOL`], which calls
//!    [`AgentReply`] (`review/reply`). The prompt carries a
//!    [`ReviewLink`], so a chat shows it as a "Review sent" card.
//!
//! **Access:** every route takes the daemon token, except [`AgentReply`]:
//! a session token may reply as its own session, to threads of its own
//! workspace.

use serde::{Deserialize, Serialize};

use super::agents::SessionId;
use super::git::DiffSide;
pub use super::questions::{AnchorResolution, AnchorStatus, CodeAnchor};
use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, Topic};

/// Longest comment, in bytes.
pub const MAX_COMMENT: usize = 64 * 1024;
/// Longest commented range, in lines.
pub const MAX_THREAD_LINES: u32 = 2000;
/// The `coqo-editor` tool an agent answers a review thread with (#95).
pub const REPLY_TOOL: &str = "reply_to_review";

/// Identifies a thread. Assigned by the daemon, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct ThreadId(pub u64);

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies a comment, across threads. Assigned by the daemon, never
/// reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct CommentId(pub u64);

impl std::fmt::Display for CommentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a thread lives besides Coqo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Only in Coqo: a review of an agent's change, sent to the agent.
    #[default]
    Local,
    /// A pull request review thread on GitHub (the PR inbox, M8).
    Github,
}

/// The side of a diff a thread was started on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ThreadSide {
    /// The base (removed lines, the old line numbers).
    Old,
    /// The head (added lines, the new line numbers).
    #[default]
    New,
}

/// Lines of a file, 1-based, `end` inclusive (as diffs number them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ThreadLines {
    /// The first line.
    pub start: u32,
    /// The last line.
    pub end: u32,
}

/// Who wrote a comment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    /// The person using Coqo.
    User,
    /// An agent session (its replies, #95).
    Agent {
        /// The session.
        session: SessionId,
    },
    /// A GitHub user (the GitHub backend, M8).
    Github {
        /// Their login.
        login: String,
    },
}

/// One comment of a thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    /// Id.
    pub id: CommentId,
    /// Who wrote it.
    pub author: Author,
    /// The comment, in markdown.
    pub body: String,
    /// Part of the user's pending review: not sent yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// Written, Unix milliseconds.
    pub created_at: i64,
    /// Last edited, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<i64>,
}

/// Whether a thread still needs attention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Under discussion.
    #[default]
    Open,
    /// Done with; shown collapsed.
    Resolved,
}

/// The code a thread is about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ThreadAnchor {
    /// The root `path` is relative to.
    pub root: RootId,
    /// `/`-separated path of the file, relative to the root.
    pub path: String,
    /// The diff side it was started on.
    pub side: ThreadSide,
    /// The version of the file the lines are in: `worktree` for the new
    /// side of unstaged changes, `index` for their old side, `{"rev":
    /// "HEAD"}`, … A diff shows the thread on whichever side shows this
    /// version.
    pub version: DiffSide,
    /// The lines as they were when the thread was started; absent for a
    /// thread on the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<CodeAnchor>,
    /// Where `code` is in `version` now (absent with `code`). Its range
    /// is never in an open buffer: it is in the version as git sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<AnchorResolution>,
    /// The lines now, 1-based; as they were when orphaned. Absent for a
    /// thread on the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<ThreadLines>,
}

impl ThreadAnchor {
    /// Whether the code is gone from the version (show it as outdated, on
    /// the file).
    pub fn is_orphaned(&self) -> bool {
        self.resolution
            .is_some_and(|r| r.status == AnchorStatus::Orphaned)
    }
}

/// A comment thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    /// Id.
    pub id: ThreadId,
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Where it lives besides Coqo.
    pub backend: Backend,
    /// The diff it was started in (a diff source's identity, such as
    /// `git:0:index..worktree`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The code it is about.
    pub anchor: ThreadAnchor,
    /// Open or resolved.
    pub status: ThreadStatus,
    /// The comments, oldest first; at least one.
    pub comments: Vec<Comment>,
    /// Started, Unix milliseconds.
    pub created_at: i64,
    /// Last changed, Unix milliseconds.
    pub updated_at: i64,
    /// Resolved, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    /// The `threads/changes` seq of this state.
    pub seq: u64,
}

impl Thread {
    /// Its comments in the pending review.
    pub fn pending(&self) -> impl Iterator<Item = &Comment> {
        self.comments.iter().filter(|c| c.pending)
    }
}

/// An event of [`Changes`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadEvent {
    /// A thread was created or changed: its state now.
    Changed {
        /// The thread.
        thread: Box<Thread>,
    },
    /// A thread was deleted (its last comment was).
    Deleted {
        /// Its id.
        id: ThreadId,
        /// Its workspace.
        workspace: WorkspaceId,
    },
}

impl ThreadEvent {
    /// The thread it is about.
    pub fn id(&self) -> ThreadId {
        match self {
            ThreadEvent::Changed { thread } => thread.id,
            ThreadEvent::Deleted { id, .. } => *id,
        }
    }

    /// Its workspace.
    pub fn workspace(&self) -> &WorkspaceId {
        match self {
            ThreadEvent::Changed { thread } => &thread.workspace,
            ThreadEvent::Deleted { workspace, .. } => workspace,
        }
    }
}

// ---- params and results ---------------------------------------------------------

/// Params of [`List`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Only threads in this root…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootId>,
    /// …and this file (`/`-separated, relative to `root`; in any root if
    /// `root` is omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Only threads started in this diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// Result of [`List`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ThreadList {
    /// The threads, oldest first, open and resolved.
    pub threads: Vec<Thread>,
    /// The `threads/changes` seq this list reflects; subscribe with
    /// `since: seq` to keep it current.
    pub seq: u64,
}

/// Params of [`Get`], [`Resolve`] and [`Reopen`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ThreadParams {
    /// The thread.
    pub id: ThreadId,
}

/// Params of [`Create`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CreateParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The root `path` is relative to.
    pub root: RootId,
    /// `/`-separated path of the file, relative to the root.
    pub path: String,
    /// The diff side the user commented on.
    #[serde(default)]
    pub side: ThreadSide,
    /// The version of the file that side shows (see
    /// [`ThreadAnchor::version`]).
    pub version: DiffSide,
    /// The lines (1-based, at most [`MAX_THREAD_LINES`]); omit to comment
    /// on the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<ThreadLines>,
    /// The diff source's identity, so the thread can be listed by diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The first comment, in markdown.
    pub body: String,
    /// Add it to the pending review (what the review UI does) rather than
    /// as a comment already made.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

/// Params of [`Reply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ReplyParams {
    /// The thread.
    pub thread: ThreadId,
    /// The comment, in markdown.
    pub body: String,
    /// Add it to the pending review.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

/// Params of [`EditComment`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct EditCommentParams {
    /// The comment: one of the user's.
    pub comment: CommentId,
    /// Its new text, in markdown.
    pub body: String,
}

/// Params of [`DeleteComment`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CommentParams {
    /// The comment: one of the user's.
    pub comment: CommentId,
}

/// Result of [`DeleteComment`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DeleteCommentResult {
    /// The thread without the comment; absent when it was the last one,
    /// and the thread went with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
}

/// Params of the [`Changes`] topic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// Only this workspace's threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// Params of [`Pending`] and [`Discard`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ReviewParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Only comments on threads started in this diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// A comment of the pending review, with what it is about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PendingComment {
    /// Its thread.
    pub thread: ThreadId,
    /// The diff the thread was started in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The code the thread is about, resolved now.
    pub anchor: ThreadAnchor,
    /// The comment.
    pub comment: Comment,
}

/// Result of [`Pending`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct PendingReview {
    /// How many comments are pending.
    pub count: u32,
    /// They, by thread (oldest first), then oldest first.
    pub comments: Vec<PendingComment>,
}

/// Result of [`Discard`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct DiscardResult {
    /// How many pending comments were deleted.
    pub discarded: u32,
}

/// Params of [`SendReview`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SendReviewParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Only comments on threads started in this diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The agent session to send them to: one of the workspace's, not in a
    /// turn.
    pub session: SessionId,
}

/// Result of [`SendReview`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SentReview {
    /// The session it went to.
    pub session: SessionId,
    /// The turn it started.
    pub turn: u64,
    /// How many comments were sent (no longer pending).
    pub count: u32,
    /// Their threads, oldest first.
    pub threads: Vec<ThreadId>,
}

/// Params of [`AgentReply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AgentReplyParams {
    /// The workspace: the thread's.
    pub workspace: WorkspaceId,
    /// The agent session replying.
    pub session: SessionId,
    /// The thread.
    pub thread: ThreadId,
    /// The reply, in markdown.
    pub body: String,
    /// Also resolve the thread (its request is done).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resolve: bool,
}

/// What a review prompt links to, so a chat can show it as a card and open
/// the diff: `coqo://review?workspace=…&comments=N&threads=1,2&diff=…`.
/// The prompt's [`ResourceLink`](super::agents::ContentBlock::ResourceLink)
/// has it as its `uri`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewLink {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The diff the comments were made in, if one.
    pub diff: Option<String>,
    /// How many comments were sent.
    pub comments: u32,
    /// Their threads.
    pub threads: Vec<ThreadId>,
}

impl ReviewLink {
    /// The scheme and path every review link starts with.
    pub const PREFIX: &'static str = "coqo://review?";
    /// The link's display name.
    pub const NAME: &'static str = "Coqo review";

    /// As a URI.
    pub fn to_uri(&self) -> String {
        let threads: Vec<String> = self.threads.iter().map(ToString::to_string).collect();
        let mut uri = format!(
            "{}workspace={}&comments={}&threads={}",
            Self::PREFIX,
            encode(&self.workspace.0),
            self.comments,
            threads.join(",")
        );
        if let Some(diff) = &self.diff {
            uri.push_str("&diff=");
            uri.push_str(&encode(diff));
        }
        uri
    }

    /// The link `uri` is, if it is one.
    pub fn parse(uri: &str) -> Option<Self> {
        let query = uri.strip_prefix(Self::PREFIX)?;
        let mut link = Self {
            workspace: WorkspaceId(String::new()),
            diff: None,
            comments: 0,
            threads: Vec::new(),
        };
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            let value = decode(value)?;
            match key {
                "workspace" => link.workspace = WorkspaceId(value),
                "diff" => link.diff = Some(value),
                "comments" => link.comments = value.parse().ok()?,
                "threads" => {
                    link.threads = value
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.parse().map(ThreadId))
                        .collect::<Result<_, _>>()
                        .ok()?;
                }
                _ => {}
            }
        }
        (!link.workspace.0.is_empty()).then_some(link)
    }
}

/// Percent-encode all but unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---- methods ---------------------------------------------------------------------

/// `threads/list`: a workspace's threads, anchors resolved now.
#[derive(Debug)]
pub enum List {}
impl Method for List {
    const NAME: &'static str = "threads/list";
    type Params = ListParams;
    type Result = ThreadList;
}

/// `threads/get`: one thread, anchors resolved now.
#[derive(Debug)]
pub enum Get {}
impl Method for Get {
    const NAME: &'static str = "threads/get";
    type Params = ThreadParams;
    type Result = Thread;
}

/// `threads/create`: start a thread with the user's first comment. Lines
/// past the end of the version, or a version without the file, are invalid
/// params.
#[derive(Debug)]
pub enum Create {}
impl Method for Create {
    const NAME: &'static str = "threads/create";
    type Params = CreateParams;
    type Result = Thread;
}

/// `threads/reply`: add the user's comment to a thread.
#[derive(Debug)]
pub enum Reply {}
impl Method for Reply {
    const NAME: &'static str = "threads/reply";
    type Params = ReplyParams;
    type Result = Thread;
}

/// `threads/editComment`: change one of the user's comments
/// ([`NOT_YOUR_COMMENT`](super::codes::NOT_YOUR_COMMENT) for anyone else's).
#[derive(Debug)]
pub enum EditComment {}
impl Method for EditComment {
    const NAME: &'static str = "threads/editComment";
    type Params = EditCommentParams;
    type Result = Thread;
}

/// `threads/deleteComment`: delete one of the user's comments; a thread
/// without comments is deleted.
#[derive(Debug)]
pub enum DeleteComment {}
impl Method for DeleteComment {
    const NAME: &'static str = "threads/deleteComment";
    type Params = CommentParams;
    type Result = DeleteCommentResult;
}

/// `threads/resolve`: mark a thread resolved.
#[derive(Debug)]
pub enum Resolve {}
impl Method for Resolve {
    const NAME: &'static str = "threads/resolve";
    type Params = ThreadParams;
    type Result = Thread;
}

/// `threads/reopen`: open a resolved thread again.
#[derive(Debug)]
pub enum Reopen {}
impl Method for Reopen {
    const NAME: &'static str = "threads/reopen";
    type Params = ThreadParams;
    type Result = Thread;
}

/// `review/pending`: the user's pending review, the comments not sent yet.
#[derive(Debug)]
pub enum Pending {}
impl Method for Pending {
    const NAME: &'static str = "review/pending";
    type Params = ReviewParams;
    type Result = PendingReview;
}

/// `review/discard`: delete the pending review's comments (and the threads
/// they leave empty).
#[derive(Debug)]
pub enum Discard {}
impl Method for Discard {
    const NAME: &'static str = "review/discard";
    type Params = ReviewParams;
    type Result = DiscardResult;
}

/// `review/send`: send the pending review (of one diff) to an agent session
/// in one prompt, then clear `pending`. Refused with
/// [`SESSION_BUSY`](super::codes::SESSION_BUSY) while the session is in a
/// turn, and as invalid params with nothing pending; either way nothing
/// changes.
#[derive(Debug)]
pub enum SendReview {}
impl Method for SendReview {
    const NAME: &'static str = "review/send";
    type Params = SendReviewParams;
    type Result = SentReview;
}

/// `review/reply`: an agent session's reply in a thread of its workspace,
/// optionally resolving it (the `reply_to_review` tool). A session token
/// may call it as its own session.
#[derive(Debug)]
pub enum AgentReply {}
impl Method for AgentReply {
    const NAME: &'static str = "review/reply";
    type Params = AgentReplyParams;
    type Result = Thread;
}

/// `threads/changes`: threads whose state changed, as their latest state,
/// and deleted ones. Resumable from [`ThreadList::seq`]; resuming replays
/// each thread's newest state only.
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "threads/changes";
    type Params = ChangesParams;
    type Event = ThreadEvent;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::methods::questions::{AnchorPoint, AnchorSpan};

    #[test]
    fn thread_wire_shape() {
        let span = AnchorSpan {
            start: AnchorPoint {
                line: 18,
                col: 0,
                offset: 400,
            },
            end: AnchorPoint {
                line: 19,
                col: 0,
                offset: 450,
            },
        };
        let t = Thread {
            id: ThreadId(3),
            workspace: WorkspaceId("ws-1".into()),
            backend: Backend::Local,
            diff: Some("git:0:index..worktree".into()),
            anchor: ThreadAnchor {
                root: RootId(0),
                path: "src/pricing.rs".into(),
                side: ThreadSide::New,
                version: DiffSide::Worktree,
                code: Some(CodeAnchor {
                    root: RootId(0),
                    path: "src/pricing.rs".into(),
                    rev: None,
                    range: span,
                    text_hash: "00ff".into(),
                    text: "    discount.min(cap)\n".into(),
                    prefix: String::new(),
                    suffix: String::new(),
                    context_before: vec![],
                    context_after: vec![],
                    symbol: None,
                }),
                resolution: Some(AnchorResolution {
                    status: AnchorStatus::Moved,
                    range: Some(span),
                    buffer: None,
                    rev: None,
                }),
                lines: Some(ThreadLines { start: 19, end: 19 }),
            },
            status: ThreadStatus::Open,
            comments: vec![
                Comment {
                    id: CommentId(5),
                    author: Author::User,
                    body: "Round **first**.".into(),
                    pending: true,
                    created_at: 1,
                    edited_at: None,
                },
                Comment {
                    id: CommentId(6),
                    author: Author::Agent {
                        session: SessionId("s-1".into()),
                    },
                    body: "Done.".into(),
                    pending: false,
                    created_at: 2,
                    edited_at: Some(3),
                },
            ],
            created_at: 1,
            updated_at: 3,
            resolved_at: None,
            seq: 9,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["anchor"]["version"], json!("worktree"));
        assert_eq!(v["anchor"]["side"], json!("new"));
        assert_eq!(v["anchor"]["lines"], json!({"start": 19, "end": 19}));
        assert_eq!(
            v["anchor"]["resolution"]["status"],
            json!({"kind": "moved"})
        );
        assert_eq!(
            v["comments"],
            json!([
                {"id": 5, "author": {"kind": "user"}, "body": "Round **first**.", "pending": true, "createdAt": 1},
                {"id": 6, "author": {"kind": "agent", "session": "s-1"}, "body": "Done.", "createdAt": 2, "editedAt": 3},
            ])
        );
        assert_eq!(v["backend"], json!("local"));
        assert_eq!(v["status"], json!("open"));
        assert!(v.get("resolvedAt").is_none());
        let back: Thread = serde_json::from_value(v).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.pending().count(), 1);

        let gone = ThreadEvent::Deleted {
            id: ThreadId(3),
            workspace: WorkspaceId("ws-1".into()),
        };
        assert_eq!(
            serde_json::to_value(&gone).unwrap(),
            json!({"type": "deleted", "id": 3, "workspace": "ws-1"})
        );
        let changed = serde_json::to_value(ThreadEvent::Changed {
            thread: Box::new(t),
        })
        .unwrap();
        assert_eq!(changed["type"], json!("changed"));
        assert_eq!(changed["thread"]["id"], json!(3));
    }

    #[test]
    fn review_links_round_trip() {
        let link = ReviewLink {
            workspace: WorkspaceId("ws 1".into()),
            diff: Some("git:0:index..worktree".into()),
            comments: 3,
            threads: vec![ThreadId(4), ThreadId(9)],
        };
        let uri = link.to_uri();
        assert_eq!(
            uri,
            "coqo://review?workspace=ws%201&comments=3&threads=4,9&diff=git%3A0%3Aindex..worktree"
        );
        assert_eq!(ReviewLink::parse(&uri), Some(link));
        let bare = ReviewLink {
            workspace: WorkspaceId("w".into()),
            diff: None,
            comments: 1,
            threads: vec![ThreadId(1)],
        };
        assert_eq!(ReviewLink::parse(&bare.to_uri()), Some(bare));
        assert_eq!(ReviewLink::parse("file:///a.rs"), None);
        assert_eq!(ReviewLink::parse("coqo://review?comments=x"), None);
    }

    #[test]
    fn create_params_wire_shape() {
        let p: CreateParams = serde_json::from_value(json!({
            "workspace": "ws-1",
            "root": 0,
            "path": "src/a.rs",
            "side": "old",
            "version": "index",
            "lines": {"start": 4, "end": 6},
            "body": "why?",
            "pending": true,
        }))
        .unwrap();
        assert_eq!(p.side, ThreadSide::Old);
        assert_eq!(p.version, DiffSide::Index);
        assert_eq!(p.lines, Some(ThreadLines { start: 4, end: 6 }));
        assert!(p.pending && p.diff.is_none());
        // A file-level comment on the new side, not pending.
        let p: CreateParams = serde_json::from_value(json!({
            "workspace": "ws-1", "root": 0, "path": "a", "version": {"rev": "HEAD"}, "body": "b"
        }))
        .unwrap();
        assert_eq!((p.side, p.lines, p.pending), (ThreadSide::New, None, false));
    }
}
