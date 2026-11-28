// SPDX-License-Identifier: Apache-2.0

//! Sending the pending review to an agent (`review/send`) and the agent's
//! replies (`review/reply`, the `reply_to_review` tool), issue #95.
//!
//! - **One prompt.** Every pending comment of the diff, by thread: the
//!   thread's id, the file (absolute) and lines where they are now, the
//!   quoted code, the thread's earlier comments, then the user's pending
//!   ones. It says to fix each and answer it with `reply_to_review`. A
//!   [`ReviewLink`] goes along as a resource link, so the chat shows the
//!   message as a "Review sent" card that opens the diff.
//! - **Nothing lost.** The session must be one of the workspace's and not
//!   in a turn (`SESSION_BUSY`, with what to do). Comments stop being
//!   pending only once the turn started.
//! - **Replies.** An agent's reply is a comment by [`Author::Agent`], in a
//!   thread of its own workspace (another workspace's thread is
//!   `UNKNOWN_THREAD`), optionally resolving it, as one change.

use coqo_agents::AgentError;
use coqo_protocol::ErrorObject;
use coqo_protocol::methods::agents::{ContentBlock, SessionId};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::git::DiffSide;
use coqo_protocol::methods::threads::{
    AgentReplyParams, Author, Comment, CommentId, REPLY_TOOL, ReviewLink, ReviewParams,
    SendReviewParams, SentReview, Thread, ThreadStatus,
};
use coqo_storage::Timestamp;

use super::store::NewComment;
use super::{Mirror, ThreadService, check_body, invalid, storage_error, unknown};

/// Code lines quoted per thread at most.
const MAX_QUOTED_LINES: usize = 40;

/// One thread of the prompt.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    /// The thread, as served now.
    pub thread: Thread,
    /// Its file, absolute (or as stored, if it cannot be found).
    pub file: String,
}

/// "the unstaged changes", from a diff source's identity.
fn diff_name(diff: Option<&str>) -> &'static str {
    match diff {
        Some(d) if d.ends_with(":index..worktree") => "the unstaged changes",
        Some(d) if d.ends_with(":HEAD..index") => "the staged changes",
        _ => "the changes",
    }
}

/// Who wrote `c`, as the user tells the agent (`session`).
fn who(c: &Comment, session: &SessionId) -> String {
    match &c.author {
        Author::User => "Me".into(),
        Author::Agent { session: s } if s == session => "You".into(),
        Author::Agent { .. } => "Another agent".into(),
        Author::Github { login } => format!("@{login}"),
    }
}

