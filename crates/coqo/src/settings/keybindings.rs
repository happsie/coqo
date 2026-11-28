// SPDX-License-Identifier: Apache-2.0

//! The keyboard shortcuts page of the Settings tab (issue #354, design board
//! 8, `docs/design/settings.md`).
//!
//! Every command in the registry with its category, its keys and where they
//! come from (the default or the user's file). A search narrows the list by
//! command or by keys. **Change** records new keys: while recording, every
//! keystroke (chords too) is taken by the page before any binding or text
//! field sees it, so nothing fires. **Save** writes them; if another
//! command in the same context has them, the page names it and offers
//! **Replace** or **Cancel**. **Reset** puts a command back to its
//! defaults, **Reset all** (after a confirmation) all of them.
//!
//! The changes go to `keybindings.toml` through [`keyfile::write_entry`],
//! which keeps the file's comments, and apply at once
//! ([`registry::load_user_keys`]).

use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, AppContext as _, Context, FontWeight, InteractiveElement as _,
        IntoElement, Keystroke, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement, Styled as _, Subscription, Window, div,
        prelude::FluentBuilder as _, px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{TestSupportExt as _, kbd, panel, primary, raised, section_label},
};

use super::page::{CustomPage, KEYBOARD_PAGE, OpenSettingsAt, register_page};
use crate::commands::{
    self, Command,
    keyfile::{self},
    registry::{self, CommandRegistry},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

/// The command that opens this page.
pub const OPEN_COMMAND: &str = "settings.open_keyboard";

/// Register the page (replacing the placeholder under [`KEYBOARD_PAGE`]) and
/// the palette command that opens it.
pub fn register(cx: &mut App) {
    register_page(
        cx,
        CustomPage::new(KEYBOARD_PAGE, "Keyboard shortcuts", 900, |window, cx| {
            cx.new(|cx| KeybindingsEditor::new(window, cx)).into()
        }),
    );
    commands::register(
        cx,
        [Command::new(
            OPEN_COMMAND,
            "Open keyboard shortcuts",
            "Preferences",
            OpenSettingsAt {
                section: KEYBOARD_PAGE.into(),
            },
        )],
    );
}

fn mac() -> bool {
    cfg!(target_os = "macos")
}

/// `keys` as GPUI spells them in one canonical form (modifier order and
/// aliases), to compare bindings.
fn normalize(keys: &str, cx: &App) -> String {
    let os = cx.platform_info().os;
    keys.split_whitespace()
        .map(|chord| {
            Keystroke::parse_with_os(chord, os)
                .map(|k| k.unparse(os))
                .unwrap_or_else(|_| chord.to_owned())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What is being recorded.
struct Recording {
    /// The command the keys are for.
    id: SharedString,
    /// The keystrokes so far, in GPUI's syntax.
    strokes: Vec<String>,
    /// Commands that have the recorded keys in the same context, once Save
    /// found some.
    conflicts: Vec<SharedString>,
}

/// The keyboard shortcuts page. See the module docs.
pub struct KeybindingsEditor {
    query: gpui::Entity<PromptInput>,
    recording: Option<Recording>,
    confirm_reset_all: bool,
    /// Why the last write failed.
    error: Option<String>,
    /// Takes every keystroke while recording.
    intercept: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl KeybindingsEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| PromptInput::new("Search commands or keys", 1, 1, window, cx));
        let sub = cx.subscribe(&query, |_, _, event: &PromptInputEvent, cx| {
            if matches!(event, PromptInputEvent::Changed) {
                cx.notify();
            }
        });
        Self {
            query,
            recording: None,
            confirm_reset_all: false,
            error: None,
            intercept: None,
            _subscriptions: vec![sub],
        }
    }

    /// Start recording keys for command `id`.
    pub fn record(&mut self, id: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_reset_all = false;
        self.error = None;
        self.recording = Some(Recording {
            id,
            strokes: Vec::new(),
            conflicts: Vec::new(),
        });
        let this = cx.weak_entity();
        let handle = window.window_handle();
        self.intercept = Some(cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() != handle {
                return;
            }
            let keystroke = event.keystroke.clone();
            let os = cx.platform_info().os;
            let alive = this
                .update(cx, |this, cx| this.take_key(&keystroke, os, cx))
                .is_ok();
            if alive {
                cx.stop_propagation();
            }
        }));
        cx.notify();
    }

    /// A keystroke while recording.
    fn take_key(
        &mut self,
        keystroke: &Keystroke,
        os: gpui::OperatingSystem,
        cx: &mut Context<Self>,
    ) {
        let Some(recording) = &mut self.recording else {
            return;
        };
        if keystroke.is_modifier_key() {
            return;
        }
        let m = &keystroke.modifiers;
        let bare = !(m.control || m.alt || m.shift || m.platform || m.function);
        if bare && keystroke.key == "escape" {
            self.stop(cx);
            return;
        }
        recording.strokes.push(keystroke.unparse(os));
        recording.conflicts.clear();
        cx.notify();
    }

    /// Stop recording and drop what was recorded.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.recording = None;
        self.intercept = None;
        cx.notify();
    }

    /// Save the recorded keys: write them, or, when another command has
    /// them in the same context, remember which so the page can ask.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        let Some(recording) = &self.recording else {
            return;
        };
        if recording.strokes.is_empty() {
            return;
        }
        let keys = recording.strokes.join(" ");
        let conflicts = conflicts_of(&recording.id, &keys, cx);
        if conflicts.is_empty() {
            self.commit(&[], cx);
        } else if let Some(recording) = &mut self.recording {
            recording.conflicts = conflicts;
            cx.notify();
        }
    }

    /// Give the recorded keys to the command, taking them from the commands
    /// that had them.
    pub fn replace(&mut self, cx: &mut Context<Self>) {
        let Some(recording) = &self.recording else {
            return;
        };
        let taken = recording.conflicts.clone();
        self.commit(&taken, cx);
    }

    fn commit(&mut self, take_from: &[SharedString], cx: &mut Context<Self>) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        self.intercept = None;
        let keys = recording.strokes.join(" ");
        let wanted = normalize(&keys, cx);
        let path = cx.global::<CommandRegistry>().keys_path();
        let mut result = Ok(());
        for other in take_from {
            let rest: Vec<String> = cx
                .global::<CommandRegistry>()
                .keys_of(other)
                .into_iter()
                .filter(|k| normalize(k, cx) != wanted)
                .map(|k| keyfile::to_file_syntax(&k, mac()))
                .collect();
            result = result.and_then(|()| keyfile::write_entry(&path, other, Some(&rest)));
        }
        let ours = [keyfile::to_file_syntax(&keys, mac())];
        result = result.and_then(|()| keyfile::write_entry(&path, &recording.id, Some(&ours)));
        self.finish(result, cx);
    }

    /// Put command `id` back to its default keys.
    pub fn reset(&mut self, id: &str, cx: &mut Context<Self>) {
        let path = cx.global::<CommandRegistry>().keys_path();
        let result = keyfile::write_entry(&path, id, None);
        self.finish(result, cx);
    }

    /// Put every command back to its default keys.
    pub fn reset_all(&mut self, cx: &mut Context<Self>) {
        self.confirm_reset_all = false;
        let path = cx.global::<CommandRegistry>().keys_path();
        let result = keyfile::remove_all(&path);
        self.finish(result, cx);
    }

    /// Apply what was written, or show why it wasn't.
    fn finish(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                self.error = None;
                let path = cx.global::<CommandRegistry>().keys_path();
                registry::load_user_keys(path, cx);
                registry::report(cx);
            }
            Err(e) => self.error = Some(e),
        }
        cx.notify();
    }
}

