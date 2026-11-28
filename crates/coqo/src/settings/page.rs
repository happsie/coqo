// SPDX-License-Identifier: Apache-2.0

//! The Settings tab (design board 8, `docs/design/settings.md`).
//!
//! A section list on the left and the chosen section on the right. The
//! sections are what mods contribute to the settings schema
//! (`settings/get`), rendered generically: a switch for a boolean, a
//! segmented choice for an enum, a text field for text, numbers and lists.
//! A scope switch at the top edits the user's `settings.toml` or the
//! window's workspace's `.coqo/settings.toml`.
//!
//! **Pages of their own** (the keyboard shortcuts editor, #354) join the
//! section list through [`register_page`]: an id, a title, an order and a
//! function that builds the page's view. Registering the same id again
//! replaces it. The "Keyboard shortcuts" page ([`KEYBOARD_PAGE`]) is
//! registered like this by `settings::keybindings`:
//!
//! ```ignore
//! settings::page::register_page(cx, CustomPage::new(
//!     settings::page::KEYBOARD_PAGE, "Keyboard shortcuts", 900,
//!     |window, cx| cx.new(|cx| KeymapEditor::new(window, cx)).into(),
//! ));
//! ```
//!
//! "Open settings" in the palette and `Ctrl/Cmd+,` open the tab, or bring
//! it forward; [`OpenSettingsAt`] opens it at a section.

use std::collections::HashMap;
use std::rc::Rc;

use coqo_protocol::methods::settings::{
    FieldInfo, FieldKind, FieldScope, FileSettings, GetResult, SectionInfo,
};
use coqo_protocol::methods::workspace::{WorkspaceId, WorkspaceInfo};
use coqo_ui::{
    ActiveTokens as _,
    dock::{AnyTab, TabItem, register_tab_kind},
    gpui::{
        AnyElement, AnyView, App, AppContext as _, Context, Entity, FocusHandle, FontWeight,
        Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
        prelude::FluentBuilder as _, px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{Icon, TestSupportExt as _, panel, raised},
};
use serde_json::Value;

use super::{Settings, get, set};
use crate::workspace::Workspaces;

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

/// The Settings tab's kind. Persisted: never change it.
pub const KIND: &str = "settings";

/// The id of the keyboard shortcuts page in the section list.
pub const KEYBOARD_PAGE: &str = "keyboard";

coqo_ui::gpui::actions!(
    settings,
    [
        /// Open the Settings tab, or bring it forward.
        OpenSettings,
    ]
);

/// Open the Settings tab at a section (a schema section's id, or a
/// [`CustomPage`]'s).
#[derive(Clone, Debug, PartialEq, Eq, gpui::Action)]
#[action(namespace = settings, no_json)]
pub struct OpenSettingsAt {
    pub section: SharedString,
}

#[cfg(target_os = "macos")]
const OPEN_KEY: &str = "cmd-,";
#[cfg(not(target_os = "macos"))]
const OPEN_KEY: &str = "ctrl-,";

/// Register the tab kind, the command and its key, and the keyboard
/// shortcuts page. Called by `shell::init`.
pub fn init(cx: &mut App) {
    crate::commands::register(
        cx,
        [crate::commands::Command::new(
            "settings.open",
            "Open settings",
            "Preferences",
            OpenSettings,
        )
        .key(OPEN_KEY)],
    );
    register_tab_kind(cx, KIND, |_, window, cx| Some(new(window, cx)));
    super::keybindings::register(cx);
}

/// A new Settings tab.
pub fn new(window: &mut Window, cx: &mut App) -> AnyTab {
    AnyTab::new(cx.new(|cx| SettingsPage::new(window, cx)))
}

// ---- pages of their own -----------------------------------------------------

/// Builds a [`CustomPage`]'s view.
pub type BuildPage = Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;

/// A page of the section list that is not a schema section: it renders
/// itself.
#[derive(Clone)]
pub struct CustomPage {
    /// Unique among the pages and the schema's sections.
    pub id: SharedString,
    /// The title in the section list.
    pub title: SharedString,
    /// Where it goes among the schema's sections: those have the order their
    /// mods gave them (Appearance 10, Files 20, Claude Code 30; 1000 for one
    /// without), lowest first.
    pub order: i32,
    /// Builds its view, once per Settings tab.
    pub build: BuildPage,
}

impl std::fmt::Debug for CustomPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomPage")
            .field("id", &self.id)
            .field("order", &self.order)
            .finish_non_exhaustive()
    }
}

