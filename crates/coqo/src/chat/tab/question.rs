// SPDX-License-Identifier: Apache-2.0

//! The agent's own questions in the chat (#339, ADR 0029): Claude Code's
//! `AskUserQuestion`, an MCP server's form, any provider's question that
//! comes as a [`PermissionRequest`] with `questions`. Styled after board 2's
//! question popover and the chat's permission card.
//!
//! - **The card** shows each question with its header, its options (a
//!   number, the label and the description; a tick box for a multi-select)
//!   and, where the agent takes one, a field for a typed answer ("Other",
//!   or a `text` / `number` question's answer). An option's preview (a
//!   mockup, a snippet) shows under the options while it is under the
//!   cursor or picked.
//! - **Keys** (on the card, never while typing in its field, #266): `1`–`9`
//!   pick an option of the question under the cursor, `Up`/`Down` move the
//!   cursor (into the question's field and out again), `Space` picks or
//!   unticks, `Enter` sends, `Esc` skips. In the field, `Enter` sends and
//!   `Esc` goes back to the options. Focus jumps to a new question card
//!   unless you are typing a message, and back to the composer once it is
//!   answered.
//! - **Answers** go back through the request that asked
//!   (`sessions/respondPermission`, `answered`), so the agent gets them in
//!   the shape its tool expects; **Skip** / `Esc` is `declined`, which the
//!   agent reads as "the user skipped", never a hang.
//! - **When the agent stopped waiting** (the turn was stopped, or the
//!   daemon restarted mid-question) and nothing was said since, the card
//!   stays answerable: **Send as a message** sends the answers as the next
//!   prompt, which resumes the agent's session.

use std::collections::HashMap;

use coqo_protocol::methods::agents::{
    AgentQuestion, AgentQuestionKind, ContentBlock, PermissionOutcome, PermissionRequest,
    PermissionRequestId, Prompt, PromptParams, QuestionAnswer, RespondPermission,
    RespondPermissionParams,
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, AppContext as _, Context, ElementId, Entity, FocusHandle, Focusable as _,
        FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
        prelude::FluentBuilder as _, px,
    },
    input::{PromptInput, PromptInputEvent, PromptKey},
    widgets::{Icon, TestSupportExt as _, icon, primary, raised},
};

use super::ChatTab;
use crate::chat::{
    NextAnswer, PickAnswer, PreviousAnswer, SendAnswers, SkipQuestions, ToggleAnswer,
    transcript::Entry,
};

/// Lines of an option's preview shown at most.
const PREVIEW_LINES: usize = 14;

/// Where focus goes on the next frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum QuestionFocus {
    /// The active question card.
    Card,
    /// A new active card: its field, if the cursor starts there (a question
    /// with no options), else the card.
    Jump,
    /// The field of question `.1` of request `.0`.
    Input(PermissionRequestId, String),
    /// Back to the composer, if a question card had it.
    Composer,
}

/// The chat tab's question cards.
pub(super) struct QuestionCards {
    /// The focus of the active card (the oldest waiting one).
    pub focus: FocusHandle,
    forms: HashMap<PermissionRequestId, Form>,
    /// The newest card focus jumped to.
    jumped_to: Option<PermissionRequestId>,
    pub focus_next: Option<QuestionFocus>,
}

impl QuestionCards {
    pub fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            forms: HashMap::new(),
            jumped_to: None,
            focus_next: None,
        }
    }
}

/// What the user has filled in on one card.
#[derive(Default)]
struct Form {
    /// Picked option values, by question id, in the order picked.
    picks: HashMap<String, Vec<String>>,
    /// Each question's field, if it takes typed text.
    inputs: HashMap<String, Entity<PromptInput>>,
    /// The question and option under the cursor; the option index equal to
    /// the number of options is the question's field.
    cursor: (usize, usize),
    error: Option<String>,
    /// The answer is on its way.
    sent: bool,
    _subscriptions: Vec<Subscription>,
}

/// Whether question `q` takes typed text.
fn typed(q: &AgentQuestion) -> bool {
    q.other || matches!(q.kind, AgentQuestionKind::Text | AgentQuestionKind::Number)
}

