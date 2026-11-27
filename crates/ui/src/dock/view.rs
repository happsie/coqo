// SPDX-License-Identifier: Apache-2.0

//! [`Dock`]: the main tab area. It projects a [`PaneTree`] into floating tab
//! group panels separated by the window's 10 px gaps. The gaps double as resize
//! handles, tabs drag between groups and onto group edges, and an emptied group
//! closes and collapses its split.

use std::{collections::HashMap, rc::Rc};

use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, DragMoveEvent, ElementId, Empty,
    EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder, px, relative,
};

use super::{
    CloseGroup, CloseTab, SplitDown, SplitRight,
    tab::{AnyTab, Built, build_tab, missing_tab},
    tree::{Edge, NodeId, PaneNode, PaneTree, SplitAxis, TabGroup, TabId, TabState, TabTarget},
};
use crate::{
    theme::ActiveTokens as _,
    widgets::{Icon, TestSupportExt as _, icon, panel, raised, tab_frame},
};

/// The smallest a split child can be dragged to.
const MIN_PANE: Pixels = px(160.);

/// Where a dragged tab would land inside a group's content area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropZone {
    /// Into the group, as its last tab.
    Center,
    /// Into a new group split off this edge.
    Edge(Edge),
}

/// Emitted after any change worth saving: tabs opened, moved or closed, splits
/// made or resized, the displayed tab changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockEvent {
    LayoutChanged,
}

/// The drag payload of a tab.
#[derive(Clone, Debug)]
pub struct DraggedTab {
    pub tab: TabId,
    title: SharedString,
    icon: Option<Icon>,
}

/// The drag payload of a split handle.
#[derive(Clone, Copy, Debug)]
struct DraggedHandle {
    split: NodeId,
    handle: usize,
}

type ElementFactory = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type TabFactory = Rc<dyn Fn(&mut Window, &mut App) -> AnyTab>;

/// The main tab area: tab groups in resizable splits.
pub struct Dock {
    tree: PaneTree,
    tabs: HashMap<TabId, AnyTab>,
    observers: HashMap<TabId, Subscription>,
    active_group: NodeId,
    drop_target: Option<(NodeId, DropZone)>,
    focus_handle: FocusHandle,
    empty_state: Option<ElementFactory>,
    new_tab: Option<TabFactory>,
}

impl EventEmitter<DockEvent> for Dock {}

impl std::fmt::Debug for Dock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dock")
            .field("tree", &self.tree)
            .field("active_group", &self.active_group)
            .finish_non_exhaustive()
    }
}

impl Focusable for Dock {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Dock {
    /// One empty group.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let tree = PaneTree::new();
        Self {
            active_group: tree.root().id(),
            tree,
            tabs: HashMap::new(),
            observers: HashMap::new(),
            drop_target: None,
            focus_handle: cx.focus_handle(),
            empty_state: None,
            new_tab: None,
        }
    }

    /// Rebuild a saved layout. Tabs are recreated through their kinds'
    /// builders; a kind this build doesn't know becomes a placeholder that keeps
    /// its state, and a tab its builder declines is dropped.
    pub fn restore(root: PaneNode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut dock = Self::new(cx);
        dock.tree = PaneTree::from_root(root);
        let saved: Vec<TabState> = dock.tree.tabs().into_iter().cloned().collect();
        let mut declined = Vec::new();
        for state in saved {
            let tab = match build_tab(&state.kind, &state.state, window, cx) {
                Built::Tab(tab) => tab,
                Built::Declined => {
                    declined.push(state.id);
                    continue;
                }
                Built::Unknown => missing_tab(&state.kind, state.state.clone(), cx),
            };
            dock.adopt(state.id, tab, cx);
        }
        if !declined.is_empty() {
            dock.tree.retain_tabs(|t| !declined.contains(&t.id));
        }
        dock.active_group = dock.tree.groups()[0].id;
        dock
    }

    /// The layout with each tab's current saved state, ready to persist.
    pub fn snapshot(&self, cx: &App) -> PaneNode {
        self.tree.snapshot_with(|state| {
            if let Some(tab) = self.tabs.get(&state.id) {
                state.kind = tab.kind(cx).to_string();
                state.state = tab.save(cx);
            }
        })
    }

