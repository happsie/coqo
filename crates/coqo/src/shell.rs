// SPDX-License-Identifier: Apache-2.0

//! The workbench window (design board 1): top bar, workspace rail, sidebar
//! panel, the main tab area ([`Dock`]) and the status bar.
//!
//! The chrome is filled through registries in `coqo_ui::slots`: top bar and
//! status bar items ([`Slots`]) and sidebar panes ([`SidebarPanes`]). The
//! built-in items registered by [`init`] are honest placeholders until the
//! daemon, git and language mods exist.
//!
//! Each window has its own [`Workspaces`] (the workspace it shows) and dock.
//! The layout (sidebar width and pane, splits and tabs) is saved through a
//! [`LayoutStore`] per workspace, keyed by the workspace id (the app's store
//! keeps it in the daemon, #140), and restored when the window opens or
//! switches to that workspace. While no workspace is open, the layout is
//! saved under the window's local key (`"default"`); the first workspace
//! opened that has no layout of its own adopts it.

use std::{collections::HashMap, rc::Rc, time::Duration};

use coqo_ui::{
    ActiveTokens as _,
    dock::{
        Dock, DockEvent,
        store::{LAYOUT_VERSION, LayoutStore, SidebarLayout, WorkbenchLayout},
    },
    gpui::{
        AnyElement, AnyView, App, AppContext as _, BoxShadow, Context, Div, DragMoveEvent,
        ElementId, Empty, Entity, Focusable as _, FontWeight, InteractiveElement as _, IntoElement,
        ParentElement as _, Pixels, Render, SharedString, Stateful,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, hsla, point,
        px,
    },
    slots::{PaneBuilder, SidebarPane, SidebarPanes, Slot, Slots},
    widgets::{
        Icon, TestSupportExt as _, icon, kbd, panel, raised, section_label, status_dot, tab,
    },
};

use crate::{
    chat,
    daemon::{self, Daemon},
    files,
    layout::{DaemonLayoutStore, LAYOUT_KEY},
    welcome::{self, shortcut},
    workspace::{
        KnownWorkspaces, OpenFolder, Workspaces, WorkspacesEvent, initials, prompt_open_folder_in,
    },
};

/// The key a window's layout is saved under while no workspace is open.
pub const DEFAULT_WORKSPACE: &str = "default";

const SAVE_DELAY: Duration = Duration::from_millis(300);
const SIDEBAR_MIN: Pixels = px(180.);
const SIDEBAR_MAX: Pixels = px(520.);

/// Register the built-in tab kinds, chrome items and sidebar panes. Call once
/// after `coqo_ui::init`.
pub fn init(cx: &mut App) {
    welcome::register(cx);
    register_builtin_slots(cx);
    daemon::register_status_slot(cx);
    files::init(cx);
    chat::init(cx);
    SidebarPanes::register(
        cx,
        SidebarPane::new("git", "Git", 10, |_, _, cx| {
            cx.new(|_| NotePane::new("sidebar-git-pane", "Changes", "No repository"))
                .into()
        }),
    );
}

/// The drag payload of the sidebar's resize edge.
#[derive(Clone, Copy, Debug)]
struct DraggedSidebarEdge;

/// The root view of a workbench window.
pub struct Shell {
    dock: Entity<Dock>,
    store: Rc<dyn LayoutStore>,
    workspaces: Entity<Workspaces>,
    /// The key the layout is saved under: the workspace id, or `local_key`.
    layout_key: SharedString,
    /// The key while no workspace is open.
    local_key: SharedString,
    sidebar_width: Pixels,
    sidebar_pane: Option<SharedString>,
    /// Built pane views, with the builder that made them: a pane registered
    /// again gets a fresh view.
    pane_views: HashMap<SharedString, (PaneBuilder, AnyView)>,
    pending_save: Option<Task<()>>,
    _dock_events: Subscription,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell")
            .field("layout_key", &self.layout_key)
            .field("sidebar_width", &self.sidebar_width)
            .field("sidebar_pane", &self.sidebar_pane)
            .finish_non_exhaustive()
    }
}

