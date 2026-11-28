// SPDX-License-Identifier: Apache-2.0

//! The step popover (design board 3): drawn at a step's code, under its
//! lines in the editor ([`FileEditor::show_step`]) or in a slot under them
//! in the diff ([`super::diff::StepOverlay`]).
//!
//! Top to bottom: the kind badge, "Step 3 of 8" and the anchor ("anchor 1
//! of 3" with ‹ › when the step has several), the title, **What it does**
//! and **Why it's done this way** side by side (markdown), the risks with
//! their severity badges, the acceptance criteria the step meets and the
//! steps to read first, the quoted code when the anchor moved or is gone,
//! and the buttons: ← Previous, Ask about this step, Mark understood /
//! ✓ Understood, Next step →. Asking opens a field under the title;
//! `Enter` sends the question, `Esc` closes the field. In an editor short of
//! room the content between the title and the buttons scrolls, so the
//! buttons stay in sight.
//!
//! It holds a copy of what it shows ([`StepData`]) and reports what the
//! user does ([`StepEvent`]); the player does it.
//!
//! **In a file tab** (#260, [`StepPopover::in_file_tab`]) it shows a step of
//! a walkthrough the player is not showing: the header names the
//! walkthrough, and the buttons are **Open in player** (the player at this
//! step), **Close** (`Esc`) and **Mark understood** (`u`). There is no
//! Previous / Next and no asking: those are the player's.
//!
//! [`FileEditor::show_step`]: crate::files::editor::FileEditor::show_step

use coqo_protocol::methods::{
    agents::CriterionStatus,
    questions::{AnchorStatus, QuestionId},
    walkthroughs::{Step, StepState},
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
        FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div,
        prelude::FluentBuilder as _, px,
    },
    input::{PromptInput, PromptInputEvent},
    text::markdown,
    widgets::{Icon, TestSupportExt as _, icon, popover, primary, raised},
};

use super::{anchor_location, anchor_status, kind_label, severity_color, severity_label};
use crate::{files::editor::FileEditor, questions::OpenQuestion};
use coqo_protocol::methods::walkthroughs::Walkthrough;
use coqo_ui::keys;

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    step_popover,
    [
        /// Close the step popover of a file tab (Esc).
        CloseStep,
        /// Mark its step understood, or unread again (u).
        ToggleStepUnderstood,
    ]
);

/// Where the keys of a file tab's step popover apply.
const KEY_CONTEXT: &str = "StepPopover";

/// Bind the keys of a file tab's step popover.
pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        keys::binding("escape", CloseStep, Some(KEY_CONTEXT)),
        keys::binding("u", ToggleStepUnderstood, Some(KEY_CONTEXT)),
    ]);
}

/// Lines of quoted code shown at most.
const SNIPPET_LINES: usize = 8;
/// The popover's height around its scrolling content: padding, header,
/// title, buttons and the gaps between them.
const CHROME: f32 = 172.;
/// …and the question field's.
const QUESTION: f32 = 120.;
/// The least height of the scrolling content.
const MIN_BODY: f32 = 90.;

/// A criterion a step meets, as the popover shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct CriterionRef {
    pub id: String,
    pub text: String,
    pub status: Option<CriterionStatus>,
}

/// What the popover shows.
#[derive(Clone, Debug, PartialEq)]
pub struct StepData {
    /// The step's index, and how many there are.
    pub ix: usize,
    pub total: usize,
    pub step: Step,
    pub state: StepState,
    /// The anchor shown.
    pub anchor: usize,
    pub criteria: Vec<CriterionRef>,
    /// Steps to read first: index and title.
    pub depends: Vec<(usize, String)>,
    /// Who wrote it: "Claude".
    pub agent: String,
    /// The question asked about this step here, if any.
    pub asked: Option<QuestionId>,
    /// The walkthrough's title, shown in a file tab (#260).
    pub walkthrough: String,
}