    /// Draws an empty group (only the last group can be empty).
    pub fn set_empty_state(
        &mut self,
        render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) {
        self.empty_state = Some(Rc::new(render));
    }

    /// Makes the tab a split opens when the active tab can't be duplicated (or
    /// the group is empty).
    pub fn set_new_tab(&mut self, build: impl Fn(&mut Window, &mut App) -> AnyTab + 'static) {
        self.new_tab = Some(Rc::new(build));
    }

    pub fn tree(&self) -> &PaneTree {
        &self.tree
    }

    /// The group that actions and [`Dock::open_tab`] apply to: the one last
    /// clicked or opened into.
    pub fn active_group(&self) -> NodeId {
        self.active_group
    }

    /// The displayed tab of the active group.
    pub fn active_tab(&self) -> Option<TabId> {
        self.tree.group(self.active_group)?.active_tab()
    }

    pub fn tab(&self, id: TabId) -> Option<&AnyTab> {
        self.tabs.get(&id)
    }

    /// The open tab with this kind and [`TabItem::key`](super::TabItem::key).
    pub fn find_tab(&self, kind: &str, key: &str, cx: &App) -> Option<TabId> {
        self.tree.tabs().into_iter().map(|t| t.id).find(|id| {
            self.tabs
                .get(id)
                .is_some_and(|tab| tab.kind(cx) == kind && tab.key(cx).as_deref() == Some(key))
        })
    }

    /// Open a tab in the active group and show it. If a tab with the same kind
    /// and key is already open, that one is shown instead.
    pub fn open_tab(&mut self, tab: AnyTab, window: &mut Window, cx: &mut Context<Self>) -> TabId {
        let target = TabTarget::Group {
            group: self.active_group,
            index: None,
        };
        self.open_tab_at(tab, target, window, cx)
    }

    /// Open a tab at `target` (a group, beside a tab, or a new split). An
    /// unknown target falls back to the active group.
    pub fn open_tab_at(
        &mut self,
        tab: AnyTab,
        target: TabTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabId {
        if let Some(key) = tab.key(cx)
            && let Some(existing) = self.find_tab(&tab.kind(cx), &key, cx)
        {
            self.activate_tab(existing, window, cx);
            return existing;
        }
        self.insert_tab(tab, target, window, cx)
    }

    fn insert_tab(
        &mut self,
        tab: AnyTab,
        target: TabTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabId {
        let id = TabId(self.tree.alloc_id());
        let state = TabState {
            id,
            kind: tab.kind(cx).to_string(),
            state: serde_json::Value::Null,
        };
        let fallback = TabTarget::Group {
            group: self.active_group,
            index: None,
        };
        let group = match self.tree.insert(state.clone(), target) {
            Some(group) => group,
            None => self
                .tree
                .insert(state, fallback)
                .expect("active group exists"),
        };
        self.adopt(id, tab, cx);
        self.active_group = group;
        self.focus_tab(id, window, cx);
        self.changed(cx);
        id
    }

    fn adopt(&mut self, id: TabId, tab: AnyTab, cx: &mut Context<Self>) {
        self.observers.insert(id, tab.observe(cx));
        self.tabs.insert(id, tab);
    }

    /// Show a tab and make its group active.
    pub fn activate_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree.activate(id) {
            self.active_group = self.tree.group_of(id).expect("activated");
            self.focus_tab(id, window, cx);
            self.changed(cx);
        }
    }

    fn focus_tab(&self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.get(&id).and_then(|t| t.focus_handle(cx)) {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&self.focus_handle, cx),
        }
    }