impl Shell {
    /// Open a workbench window. It restores the layout saved under
    /// `local_key` until a workspace opens, then that workspace's.
    pub fn new(
        store: Rc<dyn LayoutStore>,
        local_key: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let local_key = local_key.into();
        // Before the dock: restored tabs look up the window's workspace.
        let workspaces = cx.new(|cx| Workspaces::new(window, cx));
        let saved = store.load(&local_key).unwrap_or_else(|err| {
            eprintln!("coqo: couldn't load the saved layout, using the default: {err}");
            None
        });
        let dock = new_dock(saved.as_ref(), window, cx);
        let mut subscriptions = vec![
            cx.observe_global::<Slots>(|_, cx| cx.notify()),
            cx.observe_global::<SidebarPanes>(|_, cx| cx.notify()),
            cx.on_app_quit(|this, cx| {
                this.save_now(cx);
                this.store.flush()
            }),
            // A closed window saves its layout.
            cx.on_release(|this, cx| this.save_to_store(cx)),
            cx.observe(&workspaces, |_, _, cx| cx.notify()),
            cx.subscribe_in(&workspaces, window, |this, _, event, window, cx| {
                if *event == WorkspacesEvent::CurrentChanged {
                    this.workspace_changed(window, cx);
                }
            }),
        ];
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.observe(&daemon, |_, _, cx| cx.notify()));
        }
        if let Some(known) = KnownWorkspaces::global(cx) {
            subscriptions.push(cx.observe(&known, |_, _, cx| cx.notify()));
        }
        let focus = dock.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let sidebar = saved.map(|l| l.sidebar);
        Self {
            _dock_events: cx.subscribe(&dock, |this, _, _: &DockEvent, cx| this.schedule_save(cx)),
            dock,
            store,
            workspaces,
            layout_key: local_key.clone(),
            local_key,
            sidebar_width: sidebar.as_ref().map_or_else(
                || cx.tokens().metrics.sidebar_width,
                |s| px(s.width).clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            ),
            sidebar_pane: sidebar.and_then(|s| s.pane).map(Into::into),
            pane_views: HashMap::new(),
            pending_save: None,
            _subscriptions: subscriptions,
        }
    }

    /// The window's workspace.
    #[allow(dead_code)] // Used by tests.
    pub fn workspaces(&self) -> &Entity<Workspaces> {
        &self.workspaces
    }

    /// The key the layout is saved under now.
    #[allow(dead_code)] // Used by tests.
    pub fn layout_key(&self) -> &SharedString {
        &self.layout_key
    }

    /// The window shows another workspace (or the same one again, after a
    /// reconnect): switch to its layout.
    fn workspace_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(info) = self.workspaces.read(cx).current() else {
            return;
        };
        let key: SharedString = info.id.0.clone().into();
        if key == self.layout_key {
            // Reattached: keep the tabs, and push what may have been saved
            // while the daemon was away.
            self.schedule_save(cx);
            return;
        }
        if let Some(store) = DaemonLayoutStore::global(cx) {
            store.fetched(&key, self.workspaces.read(cx).state(LAYOUT_KEY));
        }
        self.save_now(cx);
        let from_local = self.layout_key == self.local_key;
        let saved = self.store.load(&key).unwrap_or_else(|err| {
            eprintln!("coqo: couldn't load the saved layout, using the default: {err}");
            None
        });
        let previous = std::mem::replace(&mut self.layout_key, key);
        match saved {
            Some(layout) => self.apply_layout(Some(layout), window, cx),
            // The no-workspace layout moves into the workspace (this imports
            // a layout file from before layouts lived in the daemon, once).
            None if from_local => {
                if let Err(err) = self.store.remove(&previous) {
                    eprintln!("coqo: couldn't remove the old layout: {err}");
                }
            }
            None => self.apply_layout(None, window, cx),
        }
        self.save_now(cx);
    }

    /// Replace the dock and sidebar with `layout` (or the default).
    fn apply_layout(
        &mut self,
        layout: Option<WorkbenchLayout>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dock = new_dock(layout.as_ref(), window, cx);
        self._dock_events =
            cx.subscribe(&dock, |this, _, _: &DockEvent, cx| this.schedule_save(cx));
        self.dock = dock;
        // Panes hold the old dock: build them again.
        self.pane_views.clear();
        let sidebar = layout.map(|l| l.sidebar);
        self.sidebar_width = sidebar.as_ref().map_or_else(
            || cx.tokens().metrics.sidebar_width,
            |s| px(s.width).clamp(SIDEBAR_MIN, SIDEBAR_MAX),
        );
        self.sidebar_pane = sidebar.and_then(|s| s.pane).map(Into::into);
        let focus = self.dock.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The main tab area, for opening and arranging tabs.
    #[allow(dead_code)] // Used by tests now; by the file tree and chat list next.
    pub fn dock(&self) -> &Entity<Dock> {
        &self.dock
    }

    /// A sidebar pane's view, once it has been shown.
    #[allow(dead_code)] // Used by tests.
    pub fn sidebar_pane(&self, id: &str) -> Option<AnyView> {
        self.pane_views.get(id).map(|(_, view)| view.clone())
    }

    /// The current layout, as it would be saved.
    pub fn layout(&self, cx: &App) -> WorkbenchLayout {
        WorkbenchLayout {
            version: LAYOUT_VERSION,
            sidebar: SidebarLayout {
                width: self.sidebar_width.as_f32(),
                pane: self.sidebar_pane.as_ref().map(ToString::to_string),
            },
            dock: self.dock.read(cx).snapshot(cx),
        }
    }

    /// Save shortly, coalescing bursts such as a resize drag.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.pending_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            this.update(cx, |this, cx| this.save_now(cx)).ok();
        }));
    }

    /// Save the layout now.
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        self.pending_save = None;
        self.save_to_store(cx);
    }

    fn save_to_store(&self, cx: &App) {
        let layout = self.layout(cx);
        if let Err(err) = self.store.save(&self.layout_key, &layout) {
            eprintln!("coqo: couldn't save the layout: {err}");
        }
    }

    fn select_pane(&mut self, id: SharedString, cx: &mut Context<Self>) {
        if self.sidebar_pane.as_ref() != Some(&id) {
            self.sidebar_pane = Some(id);
            self.schedule_save(cx);
            cx.notify();
        }
    }

    fn resize_sidebar(&mut self, e: &DragMoveEvent<DraggedSidebarEdge>, cx: &mut Context<Self>) {
        let m = &cx.tokens().metrics;
        let sidebar_left = e.bounds.left() + m.rail_width + m.gap;
        let width =
            (e.event.position.x - sidebar_left - m.gap / 2.).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        if width != self.sidebar_width {
            self.sidebar_width = width;
            self.schedule_save(cx);
            cx.notify();
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let workspace_name: Option<SharedString> = self
            .workspaces
            .read(cx)
            .current()
            .map(|c| c.name.clone().into());
        let top = top_bar(workspace_name, window, cx);
        let sidebar = self.render_sidebar(window, cx);
        let status = status_bar(window, cx);
        let rail = rail(&self.workspaces, cx);
        let workspaces = self.workspaces.downgrade();
        div()
            .id("workbench")
            .on_action(move |_: &OpenFolder, _, cx| prompt_open_folder_in(workspaces.clone(), cx))
            .on_action(cx.listener(|this, _: &chat::NewChat, window, cx| {
                chat::new_chat(this.dock.downgrade(), window, cx)
            }))
            .on_action(
                cx.listener(|this, location: &chat::OpenLocation, window, cx| {
                    chat::open_location(this.dock.downgrade(), location, window, cx)
                }),
            )
            .size_full()
            .flex()
            .flex_col()
            .p(m.gap)
            .gap(m.gap)
            .bg(p.ground.hsla())
            .font_family(m.font_ui.clone())
            .text_size(m.text_md)
            .text_color(p.text.hsla())
            .on_action(
                cx.listener(|this, _: &crate::terminal::NewTerminal, window, cx| {
                    crate::terminal::open_new(&this.dock, window, cx);
                }),
            )
            .child(top)
            .child(
                div()
                    .id("body")
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .on_drag_move(cx.listener(
                        |this, e: &DragMoveEvent<DraggedSidebarEdge>, _, cx| {
                            this.resize_sidebar(e, cx)
                        },
                    ))
                    .child(rail)
                    .child(div().w(m.gap).flex_none())
                    .child(sidebar)
                    .child(
                        div()
                            .id("sidebar-handle")
                            .test_support()
                            .w(m.gap)
                            .h_full()
                            .flex_none()
                            .cursor_col_resize()
                            .on_drag(DraggedSidebarEdge, |_, _, _, cx| cx.new(|_| EmptyDrag)),
                    )
                    .child(
                        div()
                            .id("main")
                            .test_support()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .child(self.dock.clone()),
                    ),
            )
            .child(status)
    }
}

