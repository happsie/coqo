// SPDX-License-Identifier: Apache-2.0

//! Walkthroughs in the chat (#97), until the player (#100) exists.
//!
//! When the agent publishes a walkthrough (`publish_walkthrough`), the
//! transcript gets a `walkthrough` update: the chat shows a card
//! "Walkthrough published: <title> · n steps", and the session panel's
//! "Walkthroughs" card lists every one the session published. Clicking
//! either opens the first step's code in a file tab with its lines
//! selected. Stepping through, the what/why/risks popover and the
//! understood/question marks are the player's (#100).

use coqo_protocol::methods::walkthroughs::{StorageMode, WalkthroughLocation, WalkthroughSummary};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, ClickEvent, Context, FocusHandle, FontWeight, InteractiveElement as _,
        IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _,
        Styled as _, Window, div, prelude::FluentBuilder as _, px,
    },
    widgets::{Icon, TestSupportExt as _, icon, section_label},
};

use super::ChatTab;
use crate::chat::OpenLocation;
use crate::workspace::Workspaces;

/// "n steps", "1 step".
pub(crate) fn steps_label(n: u32) -> String {
    if n == 1 {
        "1 step".into()
    } else {
        format!("{n} steps")
    }
}

/// The card's headline.
pub(crate) fn published_label(w: &WalkthroughSummary) -> String {
    format!(
        "Walkthrough published: {} · {}",
        w.title,
        steps_label(w.steps)
    )
}

/// Open `start` (root-relative) in the window's workspace.
fn open_start(start: &WalkthroughLocation, window: &mut Window, cx: &mut App) {
    let workspaces = Workspaces::for_window(window, cx).or_else(|| Workspaces::global(cx));
    let Some(root) = workspaces.as_ref().and_then(|w| {
        w.read(cx)
            .current()?
            .roots
            .iter()
            .find(|r| r.id == start.root)
            .map(|r| r.path.clone())
    }) else {
        return;
    };
    let action = OpenLocation {
        path: format!("{}/{}", root.trim_end_matches('/'), start.path),
        line: Some(start.lines.start),
        end: Some(start.lines.end),
    };
    window.dispatch_action(Box::new(action), cx);
}

fn opener(
    start: Option<WalkthroughLocation>,
    focus: FocusHandle,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        window.focus(&focus, cx);
        if let Some(start) = &start {
            open_start(start, window, cx);
        }
    }
}

impl ChatTab {
    /// The transcript's card for a published walkthrough.
    pub(super) fn render_walkthrough_card(
        &self,
        ix: usize,
        w: &WalkthroughSummary,
        cx: &App,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let start = w.start.clone();
        let place = start
            .as_ref()
            .map(|s| format!("Starts at {}:{}", s.path, s.lines.start + 1));
        div()
            .id(("chat-walkthrough", ix))
            .test_support()
            .flex()
            .items_start()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .rounded(px(12.))
            .border_1()
            .border_color(p.surface.hsla())
            .bg(p.panel_raised.hsla())
            .cursor_pointer()
            .text_size(m.text_sm)
            .child(
                div()
                    .pt(px(2.))
                    .child(icon(Icon::FileCode, px(14.), p.blue.hsla())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child(SharedString::from(published_label(w))),
                    )
                    .when_some(place, |d, place| {
                        d.child(
                            div()
                                .text_size(m.text_label)
                                .text_color(p.text_muted.hsla())
                                .child(SharedString::from(place)),
                        )
                    })
                    .when(w.storage == StorageMode::Committed, |d| {
                        d.child(
                            div()
                                .text_size(m.text_label)
                                .text_color(p.text_subtle.hsla())
                                .child("Written to .coqo/walkthroughs for you to commit"),
                        )
                    }),
            )
            .child(
                div()
                    .pt(px(2.))
                    .child(icon(Icon::ChevronRight, px(13.), p.text_subtle.hsla())),
            )
            .on_click(opener(start, self.focus.clone()))
            .into_any_element()
    }

    /// The session panel's "Walkthroughs" card.
    pub(super) fn render_walkthroughs(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let list = &self.transcript.walkthroughs;
        let mut card = div()
            .id("chat-walkthroughs")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(14.))
            .bg(p.panel_raised.hsla())
            .text_size(m.text_sm)
            .child(section_label("Walkthroughs", cx));
        if list.is_empty() {
            card = card.child(
                div()
                    .px_2()
                    .text_color(p.text_subtle.hsla())
                    .child("None yet: the agent publishes one when it finishes a change"),
            );
        }
        // Newest first.
        for (n, w) in list.iter().rev().enumerate() {
            card = card.child(
                div()
                    .id(("chat-walkthrough-row", n))
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px_2()
                    .cursor_pointer()
                    .text_color(p.text.hsla())
                    .child(div().flex_none().pt(px(2.)).child(icon(
                        Icon::FileCode,
                        px(13.),
                        p.blue.hsla(),
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(w.title.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(m.text_label)
                            .text_color(p.text_subtle.hsla())
                            .child(steps_label(w.steps)),
                    )
                    .on_click(opener(w.start.clone(), self.focus.clone())),
            );
        }
        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(steps_label(1), "1 step");
        assert_eq!(steps_label(7), "7 steps");
    }
}
