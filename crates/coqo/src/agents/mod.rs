// SPDX-License-Identifier: Apache-2.0

//! The agents overview and the "welcome back" summary (design board 6,
//! PLAN §7.9, issue #79). See `docs/design/agents.md`.
//!
//! - The **agents overview** ([`view::AgentsTab`], tab kind [`KIND`]) lists
//!   every open agent session of every workspace, live from
//!   [`AgentActivity`] (`sessions/list` + `sessions/changes`): what needs
//!   you first (a question, a permission request, blocked), then errors,
//!   then working, then done and idle ([`ordered`]). Each row says what the
//!   agent is doing and how long it has been so, and offers the next step:
//!   **Answer** (the question's popover), **Allow once** / **Deny**, **Open
//!   chat**, stop and close. The rail's agents button and `Ctrl Shift A`
//!   (`Cmd Shift A`) show it.
//! - **Welcome back** ([`away`]): when the UI attaches after being away for
//!   more than [`away::AWAY_AFTER`], a banner over the tab area sums up what
//!   the agents did meanwhile, with a link to the first one that needs you.

pub mod away;
pub mod view;

#[cfg(test)]
mod tests;

use coqo_protocol::methods::{
    agents::{AgentState, PermissionRequest, SessionId, SessionInfo},
    questions::{Question, QuestionId},
    workspace::WorkspaceId,
};
use coqo_ui::keys;
use coqo_ui::{dock::register_tab_kind, gpui::App};

use crate::{
    questions::{self, primary_anchor},
    workspace::KnownWorkspaces,
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

/// The agents overview's tab kind. Persisted: never change it.
pub const KIND: &str = "agents";

coqo_ui::gpui::actions!(
    agents,
    [
        /// Show the agents overview (open it, or bring its tab forward).
        ShowAgents,
        /// Select the next session in the overview.
        SelectNext,
        /// Select the previous session.
        SelectPrev,
        /// Take the selected session's next step: answer its question, else
        /// open its chat.
        Confirm,
    ]
);

#[cfg(target_os = "macos")]
const SHOW_AGENTS_KEY: &str = "cmd-shift-a";
#[cfg(not(target_os = "macos"))]
const SHOW_AGENTS_KEY: &str = "ctrl-shift-a";

/// The overview's shortcut as shown (`welcome::shortcut` makes it `⌘` on
/// macOS).
pub const SHOW_AGENTS_LABEL: &str = "Ctrl Shift A";

/// The key context of the overview's list.
pub const KEY_CONTEXT: &str = "AgentsOverview";

/// Register the `agents` tab kind and the keys.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        keys::binding(SHOW_AGENTS_KEY, ShowAgents, None),
        keys::binding("down", SelectNext, Some(KEY_CONTEXT)),
        keys::binding("up", SelectPrev, Some(KEY_CONTEXT)),
        keys::binding("enter", Confirm, Some(KEY_CONTEXT)),
    ]);
    register_tab_kind(cx, KIND, |_, window, cx| Some(view::new(window, cx)));
}

// ---- order ------------------------------------------------------------------

/// The overview's groups, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    /// A question, a permission request, or blocked.
    NeedsYou,
    /// The last turn failed.
    Error,
    /// A turn is running.
    Working,
    /// Finished, or idle.
    Done,
}

/// The group a state is shown in.
pub fn group(state: AgentState) -> Group {
    match state {
        AgentState::NeedsInput | AgentState::NeedsPermission | AgentState::Blocked => {
            Group::NeedsYou
        }
        AgentState::Error => Group::Error,
        AgentState::Working => Group::Working,
        AgentState::Done | AgentState::Idle => Group::Done,
    }
}

/// When the session's state began: `state_since`, else its last change.
pub fn since(info: &SessionInfo) -> i64 {
    info.state_since.unwrap_or(info.updated_at)
}

/// The open sessions in overview order: by [`Group`]; what needs you the
/// longest-waiting first; the rest most recent first (done before idle).
pub fn ordered<'a>(sessions: impl IntoIterator<Item = &'a SessionInfo>) -> Vec<&'a SessionInfo> {
    let mut list: Vec<_> = sessions.into_iter().filter(|s| !s.closed).collect();
    list.sort_by(|a, b| {
        let (ga, gb) = (group(a.state), group(b.state));
        let when = if ga == Group::NeedsYou {
            since(a).cmp(&since(b))
        } else {
            since(b).cmp(&since(a))
        };
        ga.cmp(&gb)
            .then((a.state == AgentState::Idle).cmp(&(b.state == AgentState::Idle)))
            .then(when)
            .then(a.id.cmp(&b.id))
    });
    list
}

