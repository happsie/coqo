// SPDX-License-Identifier: Apache-2.0

//! The status bar's update item and its popover (board 1's status bar:
//! small text with a coloured glyph; the popover is the alert center's
//! card). See `docs/design/updates.md`.

use coqo_protocol::methods::update::{OffReason, UpdateState, UpdateStatus};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    gpui::{
        AnyElement, AnyWindowHandle, App, AppContext as _, Context, Empty, Entity, FontWeight,
        Global, InteractiveElement as _, IntoElement, ParentElement as _, Render,
        StatefulInteractiveElement as _, Styled as _, Subscription, Window, deferred, div,
        prelude::FluentBuilder as _, px,
    },
    widgets::{Icon, TestSupportExt as _, icon, popover, primary, raised},
};

use super::{Busy, Updates};

/// How the item reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// An update is ready: blue, worth knowing.
    Info,
    /// Checking or downloading: muted.
    Busy,
    /// Waiting for agents to finish before the daemon restarts: sand.
    Waiting,
    /// Something failed, or rolled back: rose.
    Failed,
    /// Nothing to do (shown only while the popover is open): muted.
    Muted,
}

/// The status bar's text for the update, if it shows at all. It shows
/// whenever there is something to do or to know, and while its popover is
/// open (after "Check for updates").
pub fn item_label(
    status: Option<&UpdateStatus>,
    busy: Option<Busy>,
    rollback_seen: bool,
    open: bool,
) -> Option<(Tone, String)> {
    if busy == Some(Busy::Installing) {
        return Some((Tone::Busy, "Installing the update…".into()));
    }
    let status = match status {
        Some(status) => status,
        None if open => return Some((Tone::Muted, "Updates".into())),
        None => return None,
    };
    if let Some(restart) = status.restart {
        return Some((Tone::Waiting, waiting_text(restart.running)));
    }
    match &status.state {
        UpdateState::Installed { .. } => return Some((Tone::Waiting, waiting_text(0))),
        UpdateState::Ready { release } => {
            return Some((Tone::Info, format!("Update to v{}", release.version)));
        }
        UpdateState::Downloading { version } => {
            return Some((Tone::Busy, format!("Downloading v{version}…")));
        }
        UpdateState::Failed {
            version: Some(version),
            ..
        } => return Some((Tone::Failed, format!("Update to v{version} failed"))),
        _ => {}
    }
    if let Some(rb) = &status.rolled_back
        && !rollback_seen
    {
        return Some((Tone::Failed, format!("v{} rolled back", rb.version)));
    }
    if !open {
        return None;
    }
    Some(match &status.state {
        _ if busy == Some(Busy::Checking) => (Tone::Busy, "Checking for updates…".into()),
        UpdateState::Checking => (Tone::Busy, "Checking for updates…".into()),
        UpdateState::Off { message, .. } => (Tone::Muted, message.clone()),
        UpdateState::Failed { .. } => (Tone::Failed, "Update check failed".into()),
        _ => (
            Tone::Muted,
            format!("Coqo v{} is up to date", status.current),
        ),
    })
}

/// "Restarting the daemon when agents are idle (N running)".
pub fn waiting_text(running: u32) -> String {
    if running == 0 {
        "Restarting the daemon…".into()
    } else {
        format!("Restarting the daemon when agents are idle ({running} running)")
    }
}

fn tone_color(tone: Tone, p: &coqo_ui::Palette) -> Rgb {
    match tone {
        Tone::Info => p.blue,
        Tone::Waiting => p.sand,
        Tone::Failed => p.rose,
        Tone::Busy | Tone::Muted => p.text_muted,
    }
}

fn tone_icon(tone: Tone) -> Icon {
    match tone {
        Tone::Info => Icon::ArrowUp,
        Tone::Busy => Icon::LoaderCircle,
        Tone::Waiting => Icon::RotateCw,
        Tone::Failed => Icon::CircleAlert,
        Tone::Muted => Icon::Check,
    }
}