impl Shell {
    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let panes = SidebarPanes::all(cx);
        let active = self
            .sidebar_pane
            .clone()
            .filter(|id| panes.iter().any(|p| &p.id == id))
            .or_else(|| panes.first().map(|p| p.id.clone()));

        let mut strip = div().flex().gap_1().p(m.gap);
        for pane in &panes {
            let id = pane.id.clone();
            let selected = active.as_ref() == Some(&id);
            strip = strip.child(
                tab(
                    ElementId::Name(format!("sidebar-{id}").into()),
                    pane.title.clone(),
                    selected,
                    cx,
                )
                .test_support()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.select_pane(id.clone(), cx))),
            );
        }

        let content = active.and_then(|id| {
            let pane = panes.iter().find(|p| p.id == id)?;
            let fresh = self
                .pane_views
                .get(&id)
                .is_some_and(|(build, _)| Rc::ptr_eq(build, &pane.build));
            if !fresh {
                let view = (pane.build)(self.dock.downgrade(), window, cx);
                self.pane_views
                    .insert(id.clone(), (pane.build.clone(), view));
            }
            self.pane_views.get(&id).map(|(_, view)| view.clone())
        });

        panel(p.panel_raised, cx)
            .id("sidebar")
            .test_support()
            .w(self.sidebar_width)
            .flex_none()
            .child(strip)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .px(m.gap)
                    .children(content),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(m.gap)
                    .pb(px(14.))
                    .child(chat::list::render_section(self.dock.downgrade(), cx)),
            )
            .into_any_element()
    }
}