impl CustomPage {
    /// A page `id` titled `title` at `order`, built by `build`.
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        order: i32,
        build: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            order,
            build: Rc::new(build),
        }
    }
}

#[derive(Default)]
struct CustomPages(Vec<CustomPage>);
impl Global for CustomPages {}

/// Add `page` to the section list, replacing a page with the same id.
pub fn register_page(cx: &mut App, page: CustomPage) {
    let pages = &mut cx.default_global::<CustomPages>().0;
    pages.retain(|p| p.id != page.id);
    pages.push(page);
}

/// How long a text field waits after the last keystroke to save.
const SAVE_PAUSE: std::time::Duration = std::time::Duration::from_millis(700);

/// The section order of a schema section without one.
const DEFAULT_ORDER: i32 = 1000;

/// What the Settings tab's section list holds.
#[derive(Clone, Debug, PartialEq)]
pub enum NavEntry {
    /// A schema section.
    Section {
        id: SharedString,
        title: SharedString,
    },
    /// A page of its own.
    Page {
        id: SharedString,
        title: SharedString,
    },
}

impl NavEntry {
    /// The entry's id.
    pub fn id(&self) -> &SharedString {
        match self {
            NavEntry::Section { id, .. } | NavEntry::Page { id, .. } => id,
        }
    }

    fn title(&self) -> &SharedString {
        match self {
            NavEntry::Section { title, .. } | NavEntry::Page { title, .. } => title,
        }
    }
}

/// The section list: the schema's sections in the daemon's order, with the
/// custom pages placed among them by their order (a section without one
/// ranks as [`DEFAULT_ORDER`]).
pub fn nav_entries(snapshot: Option<&GetResult>, cx: &App) -> Vec<NavEntry> {
    let mut entries: Vec<(i32, usize, NavEntry)> = Vec::new();
    if let Some(s) = snapshot {
        for (ix, section) in s.sections.iter().enumerate() {
            entries.push((
                section.order.unwrap_or(DEFAULT_ORDER),
                ix,
                NavEntry::Section {
                    id: section.id.clone().into(),
                    title: section.title.clone().into(),
                },
            ));
        }
    }
    if let Some(pages) = cx.try_global::<CustomPages>() {
        for (ix, page) in pages.0.iter().enumerate() {
            entries.push((
                page.order,
                usize::MAX / 2 + ix,
                NavEntry::Page {
                    id: page.id.clone(),
                    title: page.title.clone(),
                },
            ));
        }
    }
    entries.sort_by_key(|(order, ix, _)| (*order, *ix));
    entries.into_iter().map(|(_, _, e)| e).collect()
}

// ---- the tab --------------------------------------------------------------

/// Which file the page edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The user's `settings.toml`.
    User,
    /// The window's workspace's `.coqo/settings.toml`.
    Workspace,
}

