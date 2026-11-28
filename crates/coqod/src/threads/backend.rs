// SPDX-License-Identifier: Apache-2.0

//! Where a thread lives besides Coqo's store: the [`ThreadBackend`] seam.
//!
//! Every thread is kept in `coqo.db` whatever its backend, so the diff view,
//! the pending review and restarts work the same for all. A backend mirrors
//! what the user does to a thread somewhere else, after the store has it:
//!
//! - [`LocalBackend`] (`local`): nowhere else. A local thread is a review
//!   of an agent's change; `review/send` delivers the pending review to the
//!   agent and the agent replies with `reply_to_review` (#95).
//! - The GitHub backend (`github`, the PR inbox, M8) will mirror threads to
//!   a pull request's review. Mapping, for that issue:
//!   - A thread is a review thread (`pullRequestReviewThread`): `anchor.path`
//!     (plus the root's prefix in the repository), `side` `old` → `LEFT`,
//!     `new` → `RIGHT`, `lines` → `startLine`/`line`; a file-level thread is
//!     `subjectType: FILE`. `version` is the PR's base or head commit.
//!   - The pending review is GitHub's pending review: pending comments are
//!     added to it (`addPullRequestReviewThread` / `…Comment` with the
//!     review id) and submitted together with a verdict (`APPROVE`,
//!     `REQUEST_CHANGES`, `COMMENT`). Until then GitHub shows them to the
//!     author only, as Coqo does.
//!   - Comments by others arrive as [`Author::Github`](coqo_protocol::methods::threads::Author)
//!     with their login, from the daemon's poller; they are never pending.
//!   - Resolve and reopen map to `resolveReviewThread` /
//!     `unresolveReviewThread`; an edit or delete of the user's own comment
//!     to `updatePullRequestReviewComment` / `deletePullRequestReviewComment`.
//!   - The remote ids (thread node id, comment database ids, review id) need
//!     a column of their own (a later, append-only migration).
//!   - An outdated GitHub thread (`isOutdated`) is an orphaned anchor here.

use coqo_protocol::ErrorObject;
use coqo_protocol::methods::threads::{Backend, Comment, CommentId, Thread};
use futures_util::future::BoxFuture;

/// A change to mirror, after the store has it.
#[derive(Clone, Copy, Debug)]
pub enum Mirror<'a> {
    /// A thread was started (with its first comment).
    Created(&'a Thread),
    /// A comment was added to a thread.
    Commented(&'a Thread, &'a Comment),
    /// One of the user's comments was edited.
    Edited(&'a Thread, &'a Comment),
    /// One of the user's comments was deleted; `None` when the thread went
    /// with it.
    Deleted(Option<&'a Thread>, CommentId),
    /// A thread was resolved or reopened.
    Status(&'a Thread),
    /// These pending comments of the user's were sent to an agent session
    /// (`review/send`, #95): they are not pending any more.
    Sent(&'a Thread, &'a [CommentId]),
}

/// Where threads of one [`Backend`] are mirrored. See the module docs.
pub trait ThreadBackend: Send + Sync + std::fmt::Debug {
    /// Which backend it is.
    fn kind(&self) -> Backend;

    /// Mirror `change`. An error is reported to the caller; the store keeps
    /// the change (a remote failure must not lose the user's comment).
    fn mirror(&self, change: Mirror<'_>) -> BoxFuture<'static, Result<(), ErrorObject>>;
}

/// Threads kept in Coqo only.
#[derive(Debug, Default)]
pub struct LocalBackend;

impl ThreadBackend for LocalBackend {
    fn kind(&self) -> Backend {
        Backend::Local
    }

    fn mirror(&self, _: Mirror<'_>) -> BoxFuture<'static, Result<(), ErrorObject>> {
        Box::pin(std::future::ready(Ok(())))
    }
}