fn region(id: &'static str, el: Div) -> AnyElement {
    el.id(id).test_support().into_any_element()
}

/// A dock showing `layout` (or empty), with the workbench's empty state and
/// new-tab behaviour.
fn new_dock(layout: Option<&WorkbenchLayout>, window: &mut Window, cx: &mut App) -> Entity<Dock> {
    cx.new(|cx| {
        let mut dock = match layout {
            Some(layout) => Dock::restore(layout.dock.clone(), window, cx),
            None => Dock::new(cx),
        };
        dock.set_empty_state(empty_state);
        dock.set_new_tab(|_, cx| welcome::new(cx));
        dock
    })
}

fn top_bar(workspace_name: Option<SharedString>, window: &mut Window, cx: &mut App) -> AnyElement {
    let start = Slots::render(Slot::TopBarStart, window, cx);
    let end = Slots::render(Slot::TopBarEnd, window, cx);
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);

    let brand = div().flex().items_center().gap(px(10.)).child(
        div()
            .size(px(18.))
            .rounded_full()
            .border_2()
            .border_color(p.sage.hsla()),
    );
    let brand = match workspace_name {
        Some(name) => brand.child(
            div()
                .id("workspace-name")
                .test_support()
                .font_weight(FontWeight::SEMIBOLD)
                .child(name),
        ),
        None => brand
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Coqo"))
            .child(
                div()
                    .id("workspace-name")
                    .test_support()
                    .text_color(p.text_subtle.hsla())
                    .child("No workspace open"),
            ),
    }
    .children(start);

    let search = div()
        .id("command-search")
        .test_support()
        .flex_1()
        .max_w(px(520.))
        .mx_auto()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .py(px(8.))
        .rounded(m.radius_field)
        .bg(p.panel_raised.hsla())
        .border_1()
        .border_color(p.border.hsla())
        .shadow(vec![BoxShadow {
            color: hsla(0., 0., 0., 0.35),
            offset: point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
            inset: true,
        }])
        .text_size(m.text_sm)
        .text_color(p.text_muted.hsla())
        .child(icon(Icon::Search, px(14.), p.text_muted.hsla()))
        .child(
            div()
                .flex_1()
                .child("Search files, symbols, commands, agents"),
        )
        .child(kbd(shortcut("Ctrl K"), cx));

    region(
        "top-bar",
        div()
            .flex()
            .items_center()
            .gap(px(14.))
            .px(px(6.))
            .py(px(2.))
            .child(brand)
            .child(search)
            .child(
                div()
                    .id("top-bar-end")
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .children(end),
            ),
    )
}