/// The commands that have `keys` in the same context as command `id`.
fn conflicts_of(id: &str, keys: &str, cx: &App) -> Vec<SharedString> {
    let registry = cx.global::<CommandRegistry>();
    let Some(mine) = registry.get(id) else {
        return Vec::new();
    };
    let wanted = normalize(keys, cx);
    registry
        .commands()
        .iter()
        .filter(|c| c.id != mine.id && c.context == mine.context)
        .filter(|c| {
            registry
                .keys_of(&c.id)
                .iter()
                .any(|k| normalize(k, cx) == wanted)
        })
        .map(|c| c.id.clone())
        .collect()
}

fn title_of(id: &str, cx: &App) -> SharedString {
    cx.global::<CommandRegistry>()
        .get(id)
        .map(|c| c.title.clone())
        .unwrap_or_else(|| id.to_owned().into())
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    strong: bool,
    cx: &App,
) -> impl StatefulInteractiveElement + IntoElement {
    let t = cx.tokens();
    let base = div()
        .px(px(12.))
        .py(px(5.))
        .rounded(px(10.))
        .text_size(t.metrics.text_sm)
        .cursor_pointer()
        .child(label);
    let base = if strong {
        primary(base, cx).font_weight(FontWeight::SEMIBOLD)
    } else {
        raised(base, cx).text_color(t.palette.text.hsla())
    };
    base.id(id).test_support()
}