/// The cursor stops of `questions`, in order: each option, then the field.
fn stops(questions: &[AgentQuestion]) -> Vec<(usize, usize)> {
    let mut stops = Vec::new();
    for (qi, q) in questions.iter().enumerate() {
        stops.extend((0..q.options.len()).map(|oi| (qi, oi)));
        if typed(q) {
            stops.push((qi, q.options.len()));
        }
    }
    stops
}

/// How an answer reads: the picked labels, then the typed text.
pub(super) fn answer_text(q: &AgentQuestion, answer: &QuestionAnswer) -> String {
    let mut parts: Vec<String> = answer
        .picks
        .iter()
        .map(|v| {
            q.options
                .iter()
                .find(|o| &o.value == v)
                .map_or_else(|| v.clone(), |o| o.label.clone())
        })
        .collect();
    if let Some(text) = answer
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        parts.push(text.to_owned());
    }
    parts.join(", ")
}

/// The answers as a message, for an agent that no longer waits for them.
pub(super) fn answers_message(request: &PermissionRequest, answers: &[QuestionAnswer]) -> String {
    let plural = if request.questions.len() == 1 {
        ""
    } else {
        "s"
    };
    let mut text = format!("My answer{plural} to your question{plural} from before:\n");
    for q in &request.questions {
        let Some(answer) = answers.iter().find(|a| a.id == q.id) else {
            continue;
        };
        let line = answer_text(q, answer);
        if !line.is_empty() {
            text.push_str(&format!("\n- {}: {line}", q.question.trim()));
        }
    }
    text
}

/// What a question is called in messages: its header, else its text.
fn name_of(q: &AgentQuestion) -> String {
    let name = q.header.as_deref().unwrap_or(&q.question).trim();
    let mut line: String = name
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(60)
        .collect();
    if line.len() < name.len() {
        line.push('…');
    }
    line
}

impl ChatTab {
    /// Whether the active question card has the focus.
    #[cfg(test)]
    pub fn question_focused(&self, window: &Window) -> bool {
        self.questions.focus.is_focused(window)
    }

    /// The field of question `question` of request `request`, once shown.
    #[cfg(test)]
    pub fn question_input(
        &self,
        request: PermissionRequestId,
        question: &str,
    ) -> Option<Entity<PromptInput>> {
        self.questions
            .forms
            .get(&request)?
            .inputs
            .get(question)
            .cloned()
    }

    /// The picked values of question `question` of request `request`.
    #[cfg(test)]
    pub fn question_picks(&self, request: PermissionRequestId, question: &str) -> Vec<String> {
        self.questions
            .forms
            .get(&request)
            .and_then(|f| f.picks.get(question).cloned())
            .unwrap_or_default()
    }

    /// The question card's error line, if it has one.
    #[cfg(test)]
    pub fn question_error(&self, request: PermissionRequestId) -> Option<String> {
        self.questions.forms.get(&request)?.error.clone()
    }

    /// The question request of entry `ix` and its outcome.
    fn question_at(&self, ix: usize) -> Option<(&PermissionRequest, Option<&PermissionOutcome>)> {
        match self.transcript.entries.get(ix)? {
            Entry::Permission {
                request, outcome, ..
            } if request.is_question() => Some((request, outcome.as_ref())),
            _ => None,
        }
    }

    /// Whether the question card of entry `ix` takes an answer: it waits,
    /// or the agent stopped waiting and nothing was said since (the answer
    /// then goes as a message).
    fn question_live(&self, ix: usize) -> bool {
        let Some((_, outcome)) = self.question_at(ix) else {
            return false;
        };
        match outcome {
            None => !self.is_closed(),
            Some(PermissionOutcome::Cancelled) => self.question_answerable_later(ix),
            Some(_) => false,
        }
    }

    /// Whether the cancelled question of entry `ix` can still be answered
    /// as a message.
    fn question_answerable_later(&self, ix: usize) -> bool {
        !self.transcript.running
            && !self.is_closed()
            && self.transcript.ended.is_none()
            && !self.transcript.entries[ix + 1..]
                .iter()
                .any(|e| matches!(e, Entry::User { .. } | Entry::TurnStart { .. }))
    }

