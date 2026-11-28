// SPDX-License-Identifier: Apache-2.0

//! The command palette (board 1's command search): a field and a list of
//! what matches, from one **provider** at a time.
//!
//! A provider is a mode. It is picked by the field's first character: `>`
//! is the commands ([`super::CommandsProvider`]), and a provider registered
//! with the empty prefix is the default mode (the file finder, #162).
//! Until one is, the commands serve the default mode too. `@` (symbols) and
//! `#` are free for later providers:
//!
//! ```ignore
//! palette::register_provider(cx, Rc::new(FilesProvider::new()));   // prefix ""
//! palette::register_provider(cx, Rc::new(SymbolsProvider::new())); // prefix "@"
//! ```
//!
//! A provider answers a query with a [`Task`] of [`PaletteItem`]s (the
//! daemon can do the work) and runs the item the user confirms, with the
//! [`PaletteScope`]: the window, where the focus was before the palette
//! opened, and the dock to open tabs in.
//!
//! Keys: ↑/↓ move, Enter runs, Esc closes. The field is a text field, so no
//! plain-key shortcut fires while typing in it (#266).

use std::{any::TypeId, ops::Range, rc::Rc};

use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_ui::{
    ActiveTokens as _,
    dock::Dock,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle,
        Focusable as _, FontWeight, Global, HighlightStyle, Hsla, InteractiveElement as _,
        IntoElement, KeyContext, ParentElement as _, Render, ScrollHandle, SharedString,
        StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, Task, WeakEntity,
        Window, div, px,
    },
    input::{PromptInput, PromptInputEvent, PromptKey},
    widgets::{FluentBuilder as _, Icon, TestSupportExt as _, icon, kbd, popover},
};

/// The palette's key context.
pub const KEY_CONTEXT: &str = "CommandPalette";

/// The palette is as wide as the top bar's command search, but not narrower
/// than this.
const MIN_WIDTH: f32 = 420.;
/// How tall the list gets before it scrolls.
const LIST_MAX: f32 = 380.;

/// Where the palette was opened from: what a provider needs to run an item.
#[derive(Clone)]
pub struct PaletteScope {
    /// The window.
    #[allow(dead_code)] // For providers that open things (the file finder, #162).
    pub window: AnyWindowHandle,
    /// What had the focus before the palette took it. Commands run there.
    pub focus: Option<FocusHandle>,
    /// The key contexts from the window down to that focus.
    pub context: Vec<KeyContext>,
    /// The types of the actions that focus path (and the app) handles.
    pub actions: Vec<TypeId>,
    /// The window's dock, to open tabs in.
    #[allow(dead_code)] // For providers that open tabs (the file finder, #162).
    pub dock: WeakEntity<Dock>,
    /// The window's workspace, if one is open.
    pub workspace: Option<WorkspaceId>,
}

impl std::fmt::Debug for PaletteScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaletteScope")
            .field("context", &self.context)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl PaletteScope {
    /// The scope of `window` now: its focus (or `fallback` without one),
    /// that focus path's contexts and the actions it handles.
    pub fn capture(
        fallback: Option<FocusHandle>,
        dock: WeakEntity<Dock>,
        workspace: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        if window.focused(cx).is_none()
            && let Some(fallback) = &fallback
        {
            window.focus(fallback, cx);
        }
        let mut actions: Vec<TypeId> = window
            .available_actions(cx)
            .iter()
            .map(|a| a.as_any().type_id())
            .collect();
        actions.sort();
        actions.dedup();
        Self {
            window: window.window_handle(),
            focus: window.focused(cx),
            context: window.context_stack(),
            actions,
            dock,
            workspace,
        }
    }

    /// Whether an action of type `action` would be handled where the focus
    /// was.
    pub fn handles(&self, action: TypeId) -> bool {
        self.actions.binary_search(&action).is_ok()
    }
}

