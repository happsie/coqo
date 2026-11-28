// SPDX-License-Identifier: Apache-2.0

//! The question popover (design board 2): drawn over the code of a file tab,
//! under the anchored lines ([`CodeEditor::set_overlay`]).
//!
//! It shows the kind badge, who asked and when, whether an agent is blocked
//! on it, the question (markdown), the code it was asked about when that
//! moved or is gone (and its other anchors), and the ways to answer:
//!
//! - **decision:** one button per option (A, B, C…), and a free-text reply;
//! - **clarification:** the reply;
//! - **confirmation:** Yes / No, and an optional comment;
//! - **info:** OK;
//! - **a note** (annotation): Dismiss.
//!
//! `Enter` sends (in the reply field too; `Shift+Enter` is a new line),
//! `Esc` or **Later** closes it without answering. **Dismiss** closes the
//! question without an answer (`questions/dismiss`).
//!
//! A thread the user started (#229, `author: user`) shows the conversation
//! instead: each of the user's messages and the agent's reply to it,
//! streamed from that turn of the chat ([`ThreadReplies`]), and a field for
//! a follow-up (`questions/reply`). **Resolve** closes the thread. The
//! conversation stays scrolled to its end while a reply streams in, unless
//! the user scrolled up to read (#248).
//!
//! [`CodeEditor::set_overlay`]: coqo_ui::editor::CodeEditor::set_overlay

use coqo_protocol::methods::{
    codes,
    questions::{
        AnchorStatus, AnswerParams, Question, QuestionAnchor, QuestionId, QuestionKind, ReplyParams,
    },
    workspace::RootId,
};
use coqo_ui::keys;
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        Action, Animation, AnimationExt as _, AnyElement, App, AppContext as _, Context, Entity,
        EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
        Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _,
        pulsating_between, px,
    },
    input::{PromptInput, PromptInputEvent},
    text::markdown,
    widgets::{Icon, TestSupportExt as _, icon, popover, primary, raised},
};

use super::{
    WorkspaceQuestions, ago, answer, dismiss, kind_color, kind_label, location, reply, span_lines,
    thread::ThreadReplies,
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derives spell their paths `gpui::…`.

coqo_ui::gpui::actions!(
    question_popover,
    [
        /// Send the answer (Enter).
        SendAnswer,
        /// Close without answering (Esc).
        Later,
    ]
);

/// Open the chat of the session that asked question `id`.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = questions, no_json)]
pub struct OpenQuestionChat {
    pub id: u64,
}

const KEY_CONTEXT: &str = "QuestionPopover";
/// The tallest a thread gets before it scrolls, so the reply field and
/// the buttons stay in view.
const THREAD_MAX_HEIGHT: f32 = 240.;
/// Lines of a snippet shown at most.
const SNIPPET_LINES: usize = 6;

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        keys::binding("enter", SendAnswer, Some(KEY_CONTEXT)),
        keys::binding("escape", Later, Some(KEY_CONTEXT)),
    ]);
}

/// Emitted by a [`QuestionPopover`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopoverEvent {
    /// Close it: answered, dismissed, or put off (Later / Esc).
    Close,
}