/// The Settings tab. See the module docs.
pub struct SettingsPage {
    focus: FocusHandle,
    selected: Option<SharedString>,
    scope: Scope,
    workspaces: Option<Entity<Workspaces>>,
    /// Text fields by (scope, key).
    inputs: HashMap<(bool, String), Entity<PromptInput>>,
    /// What a text field holds that is not a valid value, by key.
    input_errors: HashMap<String, String>,
    /// Built custom pages, by id.
    pages: HashMap<SharedString, AnyView>,
    /// A text field's save, waiting for the typing to pause, by key.
    typing: HashMap<String, Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for SettingsPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsPage")
            .field("selected", &self.selected)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl SettingsPage {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = vec![cx.observe_global::<Settings>(|_, cx| cx.notify())];
        let workspaces = Workspaces::for_window(window, cx);
        if let Some(w) = &workspaces {
            subscriptions.push(cx.observe(w, |_, _, cx| cx.notify()));
        }
        if let Some(sync) = super::SettingsSync::global(cx) {
            sync.update(cx, |sync, cx| sync.fetch(cx));
        }
        Self {
            focus: cx.focus_handle(),
            selected: None,
            scope: Scope::User,
            workspaces,
            inputs: HashMap::new(),
            input_errors: HashMap::new(),
            pages: HashMap::new(),
            typing: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The section shown.
    pub fn selected(&self, cx: &App) -> Option<SharedString> {
        let entries = nav_entries(get(cx).snapshot(), cx);
        self.selected
            .clone()
            .filter(|s| entries.iter().any(|e| e.id() == s))
            .or_else(|| entries.first().map(|e| e.id().clone()))
    }

    /// Show section `id`.
    pub fn select(&mut self, id: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.selected = Some(id.into());
        cx.notify();
    }

    /// A page for screenshots, without a window's workspaces.
    #[cfg(test)]
    pub fn new_for_tests(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut page = Self::new(window, cx);
        page.workspaces = None;
        page
    }

    /// Which file the page edits.
    #[cfg(test)]
    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// Edit the user's file or the workspace's.
    pub fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        cx.notify();
    }

    fn workspace(&self, cx: &App) -> Option<WorkspaceInfo> {
        self.workspaces.as_ref()?.read(cx).current().cloned()
    }

    /// The workspace the page writes to: `None` for the user's file.
    fn target(&self, cx: &App) -> Option<WorkspaceId> {
        match self.scope {
            Scope::User => None,
            Scope::Workspace => self.workspace(cx).map(|w| w.id),
        }
    }

    /// Save a text field's `text` as `key`'s value, if it is one.
    fn commit(&mut self, key: &str, kind: FieldKind, text: &str, cx: &mut Context<Self>) {
        match parse_text(kind, text) {
            Ok(value) => {
                let file = self.file(cx);
                if file.as_ref().and_then(|f| f.values.get(key)) != Some(&value) {
                    self.write(key, Some(value), cx);
                }
            }
            Err(e) => {
                self.input_errors.insert(key.to_owned(), e);
                cx.notify();
            }
        }
    }

    /// The file the page edits, as the daemon last sent it.
    fn file(&self, cx: &App) -> Option<FileSettings> {
        let settings = get(cx);
        let snapshot = settings.snapshot()?;
        match self.target(cx) {
            None => Some(snapshot.user.clone()),
            Some(w) => snapshot.workspace(&w).cloned(),
        }
    }

    fn write(&mut self, key: &str, value: Option<Value>, cx: &mut Context<Self>) {
        self.input_errors.remove(key);
        let target = self.target(cx);
        set(cx, key, value, target);
        cx.notify();
    }

    /// The field's text input for the current scope, made on first use.
    fn input(
        &mut self,
        field: &FieldInfo,
        current: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<PromptInput> {
        let slot = (self.scope == Scope::Workspace, field.key.clone());
        if let Some(input) = self.inputs.get(&slot) {
            return input.clone();
        }
        let placeholder = match field.kind {
            FieldKind::StringList => "Comma-separated",
            FieldKind::Integer | FieldKind::Number => "A number",
            _ => "",
        };
        let text = value_text(current);
        let input = cx.new(|cx| {
            let mut input = PromptInput::new(placeholder, 1, 1, window, cx);
            input.set_text(text, window, cx);
            input
        });
        let key = field.key.clone();
        let kind = field.kind;
        let sub = cx.subscribe(&input, move |this, input, event: &PromptInputEvent, cx| {
            match event {
                PromptInputEvent::Submit => {
                    this.typing.remove(&key);
                    this.commit(&key, kind, &input.read(cx).text(cx), cx);
                }
                // Saved once the typing pauses.
                PromptInputEvent::Changed => {
                    let waiting = key.clone();
                    let input = input.downgrade();
                    let pause = cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(SAVE_PAUSE).await;
                        let Some(input) = input.upgrade() else {
                            return;
                        };
                        let text = cx.update(|cx| input.read(cx).text(cx));
                        this.update(cx, |this, cx| {
                            this.typing.remove(&waiting);
                            this.commit(&waiting, kind, &text, cx);
                        })
                        .ok();
                    });
                    this.typing.insert(key.clone(), pause);
                }
                _ => {}
            }
        });
        self._subscriptions.push(sub);
        self.inputs.insert(slot, input.clone());
        input
    }

