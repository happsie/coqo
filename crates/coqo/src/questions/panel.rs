// SPDX-License-Identifier: Apache-2.0

//! The Questions sidebar pane (design board 2, right column): the window's
//! workspace's open questions and notes, grouped by file, and the answered
//! history.
//!
//! A card shows the kind (and "BLOCKING" while an agent waits), how long ago
//! it was asked, the question's first line and where it is pinned. Clicking
//! a card shows it in its file with the popover open ([`OpenQuestion`]). A
//! confirmation can be answered right on its card (Yes / No).
//!
//! Threads the user started (#229) are listed too; a filter shows all, the
//! agents' questions only, or the user's threads only.

use coqo_protocol::methods::questions::{AnswerParams, Question, QuestionKind, QuestionStatus};
use coqo_ui::{
    ActiveTokens as _,
    dock::Dock,
    gpui::{
        AnyElement, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription,
        WeakEntity, Window, div, prelude::FluentBuilder as _, px,
    },
    widgets::{TestSupportExt as _, raised, tab},
};

use super::{
    OpenQuestion, WorkspaceQuestions, ago_short, anchor_line, answer, answer_text, kind_color,
    kind_label, location, primary_anchor, questions_for,
};
use crate::workspace::{Workspaces, WorkspacesEvent};

/// Which list the pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Showing {
    Open,
    Answered,
}

/// Whose questions the pane lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    /// Asked by agents.
    Agents,
    /// Threads the user started.
    Yours,
}

impl Filter {
    fn keeps(self, q: &Question) -> bool {
        match self {
            Filter::All => true,
            Filter::Agents => q.author.is_agent(),
            Filter::Yours => !q.author.is_agent(),
        }
    }
}

/// The Questions pane.
pub struct QuestionsPane {
    workspaces: Option<Entity<Workspaces>>,
    store: Option<Entity<WorkspaceQuestions>>,
    showing: Showing,
    filter: Filter,
    _store: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for QuestionsPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionsPane")
            .field("showing", &self.showing)
            .finish_non_exhaustive()
    }
}

impl QuestionsPane {
    pub fn new(_dock: WeakEntity<Dock>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspaces = Workspaces::for_window(window, cx);
        let mut subscriptions = Vec::new();
        if let Some(workspaces) = &workspaces {
            subscriptions.push(cx.subscribe(workspaces, |this, _, event, cx| {
                if *event == WorkspacesEvent::CurrentChanged {
                    this.follow_current(cx);
                }
            }));
        }
        let mut this = Self {
            workspaces,
            store: None,
            showing: Showing::Open,
            filter: Filter::All,
            _store: None,
            _subscriptions: subscriptions,
        };
        this.follow_current(cx);
        this
    }

    fn follow_current(&mut self, cx: &mut Context<Self>) {
        let current = self
            .workspaces
            .as_ref()
            .and_then(|w| w.read(cx).current().map(|c| c.id.clone()));
        self.store = current.and_then(|id| questions_for(&id, cx));
        self._store = self
            .store
            .as_ref()
            .map(|store| cx.observe(store, |_, _, cx| cx.notify()));
        cx.notify();
    }

    /// Which list is shown.
    #[allow(dead_code)]
    pub fn showing(&self) -> Showing {
        self.showing
    }

    /// Show the open questions or the answered ones.
    pub fn show(&mut self, showing: Showing, cx: &mut Context<Self>) {
        self.showing = showing;
        cx.notify();
    }

    /// List only these.
    pub fn filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    fn render_card(&self, q: &Question, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let blocking = q.waiting && q.status == QuestionStatus::Open;
        let mut kind = kind_label(q).to_owned();
        if blocking {
            kind.push_str(" · BLOCKING");
        }
        let title = q
            .body
            .trim()
            .lines()
            .next()
            .unwrap_or_default()
            .trim_start_matches('#')
            .trim()
            .to_owned();
        let mut place = primary_anchor(q).map(location).unwrap_or_default();
        if q.anchors.len() > 1 {
            let more = q.anchors.len() - 1;
            place.push_str(&format!(
                " · +{more} anchor{}",
                if more > 1 { "s" } else { "" }
            ));
        }
        let when = match q.closed_at {
            Some(closed) if q.status != QuestionStatus::Open => ago_short(closed),
            _ => ago_short(q.created_at),
        };
        let id = q.id.0;
        let color = kind_color(q, cx);
        let mut card = div()
            .id(("question-card", id as usize))
            .test_support()
            .px(px(14.))
            .py(px(12.))
            .rounded(px(14.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .cursor_pointer()
            .bg(if blocking {
                p.sand.alpha(0.08)
            } else {
                p.surface.hsla()
            })
            .when(blocking, |d| d.border_1().border_color(p.sand.alpha(0.3)))
            .on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(OpenQuestion { id }), cx)
            })
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(m.text_label)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(div().text_color(color.hsla()).child(kind))
                    .child(div().text_color(p.text_subtle.hsla()).child(when)),
            )
            .child(div().text_size(m.text_md).line_height(px(19.)).child(title))
            .child(
                div()
                    .font_family(m.font_mono.clone())
                    .text_size(m.text_xs)
                    .text_color(p.text_muted.hsla())
                    .child(place),
            );
        if q.status != QuestionStatus::Open {
            let outcome = match q.status {
                QuestionStatus::Dismissed => "Dismissed".to_owned(),
                _ => format!("Answered: {}", answer_text(q).unwrap_or_default()),
            };
            card = card.child(
                div()
                    .text_size(m.text_sm)
                    .text_color(p.text_muted.hsla())
                    .child(outcome),
            );
        } else if q.kind == QuestionKind::Confirmation {
            let mut row = div().flex().gap(px(6.)).mt(px(4.));
            for option in &q.options {
                let option_id = option.id.clone();
                let qid = q.id;
                row = row.child(
                    raised(div(), cx)
                        .id(("question-card-option", id as usize * 4 + row_ix(&option.id)))
                        .test_support()
                        .px(px(12.))
                        .py(px(5.))
                        .rounded(px(9.))
                        .text_size(m.text_sm)
                        .cursor_pointer()
                        .child(option.label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(store) = this.store.clone() {
                                answer(
                                    &store,
                                    AnswerParams {
                                        id: qid,
                                        option: Some(option_id.clone()),
                                        text: None,
                                    },
                                    cx,
                                )
                                .detach();
                            }
                        })),
                );
            }
            card = card.child(row);
        }
        card.into_any_element()
    }
}