// ---- what a row says -----------------------------------------------------------

/// A session's title, or "New chat".
pub fn title(info: &SessionInfo) -> String {
    info.title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "New chat".to_owned())
}

/// The name of the session's workspace.
pub fn workspace_name(info: &SessionInfo, cx: &App) -> Option<String> {
    let id = WorkspaceId(info.workspace.clone()?);
    KnownWorkspaces::global(cx)
        .and_then(|k| k.read(cx).get(&id).map(|w| w.name.clone()))
        .or(Some(id.0))
}

/// The badge of a state, in capitals as on board 6.
pub fn badge(state: AgentState) -> &'static str {
    match state {
        AgentState::NeedsInput => "NEEDS INPUT",
        AgentState::NeedsPermission => "PERMISSION",
        AgentState::Blocked => "BLOCKED",
        AgentState::Error => "ERROR",
        AgentState::Working => "WORKING",
        AgentState::Done => "DONE",
        AgentState::Idle => "IDLE",
    }
}

/// The first line of markdown, without its emphasis and code marks.
pub fn plain_line(markdown: &str) -> String {
    let line = markdown
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_start_matches(['#', '>', '-', ' ']);
    line.chars()
        .filter(|c| !matches!(c, '*' | '`' | '_'))
        .collect()
}

/// A row's second line: what the agent is doing or what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    /// Plain text before `code`.
    pub text: String,
    /// A command or error, shown in the mono font.
    pub code: Option<String>,
}

impl Detail {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            code: None,
        }
    }
}

/// What a row says under the title, from the session, its waiting question
/// and its waiting permission request (when known).
pub fn detail(
    info: &SessionInfo,
    question: Option<&Question>,
    ask: Option<&PermissionRequest>,
) -> Detail {
    let status = crate::chat::status_line(info);
    match info.state {
        AgentState::NeedsInput => match question {
            Some(q) => {
                let mut text = plain_line(&q.body);
                if let Some(anchor) = primary_anchor(q) {
                    let place = questions::location(anchor);
                    let file = place.rsplit('/').next().unwrap_or(&place).to_owned();
                    text = format!("{text} · {file}");
                }
                Detail::text(text)
            }
            None => Detail::text(
                info.state_reason
                    .clone()
                    .or(status)
                    .unwrap_or_else(|| "Waiting for your answer".into()),
            ),
        },
        AgentState::NeedsPermission => match ask {
            Some(ask) => {
                let title = ask.title.trim();
                match title.strip_prefix('`').and_then(|t| t.strip_suffix('`')) {
                    Some(command) => Detail {
                        text: "Wants to run ".into(),
                        code: Some(command.to_owned()),
                    },
                    None => Detail::text(format!("Asks to: {title}")),
                }
            }
            None => Detail::text("Waiting for your permission"),
        },
        AgentState::Error => Detail {
            text: String::new(),
            code: Some(
                info.state_reason
                    .clone()
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "The turn failed".into()),
            ),
        },
        AgentState::Blocked => Detail::text(status.unwrap_or_else(|| "Blocked".into())),
        AgentState::Working => Detail::text(status.unwrap_or_else(|| "Working…".into())),
        AgentState::Done => Detail::text(status.unwrap_or_else(|| "Finished its turn".into())),
        AgentState::Idle => Detail::text(status.unwrap_or_else(|| "Ready for a prompt".into())),
    }
}

/// "2 of 3 criteria met", if the session has acceptance criteria.
pub fn criteria_line(info: &SessionInfo) -> Option<String> {
    use coqo_protocol::methods::agents::CriterionStatus;
    let total = info.criteria.len();
    (total > 0).then(|| {
        let met = info
            .criteria
            .iter()
            .filter(|c| c.status == CriterionStatus::Met)
            .count();
        format!("{met} of {total} criteria met")
    })
}

/// How long, short: `<1m`, `18m`, `2h`, `3d`.
pub fn elapsed(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..60 => "<1m".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The right-hand side of a row: the provider and how long the state has
/// lasted ("waiting 18m", "7m", "41m ago").
pub fn meta(info: &SessionInfo, provider: &str, now: i64) -> String {
    let span = elapsed(now - since(info));
    match group(info.state) {
        Group::NeedsYou => format!("{provider} · waiting {span}"),
        Group::Working => format!("{provider} · {span}"),
        Group::Error | Group::Done if span == "<1m" => format!("{provider} · just now"),
        Group::Error | Group::Done => format!("{provider} · {span} ago"),
    }
}

// ---- where a row leads ---------------------------------------------------------

/// Where opening a session leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTarget {
    /// Its chat tab.
    Chat(SessionId),
    /// A question it waits on, in its file, with the popover focused.
    Question {
        session: SessionId,
        question: QuestionId,
    },
}