impl Render for KeybindingsEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let p = &t.palette;
        let registry = cx.global::<CommandRegistry>();
        let query = self.query.read(cx).text(cx).trim().to_lowercase();
        let mut commands: Vec<&Command> = registry.commands().iter().collect();
        commands.sort_by(|a, b| (&a.category, &a.title).cmp(&(&b.category, &b.title)));

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut category: Option<&SharedString> = None;
        for command in commands {
            let keys: Vec<String> = registry
                .keys_of(&command.id)
                .iter()
                .map(|k| keyfile::display(k, mac()))
                .collect();
            let recording = self.recording.as_ref().filter(|r| r.id == command.id);
            if recording.is_none() && !query.is_empty() {
                let hit = command.title.to_lowercase().contains(&query)
                    || command.category.to_lowercase().contains(&query)
                    || command.id.to_lowercase().contains(&query)
                    || keys.iter().any(|k| k.to_lowercase().contains(&query));
                if !hit {
                    continue;
                }
            }
            if category != Some(&command.category) {
                category = Some(&command.category);
                rows.push(section_label(command.category.clone(), cx).into_any_element());
            }
            rows.push(self.row(command, keys, recording, registry, cx));
        }
        let empty = rows.is_empty();
        let overridden = registry.has_overrides();
        let path = registry.keys_path().display().to_string();
        let mut problems: Vec<String> = registry.last_problems().to_vec();
        if let Some(e) = &self.error {
            problems.insert(0, format!("Couldn't save: {e}"));
        }

        panel(p.panel, cx)
            .id("settings-keyboard")
            .test_support()
            .px(px(18.))
            .py(px(18.))
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Keyboard shortcuts"),
                            )
                            .child(
                                div()
                                    .mt(px(4.))
                                    .text_color(p.text_muted.hsla())
                                    .child("Every command and the keys that run it. Changes are saved to your keybindings file."),
                            ),
                    )
                    .when(overridden && !self.confirm_reset_all, |d| {
                        d.child(button("kb-reset-all", "Reset all", false, cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.confirm_reset_all = true;
                                cx.notify();
                            }),
                        ))
                    }),
            )
            .when(self.confirm_reset_all, |d| {
                d.child(
                    div()
                        .id("kb-reset-all-confirm-bar")
                        .test_support()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(14.))
                        .py(px(10.))
                        .rounded(px(12.))
                        .bg(p.surface.mix(p.rose, 0.14).hsla())
                        .border_1()
                        .border_color(p.rose.alpha(0.35))
                        .text_size(t.metrics.text_sm)
                        .child(
                            div()
                                .flex_1()
                                .child("Reset every shortcut to its default? Your own are removed."),
                        )
                        .child(button("kb-reset-all-confirm", "Reset all", true, cx).on_click(
                            cx.listener(|this, _, _, cx| this.reset_all(cx)),
                        ))
                        .child(button("kb-reset-all-cancel", "Cancel", false, cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.confirm_reset_all = false;
                                cx.notify();
                            }),
                        )),
                )
            })
            .when(!problems.is_empty(), |d| {
                d.child(
                    div()
                        .id("kb-problems")
                        .test_support()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .px(px(14.))
                        .py(px(10.))
                        .rounded(px(12.))
                        .bg(p.surface.mix(p.rose, 0.14).hsla())
                        .border_1()
                        .border_color(p.rose.alpha(0.35))
                        .text_size(t.metrics.text_sm)
                        .children(problems.into_iter().map(|line| div().child(line))),
                )
            })
            .child(
                div()
                    .id("kb-search")
                    .test_support()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(10.))
                    .bg(p.ground.hsla())
                    .text_size(t.metrics.text_sm)
                    .child(self.query.clone()),
            )
            .child(div().flex().flex_col().gap(px(2.)).children(rows))
            .when(empty, |d| {
                d.child(
                    div()
                        .id("kb-empty")
                        .test_support()
                        .py(px(10.))
                        .text_color(p.text_muted.hsla())
                        .child("No command or key matches"),
                )
            })
            .child(
                div()
                    .text_size(t.metrics.text_sm)
                    .font_family(t.metrics.font_mono.clone())
                    .text_color(p.text_subtle.hsla())
                    .child(path),
            )
    }
}