/// One question's popover, shown in the file `root`/`path`.
pub struct QuestionPopover {
    id: QuestionId,
    store: Entity<WorkspaceQuestions>,
    root: RootId,
    path: String,
    /// The chosen option (decision, confirmation).
    selected: Option<String>,
    /// The free-text reply (decision, clarification, confirmation), or a
    /// follow-up in a user's thread.
    reply: Option<Entity<PromptInput>>,
    /// The agent's replies, in a thread the user started.
    replies: Option<Entity<ThreadReplies>>,
    /// The thread's scroll position.
    thread_scroll: ScrollHandle,
    /// Scroll the thread to its end on the next frame, even if the user
    /// scrolled up (a follow-up was sent).
    to_end: bool,
    focus: FocusHandle,
    sending: bool,
    error: Option<String>,
    _sending: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for QuestionPopover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionPopover")
            .field("id", &self.id)
            .field("selected", &self.selected)
            .field("sending", &self.sending)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<PopoverEvent> for QuestionPopover {}

impl QuestionPopover {
    pub fn new(
        id: QuestionId,
        store: Entity<WorkspaceQuestions>,
        root: RootId,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let q = store.read(cx).get(id);
        let kind = q.map(|q| q.kind);
        let thread_of = q
            .filter(|q| !q.author.is_agent())
            .map(|q| q.session.clone());
        let placeholder = match kind {
            _ if thread_of.is_some() => Some("Ask a follow-up question"),
            Some(QuestionKind::Decision) => Some("e.g. Option B, but keep the old name"),
            Some(QuestionKind::Clarification) => Some("Your answer"),
            Some(QuestionKind::Confirmation) => Some("Add a comment (optional)"),
            _ => None,
        };
        let mut subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        let reply = placeholder.map(|placeholder| {
            let rows = if thread_of.is_some() { 1 } else { 2 };
            let reply = cx.new(|cx| PromptInput::new(placeholder, rows, 6, window, cx));
            subscriptions.push(cx.subscribe_in(
                &reply,
                window,
                |this, _, event: &PromptInputEvent, window, cx| match event {
                    PromptInputEvent::Submit => this.send(window, cx),
                    PromptInputEvent::Cancel => cx.emit(PopoverEvent::Close),
                    PromptInputEvent::Changed => {
                        this.error = None;
                        cx.notify();
                    }
                    PromptInputEvent::Key(_) => {}
                },
            ));
            reply
        });
        let replies = thread_of.map(|session| {
            let replies = cx.new(|cx| ThreadReplies::follow(session, cx));
            subscriptions.push(cx.observe(&replies, |_, _, cx| cx.notify()));
            replies
        });
        Self {
            id,
            store,
            root,
            path,
            selected: None,
            reply,
            replies,
            thread_scroll: ScrollHandle::new(),
            to_end: true,
            focus: cx.focus_handle(),
            sending: false,
            error: None,
            _sending: None,
            _subscriptions: subscriptions,
        }
    }

    /// The question it shows.
    pub fn question_id(&self) -> QuestionId {
        self.id
    }

    /// The chosen option.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The reply field, if the kind takes one.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reply(&self) -> Option<&Entity<PromptInput>> {
        self.reply.as_ref()
    }

    /// Whether the keyboard is in the popover.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
            || self
                .reply
                .as_ref()
                .is_some_and(|r| r.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Put the keyboard in the popover: the reply field when the answer is
    /// text, else the popover (Enter sends, Esc closes).
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let kind = self.store.read(cx).get(self.id).map(|q| q.kind);
        match (&self.reply, kind) {
            (Some(reply), _) if self.replies.is_some() => {
                let handle = reply.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            (Some(reply), Some(QuestionKind::Decision | QuestionKind::Clarification)) => {
                let handle = reply.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
    }

    /// Choose option `id` (a click again un-chooses it).
    pub fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        } else {
            self.selected = Some(id.to_owned());
        }
        self.error = None;
        cx.notify();
    }

    fn reply_text(&self, cx: &App) -> Option<String> {
        let text = self.reply.as_ref()?.read(cx).text(cx);
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// What Send would send, or why it can't.
    fn params(&self, q: &Question, cx: &App) -> Result<AnswerParams, &'static str> {
        let text = self.reply_text(cx);
        let option = self.selected.clone();
        let params = |option, text| AnswerParams {
            id: q.id,
            option,
            text,
        };
        match q.kind {
            QuestionKind::Decision if option.is_none() && text.is_none() => {
                Err("Pick an option or write a reply.")
            }
            QuestionKind::Decision => Ok(params(option, text)),
            QuestionKind::Clarification => match text {
                Some(text) => Ok(params(None, Some(text))),
                None => Err("Write a reply first."),
            },
            QuestionKind::Confirmation => match option {
                Some(option) => Ok(params(Some(option), text)),
                None => Err("Choose Yes or No."),
            },
            QuestionKind::Info => Ok(params(Some("ok".into()), None)),
            QuestionKind::Annotation => Err("A note is dismissed, not answered."),
        }
    }

    /// The thread's scroll position.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn thread_scroll(&self) -> &ScrollHandle {
        &self.thread_scroll
    }

