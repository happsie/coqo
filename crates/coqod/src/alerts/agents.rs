// SPDX-License-Identifier: Apache-2.0

//! The core alert sources: agent sessions (ADR 0015, 0016).
//!
//! [`AgentSource`] follows `sessions/changes` and keeps one alert per
//! session in step with its state ([`session_alert`]):
//!
//! | State | Source | Kind | Opens |
//! |---|---|---|---|
//! | `needs_permission` | `agents.permission` | needs you | the chat |
//! | `needs_input` | `agents.question` | needs you | the open question, else the chat |
//! | `blocked` | `agents.blocked` | needs you | the chat |
//! | `error` | `agents.error` | error | the chat |
//! | `done` | `agents.done` | done | the chat |
//! | `working`, `idle`, closed | — | resolves the session's alert | |
//!
//! A session has at most one standing alert: moving to another state
//! resolves the alert of the old one. Every source keys its alert by the
//! session id. The last change handled is kept, so after a restart the
//! source picks up the sessions that changed meanwhile (a turn the restart
//! interrupted is an error), and on the very first start nothing old is
//! raised.

use std::sync::Arc;
use std::time::Duration;

use coqo_agents::SessionManager;
use coqo_protocol::methods::agents::{AgentState, SessionId, SessionInfo};
use coqo_protocol::methods::alerts::{AlertKind, AlertTarget, FileLocation};
use coqo_protocol::methods::workspace::WorkspaceId;
use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use super::{AlertService, NewAlert};
use crate::questions::{OpenQuestion, QuestionService};

/// A permission request waits.
pub const PERMISSION: &str = "agents.permission";
/// A question waits for its answer.
pub const QUESTION: &str = "agents.question";
/// The agent says it is blocked.
pub const BLOCKED: &str = "agents.blocked";
/// A turn or the provider failed.
pub const ERROR: &str = "agents.error";
/// A turn finished.
pub const DONE: &str = "agents.done";

/// Every agent source.
pub const SOURCES: [&str; 5] = [PERMISSION, QUESTION, BLOCKED, ERROR, DONE];

/// Where the last handled `sessions/changes` seq is kept.
const CURSOR: &str = "agents.cursor";
/// How often the cursor is saved while changes come in.
const SAVE_EVERY: Duration = Duration::from_secs(5);
/// How long to wait before following again after the feed failed.
const RETRY: Duration = Duration::from_secs(1);

/// A session's open questions, for the alert of a session that waits on
/// one.
pub trait OpenQuestions: Send + Sync {
    /// The session's oldest open question, if any.
    fn first_open(&self, session: &SessionId) -> Option<OpenQuestion>;
}

impl OpenQuestions for QuestionService {
    fn first_open(&self, session: &SessionId) -> Option<OpenQuestion> {
        self.open_questions_of(session).into_iter().next()
    }
}

/// The first line of `text`, markdown emphasis and code marks dropped.
fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    line.unwrap_or_default()
        .trim_start_matches('#')
        .replace(['*', '`'], "")
        .trim()
        .to_owned()
}

/// What `info`'s alert should be now: `None` if it needs none. `agent` is
/// the provider's name, `permission` what the first waiting permission
/// request asks, `question` the session's first open question.
pub fn session_alert(
    info: &SessionInfo,
    agent: &str,
    permission: Option<&str>,
    question: Option<&OpenQuestion>,
) -> Option<NewAlert> {
    if info.closed {
        return None;
    }
    let title = info
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "New chat".to_owned());
    let reason = info
        .state_reason
        .as_deref()
        .map(first_line)
        .filter(|r| !r.is_empty());
    let chat = AlertTarget::Session {
        session: info.id.clone(),
    };
    let (source, kind, headline, body, target) = match info.state {
        AgentState::Working | AgentState::Idle => return None,
        AgentState::NeedsPermission => (
            PERMISSION,
            AlertKind::NeedsYou,
            permission.map_or_else(
                || format!("{agent} asks for permission"),
                |p| format!("Permission requested: {}", first_line(p)),
            ),
            Some(title),
            chat,
        ),
        AgentState::NeedsInput => (
            QUESTION,
            AlertKind::NeedsYou,
            format!("{agent} asked a question"),
            question.map(|q| first_line(&q.body)).or(reason),
            question.map_or(chat, |q| AlertTarget::Question {
                session: info.id.clone(),
                question: q.id,
                location: q.location.clone().map(|(root, path, line)| FileLocation {
                    root,
                    path,
                    line: Some(line),
                }),
            }),
        ),
        AgentState::Blocked => (
            BLOCKED,
            AlertKind::NeedsYou,
            format!("{agent} is blocked"),
            reason.or(Some(title)),
            chat,
        ),
        AgentState::Error => (
            ERROR,
            AlertKind::Error,
            format!("{agent} failed"),
            reason.or(Some(title)),
            chat,
        ),
        AgentState::Done => (
            DONE,
            AlertKind::Done,
            format!("{agent} finished"),
            info.status
                .as_ref()
                .map(|s| first_line(&s.text))
                .filter(|s| !s.is_empty())
                .or(Some(title)),
            chat,
        ),
    };
    Some(NewAlert {
        source: source.to_owned(),
        key: info.id.0.clone(),
        workspace: info.workspace.clone().map(WorkspaceId),
        session: Some(info.id.clone()),
        kind,
        title: headline,
        body,
        target: Some(target),
    })
}