/// The update item of one window. See [`status_item`].
pub struct UpdateItem {
    updates: Entity<Updates>,
    window: AnyWindowHandle,
    _observe: Subscription,
}

#[derive(Default)]
struct ItemViews(Vec<(AnyWindowHandle, Entity<UpdateItem>)>);

impl Global for ItemViews {}

/// The status bar's update item in `window`: a view per window over the
/// app's [`Updates`], so it re-renders when they change.
pub fn status_item(window: &mut Window, cx: &mut App) -> AnyElement {
    let Some(updates) = Updates::global(cx) else {
        return Empty.into_any_element();
    };
    let handle = window.window_handle();
    let open = cx.windows();
    let views = cx.default_global::<ItemViews>();
    views.0.retain(|(h, _)| *h == handle || open.contains(h));
    if let Some((_, view)) = views.0.iter().find(|(h, _)| *h == handle) {
        return view.clone().into_any_element();
    }
    let view = cx.new(|cx| UpdateItem {
        _observe: cx.observe(&updates, |_, _, cx| cx.notify()),
        updates,
        window: handle,
    });
    cx.default_global::<ItemViews>()
        .0
        .push((handle, view.clone()));
    view.into_any_element()
}

impl Render for UpdateItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let updates = self.updates.read(cx);
        let open = updates.is_open_in(self.window);
        let Some((tone, label)) = item_label(
            updates.status(),
            updates.busy(),
            updates.rollback_seen(),
            open,
        ) else {
            return Empty.into_any_element();
        };
        let p = cx.tokens().palette.clone();
        let color = tone_color(tone, &p);
        let window = self.window;
        let entity = self.updates.clone();
        let item = div()
            .id("status-update")
            .test_support()
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .text_color(color.hsla())
            .hover(|s| s.text_color(p.text.hsla()))
            .child(icon(tone_icon(tone), px(12.), color.hsla()))
            .child(label)
            .on_click(move |_, _, cx| {
                entity.update(cx, |u, cx| u.toggle(window, cx));
            });
        let mut slot = div().relative().child(item);
        if open {
            let entity = self.updates.clone();
            let card = div()
                .id("update-popup")
                .on_mouse_down_out(move |_, _, cx| entity.update(cx, |u, cx| u.close(cx)))
                .child(card(&self.updates, cx));
            slot = slot.child(
                deferred(div().absolute().right(px(-8.)).bottom(px(24.)).child(card))
                    .with_priority(3),
            );
        }
        slot.into_any_element()
    }
}

fn button(
    id: &'static str,
    label: &str,
    go: bool,
    cx: &App,
) -> coqo_ui::gpui::Stateful<coqo_ui::gpui::Div> {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let base = div()
        .px(px(12.))
        .py(px(7.))
        .rounded(px(10.))
        .text_size(m.text_sm)
        .cursor_pointer();
    let styled = if go {
        primary(base, cx)
    } else {
        raised(base, cx).text_color(p.text.hsla())
    };
    styled.id(id).child(label.to_owned())
}

