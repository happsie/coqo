// SPDX-License-Identifier: Apache-2.0

//! A chat's transcript, folded from `sessions/updates` into what the chat tab
//! shows: messages, thoughts, tool calls, permission requests and notices,
//! plus the session's plan, modes, usage and billing source.
//!
//! Pure data, so it is tested without a window. [`Transcript::apply`] takes
//! updates in seq order, ignores ones it has seen (a resubscribe may repeat
//! a few), and reports which entries changed so the virtual list only
//! re-measures those.

use std::collections::HashMap;

use coqo_protocol::methods::agents::{
    AgentState, AuthKind, AuthStatus, AvailableCommand, ContentBlock, PermissionOutcome,
    PermissionRequest, PermissionRequestId, PermissionRule, PlanEntry, SessionConfigOption,
    SessionModes, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallId, ToolCallUpdate,
    Usage, config_category, follow_mode,
};
use coqo_protocol::methods::questions::{Question, QuestionId};
use coqo_protocol::methods::walkthroughs::WalkthroughSummary;

use super::diff::DiffPreview;

/// How the daemon words a provider's sign-in error (`ProviderError::AuthRequired`).
const SIGN_IN_REQUIRED: &str = "sign-in required";

/// A tool call and its diffs.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolEntry {
    pub call: ToolCall,
    /// One per `diff` content item.
    pub diffs: Vec<DiffPreview>,
}

impl ToolEntry {
    fn new(call: ToolCall) -> Self {
        let mut entry = Self {
            call,
            diffs: Vec::new(),
        };
        entry.refresh_diffs();
        entry
    }

    fn refresh_diffs(&mut self) {
        self.diffs = diffs_of(&self.call.content);
    }

    fn apply(&mut self, update: ToolCallUpdate) {
        let call = &mut self.call;
        if let Some(title) = update.title {
            call.title = title;
        }
        if let Some(kind) = update.kind {
            call.kind = kind;
        }
        if let Some(status) = update.status {
            call.status = status;
        }
        if let Some(locations) = update.locations {
            call.locations = locations;
        }
        if update.raw_input.is_some() {
            call.raw_input = update.raw_input;
        }
        if update.raw_output.is_some() {
            call.raw_output = update.raw_output;
        }
        if let Some(content) = update.content {
            call.content = content;
            self.refresh_diffs();
        }
    }

    /// Lines added and removed over all its diffs.
    pub fn counts(&self) -> (usize, usize) {
        self.diffs
            .iter()
            .fold((0, 0), |(a, r), d| (a + d.added, r + d.removed))
    }
}

/// The diffs among tool call content.
pub fn diffs_of(content: &[ToolCallContent]) -> Vec<DiffPreview> {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Diff {
                path,
                old_text,
                new_text,
            } => Some(DiffPreview::new(path, old_text.as_deref(), new_text)),
            _ => None,
        })
        .collect()
}

/// What a notice line is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// The turn was cancelled or interrupted.
    Stopped,
    /// The agent waits for the user, or is blocked.
    Attention,
    /// The turn or the provider failed.
    Error,
    /// The session was closed.
    Closed,
}

/// One row of the transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// The user's prompt.
    User {
        content: Vec<ContentBlock>,
    },
    /// Where an agent turn starts: the provider and model line.
    TurnStart {
        turn: u64,
        /// The model chosen when the turn started (the session's `model`
        /// config option), if the provider offers that choice.
        model: Option<String>,
    },
    /// Agent output (Markdown), streamed.
    Agent {
        text: String,
    },
    /// Agent reasoning, streamed; collapsed by default.
    Thought {
        text: String,
    },
    Tool(Box<ToolEntry>),
    /// A permission request, and its answer once given.
    Permission {
        request: Box<PermissionRequest>,
        outcome: Option<PermissionOutcome>,
        /// The workspace rule that answered it without asking.
        by_rule: Option<Box<PermissionRule>>,
        /// The rule the user's answer created.
        remembered: Option<Box<PermissionRule>>,
    },
    Notice {
        kind: NoticeKind,
        text: String,
    },
    /// A question or note the agent pinned to code, in its latest state
    /// (asked, then answered or dismissed).
    Question(Box<Question>),
    /// The agent published a code walkthrough: a card that opens it.
    Walkthrough(Box<WalkthroughSummary>),
}