/// One row of the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    /// The provider's key for it (a command id, a path).
    pub id: SharedString,
    /// The main text.
    pub title: SharedString,
    /// Byte offsets of the matched characters in `title`.
    pub highlights: Vec<usize>,
    /// Shown before the title, quieter (a command's category).
    pub prefix: Option<SharedString>,
    /// Shown after the title, quieter (a file's folder).
    pub detail: Option<SharedString>,
    /// Its keys, as shown (`Ctrl Shift P`).
    pub keys: Option<SharedString>,
    pub icon: Option<Icon>,
    /// Recently used: the list labels it.
    pub recent: bool,
}

impl PaletteItem {
    pub fn new(id: impl Into<SharedString>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            highlights: Vec::new(),
            prefix: None,
            detail: None,
            keys: None,
            icon: None,
            recent: false,
        }
    }
}

/// A palette mode: see the module docs.
pub trait PaletteProvider: 'static {
    /// The field's first characters that pick this mode: `>`, `@`, or `""`
    /// for the default mode.
    fn prefix(&self) -> &'static str;
    /// What the empty field says when this is the default mode.
    fn placeholder(&self) -> SharedString;
    /// What the list says when nothing matches.
    fn empty_text(&self) -> SharedString {
        "Nothing matches".into()
    }
    /// The items for `query` (the field without the prefix, trimmed), best
    /// first.
    fn search(&self, query: &str, scope: &PaletteScope, cx: &mut App) -> Task<Vec<PaletteItem>>;
    /// Run `item`. The palette has closed and the focus is back where it
    /// was.
    fn confirm(&self, item: &PaletteItem, scope: &PaletteScope, window: &mut Window, cx: &mut App);
}

/// The registered providers.
#[derive(Default)]
pub struct PaletteProviders(Vec<Rc<dyn PaletteProvider>>);

impl Global for PaletteProviders {}

impl std::fmt::Debug for PaletteProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|p| p.prefix()))
            .finish()
    }
}

/// Add `provider`, replacing the one with its prefix.
pub fn register_provider(cx: &mut App, provider: Rc<dyn PaletteProvider>) {
    let providers = cx.default_global::<PaletteProviders>();
    providers.0.retain(|p| p.prefix() != provider.prefix());
    providers.0.push(provider);
}

/// The provider for the field's `text`, and the query: the longest
/// registered prefix `text` starts with, else the default mode, else the
/// commands (`>`).
pub fn provider_for(text: &str, cx: &App) -> Option<(Rc<dyn PaletteProvider>, String)> {
    let providers = &cx.try_global::<PaletteProviders>()?.0;
    let prefixed = providers
        .iter()
        .filter(|p| !p.prefix().is_empty() && text.starts_with(p.prefix()))
        .max_by_key(|p| p.prefix().len());
    let provider = prefixed
        .or_else(|| providers.iter().find(|p| p.prefix().is_empty()))
        .or_else(|| {
            providers
                .iter()
                .find(|p| p.prefix() == super::COMMANDS_PREFIX)
        })?;
    let query = text.strip_prefix(provider.prefix()).unwrap_or(text);
    Some((Rc::clone(provider), query.trim().to_owned()))
}

/// Emitted by the [`Palette`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteEvent {
    /// Closed: by Esc, a click outside, or running an item.
    Closed,
}

/// The palette view. The workbench shows it over the window while open.
pub struct Palette {
    input: Entity<PromptInput>,
    scope: PaletteScope,
    provider: Option<Rc<dyn PaletteProvider>>,
    /// What the list shows, for `query`.
    items: Vec<PaletteItem>,
    query: String,
    selected: usize,
    scroll: ScrollHandle,
    searching: Option<Task<()>>,
    closed: bool,
    _events: Subscription,
}

