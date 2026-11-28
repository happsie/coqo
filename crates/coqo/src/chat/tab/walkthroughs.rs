// SPDX-License-Identifier: Apache-2.0

//! Walkthroughs in the chat (#97, #100).
//!
//! When the agent publishes a walkthrough (`publish_walkthrough`), the
//! transcript gets a `walkthrough` update: the chat shows a card
//! "Walkthrough published: <title> · n steps", and the session panel's
//! "Walkthroughs" card lists every one the session published. Published
//! again (#253, the same id), the card shows the new one and the chat adds a
//! line "Walkthrough updated: <title> · n steps" rather than a second card. Clicking
//! either opens the walkthrough player ([`crate::walkthrough`]) at the first
//! unread step, with its popover (what, why, risks) at the code.
//!
//! **Coverage** (#99, board 3's coverage guard): the card says how many of
//! the change's hunks the steps cover. When some are not, a rose banner
//! lists them ("pom.xml:88-94 (+7 −1) is not explained by any step and
//! wasn't marked trivial"), each opening its lines, with "Ask <agent> to
//! explain it", which sends the agent the list. The panel's row has the
//! count and the first uncovered hunks.

use coqo_protocol::methods::walkthroughs::{
    StorageMode, UncoveredHunk, WalkthroughCoverage, WalkthroughLocation, WalkthroughSummary,
    WholeFile,
};
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
use crate::walkthrough::OpenWalkthrough;
use crate::workspace::Workspaces;

/// Uncovered hunks listed in the chat card's banner.
const CARD_HUNKS: usize = 6;
/// Uncovered hunks listed under a session panel row.
const ROW_HUNKS: usize = 3;

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

/// The line of a walkthrough published again.
pub(crate) fn updated_label(w: &WalkthroughSummary) -> String {
    format!(
        "Walkthrough updated: {} · {}",
        w.title,
        steps_label(w.steps)
    )
}

/// "Coverage 12/14 hunks", for the card and the panel row.
pub(crate) fn coverage_label(c: &WalkthroughCoverage) -> String {
    if c.unavailable.is_some() {
        "Coverage not checked".into()
    } else if c.total == 0 {
        "No changed hunks".into()
    } else {
        format!(
            "Coverage {}/{} hunk{}",
            c.covered + c.trivial,
            c.total,
            if c.total == 1 { "" } else { "s" }
        )
    }
}

/// Whether to flag `c`: some hunks are uncovered.
fn incomplete(c: &WalkthroughCoverage) -> bool {
    c.unavailable.is_none() && c.uncovered_count() > 0
}

/// The banner's sentence, as board 3 has it.
pub(crate) fn banner_label(c: &WalkthroughCoverage) -> (String, String) {
    let head = format!(
        "{} of {} changed hunks covered.",
        c.covered + c.trivial,
        c.total
    );
    let n = c.uncovered_count();
    let body = match (n, c.uncovered.first()) {
        (1, Some(h)) => format!(
            "{} ({}) is not explained by any step and wasn't marked trivial.",
            h.location(),
            h.change()
        ),
        _ => format!("{n} hunks are not explained by any step and weren't marked trivial."),
    };
    (head, body)
}

/// What "Ask the agent to explain it" sends.
pub(crate) fn explain_prompt(w: &WalkthroughSummary, c: &WalkthroughCoverage) -> String {
    let n = c.uncovered_count();
    let mut text = format!(
        "Your walkthrough \"{}\" ({}) doesn't cover {} changed hunk{}:",
        w.title,
        w.id,
        n,
        if n == 1 { "" } else { "s" }
    );
    for h in &c.uncovered {
        text.push_str(&format!("\n- {} ({})", h.location(), h.change()));
    }
    if (n as usize) > c.uncovered.len() {
        text.push_str(&format!(
            "\n- … and {} more",
            n as usize - c.uncovered.len()
        ));
    }
    text.push_str(
        "\nExplain each: add it to a step's anchors, or to trivial_hunks with a reason if it needs no explanation, and publish the walkthrough again.",
    );
    text
}