/// What [`Transcript::apply`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Entries before this index were already there; the rest are new.
    pub appended_from: Option<usize>,
    /// Existing entries whose content changed.
    pub updated: Vec<usize>,
    /// Something outside the entries changed (plan, mode, state, …).
    pub meta: bool,
}

impl Changes {
    fn update(&mut self, ix: usize) {
        if self.appended_from.is_none_or(|from| ix < from) && !self.updated.contains(&ix) {
            self.updated.push(ix);
        }
    }

    fn append(&mut self, ix: usize) {
        self.appended_from.get_or_insert(ix);
    }

    pub fn is_empty(&self) -> bool {
        self.appended_from.is_none() && self.updated.is_empty() && !self.meta
    }
}

/// A session's transcript and folded state.
#[derive(Clone, Debug, Default)]
pub struct Transcript {
    /// The last seq applied (0: none).
    pub head: u64,
    pub entries: Vec<Entry>,
    tools: HashMap<ToolCallId, usize>,
    permissions: HashMap<PermissionRequestId, usize>,
    questions: HashMap<QuestionId, usize>,
    pub plan: Vec<PlanEntry>,
    pub modes: Option<SessionModes>,
    /// The session's config options (model, effort, …), in the agent's
    /// order.
    pub config_options: Vec<SessionConfigOption>,
    pub commands: Vec<AvailableCommand>,
    pub usage: Option<Usage>,
    pub auth: Option<AuthStatus>,
    pub state: AgentState,
    pub state_reason: Option<String>,
    /// A turn is running.
    pub running: bool,
    pub closed: bool,
    /// The provider needs the user to sign in, with its message.
    pub sign_in: Option<String>,
    /// The walkthroughs the agent published, oldest first.
    pub walkthroughs: Vec<WalkthroughSummary>,
}

impl Transcript {
    /// Apply a batch of updates.
    pub fn apply_all(
        &mut self,
        updates: impl IntoIterator<Item = (u64, SessionUpdate)>,
    ) -> Changes {
        let mut changes = Changes::default();
        for (seq, update) in updates {
            self.apply(seq, update, &mut changes);
        }
        changes
    }

