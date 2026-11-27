// SPDX-License-Identifier: Apache-2.0

//! Tabs are generic: anything that implements [`TabItem`] can sit in a tab group
//! (files, chats, diffs, PRs, traces, …). A tab kind registers a builder with
//! [`register_tab_kind`] so a saved layout can recreate it after a restart.

use std::{collections::HashMap, rc::Rc};

use gpui_kit::{
    AnyElement, AnyView, App, AppContext as _, Context, Entity, EntityId, FocusHandle, Global,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
    px,
};
use serde_json::Value;

use crate::{
    theme::ActiveTokens as _,
    widgets::{Icon, icon},
};

/// A view that can live in a tab. Implement it on the tab's view type and wrap
/// the entity with [`AnyTab::new`].
#[allow(unused_variables)]
pub trait TabItem: Render {
    /// The kind this tab is restored by, as passed to [`register_tab_kind`]
    /// (`"file"`, `"chat"`, …). Persisted, so never change it once shipped.
    fn kind(&self) -> SharedString;

    /// The tab strip label.
    fn title(&self, cx: &App) -> SharedString;

    /// An icon before the title.
    fn icon(&self, cx: &App) -> Option<Icon> {
        None
    }

    /// Identity for de-duplication: opening a tab whose kind and key match an
    /// open tab focuses that tab instead (a file path, a chat session id).
    fn key(&self, cx: &App) -> Option<SharedString> {
        None
    }

    /// Whatever the builder needs to recreate this tab (a path, a session id,
    /// a scroll position). Captured each time the layout is saved.
    fn save(&self, cx: &App) -> Value {
        Value::Null
    }

    /// A custom tab strip label, replacing the icon and title (for example a
    /// coloured file-type letter, or a "needs input" badge on a chat).
    fn render_label(&self, selected: bool, window: &mut Window, cx: &App) -> Option<AnyElement> {
        None
    }

    /// The focus handle to focus when the tab is shown, for tabs that take
    /// keyboard input.
    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        None
    }

    /// The user closed the tab (or its group). Not called when the window or
    /// the app closes, so a tab that owns something outside the UI (a
    /// terminal, a session) can end it here and keep it across restarts.
    fn on_close(&mut self, cx: &mut Context<Self>) {}
}

trait TabHandle {
    fn view(&self) -> AnyView;
    fn entity_id(&self) -> EntityId;
    fn kind(&self, cx: &App) -> SharedString;
    fn title(&self, cx: &App) -> SharedString;
    fn icon(&self, cx: &App) -> Option<Icon>;
    fn key(&self, cx: &App) -> Option<SharedString>;
    fn save(&self, cx: &App) -> Value;
    fn render_label(&self, selected: bool, window: &mut Window, cx: &App) -> Option<AnyElement>;
    fn focus_handle(&self, cx: &App) -> Option<FocusHandle>;
    fn observe(&self, cx: &mut Context<super::Dock>) -> Subscription;
    fn on_close(&self, cx: &mut App);
}

impl<T: TabItem + 'static> TabHandle for Entity<T> {
    fn view(&self) -> AnyView {
        self.clone().into()
    }
    fn entity_id(&self) -> EntityId {
        Entity::entity_id(self)
    }
    fn kind(&self, cx: &App) -> SharedString {
        self.read(cx).kind()
    }
    fn title(&self, cx: &App) -> SharedString {
        self.read(cx).title(cx)
    }
    fn icon(&self, cx: &App) -> Option<Icon> {
        self.read(cx).icon(cx)
    }
    fn key(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).key(cx)
    }
    fn save(&self, cx: &App) -> Value {
        self.read(cx).save(cx)
    }
    fn render_label(&self, selected: bool, window: &mut Window, cx: &App) -> Option<AnyElement> {
        self.read(cx).render_label(selected, window, cx)
    }
    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        self.read(cx).focus_handle(cx)
    }
    fn observe(&self, cx: &mut Context<super::Dock>) -> Subscription {
        cx.observe(self, |_, _, cx| cx.notify())
    }
    fn on_close(&self, cx: &mut App) {
        self.update(cx, |tab, cx| tab.on_close(cx));
    }
}

/// A type-erased tab: any [`TabItem`] entity.
#[derive(Clone)]
pub struct AnyTab(Rc<dyn TabHandle>);

impl std::fmt::Debug for AnyTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AnyTab").field(&self.0.entity_id()).finish()
    }
}

