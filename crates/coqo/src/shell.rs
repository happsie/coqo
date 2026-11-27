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

use coqo_protocol::methods::{
    agents::SessionId,
    alerts::{Alert, AlertTarget, FileLocation},
    editor::{Position, Range},
    workspace::{RootId, WorkspaceId},
};

use coqo_ui::{
    ActiveTokens as _,
    dock::{
        AnyTab, Dock, DockEvent,
        store::{LAYOUT_VERSION, LayoutStore, SidebarLayout, WorkbenchLayout},
    },
    gpui::{
        AnyElement, AnyView, AnyWindowHandle, App, AppContext as _, BoxShadow, Context, Div,
        DragMoveEvent, ElementId, Empty, Entity, Focusable as _, FontWeight, Global,
        InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window,
        deferred, div, hsla, point, px,
    },
    slots::{PaneBuilder, SidebarPanes, Slot, Slots},
    widgets::{Icon, TestSupportExt as _, icon, kbd, panel, raised, status_dot, tab},
};

use crate::{
    activity,
    alerts::{self, AlertCenter, Raised, TOAST_FOR},
    chat,
    daemon::{self, Daemon, DaemonEvent},
    files::{
        self,
        context::{ContextPublisher, PublishContext, RevealHub, RevealRequest},
        editor::{self as file_editor, FileEditor},
    },
    layout::{DaemonLayoutStore, LAYOUT_KEY},
    questions::{self, QuestionsEvent, navigate, popover::OpenQuestionChat},
    welcome::{self, shortcut},
    workspace::{
        KnownWorkspaces, OpenFolder, SwitchToWorkspace, Workspaces, WorkspacesEvent,
        prompt_open_folder_in,
    },
};

pub mod rail;

use rail::{Rail, RailEvent};

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
    questions::init(cx);
    activity::init(cx);
    crate::git::init(cx);
    alerts::init(cx);
}

/// The drag payload of the sidebar's resize edge.
#[derive(Clone, Copy, Debug)]
struct DraggedSidebarEdge;

