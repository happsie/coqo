// SPDX-License-Identifier: Apache-2.0

//! The agents overview tab (board 6's "Agents" column). See the module docs
//! of [`crate::agents`] and `docs/design/agents.md`.

use std::collections::HashMap;
use std::time::Duration;

use coqo_protocol::{
    daemon::{DaemonStatus, Status},
    methods::{
        agents::{
            self as agm, AgentState, PermissionOptionKind, PermissionOutcome, PermissionRequest,
            RespondPermissionParams, SessionId, SessionInfo, SessionParams,
        },
        walkthroughs::WalkthroughSummary,
        workspace::WorkspaceId,
    },
};
use coqo_ui::{
    ActiveTokens as _, Appearance, Rgb,
    dock::{AnyTab, TabItem},
    gpui::{
        AnyElement, AnyWindowHandle, App, AppContext as _, Context, Div, Entity, FocusHandle,
        FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        Stateful, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
        prelude::FluentBuilder as _, px,
    },
    widgets::{Icon, TestSupportExt as _, icon, kbd, primary, raised},
};

use super::{
    AgentTarget, Confirm, Detail, Group, KEY_CONTEXT, KIND, SelectNext, SelectPrev, badge,
    criteria_line, detail, group, meta, ordered, questions_of, target_of, title, walkthrough_line,
    workspace_name,
};
use crate::{
    activity::AgentActivity,
    alerts::now_ms,
    daemon::{Daemon, DaemonEvent},
    questions::{WorkspaceQuestions, questions_for},
    shell::Shell,
    walkthrough::marks::{WorkspaceWalkthroughs, walkthroughs_for},
    welcome::shortcut,
    workspace::KnownWorkspaces,
};

/// How often the waiting times are redrawn, and the daemon's status (its
/// memory) fetched again.
const TICK: Duration = Duration::from_secs(30);

/// A new agents overview tab.
pub fn new(window: &mut Window, cx: &mut App) -> AnyTab {
    AnyTab::new(cx.new(|cx| AgentsTab::new(window, cx)))
}

/// A waiting permission request, as of the session's `updated_at`.
struct Ask {
    updated_at: i64,
    request: Option<PermissionRequest>,
    _fetch: Option<Task<()>>,
}

