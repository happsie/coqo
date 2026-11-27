// SPDX-License-Identifier: Apache-2.0

//! Places in the workbench chrome that other code fills: the top bar and status
//! bar slots, and the sidebar panes. Each is a global registry of render
//! callbacks, so the language-intelligence mod can put its bolt in the top bar,
//! the git mod its branch in the status bar, the file tree its pane in the
//! sidebar, without the shell knowing about any of them.
//!
//! Registering an id again replaces that entry. Windows re-render when a
//! registry changes.

use std::rc::Rc;

use gpui_kit::{AnyElement, AnyView, App, Global, SharedString, WeakEntity, Window};

use crate::dock::Dock;

/// A place in the top bar or status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Top bar, after the workspace name.
    TopBarStart,
    /// Top bar, right-aligned (the bolt, the daemon pill).
    TopBarEnd,
    /// Status bar, left-aligned.
    StatusBarStart,
    /// Status bar, right-aligned.
    StatusBarEnd,
}

type RenderFn = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

#[derive(Clone)]
struct SlotItem {
    id: SharedString,
    slot: Slot,
    order: i32,
    render: RenderFn,
}

/// Registered top bar and status bar items.
#[derive(Clone, Default)]
pub struct Slots {
    items: Vec<SlotItem>,
}

impl Global for Slots {}

impl std::fmt::Debug for Slots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ids: Vec<_> = self.items.iter().map(|i| (&i.id, i.slot)).collect();
        f.debug_struct("Slots").field("items", &ids).finish()
    }
}

impl Slots {
    /// Add (or replace, by `id`) an item. Items in a slot are drawn in ascending
    /// `order`.
    pub fn register(
        cx: &mut App,
        slot: Slot,
        id: impl Into<SharedString>,
        order: i32,
        render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) {
        let id = id.into();
        let slots = cx.default_global::<Slots>();
        slots.items.retain(|i| i.id != id);
        slots.items.push(SlotItem {
            id,
            slot,
            order,
            render: Rc::new(render),
        });
    }

    /// Remove an item. Returns whether it was registered.
    pub fn unregister(cx: &mut App, id: &str) -> bool {
        let slots = cx.default_global::<Slots>();
        let before = slots.items.len();
        slots.items.retain(|i| i.id != id);
        slots.items.len() != before
    }

    /// The ids in a slot, in drawing order.
    pub fn ids(cx: &App, slot: Slot) -> Vec<SharedString> {
        Self::sorted(cx, slot).into_iter().map(|i| i.id).collect()
    }

    /// Render a slot's items.
    pub fn render(slot: Slot, window: &mut Window, cx: &mut App) -> Vec<AnyElement> {
        Self::sorted(cx, slot)
            .into_iter()
            .map(|item| (item.render)(window, cx))
            .collect()
    }

    fn sorted(cx: &App, slot: Slot) -> Vec<SlotItem> {
        let mut items: Vec<SlotItem> = cx
            .try_global::<Slots>()
            .map(|s| s.items.iter().filter(|i| i.slot == slot).cloned().collect())
            .unwrap_or_default();
        items.sort_by_key(|i| i.order);
        items
    }
}

/// Builds a sidebar pane's view for a window, given that window's dock.
pub type PaneBuilder = Rc<dyn Fn(WeakEntity<Dock>, &mut Window, &mut App) -> AnyView>;

/// A pane of the sidebar panel, shown under its title in the sidebar's tab
/// strip ("Files", "Git", …).
#[derive(Clone)]
pub struct SidebarPane {
    pub id: SharedString,
    pub title: SharedString,
    /// Position in the strip, ascending.
    pub order: i32,
    /// Builds the pane's view the first time it is shown in a window. It gets
    /// that window's dock, to open tabs (a file tree opening files).
    pub build: PaneBuilder,
}

impl SidebarPane {
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        order: i32,
        build: impl Fn(WeakEntity<Dock>, &mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            order,
            build: Rc::new(build),
        }
    }
}

/// Registered sidebar panes.
#[derive(Clone, Default)]
pub struct SidebarPanes {
    panes: Vec<SidebarPane>,
}

impl Global for SidebarPanes {}

impl std::fmt::Debug for SidebarPanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(&self.panes).finish()
    }
}

impl std::fmt::Debug for SidebarPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidebarPane")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("order", &self.order)
            .finish_non_exhaustive()
    }
}

impl SidebarPanes {
    /// Add (or replace, by id) a pane.
    pub fn register(cx: &mut App, pane: SidebarPane) {
        let panes = cx.default_global::<SidebarPanes>();
        panes.panes.retain(|p| p.id != pane.id);
        panes.panes.push(pane);
        panes.panes.sort_by_key(|p| p.order);
    }

    /// Every pane, in strip order.
    pub fn all(cx: &App) -> Vec<SidebarPane> {
        cx.try_global::<SidebarPanes>()
            .map(|p| p.panes.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{IntoElement as _, TestAppContext, div};

    use super::*;

    #[test]
    fn slot_items_sort_by_order_and_replace_by_id() {
        let cx = TestAppContext::single();
        cx.update(|cx| {
            Slots::register(cx, Slot::StatusBarStart, "b", 20, |_, _| {
                div().into_any_element()
            });
            Slots::register(cx, Slot::StatusBarStart, "a", 10, |_, _| {
                div().into_any_element()
            });
            Slots::register(cx, Slot::StatusBarEnd, "c", 0, |_, _| {
                div().into_any_element()
            });
            assert_eq!(Slots::ids(cx, Slot::StatusBarStart), ["a", "b"]);
            // Re-registering moves the item.
            Slots::register(cx, Slot::StatusBarStart, "a", 30, |_, _| {
                div().into_any_element()
            });
            assert_eq!(Slots::ids(cx, Slot::StatusBarStart), ["b", "a"]);
            assert!(Slots::unregister(cx, "b"));
            assert!(!Slots::unregister(cx, "b"));
            assert_eq!(Slots::ids(cx, Slot::StatusBarStart), ["a"]);
            assert_eq!(Slots::ids(cx, Slot::StatusBarEnd), ["c"]);
        });
    }
}