/// Where to open an uncovered hunk; a deleted file has nowhere.
fn hunk_location(h: &UncoveredHunk) -> Option<WalkthroughLocation> {
    (h.whole_file != Some(WholeFile::Deleted)).then(|| WalkthroughLocation {
        root: h.root,
        path: h.path.clone(),
        lines: h.new_range,
    })
}

/// Open `start` (root-relative) in the window's workspace.
fn open_location(start: &WalkthroughLocation, window: &mut Window, cx: &mut App) {
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

/// Opens `w` in the player.
fn opener(
    w: &WalkthroughSummary,
    focus: FocusHandle,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let action = OpenWalkthrough {
        workspace: w.workspace.0.clone(),
        id: w.id.0.clone(),
    };
    move |_, window, cx| {
        window.focus(&focus, cx);
        window.dispatch_action(Box::new(action.clone()), cx);
    }
}

/// Opens a hunk's lines, from a link inside a clickable card (the card
/// does not also open the player).
fn hunk_opener(
    at: Option<WalkthroughLocation>,
    focus: FocusHandle,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        cx.stop_propagation();
        window.focus(&focus, cx);
        if let Some(at) = &at {
            open_location(at, window, cx);
        }
    }
}

impl ChatTab {
    /// Fill the composer with the uncovered hunks of `w` for the agent, and
    /// send it unless a turn is running (then the user sends it after).
    pub(super) fn ask_to_explain(
        &mut self,
        w: &WalkthroughSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = &w.coverage else {
            return;
        };
        let prompt = explain_prompt(w, c);
        self.composer
            .update(cx, |c, cx| c.set_text(prompt, window, cx));
        if self.transcript.running {
            cx.notify();
            return;
        }
        self.send(window, cx);
    }

    /// "Ask Claude Code to explain it".
    fn ask_label(&self) -> String {
        match self.provider() {
            Some(p) => format!("Ask {} to explain it", p.name),
            None => "Ask the agent to explain it".into(),
        }
    }