    /// Keep the thread at its end as it grows (a reply streaming in, a new
    /// message), unless the user scrolled up from there: as laid out last,
    /// before this frame adds to it.
    fn follow_thread(&mut self) {
        let s = &self.thread_scroll;
        let at_end = -s.offset().y >= s.max_offset().y - px(2.);
        if std::mem::take(&mut self.to_end) || at_end {
            s.scroll_to_bottom();
        }
    }

    /// The agent's replies, in a thread the user started.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn replies(&self) -> Option<&Entity<ThreadReplies>> {
        self.replies.as_ref()
    }

    /// Send the answer (a note: dismiss it; a user's thread: the follow-up).
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let Some(q) = self.store.read(cx).get(self.id).cloned() else {
            return;
        };
        if self.replies.is_some() {
            self.follow_up(window, cx);
            return;
        }
        if q.kind == QuestionKind::Annotation {
            self.dismiss(cx);
            return;
        }
        let params = match self.params(&q, cx) {
            Ok(params) => params,
            Err(why) => {
                self.error = Some(why.to_owned());
                cx.notify();
                return;
            }
        };
        let task = answer(&self.store, params, cx);
        self.sent(task, cx);
    }

    /// Close the question without an answer.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let task = dismiss(&self.store, self.id, cx);
        self.sent(task, cx);
    }

    fn sent(
        &mut self,
        task: Task<Result<Question, crate::daemon::DaemonError>>,
        cx: &mut Context<Self>,
    ) {
        self.sending = true;
        self.error = None;
        cx.notify();
        self._sending = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.sending = false;
                match result {
                    Ok(_) => cx.emit(PopoverEvent::Close),
                    // Answered elsewhere in the meantime.
                    Err(e) if e.code() == Some(codes::QUESTION_CLOSED) => {
                        cx.emit(PopoverEvent::Close)
                    }
                    Err(e) => {
                        this.error = Some(format!("Couldn't send the answer: {e}"));
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// Send the follow-up in a user's thread; the field empties once sent.
    fn follow_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(body) = self.reply_text(cx) else {
            self.error = Some("Write a follow-up first.".into());
            cx.notify();
            return;
        };
        let task = reply(&self.store, ReplyParams { id: self.id, body }, cx);
        self.sending = true;
        self.error = None;
        cx.notify();
        self._sending = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(_) => {
                        if let Some(reply) = &this.reply {
                            reply.update(cx, |r, cx| r.set_text("", window, cx));
                        }
                        this.to_end = true;
                    }
                    Err(e) => this.error = Some(format!("Couldn't send it: {e}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn later(&mut self, cx: &mut Context<Self>) {
        cx.emit(PopoverEvent::Close);
    }

    // ---- rendering ----------------------------------------------------------

    /// A user's thread: their messages and the agent's replies.
    fn render_thread(&self, q: &Question, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let replies = self.replies.as_ref().map(|r| r.read(cx));
        let mut col = div()
            .id("thread")
            .test_support()
            .px(px(18.))
            .pt(px(10.))
            .max_h(px(THREAD_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.thread_scroll)
            .flex()
            .flex_col()
            .gap(px(10.));
        let messages: Vec<(String, Option<u64>)> = if q.thread.is_empty() {
            vec![(q.body.clone(), None)]
        } else {
            q.thread
                .iter()
                .map(|m| (m.text.clone(), Some(m.turn)))
                .collect()
        };
        for (ix, (text, turn)) in messages.into_iter().enumerate() {
            col = col.child(
                div()
                    .id(("thread-message", ix))
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(m.text_label)
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.blue.hsla())
                            .child("YOU"),
                    )
                    .child(
                        div()
                            .line_height(px(21.))
                            .child(markdown(("thread-text", ix), text)),
                    ),
            );
            let reply = turn.and_then(|turn| replies.and_then(|r| r.reply(turn)));
            let (body, color): (AnyElement, _) = match reply {
                Some(r) if !r.text.trim().is_empty() => (
                    markdown(("thread-reply-text", ix), r.text.clone()).into_any_element(),
                    p.text,
                ),
                Some(r) if r.error.is_some() => (
                    SharedString::from(format!(
                        "The turn failed: {}",
                        r.error.clone().unwrap_or_default()
                    ))
                    .into_any_element(),
                    p.rose,
                ),
                Some(r) if r.done => ("No reply.".into_any_element(), p.text_subtle),
                _ => ("Thinking…".into_any_element(), p.text_subtle),
            };
            col = col.child(
                div()
                    .id(("thread-reply", ix))
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .pl(px(10.))
                    .border_l_2()
                    .border_color(p.border.hsla())
                    .child(
                        div()
                            .text_size(m.text_label)
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.text_subtle.hsla())
                            .child(self.store.read(cx).asker(&q.session, cx).to_uppercase()),
                    )
                    .child(
                        div()
                            .line_height(px(21.))
                            .text_color(color.hsla())
                            .child(body),
                    ),
            );
        }
        if let Some(e) = replies.and_then(|r| r.error()) {
            col = col.child(
                div()
                    .text_size(m.text_sm)
                    .text_color(p.rose.hsla())
                    .child(SharedString::from(e.to_owned())),
            );
        }
        col.into_any_element()
    }

    fn render_header(&self, q: &Question, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let store = self.store.read(cx);
        let meta = if q.author.is_agent() {
            format!("{} · {}", store.asker(&q.session, cx), ago(q.created_at))
        } else {
            format!(
                "You → {} · {}",
                store.asker(&q.session, cx),
                ago(q.created_at)
            )
        };
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .px(px(18.))
            .pt(px(16.))
            .child(
                div()
                    .id("question-kind")
                    .test_support()
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(8.))
                    .bg(kind_color(q, cx).hsla())
                    .text_color(p.panel.hsla())
                    .text_size(m.text_label)
                    .font_weight(FontWeight::BOLD)
                    .child(kind_label(q)),
            )
            .child(
                div()
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .child(meta),
            )
            .child(div().flex_1());
        if q.waiting {
            row = row.child(
                div()
                    .id("question-waiting")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(m.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .bg(p.sand.hsla())
                            .with_animation(
                                "question-waiting-dot",
                                Animation::new(std::time::Duration::from_millis(1400))
                                    .repeat()
                                    .with_easing(pulsating_between(0.35, 1.)),
                                |dot, delta| dot.opacity(delta),
                            ),
                    )
                    .child("Blocking the agent"),
            );
        }
        row.into_any_element()
    }

    fn render_body(&self, q: &Question, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let body = q.body.trim();
        let (title, rest) = match body.split_once('\n') {
            Some((first, rest)) => (first.trim(), rest.trim()),
            None => (body, ""),
        };
        let title = title.trim_start_matches('#').trim().to_owned();
        div()
            .id("question-body")
            .test_support()
            .px(px(18.))
            .pt(px(10.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(px(22.))
                    .child(markdown(("question-title", self.id.0 as usize), title)),
            )
            .when(!rest.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(m.text_md)
                        .text_color(p.text_muted.hsla())
                        .line_height(px(21.))
                        .child(markdown(
                            ("question-detail", self.id.0 as usize),
                            rest.to_owned(),
                        )),
                )
            })
            .into_any_element()
    }

    /// The code as asked when this file's anchor moved or is gone, and the
    /// other anchors.
    fn render_snippets(&self, q: &Question, cx: &App) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let here = |a: &&QuestionAnchor| a.anchor.root == self.root && a.anchor.path == self.path;
        // The anchor this popover is at: the first in this file.
        let shown = q.anchors.iter().find(here);
        if let Some(a) = shown {
            let notice = match a.resolution.status {
                AnchorStatus::Orphaned => {
                    Some("Code moved: it isn't here any more. This is what was asked about:")
                }
                AnchorStatus::Fuzzy { .. } => {
                    Some("Code moved: it changed since it was asked. As it was:")
                }
                AnchorStatus::Exact | AnchorStatus::Moved => None,
            };
            if let Some(notice) = notice {
                out.push(self.snippet("question-orphaned", notice.to_owned(), a, true, cx));
            }
        }
        for (ix, a) in q.anchors.iter().enumerate() {
            if shown.is_some_and(|s| std::ptr::eq(s, a)) {
                continue;
            }
            let mut label = format!("Also anchored: {}", location(a));
            if a.resolution.status == AnchorStatus::Orphaned {
                label.push_str(" (code moved)");
            }
            out.push(self.snippet(("question-anchor", ix), label, a, false, cx));
        }
        if shown.is_none()
            && let Some(first) = q.anchors.first()
            && first.resolution.status == AnchorStatus::Orphaned
            && out.is_empty()
        {
            out.push(self.snippet(
                "question-orphaned",
                "Code moved: it isn't here any more. This is what was asked about:".to_owned(),
                first,
                true,
                cx,
            ));
        }
        out
    }

    fn snippet(
        &self,
        id: impl Into<coqo_ui::gpui::ElementId>,
        label: String,
        a: &QuestionAnchor,
        notice: bool,
        cx: &App,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let first = span_lines(&a.anchor.range).start as usize;
        let text = format!("{}{}{}", a.anchor.prefix, a.anchor.text, a.anchor.suffix);
        let lines: Vec<&str> = text.lines().collect();
        let more = lines.len().saturating_sub(SNIPPET_LINES);
        let mut block = div()
            .id(id)
            .test_support()
            .mx(px(18.))
            .mt(px(12.))
            .py(px(8.))
            .rounded(px(12.))
            .bg(p.panel.hsla())
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(14.))
                    .pb(px(4.))
                    .text_size(m.text_xs)
                    .text_color(if notice {
                        p.sand.hsla()
                    } else {
                        p.text_subtle.hsla()
                    })
                    .when(notice, |d| {
                        d.child(icon(Icon::CircleAlert, px(12.), p.sand.hsla()))
                    })
                    .child(label),
            );
        for (ix, line) in lines.iter().take(SNIPPET_LINES).enumerate() {
            block = block.child(
                div()
                    .flex()
                    .font_family(m.font_mono.clone())
                    .text_size(px(13.))
                    .line_height(px(23.))
                    .whitespace_nowrap()
                    .child(
                        div()
                            .w(px(48.))
                            .flex_none()
                            .pr(px(14.))
                            .flex()
                            .justify_end()
                            .text_color(p.text_subtle.hsla())
                            .child((first + ix + 1).to_string()),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_color(p.text.hsla())
                            .child(line.replace('\t', "    ")),
                    ),
            );
        }
        if more > 0 {
            block = block.child(
                div()
                    .px(px(14.))
                    .text_size(m.text_xs)
                    .text_color(p.text_subtle.hsla())
                    .child(format!("… {more} more lines")),
            );
        }
        block.into_any_element()
    }

    fn render_options(&self, q: &Question, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        match q.kind {
            QuestionKind::Decision => {
                let mut list = div().px(px(18.)).pt(px(14.)).flex().flex_col().gap(px(8.));
                for (ix, option) in q.options.iter().enumerate() {
                    let selected = self.selected.as_deref() == Some(option.id.as_str());
                    let letter = char::from(b'A' + (ix % 26) as u8).to_string();
                    let id = option.id.clone();
                    let button = raised(div(), cx)
                        .id(("question-option", ix))
                        .test_support()
                        .flex()
                        .items_start()
                        .gap(px(12.))
                        .px(px(14.))
                        .py(px(11.))
                        .rounded(px(12.))
                        .cursor_pointer()
                        .when(selected, |d| d.border_color(p.sand.hsla()))
                        .child(
                            div()
                                .font_weight(FontWeight::BOLD)
                                .text_color(p.sand.hsla())
                                .child(letter),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
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
                        .on_click(cx.listener(move |this, _, _, cx| this.select(&id, cx)));
                    list = list.child(button);
                }
                Some(list.into_any_element())
            }
            QuestionKind::Confirmation => {
                let mut row = div().px(px(18.)).pt(px(14.)).flex().gap(px(8.));
                for (ix, option) in q.options.iter().enumerate() {
                    let selected = self.selected.as_deref() == Some(option.id.as_str());
                    let id = option.id.clone();
                    row = row.child(
                        raised(div(), cx)
                            .id(("question-option", ix))
                            .test_support()
                            .px(px(18.))
                            .py(px(8.))
                            .rounded(px(10.))
                            .cursor_pointer()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(selected, |d| d.border_color(p.sand.hsla()))
                            .child(option.label.clone())
                            .on_click(cx.listener(move |this, _, _, cx| this.select(&id, cx))),
                    );
                }
                Some(row.into_any_element())
            }
            _ => None,
        }
    }

    fn render_reply(&self, q: &Question, cx: &App) -> Option<AnyElement> {
        let reply = self.reply.clone()?;
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let label = match q.kind {
            _ if self.replies.is_some() => "",
            QuestionKind::Decision => "Or reply in your own words",
            QuestionKind::Confirmation => "Comment",
            _ => "Your answer",
        };
        Some(
            div()
                .px(px(18.))
                .pt(px(12.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .when(!label.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(m.text_sm)
                            .text_color(p.text_muted.hsla())
                            .child(label),
                    )
                })
                .child(
                    div()
                        .id("question-reply")
                        .test_support()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(12.))
                        .bg(p.panel.hsla())
                        .border_1()
                        .border_color(p.border.hsla())
                        .child(reply),
                )
                .into_any_element(),
        )
    }

    fn render_footer(&self, q: &Question, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let thread = self.replies.is_some();
        let valid = if thread {
            self.reply_text(cx).is_some()
        } else {
            q.kind == QuestionKind::Annotation || self.params(q, cx).is_ok()
        };
        let send_label = match q.kind {
            _ if thread => "Send",
            QuestionKind::Info => "OK",
            QuestionKind::Annotation => "Dismiss",
            _ => "Send answer",
        };
        let link = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .test_support()
                .text_size(m.text_sm)
                .text_color(p.blue.hsla())
                .cursor_pointer()
                .child(label)
        };
        let id = self.id.0;
        div()
            .px(px(18.))
            .pt(px(12.))
            .pb(px(16.))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(
                link("question-open-chat", "Open in chat").on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(OpenQuestionChat { id }), cx)
                }),
            )
            .when(q.kind.is_question(), |d| {
                d.child(
                    link(
                        "question-dismiss",
                        if thread { "Resolve" } else { "Dismiss" },
                    )
                    .text_color(p.text_subtle.hsla())
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
                )
            })
            .when(!thread, |d| {
                d.child(
                    div()
                        .text_size(m.text_sm)
                        .text_color(p.text_subtle.hsla())
                        .child("F8 next question"),
                )
            })
            .child(div().flex_1())
            .child(
                raised(div(), cx)
                    .id("question-later")
                    .test_support()
                    .px(px(14.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .text_size(m.text_sm)
                    .cursor_pointer()
                    .child("Later")
                    .on_click(cx.listener(|this, _, _, cx| this.later(cx))),
            )
            .child(
                primary(div(), cx)
                    .id("question-send")
                    .test_support()
                    .px(px(18.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .text_size(m.text_sm)
                    .cursor_pointer()
                    .when(!valid || self.sending, |d| d.opacity(0.55))
                    .child(if self.sending {
                        "Sending…"
                    } else {
                        send_label
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
            .into_any_element()
    }
}

/// Dark text on a pastel badge, in both appearances (board 2: #2A2012).
impl Focusable for QuestionPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for QuestionPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let root = popover(cx)
            .id("question-popover")
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SendAnswer, window, cx| this.send(window, cx)))
            .on_action(cx.listener(|this, _: &Later, _, cx| this.later(cx)))
            .rounded(px(18.))
            .w_full()
            .font_family(m.font_ui.clone())
            .text_size(m.text_md)
            .text_color(p.text.hsla());
        let Some(q) = self.store.read(cx).get(self.id).cloned() else {
            return root
                .p(px(18.))
                .text_color(p.text_muted.hsla())
                .child("This question is gone.");
        };
        let header = self.render_header(&q, cx);
        let body = if self.replies.is_some() {
            self.follow_thread();
            self.render_thread(&q, cx)
        } else {
            self.render_body(&q, cx)
        };
        let snippets = self.render_snippets(&q, cx);
        let options = self.render_options(&q, cx);
        let reply = self.render_reply(&q, cx);
        let error = self.error.clone().map(|e| {
            div()
                .id("question-error")
                .test_support()
                .px(px(18.))
                .pt(px(10.))
                .text_size(m.text_sm)
                .text_color(p.rose.hsla())
                .child(SharedString::from(e))
        });
        let footer = self.render_footer(&q, cx);
        root.child(header)
            .child(body)
            .children(snippets)
            .children(options)
            .children(reply)
            .children(error)
            .child(footer)
    }
}