impl AgentTarget {
    /// The session it leads to.
    pub fn session(&self) -> &SessionId {
        match self {
            Self::Chat(session) | Self::Question { session, .. } => session,
        }
    }
}

/// The open questions of `session` in a workspace's questions, the one it
/// waits on first, then oldest first.
pub fn questions_of<'a>(
    store: &'a questions::WorkspaceQuestions,
    session: &SessionId,
) -> Vec<&'a Question> {
    let mut list: Vec<_> = store
        .open()
        .into_iter()
        .filter(|q| &q.session == session && q.kind.is_question())
        .collect();
    list.sort_by_key(|q| (!q.waiting, q.created_at, q.id));
    list
}

/// Where the next step of `info` is: the question it waits on, else its
/// chat.
pub fn target_of(info: &SessionInfo, cx: &mut App) -> AgentTarget {
    let chat = AgentTarget::Chat(info.id.clone());
    if info.state != AgentState::NeedsInput {
        return chat;
    }
    let Some(workspace) = info.workspace.clone().map(WorkspaceId) else {
        return chat;
    };
    let Some(store) = questions::questions_for(&workspace, cx) else {
        return chat;
    };
    let question = questions_of(store.read(cx), &info.id).first().map(|q| q.id);
    match question {
        Some(question) => AgentTarget::Question {
            session: info.id.clone(),
            question,
        },
        None => chat,
    }
}

#[cfg(test)]
mod unit_tests {
    use coqo_protocol::methods::agents::{AgentState, SessionId, SessionInfo};

    use super::{Group, elapsed, group, meta, ordered, plain_line};

    pub(super) fn session(id: &str, state: AgentState, since: i64) -> SessionInfo {
        SessionInfo {
            id: SessionId(id.into()),
            provider: "fake".into(),
            workspace: Some("ws-a".into()),
            cwd: "/tmp".into(),
            title: None,
            state,
            state_reason: None,
            state_since: Some(since),
            status: None,
            criteria: Vec::new(),
            tool_use: Vec::new(),
            mode: None,
            closed: false,
            created_at: 0,
            updated_at: since,
        }
    }

    #[test]
    fn needs_you_first_then_errors_working_done_and_idle() {
        let mut closed = session("closed", AgentState::NeedsInput, 1);
        closed.closed = true;
        let sessions = [
            session("idle", AgentState::Idle, 90),
            session("done-old", AgentState::Done, 10),
            session("done-new", AgentState::Done, 80),
            session("working", AgentState::Working, 50),
            session("error", AgentState::Error, 40),
            session("asks-new", AgentState::NeedsPermission, 70),
            session("asks-old", AgentState::NeedsInput, 20),
            session("blocked", AgentState::Blocked, 30),
            closed,
        ];
        let ids: Vec<_> = ordered(&sessions)
            .into_iter()
            .map(|s| s.id.0.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "asks-old", "blocked", "asks-new", "error", "working", "done-new", "done-old",
                "idle"
            ]
        );
        assert_eq!(group(AgentState::Blocked), Group::NeedsYou);
    }

    #[test]
    fn rows_say_how_long() {
        assert_eq!(elapsed(30_000), "<1m");
        assert_eq!(elapsed(18 * 60_000), "18m");
        assert_eq!(elapsed(2 * 3_600_000 + 5), "2h");
        assert_eq!(elapsed(3 * 86_400_000), "3d");
        let now = 100 * 60_000;
        let waiting = session("a", AgentState::NeedsInput, now - 18 * 60_000);
        assert_eq!(
            meta(&waiting, "Claude Code", now),
            "Claude Code · waiting 18m"
        );
        let working = session("b", AgentState::Working, now - 7 * 60_000);
        assert_eq!(meta(&working, "Codex", now), "Codex · 7m");
        let failed = session("c", AgentState::Error, now - 41 * 60_000);
        assert_eq!(meta(&failed, "Gemini", now), "Gemini · 41m ago");
        let done = session("d", AgentState::Done, now);
        assert_eq!(meta(&done, "Claude", now), "Claude · just now");
    }

    #[test]
    fn plain_lines() {
        assert_eq!(plain_line("Why **1**?\nmore"), "Why 1?");
        assert_eq!(plain_line("\n## Use `foo`"), "Use foo");
    }
}