    /// The card's coverage banner (board 3): rose while hunks are
    /// uncovered.
    fn render_coverage_banner(
        &self,
        ix: usize,
        w: &WalkthroughSummary,
        c: &WalkthroughCoverage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let (head, body) = banner_label(c);
        let mut banner = div()
            .id(("chat-walkthrough-coverage", ix))
            .test_support()
            .flex()
            .flex_col()
            .gap(px(6.))
            .mt(px(6.))
            .px(px(12.))
            .py(px(8.))
            .rounded(px(10.))
            .bg(p.rose.alpha(0.12))
            .border_1()
            .border_color(p.rose.alpha(0.3))
            .text_size(m.text_sm)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(div().flex_none().pt(px(2.)).child(icon(
                        Icon::CircleAlert,
                        px(14.),
                        p.rose.hsla(),
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.rose.hsla())
                                    .child(SharedString::from(head)),
                            )
                            .child(
                                div()
                                    .text_color(p.text_muted.hsla())
                                    .child(SharedString::from(body)),
                            ),
                    ),
            );
        if c.uncovered_count() > 1 {
            for (k, h) in c.uncovered.iter().take(CARD_HUNKS).enumerate() {
                banner = banner.child(hunk_link(
                    SharedString::from(format!("chat-walkthrough-hunk-{ix}-{k}")),
                    h,
                    self.focus.clone(),
                    &p,
                    &m,
                ));
            }
            let listed = c.uncovered.len().min(CARD_HUNKS);
            if c.uncovered_count() as usize > listed {
                banner = banner.child(
                    div()
                        .pl(px(22.))
                        .text_size(m.text_label)
                        .text_color(p.text_subtle.hsla())
                        .child(SharedString::from(format!(
                            "and {} more",
                            c.uncovered_count() as usize - listed
                        ))),
                );
            }
        } else if let Some(h) = c.uncovered.first() {
            banner = banner.child(hunk_link(
                SharedString::from(format!("chat-walkthrough-hunk-{ix}-0")),
                h,
                self.focus.clone(),
                &p,
                &m,
            ));
        }
        let summary = w.clone();
        banner = banner.child(
            div().flex().pl(px(22.)).child(
                div()
                    .id(("chat-walkthrough-ask", ix))
                    .test_support()
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(8.))
                    .bg(p.rose.alpha(0.18))
                    .border_1()
                    .border_color(p.rose.alpha(0.35))
                    .text_size(m.text_label)
                    .text_color(p.text.hsla())
                    .cursor_pointer()
                    .child(SharedString::from(self.ask_label()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.ask_to_explain(&summary, window, cx);
                    })),
            ),
        );
        banner.into_any_element()
    }

    /// The transcript's card for a published walkthrough.
    pub(super) fn render_walkthrough_card(
        &self,
        ix: usize,
        w: &WalkthroughSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let place = w.start.as_ref().map(|s| {
            format!(
                "Starts at {}:{} · Step through it",
                s.path,
                s.lines.start + 1
            )
        });
        let coverage = w.coverage.clone();
        let flagged = coverage.as_ref().is_some_and(incomplete);
        let banner = coverage
            .as_ref()
            .filter(|c| incomplete(c))
            .map(|c| self.render_coverage_banner(ix, w, c, cx));
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
                    .when_some(coverage.filter(|_| !flagged), |d, c| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .text_size(m.text_label)
                                .text_color(p.text_muted.hsla())
                                .when(c.is_complete() && c.total > 0, |d| {
                                    d.child(icon(Icon::Check, px(11.), p.sage.hsla()))
                                })
                                .child(SharedString::from(coverage_label(&c))),
                        )
                    })
                    .when(w.storage == StorageMode::Committed, |d| {
                        d.child(
                            div()
                                .text_size(m.text_label)
                                .text_color(p.text_subtle.hsla())
                                .child("Written to .coqo/walkthroughs for you to commit"),
                        )
                    })
                    .children(banner),
            )
            .child(
                div()
                    .pt(px(2.))
                    .child(icon(Icon::ChevronRight, px(13.), p.text_subtle.hsla())),
            )
            .on_click(opener(w, self.focus.clone()))
            .into_any_element()
    }

    /// The transcript's line for a walkthrough published again: what it is
    /// now, opening it; the walkthrough's card above has the details.
    pub(super) fn render_walkthrough_updated(
        &self,
        ix: usize,
        w: &WalkthroughSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let coverage = w.coverage.as_ref().map(|c| {
            let flagged = incomplete(c);
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .text_size(m.text_label)
                .text_color(if flagged {
                    p.rose.hsla()
                } else {
                    p.text_muted.hsla()
                })
                .when(flagged, |d| {
                    d.child(icon(Icon::CircleAlert, px(11.), p.rose.hsla()))
                })
                .when(c.is_complete() && c.total > 0, |d| {
                    d.child(icon(Icon::Check, px(11.), p.sage.hsla()))
                })
                .child(SharedString::from(coverage_label(c)))
        });
        div()
            .id(("chat-walkthrough-updated", ix))
            .test_support()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(6.))
            .rounded(px(10.))
            .border_1()
            .border_color(p.surface.hsla())
            .cursor_pointer()
            .text_size(m.text_sm)
            .child(icon(Icon::RotateCw, px(13.), p.blue.hsla()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(10.))
                    .child(
                        div()
                            .text_color(p.text.hsla())
                            .child(SharedString::from(updated_label(w))),
                    )
                    .children(coverage),
            )
            .child(icon(Icon::ChevronRight, px(13.), p.text_subtle.hsla()))
            .on_click(opener(w, self.focus.clone()))
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
            let coverage = w.coverage.as_ref().map(|c| {
                let flagged = incomplete(c);
                let mut col = div().flex().flex_col().gap(px(2.)).child(
                    div()
                        .id(("chat-walkthrough-coverage-row", n))
                        .test_support()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .text_size(m.text_label)
                        .text_color(if flagged {
                            p.rose.hsla()
                        } else {
                            p.text_subtle.hsla()
                        })
                        .when(flagged, |d| {
                            d.child(icon(Icon::CircleAlert, px(11.), p.rose.hsla()))
                        })
                        .child(SharedString::from(coverage_label(c))),
                );
                if flagged {
                    for (k, h) in c.uncovered.iter().take(ROW_HUNKS).enumerate() {
                        col = col.child(hunk_link(
                            SharedString::from(format!("chat-walkthrough-row-hunk-{n}-{k}")),
                            h,
                            self.focus.clone(),
                            &p,
                            &m,
                        ));
                    }
                }
                col
            });
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
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().truncate().child(SharedString::from(w.title.clone())))
                            .children(coverage),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(m.text_label)
                            .text_color(p.text_subtle.hsla())
                            .child(steps_label(w.steps)),
                    )
                    .on_click(opener(w, self.focus.clone())),
            );
        }
        card.into_any_element()
    }
}