/// A stable index for a confirmation's option ids.
fn row_ix(option: &str) -> usize {
    match option {
        "yes" => 0,
        "no" => 1,
        _ => 2,
    }
}

impl Render for QuestionsPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let filter = self.filter;
        let (open, closed): (Vec<Question>, Vec<Question>) = match &self.store {
            Some(store) => {
                let store = store.read(cx);
                // By file, then line.
                let mut open: Vec<Question> = store
                    .open()
                    .into_iter()
                    .filter(|q| filter.keeps(q))
                    .cloned()
                    .collect();
                open.sort_by_cached_key(|q| {
                    let a = primary_anchor(q);
                    (
                        a.map_or(0, |a| a.anchor.root.0),
                        a.map(|a| a.anchor.path.clone()).unwrap_or_default(),
                        a.map_or(0, anchor_line),
                        q.id,
                    )
                });
                let closed = store
                    .closed()
                    .into_iter()
                    .filter(|q| filter.keeps(q))
                    .cloned()
                    .collect();
                (open, closed)
            }
            None => (Vec::new(), Vec::new()),
        };
        let showing = self.showing;
        let tabs = div()
            .flex()
            .gap(px(6.))
            .child(
                tab(
                    "questions-open",
                    format!("Open {}", open.len()),
                    showing == Showing::Open,
                    cx,
                )
                .test_support()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.show(Showing::Open, cx))),
            )
            .child(
                tab(
                    "questions-answered",
                    format!("Answered {}", closed.len()),
                    showing == Showing::Answered,
                    cx,
                )
                .test_support()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.show(Showing::Answered, cx))),
            );
        let mut filters = div().flex().gap(px(4.)).px(px(4.));
        for (id, label, f) in [
            ("questions-filter-all", "All", Filter::All),
            ("questions-filter-agents", "Agents", Filter::Agents),
            ("questions-filter-yours", "Yours", Filter::Yours),
        ] {
            filters = filters.child(
                div()
                    .id(id)
                    .test_support()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(999.))
                    .text_size(m.text_xs)
                    .cursor_pointer()
                    .map(|d| {
                        if filter == f {
                            d.bg(p.surface.hsla()).text_color(p.text.hsla())
                        } else {
                            d.text_color(p.text_subtle.hsla())
                        }
                    })
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.filter(f, cx))),
            );
        }

        let mut list = div().flex().flex_col().gap(px(8.));
        let shown = match showing {
            Showing::Open => &open,
            Showing::Answered => &closed,
        };
        if shown.is_empty() {
            list = list.child(
                div()
                    .id("questions-empty")
                    .test_support()
                    .px_2()
                    .py_1()
                    .text_size(m.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child(match showing {
                        Showing::Open => "No open questions. When an agent asks about your code, it shows here and in the file.",
                        Showing::Answered => "Nothing answered yet.",
                    }),
            );
        }
        let mut last_file: Option<String> = None;
        for q in shown {
            if showing == Showing::Open {
                let file = primary_anchor(q).map(|a| a.anchor.path.clone());
                if file != last_file {
                    if let Some(file) = &file {
                        list = list.child(
                            div()
                                .pt(px(4.))
                                .px(px(4.))
                                .font_family(m.font_mono.clone())
                                .text_size(m.text_xs)
                                .text_color(p.text_subtle.hsla())
                                .child(file.clone()),
                        );
                    }
                    last_file = file;
                }
            }
            list = list.child(self.render_card(q, cx));
        }

        div()
            .id("questions-pane")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .pt(px(6.))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .px(px(4.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(14.))
                            .child("Questions"),
                    )
                    .child(
                        div()
                            .text_size(m.text_xs)
                            .text_color(p.text_subtle.hsla())
                            .child("this workspace"),
                    ),
            )
            .child(tabs)
            .child(filters)
            .child(list)
            .child(
                div()
                    .mt(px(4.))
                    .mb(px(8.))
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(14.))
                    .bg(p.surface.alpha(0.6))
                    .text_size(m.text_sm)
                    .line_height(px(18.))
                    .text_color(p.text_muted.hsla())
                    .child(
                        "Answers go back to the agent as the result of its question, and are saved in the chat with a link to the code.",
                    ),
            )
    }
}