fn status_bar(window: &mut Window, cx: &mut App) -> AnyElement {
    let start = Slots::render(Slot::StatusBarStart, window, cx);
    let end = Slots::render(Slot::StatusBarEnd, window, cx);
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    region(
        "status-bar",
        div()
            .flex()
            .items_center()
            .gap(px(18.))
            .px(px(8.))
            .text_size(m.text_xs)
            .text_color(p.text_muted.hsla())
            .children(start)
            .child(div().flex_1())
            .children(end),
    )
}

/// The chrome items that exist before any mod does. The bolt, daemon, git and
/// agent items are replaced by their mods as those land.
fn register_builtin_slots(cx: &mut App) {
    Slots::register(cx, Slot::TopBarEnd, "bolt", 0, |_, cx| {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        // Nothing needs attention yet, so the bolt is calm (no sand glow).
        raised(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(14.))
                .py(px(7.))
                .rounded(m.radius_field)
                .text_size(m.text_sm)
                .text_color(p.text_muted.hsla()),
            cx,
        )
        .id("bolt")
        .test_support()
        .child(icon(Icon::Zap, px(15.), p.text_muted.hsla()))
        .child("Intellisense · off")
        .into_any_element()
    });
    Slots::register(cx, Slot::StatusBarStart, "branch", 0, |_, cx| {
        let p = &cx.tokens().palette;
        div()
            .id("status-branch")
            .flex()
            .items_center()
            .gap(px(6.))
            .child(icon(Icon::GitBranch, px(12.), p.text_muted.hsla()))
            .child("No repository")
            .into_any_element()
    });
    Slots::register(cx, Slot::StatusBarStart, "problems", 10, |_, cx| {
        let p = &cx.tokens().palette;
        div()
            .id("status-problems")
            .flex()
            .items_center()
            .gap(px(6.))
            .child(status_dot(p.sage, false))
            .child("0 problems")
            .into_any_element()
    });
    Slots::register(cx, Slot::StatusBarEnd, "agents", 0, |_, _| {
        div()
            .id("status-agents")
            .child("0 agents")
            .into_any_element()
    });
}

fn rail_button(id: &'static str, cx: &App) -> Stateful<Div> {
    let m = &cx.tokens().metrics;
    div()
        .id(id)
        .size(m.rail_button)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(m.radius_rail)
}

fn rail(workspaces: &Entity<Workspaces>, cx: &App) -> AnyElement {
    let buttons = workspace_buttons(workspaces, cx);
    let open_folder = workspaces.downgrade();
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let muted = p.text_muted.hsla();
    region(
        "workspace-rail",
        div()
            .w(m.rail_width)
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.))
            .py(px(6.))
            .children(buttons)
            .child(
                rail_button("add-workspace", cx)
                    .test_support()
                    .border_1()
                    .border_dashed()
                    .border_color(p.border_strong.hsla())
                    .cursor_pointer()
                    .child(icon(Icon::Plus, px(18.), muted))
                    .on_click(move |_, _, cx| prompt_open_folder_in(open_folder.clone(), cx)),
            )
            .child(div().flex_1())
            .child(rail_button("alerts", cx).child(icon(Icon::Bell, px(20.), muted)))
            .child(rail_button("settings", cx).child(icon(Icon::Settings, px(20.), muted))),
    )
}