/// What the popover shows for step `ix` of `w` at its anchor `anchor`.
pub fn step_data(
    w: &Walkthrough,
    ix: usize,
    anchor: usize,
    agent: String,
    asked: Option<QuestionId>,
) -> Option<StepData> {
    let step = w.steps.get(ix)?;
    let criteria = step
        .criteria_refs
        .iter()
        .map(|id| {
            let known = w.acceptance_criteria.iter().find(|c| &c.id == id);
            CriterionRef {
                id: id.clone(),
                text: known.map(|c| c.text.clone()).unwrap_or_default(),
                status: known.and_then(|c| c.status),
            }
        })
        .collect();
    let depends = step
        .depends_on
        .iter()
        .filter_map(|id| {
            let ix = w.steps.iter().position(|s| &s.id == id)?;
            Some((ix, w.steps[ix].title.clone()))
        })
        .collect();
    Some(StepData {
        ix,
        total: w.steps.len(),
        step: step.clone(),
        state: super::step_state(w, step),
        anchor: anchor.min(step.anchors.len().saturating_sub(1)),
        criteria,
        depends,
        agent,
        asked,
        walkthrough: w.title.clone(),
    })
}

/// What the user did in the popover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepEvent {
    Previous,
    Next,
    ToggleUnderstood,
    /// Go to step `ix`.
    GoTo(usize),
    /// Show the next (`true`) or previous anchor.
    Anchor(bool),
    /// Send this question about the step to the agent.
    Ask(String),
    /// Show the step in the diff.
    ViewDiff,
    /// Open the walkthrough's player at this step (in a file tab).
    OpenInPlayer,
    /// Close the popover (in a file tab).
    Close,
}

/// The popover. See the module docs.
pub struct StepPopover {
    data: Option<StepData>,
    /// The question field, while asking.
    question: Option<Entity<PromptInput>>,
    sending: bool,
    error: Option<String>,
    focus: FocusHandle,
    /// The file tab it is shown in, for the room it has there.
    host: Option<WeakEntity<FileEditor>>,
    /// Shown in an ordinary file tab, not the player (#260).
    in_file_tab: bool,
    _question: Option<Subscription>,
}

impl std::fmt::Debug for StepPopover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepPopover")
            .field("step", &self.data.as_ref().map(|d| d.ix))
            .field("asking", &self.question.is_some())
            .finish_non_exhaustive()
    }
}

impl EventEmitter<StepEvent> for StepPopover {}