    /// Apply one update with its seq. Seqs at or below [`Self::head`] are
    /// skipped.
    pub fn apply(&mut self, seq: u64, update: SessionUpdate, changes: &mut Changes) {
        if seq <= self.head {
            return;
        }
        self.head = seq;
        match update {
            SessionUpdate::UserMessage { content } => {
                self.push(Entry::User { content }, changes);
            }
            SessionUpdate::UserMessageChunk { content } => {
                if let Some(Entry::User { content: blocks }) = self.entries.last_mut() {
                    blocks.push(content);
                    changes.update(self.entries.len() - 1);
                } else {
                    self.push(
                        Entry::User {
                            content: vec![content],
                        },
                        changes,
                    );
                }
            }
            SessionUpdate::AgentMessageChunk { content } => {
                let text = block_text(&content);
                if let Some(Entry::Agent { text: t }) = self.entries.last_mut() {
                    t.push_str(&text);
                    changes.update(self.entries.len() - 1);
                } else {
                    self.push(Entry::Agent { text }, changes);
                }
            }
            SessionUpdate::AgentThoughtChunk { content } => {
                let text = block_text(&content);
                if let Some(Entry::Thought { text: t }) = self.entries.last_mut() {
                    t.push_str(&text);
                    changes.update(self.entries.len() - 1);
                } else {
                    self.push(Entry::Thought { text }, changes);
                }
            }
            SessionUpdate::ToolCall(call) => {
                if let Some(&ix) = self.tools.get(&call.id) {
                    self.entries[ix] = Entry::Tool(Box::new(ToolEntry::new(call)));
                    changes.update(ix);
                } else {
                    self.tools.insert(call.id.clone(), self.entries.len());
                    self.push(Entry::Tool(Box::new(ToolEntry::new(call))), changes);
                }
            }
            SessionUpdate::ToolCallUpdate(update) => match self.tools.get(&update.id) {
                Some(&ix) => {
                    if let Entry::Tool(tool) = &mut self.entries[ix] {
                        tool.apply(update);
                        changes.update(ix);
                    }
                }
                None => {
                    let mut tool = ToolEntry::new(ToolCall {
                        id: update.id.clone(),
                        title: String::new(),
                        name: None,
                        kind: Default::default(),
                        status: Default::default(),
                        content: Vec::new(),
                        locations: Vec::new(),
                        raw_input: None,
                        raw_output: None,
                    });
                    tool.apply(update);
                    self.tools.insert(tool.call.id.clone(), self.entries.len());
                    self.push(Entry::Tool(Box::new(tool)), changes);
                }
            },
            SessionUpdate::Plan { entries } => {
                self.plan = entries;
                changes.meta = true;
            }
            SessionUpdate::AvailableCommands { commands } => {
                self.commands = commands;
                changes.meta = true;
            }
            SessionUpdate::Modes(modes) => {
                self.modes = Some(modes);
                changes.meta = true;
            }
            SessionUpdate::CurrentMode { mode_id } => {
                follow_mode(&mut self.config_options, &mode_id);
                if let Some(modes) = &mut self.modes {
                    modes.current_mode_id = mode_id;
                }
                changes.meta = true;
            }
            SessionUpdate::ConfigOptions { config_options } => {
                if let Some(modes) = &mut self.modes {
                    modes.follow(&config_options);
                }
                self.config_options = config_options;
                changes.meta = true;
            }
            SessionUpdate::Usage(usage) => {
                self.usage = Some(usage);
                changes.meta = true;
            }
            SessionUpdate::AuthStatus(auth) => {
                if auth.kind != AuthKind::None {
                    self.sign_in = None;
                }
                self.auth = Some(auth);
                changes.meta = true;
            }
            SessionUpdate::PermissionRequested(request) => {
                self.permissions.insert(request.id, self.entries.len());
                self.push(
                    Entry::Permission {
                        request: Box::new(request),
                        outcome: None,
                        by_rule: None,
                        remembered: None,
                    },
                    changes,
                );
            }
            SessionUpdate::PermissionResolved {
                request_id,
                outcome: answer,
                by_rule: rule,
                remembered: created,
            } => {
                if let Some(&ix) = self.permissions.get(&request_id)
                    && let Entry::Permission {
                        outcome,
                        by_rule,
                        remembered,
                        ..
                    } = &mut self.entries[ix]
                {
                    *outcome = Some(answer);
                    *by_rule = rule.map(Box::new);
                    *remembered = created.map(Box::new);
                    changes.update(ix);
                }
            }
            SessionUpdate::State { state, reason } => {
                self.state = state;
                self.state_reason = reason.clone();
                changes.meta = true;
                if let Some(reason) = reason {
                    match state {
                        AgentState::NeedsInput | AgentState::Blocked => {
                            self.push(
                                Entry::Notice {
                                    kind: NoticeKind::Attention,
                                    text: reason,
                                },
                                changes,
                            );
                        }
                        AgentState::Error => self.error(reason, changes),
                        _ => {}
                    }
                }
            }
            SessionUpdate::TurnStarted { turn } => {
                self.running = true;
                changes.meta = true;
                let model = self.model_option().map(SessionConfigOption::value_name);
                self.push(Entry::TurnStart { turn, model }, changes);
            }
            SessionUpdate::TurnEnded {
                stop_reason, error, ..
            } => {
                self.running = false;
                changes.meta = true;
                let notice = match stop_reason {
                    StopReason::EndTurn => {
                        self.sign_in = None;
                        None
                    }
                    StopReason::Cancelled => Some((NoticeKind::Stopped, "Stopped".to_owned())),
                    StopReason::Interrupted => Some((
                        NoticeKind::Stopped,
                        "Interrupted: the daemon stopped during this turn".to_owned(),
                    )),
                    StopReason::MaxTokens => Some((
                        NoticeKind::Attention,
                        "Stopped at the token limit".to_owned(),
                    )),
                    StopReason::MaxTurnRequests => Some((
                        NoticeKind::Attention,
                        "Stopped at the request limit for one turn".to_owned(),
                    )),
                    StopReason::Refusal => {
                        Some((NoticeKind::Attention, "The agent declined".to_owned()))
                    }
                    StopReason::Error => {
                        self.error(error.unwrap_or_else(|| "The turn failed".into()), changes);
                        None
                    }
                };
                if let Some((kind, text)) = notice {
                    self.push(Entry::Notice { kind, text }, changes);
                }
            }
            // A question card, updated in place when it is answered or
            // dismissed. The waiting state already shows as a notice.
            SessionUpdate::Question(question) => match self.questions.get(&question.id) {
                Some(&ix) => {
                    self.entries[ix] = Entry::Question(question);
                    changes.update(ix);
                }
                None => {
                    self.questions.insert(question.id, self.entries.len());
                    self.push(Entry::Question(question), changes);
                }
            },
            SessionUpdate::Walkthrough(w) => {
                changes.meta = true;
                self.walkthroughs.retain(|o| o.id != w.id);
                self.walkthroughs.push((*w).clone());
                self.push(Entry::Walkthrough(w), changes);
            }
            SessionUpdate::Closed => {
                self.closed = true;
                self.running = false;
                changes.meta = true;
                self.push(
                    Entry::Notice {
                        kind: NoticeKind::Closed,
                        text: "This chat was closed".into(),
                    },
                    changes,
                );
            }
        }
    }