/// The agents overview. See the module docs.
pub struct AgentsTab {
    focus: FocusHandle,
    window: AnyWindowHandle,
    selected: Option<SessionId>,
    /// The questions of the workspaces that have sessions.
    stores: HashMap<WorkspaceId, Entity<WorkspaceQuestions>>,
    /// Permission requests of sessions that need permission.
    asks: HashMap<SessionId, Ask>,
    /// The walkthroughs of the workspaces that have sessions: a done
    /// session that published one offers **Walkthrough**.
    walkthroughs: HashMap<WorkspaceId, Entity<WorkspaceWalkthroughs>>,
    /// Walkthroughs by session, for screenshots without a daemon.
    #[cfg(test)]
    test_walkthroughs: HashMap<SessionId, WalkthroughSummary>,
    /// Provider names by id.
    providers: HashMap<String, String>,
    status: Option<DaemonStatus>,
    _fetches: Vec<Task<()>>,
    _status: Option<Task<()>>,
    _tick: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for AgentsTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentsTab")
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl AgentsTab {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(activity) = AgentActivity::global(cx) {
            subscriptions.push(cx.observe(&activity, |this, _, cx| this.sync(cx)));
        }
        if let Some(known) = KnownWorkspaces::global(cx) {
            subscriptions.push(cx.observe(&known, |_, _, cx| cx.notify()));
        }
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| {
                if *event == DaemonEvent::Connected {
                    this.fetch(cx);
                }
            }));
        }
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let tick = this.update(cx, |this, cx| {
                    this.fetch_status(cx);
                    cx.notify();
                });
                if tick.is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            focus: cx.focus_handle(),
            window: window.window_handle(),
            selected: None,
            stores: HashMap::new(),
            asks: HashMap::new(),
            walkthroughs: HashMap::new(),
            #[cfg(test)]
            test_walkthroughs: HashMap::new(),
            providers: HashMap::new(),
            status: None,
            _fetches: Vec::new(),
            _status: None,
            _tick: tick,
            _subscriptions: subscriptions,
        };
        this.fetch(cx);
        this.sync(cx);
        this
    }

    /// Fetch the provider names and the daemon's status.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let providers = daemon.read(cx).request::<agm::Providers>(());
        self._fetches = vec![cx.spawn(async move |this, cx| {
            if let Ok(list) = providers.await {
                this.update(cx, |this, cx| {
                    this.providers = list.into_iter().map(|p| (p.id, p.name)).collect();
                    cx.notify();
                })
                .ok();
            }
        })];
        self.fetch_status(cx);
    }

    /// Fetch the daemon's status: its uptime and memory.
    fn fetch_status(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let status = daemon.read(cx).request::<Status>(());
        self._status = Some(cx.spawn(async move |this, cx| {
            if let Ok(status) = status.await {
                this.update(cx, |this, cx| {
                    this.status = Some(status);
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// The daemon's status, once fetched.
    #[allow(dead_code)] // Tests use it.
    pub fn status(&self) -> Option<&DaemonStatus> {
        self.status.as_ref()
    }

    /// The header's facts about `rows` at `now`: "coqod up 3 days · 5
    /// sessions · 3 workspaces · 2.1 GB incl. language servers".
    pub fn facts(&self, rows: &[SessionInfo], now: i64) -> String {
        let workspaces = {
            let mut list: Vec<_> = rows.iter().filter_map(|r| r.workspace.as_ref()).collect();
            list.sort();
            list.dedup();
            list.len()
        };
        let mut facts = Vec::new();
        if let Some(status) = &self.status {
            let started = i64::try_from(status.started_at_ms).unwrap_or(now);
            facts.push(format!("coqod up {}", uptime(now - started)));
        }
        facts.push(match rows.len() {
            1 => "1 session".to_owned(),
            n => format!("{n} sessions"),
        });
        if workspaces > 1 {
            facts.push(format!("{workspaces} workspaces"));
        }
        // Board 6: the daemon with its child processes.
        if let Some(memory) = self.status.as_ref().and_then(|s| s.memory) {
            facts.push(format!(
                "{} incl. language servers",
                memory_size(memory.rss_bytes)
            ));
        }
        facts.join(" · ")
    }

    /// The sessions changed: follow the questions of new workspaces and
    /// fetch the permission requests of sessions that now need permission.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let sessions: Vec<SessionInfo> = AgentActivity::global(cx)
            .map(|a| a.read(cx).sessions().cloned().collect())
            .unwrap_or_default();
        for workspace in sessions.iter().filter_map(|s| s.workspace.clone()) {
            let id = WorkspaceId(workspace);
            if self.stores.contains_key(&id) {
                continue;
            }
            if let Some(store) = questions_for(&id, cx) {
                self._subscriptions
                    .push(cx.observe(&store, |_, _, cx| cx.notify()));
                self.stores.insert(id.clone(), store);
            }
            if let Some(store) = walkthroughs_for(&id, cx) {
                self._subscriptions
                    .push(cx.observe(&store, |_, _, cx| cx.notify()));
                self.walkthroughs.insert(id, store);
            }
        }
        self.asks.retain(|id, _| {
            sessions
                .iter()
                .any(|s| &s.id == id && s.state == AgentState::NeedsPermission)
        });
        for info in sessions
            .iter()
            .filter(|s| s.state == AgentState::NeedsPermission)
        {
            if self
                .asks
                .get(&info.id)
                .is_some_and(|a| a.updated_at == info.updated_at)
            {
                continue;
            }
            let fetch = Daemon::global(cx)
                .filter(|d| d.read(cx).is_connected())
                .map(|daemon| {
                    let get = daemon.read(cx).request::<agm::Get>(SessionParams {
                        session: info.id.clone(),
                    });
                    let id = info.id.clone();
                    cx.spawn(async move |this, cx| {
                        let Ok(snapshot) = get.await else { return };
                        this.update(cx, |this, cx| {
                            if let Some(ask) = this.asks.get_mut(&id) {
                                ask.request = snapshot
                                    .pending_permissions
                                    .into_iter()
                                    .find(|r| !r.is_question());
                                cx.notify();
                            }
                        })
                        .ok();
                    })
                });
            let request = self.asks.remove(&info.id).and_then(|a| a.request);
            self.asks.insert(
                info.id.clone(),
                Ask {
                    updated_at: info.updated_at,
                    request,
                    _fetch: fetch,
                },
            );
        }
        if let Some(selected) = &self.selected
            && !sessions.iter().any(|s| &s.id == selected)
        {
            self.selected = None;
        }
        cx.notify();
    }

    /// The sessions as listed, top to bottom.
    pub fn rows(&self, cx: &App) -> Vec<SessionInfo> {
        AgentActivity::global(cx)
            .map(|a| {
                ordered(a.read(cx).sessions())
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The selected session.
    #[allow(dead_code)] // Tests use it.
    pub fn selected(&self) -> Option<&SessionId> {
        self.selected.as_ref()
    }

    /// Name the providers and set waiting permission requests, published
    /// walkthroughs and the daemon's status, for screenshots without a
    /// daemon.
    #[cfg(test)]
    pub fn set_for_tests(
        &mut self,
        providers: &[(&str, &str)],
        asks: Vec<(SessionId, PermissionRequest)>,
        walkthroughs: Vec<WalkthroughSummary>,
        status: Option<DaemonStatus>,
        cx: &mut Context<Self>,
    ) {
        self.test_walkthroughs = walkthroughs
            .into_iter()
            .map(|w| (w.session.clone(), w))
            .collect();
        self.status = status;
        self.providers = providers
            .iter()
            .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
            .collect();
        for (session, request) in asks {
            self.asks.insert(
                session,
                Ask {
                    updated_at: i64::MIN,
                    request: Some(request),
                    _fetch: None,
                },
            );
        }
        cx.notify();
    }

    /// The waiting permission request of `session`, once fetched.
    #[allow(dead_code)] // Tests use it.
    pub fn ask(&self, session: &SessionId) -> Option<&PermissionRequest> {
        self.asks.get(session)?.request.as_ref()
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if rows.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|s| rows.iter().position(|r| &r.id == s));
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => rows.len() - 1,
            (Some(ix), true) => (ix + 1).min(rows.len() - 1),
            (Some(ix), false) => ix.saturating_sub(1),
        };
        self.selected = Some(rows[next].id.clone());
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let info = self
            .selected
            .as_ref()
            .and_then(|s| self.rows(cx).into_iter().find(|r| &r.id == s));
        if let Some(info) = info {
            let target = self.primary_target(&info, cx);
            self.open(&info, target, window, cx);
        }
    }

    /// The newest walkthrough of a done session, if it published one.
    pub fn walkthrough_of(&self, info: &SessionInfo, cx: &App) -> Option<WalkthroughSummary> {
        if info.state != AgentState::Done {
            return None;
        }
        #[cfg(test)]
        if let Some(w) = self.test_walkthroughs.get(&info.id) {
            return Some(w.clone());
        }
        let store = self
            .walkthroughs
            .get(&WorkspaceId(info.workspace.clone()?))?;
        store.read(cx).latest_of(&info.id).cloned()
    }

    /// Where the row's primary action (and Enter) leads: the question it
    /// waits on, a done session's walkthrough, else its chat.
    fn primary_target(&self, info: &SessionInfo, cx: &mut App) -> AgentTarget {
        match self.walkthrough_of(info, cx) {
            Some(w) => AgentTarget::Walkthrough {
                session: info.id.clone(),
                walkthrough: w.id,
            },
            None => target_of(info, cx),
        }
    }

    /// Open where `target` points, switching the window to the session's
    /// workspace first (or focusing the window that shows it).
    fn open(
        &mut self,
        info: &SessionInfo,
        target: AgentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(info.id.clone());
        cx.notify();
        let workspace = info.workspace.clone().map(WorkspaceId);
        let Some(shell) = Shell::of_window(self.window, cx) else {
            return;
        };
        // After this update: the dock reads every tab, this one too.
        window.defer(cx, move |window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_agent(workspace, target, window, cx);
            });
        });
    }

    fn respond(&mut self, session: SessionId, allow: bool, cx: &mut Context<Self>) {
        let Some(request) = self.asks.get(&session).and_then(|a| a.request.clone()) else {
            return;
        };
        let Some(option) = permission_option(&request, allow) else {
            return;
        };
        self.send::<agm::RespondPermission>(
            RespondPermissionParams {
                session,
                request_id: request.id,
                outcome: PermissionOutcome::Selected { option_id: option },
                remember: false,
            },
            "answer the permission request",
            cx,
        );
    }

    fn send<M: coqo_protocol::Method>(
        &self,
        params: M::Params,
        what: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let sent = daemon.read(cx).request::<M>(params);
        cx.background_spawn(async move {
            if let Err(err) = sent.await {
                eprintln!("coqo: couldn't {what}: {err}");
            }
        })
        .detach();
    }

    fn provider(&self, id: &str) -> String {
        self.providers
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }

    fn question_count(&self, info: &SessionInfo, cx: &App) -> usize {
        info.workspace
            .clone()
            .and_then(|w| self.stores.get(&WorkspaceId(w)))
            .map_or(0, |store| questions_of(store.read(cx), &info.id).len())
    }
}

/// The option of `request` that allows (once) or rejects (once).
pub fn permission_option(request: &PermissionRequest, allow: bool) -> Option<String> {
    let wanted: &[PermissionOptionKind] = if allow {
        &[
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        &[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    wanted.iter().find_map(|kind| {
        request
            .options
            .iter()
            .find(|o| o.kind == *kind)
            .map(|o| o.id.clone())
    })
}

/// "3 days", "5 h", "12 min".
fn uptime(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    match mins {
        0..60 => format!("{mins} min"),
        60..1440 => format!("{} h", mins / 60),
        1440..2880 => "1 day".to_owned(),
        _ => format!("{} days", mins / 1440),
    }
}

/// "2.1 GB", "640 MB": how much memory, for the header.
pub fn memory_size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        // Tenths of a GB, rounded.
        let tenths = (bytes * 10 + GB / 2) / GB;
        format!("{}.{} GB", tenths / 10, tenths % 10)
    } else {
        format!("{} MB", ((bytes + MB / 2) / MB).max(1))
    }
}

impl TabItem for AgentsTab {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        "Agents".into()
    }

    fn icon(&self, _: &App) -> Option<Icon> {
        Some(Icon::LayoutGrid)
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(KIND.into())
    }

    fn focus_handle(&self, _: &App) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }
}

// ---- rendering --------------------------------------------------------------------

/// The colours of a state's badge: fill and text.
fn badge_colors(state: AgentState, cx: &App) -> (Rgb, Rgb) {
    let t = cx.tokens();
    let p = &t.palette;
    let on_accent = if t.appearance == Appearance::Light {
        p.panel_raised
    } else {
        p.ground
    };
    match group(state) {
        Group::NeedsYou => (p.sand, on_accent),
        Group::Error => (p.rose, on_accent),
        Group::Working => (p.surface.mix(p.blue, 0.16), p.text.mix(p.blue, 0.45)),
        Group::Done if state == AgentState::Done => {
            (p.surface.mix(p.sage, 0.16), p.text.mix(p.sage, 0.45))
        }
        Group::Done => (p.surface_strong, p.text_muted),
    }
}

fn button(id: (&'static str, usize), label: &'static str, cx: &App) -> Stateful<Div> {
    raised(div(), cx)
        .id(id)
        .flex_none()
        .px(px(14.))
        .py(px(7.))
        .rounded(px(10.))
        .cursor_pointer()
        .text_size(px(12.5))
        .text_color(cx.tokens().palette.text.hsla())
        .child(label)
}

/// Board 6's **Answer**: a raised button tinted sand.
fn sand_button(id: (&'static str, usize), label: &'static str, cx: &App) -> Stateful<Div> {
    let p = &cx.tokens().palette;
    let base = p.panel_raised;
    div()
        .id(id)
        .flex_none()
        .px(px(14.))
        .py(px(7.))
        .rounded(px(10.))
        .cursor_pointer()
        .bg(coqo_ui::gpui::linear_gradient(
            180.,
            coqo_ui::gpui::linear_color_stop(base.mix(p.sand, 0.22).hsla(), 0.),
            coqo_ui::gpui::linear_color_stop(base.mix(p.sand, 0.15).hsla(), 1.),
        ))
        .border_1()
        .border_color(base.mix(p.sand, 0.3).hsla())
        .text_color(p.sand.mix(p.text, 0.45).hsla())
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(12.5))
        .child(label)
}

fn icon_button(
    id: (&'static str, usize),
    glyph: Icon,
    selected: bool,
    group_name: SharedString,
    cx: &App,
) -> Stateful<Div> {
    let p = &cx.tokens().palette;
    div()
        .id(id)
        .flex_none()
        .p(px(6.))
        .rounded(px(8.))
        .cursor_pointer()
        .when(!selected, |b| {
            b.invisible().group_hover(group_name, |s| s.visible())
        })
        .hover(|s| s.bg(p.surface_selected.hsla()))
        .child(icon(glyph, px(13.), p.text_subtle.hsla()))
}

impl AgentsTab {
    fn render_row(
        &self,
        ix: usize,
        info: &SessionInfo,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let selected = self.selected.as_ref() == Some(&info.id);
        let g = group(info.state);
        let (fill, ring) = match g {
            Group::NeedsYou => (
                p.panel_raised.mix(p.sand, 0.06),
                p.panel_raised.mix(p.sand, 0.22),
            ),
            Group::Error => (
                p.panel_raised.mix(p.rose, 0.08),
                p.panel_raised.mix(p.rose, 0.23),
            ),
            _ => (p.surface, p.surface),
        };
        let (badge_bg, badge_fg) = badge_colors(info.state, cx);
        let question = info.workspace.clone().and_then(|w| {
            let store = self.stores.get(&WorkspaceId(w))?;
            questions_of(store.read(cx), &info.id)
                .first()
                .cloned()
                .cloned()
        });
        let ask = self.asks.get(&info.id).and_then(|a| a.request.as_ref());
        let walkthrough = self.walkthrough_of(info, cx);
        let Detail { text, code } = match &walkthrough {
            Some(w) => Detail {
                text: walkthrough_line(w),
                code: None,
            },
            None => detail(info, question.as_ref(), ask),
        };
        let place = workspace_name(info, cx)
            .map(|w| format!(" · {w}"))
            .unwrap_or_default();
        let questions = self.question_count(info, cx);
        let mut chips: Vec<String> = criteria_line(info).into_iter().collect();
        match questions {
            0 => {}
            1 => chips.push("1 open question".into()),
            n => chips.push(format!("{n} open questions")),
        }
        let group_name: SharedString = format!("agents-row-{ix}").into();

        let mut actions = div().flex().items_center().gap(px(6.));
        let id = info.id.clone();
        match info.state {
            AgentState::NeedsInput => {
                let row = info.clone();
                actions = actions.child(
                    sand_button(("agents-primary", ix), "Answer", cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            let target = target_of(&row, cx);
                            this.open(&row, target, window, cx);
                        }))
                        .test_support(),
                );
            }
            AgentState::NeedsPermission if ask.is_some() => {
                let (allow, deny) = (id.clone(), id.clone());
                actions = actions
                    .child(
                        primary(div(), cx)
                            .id(("agents-primary", ix))
                            .test_support()
                            .flex_none()
                            .px(px(14.))
                            .py(px(7.))
                            .rounded(px(10.))
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .child("Allow once")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.respond(allow.clone(), true, cx);
                            })),
                    )
                    .child(
                        button(("agents-deny", ix), "Deny", cx)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.respond(deny.clone(), false, cx);
                            }))
                            .test_support(),
                    );
            }
            AgentState::Done if walkthrough.is_some() => {
                let row = info.clone();
                actions = actions.child(
                    button(("agents-primary", ix), "Walkthrough", cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            let target = this.primary_target(&row, cx);
                            this.open(&row, target, window, cx);
                        }))
                        .test_support(),
                );
            }
            state => {
                let label = if state == AgentState::Working {
                    "Watch"
                } else {
                    "Open chat"
                };
                let row = info.clone();
                actions = actions.child(
                    button(("agents-primary", ix), label, cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open(&row, AgentTarget::Chat(row.id.clone()), window, cx);
                        }))
                        .test_support(),
                );
            }
        }
        let running = matches!(
            info.state,
            AgentState::Working | AgentState::NeedsInput | AgentState::NeedsPermission
        );
        if running {
            let stop = id.clone();
            actions = actions.child(
                icon_button(
                    ("agents-stop", ix),
                    Icon::Square,
                    selected,
                    group_name.clone(),
                    cx,
                )
                .tooltip(|_, cx| tooltip("Stop the turn", cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.send::<agm::Cancel>(
                        SessionParams {
                            session: stop.clone(),
                        },
                        "stop the turn",
                        cx,
                    );
                }))
                .test_support(),
            );
        }
        let close = id.clone();
        actions = actions.child(
            icon_button(
                ("agents-close", ix),
                Icon::X,
                selected,
                group_name.clone(),
                cx,
            )
            .tooltip(|_, cx| tooltip("Close the session", cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.send::<agm::Close>(
                    SessionParams {
                        session: close.clone(),
                    },
                    "close the session",
                    cx,
                );
            }))
            .test_support(),
        );

        let row = info.clone();
        div()
            .id(("agents-row", ix))
            .test_support()
            .group(group_name)
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(14.))
            .px(px(16.))
            .py(px(14.))
            .rounded(px(14.))
            .bg(fill.hsla())
            .border_1()
            .border_color(if selected { p.blue } else { ring }.hsla())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open(&row, AgentTarget::Chat(row.id.clone()), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .px(px(9.))
                    .py(px(4.))
                    .rounded(px(8.))
                    .bg(badge_bg.hsla())
                    .text_color(badge_fg.hsla())
                    .text_size(px(10.5))
                    .font_weight(FontWeight::BOLD)
                    .child(badge(info.state)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(280.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .min_w_0()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(title(info)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(p.text_muted.hsla())
                                    .child(place),
                            ),
                    )
                    .child(
                        div()
                            .id(("agents-detail", ix))
                            .test_support()
                            .flex()
                            .min_w_0()
                            .text_size(px(12.5))
                            .text_color(p.text_muted.hsla())
                            .when(!text.is_empty(), |d| d.child(div().truncate().child(text)))
                            .children(code.map(|code| {
                                div()
                                    .truncate()
                                    .font_family(m.font_mono.clone())
                                    .text_size(px(12.))
                                    .text_color(p.text.hsla())
                                    .child(code)
                            })),
                    )
                    .when(!chips.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(m.text_xs)
                                .text_color(p.text_subtle.hsla())
                                .child(chips.join(" · ")),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(p.text_muted.hsla())
                    .child(meta(info, &self.provider(&info.provider), now)),
            )
            .child(actions)
            .into_any_element()
    }
}