/// One rail button per open workspace, the window's current one lit in sage.
fn workspace_buttons(workspaces: &Entity<Workspaces>, cx: &App) -> Vec<AnyElement> {
    let window_workspaces = workspaces.downgrade();
    let current = workspaces.read(cx).current().cloned();
    let mut list: Vec<_> = KnownWorkspaces::global(cx)
        .map(|known| known.read(cx).open_workspaces().cloned().collect())
        .unwrap_or_default();
    if let Some(cur) = &current
        && !list.iter().any(|w| w.id == cur.id)
    {
        list.insert(0, cur.clone());
    }
    let current = current.map(|w| w.id);
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    list.into_iter()
        .enumerate()
        .map(|(ix, info)| {
            let selected = current.as_ref() == Some(&info.id);
            let base = div()
                .id(("workspace", ix))
                .test_support()
                .size(m.rail_button)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(m.radius_rail)
                .text_size(m.text_md)
                .font_weight(FontWeight::BOLD)
                .cursor_pointer();
            let base = if selected {
                base.bg(p.sage.alpha(0.22))
                    .border_1()
                    .border_color(p.sage.alpha(0.4))
                    .text_color(p.text.hsla())
            } else {
                base.bg(p.surface.hsla())
                    .border_1()
                    .border_color(p.border.hsla())
                    .text_color(p.text_muted.hsla())
                    .hover(|s| s.bg(p.surface_strong.hsla()))
            };
            let id = info.id.clone();
            let workspaces = window_workspaces.clone();
            base.child(initials(&info.name))
                .on_click(move |_, _, cx| {
                    let id = id.clone();
                    workspaces.update(cx, |w, cx| w.switch_to(id, cx)).ok();
                })
                .into_any_element()
        })
        .collect()
}

fn empty_note(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = cx.tokens();
    div()
        .px_2()
        .py_1()
        .text_size(t.metrics.text_sm)
        .text_color(t.palette.text_subtle.hsla())
        .child(text.into())
}

/// What an empty tab area shows.
fn empty_state(_: &mut Window, cx: &mut App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    div()
        .id("empty-state")
        .test_support()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .child(icon(Icon::MessageCircle, px(28.), p.text_subtle.hsla()))
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Nothing open yet"),
        )
        .child(
            div().text_color(p.text_muted.hsla()).child(
                "Open a folder to start a workspace, then chat with an agent or open a file.",
            ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(m.text_sm)
                .text_color(p.text_subtle.hsla())
                .child("Command palette")
                .child(kbd(shortcut("Ctrl K"), cx)),
        )
        .into_any_element()
}

/// A sidebar pane with a section label and a note, for panes whose content
/// hasn't been built yet.
struct NotePane {
    id: &'static str,
    label: &'static str,
    note: &'static str,
}

impl NotePane {
    fn new(id: &'static str, label: &'static str, note: &'static str) -> Self {
        Self { id, label, note }
    }
}

impl Render for NotePane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(self.id)
            .test_support()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_label(self.label, cx))
            .child(empty_note(self.note, cx))
    }
}

struct EmptyDrag;

impl Render for EmptyDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[cfg(test)]
mod tests;

/// Renders the shell offscreen to PNGs for visual review against board 1.
/// Needs a GPU: `cargo test -p coqo -- --ignored shell_screenshots`.
/// Writes to `$COQO_SHOT_DIR` (default: the system temp dir).
#[cfg(test)]
mod screenshots {
    use std::rc::Rc;

    use coqo_ui::{
        Appearance,
        dock::{Edge, store::MemoryLayoutStore},
        gpui::{AppContext as _, size},
        test::render_offscreen,
    };

    use super::{DEFAULT_WORKSPACE, Shell};
    use crate::welcome;

    #[test]
    #[ignore = "needs a GPU; run manually"]
    fn shell_screenshots() {
        let dir = std::env::var_os("COQO_SHOT_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for split in [false, true] {
            for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
                let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
                    super::init(cx);
                    let store = Rc::new(MemoryLayoutStore::default());
                    cx.new(|cx| {
                        let shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                        if split {
                            shell.dock().update(cx, |dock, cx| {
                                dock.open_tab(welcome::new(cx), window, cx);
                                dock.split(Edge::Right, window, cx);
                                dock.split(Edge::Bottom, window, cx);
                            });
                        }
                        shell
                    })
                })
                .expect("offscreen render");
                let suffix = if split { "-split" } else { "" };
                let path = dir.join(format!("coqo-shell{suffix}-{name}.png"));
                image.save(&path).expect("save png");
                println!("wrote {}", path.display());
            }
        }
    }
}
