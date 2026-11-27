// SPDX-License-Identifier: Apache-2.0

//! The bell, the alert center and the toast (board 6's alert column and
//! desktop notification, board 1's rail bell). See `docs/design/alerts.md`.

use std::rc::Rc;

use coqo_protocol::methods::alerts::{Alert, AlertId, AlertKind};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_ui::{
    ActiveTokens as _, Appearance, Rgb,
    gpui::{
        AnyElement, App, ElementId, FontWeight, InteractiveElement as _, IntoElement as _,
        ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
        div, prelude::FluentBuilder as _, px,
    },
    widgets::{Icon, TestSupportExt as _, icon, popover, raised},
};

use super::{AlertCenter, BellBadge, age, now_ms};
use crate::workspace::KnownWorkspaces;

/// What the alert center's rows and buttons do.
type OnAlert = Rc<dyn Fn(&Alert, &mut Window, &mut App)>;
type OnId = Rc<dyn Fn(AlertId, &mut Window, &mut App)>;
type OnClick = Rc<dyn Fn(&mut Window, &mut App)>;

/// What the alert center's controls do.
#[derive(Clone)]
pub struct CenterActions {
    /// A row was clicked: open where it points.
    pub open: OnAlert,
    /// A row's × was clicked.
    pub dismiss: OnId,
    /// The do-not-disturb switch was flipped.
    pub toggle_dnd: OnClick,
    /// **Dismiss all** was clicked.
    pub dismiss_all: OnClick,
}

/// The colour of an alert kind: sand needs you, rose failed, sage done,
/// blue worth knowing.
pub fn kind_color(kind: AlertKind, p: &coqo_ui::Palette) -> Rgb {
    match kind {
        AlertKind::NeedsYou => p.sand,
        AlertKind::Error => p.rose,
        AlertKind::Done => p.sage,
        AlertKind::Info => p.blue,
    }
}

fn workspace_name(id: Option<&WorkspaceId>, cx: &App) -> Option<String> {
    let id = id?;
    KnownWorkspaces::global(cx)
        .and_then(|k| k.read(cx).get(id).map(|w| w.name.clone()))
        .or_else(|| Some(id.0.clone()))
}

/// The bell's unread badge: a small count in the corner where board 1
/// puts its dot, in the most pressing kind's colour.
pub fn bell_badge(badge: BellBadge, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let p = &t.palette;
    let light = t.appearance == Appearance::Light;
    let text = if light { p.panel_raised } else { p.ground };
    let id = match badge.kind {
        AlertKind::NeedsYou => "alerts-badge-needs-you",
        AlertKind::Error => "alerts-badge-error",
        AlertKind::Done => "alerts-badge-done",
        AlertKind::Info => "alerts-badge-info",
    };
    let label = if badge.unread > 9 {
        "9+".to_owned()
    } else {
        badge.unread.to_string()
    };
    div()
        .id(id)
        .test_support()
        .absolute()
        .top(px(5.))
        .right(px(4.))
        .min_w(px(15.))
        .h(px(15.))
        .px(px(3.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .bg(kind_color(badge.kind, p).hsla())
        .border_1()
        .border_color(p.ground.hsla())
        .text_color(text.hsla())
        .text_size(px(9.5))
        .font_weight(FontWeight::BOLD)
        .child(label)
        .into_any_element()
}

/// The do-not-disturb switch (board 6): a 38 × 22 track, the knob left
/// when off and right, in sage, when on.
fn switch(on: bool, toggle: OnClick, cx: &App) -> AnyElement {
    let p = &cx.tokens().palette;
    div()
        .id("alerts-dnd")
        .test_support()
        .relative()
        .w(px(38.))
        .h(px(22.))
        .flex_none()
        .rounded(px(11.))
        .cursor_pointer()
        .bg(if on {
            p.ground.mix(p.sage, 0.45).hsla()
        } else {
            p.border_strong.hsla()
        })
        .child(
            div()
                .absolute()
                .top(px(3.))
                .when(on, |k| k.right(px(3.)))
                .when(!on, |k| k.left(px(3.)))
                .size(px(16.))
                .rounded_full()
                .bg(if on {
                    p.sage.hsla()
                } else {
                    p.text_muted.hsla()
                }),
        )
        .on_click(move |_, window, cx| toggle(window, cx))
        .into_any_element()
}

fn row(ix: usize, alert: &Alert, now: i64, actions: &CenterActions, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let unread = alert.is_unread();
    let mut meta = age(alert.created_at, now);
    if alert.resolved_at.is_some() {
        meta.push_str(" · resolved");
    }
    let open = Rc::clone(&actions.open);
    let dismiss = Rc::clone(&actions.dismiss);
    let clicked = alert.clone();
    let id = alert.id;
    let group: SharedString = format!("alert-row-{ix}").into();
    div()
        .id(("alert", ix))
        .test_support()
        .group(group.clone())
        .flex()
        .items_start()
        .gap(px(10.))
        .px(px(12.))
        .py(px(10.))
        .rounded(px(12.))
        .cursor_pointer()
        .when(unread, |r| r.bg(p.surface.hsla()))
        .hover(|r| r.bg(p.surface_selected.hsla()))
        .child(
            div()
                .mt(px(6.))
                .size(px(8.))
                .flex_none()
                .rounded_full()
                .bg(kind_color(alert.kind, p).hsla()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .line_height(px(18.))
                .text_color(if unread { p.text } else { p.text_muted }.hsla())
                .child(div().child(alert.title.clone()))
                .children(alert.body.clone().map(|body| {
                    div()
                        .truncate()
                        .text_size(m.text_sm)
                        .text_color(p.text_muted.hsla())
                        .child(body)
                }))
                .child(
                    div()
                        .text_size(m.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .child(meta),
                ),
        )
        .child(
            div()
                .id(("alert-dismiss", ix))
                .test_support()
                .flex_none()
                .mt(px(2.))
                .p(px(2.))
                .rounded(px(6.))
                .invisible()
                .group_hover(group, |s| s.visible())
                .hover(|s| s.bg(p.border_strong.hsla()))
                .child(icon(Icon::X, px(12.), p.text_subtle.hsla()))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    dismiss(id, window, cx);
                }),
        )
        .on_click(move |_, window, cx| open(&clicked, window, cx))
        .into_any_element()
}

/// The alerts grouped by workspace: the groups ordered by their newest
/// alert, each newest first. Alerts about no workspace come under `None`.
pub fn groups<'a>(alerts: &[&'a Alert]) -> Vec<(Option<WorkspaceId>, Vec<&'a Alert>)> {
    let mut out: Vec<(Option<WorkspaceId>, Vec<&'a Alert>)> = Vec::new();
    for alert in alerts {
        match out.iter_mut().find(|(w, _)| *w == alert.workspace) {
            Some((_, list)) => list.push(alert),
            None => out.push((alert.workspace.clone(), vec![alert])),
        }
    }
    out
}

/// The alert center popover (board 6's alert column).
pub fn center(center: &AlertCenter, actions: &CenterActions, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let now = now_ms();
    let alerts = center.alerts();
    let mut list = div()
        .id("alerts-list")
        .flex()
        .flex_col()
        .gap(px(6.))
        .max_h(px(520.))
        .overflow_y_scroll();
    let mut ix = 0;
    for (workspace, group) in groups(&alerts) {
        let label = workspace_name(workspace.as_ref(), cx).unwrap_or_else(|| "Other".to_owned());
        list = list.child(
            div()
                .pt(px(4.))
                .pb(px(2.))
                .text_size(m.text_label)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_subtle.hsla())
                .child(label.to_uppercase()),
        );
        for alert in group {
            list = list.child(row(ix, alert, now, actions, cx));
            ix += 1;
        }
    }
    if alerts.is_empty() {
        list = list.child(
            div()
                .id("alerts-empty")
                .test_support()
                .py(px(18.))
                .text_size(m.text_sm)
                .text_color(p.text_subtle.hsla())
                .child("Nothing needs you right now."),
        );
    }
    let dismiss_all = Rc::clone(&actions.dismiss_all);
    popover(cx)
        .id("alerts-center")
        .occlude()
        .test_support()
        // Board 6's column: `panel_raised`, unread rows on `surface`.
        .bg(p.panel_raised.hsla())
        .w(px(340.))
        .p(px(16.))
        .gap(px(10.))
        .text_size(m.text_md)
        .text_color(p.text.hsla())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Alerts"),
                )
                .child(
                    div()
                        .text_size(m.text_xs)
                        .text_color(p.text_muted.hsla())
                        .child("Do not disturb"),
                )
                .child(switch(
                    center.do_not_disturb(),
                    Rc::clone(&actions.toggle_dnd),
                    cx,
                )),
        )
        .child(list)
        .when(!alerts.is_empty(), |card| {
            card.child(
                div().flex().justify_end().child(
                    div()
                        .id("alerts-dismiss-all")
                        .test_support()
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(m.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .hover(|s| s.bg(p.surface_selected.hsla()).text_color(p.text.hsla()))
                        .child("Dismiss all")
                        .on_click(move |_, window, cx| dismiss_all(window, cx)),
                ),
            )
        })
        .into_any_element()
}