/// `text` as a markdown quote.
fn quote(text: &str) -> String {
    text.trim()
        .lines()
        .map(|l| {
            if l.is_empty() {
                ">".into()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `code` in a fence longer than any backtick run in it.
fn fenced(code: &str) -> String {
    let mut lines: Vec<&str> = code.lines().collect();
    let cut = lines.len() > MAX_QUOTED_LINES;
    lines.truncate(MAX_QUOTED_LINES);
    let mut longest = 0;
    for l in &lines {
        let mut run = 0;
        for ch in l.chars() {
            run = if ch == '`' { run + 1 } else { 0 };
            longest = longest.max(run);
        }
    }
    let fence = "`".repeat(longest.max(2) + 1);
    let mut out = format!("{fence}\n{}\n", lines.join("\n"));
    if cut {
        out.push_str("…\n");
    }
    out.push_str(&fence);
    out
}

/// The review prompt to `session` for `items` (in `diff`), and the pending
/// comments it sends, by thread.
pub(crate) fn prompt(
    items: &[Item],
    link: &ReviewLink,
    session: &SessionId,
) -> (Vec<ContentBlock>, Vec<(Thread, Vec<CommentId>)>) {
    let count: usize = items.iter().map(|i| i.thread.pending().count()).sum();
    let threads = items.len();
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut text = format!(
        "Please address my code review: {} in {} on {}. For each thread, make the change it \
         asks for (or decide it should not be made), then call the coqo-editor tool \
         `{REPLY_TOOL}` (mcp__coqo-editor__{REPLY_TOOL}) with its `thread_id` and a short reply \
         saying what you did, with `resolve: true` once it is done. Reply in every thread, \
         also when you change nothing; I read the replies next to the code. Lines are 1-based.",
        plural(count, "comment", "comments"),
        plural(threads, "thread", "threads"),
        diff_name(link.diff.as_deref()),
    );
    let mut sent = Vec::new();
    for item in items {
        let t = &item.thread;
        let a = &t.anchor;
        let place = match a.lines {
            Some(l) if l.start == l.end => format!("{}:{}", item.file, l.start),
            Some(l) => format!("{}:{}-{}", item.file, l.start, l.end),
            None => format!("{} (the whole file)", item.file),
        };
        let version = match &a.version {
            DiffSide::Worktree => String::new(),
            DiffSide::Index => " (as staged in the index)".into(),
            DiffSide::Rev(rev) => format!(" (at `{rev}`)"),
        };
        text.push_str(&format!("\n\n## Thread {}: {place}{version}", t.id));
        if let Some(code) = &a.code {
            if a.is_orphaned() {
                text.push_str("\n\nThe code it was on has changed since. It was:");
            }
            text.push_str("\n\n");
            text.push_str(&fenced(&code.text));
        }
        let first_pending = t
            .comments
            .iter()
            .position(|c| c.pending)
            .unwrap_or(t.comments.len());
        let earlier: Vec<&Comment> = t.comments[..first_pending].iter().collect();
        if !earlier.is_empty() {
            text.push_str("\n\nEarlier in this thread:");
            for c in earlier {
                text.push_str(&format!("\n\n**{}:**\n{}", who(c, session), quote(&c.body)));
            }
        }
        let pending: Vec<&Comment> = t.pending().collect();
        text.push_str(if pending.len() == 1 {
            "\n\nMy comment:"
        } else {
            "\n\nMy comments:"
        });
        for c in &pending {
            text.push_str("\n\n");
            text.push_str(&quote(&c.body));
        }
        sent.push((t.clone(), pending.iter().map(|c| c.id).collect()));
    }
    let content = vec![
        ContentBlock::text(text),
        ContentBlock::ResourceLink {
            uri: link.to_uri(),
            name: ReviewLink::NAME.into(),
            mime_type: None,
            size: None,
        },
    ];
    (content, sent)
}

/// The refusal while `title`'s session is in a turn.
fn busy(title: Option<&str>, count: usize) -> ErrorObject {
    let chat = title
        .filter(|t| !t.is_empty())
        .map_or_else(|| "That chat".to_owned(), |t| format!("\"{t}\""));
    let comments = if count == 1 {
        "comment stays"
    } else {
        "comments stay"
    };
    ErrorObject::new(
        codes::SESSION_BUSY,
        format!(
            "{chat} is in the middle of a turn. Wait for it to finish or pick another chat; \
             your {count} {comments} pending."
        ),
    )
}

impl ThreadService {
    /// `review/send`.
    pub async fn send(&self, p: SendReviewParams) -> Result<SentReview, ErrorObject> {
        let sessions = self
            .sessions
            .as_ref()
            .ok_or_else(|| ErrorObject::internal("this daemon runs no agent sessions"))?;
        let snapshot = sessions.snapshot(&p.session).map_err(ErrorObject::from)?;
        let info = &snapshot.info;
        if info.workspace.as_deref() != Some(p.workspace.0.as_str()) {
            return Err(invalid(format!(
                "session {} is not in workspace {}",
                p.session, p.workspace
            )));
        }
        let review = ReviewParams {
            workspace: p.workspace.clone(),
            diff: p.diff.clone(),
        };
        let threads = self.threads(self.pending_records(&review)?).await;
        let count: usize = threads.iter().map(|t| t.pending().count()).sum();
        if count == 0 {
            return Err(invalid("there are no pending comments to send"));
        }
        if snapshot.running {
            return Err(busy(info.title.as_deref(), count));
        }
        let mut items = Vec::with_capacity(threads.len());
        for thread in threads {
            let a = &thread.anchor;
            let file = match self
                .workspaces
                .resolve_file(p.workspace.clone(), a.root, a.path.clone())
                .await
            {
                Ok(file) => file.real.to_string_lossy().into_owned(),
                Err(_) => a.path.clone(),
            };
            items.push(Item { thread, file });
        }
        let link = ReviewLink {
            workspace: p.workspace.clone(),
            diff: p.diff.clone(),
            comments: u32::try_from(count).unwrap_or(u32::MAX),
            threads: items.iter().map(|i| i.thread.id).collect(),
        };
        let (content, sent) = prompt(&items, &link, &p.session);
        let started = match sessions.prompt(&p.session, content).await {
            Ok(started) => started,
            Err(AgentError::Busy(_)) => return Err(busy(info.title.as_deref(), count)),
            Err(e) => return Err(e.into()),
        };
        // The turn started: these are not pending any more.
        for (thread, ids) in &sent {
            let cleared = self
                .change(thread.id, |store, _| {
                    store.send_comments(ids).map_err(|e| storage_error(&e))
                })
                .await;
            match cleared {
                Ok((Some(now), ())) => self.mirror(now.backend, Mirror::Sent(&now, ids)).await?,
                Ok((None, ())) => {}
                // Deleted meanwhile.
                Err(e) if e.code == codes::UNKNOWN_THREAD => {}
                Err(e) => return Err(e),
            }
        }
        tracing::debug!(session = %p.session, comments = count, "review sent");
        Ok(SentReview {
            session: p.session,
            turn: started.turn,
            count: link.comments,
            threads: link.threads,
        })
    }

    /// `review/reply`: an agent session's reply, optionally resolving.
    pub async fn agent_reply(&self, p: AgentReplyParams) -> Result<Thread, ErrorObject> {
        let body = check_body(&p.body)?;
        // Only threads of the session's own workspace.
        if self.record(p.thread)?.workspace != p.workspace {
            return Err(unknown(p.thread));
        }
        let author = Author::Agent {
            session: p.session.clone(),
        };
        let resolve = p.resolve;
        let (thread, (comment, resolved)) = self
            .change(p.thread, |store, record| {
                let comment = store
                    .add_comment(
                        p.thread,
                        &NewComment {
                            author,
                            body,
                            pending: false,
                            created_at: Timestamp::now().0,
                        },
                    )
                    .map_err(|e| storage_error(&e))?;
                let resolved = resolve && record.status != ThreadStatus::Resolved;
                if resolved {
                    record.status = ThreadStatus::Resolved;
                    record.resolved_at = Some(Timestamp::now().0);
                }
                Ok((comment, resolved))
            })
            .await?;
        let thread = thread.ok_or_else(|| unknown(p.thread))?;
        self.mirror(thread.backend, Mirror::Commented(&thread, &comment))
            .await?;
        if resolved {
            self.mirror(thread.backend, Mirror::Status(&thread)).await?;
        }
        tracing::debug!(thread = %thread.id, session = %p.session, resolved, "agent replied");
        Ok(thread)
    }
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::threads::{
        Backend, CommentId, ThreadAnchor, ThreadId, ThreadLines, ThreadSide,
    };
    use coqo_protocol::methods::workspace::{RootId, WorkspaceId};

    use super::*;

    fn comment(id: u64, author: Author, body: &str, pending: bool) -> Comment {
        Comment {
            id: CommentId(id),
            author,
            body: body.into(),
            pending,
            created_at: 0,
            edited_at: None,
        }
    }

    fn thread(id: u64, lines: Option<ThreadLines>, comments: Vec<Comment>) -> Thread {
        Thread {
            id: ThreadId(id),
            workspace: WorkspaceId("w".into()),
            backend: Backend::Local,
            diff: Some("git:0:index..worktree".into()),
            anchor: ThreadAnchor {
                root: RootId(0),
                path: "src/a.rs".into(),
                side: ThreadSide::New,
                version: DiffSide::Worktree,
                code: None,
                resolution: None,
                lines,
            },
            status: ThreadStatus::Open,
            comments,
            created_at: 0,
            updated_at: 0,
            resolved_at: None,
            seq: 1,
        }
    }

    #[test]
    fn the_prompt_has_every_comment_its_thread_and_the_link() {
        let me = SessionId("s1".into());
        let items = vec![
            Item {
                thread: thread(
                    4,
                    Some(ThreadLines { start: 6, end: 7 }),
                    vec![
                        comment(1, Author::User, "Why a loop?", false),
                        comment(
                            2,
                            Author::Agent {
                                session: me.clone(),
                            },
                            "It is short.",
                            false,
                        ),
                        comment(3, Author::User, "Use a fold.\n\nAnd test it.", true),
                    ],
                ),
                file: "/p/src/a.rs".into(),
            },
            Item {
                thread: thread(
                    9,
                    None,
                    vec![
                        comment(5, Author::User, "Rename the file.", true),
                        comment(6, Author::User, "Or split it.", true),
                    ],
                ),
                file: "/p/src/b.rs".into(),
            },
        ];
        let link = ReviewLink {
            workspace: WorkspaceId("w".into()),
            diff: Some("git:0:index..worktree".into()),
            comments: 3,
            threads: vec![ThreadId(4), ThreadId(9)],
        };
        let (content, sent) = prompt(&items, &link, &me);
        let ContentBlock::Text { text } = &content[0] else {
            panic!("{content:?}")
        };
        assert!(
            text.starts_with(
                "Please address my code review: 3 comments in 2 threads on the unstaged changes."
            ),
            "{text}"
        );
        assert!(text.contains("`reply_to_review`"), "{text}");
        assert!(text.contains("## Thread 4: /p/src/a.rs:6-7"), "{text}");
        assert!(
            text.contains("## Thread 9: /p/src/b.rs (the whole file)"),
            "{text}"
        );
        assert!(
            text.contains(
                "Earlier in this thread:\n\n**Me:**\n> Why a loop?\n\n**You:**\n> It is short."
            ),
            "{text}"
        );
        assert!(
            text.contains("My comment:\n\n> Use a fold.\n>\n> And test it."),
            "{text}"
        );
        assert!(
            text.contains("My comments:\n\n> Rename the file.\n\n> Or split it."),
            "{text}"
        );
        assert_eq!(
            content[1],
            ContentBlock::ResourceLink {
                uri: link.to_uri(),
                name: "Coqo review".into(),
                mime_type: None,
                size: None,
            }
        );
        let ids: Vec<(u64, Vec<CommentId>)> =
            sent.iter().map(|(t, c)| (t.id.0, c.clone())).collect();
        assert_eq!(
            ids,
            [
                (4, vec![CommentId(3)]),
                (9, vec![CommentId(5), CommentId(6)])
            ]
        );
    }

    #[test]
    fn code_is_fenced_past_its_backticks_and_cut() {
        assert_eq!(fenced("let a = 1;"), "```\nlet a = 1;\n```");
        assert_eq!(fenced("a ```` b"), "`````\na ```` b\n`````");
        let long: String = (0..50).map(|i| format!("{i}\n")).collect();
        let f = fenced(&long);
        assert!(f.ends_with("39\n…\n```"), "{f}");
    }

    #[test]
    fn busy_says_what_to_do_and_that_nothing_is_lost() {
        let e = busy(Some("Volume discount"), 2);
        assert_eq!(e.code, codes::SESSION_BUSY);
        assert_eq!(
            e.message,
            "\"Volume discount\" is in the middle of a turn. Wait for it to finish or pick \
             another chat; your 2 comments stay pending."
        );
        assert!(busy(None, 1).message.starts_with("That chat is"));
    }
}