    fn custom_page(
        &mut self,
        id: &SharedString,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyView> {
        if let Some(view) = self.pages.get(id) {
            return Some(view.clone());
        }
        let build = cx
            .try_global::<CustomPages>()?
            .0
            .iter()
            .find(|p| &p.id == id)?
            .build
            .clone();
        let view = build(window, cx);
        self.pages.insert(id.clone(), view.clone());
        Some(view)
    }
}

impl TabItem for SettingsPage {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        "Settings".into()
    }

    fn icon(&self, _: &App) -> Option<Icon> {
        Some(Icon::Settings)
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(KIND.into())
    }

    fn focus_handle(&self, _: &App) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }
}

// ---- values -----------------------------------------------------------------

/// A value as its text field shows it.
pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A text field's text as a value of `kind`.
pub fn parse_text(kind: FieldKind, text: &str) -> Result<Value, String> {
    let text = text.trim();
    match kind {
        FieldKind::Integer => text
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "Enter a whole number.".to_owned()),
        FieldKind::Number => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "Enter a number.".to_owned()),
        FieldKind::StringList => Ok(Value::Array(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        )),
        FieldKind::Boolean => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err("Enter true or false.".to_owned()),
        },
        FieldKind::String | FieldKind::Enum => Ok(Value::String(text.to_owned())),
    }
}