    /// Close a tab. An emptied group closes and its split collapses.
    pub fn close_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        if self.tree.remove(id).is_some() {
            self.observers.remove(&id);
            if let Some(tab) = self.tabs.remove(&id) {
                tab.on_close(cx);
            }
            self.changed(cx);
        }
    }

    /// Close a group and every tab in it.
    pub fn close_group(&mut self, group: NodeId, cx: &mut Context<Self>) {
        let closed = self.tree.close_group(group);
        if closed.is_empty() {
            return;
        }
        for tab in closed {
            self.observers.remove(&tab.id);
            if let Some(tab) = self.tabs.remove(&tab.id) {
                tab.on_close(cx);
            }
        }
        self.changed(cx);
    }

    /// Move an open tab. Dropping a tab into the middle of its own group only
    /// shows it.
    pub fn move_tab(
        &mut self,
        id: TabId,
        target: TabTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let TabTarget::Group { group, index: None } = target
            && self.tree.group_of(id) == Some(group)
        {
            self.activate_tab(id, window, cx);
            return;
        }
        if let Some(group) = self.tree.move_tab(id, target) {
            self.active_group = group;
            self.focus_tab(id, window, cx);
            self.changed(cx);
        }
    }

    /// Split the active group: the new group beside it shows a duplicate of the
    /// active tab (rebuilt from its saved state), or the new-tab fallback.
    pub fn split(&mut self, edge: Edge, window: &mut Window, cx: &mut Context<Self>) {
        let duplicate = self.active_tab().and_then(|id| {
            let tab = self.tabs.get(&id)?;
            let (kind, state) = (tab.kind(cx), tab.save(cx));
            match build_tab(&kind, &state, window, cx) {
                Built::Tab(tab) => Some(tab),
                _ => None,
            }
        });
        let Some(tab) = duplicate.or_else(|| self.new_tab.clone().map(|f| f(window, cx))) else {
            return;
        };
        let target = TabTarget::Split {
            group: self.active_group,
            edge,
        };
        self.insert_tab(tab, target, window, cx);
    }

    /// Resize a split from a handle drag.
    fn drag_resize(
        &mut self,
        drag: DraggedHandle,
        axis: SplitAxis,
        bounds: Bounds<Pixels>,
        pointer: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let gap = cx.tokens().metrics.gap;
        let Some(sizes) = self.tree.split_sizes(drag.split) else {
            return;
        };
        let n = sizes.len();
        let (start, length, at) = match axis {
            SplitAxis::Row => (bounds.left(), bounds.size.width, pointer.x),
            SplitAxis::Column => (bounds.top(), bounds.size.height, pointer.y),
        };
        let available = length - gap * (n - 1) as f32;
        if available <= px(0.) || drag.handle + 1 >= n {
            return;
        }
        let before: f32 = sizes[..drag.handle].iter().sum();
        let pair = sizes[drag.handle] + sizes[drag.handle + 1];
        let min = (MIN_PANE / available).min(pair / 2.);
        let share = (at - start - gap * drag.handle as f32 - gap / 2.) / available - before;
        if self
            .tree
            .resize(drag.split, drag.handle, share.clamp(min, pair - min))
        {
            self.changed(cx);
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        if self.tree.group(self.active_group).is_none() {
            self.active_group = self.tree.groups()[0].id;
        }
        cx.emit(DockEvent::LayoutChanged);
        cx.notify();
    }

    fn focus_group(&mut self, group: NodeId, cx: &mut Context<Self>) {
        if self.active_group != group {
            self.active_group = group;
            cx.notify();
        }
    }
}

fn zone_for(bounds: Bounds<Pixels>, pointer: Point<Pixels>) -> DropZone {
    let x = (pointer.x - bounds.left()) / bounds.size.width;
    let y = (pointer.y - bounds.top()) / bounds.size.height;
    let (dx, dy) = (x.min(1. - x), y.min(1. - y));
    if dx > 0.25 && dy > 0.25 {
        DropZone::Center
    } else if dx < dy {
        DropZone::Edge(if x < 0.5 { Edge::Left } else { Edge::Right })
    } else {
        DropZone::Edge(if y < 0.5 { Edge::Top } else { Edge::Bottom })
    }
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !cx.has_active_drag() {
            self.drop_target = None;
        }
        let root = self.tree.root().clone();
        div()
            .id("dock")
            .key_context("Dock")
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &SplitRight, window, cx| this.split(Edge::Right, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SplitDown, window, cx| this.split(Edge::Bottom, window, cx)),
            )
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                if let Some(tab) = this.active_tab() {
                    this.close_tab(tab, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseGroup, _, cx| {
                let group = this.active_group;
                this.close_group(group, cx);
            }))
            .size_full()
            .flex()
            .child(self.render_node(&root, window, cx))
    }
}