fn tooltip(text: &'static str, cx: &mut App) -> coqo_ui::gpui::AnyView {
    cx.new(|_| Tip(text)).into()
}

struct Tip(&'static str);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        coqo_ui::widgets::popover(cx)
            .px(px(10.))
            .py(px(6.))
            .text_size(t.metrics.text_sm)
            .text_color(t.palette.text.hsla())
            .font_family(t.metrics.font_ui.clone())
            .child(self.0)
    }
}

impl Render for AgentsTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_ms();
        let rows = self.rows(cx);
        let facts = self.facts(&rows, now);
        let cards: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(ix, info)| self.render_row(ix, info, now, cx))
            .collect();
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let empty = rows.is_empty().then(|| {
            div()
                .id("agents-empty")
                .test_support()
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .py(px(48.))
                .text_color(p.text_subtle.hsla())
                .child(icon(Icon::LayoutGrid, px(28.), p.text_subtle.hsla()))
                .child("No agent sessions yet.")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(m.text_sm)
                        .child("Start a chat")
                        .child(kbd(shortcut("Ctrl Shift N"), cx)),
                )
        });
        div()
            .id("agents-overview")
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(16.))
            .text_size(m.text_md)
            .text_color(p.text.hsla())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_end()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Agents"),
                    )
                    .child(
                        div()
                            .id("agents-facts")
                            .test_support()
                            .text_size(px(12.))
                            .text_color(p.text_subtle.hsla())
                            .child(facts),
                    ),
            )
            .child(
                div()
                    .id("agents-list")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .overflow_y_scroll()
                    .children(cards)
                    .children(empty),
            )
    }
}