impl AnyTab {
    pub fn new<T: TabItem + 'static>(entity: Entity<T>) -> Self {
        Self(Rc::new(entity))
    }

    /// The tab's view, for rendering.
    pub fn view(&self) -> AnyView {
        self.0.view()
    }

    /// The tab's entity, if it is a `T`.
    pub fn downcast<T: TabItem + 'static>(&self) -> Option<Entity<T>> {
        self.0.view().downcast::<T>().ok()
    }

    pub fn kind(&self, cx: &App) -> SharedString {
        self.0.kind(cx)
    }
    pub fn title(&self, cx: &App) -> SharedString {
        self.0.title(cx)
    }
    pub fn icon(&self, cx: &App) -> Option<Icon> {
        self.0.icon(cx)
    }
    pub fn key(&self, cx: &App) -> Option<SharedString> {
        self.0.key(cx)
    }
    pub fn save(&self, cx: &App) -> Value {
        self.0.save(cx)
    }
    pub fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        self.0.focus_handle(cx)
    }
    pub(crate) fn render_label(
        &self,
        selected: bool,
        window: &mut Window,
        cx: &App,
    ) -> Option<AnyElement> {
        self.0.render_label(selected, window, cx)
    }
    pub(crate) fn observe(&self, cx: &mut Context<super::Dock>) -> Subscription {
        self.0.observe(cx)
    }
    pub(crate) fn on_close(&self, cx: &mut App) {
        self.0.on_close(cx);
    }
}

type TabBuilder = Rc<dyn Fn(&Value, &mut Window, &mut App) -> Option<AnyTab>>;

/// Builders for every registered tab kind.
#[derive(Default)]
pub struct TabRegistry {
    builders: HashMap<SharedString, TabBuilder>,
}

impl Global for TabRegistry {}

impl std::fmt::Debug for TabRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TabRegistry")
            .field("kinds", &self.builders.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Register how to recreate tabs of `kind` from their saved state. Registering
/// a kind again replaces its builder. The builder returns `None` when the state
/// no longer makes sense (say, a deleted file); the tab is then dropped.
pub fn register_tab_kind(
    cx: &mut App,
    kind: impl Into<SharedString>,
    build: impl Fn(&Value, &mut Window, &mut App) -> Option<AnyTab> + 'static,
) {
    cx.default_global::<TabRegistry>()
        .builders
        .insert(kind.into(), Rc::new(build));
}

/// What restoring a saved tab produced.
pub(crate) enum Built {
    Tab(AnyTab),
    /// The kind is registered but declined this state.
    Declined,
    /// No builder for this kind in this build.
    Unknown,
}

pub(crate) fn build_tab(kind: &str, state: &Value, window: &mut Window, cx: &mut App) -> Built {
    let builder = cx
        .try_global::<TabRegistry>()
        .and_then(|r| r.builders.get(kind).cloned());
    match builder {
        Some(build) => match build(state, window, cx) {
            Some(tab) => Built::Tab(tab),
            None => Built::Declined,
        },
        None => Built::Unknown,
    }
}

/// Stands in for a tab whose kind this build doesn't know (a mod that was
/// removed, say). It keeps the saved state, so the tab comes back intact once
/// the kind is available again.
#[derive(Debug)]
pub struct MissingTab {
    kind: SharedString,
    state: Value,
}

impl MissingTab {
    pub fn new(kind: impl Into<SharedString>, state: Value) -> Self {
        Self {
            kind: kind.into(),
            state,
        }
    }
}

impl TabItem for MissingTab {
    fn kind(&self) -> SharedString {
        self.kind.clone()
    }
    fn title(&self, _: &App) -> SharedString {
        "Unavailable".into()
    }
    fn icon(&self, _: &App) -> Option<Icon> {
        Some(Icon::CircleAlert)
    }
    fn save(&self, _: &App) -> Value {
        self.state.clone()
    }
}

impl Render for MissingTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let p = &t.palette;
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(icon(Icon::CircleAlert, px(24.), p.text_subtle.hsla()))
            .child(
                div()
                    .text_color(p.text_muted.hsla())
                    .child(format!("Nothing in this build opens “{}” tabs.", self.kind)),
            )
            .child(
                div()
                    .text_size(t.metrics.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child("The tab is kept and comes back when its mod is installed."),
            )
    }
}

pub(crate) fn missing_tab(kind: &str, state: Value, cx: &mut App) -> AnyTab {
    let kind = kind.to_owned();
    AnyTab::new(cx.new(|_| MissingTab::new(kind, state)))
}