impl Dock {
    fn render_node(
        &self,
        node: &PaneNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            PaneNode::Group(group) => self.render_group(group, window, cx),
            PaneNode::Split {
                id,
                axis,
                children,
                sizes,
            } => {
                let (split, axis) = (*id, *axis);
                let mut el = div()
                    .id(("dock-split", split.0))
                    .test_support()
                    .flex()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .when(axis == SplitAxis::Column, |d| d.flex_col())
                    .on_drag_move(cx.listener(
                        move |this, e: &DragMoveEvent<DraggedHandle>, _, cx| {
                            let drag = *e.drag(cx);
                            if drag.split == split {
                                this.drag_resize(drag, axis, e.bounds, e.event.position, cx);
                            }
                        },
                    ));
                for (ix, (child, size)) in children.iter().zip(sizes).enumerate() {
                    if ix > 0 {
                        el = el.child(render_handle(split, axis, ix - 1, cx));
                    }
                    let slot = div()
                        .flex()
                        .min_w_0()
                        .min_h_0()
                        .flex_grow(*size)
                        .flex_shrink(1.)
                        .flex_basis(px(0.));
                    el = el.child(slot.child(self.render_node(child, window, cx)));
                }
                el.into_any_element()
            }
        }
    }

    fn render_group(
        &self,
        group: &TabGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let gid = group.id;
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());

        let mut strip = div()
            .id(("tab-strip", gid.0))
            .test_support()
            .flex()
            .items_center()
            .gap(px(6.))
            .p(px(8.))
            .min_h(px(48.))
            .on_drop(cx.listener(move |this, drag: &DraggedTab, window, cx| {
                let target = TabTarget::Group {
                    group: gid,
                    index: None,
                };
                this.move_tab(drag.tab, target, window, cx);
            }));
        for (ix, state) in group.tabs.iter().enumerate() {
            if let Some(tab) = self.tabs.get(&state.id) {
                strip = strip.child(self.render_tab(state.id, tab, ix == group.active, window, cx));
            }
        }
        let strip = strip.child(div().flex_1()).child(
            div()
                .id(("split", gid.0))
                .test_support()
                .size(px(32.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(m.radius_control)
                .cursor_pointer()
                .hover(|s| s.bg(p.surface.hsla()))
                .child(icon(Icon::Columns, px(16.), p.text_muted.hsla()))
                .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                    this.active_group = gid;
                    let edge = if e.modifiers().shift {
                        Edge::Bottom
                    } else {
                        Edge::Right
                    };
                    this.split(edge, window, cx);
                })),
        );

        let body = match group.active_tab().and_then(|id| self.tabs.get(&id)) {
            Some(tab) => div()
                .size_full()
                .flex()
                .flex_col()
                .child(tab.view())
                .into_any_element(),
            None => match &self.empty_state {
                Some(render) => render(window, cx),
                None => Empty.into_any_element(),
            },
        };
        let indicator = self
            .drop_target
            .filter(|(g, _)| *g == gid && cx.has_active_drag())
            .map(|(_, zone)| render_drop_indicator(zone, cx));
        let content = div()
            .id(("group-content", gid.0))
            .test_support()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(body)
            .children(indicator)
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                    let pointer = e.event.position;
                    if e.bounds.contains(&pointer) {
                        let target = Some((gid, zone_for(e.bounds, pointer)));
                        if this.drop_target != target {
                            this.drop_target = target;
                            cx.notify();
                        }
                    } else if this.drop_target.is_some_and(|(g, _)| g == gid) {
                        this.drop_target = None;
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(move |this, drag: &DraggedTab, window, cx| {
                let zone = this
                    .drop_target
                    .take()
                    .filter(|(g, _)| *g == gid)
                    .map_or(DropZone::Center, |(_, zone)| zone);
                let target = match zone {
                    DropZone::Center => TabTarget::Group {
                        group: gid,
                        index: None,
                    },
                    DropZone::Edge(edge) => TabTarget::Split { group: gid, edge },
                };
                this.move_tab(drag.tab, target, window, cx);
            }));

        panel(p.panel, cx)
            .id(("group", gid.0))
            .test_support()
            .size_full()
            .min_w_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.focus_group(gid, cx)),
            )
            .child(strip)
            .child(content)
            .into_any_element()
    }

    fn render_tab(
        &self,
        id: TabId,
        tab: &AnyTab,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let p = t.palette.clone();
        let title = tab.title(cx);
        let tab_icon = tab.icon(cx);
        let hover_group: SharedString = format!("tab-{}", id.0).into();
        let label = tab.render_label(selected, window, cx).unwrap_or_else(|| {
            let color = if selected { p.text } else { p.text_muted };
            div()
                .flex()
                .items_center()
                .gap_2()
                .children(tab_icon.map(|i| icon(i, px(14.), color.hsla())))
                .child(title.clone())
                .into_any_element()
        });
        let close = div()
            .id(("tab-close", id.0))
            .test_support()
            .size(px(18.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .opacity(0.)
            .group_hover(hover_group.clone(), |s| s.opacity(1.))
            .hover(|s| s.bg(p.surface_strong.hsla()))
            .child(icon(Icon::X, px(12.), p.text_subtle.hsla()))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.close_tab(id, cx);
            }));
        let drop_bg = p.surface_selected.hsla();
        tab_frame(("tab", id.0), selected, cx)
            .test_support()
            .group(hover_group)
            .pr(px(6.))
            .child(label)
            .child(close)
            .on_click(cx.listener(move |this, _, window, cx| this.activate_tab(id, window, cx)))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| this.close_tab(id, cx)),
            )
            .on_drag(
                DraggedTab {
                    tab: id,
                    title,
                    icon: tab_icon,
                },
                |drag, _, _, cx| cx.new(|_| DragPreview(drag.clone())),
            )
            .drag_over::<DraggedTab>(move |s, _, _, _| s.bg(drop_bg))
            .on_drop(cx.listener(move |this, drag: &DraggedTab, window, cx| {
                this.move_tab(drag.tab, TabTarget::Before(id), window, cx);
            }))
            .into_any_element()
    }
}