/// Release notes as plain lines: headings bold, list items with a bullet.
fn notes(text: &str, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let mut list = div()
        .id("update-notes")
        .test_support()
        .flex()
        .flex_col()
        .gap(px(3.))
        .max_h(px(240.))
        .overflow_y_scroll()
        .p(px(12.))
        .rounded(px(12.))
        .bg(p.panel.hsla())
        .text_size(m.text_sm)
        .text_color(p.text_muted.hsla());
    let text = text.trim();
    if text.is_empty() {
        list = list.child("No release notes.");
    }
    for line in text.lines().map(str::trim_end) {
        let (heading, body) = match line.trim_start_matches('#') {
            rest if rest.len() < line.len() => (true, rest.trim()),
            _ => (false, line),
        };
        let body = body
            .strip_prefix("- ")
            .or_else(|| body.strip_prefix("* "))
            .map(|b| format!("• {b}"))
            .unwrap_or_else(|| body.to_owned());
        list = list.child(
            div()
                .when(heading, |d| {
                    d.pt(px(4.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text.hsla())
                })
                .when(body.is_empty(), |d| d.h(px(4.)))
                .child(body),
        );
    }
    list.into_any_element()
}

/// The popover: what the item means, and what to do about it.
fn card(updates: &Entity<Updates>, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (t.palette.clone(), t.metrics.clone());
    let u = updates.read(cx);
    let dev = u.location().is_dev();
    let status = u.status();
    let current = status
        .map_or(super::VERSION, |s| s.current.as_str())
        .to_owned();

    let title = |text: String| {
        div()
            .text_size(px(15.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.text.hsla())
            .child(text)
    };
    let line = |text: String| {
        div()
            .text_size(m.text_sm)
            .text_color(p.text_muted.hsla())
            .child(text)
    };
    let row = || {
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.))
            .pt(px(2.))
    };
    let on = |f: fn(&mut Updates, &mut Context<Updates>)| {
        let updates = updates.clone();
        move |_: &coqo_ui::gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            updates.update(cx, f);
        }
    };

    let mut body = popover(cx)
        .id("update-popover")
        .occlude()
        .test_support()
        .bg(p.panel_raised.hsla())
        .w(px(380.))
        .p(px(16.))
        .gap(px(10.))
        .text_size(m.text_md)
        .text_color(p.text.hsla());

    let restart = status.and_then(|s| s.restart);
    let installed = status.and_then(|s| match &s.state {
        UpdateState::Installed { version } => Some(version.clone()),
        _ => None,
    });
    if restart.is_some() || installed.is_some() {
        let running = restart.map_or(0, |r| r.running);
        body = body
            .child(title(super::view::waiting_text(running)))
            .child(line(match &installed {
                Some(v) => format!(
                    "Coqo v{v} is installed. Its daemon takes over once no agent turn is running; your chats resume there."
                ),
                None => "The daemon takes over from the new version once no agent turn is running; your chats resume there.".into(),
            }));
        if u.confirm_restart() {
            body = body
                .child(
                    div()
                        .id("update-restart-warning")
                        .test_support()
                        .p(px(10.))
                        .rounded(px(10.))
                        .bg(p.ground.mix(p.sand, 0.14).hsla())
                        .text_size(m.text_sm)
                        .text_color(p.text.hsla())
                        .child(format!(
                            "Restarting now stops {running} running agent turn{}. Their chats resume, but each turn stops where it is.",
                            if running == 1 { "" } else { "s" }
                        )),
                )
                .child(
                    row()
                        .child(button("update-restart-cancel", "Cancel", false, cx).on_click(on(Updates::cancel_restart)).test_support())
                        .child(button("update-restart-now", "Restart now", true, cx).on_click(on(Updates::restart_now)).test_support()),
                );
        } else {
            body = body.child(
                row().child(
                    button("update-restart-now", "Restart now", false, cx)
                        .on_click(on(Updates::restart_now))
                        .test_support(),
                ),
            );
        }
    } else {
        match status.map(|s| &s.state) {
            Some(UpdateState::Ready { release }) => {
                let mut sub = format!("You have v{current}");
                if let Some(date) = release.published_at.as_deref().and_then(|d| d.get(..10)) {
                    sub.push_str(&format!(" · released {date}"));
                }
                body = body
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(icon(Icon::ArrowUp, px(16.), p.blue.hsla()))
                            .child(title(format!("Coqo v{}", release.version))),
                    )
                    .child(line(sub))
                    .child(notes(&release.notes, cx))
                    .child(line(
                        "Signed with Coqo's release key and verified. Coqo restarts on the new version; the daemon follows once agents are idle."
                            .into(),
                    ));
                if dev {
                    body = body.child(line(
                        "Running a development build: nothing to install into.".into(),
                    ));
                }
                let installing = u.busy() == Some(Busy::Installing);
                let later = button("update-later", "Later", false, cx)
                    .on_click(on(Updates::close))
                    .test_support();
                let go = button(
                    "update-install",
                    if installing {
                        "Installing…"
                    } else {
                        "Update and restart"
                    },
                    true,
                    cx,
                );
                let go = if dev || installing {
                    go.opacity(0.5)
                        .cursor_default()
                        .test_support()
                        .into_any_element()
                } else {
                    go.on_click(on(Updates::install))
                        .test_support()
                        .into_any_element()
                };
                body = body.child(row().child(later).child(go));
            }
            Some(UpdateState::Downloading { version }) => {
                body = body
                    .child(title(format!("Downloading v{version}…")))
                    .child(line(
                        "It is checked against Coqo's release key before anything is installed."
                            .into(),
                    ));
            }
            Some(UpdateState::Failed {
                message, version, ..
            }) => {
                body = body
                    .child(title(match version {
                        Some(v) => format!("Update to v{v} failed"),
                        None => "Update check failed".into(),
                    }))
                    .child(
                        div()
                            .id("update-failure")
                            .test_support()
                            .text_size(m.text_sm)
                            .text_color(p.rose.hsla())
                            .child(message.clone()),
                    )
                    .child(
                        row().child(
                            button("update-check", "Try again", false, cx)
                                .on_click(on(Updates::check))
                                .test_support(),
                        ),
                    );
            }
            Some(UpdateState::Off { reason, message }) => {
                body = body.child(title(message.clone()));
                body = body.child(line(match reason {
                    OffReason::DevBuild => "Updates install only into an installed Coqo: ~/Applications/Coqo.app on macOS, ~/.local/share/coqo/bin on Linux.".into(),
                    OffReason::NoReleaseKey => "Builds made without the release key never install updates.".into(),
                    OffReason::Setting => "Set updates.check = true in settings.toml to check again.".into(),
                    OffReason::Unsupported => "Build Coqo from source to update it.".into(),
                }));
            }
            Some(UpdateState::Checking) => {
                body = body.child(title("Checking for updates…".into()));
            }
            Some(UpdateState::UpToDate { .. }) | None => {
                let checking = u.busy() == Some(Busy::Checking);
                body = body
                    .child(title(if checking {
                        "Checking for updates…".into()
                    } else {
                        format!("Coqo v{current} is up to date")
                    }))
                    .when(!checking, |b| {
                        b.child(
                            row().child(
                                button("update-check", "Check again", false, cx)
                                    .on_click(on(Updates::check))
                                    .test_support(),
                            ),
                        )
                    });
            }
            Some(UpdateState::Installed { .. }) => {}
        }
    }
    if let Some(rb) = status.and_then(|s| s.rolled_back.as_ref())
        && !u.rollback_seen()
        && !matches!(status.map(|s| &s.state), Some(UpdateState::Ready { .. }))
    {
        body = body.child(
            div()
                .id("update-rolled-back")
                .test_support()
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(10.))
                .rounded(px(10.))
                .bg(p.ground.mix(p.rose, 0.14).hsla())
                .text_size(m.text_sm)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                    "v{} didn't start, so Coqo went back to v{}",
                    rb.version, rb.restored
                )))
                .child(
                    div()
                        .font_family(m.font_mono.clone())
                        .text_size(m.text_xs)
                        .text_color(p.text_muted.hsla())
                        .child(rb.reason.clone()),
                ),
        );
    }
    if let Some(error) = u.error() {
        body = body.child(
            div()
                .id("update-error")
                .test_support()
                .text_size(m.text_sm)
                .text_color(p.rose.hsla())
                .child(error.to_owned()),
        );
    }
    body.into_any_element()
}
