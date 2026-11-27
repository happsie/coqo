// SPDX-License-Identifier: Apache-2.0

//! The session panel's "Acceptance criteria" card (board 1, #96).
//!
//! The agent states what "done" means with `set_acceptance_criteria`; the
//! criteria arrive on the session info (`sessions/changes`). Each row has
//! the criterion's status glyph (sage ✓ met, rose ✗ failed, sand ○ pending,
//! subtle unknown), its id in blue (`AC1`, the id walkthroughs refer to)
//! and its text; evidence is listed under it, and evidence with a file
//! opens that file at its line.
//!
//! The user can change a status by clicking its glyph (pending → met →
//! failed → pending). That goes to the daemon as `sessions/setCriteria` with
//! the daemon token, so the change is the user's: the agent sees it, marked
//! "set by the user", in its next `set_acceptance_criteria` result.

use coqo_protocol::methods::agents::{
    AcceptanceCriterion, CriterionAuthor, CriterionInput, CriterionStatus, SetCriteria,
    SetCriteriaParams,
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
        SharedString, StatefulInteractiveElement as _, Styled as _, div,
        prelude::FluentBuilder as _, px,
    },
    widgets::{Icon, TestSupportExt as _, icon, section_label},
};

use super::ChatTab;
use crate::chat::OpenLocation;

/// The status a criterion's glyph moves to when the user clicks it.
pub(crate) fn next_status(status: CriterionStatus) -> CriterionStatus {
    match status {
        CriterionStatus::Pending | CriterionStatus::Unknown => CriterionStatus::Met,
        CriterionStatus::Met => CriterionStatus::Failed,
        CriterionStatus::Failed => CriterionStatus::Pending,
    }
}

impl ChatTab {
    /// The session's acceptance criteria, newest info first.
    pub(crate) fn criteria<'a>(&'a self, cx: &'a coqo_ui::gpui::App) -> &'a [AcceptanceCriterion] {
        self.latest_info(cx).map_or(&[], |i| i.criteria.as_slice())
    }

    /// The user marks criterion `id` with `status`.
    pub(crate) fn mark_criterion(
        &mut self,
        id: String,
        status: CriterionStatus,
        cx: &mut Context<Self>,
    ) {
        if self.is_closed() {
            return;
        }
        if let Some(daemon) = Self::daemon(cx) {
            let call = daemon.read(cx).request::<SetCriteria>(SetCriteriaParams {
                session: self.session.clone(),
                criteria: vec![CriterionInput {
                    id: Some(id),
                    status: Some(status),
                    ..CriterionInput::default()
                }],
                replace: false,
            });
            self.spawn_request(call, "Couldn't change the criterion", cx);
        }
    }

    pub(super) fn render_criteria(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let criteria = self.criteria(cx).to_vec();
        let met = criteria
            .iter()
            .filter(|c| c.status == CriterionStatus::Met)
            .count();
        let mut card = div()
            .id("chat-criteria")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(14.))
            .bg(p.panel_raised.hsla())
            .text_size(m.text_sm)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(section_label("Acceptance criteria", cx))
                    .when(!criteria.is_empty(), |d| {
                        d.child(
                            div()
                                .id("chat-criteria-count")
                                .test_support()
                                .px_2()
                                .py_1()
                                .text_size(m.text_label)
                                .text_color(p.text_subtle.hsla())
                                .child(format!("{met} of {} met", criteria.len())),
                        )
                    }),
            );
        if criteria.is_empty() {
            card = card.child(
                div()
                    .px_2()
                    .text_color(p.text_subtle.hsla())
                    .child("The agent hasn't set any yet"),
            );
        }
        let closed = self.is_closed();
        for (n, c) in criteria.into_iter().enumerate() {
            let (glyph, color) = match c.status {
                CriterionStatus::Met => (Icon::Check, p.sage.hsla()),
                CriterionStatus::Failed => (Icon::CircleX, p.rose.hsla()),
                CriterionStatus::Pending => (Icon::Circle, p.sand.hsla()),
                CriterionStatus::Unknown => (Icon::CircleAlert, p.text_subtle.hsla()),
            };
            let by_user = c.changed_by == CriterionAuthor::User;
            let next = next_status(c.status);
            let id = c.id.clone();
            let mut mark = div()
                .id(("chat-criterion-mark", n))
                .test_support()
                .flex_none()
                .pt(px(2.))
                .child(icon(glyph, px(13.), color));
            if !closed {
                mark = mark
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mark_criterion(id.clone(), next, cx);
                    }));
            }
            let mut body = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.blue.hsla())
                                .child(SharedString::from(c.id.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(SharedString::from(c.text.clone())),
                        ),
                )
                .when(by_user, |d| {
                    d.child(
                        div()
                            .id(("chat-criterion-by-user", n))
                            .test_support()
                            .text_size(m.text_label)
                            .text_color(p.text_subtle.hsla())
                            .child("Marked by you"),
                    )
                });
            for (k, e) in c.evidence.iter().enumerate() {
                let place = e.path.as_deref().map(|path| {
                    let shown = self.relative(path);
                    match e.line {
                        Some(l) => format!("{shown}:{l}"),
                        None => shown.to_owned(),
                    }
                });
                let mut row = div()
                    .id(SharedString::from(format!(
                        "chat-criterion-evidence-{n}-{k}"
                    )))
                    .test_support()
                    .text_size(m.text_label)
                    .text_color(p.text_muted.hsla())
                    .child(SharedString::from(e.text.clone()));
                if let (Some(path), Some(place)) = (e.path.clone(), place) {
                    let action = OpenLocation {
                        path,
                        line: e.line.map(|l| l.saturating_sub(1)),
                        end: None,
                    };
                    let focus = self.focus.clone();
                    row = row.child(
                        div()
                            .text_color(p.blue.hsla())
                            .cursor_pointer()
                            .child(SharedString::from(place)),
                    );
                    row = row.cursor_pointer().on_click(move |_, window, cx| {
                        window.focus(&focus, cx);
                        window.dispatch_action(Box::new(action.clone()), cx);
                    });
                }
                body = body.child(row);
            }
            card = card.child(
                div()
                    .id(("chat-criterion", n))
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px_2()
                    .text_color(p.text.hsla())
                    .child(mark)
                    .child(body),
            );
        }
        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_glyph_cycles_through_the_statuses() {
        use CriterionStatus as C;
        let mut s = C::Unknown;
        let mut seen = Vec::new();
        for _ in 0..4 {
            s = next_status(s);
            seen.push(s);
        }
        assert_eq!(seen, [C::Met, C::Failed, C::Pending, C::Met]);
    }
}