impl std::fmt::Debug for Palette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Palette")
            .field("query", &self.query)
            .field("items", &self.items.len())
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    /// A palette for `scope` with `text` in its field (`>` for the
    /// commands). Focus it with [`Palette::focus`].
    pub fn new(
        scope: PaletteScope,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The default mode's placeholder: the field is empty only there.
        let placeholder = provider_for("", cx)
            .map(|(p, _)| p.placeholder())
            .unwrap_or_else(|| "Search".into());
        let input = cx.new(|cx| {
            let mut input = PromptInput::new(placeholder, 1, 1, window, cx);
            input.set_capture(true);
            input
        });
        let events = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            PromptInputEvent::Changed => this.search(cx),
            PromptInputEvent::Key(PromptKey::Up) => this.move_selection(-1, cx),
            PromptInputEvent::Key(PromptKey::Down) => this.move_selection(1, cx),
            PromptInputEvent::Key(PromptKey::Accept) | PromptInputEvent::Submit => {
                this.confirm(window, cx)
            }
            PromptInputEvent::Key(PromptKey::Dismiss) | PromptInputEvent::Cancel => {
                this.close(window, cx)
            }
            PromptInputEvent::Key(PromptKey::Complete) => {}
        });
        let mut this = Self {
            input,
            scope,
            provider: None,
            items: Vec::new(),
            query: String::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            searching: None,
            closed: false,
            _events: events,
        };
        this.set_text(text, window, cx);
        this
    }

    /// Put the keyboard in the field.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let focus = self.input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// Whether the field has the keyboard.
    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    }

    /// Replace the field's text (and search again).
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_text(text.to_owned(), window, cx));
        self.search(cx);
    }

    /// The field's text.
    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).text(cx)
    }

    /// What the list shows.
    #[cfg(test)]
    pub fn items(&self) -> &[PaletteItem] {
        &self.items
    }

    /// The selected row.
    pub fn selected(&self) -> Option<&PaletteItem> {
        self.items.get(self.selected)
    }

    /// The mode's prefix (`>`, `""`), if a provider serves the text.
    #[cfg(test)]
    pub fn mode(&self) -> Option<&'static str> {
        self.provider.as_ref().map(|p| p.prefix())
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let Some((provider, query)) = provider_for(&text, cx) else {
            self.provider = None;
            self.items.clear();
            cx.notify();
            return;
        };
        let task = provider.search(&query, &self.scope, cx);
        self.provider = Some(provider);
        self.searching = Some(cx.spawn(async move |this, cx| {
            let items = task.await;
            this.update(cx, |this, cx| {
                this.items = items;
                this.query = query;
                this.selected = 0;
                this.scroll.scroll_to_item(0);
                this.searching = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn move_selection(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as isize;
        self.selected = (self.selected as isize + by).rem_euclid(n) as usize;
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    /// Select row `ix`.
    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.items.len() {
            self.selected = ix;
            cx.notify();
        }
    }

    /// Run the selected item: close, then hand it to its provider.
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(item), Some(provider)) = (self.selected().cloned(), self.provider.clone()) else {
            return;
        };
        let scope = self.scope.clone();
        self.close(window, cx);
        provider.confirm(&item, &scope, window, cx);
    }

    /// Close: give the focus back to where it was.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        if self.is_focused(window, cx)
            && let Some(focus) = &self.scope.focus
        {
            window.focus(focus, cx);
        }
        cx.emit(PaletteEvent::Closed);
    }
}

/// `text` with the bytes at `positions` in `color`, semibold.
fn highlighted(text: &str, positions: &[usize], color: Hsla) -> StyledText {
    let style = HighlightStyle {
        color: Some(color),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let ranges: Vec<(Range<usize>, HighlightStyle)> = positions
        .iter()
        .filter_map(|&p| {
            let len = text.get(p..)?.chars().next()?.len_utf8();
            Some((p..p + len, style))
        })
        .collect();
    StyledText::new(text.to_owned()).with_highlights(ranges)
}

impl Palette {
    fn render_row(
        &self,
        ix: usize,
        item: &PaletteItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let selected = ix == self.selected;
        div()
            .id(("palette-item", ix))
            .test_support()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .py(px(7.))
            .rounded(m.radius_control)
            .cursor_pointer()
            .when(selected, |row| row.bg(p.surface_selected.hsla()))
            .when(!selected, |row| {
                row.hover(|row| row.bg(p.surface_strong.hsla()))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select(ix, cx);
                this.confirm(window, cx);
            }))
            .children(item.icon.map(|i| icon(i, px(14.), p.text_muted.hsla())))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_baseline()
                    .gap(px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .children(item.prefix.clone().map(|prefix| {
                        div()
                            .flex_none()
                            .text_color(p.text_subtle.hsla())
                            .child(SharedString::from(format!("{prefix}:")))
                    }))
                    .child(div().text_color(p.text.hsla()).child(highlighted(
                        &item.title,
                        &item.highlights,
                        p.sage.hsla(),
                    )))
                    .children(item.detail.clone().map(|detail| {
                        div()
                            .min_w_0()
                            .text_size(m.text_sm)
                            .text_color(p.text_subtle.hsla())
                            .child(detail)
                    })),
            )
            .when(item.recent, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(m.text_xs)
                        .text_color(p.text_subtle.hsla())
                        .child("recently used"),
                )
            })
            .children(item.keys.clone().map(|keys| kbd(keys, cx).flex_none()))
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let rows: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| self.render_row(ix, item, cx).into_any_element())
            .collect();
        let nothing = self.items.is_empty() && self.searching.is_none();
        let empty_text = self
            .provider
            .as_ref()
            .map(|p| p.empty_text())
            .unwrap_or_else(|| "Nothing matches".into());

        let field = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(14.))
            .py(px(10.))
            .border_b_1()
            .border_color(p.border.hsla())
            .child(icon(Icon::Search, px(14.), p.text_muted.hsla()))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .child(self.input.clone()),
            );

        let list = div()
            .id("palette-list")
            .test_support()
            .flex()
            .flex_col()
            .p(px(6.))
            .max_h(px(LIST_MAX))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .children(rows)
            .when(nothing, |list| {
                list.child(
                    div()
                        .id("palette-empty")
                        .test_support()
                        .px(px(10.))
                        .py(px(12.))
                        .text_color(p.text_subtle.hsla())
                        .child(empty_text),
                )
            });

        // Problems with the user's keybindings, so they aren't only in the log.
        let problems = cx
            .try_global::<super::CommandRegistry>()
            .map_or(0, |r| r.last_problems().len());
        let problems = (problems > 0).then(|| {
            let what = if problems == 1 { "problem" } else { "problems" };
            div()
                .id("palette-problems")
                .test_support()
                .px(px(14.))
                .py(px(6.))
                .border_t_1()
                .border_color(p.border.hsla())
                .text_size(m.text_xs)
                .text_color(p.sand.hsla())
                .child(SharedString::from(format!(
                    "{} has {problems} {what}; Coqo's log lists them",
                    super::keyfile::FILE_NAME
                )))
        });

        let hints = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(14.))
            .py(px(8.))
            .border_t_1()
            .border_color(p.border.hsla())
            .text_size(m.text_xs)
            .text_color(p.text_subtle.hsla())
            .child(kbd("↑↓", cx))
            .child("to move")
            .child(kbd("Enter", cx))
            .child("to run")
            .child(kbd("Esc", cx))
            .child("to close")
            .child(div().flex_1())
            .child(kbd(">", cx))
            .child("commands");

        let card = popover(cx)
            .id("command-palette")
            .test_support()
            .key_context(KEY_CONTEXT)
            .occlude()
            .w_full()
            .min_w(px(MIN_WIDTH))
            .text_size(m.text_md)
            .on_mouse_down_out(cx.listener(|this, _, window, cx| this.close(window, cx)))
            .child(field)
            .child(list)
            .children(problems)
            .child(hints);

        // The workbench places it under the top bar's command search, as
        // wide as the search.
        card
    }
}