fn button(
    id: &'static str,
    label: &'static str,
    cx: &App,
) -> coqo_ui::gpui::Stateful<coqo_ui::gpui::Div> {
    let t = cx.tokens();
    raised(div(), cx)
        .id(id)
        .flex_1()
        .flex()
        .justify_center()
        .py(px(6.))
        .rounded(px(9.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(t.palette.text.hsla())
        .child(label)
}

/// The toast for a new alert that needs you (board 6's desktop
/// notification, in the window): where, what, and **Open** / **Later**.
pub fn toast(alert: &Alert, open: OnAlert, later: OnClick, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let place = workspace_name(alert.workspace.as_ref(), cx)
        .map_or_else(|| "Coqo".to_owned(), |w| format!("Coqo · {w}"));
    let clicked = alert.clone();
    let opened = alert.clone();
    let open_row = Rc::clone(&open);
    popover(cx)
        .id("alerts-toast")
        .occlude()
        .test_support()
        .w(px(340.))
        .px(px(14.))
        .py(px(12.))
        .gap(px(8.))
        .text_size(m.text_md)
        .text_color(p.text.hsla())
        .cursor_pointer()
        .on_click(move |_, window, cx| open_row(&clicked, window, cx))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(m.text_xs)
                .text_color(p.text_muted.hsla())
                .child(icon(Icon::Bell, px(14.), kind_color(alert.kind, p).hsla()))
                .child(div().flex_1().truncate().child(place))
                .child("now"),
        )
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(alert.title.clone()),
        )
        .children(alert.body.clone().map(|body| {
            div()
                .text_size(m.text_sm)
                .text_color(p.text_muted.hsla())
                .line_clamp(2)
                .child(body)
        }))
        .child(
            div()
                .flex()
                .gap(px(6.))
                .child(
                    button("alerts-toast-open", "Open", cx)
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            open(&opened, window, cx);
                        })
                        .test_support(),
                )
                .child(
                    button("alerts-toast-later", "Later", cx)
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            later(window, cx);
                        })
                        .test_support(),
                ),
        )
        .into_any_element()
}

/// The element id of the `ix`th row of the alert center, top to bottom.
#[allow(dead_code)] // Tests use it.
pub fn row_id(ix: usize) -> ElementId {
    ("alert", ix).into()
}