fn render_handle(split: NodeId, axis: SplitAxis, ix: usize, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (gap, pill) = (t.metrics.gap, t.palette.border_strong.hsla());
    let name: SharedString = format!("split-handle-{}-{}", split.0, ix).into();
    let bar = div().rounded_full().flex_none().bg(pill).opacity(0.);
    let bar = match axis {
        SplitAxis::Row => bar.w(px(3.)).h(px(32.)),
        SplitAxis::Column => bar.h(px(3.)).w(px(32.)),
    };
    let el = div()
        .id(ElementId::Name(name.clone()))
        .test_support()
        .group(name.clone())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .child(bar.group_hover(name, |s| s.opacity(1.)))
        .on_drag(DraggedHandle { split, handle: ix }, |_, _, _, cx| {
            cx.new(|_| EmptyDrag)
        });
    match axis {
        SplitAxis::Row => el.w(gap).h_full().cursor_col_resize(),
        SplitAxis::Column => el.h(gap).w_full().cursor_row_resize(),
    }
    .into_any_element()
}

fn render_drop_indicator(zone: DropZone, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let blue = t.palette.blue;
    let half = relative(0.5);
    div()
        .id("drop-indicator")
        .test_support()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .map(|d| match zone {
            DropZone::Center => d,
            DropZone::Edge(Edge::Left) => d.w(half),
            DropZone::Edge(Edge::Right) => d.left(half).w(half),
            DropZone::Edge(Edge::Top) => d.h(half),
            DropZone::Edge(Edge::Bottom) => d.top(half).h(half),
        })
        .p(px(6.))
        .child(
            div()
                .size_full()
                .rounded(t.metrics.radius_control)
                .bg(blue.alpha(0.10))
                .border_2()
                .border_color(blue.alpha(0.45)),
        )
        .into_any_element()
}

struct DragPreview(DraggedTab);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        raised(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py(px(6.))
                .rounded(m.radius_control)
                .font_family(m.font_ui.clone())
                .text_size(m.text_sm)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text.hsla()),
            cx,
        )
        .children(self.0.icon.map(|i| icon(i, px(14.), p.text.hsla())))
        .child(self.0.title.clone())
    }
}

struct EmptyDrag;

impl Render for EmptyDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}