#[cfg(test)]
mod unit_tests {
    use coqo_protocol::methods::agents::{
        PermissionOption, PermissionOptionKind, PermissionRequest, PermissionRequestId,
    };

    use super::{memory_size, permission_option, uptime};

    #[test]
    fn picks_the_once_options() {
        let option = |id: &str, kind| PermissionOption {
            id: id.into(),
            name: id.into(),
            kind,
        };
        let request = PermissionRequest {
            id: PermissionRequestId(1),
            title: "Run".into(),
            tool_call: None,
            options: vec![
                option("always", PermissionOptionKind::AllowAlways),
                option("once", PermissionOptionKind::AllowOnce),
                option("no", PermissionOptionKind::RejectOnce),
            ],
            suggested_rule: None,
            questions: Vec::new(),
        };
        assert_eq!(permission_option(&request, true).as_deref(), Some("once"));
        assert_eq!(permission_option(&request, false).as_deref(), Some("no"));
        assert_eq!(uptime(3 * 86_400_000), "3 days");
        assert_eq!(uptime(5 * 3_600_000), "5 h");
    }

    #[test]
    fn memory_reads_like_board_6() {
        const MB: u64 = 1024 * 1024;
        assert_eq!(memory_size(2150 * MB), "2.1 GB");
        assert_eq!(memory_size(1024 * MB), "1.0 GB");
        assert_eq!(memory_size(640 * MB + 100), "640 MB");
        assert_eq!(memory_size(10), "1 MB");
    }
}