    /// Record an error, once: the daemon reports a failed turn both as the
    /// turn's end and as the `error` state.
    pub fn error(&mut self, text: String, changes: &mut Changes) {
        if text.contains(SIGN_IN_REQUIRED) {
            self.sign_in = Some(text.clone());
            changes.meta = true;
        }
        let repeated = matches!(
            self.entries.last(),
            Some(Entry::Notice { kind: NoticeKind::Error, text: t }) if *t == text
        );
        if !repeated {
            self.push(
                Entry::Notice {
                    kind: NoticeKind::Error,
                    text,
                },
                changes,
            );
        }
    }

    fn push(&mut self, entry: Entry, changes: &mut Changes) {
        changes.append(self.entries.len());
        self.entries.push(entry);
    }

    /// The current mode's display name.
    /// The `model` config option, if the provider offers a choice of model.
    pub fn model_option(&self) -> Option<&SessionConfigOption> {
        self.config_options
            .iter()
            .find(|o| o.is(config_category::MODEL))
    }

    /// Whether a `mode` config option stands in for the modes (its chip
    /// replaces the mode chip).
    pub fn has_mode_option(&self) -> bool {
        self.config_options
            .iter()
            .any(|o| o.is(config_category::MODE))
    }

    /// The model to show: the chosen one, else the one the provider's usage
    /// reports name.
    pub fn model_name(&self) -> Option<String> {
        self.model_option()
            .map(SessionConfigOption::value_name)
            .or_else(|| self.usage.as_ref().and_then(|u| u.model.clone()))
    }

    pub fn mode_name(&self) -> Option<&str> {
        let modes = self.modes.as_ref()?;
        modes
            .available_modes
            .iter()
            .find(|m| m.id == modes.current_mode_id)
            .map(|m| m.name.as_str())
            .or(Some(modes.current_mode_id.as_str()))
    }

    /// Permission requests still waiting for an answer, oldest first.
    pub fn pending_permissions(&self) -> impl Iterator<Item = &PermissionRequest> {
        self.entries.iter().filter_map(|e| match e {
            Entry::Permission {
                request,
                outcome: None,
                ..
            } => Some(&**request),
            _ => None,
        })
    }

