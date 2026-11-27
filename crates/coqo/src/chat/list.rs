// SPDX-License-Identifier: Apache-2.0

//! The sidebar's CHATS list: the current workspace's sessions from
//! `sessions/list`, kept current by `sessions/changes` (state dots, titles,
//! the agent's status line, closed sessions dropping out).

use coqo_protocol::{
    codes,
    methods::agents::{
        Changes, ChangesParams, List, ListSessionsParams, ProviderAction, SessionId, SessionInfo,
    },
};
use coqo_ui::{
    ActiveTokens as _,
    dock::Dock,
    gpui::{
        AnyElement, App, AppContext as _, Context, ElementId, Entity, Global,
        InteractiveElement as _, IntoElement as _, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, div, px,
    },
    widgets::{Icon, TestSupportExt as _, icon, primary, section_label},
};

use super::{KIND, agent_dot, new_chat, open_session, press_action, status_line};
use crate::{
    daemon::{Daemon, DaemonEvent},
    workspace::{KnownWorkspaces, WindowWorkspaceChanged, Workspaces},
};

/// The current workspace's chats.
pub struct ChatList {
    workspace: Option<String>,
    /// Most recently updated first; closed sessions are left out.
    sessions: Vec<SessionInfo>,
    /// Why the last "new chat" (or the list) failed.
    error: Option<String>,
    /// What the user can do about it: a provider's setup or sign-in
    /// buttons, as `(provider id, action)`.
    actions: Vec<(String, ProviderAction)>,
    creating: bool,
    _loading: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ChatList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatList")
            .field("workspace", &self.workspace)
            .field("sessions", &self.sessions.len())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

struct GlobalChatList(Entity<ChatList>);

impl Global for GlobalChatList {}

/// Create the app's [`ChatList`]. Call after the daemon and workspaces.
pub fn init(cx: &mut App) -> Entity<ChatList> {
    let list = cx.new(ChatList::new);
    cx.set_global(GlobalChatList(list.clone()));
    list
}

impl ChatList {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| match event {
                DaemonEvent::Connected => this.reload(cx),
                DaemonEvent::Disconnected => this._loading = None,
            }));
        }
        // The list is app-wide and follows the focused window's workspace.
        if let Some(known) = KnownWorkspaces::global(cx) {
            subscriptions.push(
                cx.subscribe(&known, |this, _, _: &WindowWorkspaceChanged, cx| {
                    this.reload(cx);
                }),
            );
        }
        let mut this = Self {
            workspace: None,
            sessions: Vec::new(),
            error: None,
            actions: Vec::new(),
            creating: false,
            _loading: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// The app's chat list, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<ChatList>> {
        cx.try_global::<GlobalChatList>().map(|g| g.0.clone())
    }

    /// The listed sessions, most recently updated first.
    pub fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    /// One session's latest info, if listed.
    pub fn info(&self, id: &SessionId) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| &s.id == id)
    }

    /// Why the last "new chat" failed.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        self.actions.clear();
        cx.notify();
    }

    /// Show `error` with `provider`'s buttons for it.
    pub fn set_problem(
        &mut self,
        error: String,
        provider: String,
        actions: Vec<ProviderAction>,
        cx: &mut Context<Self>,
    ) {
        self.error = Some(error);
        self.actions = actions.into_iter().map(|a| (provider.clone(), a)).collect();
        cx.notify();
    }

    /// The buttons under the error.
    pub fn actions(&self) -> &[(String, ProviderAction)] {
        &self.actions
    }

    pub fn set_creating(&mut self, creating: bool, cx: &mut Context<Self>) {
        self.creating = creating;
        cx.notify();
    }

    /// Add or update a session (closed ones drop out).
    pub fn upsert(&mut self, info: SessionInfo, cx: &mut Context<Self>) {
        if info.workspace != self.workspace {
            return;
        }
        self.sessions.retain(|s| s.id != info.id);
        if !info.closed {
            let at = self
                .sessions
                .iter()
                .position(|s| s.updated_at <= info.updated_at)
                .unwrap_or(self.sessions.len());
            self.sessions.insert(at, info);
        }
        cx.notify();
    }

    /// List the current workspace's sessions and follow their changes.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.workspace =
            Workspaces::global(cx).and_then(|w| w.read(cx).current().map(|c| c.id.0.clone()));
        self.sessions.clear();
        cx.notify();
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            self._loading = None;
            return;
        };
        let Some(workspace) = self.workspace.clone() else {
            self._loading = None;
            return;
        };
        let list = daemon.read(cx).request::<List>(ListSessionsParams {
            workspace: Some(workspace.clone()),
            include_closed: false,
        });
        let daemon = daemon.downgrade();
        self._loading = Some(cx.spawn(async move |this, cx| {
            let listed = match list.await {
                Ok(listed) => listed,
                Err(e) => {
                    eprintln!("coqo: couldn't list chats: {e}");
                    return;
                }
            };
            let seq = listed.seq;
            let changes = this.update(cx, |this, cx| {
                this.sessions = listed.sessions;
                cx.notify();
                daemon.upgrade().map(|d| {
                    d.read(cx).subscribe_since::<Changes>(
                        ChangesParams {
                            workspace: Some(workspace),
                        },
                        seq,
                    )
                })
            });
            let Ok(Some(changes)) = changes else { return };
            let Ok(mut stream) = changes.await else {
                return;
            };
            while let Some(item) = stream.next().await {
                let alive = this.update(cx, |this, cx| match item {
                    Ok(change) => this.upsert(change.event, cx),
                    Err(e) if e.code() == Some(codes::RESUME_GAP) => this.reload(cx),
                    Err(_) => {}
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }
}

/// The CHATS section at the bottom of the sidebar.
pub fn render_section(dock: WeakEntity<Dock>, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (t.palette.clone(), t.metrics.clone());
    let list = ChatList::global(cx);
    let active = dock.upgrade().and_then(|dock| {
        let dock = dock.read(cx);
        let tab = dock.tab(dock.active_tab()?)?;
        (tab.kind(cx) == KIND).then(|| tab.key(cx)).flatten()
    });
    let add_dock = dock.clone();
    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .child(section_label("Chats", cx))
        .child(
            div()
                .id("chat-new")
                .test_support()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(p.surface_selected.hsla()))
                .child(icon(Icon::Plus, px(13.), p.text_muted.hsla()))
                .on_click(move |_, window, cx| new_chat(add_dock.clone(), window, cx)),
        );
    let mut section = div()
        .id("sidebar-chats")
        .test_support()
        .flex()
        .flex_col()
        .gap(px(2.))
        .text_size(px(12.))
        .child(header);
    let (sessions, error, actions) = match &list {
        Some(list) => {
            let list = list.read(cx);
            (
                list.sessions().to_vec(),
                list.error().map(str::to_owned),
                list.actions().to_vec(),
            )
        }
        None => (Vec::new(), None, Vec::new()),
    };
    if sessions.is_empty() && error.is_none() {
        section = section.child(
            div()
                .px_2()
                .py_1()
                .text_size(m.text_sm)
                .text_color(p.text_subtle.hsla())
                .child("No chats yet"),
        );
    }
    for (ix, info) in sessions.into_iter().enumerate() {
        let selected = active.as_deref() == Some(info.id.0.as_str());
        let title = info
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "New chat".into());
        let (dock, id) = (dock.clone(), info.id.clone());
        let line = status_line(&info);
        section = section.child(
            div()
                .id(ElementId::Name(format!("chat-row-{}", info.id).into()))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(6.))
                .rounded(px(9.))
                .cursor_pointer()
                .when_selected(selected, &p)
                .test_support()
                .child(agent_dot(info.state, ("chat-row-dot", ix), cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().child(title))
                        .children(line.map(|line| {
                            div()
                                .id(("chat-row-status", ix))
                                .test_support()
                                .truncate()
                                .text_size(m.text_label)
                                .text_color(p.text_subtle.hsla())
                                .child(line)
                        })),
                )
                .on_click(move |_, window, cx| open_session(dock.clone(), id.clone(), window, cx)),
        );
    }
    if let Some(error) = error {
        section = section.child(
            div()
                .id("chat-list-error")
                .test_support()
                .px_2()
                .py_1()
                .text_size(m.text_sm)
                .text_color(p.sand.hsla())
                .child(error),
        );
    }
    if !actions.is_empty() {
        let mut row = div().flex().flex_wrap().gap(px(6.)).px_2().pb_1();
        for (provider, action) in actions {
            let dock = dock.clone();
            let label = action.label.clone();
            row = row.child(
                primary(
                    div()
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .text_size(m.text_sm),
                    cx,
                )
                .id(ElementId::Name(format!("chat-action-{}", action.id).into()))
                .test_support()
                .cursor_pointer()
                .child(label)
                .on_click(move |_, window, cx| {
                    press_action(dock.clone(), provider.clone(), action.clone(), window, cx)
                }),
            );
        }
        section = section.child(row);
    }
    section.into_any_element()
}

trait RowExt {
    fn when_selected(self, selected: bool, p: &coqo_ui::Palette) -> Self;
}

impl RowExt for coqo_ui::gpui::Stateful<coqo_ui::gpui::Div> {
    fn when_selected(self, selected: bool, p: &coqo_ui::Palette) -> Self {
        if selected {
            self.bg(p.surface_selected.hsla()).text_color(p.text.hsla())
        } else {
            self.text_color(p.text_muted.hsla())
                .hover(|s| s.bg(p.surface.hsla()))
        }
    }
}
