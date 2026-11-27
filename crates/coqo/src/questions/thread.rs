// SPDX-License-Identifier: Apache-2.0

//! The agent's replies in a thread the user started (#229): each message of
//! the thread starts a turn of the asked session, and the reply is that
//! turn's agent message, streamed from `sessions/updates`.

use std::collections::BTreeMap;

use coqo_protocol::methods::agents::{
    ContentBlock, SessionId, SessionParams, SessionUpdate, Updates,
};
use coqo_ui::gpui::{Context, Task};

use crate::daemon::Daemon;

/// One turn's reply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnReply {
    /// The agent's message so far, markdown.
    pub text: String,
    /// Whether the turn ended.
    pub done: bool,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

/// The replies of one session's turns, as they stream.
pub struct ThreadReplies {
    session: SessionId,
    replies: BTreeMap<u64, TurnReply>,
    /// The turn running now.
    current: Option<u64>,
    error: Option<String>,
    _follow: Option<Task<()>>,
}

impl std::fmt::Debug for ThreadReplies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadReplies")
            .field("session", &self.session)
            .field("replies", &self.replies)
            .finish_non_exhaustive()
    }
}

impl ThreadReplies {
    /// Follow `session`'s updates from its first.
    pub fn follow(session: SessionId, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            session,
            replies: BTreeMap::new(),
            current: None,
            error: None,
            _follow: None,
        };
        this.attach(cx);
        this
    }

    fn attach(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx) else {
            self.error = Some("coqod isn't connected".into());
            return;
        };
        let updates = daemon.read(cx).subscribe_since::<Updates>(
            SessionParams {
                session: self.session.clone(),
            },
            0,
        );
        self._follow = Some(cx.spawn(async move |this, cx| {
            let mut stream = match updates.await {
                Ok(stream) => stream,
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("Couldn't follow the chat: {e}"));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            while let Some(update) = stream.next().await {
                let alive = this
                    .update(cx, |this, cx| match update {
                        Ok(update) => {
                            if this.apply(update.event) {
                                cx.notify();
                            }
                        }
                        Err(e) => {
                            this.error = Some(format!("Couldn't follow the chat: {e}"));
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Apply one update; whether a reply changed.
    fn apply(&mut self, update: SessionUpdate) -> bool {
        match update {
            SessionUpdate::TurnStarted { turn } => {
                self.current = Some(turn);
                self.replies.entry(turn).or_default();
                true
            }
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
            } => match self.current.and_then(|t| self.replies.get_mut(&t)) {
                Some(reply) => {
                    reply.text.push_str(&text);
                    true
                }
                None => false,
            },
            SessionUpdate::TurnEnded { turn, error, .. } => {
                let reply = self.replies.entry(turn).or_default();
                reply.done = true;
                reply.error = error;
                if self.current == Some(turn) {
                    self.current = None;
                }
                true
            }
            _ => false,
        }
    }

    /// The reply of `turn`, once it started.
    pub fn reply(&self, turn: u64) -> Option<&TurnReply> {
        self.replies.get(&turn)
    }

    /// Why the replies can't be shown, if they can't.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::agents::StopReason;

    use super::*;

    #[test]
    fn replies_are_the_agent_messages_of_their_turn() {
        let mut t = ThreadReplies {
            session: SessionId("s".into()),
            replies: BTreeMap::new(),
            current: None,
            error: None,
            _follow: None,
        };
        let chunk = |text: &str| SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text(text),
        };
        t.apply(chunk("before any turn"));
        t.apply(SessionUpdate::TurnStarted { turn: 2 });
        t.apply(chunk("Because "));
        t.apply(SessionUpdate::AgentThoughtChunk {
            content: ContentBlock::text("hmm"),
        });
        t.apply(chunk("π."));
        assert_eq!(
            t.reply(2),
            Some(&TurnReply {
                text: "Because π.".into(),
                done: false,
                error: None,
            })
        );
        t.apply(SessionUpdate::TurnEnded {
            turn: 2,
            stop_reason: StopReason::EndTurn,
            error: None,
        });
        assert!(t.reply(2).unwrap().done);
        assert_eq!(t.reply(1), None);
    }
}