impl StepPopover {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            data: None,
            question: None,
            sending: false,
            error: None,
            focus: cx.focus_handle(),
            host: None,
            in_file_tab: false,
            _question: None,
        }
    }

    /// A popover for an ordinary file tab (see the module docs).
    pub fn in_file_tab(host: WeakEntity<FileEditor>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(cx);
        this.host = Some(host);
        this.in_file_tab = true;
        this
    }

    /// The file tab it is shown in (`None` in the diff).
    pub fn set_host(&mut self, host: Option<WeakEntity<FileEditor>>, cx: &mut Context<Self>) {
        self.host = host;
        cx.notify();
    }

    /// Show `data`. Another step closes the question field.
    pub fn set(&mut self, data: StepData, cx: &mut Context<Self>) {
        let other = self.data.as_ref().is_none_or(|d| d.step.id != data.step.id);
        if other {
            self.question = None;
            self._question = None;
            self.sending = false;
            self.error = None;
        }
        if self.data.as_ref() != Some(&data) {
            self.data = Some(data);
            cx.notify();
        }
    }

    /// What it shows.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn data(&self) -> Option<&StepData> {
        self.data.as_ref()
    }

    /// The question field, while asking.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn question(&self) -> Option<&Entity<PromptInput>> {
        self.question.as_ref()
    }

    /// Whether the keyboard is in the popover.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
            || self
                .question
                .as_ref()
                .is_some_and(|q| q.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Open the question field, with the keyboard in it.
    pub fn ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.question.is_none() {
            let agent = self
                .data
                .as_ref()
                .map_or_else(|| "the agent".to_owned(), |d| d.agent.clone());
            let field = cx.new(|cx| {
                PromptInput::new(format!("Ask {agent} about this step"), 2, 6, window, cx)
            });
            self._question = Some(cx.subscribe_in(
                &field,
                window,
                |this, _, event: &PromptInputEvent, window, cx| match event {
                    PromptInputEvent::Submit => this.send(cx),
                    PromptInputEvent::Cancel => this.cancel_question(window, cx),
                    PromptInputEvent::Changed => {
                        this.error = None;
                        cx.notify();
                    }
                    PromptInputEvent::Key(_) => {}
                },
            ));
            self.question = Some(field);
        }
        if let Some(field) = &self.question {
            let handle = field.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn cancel_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.question = None;
        self._question = None;
        self.error = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Send the question in the field.
    pub fn send(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self
            .question
            .as_ref()
            .map(|q| q.read(cx).text(cx))
            .filter(|t| !t.trim().is_empty())
        else {
            self.error = Some("Write the question first".to_owned());
            cx.notify();
            return;
        };
        if self.sending {
            return;
        }
        self.sending = true;
        self.error = None;
        cx.emit(StepEvent::Ask(text));
        cx.notify();
    }

    /// How sending went: the field closes once it is sent.
    pub fn sent(
        &mut self,
        result: Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sending = false;
        match result {
            Ok(()) => {
                self.question = None;
                self._question = None;
                window.focus(&self.focus, cx);
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }

    fn render_header(&self, d: &StepData, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let anchors = d.step.anchors.len();
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .id("step-kind")
                    .test_support()
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(8.))
                    .bg(p.sage.hsla())
                    .text_color(p.panel.hsla())
                    .text_size(m.text_label)
                    .font_weight(FontWeight::BOLD)
                    .child(SharedString::from(kind_label(d.step.kind).to_uppercase())),
            )
            .child(
                div()
                    .id("step-of")
                    .test_support()
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .child(if self.in_file_tab {
                        format!("{} · Step {} of {}", d.walkthrough, d.ix + 1, d.total)
                    } else {
                        format!("Step {} of {}", d.ix + 1, d.total)
                    }),
            )
            .child(div().flex_1());
        if let Some(a) = d.step.anchors.get(d.anchor) {
            row = row.child(
                div()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(8.))
                    .bg(p.panel.hsla())
                    .font_family(m.font_mono.clone())
                    .text_size(m.text_xs)
                    .text_color(p.text_muted.hsla())
                    .child(anchor_location(a)),
            );
        }
        if anchors > 1 && !self.in_file_tab {
            let arrow = |id: &'static str, glyph: &'static str| {
                div()
                    .id(id)
                    .test_support()
                    .px(px(6.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(p.text_muted.hsla())
                    .hover(|d| d.bg(p.surface_strong.hsla()))
                    .child(glyph)
            };
            row = row
                .child(
                    arrow("step-anchor-previous", "‹")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::Anchor(false)))),
                )
                .child(
                    div()
                        .id("step-anchor-count")
                        .test_support()
                        .text_size(m.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .child(format!("anchor {} of {anchors}", d.anchor + 1)),
                )
                .child(
                    arrow("step-anchor-next", "›")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::Anchor(true)))),
                );
        }
        row.into_any_element()
    }

    fn column(
        id: &'static str,
        label: &'static str,
        text: &str,
        ix: usize,
        cx: &App,
    ) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        div()
            .id(id)
            .test_support()
            .flex_1()
            .min_w(px(240.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .text_size(m.text_label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_subtle.hsla())
                    .child(label),
            )
            .child(
                div()
                    .line_height(px(21.))
                    .child(markdown((id, ix), text.trim().to_owned())),
            )
    }

    fn render_risks(&self, d: &StepData, cx: &App) -> Option<AnyElement> {
        if d.step.risks.is_empty() {
            return None;
        }
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let mut list = div()
            .id("step-risks")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .text_size(m.text_label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_subtle.hsla())
                    .child("RISKS"),
            );
        for (ix, risk) in d.step.risks.iter().enumerate() {
            let color = severity_color(risk.severity, cx).hsla();
            list = list.child(
                div()
                    .id(("step-risk", ix))
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(12.))
                    .bg(p.panel_raised.hsla())
                    .child(
                        div()
                            .flex_none()
                            .px(px(8.))
                            .py(px(3.))
                            .rounded(px(7.))
                            .bg(color.opacity(0.22))
                            .text_color(color)
                            .text_size(px(10.5))
                            .font_weight(FontWeight::BOLD)
                            .child(severity_label(risk.severity)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .line_height(px(20.))
                            .child(SharedString::from(risk.text.clone())),
                    ),
            );
        }
        Some(list.into_any_element())
    }

    fn render_links(&self, d: &StepData, cx: &mut Context<Self>) -> Option<AnyElement> {
        if d.criteria.is_empty() && d.depends.is_empty() {
            return None;
        }
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let mut col = div().flex().flex_col().gap(px(6.)).text_size(m.text_sm);
        if !d.criteria.is_empty() {
            let mut row = div()
                .id("step-criteria")
                .test_support()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(m.text_label)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text_subtle.hsla())
                        .child("MEETS"),
                );
            for c in &d.criteria {
                let (glyph, color) = match c.status {
                    Some(CriterionStatus::Met) => (Icon::Check, p.sage.hsla()),
                    Some(CriterionStatus::Failed) => (Icon::CircleX, p.rose.hsla()),
                    Some(CriterionStatus::Pending) => (Icon::Circle, p.sand.hsla()),
                    Some(CriterionStatus::Unknown) | None => {
                        (Icon::CircleAlert, p.text_subtle.hsla())
                    }
                };
                row = row.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .min_w_0()
                        .child(icon(glyph, px(12.), color))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.blue.hsla())
                                .child(SharedString::from(c.id.clone())),
                        )
                        .child(
                            div()
                                .text_color(p.text_muted.hsla())
                                .child(SharedString::from(c.text.clone())),
                        ),
                );
            }
            col = col.child(row);
        }
        if !d.depends.is_empty() {
            let mut row = div()
                .id("step-depends")
                .test_support()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(m.text_label)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text_subtle.hsla())
                        .child("READ FIRST"),
                );
            for (ix, title) in &d.depends {
                let ix = *ix;
                row = row.child(
                    div()
                        .id(("step-depends-on", ix))
                        .test_support()
                        .text_color(p.blue.hsla())
                        .cursor_pointer()
                        .child(format!("Step {} · {title}", ix + 1))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(StepEvent::GoTo(ix)))),
                );
            }
            col = col.child(row);
        }
        Some(col.into_any_element())
    }

    /// The quoted code when the anchor moved or is gone.
    fn render_snippet(&self, d: &StepData, cx: &mut Context<Self>) -> Option<AnyElement> {
        let a = d.step.anchors.get(d.anchor)?;
        let notice = match anchor_status(a) {
            AnchorStatus::Orphaned => {
                "Code moved: it isn't here any more. This is what the step is about:"
            }
            AnchorStatus::Fuzzy { .. } => {
                "Code moved: it changed since the walkthrough was written. As it was:"
            }
            AnchorStatus::Exact | AnchorStatus::Moved => return None,
        };
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let first = crate::questions::span_lines(&a.anchor.range).start as usize;
        let text = format!("{}{}{}", a.anchor.prefix, a.anchor.text, a.anchor.suffix);
        let lines: Vec<&str> = text.lines().collect();
        let more = lines.len().saturating_sub(SNIPPET_LINES);
        let mut block = div()
            .id("step-moved")
            .test_support()
            .py(px(8.))
            .rounded(px(12.))
            .bg(p.panel.hsla())
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .px(px(14.))
                    .pb(px(4.))
                    .text_size(m.text_xs)
                    .text_color(p.sand.hsla())
                    .child(icon(Icon::CircleAlert, px(12.), p.sand.hsla()))
                    .child(notice)
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("step-moved-diff")
                            .test_support()
                            .text_color(p.blue.hsla())
                            .cursor_pointer()
                            .child("View as diff")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::ViewDiff))),
                    ),
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
        Some(block.into_any_element())
    }

    fn render_question(&self, d: &StepData, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        if let Some(field) = &self.question {
            return Some(
                div()
                    .id("step-question")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(12.))
                            .bg(p.panel_raised.hsla())
                            .border_1()
                            .border_color(p.border.hsla())
                            .child(field.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .text_size(m.text_sm)
                            .child(div().text_color(p.text_subtle.hsla()).child(format!(
                                "Goes to {} with the step's code attached",
                                d.agent
                            )))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("step-question-cancel")
                                    .test_support()
                                    .text_color(p.text_subtle.hsla())
                                    .cursor_pointer()
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_question(window, cx)
                                    })),
                            )
                            .child(
                                primary(div(), cx)
                                    .id("step-question-send")
                                    .test_support()
                                    .px(px(14.))
                                    .py(px(6.))
                                    .rounded(px(10.))
                                    .cursor_pointer()
                                    .when(self.sending, |d| d.opacity(0.55))
                                    .child(if self.sending { "Sending…" } else { "Ask" })
                                    .on_click(cx.listener(|this, _, _, cx| this.send(cx))),
                            ),
                    )
                    .when_some(self.error.clone(), |d, e| {
                        d.child(
                            div()
                                .id("step-question-error")
                                .test_support()
                                .text_size(m.text_sm)
                                .text_color(p.rose.hsla())
                                .child(SharedString::from(e)),
                        )
                    })
                    .into_any_element(),
            );
        }
        let id = d.asked?;
        Some(
            div()
                .id("step-asked")
                .test_support()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(m.text_sm)
                .text_color(p.text_muted.hsla())
                .child(icon(Icon::MessageCircle, px(13.), p.blue.hsla()))
                .child(format!("You asked {} about this step.", d.agent))
                .child(
                    div()
                        .id("step-asked-open")
                        .test_support()
                        .text_color(p.blue.hsla())
                        .cursor_pointer()
                        .child("Open the thread")
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(Box::new(OpenQuestion { id: id.0 }), cx)
                        }),
                )
                .into_any_element(),
        )
    }

    /// A file tab's buttons: Open in player, Close, Mark understood.
    fn render_file_tab_footer(&self, d: &StepData, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let m = t.metrics.clone();
        let p = t.palette.clone();
        let understood = d.state == StepState::Understood;
        let button = |id: &'static str, cx: &App| {
            raised(div(), cx)
                .id(id)
                .test_support()
                .px(px(14.))
                .py(px(8.))
                .rounded(px(10.))
                .text_size(m.text_sm)
                .cursor_pointer()
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(
                button("step-open-player", cx)
                    .child("Open in player")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::OpenInPlayer))),
            )
            .child(
                div()
                    .id("step-close")
                    .test_support()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .cursor_pointer()
                    .hover(|d| d.bg(p.surface_strong.hsla()))
                    .child("Close")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::Close))),
            )
            .child(div().flex_1())
            .child(
                primary(div(), cx)
                    .id("step-understood")
                    .test_support()
                    .px(px(18.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .text_size(m.text_sm)
                    .cursor_pointer()
                    .child(if understood {
                        "✓ Understood"
                    } else {
                        "Mark understood"
                    })
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::ToggleUnderstood))),
            )
            .into_any_element()
    }

    fn render_footer(&self, d: &StepData, cx: &mut Context<Self>) -> AnyElement {
        if self.in_file_tab {
            return self.render_file_tab_footer(d, cx);
        }
        let t = cx.tokens();
        let m = t.metrics.clone();
        let p = t.palette.clone();
        let at_start = d.ix == 0;
        let at_end = d.ix + 1 >= d.total;
        let understood = d.state == StepState::Understood;
        let button = |id: &'static str, cx: &App| {
            raised(div(), cx)
                .id(id)
                .test_support()
                .px(px(14.))
                .py(px(8.))
                .rounded(px(10.))
                .text_size(m.text_sm)
                .cursor_pointer()
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(10.))
            .child(
                button("step-previous", cx)
                    .when(at_start, |d| d.opacity(0.45))
                    .child("← Previous")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::Previous))),
            )
            .child(
                button("step-ask", cx)
                    .border_color(p.sand.hsla().opacity(0.45))
                    .text_color(p.sand.hsla())
                    .child("Ask about this step")
                    .on_click(cx.listener(|this, _, window, cx| this.ask(window, cx))),
            )
            .child(div().flex_1())
            .child(
                button("step-understood", cx)
                    .when(understood, |d| d.text_color(p.sage.hsla()))
                    .child(if understood {
                        "✓ Understood"
                    } else {
                        "Mark understood"
                    })
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::ToggleUnderstood))),
            )
            .child(
                primary(div(), cx)
                    .id("step-next")
                    .test_support()
                    .px(px(18.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .text_size(m.text_sm)
                    .cursor_pointer()
                    .when(at_end, |d| d.opacity(0.45))
                    .child("Next step →")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(StepEvent::Next))),
            )
            .into_any_element()
    }
}