/// An uncovered hunk as a link to its lines: `path:12-14` in mono and what
/// changed.
fn hunk_link(
    id: SharedString,
    h: &UncoveredHunk,
    focus: FocusHandle,
    p: &coqo_ui::theme::Palette,
    m: &coqo_ui::theme::Metrics,
) -> AnyElement {
    let at = hunk_location(h);
    let link = at.is_some();
    div()
        .id(id)
        .test_support()
        .flex()
        .items_center()
        .gap(px(6.))
        .pl(px(22.))
        .min_w_0()
        .text_size(m.text_label)
        .when(link, |d| d.cursor_pointer())
        .child(
            div()
                .flex_none()
                .font_family(m.font_mono.clone())
                .text_color(if link {
                    p.blue.hsla()
                } else {
                    p.text_muted.hsla()
                })
                .child(SharedString::from(h.location())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(p.text_subtle.hsla())
                .child(SharedString::from(h.change())),
        )
        .on_click(hunk_opener(at, focus))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::editor::LineRange;
    use coqo_protocol::methods::workspace::RootId;

    use super::*;

    fn hunk(path: &str, start: u32, end: u32, added: u32, removed: u32) -> UncoveredHunk {
        UncoveredHunk {
            root: RootId(0),
            path: path.into(),
            new_range: LineRange { start, end },
            old_range: None,
            added,
            removed,
            whole_file: None,
            renamed_from: None,
            excerpt: String::new(),
        }
    }

    #[test]
    fn labels() {
        assert_eq!(steps_label(1), "1 step");
        assert_eq!(steps_label(7), "7 steps");
        let mut c = WalkthroughCoverage {
            total: 15,
            covered: 12,
            trivial: 2,
            uncovered: vec![hunk("pom.xml", 87, 93, 7, 1)],
            unavailable: None,
        };
        assert_eq!(coverage_label(&c), "Coverage 14/15 hunks");
        assert_eq!(
            banner_label(&c),
            (
                "14 of 15 changed hunks covered.".to_owned(),
                "pom.xml:88-94 (+7 \u{2212}1) is not explained by any step and wasn't marked trivial."
                    .to_owned()
            )
        );
        c.covered = 11;
        c.uncovered.push(hunk("src/a.rs", 3, 3, 0, 2));
        assert_eq!(
            banner_label(&c).1,
            "2 hunks are not explained by any step and weren't marked trivial."
        );
        assert_eq!(
            coverage_label(&WalkthroughCoverage::default()),
            "No changed hunks"
        );
        assert_eq!(
            coverage_label(&WalkthroughCoverage::unavailable("no git")),
            "Coverage not checked"
        );
    }
}