/// "system" → "System", "on-save" → "On save".
pub fn choice_label(value: &str) -> String {
    let text = value.replace(['-', '_'], " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---- rendering ----------------------------------------------------------------

/// A switch: a 42 × 24 track, the knob right and sage when on (board 8).
fn switch(id: SharedString, on: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let p = &cx.tokens().palette;
    div()
        .id(id)
        .relative()
        .w(px(42.))
        .h(px(24.))
        .flex_none()
        .rounded(px(12.))
        .cursor_pointer()
        .bg(if on {
            p.sage.hsla()
        } else {
            p.border_strong.hsla()
        })
        .child(
            div()
                .absolute()
                .top(px(3.))
                .when(on, |k| k.right(px(3.)))
                .when(!on, |k| k.left(px(3.)))
                .size(px(18.))
                .rounded_full()
                .bg(if on {
                    p.ground.hsla()
                } else {
                    p.text_muted.hsla()
                }),
        )
}

/// A segmented choice's frame: an inset track (board 8's "Mode"), on a
/// card; [`segmented_on_ground`] off one.
fn segmented(cx: &App) -> gpui::Div {
    segmented_track(cx.tokens().palette.ground, cx)
}

fn segmented_on_ground(cx: &App) -> gpui::Div {
    segmented_track(cx.tokens().palette.panel, cx)
}

fn segmented_track(fill: coqo_ui::Rgb, _cx: &App) -> gpui::Div {
    let p = fill;
    div()
        .flex()
        .flex_wrap()
        .p(px(3.))
        .gap(px(2.))
        .rounded(px(12.))
        .bg(p.hsla())
}

fn segment(id: SharedString, label: String, selected: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    let t = cx.tokens();
    let p = &t.palette;
    let base = div()
        .px(px(12.))
        .py(px(5.))
        .rounded(px(9.))
        .text_size(t.metrics.text_sm)
        .cursor_pointer();
    let base = if selected {
        raised(base, cx).text_color(p.text.hsla())
    } else {
        base.border_1()
            .border_color(gpui::transparent_black())
            .text_color(p.text_muted.hsla())
    };
    base.id(id).child(label)
}

fn problem_banner(
    id: &'static str,
    lines: Vec<String>,
    note: Option<&str>,
    cx: &App,
) -> AnyElement {
    let t = cx.tokens();
    let p = &t.palette;
    div()
        .id(id)
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
        .text_color(p.text.hsla())
        .children(lines.into_iter().map(|l| div().child(l)))
        .when_some(note, |d, note| {
            d.child(div().text_color(p.text_muted.hsla()).child(note.to_owned()))
        })
        .into_any_element()
}

impl SettingsPage {
    fn render_nav(
        &self,
        entries: &[NavEntry],
        selected: Option<&SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let p = &t.palette;
        let items = entries.iter().map(|entry| {
            let id = entry.id().clone();
            let on = selected == Some(&id);
            let base = div()
                .px(px(10.))
                .py(px(8.))
                .rounded(px(10.))
                .text_size(t.metrics.text_md)
                .cursor_pointer();
            let base = if on {
                raised(base, cx)
                    .text_color(p.text.hsla())
                    .font_weight(FontWeight::SEMIBOLD)
            } else {
                base.border_1()
                    .border_color(gpui::transparent_black())
                    .text_color(p.text_muted.hsla())
            };
            base.id(SharedString::from(format!("settings-nav-{id}")))
                .test_support()
                .child(entry.title().clone())
                .on_click(cx.listener(move |this, _, _, cx| this.select(id.clone(), cx)))
        });
        panel(p.panel, cx)
            .w(px(240.))
            .flex_none()
            .h_full()
            .px(px(10.))
            .py(px(14.))
            .gap(px(2.))
            .child(
                div()
                    .px(px(10.))
                    .pt(px(4.))
                    .pb(px(12.))
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text.hsla())
                    .child("Settings"),
            )
            .children(items)
            .child(div().flex_1())
            .child(
                div()
                    .p(px(10.))
                    .text_size(t.metrics.text_xs)
                    .text_color(p.text_subtle.hsla())
                    .child("Workspace overrides are stored in ")
                    .child(
                        div()
                            .font_family(t.metrics.font_mono.clone())
                            .child(".coqo/settings.toml"),
                    ),
            )
            .into_any_element()
    }

    fn render_scope(
        &self,
        workspace: Option<&WorkspaceInfo>,
        file: Option<&FileSettings>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let p = &t.palette;
        let user = segment(
            "settings-scope-user".into(),
            "User".into(),
            self.scope == Scope::User,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| this.set_scope(Scope::User, cx)))
        .test_support();
        let ws_label = match workspace {
            Some(w) => format!("Workspace · {}", w.name),
            None => "Workspace".to_owned(),
        };
        let ws = segment(
            "settings-scope-workspace".into(),
            ws_label,
            self.scope == Scope::Workspace,
            cx,
        )
        .when(workspace.is_some(), |s| {
            s.on_click(cx.listener(|this, _, _, cx| this.set_scope(Scope::Workspace, cx)))
        })
        .when(workspace.is_none(), |s| s.opacity(0.5).cursor_default())
        .test_support();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.))
            .child(segmented_on_ground(cx).child(user).child(ws))
            .child(div().flex_1())
            .when_some(file, |d, file| {
                d.child(
                    div()
                        .text_size(t.metrics.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .font_family(t.metrics.font_mono.clone())
                        .child(file.path.clone()),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_field(
        &mut self,
        field: &FieldInfo,
        snapshot: &GetResult,
        file: Option<&FileSettings>,
        workspace: Option<&WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let own = file.and_then(|f| f.values.get(&field.key)).cloned();
        let current = own.clone().unwrap_or_else(|| {
            snapshot
                .effective(&field.key, None)
                .unwrap_or_else(|| field.default.clone())
        });
        let problem = file
            .and_then(|f| {
                f.problems
                    .iter()
                    .find(|p| p.key.as_deref() == Some(&field.key))
            })
            .map(|p| p.message.clone())
            .or_else(|| self.input_errors.get(&field.key).cloned());
        let key = field.key.clone();
        let control: AnyElement = match field.kind {
            FieldKind::Boolean => {
                let on = current.as_bool().unwrap_or(false);
                switch(format!("setting-{key}").into(), on, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.write(&key, Some(Value::Bool(!on)), cx)
                    }))
                    .test_support()
                    .into_any_element()
            }
            FieldKind::Enum => {
                let chosen = current.as_str().unwrap_or_default().to_owned();
                let options: Vec<_> = field
                    .values
                    .iter()
                    .map(|v| {
                        let value = v.clone();
                        let key = key.clone();
                        segment(
                            format!("setting-{}-{v}", field.key).into(),
                            choice_label(v),
                            *v == chosen,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.write(&key, Some(Value::String(value.clone())), cx)
                        }))
                        .test_support()
                    })
                    .collect();
                segmented(cx).children(options).into_any_element()
            }
            _ => {
                let input = self.input(field, &current, window, cx);
                let t = cx.tokens();
                div()
                    .id(SharedString::from(format!("setting-{key}")))
                    .test_support()
                    .w(px(260.))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(9.))
                    .bg(t.palette.ground.hsla())
                    .text_size(t.metrics.text_sm)
                    .child(input)
                    .into_any_element()
            }
        };
        let t = cx.tokens();
        let p = &t.palette;
        let inherited = workspace.is_some() && own.is_none();
        let reset = own.is_some().then(|| {
            let key = field.key.clone();
            div()
                .id(SharedString::from(format!("setting-reset-{key}")))
                .test_support()
                .text_size(t.metrics.text_xs)
                .text_color(p.text_subtle.hsla())
                .cursor_pointer()
                .child("Reset")
                .on_click(cx.listener(move |this, _, _, cx| this.write(&key, None, cx)))
        });
        let user_only = workspace.is_some() && field.scope == FieldScope::User;
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .py(px(10.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(220.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_color(p.text.hsla()).child(field.title.clone()))
                            .when(!field.description.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(t.metrics.text_sm)
                                        .text_color(p.text_muted.hsla())
                                        .child(field.description.clone()),
                                )
                            })
                            .when(inherited && !user_only, |d| {
                                d.child(
                                    div()
                                        .text_size(t.metrics.text_xs)
                                        .text_color(p.text_subtle.hsla())
                                        .child("From your user settings"),
                                )
                            })
                            .when(user_only, |d| {
                                d.child(
                                    div()
                                        .text_size(t.metrics.text_xs)
                                        .text_color(p.text_subtle.hsla())
                                        .child("Only in your user settings"),
                                )
                            }),
                    )
                    .children(reset)
                    .when(!user_only, |d| d.child(control)),
            )
            .when_some(problem, |d, problem| {
                d.child(
                    div()
                        .text_size(t.metrics.text_sm)
                        .text_color(p.rose.hsla())
                        .child(problem),
                )
            })
            .into_any_element()
    }

    fn render_section(
        &mut self,
        section: &SectionInfo,
        snapshot: &GetResult,
        workspace: Option<&WorkspaceInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = match self.scope {
            Scope::User => None,
            Scope::Workspace => workspace.map(|w| w.id.clone()),
        };
        let file = match &target {
            None => Some(snapshot.user.clone()),
            Some(w) => snapshot.workspace(w).cloned(),
        };
        let rows: Vec<AnyElement> = section
            .fields
            .iter()
            .map(|f| self.render_field(f, snapshot, file.as_ref(), target.as_ref(), window, cx))
            .collect();
        let t = cx.tokens();
        let p = &t.palette;
        panel(p.panel, cx)
            .px(px(18.))
            .py(px(18.))
            .gap(px(4.))
            .child(
                div()
                    .pb(px(6.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child(section.title.clone()),
                    )
                    .when_some(section.description.clone(), |d, desc| {
                        d.child(div().mt(px(4.)).text_color(p.text_muted.hsla()).child(desc))
                    }),
            )
            .children(rows)
            .into_any_element()
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = get(cx);
        let snapshot = settings.snapshot().cloned();
        let entries = nav_entries(snapshot.as_ref(), cx);
        let selected = self.selected(cx);
        let workspace = self.workspace(cx);
        if workspace.is_none() && self.scope == Scope::Workspace {
            self.scope = Scope::User;
        }
        let nav = self.render_nav(&entries, selected.as_ref(), cx);
        let file = snapshot.as_ref().and_then(|s| match self.scope {
            Scope::User => Some(s.user.clone()),
            Scope::Workspace => s.workspace(&workspace.as_ref()?.id).cloned(),
        });
        let mut main: Vec<AnyElement> = Vec::new();
        let page = selected
            .as_ref()
            .and_then(|id| entries.iter().find(|e| e.id() == id).cloned());
        let is_section = matches!(page, Some(NavEntry::Section { .. }));
        if is_section || page.is_none() {
            main.push(self.render_scope(workspace.as_ref(), file.as_ref(), cx));
        }
        if let Some(error) = settings.error() {
            main.push(problem_banner(
                "settings-error",
                vec![format!("Couldn't save: {error}")],
                None,
                cx,
            ));
        }
        if let Some(file) = &file {
            let known = |key: &str| snapshot.as_ref().is_some_and(|s| s.field(key).is_some());
            let lines: Vec<String> = file
                .problems
                .iter()
                .filter(|p| p.key.as_deref().is_none_or(|k| !known(k)))
                .map(|p| match (p.line, &p.key) {
                    (Some(line), Some(_)) => format!("Line {line}: {}", p.message),
                    _ => p.message.clone(),
                })
                .collect();
            if !lines.is_empty() {
                let broken = file.problems.iter().any(|p| p.key.is_none());
                main.push(problem_banner(
                    "settings-problems",
                    lines,
                    broken.then_some("Coqo keeps using the values from before the error."),
                    cx,
                ));
            }
        }
        match (page, &snapshot) {
            (Some(NavEntry::Section { id, .. }), Some(snapshot)) => {
                if let Some(section) = snapshot
                    .sections
                    .iter()
                    .find(|s| s.id.as_str() == id.as_ref())
                {
                    let section = section.clone();
                    main.push(self.render_section(
                        &section,
                        snapshot,
                        workspace.as_ref(),
                        window,
                        cx,
                    ));
                }
            }
            (Some(NavEntry::Page { id, .. }), _) => {
                if let Some(view) = self.custom_page(&id, window, cx) {
                    main.push(div().size_full().child(view).into_any_element());
                }
            }
            _ => {
                let p = &cx.tokens().palette;
                main.push(
                    div()
                        .p(px(18.))
                        .text_color(p.text_muted.hsla())
                        .child("Waiting for Coqo's daemon…")
                        .into_any_element(),
                );
            }
        }
        let p = &cx.tokens().palette;
        div()
            .id("settings-page")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .gap(px(10.))
            .p(px(10.))
            .bg(p.ground.hsla())
            .text_color(p.text.hsla())
            .text_size(cx.tokens().metrics.text_md)
            .child(nav)
            .child(
                div()
                    .id("settings-main")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .children(main),
            )
    }
}