impl Focusable for StepPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for StepPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let root = popover(cx)
            .id("step-popover")
            .test_support()
            .track_focus(&self.focus)
            .when(self.in_file_tab, |d| {
                d.key_context(KEY_CONTEXT)
                    .on_action(cx.listener(|_, _: &CloseStep, _, cx| cx.emit(StepEvent::Close)))
                    .on_action(cx.listener(|_, _: &ToggleStepUnderstood, _, cx| {
                        cx.emit(StepEvent::ToggleUnderstood)
                    }))
            })
            .rounded(px(18.))
            .w_full()
            .px(px(20.))
            .py(px(18.))
            .gap(px(14.))
            .font_family(m.font_ui.clone())
            .text_size(m.text_md)
            .text_color(p.text.hsla());
        let Some(d) = self.data.clone() else {
            return root
                .text_color(p.text_muted.hsla())
                .child("Loading the walkthrough…");
        };
        let header = self.render_header(&d, cx);
        let title = div()
            .id("step-title")
            .test_support()
            .text_size(px(16.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(SharedString::from(d.step.title.clone()));
        let ix = d.ix;
        let columns = div()
            .flex()
            .flex_wrap()
            .gap(px(18.))
            .child(Self::column(
                "step-what",
                "WHAT IT DOES",
                &d.step.what,
                ix,
                cx,
            ))
            .child(Self::column(
                "step-why",
                "WHY IT'S DONE THIS WAY",
                &d.step.why,
                ix,
                cx,
            ));
        let snippet = self.render_snippet(&d, cx);
        let risks = self.render_risks(&d, cx);
        let links = self.render_links(&d, cx);
        let question = self.render_question(&d, cx);
        let footer = self.render_footer(&d, cx);
        // In an editor short of room, the content scrolls and the buttons
        // stay in sight.
        let chrome = if self.question.is_some() {
            CHROME + QUESTION
        } else {
            CHROME
        };
        let room = self
            .host
            .as_ref()
            .and_then(|h| h.upgrade())
            .and_then(|f| f.read(cx).editor().cloned())
            .and_then(|code| code.read(cx).overlay_room())
            .map(|room| (room - px(chrome)).max(px(MIN_BODY)));
        let body = div()
            .id("step-body")
            .flex()
            .flex_col()
            .gap(px(14.))
            .when_some(room, |d, room| d.max_h(room).overflow_y_scroll())
            .children(snippet)
            .child(columns)
            .children(risks)
            .children(links);
        // The question field goes first: it is what the user is doing.
        root.child(header)
            .child(title)
            .children(question)
            .child(body)
            .child(footer)
    }
}