/// Keeps agent sessions' alerts in step with their states. See the module
/// docs.
#[derive(Clone)]
pub struct AgentSource {
    alerts: Arc<AlertService>,
    sessions: SessionManager,
    questions: Option<Arc<dyn OpenQuestions>>,
}

impl std::fmt::Debug for AgentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSource").finish_non_exhaustive()
    }
}

impl AgentSource {
    /// The source raising into `alerts` for `sessions`, with the open
    /// questions from `questions`.
    pub fn new(
        alerts: Arc<AlertService>,
        sessions: SessionManager,
        questions: Option<Arc<dyn OpenQuestions>>,
    ) -> Self {
        Self {
            alerts,
            sessions,
            questions,
        }
    }

    fn agent_name(&self, provider: &str) -> String {
        self.sessions
            .providers()
            .into_iter()
            .find(|p| p.id == provider)
            .map_or_else(|| "The agent".to_owned(), |p| p.name)
    }

    /// Bring `info`'s alert in step with its state. Returns whether an
    /// alert changed.
    pub fn apply(&self, info: &SessionInfo) -> bool {
        let permission = (info.state == AgentState::NeedsPermission)
            .then(|| self.sessions.snapshot(&info.id).ok())
            .flatten()
            .and_then(|s| s.pending_permissions.first().map(|p| p.title.clone()));
        let question = (info.state == AgentState::NeedsInput)
            .then(|| self.questions.as_ref()?.first_open(&info.id))
            .flatten();
        let agent = self.agent_name(&info.provider);
        let want = session_alert(info, &agent, permission.as_deref(), question.as_ref());
        let mut changed = false;
        for source in SOURCES {
            let result = match &want {
                Some(new) if new.source == source => self.alerts.raise(new.clone()),
                _ => self.alerts.resolve(source, &info.id.0),
            };
            match result {
                Ok(c) => changed |= c.is_some(),
                Err(e) => {
                    tracing::warn!(session = %info.id, source, error = %e, "cannot update an alert");
                }
            }
        }
        changed
    }

    /// Follow `sessions/changes` until `shutdown`, from the last change
    /// handled.
    pub fn spawn(self, shutdown: CancellationToken) {
        tokio::spawn(async move {
            loop {
                if !self.follow(&shutdown).await {
                    return;
                }
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(RETRY) => {}
                }
            }
        });
    }

    /// Follow once. Returns whether to follow again (the feed failed).
    async fn follow(&self, shutdown: &CancellationToken) -> bool {
        let since = self.alerts.cursor(CURSOR);
        let feed = match self.sessions.changes(None, since) {
            Ok(feed) => feed,
            // Ahead of the sessions (a reset database): from now on.
            Err(_) if since.is_some() => match self.sessions.changes(None, None) {
                Ok(feed) => feed,
                Err(e) => {
                    tracing::warn!(error = %e.message, "cannot follow sessions for alerts");
                    return true;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e.message, "cannot follow sessions for alerts");
                return true;
            }
        };
        let mut saved = since.unwrap_or(0);
        let mut last = since.unwrap_or(feed.head);
        if since.is_none() {
            // First start: nothing old is raised.
            self.alerts.set_cursor(CURSOR, last);
            saved = last;
        }
        let mut events = feed.events;
        let mut tick = tokio::time::interval(SAVE_EVERY);
        let again = loop {
            tokio::select! {
                () = shutdown.cancelled() => break false,
                _ = tick.tick() => {
                    if last != saved {
                        self.alerts.set_cursor(CURSOR, last);
                        saved = last;
                    }
                }
                event = events.next() => match event {
                    Some(Ok(change)) => {
                        let changed = self.apply(&change.event);
                        last = change.seq;
                        if changed {
                            self.alerts.set_cursor(CURSOR, last);
                            saved = last;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e.message, "the sessions feed for alerts failed");
                        break true;
                    }
                    None => break false,
                },
            }
        };
        if last != saved {
            self.alerts.set_cursor(CURSOR, last);
        }
        again
    }
}