    /// The entry of permission request `id`.
    pub fn permission_index(&self, id: PermissionRequestId) -> Option<usize> {
        self.permissions.get(&id).copied()
    }

    /// The tool call `id`, as reported so far.
    pub fn tool(&self, id: &ToolCallId) -> Option<&ToolEntry> {
        match self.entries.get(*self.tools.get(id)?)? {
            Entry::Tool(tool) => Some(tool),
            _ => None,
        }
    }

    /// Files the agent changed or proposes to change, with `+`/`−` counts
    /// summed per file, in first-touched order.
    pub fn changed_files(&self) -> Vec<(String, usize, usize)> {
        let mut files: Vec<(String, usize, usize)> = Vec::new();
        for entry in &self.entries {
            let Entry::Tool(tool) = entry else { continue };
            for diff in &tool.diffs {
                match files.iter_mut().find(|(p, ..)| *p == diff.path) {
                    Some((_, a, r)) => {
                        *a += diff.added;
                        *r += diff.removed;
                    }
                    None => files.push((diff.path.clone(), diff.added, diff.removed)),
                }
            }
        }
        files
    }
}

/// A content block as Markdown text.
pub fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Image { .. } => "*[image]*".into(),
        ContentBlock::ResourceLink { uri, name, .. } => format!("[{name}]({uri})"),
        ContentBlock::Resource { uri, text, .. } => format!("`{uri}`\n\n```\n{text}\n```\n"),
    }
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::agents::{
        PermissionOption, PermissionOptionKind, PlanEntryStatus, SessionMode, ToolCallStatus,
        ToolKind,
    };

    use super::*;

    fn text(t: &str) -> ContentBlock {
        ContentBlock::text(t)
    }

    fn tool(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId(id.into()),
            title: "Write a.txt".into(),
            name: None,
            kind: ToolKind::Edit,
            status: ToolCallStatus::Pending,
            content: vec![ToolCallContent::Diff {
                path: "/w/a.txt".into(),
                old_text: Some("one\n".into()),
                new_text: "one\ntwo\n".into(),
            }],
            locations: vec![],
            raw_input: None,
            raw_output: None,
        }
    }

    #[test]
    fn chunks_stream_into_one_message_and_repeats_are_skipped() {
        let mut t = Transcript::default();
        let c = t.apply_all([
            (
                1,
                SessionUpdate::UserMessage {
                    content: vec![text("hi")],
                },
            ),
            (2, SessionUpdate::TurnStarted { turn: 1 }),
            (
                3,
                SessionUpdate::AgentThoughtChunk {
                    content: text("hmm"),
                },
            ),
            (
                4,
                SessionUpdate::AgentMessageChunk {
                    content: text("Hel"),
                },
            ),
        ]);
        assert_eq!(c.appended_from, Some(0));
        assert!(t.running);
        let c = t.apply_all([
            (
                4,
                SessionUpdate::AgentMessageChunk {
                    content: text("Hel"),
                },
            ),
            (
                5,
                SessionUpdate::AgentMessageChunk {
                    content: text("lo"),
                },
            ),
        ]);
        assert_eq!(c.updated, vec![3]);
        assert_eq!(c.appended_from, None);
        assert_eq!(
            t.entries[3],
            Entry::Agent {
                text: "Hello".into()
            }
        );
        assert_eq!(t.head, 5);
    }

    #[test]
    fn tool_calls_update_in_place_and_count_their_diffs() {
        let mut t = Transcript::default();
        t.apply_all([(1, SessionUpdate::ToolCall(tool("t1")))]);
        let c = t.apply_all([(
            2,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                id: ToolCallId("t1".into()),
                status: Some(ToolCallStatus::Completed),
                ..ToolCallUpdate::default()
            }),
        )]);
        assert_eq!(c.updated, vec![0]);
        let Entry::Tool(entry) = &t.entries[0] else {
            panic!("{:?}", t.entries[0])
        };
        assert_eq!(entry.call.status, ToolCallStatus::Completed);
        assert_eq!(entry.counts(), (1, 0));
        assert_eq!(t.changed_files(), vec![("/w/a.txt".to_owned(), 1, 0)]);
    }

    #[test]
    fn permissions_plan_modes_and_errors() {
        let mut t = Transcript::default();
        t.apply_all([
            (
                1,
                SessionUpdate::Modes(SessionModes {
                    current_mode_id: "default".into(),
                    available_modes: vec![SessionMode {
                        id: "default".into(),
                        name: "Manual".into(),
                        description: None,
                    }],
                }),
            ),
            (
                2,
                SessionUpdate::Plan {
                    entries: vec![PlanEntry {
                        content: "Step".into(),
                        priority: Default::default(),
                        status: PlanEntryStatus::InProgress,
                    }],
                },
            ),
            (
                3,
                SessionUpdate::PermissionRequested(PermissionRequest {
                    id: PermissionRequestId(7),
                    title: "Write a.txt".into(),
                    tool_call: None,
                    options: vec![PermissionOption {
                        id: "allow".into(),
                        name: "Allow".into(),
                        kind: PermissionOptionKind::AllowOnce,
                    }],
                    suggested_rule: None,
                }),
            ),
        ]);
        assert_eq!(t.mode_name(), Some("Manual"));
        assert_eq!(t.plan.len(), 1);
        assert_eq!(t.pending_permissions().count(), 1);
        t.apply_all([(
            4,
            SessionUpdate::PermissionResolved {
                request_id: PermissionRequestId(7),
                outcome: PermissionOutcome::Selected {
                    option_id: "allow".into(),
                },
                by_rule: None,
                remembered: None,
            },
        )]);
        assert_eq!(t.pending_permissions().count(), 0);

        let err = "provider failed: sign-in required: run claude".to_owned();
        t.apply_all([
            (
                5,
                SessionUpdate::TurnEnded {
                    turn: 1,
                    stop_reason: StopReason::Error,
                    error: Some(err.clone()),
                },
            ),
            (
                6,
                SessionUpdate::State {
                    state: AgentState::Error,
                    reason: Some(err.clone()),
                },
            ),
        ]);
        let errors = t
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Entry::Notice {
                        kind: NoticeKind::Error,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(errors, 1, "{:?}", t.entries);
        assert_eq!(t.sign_in.as_deref(), Some(err.as_str()));
        assert_eq!(t.state, AgentState::Error);
    }

    #[test]
    fn a_question_is_one_card_updated_as_it_is_answered() {
        use coqo_protocol::methods::{
            agents::SessionId,
            questions::{QuestionAnswer, QuestionKind, QuestionStatus},
            workspace::WorkspaceId,
        };
        let asked = Question {
            id: QuestionId(4),
            workspace: WorkspaceId("w".into()),
            session: SessionId("s".into()),
            author: Default::default(),
            kind: QuestionKind::Clarification,
            severity: None,
            body: "Which rounding mode?".into(),
            options: vec![],
            anchors: vec![],
            status: QuestionStatus::Open,
            answer: None,
            waiting: true,
            thread: vec![],
            created_at: 0,
            closed_at: None,
            seq: 1,
        };
        let answered = Question {
            status: QuestionStatus::Answered,
            answer: Some(QuestionAnswer {
                option: None,
                text: Some("HALF_EVEN".into()),
            }),
            waiting: false,
            seq: 2,
            ..asked.clone()
        };
        let mut t = Transcript::default();
        let c = t.apply_all([(1, SessionUpdate::Question(Box::new(asked)))]);
        assert_eq!(c.appended_from, Some(0));
        let c = t.apply_all([(2, SessionUpdate::Question(Box::new(answered.clone())))]);
        assert_eq!((c.appended_from, c.updated), (None, vec![0]));
        assert_eq!(t.entries, vec![Entry::Question(Box::new(answered))]);
    }
}