    /// The card that has the keys: the oldest waiting question, else the
    /// one that can still be answered as a message.
    fn active_question(&self) -> Option<(usize, PermissionRequestId)> {
        let mut later = None;
        for (ix, entry) in self.transcript.entries.iter().enumerate() {
            let Entry::Permission {
                request, outcome, ..
            } = entry
            else {
                continue;
            };
            if !request.is_question() {
                continue;
            }
            match outcome {
                None if !self.is_closed() => return Some((ix, request.id)),
                Some(PermissionOutcome::Cancelled) if self.question_answerable_later(ix) => {
                    later = Some((ix, request.id));
                }
                _ => {}
            }
        }
        later
    }

    fn remeasure_question(&mut self, id: PermissionRequestId) {
        if let Some(ix) = self.transcript.permission_index(id) {
            self.list.remeasure_items(ix..ix + 1);
        }
    }

    /// Make the fields of every live question card, and drop the state of
    /// the others. Call while rendering.
    pub(super) fn ensure_question_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let live: Vec<(PermissionRequestId, Vec<AgentQuestion>)> =
            (0..self.transcript.entries.len())
                .filter(|&ix| self.question_live(ix))
                .filter_map(|ix| {
                    let (request, _) = self.question_at(ix)?;
                    Some((request.id, request.questions.clone()))
                })
                .collect();
        self.questions
            .forms
            .retain(|id, _| live.iter().any(|(l, _)| l == id));
        for (id, questions) in live {
            if self.questions.forms.contains_key(&id) {
                continue;
            }
            let mut form = Form::default();
            for q in questions.iter().filter(|q| typed(q)) {
                let placeholder = match q.kind {
                    AgentQuestionKind::Text => "Your answer",
                    AgentQuestionKind::Number => "A number",
                    _ => "Or type your own answer",
                };
                let input = cx.new(|cx| {
                    let mut input = PromptInput::new(placeholder, 1, 4, window, cx);
                    input.set_edge_keys(true);
                    input
                });
                let qid = q.id.clone();
                form._subscriptions.push(cx.subscribe_in(
                    &input,
                    window,
                    move |this, _, event: &PromptInputEvent, window, cx| match event {
                        PromptInputEvent::Submit => this.send_answers(id, window, cx),
                        PromptInputEvent::Cancel => {
                            this.questions.focus_next = Some(QuestionFocus::Card);
                            cx.notify();
                        }
                        PromptInputEvent::Changed => {
                            if let Some(form) = this.questions.forms.get_mut(&id) {
                                form.error = None;
                            }
                            this.remeasure_question(id);
                            cx.notify();
                        }
                        PromptInputEvent::Key(PromptKey::Up) => {
                            this.move_out_of_input(id, &qid, false, cx);
                        }
                        PromptInputEvent::Key(PromptKey::Down) => {
                            this.move_out_of_input(id, &qid, true, cx);
                        }
                        PromptInputEvent::Key(_) => {}
                    },
                ));
                form.inputs.insert(q.id.clone(), input);
            }
            self.questions.forms.insert(id, form);
        }
    }

    /// After a batch: jump to a new question card.
    pub(super) fn jump_to_question(&mut self) {
        let Some((ix, id)) = self.active_question() else {
            return;
        };
        if self.questions.jumped_to.is_some_and(|j| j >= id) {
            return;
        }
        self.questions.jumped_to = Some(id);
        self.questions.focus_next = Some(QuestionFocus::Jump);
        if !self.list.is_following_tail() {
            self.list.scroll_to_reveal_item(ix);
        }
    }

    /// Moves focus as the last updates or keys asked. Call while rendering.
    pub(super) fn apply_question_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.questions.focus_next.take() else {
            return;
        };
        let composer = self.composer.read(cx).focus_handle(cx);
        let in_card = self.questions.focus.contains_focused(window, cx);
        let target = match target {
            QuestionFocus::Jump => self.jump_target(),
            other => other,
        };
        match target {
            QuestionFocus::Card | QuestionFocus::Jump => {
                // Don't take a message being typed: Enter would send.
                let typing =
                    composer.is_focused(window) && !self.composer.read(cx).text(cx).is_empty();
                if (!typing || in_card) && self.active_question().is_some() {
                    window.focus(&self.questions.focus, cx);
                }
            }
            QuestionFocus::Input(id, qid) => {
                let typing =
                    composer.is_focused(window) && !self.composer.read(cx).text(cx).is_empty();
                if typing && !in_card {
                    return;
                }
                if let Some(input) = self
                    .questions
                    .forms
                    .get(&id)
                    .and_then(|f| f.inputs.get(&qid))
                {
                    let focus = input.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                }
            }
            QuestionFocus::Composer => {
                if in_card || self.questions.focus.is_focused(window) {
                    window.focus(&composer, cx);
                }
            }
        }
    }

    /// Where a jump to the active card lands: the field under its cursor,
    /// else the card.
    fn jump_target(&self) -> QuestionFocus {
        let Some(id) = self.active_id() else {
            return QuestionFocus::Card;
        };
        let cursor = self.questions.forms.get(&id).map_or((0, 0), |f| f.cursor);
        let field = self
            .questions_of(id)
            .and_then(|(_, r)| r.questions.get(cursor.0))
            .filter(|q| typed(q) && cursor.1 == q.options.len())
            .map(|q| q.id.clone());
        match field {
            Some(qid) => QuestionFocus::Input(id, qid),
            None => QuestionFocus::Card,
        }
    }

    fn questions_of(&self, id: PermissionRequestId) -> Option<(usize, &PermissionRequest)> {
        let ix = self.transcript.permission_index(id)?;
        let (request, _) = self.question_at(ix)?;
        Some((ix, request))
    }

    /// Step the cursor of card `id` one stop on (or back), into a
    /// question's field when it reaches one.
    fn step_cursor(&mut self, id: PermissionRequestId, forward: bool, cx: &mut Context<Self>) {
        let Some((_, request)) = self.questions_of(id) else {
            return;
        };
        let stops = stops(&request.questions);
        let questions = request.questions.clone();
        let Some(form) = self.questions.forms.get_mut(&id) else {
            return;
        };
        let at = stops.iter().position(|s| *s == form.cursor).unwrap_or(0);
        let next = if forward {
            (at + 1).min(stops.len().saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        let Some(&cursor) = stops.get(next) else {
            return;
        };
        form.cursor = cursor;
        let (qi, oi) = cursor;
        if oi == questions[qi].options.len() {
            self.questions.focus_next = Some(QuestionFocus::Input(id, questions[qi].id.clone()));
        }
        self.remeasure_question(id);
        cx.notify();
    }

    /// `Up` / `Down` in question `qid`'s field: back to its options, or on
    /// to the next question's.
    fn move_out_of_input(
        &mut self,
        id: PermissionRequestId,
        qid: &str,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((_, request)) = self.questions_of(id) else {
            return;
        };
        let Some(qi) = request.questions.iter().position(|q| q.id == qid) else {
            return;
        };
        let options = request.questions[qi].options.len();
        if let Some(form) = self.questions.forms.get_mut(&id) {
            form.cursor = (qi, options);
        }
        self.questions.focus_next = Some(QuestionFocus::Card);
        self.step_cursor(id, forward, cx);
        // Stepping on from the last field stays there: back to the card.
        if matches!(&self.questions.focus_next, Some(QuestionFocus::Input(_, q)) if q == qid) {
            self.questions.focus_next = Some(QuestionFocus::Card);
        }
    }

    /// Pick option `oi` of question `qi` of card `id`: the one answer of a
    /// single-select, a tick (or untick) of a multi-select.
    fn pick_answer(
        &mut self,
        id: PermissionRequestId,
        qi: usize,
        oi: usize,
        cx: &mut Context<Self>,
    ) {
        let Some((_, request)) = self.questions_of(id) else {
            return;
        };
        let Some(q) = request.questions.get(qi) else {
            return;
        };
        let Some(option) = q.options.get(oi) else {
            return;
        };
        let (qid, value, multi) = (
            q.id.clone(),
            option.value.clone(),
            q.kind == AgentQuestionKind::Multi,
        );
        let Some(form) = self.questions.forms.get_mut(&id) else {
            return;
        };
        if form.sent {
            return;
        }
        let picks = form.picks.entry(qid).or_default();
        if multi {
            if let Some(at) = picks.iter().position(|p| *p == value) {
                picks.remove(at);
            } else {
                picks.push(value);
            }
        } else if picks.first() == Some(&value) {
            picks.clear();
        } else {
            *picks = vec![value];
        }
        form.cursor = (qi, oi);
        form.error = None;
        self.remeasure_question(id);
        cx.notify();
    }

    fn active_id(&self) -> Option<PermissionRequestId> {
        self.active_question().map(|(_, id)| id)
    }

    /// `1`–`9` on the active card.
    fn pick_by_number(&mut self, n: usize, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.active_id() else {
            return false;
        };
        let qi = self.questions.forms.get(&id).map_or(0, |f| f.cursor.0);
        let options = self
            .questions_of(id)
            .and_then(|(_, r)| r.questions.get(qi))
            .map_or(0, |q| q.options.len());
        if n == 0 || n > options {
            return false;
        }
        self.pick_answer(id, qi, n - 1, cx);
        true
    }

    /// `Space` on the active card.
    fn toggle_at_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active_id() else {
            return;
        };
        let Some((qi, oi)) = self.questions.forms.get(&id).map(|f| f.cursor) else {
            return;
        };
        self.pick_answer(id, qi, oi, cx);
    }

    /// The answers on card `id`, or what is missing.
    fn collect_answers(
        &self,
        request: &PermissionRequest,
        cx: &Context<Self>,
    ) -> Result<Vec<QuestionAnswer>, String> {
        let form = self.questions.forms.get(&request.id);
        let mut answers = Vec::new();
        for q in &request.questions {
            let picks = form
                .and_then(|f| f.picks.get(&q.id).cloned())
                .unwrap_or_default();
            let text = form
                .and_then(|f| f.inputs.get(&q.id))
                .map(|i| i.read(cx).text(cx).trim().to_owned())
                .filter(|t| !t.is_empty());
            if q.kind == AgentQuestionKind::Number
                && let Some(t) = &text
                && t.parse::<f64>().is_err()
            {
                return Err(format!("“{}” takes a number", name_of(q)));
            }
            if picks.is_empty() && text.is_none() {
                if q.required {
                    return Err(format!("Answer “{}” first", name_of(q)));
                }
                continue;
            }
            answers.push(QuestionAnswer {
                id: q.id.clone(),
                picks,
                text,
            });
        }
        if answers.is_empty() {
            return Err("Pick an answer, or skip the question".into());
        }
        Ok(answers)
    }

    /// Send card `id`'s answers: through the request while the agent
    /// waits, else as a message.
    pub(super) fn send_answers(
        &mut self,
        id: PermissionRequestId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        let Some(ix) = self.transcript.permission_index(id) else {
            return;
        };
        if !self.question_live(ix) || self.questions.forms.get(&id).is_some_and(|f| f.sent) {
            return;
        }
        let Some((request, outcome)) = self.question_at(ix) else {
            return;
        };
        let (request, waiting) = (request.clone(), outcome.is_none());
        let answers = match self.collect_answers(&request, cx) {
            Ok(answers) => answers,
            Err(e) => {
                if let Some(form) = self.questions.forms.get_mut(&id) {
                    form.error = Some(e);
                }
                self.remeasure_question(id);
                cx.notify();
                return;
            }
        };
        let Some(daemon) = Self::daemon(cx) else {
            self.error = Some("coqod isn't connected; the answer wasn't sent".into());
            cx.notify();
            return;
        };
        if waiting {
            let call = daemon
                .read(cx)
                .request::<RespondPermission>(RespondPermissionParams {
                    session: self.session.clone(),
                    request_id: id,
                    outcome: PermissionOutcome::Answered { answers },
                    remember: false,
                });
            self.spawn_request(call, "Couldn't send the answer", cx);
        } else {
            let text = answers_message(&request, &answers);
            let call = daemon.read(cx).request::<Prompt>(PromptParams {
                session: self.session.clone(),
                content: vec![ContentBlock::text(text)],
            });
            self.spawn_request(call, "The answer wasn't sent", cx);
        }
        if let Some(form) = self.questions.forms.get_mut(&id) {
            form.sent = true;
        }
        self.questions.focus_next = Some(QuestionFocus::Composer);
        self.remeasure_question(id);
        cx.notify();
    }

    /// Skip card `id`: the agent is told the user skipped (`declined`).
    fn skip_questions(&mut self, id: PermissionRequestId, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.transcript.permission_index(id) else {
            return false;
        };
        if !matches!(self.question_at(ix), Some((_, None))) || self.is_closed() {
            return false;
        }
        let Some(daemon) = Self::daemon(cx) else {
            return false;
        };
        let call = daemon
            .read(cx)
            .request::<RespondPermission>(RespondPermissionParams {
                session: self.session.clone(),
                request_id: id,
                outcome: PermissionOutcome::Declined,
                remember: false,
            });
        self.spawn_request(call, "Couldn't skip the question", cx);
        if let Some(form) = self.questions.forms.get_mut(&id) {
            form.sent = true;
        }
        self.questions.focus_next = Some(QuestionFocus::Composer);
        cx.notify();
        true
    }

    /// A question request's row.
    pub(super) fn render_question_request(
        &self,
        ix: usize,
        request: &PermissionRequest,
        outcome: Option<&PermissionOutcome>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let live = self.question_live(ix);
        let waiting = outcome.is_none() && live;
        let active = self.active_question().map(|(i, _)| i) == Some(ix);
        let form = self.questions.forms.get(&request.id);
        let focused = active && self.questions.focus.is_focused(window);
        let agent = self.provider_label();
        let (status, status_color) = match outcome {
            None if waiting => ("Waiting for your answer".to_owned(), p.sand),
            None => ("Closed before an answer".to_owned(), p.text_subtle),
            Some(PermissionOutcome::Answered { .. }) => ("Answered".to_owned(), p.sage),
            Some(PermissionOutcome::Declined) => ("Skipped".to_owned(), p.text_subtle),
            Some(_) if live => (format!("{agent} stopped waiting"), p.sand),
            Some(_) => ("Not answered".to_owned(), p.text_subtle),
        };
        let single = request.questions.len() == 1;
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(8.))
                    .bg(p.sand.hsla())
                    .text_color(p.ground.hsla())
                    .text_size(m.text_label)
                    .font_weight(FontWeight::BOLD)
                    .child(if single { "QUESTION" } else { "QUESTIONS" }),
            )
            .child(
                div()
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .child(
                        match request.questions.first().and_then(|q| q.header.clone()) {
                            Some(h) if single => format!("{agent} asks · {h}"),
                            _ => format!("{agent} asks"),
                        },
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(m.text_sm)
                    .text_color(status_color.hsla())
                    .child(status),
            );
        let mut card = div()
            .id(("chat-agent-question", ix))
            .test_support()
            .flex()
            .flex_col()
            .gap(px(12.))
            .px(px(18.))
            .py(px(16.))
            .rounded(px(18.))
            .when(live, |d| {
                d.bg(p.sand.alpha(0.06))
                    .border_1()
                    .border_color(p.sand.alpha(0.3))
            })
            .when(!live, |d| d.bg(p.surface.hsla()))
            .child(header);
        if !live {
            // Answered, skipped or gone: one line per question.
            let answers = match outcome {
                Some(PermissionOutcome::Answered { answers }) => answers.as_slice(),
                _ => &[],
            };
            for q in &request.questions {
                let answer = answers
                    .iter()
                    .find(|a| a.id == q.id)
                    .map(|a| answer_text(q, a))
                    .filter(|a| !a.is_empty());
                card = card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(q.question.clone()),
                        )
                        .child(
                            div()
                                .text_size(m.text_sm)
                                .text_color(if answer.is_some() {
                                    p.sage.hsla()
                                } else {
                                    p.text_subtle.hsla()
                                })
                                .child(answer.unwrap_or_else(|| "No answer".into())),
                        ),
                );
            }
            return card.into_any_element();
        }
        let sent = form.is_some_and(|f| f.sent);
        let cursor = form.map(|f| f.cursor);
        for (qi, q) in request.questions.iter().enumerate() {
            let picks = form
                .and_then(|f| f.picks.get(&q.id))
                .cloned()
                .unwrap_or_default();
            let mut section = div().flex().flex_col().gap(px(8.));
            if !single && let Some(h) = &q.header {
                section = section.child(
                    div()
                        .text_size(m.text_label)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.sand.hsla())
                        .child(h.to_uppercase()),
                );
            }
            section = section.child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(px(21.))
                    .child(q.question.clone()),
            );
            if q.kind == AgentQuestionKind::Multi {
                section = section.child(
                    div()
                        .text_size(m.text_sm)
                        .text_color(p.text_subtle.hsla())
                        .child("Pick any"),
                );
            }
            let mut preview = None;
            for (oi, option) in q.options.iter().enumerate() {
                let picked = picks.contains(&option.value);
                let under = focused && cursor == Some((qi, oi));
                if option.preview.is_some() && (under || (picked && preview.is_none())) {
                    preview = option.preview.clone();
                }
                let mark = if q.kind == AgentQuestionKind::Multi {
                    div()
                        .size(px(16.))
                        .mt(px(2.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.))
                        .border_1()
                        .map(|d| {
                            if picked {
                                d.bg(p.sage.hsla()).border_color(p.sage.hsla())
                            } else {
                                d.border_color(p.border_strong.hsla())
                            }
                        })
                        .when(picked, |d| {
                            d.child(icon(Icon::Check, px(12.), p.ground.hsla()))
                        })
                        .into_any_element()
                } else {
                    div()
                        .w(px(16.))
                        .flex_none()
                        .font_weight(FontWeight::BOLD)
                        .text_color(p.sand.hsla())
                        .child(if oi < 9 {
                            (oi + 1).to_string()
                        } else {
                            "·".into()
                        })
                        .into_any_element()
                };
                let (rid, value_id) = (request.id, option.value.clone());
                let button = raised(div(), cx)
                    .id(ElementId::Name(
                        format!("chat-agent-question-{ix}-{}-{value_id}", q.id).into(),
                    ))
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(12.))
                    .when(!sent, |d| d.cursor_pointer())
                    .when(picked, |d| {
                        d.border_color(p.sage.hsla()).bg(p.sage.alpha(0.10))
                    })
                    .when(under && !picked, |d| d.border_color(p.blue.hsla()))
                    .child(mark)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(option.label.clone()),
                            )
                            .children(option.description.clone().map(|d| {
                                div()
                                    .text_size(m.text_sm)
                                    .text_color(p.text_muted.hsla())
                                    .child(d)
                            })),
                    )
                    .when(picked && q.kind != AgentQuestionKind::Multi, |d| {
                        d.child(icon(Icon::Check, px(14.), p.sage.hsla()))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_answer(rid, qi, oi, cx)));
                section = section.child(button);
            }
            if let Some(preview) = preview {
                let mut lines: Vec<&str> = preview.lines().take(PREVIEW_LINES + 1).collect();
                let cut = lines.len() > PREVIEW_LINES;
                lines.truncate(PREVIEW_LINES);
                section = section.child(
                    div()
                        .id(ElementId::Name(
                            format!("chat-agent-question-{ix}-{}-preview", q.id).into(),
                        ))
                        .test_support()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(10.))
                        .bg(p.panel.hsla())
                        .font_family(m.font_mono.clone())
                        .text_size(px(12.))
                        .children(lines.into_iter().map(|l| div().child(l.to_owned())))
                        .when(cut, |d| {
                            d.child(div().text_color(p.text_subtle.hsla()).child("…"))
                        }),
                );
            }
            if let Some(input) = form.and_then(|f| f.inputs.get(&q.id)).cloned() {
                let under = focused && cursor == Some((qi, q.options.len()));
                section = section
                    .when(!q.options.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(m.text_sm)
                                .text_color(p.text_muted.hsla())
                                .child(if q.kind == AgentQuestionKind::Multi {
                                    "Or add your own"
                                } else {
                                    "Or answer in your own words"
                                }),
                        )
                    })
                    .child(
                        div()
                            .id(ElementId::Name(
                                format!("chat-agent-question-{ix}-{}-other", q.id).into(),
                            ))
                            .test_support()
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(12.))
                            .bg(p.panel.hsla())
                            .border_1()
                            .border_color(if under {
                                p.blue.hsla()
                            } else {
                                p.border.hsla()
                            })
                            .child(input),
                    );
            }
            card = card.child(section);
        }
        if let Some(error) = form.and_then(|f| f.error.clone()) {
            card = card.child(
                div()
                    .id(("chat-agent-question-error", ix))
                    .test_support()
                    .text_size(m.text_sm)
                    .text_color(p.rose.hsla())
                    .child(error),
            );
        }
        let rid = request.id;
        let hint: SharedString = if !waiting {
            format!("{agent} no longer waits; your answers go as a new message.").into()
        } else {
            "".into()
        };
        let mut footer = div().flex().flex_wrap().items_center().gap(px(10.)).child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(m.text_xs)
                .text_color(p.text_subtle.hsla())
                .child(hint),
        );
        if waiting {
            footer = footer.child(
                raised(div(), cx)
                    .id(ElementId::Name(
                        format!("chat-agent-question-{ix}-skip").into(),
                    ))
                    .test_support()
                    .px(px(14.))
                    .py(px(7.))
                    .rounded(m.radius_control)
                    .text_size(m.text_sm)
                    .text_color(p.text.hsla())
                    .when(!sent, |d| d.cursor_pointer())
                    .child("Skip")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.skip_questions(rid, cx);
                    })),
            );
        }
        footer = footer.child(
            primary(div(), cx)
                .id(ElementId::Name(
                    format!("chat-agent-question-{ix}-send").into(),
                ))
                .test_support()
                .px(px(16.))
                .py(px(7.))
                .rounded(m.radius_control)
                .text_size(m.text_sm)
                .font_weight(FontWeight::SEMIBOLD)
                .when(!sent, |d| d.cursor_pointer())
                .when(sent, |d| d.opacity(0.6))
                .child(if sent {
                    "Sending…"
                } else if waiting {
                    "Send answer"
                } else {
                    "Send as a message"
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.send_answers(rid, window, cx);
                })),
        );
        card = card.child(footer);
        if active {
            card = card
                .key_context("ChatQuestion")
                .track_focus(&self.questions.focus)
                .on_action(cx.listener(
                    |this, _: &SendAnswers, window, cx| match this.active_id() {
                        Some(id) => this.send_answers(id, window, cx),
                        None => cx.propagate(),
                    },
                ))
                .on_action(cx.listener(|this, _: &SkipQuestions, _, cx| {
                    let skipped = this
                        .active_id()
                        .is_some_and(|id| this.skip_questions(id, cx));
                    if !skipped {
                        cx.propagate();
                    }
                }))
                .on_action(cx.listener(|this, _: &NextAnswer, _, cx| {
                    if let Some(id) = this.active_id() {
                        this.step_cursor(id, true, cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &PreviousAnswer, _, cx| {
                    if let Some(id) = this.active_id() {
                        this.step_cursor(id, false, cx);
                    }
                }))
                .on_action(cx.listener(|this, _: &ToggleAnswer, _, cx| this.toggle_at_cursor(cx)))
                .on_action(cx.listener(|this, a: &PickAnswer, _, cx| {
                    if !this.pick_by_number(a.n, cx) {
                        cx.propagate();
                    }
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    // A click on the card (not its field) gives it the keys.
                    if !this
                        .questions
                        .forms
                        .values()
                        .flat_map(|f| f.inputs.values())
                        .any(|i| i.read(cx).focus_handle(cx).is_focused(window))
                    {
                        this.questions.focus_next = Some(QuestionFocus::Card);
                        cx.notify();
                    }
                }));
        }
        card.into_any_element()
    }
}
