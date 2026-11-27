// SPDX-License-Identifier: Apache-2.0

//! The Welcome tab: a placeholder tab kind that shows the workbench shortcuts.
//! It is what a split opens when there is nothing to duplicate.

use coqo_ui::{
    ActiveTokens as _,
    dock::{AnyTab, TabItem, register_tab_kind},
    gpui::{
        App, AppContext as _, Context, FontWeight, IntoElement, ParentElement as _, Render,
        SharedString, Styled as _, Window, div, px,
    },
    widgets::{Icon, icon, kbd},
};

pub const KIND: &str = "welcome";

/// The Welcome tab's view.
#[derive(Debug, Default)]
pub struct WelcomeTab;

pub fn register(cx: &mut App) {
    register_tab_kind(cx, KIND, |_, _, cx| Some(new(cx)));
}

pub fn new(cx: &mut App) -> AnyTab {
    AnyTab::new(cx.new(|_| WelcomeTab))
}

/// A shortcut as shown to the user, with this platform's modifier.
pub fn shortcut(keys: &str) -> SharedString {
    if cfg!(target_os = "macos") {
        keys.replace("Ctrl ", "⌘").replace("Alt ", "⌥").into()
    } else {
        keys.to_owned().into()
    }
}

impl TabItem for WelcomeTab {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        "Welcome".into()
    }

    fn icon(&self, _: &App) -> Option<Icon> {
        Some(Icon::Sparkles)
    }
}

impl Render for WelcomeTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let p = &t.palette;
        let row = |label: &'static str, keys: &str| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(180.))
                        .text_color(p.text_muted.hsla())
                        .child(label),
                )
                .child(kbd(shortcut(keys), cx))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(icon(Icon::Sparkles, px(28.), p.sage.hsla()))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Welcome to Coqo"),
            )
            .child(
                div()
                    .text_color(p.text_muted.hsla())
                    .child("Files, chats and diffs are all tabs. Split them, drag them, mix them."),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pt_2()
                    .text_size(t.metrics.text_sm)
                    .child(row("Split right", "Ctrl \\"))
                    .child(row("Split down", "Ctrl Alt \\"))
                    .child(row("Close tab", "Ctrl W"))
                    .child(row("Close group", "Ctrl Alt W"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .w(px(180.))
                                    .text_color(p.text_muted.hsla())
                                    .child("New terminal"),
                            )
                            // Ctrl on every platform, like other editors.
                            .child(kbd(crate::terminal::NEW_TERMINAL_KEYS, cx)),
                    )
                    .child(row("Command palette", "Ctrl K")),
            )
    }
}