impl KeybindingsEditor {
    fn row(
        &self,
        command: &Command,
        keys: Vec<String>,
        recording: Option<&Recording>,
        registry: &CommandRegistry,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let p = &t.palette;
        let id = command.id.clone();
        let own = registry.is_overridden(&id);
        let tag = |label: &'static str, mine: bool| {
            div()
                .px(px(7.))
                .py(px(2.))
                .rounded(px(6.))
                .text_size(t.metrics.text_label)
                .font_weight(FontWeight::BOLD)
                .when(mine, |d| {
                    d.bg(p.surface.mix(p.sage, 0.25).hsla())
                        .text_color(p.text.hsla())
                })
                .when(!mine, |d| {
                    d.bg(p.surface_strong.hsla())
                        .text_color(p.text_muted.hsla())
                })
                .child(label)
        };

        let shown = div().flex().items_center().gap(px(6.)).flex_wrap();
        let shown = match recording {
            Some(r) => shown
                .child(
                    div()
                        .id(SharedString::from(format!("kb-recording-{id}")))
                        .test_support()
                        .text_size(t.metrics.text_sm)
                        .text_color(p.sage.hsla())
                        .child("Recording…"),
                )
                .children(
                    (!r.strokes.is_empty())
                        .then(|| kbd(keyfile::display(&r.strokes.join(" "), mac()), cx)),
                ),
            None if keys.is_empty() => shown.child(
                div()
                    .text_size(t.metrics.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child("Unbound"),
            ),
            None => shown.children(keys.into_iter().map(|k| kbd(k, cx))),
        };

        let actions = div().flex().items_center().gap(px(8.));
        let actions = match recording {
            Some(r) if r.conflicts.is_empty() => actions
                .child(
                    button(format!("kb-save-{id}"), "Save", true, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                )
                .child(
                    button(format!("kb-cancel-{id}"), "Cancel", false, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                ),
            Some(_) => actions,
            None => {
                let change_id = id.clone();
                let reset_id = id.clone();
                actions
                    .when(own, |d| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("kb-reset-{id}")))
                                .test_support()
                                .text_size(t.metrics.text_xs)
                                .text_color(p.text_subtle.hsla())
                                .cursor_pointer()
                                .child("Reset")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.reset(&reset_id, cx)),
                                ),
                        )
                    })
                    .child(
                        button(format!("kb-change-{id}"), "Change", false, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.record(change_id.clone(), window, cx)
                            }),
                        ),
                    )
            }
        };

        let conflict = recording.filter(|r| !r.conflicts.is_empty()).map(|r| {
            let titles: Vec<String> = r
                .conflicts
                .iter()
                .map(|c| title_of(c, cx).to_string())
                .collect();
            div()
                .id(SharedString::from(format!("kb-conflict-{id}")))
                .test_support()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(10.))
                .bg(p.surface.mix(p.rose, 0.14).hsla())
                .border_1()
                .border_color(p.rose.alpha(0.35))
                .text_size(t.metrics.text_sm)
                .child(div().flex_1().child(format!(
                    "{} is already used by {}",
                    keyfile::display(&r.strokes.join(" "), mac()),
                    titles.join(", ")
                )))
                .child(
                    button(format!("kb-replace-{id}"), "Replace", true, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.replace(cx))),
                )
                .child(
                    button(format!("kb-cancel-{id}"), "Cancel", false, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
        });

        div()
            .id(SharedString::from(format!("kb-row-{id}")))
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(12.))
            .py(px(8.))
            .rounded(px(12.))
            .when(recording.is_some(), |d| {
                d.bg(p.panel_raised.hsla())
                    .border_1()
                    .border_color(p.sage.alpha(0.5))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(div().child(command.title.clone()))
                            .child(
                                div()
                                    .text_size(t.metrics.text_xs)
                                    .font_family(t.metrics.font_mono.clone())
                                    .text_color(p.text_subtle.hsla())
                                    .child(id.clone()),
                            ),
                    )
                    .child(div().flex_1().min_w(px(0.)).child(shown))
                    .child(tag(if own { "YOURS" } else { "DEFAULT" }, own))
                    .child(actions),
            )
            .children(conflict)
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "keybindings_tests.rs"]
mod tests;