/// The root view of a workbench window.
pub struct Shell {
    dock: Entity<Dock>,
    store: Rc<dyn LayoutStore>,
    workspaces: Entity<Workspaces>,
    rail: Entity<Rail>,
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
    /// This window.
    window: AnyWindowHandle,
    /// Publishes the window's editor context for agents (`editor/context`).
    context: ContextPublisher,
    /// The alert shown as a toast, if any.
    toast: Option<Alert>,
    _toast_timer: Option<Task<()>>,
    /// An alert to open once the window shows its workspace.
    pending_alert: Option<Alert>,
    /// The current workspace's questions, for the popovers of questions an
    /// agent starts waiting for.
    _questions: Option<Subscription>,
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
            cx.on_release(|this, cx| {
                this.save_to_store(cx);
                this.context.withdraw(cx);
            }),
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                this.context.set_focused(window.is_window_active());
                ContextPublisher::schedule(this, cx);
            }),
            cx.observe(&workspaces, |_, _, cx| cx.notify()),
            cx.subscribe_in(&workspaces, window, |this, _, event, window, cx| {
                if *event == WorkspacesEvent::CurrentChanged {
                    this.workspace_changed(window, cx);
                    this.follow_questions(window, cx);
                    ContextPublisher::schedule(this, cx);
                    this.open_pending_alert(window, cx);
                }
            }),
        ];
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.observe(&daemon, |_, _, cx| cx.notify()));
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| {
                if *event == DaemonEvent::Connected {
                    this.context.reconnected();
                    ContextPublisher::schedule(this, cx);
                }
            }));
        }
        if let Some(hub) = RevealHub::global(cx) {
            subscriptions.push(cx.subscribe_in(
                &hub,
                window,
                |this, _, request: &RevealRequest, window, cx| {
                    if request.window == this.window {
                        this.reveal(request, window, cx);
                    }
                },
            ));
        }
        if let Some(known) = KnownWorkspaces::global(cx) {
            subscriptions.push(cx.observe(&known, |_, _, cx| cx.notify()));
        }
        if let Some(center) = AlertCenter::global(cx) {
            subscriptions.push(cx.observe(&center, |_, _, cx| cx.notify()));
            subscriptions.push(cx.subscribe_in(
                &center,
                window,
                |this, _, Raised(alert), window, cx| this.maybe_toast(alert, window, cx),
            ));
        }
        let focus = dock.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let sidebar = saved.map(|l| l.sidebar);
        let rail = cx.new(|cx| Rail::new(workspaces.clone(), cx));
        subscriptions.push(cx.subscribe_in(
            &rail,
            window,
            |this, _, event: &RailEvent, window, cx| match event {
                RailEvent::OpenAlert(alert) => this.open_alert(alert.clone(), window, cx),
            },
        ));
        ShellWindows::add(window.window_handle(), cx.entity().downgrade(), cx);
        let mut this = Self {
            _dock_events: dock_events(&dock, cx),
            window: window.window_handle(),
            context: ContextPublisher::new(window),
            dock,
            store,
            workspaces,
            rail,
            layout_key: local_key.clone(),
            local_key,
            sidebar_width: sidebar.as_ref().map_or_else(
                || cx.tokens().metrics.sidebar_width,
                |s| px(s.width).clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            ),
            sidebar_pane: sidebar.and_then(|s| s.pane).map(Into::into),
            pane_views: HashMap::new(),
            pending_save: None,
            toast: None,
            _toast_timer: None,
            pending_alert: None,
            _questions: None,
            _subscriptions: subscriptions,
        };
        this.follow_questions(window, cx);
        ContextPublisher::schedule(&mut this, cx);
        this
    }

    /// Follow the current workspace's questions: a question an agent starts
    /// waiting for opens its popover in a file tab on screen.
    fn follow_questions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.workspaces.read(cx).current().map(|w| w.id.clone());
        let store = current.and_then(|id| questions::questions_for(&id, cx));
        self._questions = store.map(|store| {
            cx.subscribe_in(&store, window, |this, _, event, window, cx| {
                if let QuestionsEvent::Waiting(id) = event {
                    navigate::show_waiting(&this.dock, &this.workspaces, *id, window, cx);
                }
            })
        });
    }

    /// Open the file of an agent's `editor/reveal` (or show its tab) and
    /// select the range.
    fn reveal(&mut self, request: &RevealRequest, window: &mut Window, cx: &mut Context<Self>) {
        let event = &request.event;
        if self.workspaces.read(cx).current().map(|w| &w.id) != Some(&event.workspace) {
            return;
        }
        self.open_file_at(
            event.workspace.clone(),
            event.root,
            event.path.clone(),
            event.range,
            window,
            cx,
        );
    }

    /// Open a file of the window's workspace (or show its tab) and select
    /// `range` (the top if `None`).
    fn open_file_at(
        &mut self,
        workspace: WorkspaceId,
        root: RootId,
        path: String,
        range: Option<Range>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<FileEditor>> {
        let dock = self.dock.clone();
        let key = file_editor::key(&workspace, root, &path);
        let open = dock.read(cx).find_tab(file_editor::KIND, &key, cx);
        let file = match open {
            Some(id) => {
                dock.update(cx, |dock, cx| dock.activate_tab(id, window, cx));
                dock.read(cx)
                    .tab(id)
                    .and_then(AnyTab::downcast::<FileEditor>)
            }
            None => {
                let tab: Entity<FileEditor> =
                    cx.new(|cx| FileEditor::new(workspace, root, path, window, cx));
                dock.update(cx, |dock, cx| {
                    dock.open_tab(AnyTab::new(tab.clone()), window, cx);
                });
                Some(tab)
            }
        };
        if let Some(file) = &file {
            file.update(cx, |file, cx| file.reveal(range, window, cx));
        }
        file
    }

    // ---- alerts ------------------------------------------------------------

    /// The agent sessions whose chats are on screen: the shown tab of each
    /// tab group.
    pub fn visible_sessions(&self, cx: &App) -> Vec<SessionId> {
        let dock = self.dock.read(cx);
        dock.tree()
            .groups()
            .into_iter()
            .filter_map(|g| g.active_tab())
            .filter_map(|id| dock.tab(id))
            .filter(|tab| tab.kind(cx) == chat::KIND)
            .filter_map(|tab| tab.key(cx))
            .map(|key| SessionId(key.to_string()))
            .collect()
    }

    /// The toast shown now, if any.
    #[allow(dead_code)] // Tests use it.
    pub fn toast(&self) -> Option<&Alert> {
        self.toast.as_ref()
    }

    /// Show `alert` as the toast, for screenshots.
    #[cfg(test)]
    pub fn show_toast_for_tests(&mut self, alert: Alert) {
        self.toast = Some(alert);
    }

    /// Show a new alert that needs you as a toast, in the app's active
    /// window only, unless this window shows its session's chat or
    /// do-not-disturb is on.
    fn maybe_toast(&mut self, alert: &Alert, window: &mut Window, cx: &mut Context<Self>) {
        let quiet = AlertCenter::global(cx).is_none_or(|c| c.read(cx).do_not_disturb());
        let active = Workspaces::global(cx).map(|w| w.entity_id());
        let shown = alert
            .session
            .as_ref()
            .is_some_and(|s| self.visible_sessions(cx).contains(s));
        if !alert.kind.needs_you() || quiet || shown || active != Some(self.workspaces.entity_id())
        {
            return;
        }
        self.toast = Some(alert.clone());
        let id = alert.id;
        self._toast_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(TOAST_FOR).await;
            this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|t| t.id == id) {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn hide_toast(&mut self, cx: &mut Context<Self>) {
        if self.toast.take().is_some() {
            self._toast_timer = None;
            cx.notify();
        }
    }

    /// Open where `alert` points (the chat tab, the question's code, a
    /// file) and mark it read. An alert of another workspace switches this
    /// window to it first, or focuses the window that shows it.
    pub fn open_alert(&mut self, alert: Alert, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(center) = AlertCenter::global(cx) {
            center.update(cx, |c, cx| c.mark_read(vec![alert.id], cx));
        }
        self.hide_toast(cx);
        let current = self.workspaces.read(cx).current().map(|w| w.id.clone());
        let Some(workspace) = alert
            .workspace
            .clone()
            .filter(|w| Some(w) != current.as_ref())
        else {
            self.follow_alert(&alert, window, cx);
            return;
        };
        if let Some((handle, shell)) = ShellWindows::showing(&workspace, cx) {
            handle
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    shell.update(cx, |shell, cx| shell.follow_alert(&alert, window, cx));
                })
                .ok();
            return;
        }
        self.pending_alert = Some(alert);
        self.workspaces
            .update(cx, |w, cx| w.switch_to(workspace, cx));
    }

    fn open_pending_alert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.workspaces.read(cx).current().map(|w| w.id.clone());
        if self
            .pending_alert
            .as_ref()
            .is_some_and(|a| a.workspace.is_some() && a.workspace == current)
            && let Some(alert) = self.pending_alert.take()
        {
            self.follow_alert(&alert, window, cx);
        }
    }

    /// Open where `alert` points, in this window's workspace.
    fn follow_alert(&mut self, alert: &Alert, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = alert
            .workspace
            .clone()
            .or_else(|| self.workspaces.read(cx).current().map(|w| w.id.clone()));
        let chat_of = |session: &SessionId, this: &Self, window: &mut Window, cx: &mut App| {
            chat::open_session(this.dock.downgrade(), session.clone(), window, cx);
        };
        match &alert.target {
            Some(AlertTarget::Session { session })
            | Some(AlertTarget::Question {
                session,
                location: None,
                ..
            }) => chat_of(session, self, window, cx),
            // The question's code, with its popover open (once the
            // question is listed in the tab's workspace).
            Some(AlertTarget::Question {
                question,
                location: Some(location),
                ..
            }) => {
                if let Some(workspace) = workspace {
                    let (root, path, range) = file_target(location);
                    let file = self.open_file_at(workspace, root, path, range, window, cx);
                    if let Some(file) = file {
                        file.update(cx, |file, cx| {
                            file.show_question(*question, true, true, window, cx)
                        });
                    }
                }
            }
            Some(AlertTarget::File { location }) => {
                if let Some(workspace) = workspace {
                    let (root, path, range) = file_target(location);
                    self.open_file_at(workspace, root, path, range, window, cx);
                }
            }
            None => {
                if let Some(session) = &alert.session {
                    chat_of(session, self, window, cx);
                }
            }
        }
    }

    fn render_toast(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Gone once it is read, resolved or dismissed (here or elsewhere).
        let center = AlertCenter::global(cx)?;
        let id = self.toast.as_ref()?.id;
        let Some(alert) = center.read(cx).get(id).filter(|a| a.is_unread()).cloned() else {
            self.toast = None;
            self._toast_timer = None;
            return None;
        };
        let (open, later) = (cx.entity().downgrade(), cx.entity().downgrade());
        let card = alerts::view::toast(
            &alert,
            Rc::new(move |alert, window, cx| {
                let alert = alert.clone();
                open.update(cx, |shell, cx| shell.open_alert(alert, window, cx))
                    .ok();
            }),
            Rc::new(move |_, cx| {
                later.update(cx, |shell, cx| shell.hide_toast(cx)).ok();
            }),
            cx,
        );
        Some(
            deferred(div().absolute().right(px(18.)).bottom(px(40.)).child(card))
                .with_priority(4)
                .into_any_element(),
        )
    }

    /// The window's workspace.
    #[allow(dead_code)] // Used by tests.
    pub fn workspaces(&self) -> &Entity<Workspaces> {
        &self.workspaces
    }

    /// The window's workspace rail.
    #[allow(dead_code)] // Used by tests.
    pub fn rail(&self) -> &Entity<Rail> {
        &self.rail
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
            // The workspace was removed: nothing to save it to. Show the
            // window's own layout (or the default).
            if self.layout_key != self.local_key {
                self.pending_save = None;
                self.layout_key = self.local_key.clone();
                let saved = self.store.load(&self.local_key).unwrap_or_else(|err| {
                    eprintln!("coqo: couldn't load the saved layout, using the default: {err}");
                    None
                });
                self.apply_layout(saved, window, cx);
            }
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
        self._dock_events = dock_events(&dock, cx);
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

/// The dock changed: save the layout and publish the editor context.
fn dock_events(dock: &Entity<Dock>, cx: &mut Context<Shell>) -> Subscription {
    cx.subscribe(dock, |this, _, _: &DockEvent, cx| {
        this.schedule_save(cx);
        ContextPublisher::schedule(this, cx);
    })
}

impl PublishContext for Shell {
    fn publisher(&mut self) -> &mut ContextPublisher {
        &mut self.context
    }

    fn shown(&self, cx: &App) -> Option<(Entity<Dock>, WorkspaceId)> {
        let workspace = self.workspaces.read(cx).current()?.id.clone();
        Some((self.dock.clone(), workspace))
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
        let rail = self.rail.clone();
        let workspaces = self.workspaces.downgrade();
        // Chats on screen in a focused window: their alerts are read.
        if window.is_window_active()
            && let Some(center) = AlertCenter::global(cx)
        {
            let visible = self.visible_sessions(cx);
            if !visible.is_empty() {
                cx.defer(move |cx| center.update(cx, |c, cx| c.seen(&visible, cx)));
            }
        }
        let toast = self.render_toast(cx);
        div()
            .id("workbench")
            .relative()
            .on_action(move |_: &OpenFolder, _, cx| prompt_open_folder_in(workspaces.clone(), cx))
            .on_action(cx.listener(|this, action: &SwitchToWorkspace, _, cx| {
                this.rail
                    .update(cx, |rail, cx| rail.switch_to_index(action.index, cx))
            }))
            .on_action(cx.listener(|this, _: &chat::NewChat, window, cx| {
                chat::new_chat(this.dock.downgrade(), window, cx)
            }))
            .on_action(
                cx.listener(|this, location: &chat::OpenLocation, window, cx| {
                    chat::open_location(this.dock.downgrade(), location, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, action: &chat::RunInTerminal, window, cx| {
                    chat::run_in_terminal(&this.dock, action, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, action: &chat::OpenTerminal, window, cx| {
                    chat::open_terminal(&this.dock, action, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &chat::SessionSwitched, _, cx| this.schedule_save(cx)))
            .on_action(
                cx.listener(|this, _: &questions::NextQuestion, window, cx| {
                    navigate::step_question(&this.dock, &this.workspaces, true, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &questions::PreviousQuestion, window, cx| {
                    navigate::step_question(&this.dock, &this.workspaces, false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, action: &questions::OpenQuestion, window, cx| {
                    let id = coqo_protocol::methods::questions::QuestionId(action.id);
                    navigate::open_question(&this.dock, &this.workspaces, id, window, cx)
                }),
            )
            .on_action(cx.listener(|this, action: &OpenQuestionChat, window, cx| {
                let id = coqo_protocol::methods::questions::QuestionId(action.id);
                navigate::open_chat(&this.dock, &this.workspaces, id, window, cx)
            }))
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
            .children(toast)
    }
}

/// Where a file target opens: root, path and the line as a range.
fn file_target(location: &FileLocation) -> (RootId, String, Option<Range>) {
    let range = location.line.map(|line| {
        let at = Position { line, character: 0 };
        Range::new(at, at)
    });
    (location.root, location.path.clone(), range)
}

/// Every workbench window, to open an alert in the one that shows its
/// workspace.
#[derive(Default)]
struct ShellWindows(Vec<(AnyWindowHandle, WeakEntity<Shell>)>);

impl Global for ShellWindows {}

impl ShellWindows {
    fn add(handle: AnyWindowHandle, shell: WeakEntity<Shell>, cx: &mut App) {
        let windows = cx.default_global::<ShellWindows>();
        windows.0.retain(|(_, s)| s.upgrade().is_some());
        windows.0.push((handle, shell));
    }

    /// The window showing `workspace`, and its shell.
    fn showing(workspace: &WorkspaceId, cx: &App) -> Option<(AnyWindowHandle, Entity<Shell>)> {
        cx.try_global::<ShellWindows>()?
            .0
            .iter()
            .filter_map(|(h, s)| Some((*h, s.upgrade()?)))
            .find(|(_, s)| {
                s.read(cx).workspaces.read(cx).current().map(|w| &w.id) == Some(workspace)
            })
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
