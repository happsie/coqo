// SPDX-License-Identifier: Apache-2.0

//! The chat tab (design board 1): a centred reading column with the
//! transcript in a virtual list and the composer below it, and the session
//! panel (state and the agent's status, plan, acceptance criteria, changed
//! files) on the right.
//!
//! The tab attaches with `sessions/get` (info, title, provider) and
//! `sessions/updates` from seq 0, so a restored tab replays the whole
//! transcript and then follows the live stream. After a daemon reconnect it
//! resubscribes from the last seq it applied; a `RESUME_GAP` starts over.
//!
//! The composer (mentions, slash commands, the provider and mode dropdowns,
//! prompt history) is in [`composer`]; permission cards, their keys and the
//! workspace's permission rules are in [`permission`]; the acceptance
//! criteria card is in [`criteria`].

mod composer;
mod criteria;
mod permission;

use std::collections::{HashMap, HashSet};

use coqo_protocol::{
    Sequenced, codes,
    methods::agents::{
        AgentState, AuthKind, Cancel, ContentBlock, Get, PermissionOptionKind, PermissionOutcome,
        PermissionRequest, PermissionRequestId, PlanEntryStatus, ProviderActionRun, ProviderInfo,
        Providers, RespondPermission, RespondPermissionParams, SessionId, SessionInfo,
        SessionParams, SessionSnapshot, SessionUpdate, ToolCallContent, ToolCallStatus, ToolKind,
        Updates,
    },
};
use coqo_ui::{
    ActiveTokens as _,
    dock::TabItem,
    gpui::{
        AnyElement, App, AppContext as _, Context, ElementId, Entity, FocusHandle, Focusable as _,
        FontWeight, Hsla, InteractiveElement as _, IntoElement, ListAlignment, ListState,
        ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
        Subscription, Task, Window, div, list, prelude::FluentBuilder as _, px, relative,
    },
    input::{PromptInput, PromptInputEvent},
    serde_json::{Value, json},
    text::markdown,
    widgets::{Icon, TestSupportExt as _, icon, primary, section_label},
};

use super::{
    CancelTurn, KIND, OpenLocation, OpenTerminal, RunInTerminal, SIGN_IN_HELP, agent_dot,
    attention_label,
    diff::{DiffLine, DiffPreview, LineKind, file_name},
    list::ChatList,
    state_color, state_name,
    transcript::{Entry, NoticeKind, ToolEntry, Transcript, block_text},
};
use crate::activity::AgentActivity;
use crate::daemon::{Daemon, DaemonError, DaemonEvent, EventStream};

/// Width of the reading column.
const COLUMN: f32 = 760.;
/// Lines of tool output shown when a tool call is expanded.
const MAX_OUTPUT_LINES: usize = 40;

/// A chat session's tab.
pub struct ChatTab {
    session: SessionId,
    info: Option<SessionInfo>,
    provider_name: Option<String>,
    transcript: Transcript,
    list: ListState,
    composer: Entity<PromptInput>,
    /// Entries the user expanded (tool calls, thoughts).
    expanded: HashSet<usize>,
    /// The last failed request, shown above the composer.
    error: Option<String>,
    focus: FocusHandle,
    /// `agents/providers`, for the provider dropdown and capabilities.
    providers: Vec<ProviderInfo>,
    /// The composer's completion popover or open dropdown.
    popover: Option<composer::Popover>,
    /// Where the `@` or `/` word that was dismissed with Esc starts.
    dismissed: Option<usize>,
    /// Where the click that closed a dropdown was, so it doesn't reopen it.
    closed_by: Option<coqo_ui::gpui::Point<coqo_ui::gpui::Pixels>>,
    /// The workspace's files, for @-mentions.
    files: composer::FileIndex,
    /// Mentioned files by URI, for embedding their contents.
    targets: HashMap<String, composer::Target>,
    /// Which earlier prompt the composer shows (index among the prompts).
    history: Option<usize>,
    /// A provider switch is creating the new session.
    switching: bool,
    /// Permission focus, "remember" ticks and the workspace's rules.
    permissions: permission::Permissions,
    _updates: Option<Task<()>>,
    _index: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ChatTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatTab")
            .field("session", &self.session)
            .field("head", &self.transcript.head)
            .finish_non_exhaustive()
    }
}

/// Recreate a saved chat tab.
pub fn restore(state: &Value, window: &mut Window, cx: &mut App) -> Option<Entity<ChatTab>> {
    let session = SessionId(state.get("session")?.as_str()?.to_owned());
    Some(cx.new(|cx| ChatTab::new(session, window, cx)))
}

impl ChatTab {
    pub fn new(session: SessionId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            PromptInput::new(
                "Message the agent. @ to mention, / for commands and skills",
                2,
                8,
                window,
                cx,
            )
        });
        let list = ListState::new(0, ListAlignment::Top, px(600.));
        list.set_follow_mode(coqo_ui::gpui::FollowMode::Tail);
        let mut subscriptions = vec![cx.subscribe_in(
            &composer,
            window,
            |this, _, event: &PromptInputEvent, window, cx| match event {
                PromptInputEvent::Submit => this.send(window, cx),
                PromptInputEvent::Cancel => this.on_composer_cancel(cx),
                PromptInputEvent::Changed => this.refresh_completion(cx),
                PromptInputEvent::Key(key) => this.on_composer_key(*key, window, cx),
            },
        )];
        let mut connected = false;
        if let Some(daemon) = Daemon::global(cx) {
            connected = daemon.read(cx).is_connected();
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| match event {
                DaemonEvent::Connected => this.attach(cx),
                DaemonEvent::Disconnected => this._updates = None,
            }));
        }
        if let Some(chats) = ChatList::global(cx) {
            subscriptions.push(cx.observe(&chats, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            session,
            info: None,
            provider_name: None,
            transcript: Transcript::default(),
            list,
            composer,
            expanded: HashSet::new(),
            error: None,
            focus: cx.focus_handle(),
            providers: Vec::new(),
            popover: None,
            dismissed: None,
            closed_by: None,
            files: composer::FileIndex::default(),
            targets: HashMap::new(),
            history: None,
            switching: false,
            permissions: permission::Permissions::new(cx.focus_handle()),
            _updates: None,
            _index: None,
            _subscriptions: subscriptions,
        };
        if connected {
            this.attach(cx);
        }
        this
    }

    /// The session this tab shows.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The folded transcript.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The composer.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn composer(&self) -> &Entity<PromptInput> {
        &self.composer
    }

    /// Show `info` and a provider name without a daemon (screenshots).
    #[cfg(test)]
    pub fn set_preview(&mut self, info: SessionInfo, provider: &str, cx: &mut Context<Self>) {
        self.info = Some(info);
        self.provider_name = Some(provider.to_owned());
        cx.notify();
    }

    /// The providers the daemon offers.
    #[cfg(test)]
    pub fn providers(&self) -> &[ProviderInfo] {
        &self.providers
    }

    fn daemon(cx: &App) -> Option<Entity<Daemon>> {
        Daemon::global(cx).filter(|d| d.read(cx).is_connected())
    }

    fn params(&self) -> SessionParams {
        SessionParams {
            session: self.session.clone(),
        }
    }

    /// Fetch the session's info and follow its updates from the last seq
    /// applied (0 the first time: the whole transcript).
    fn attach(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let daemon = daemon.read(cx);
        let get = daemon.request::<Get>(self.params());
        let providers = daemon.request::<Providers>(());
        let updates = daemon.subscribe_since::<Updates>(self.params(), self.transcript.head);
        self._updates = Some(cx.spawn(async move |this, cx| {
            match get.await {
                Ok(snapshot) => {
                    this.update(cx, |this, cx| this.on_snapshot(snapshot, cx))
                        .ok();
                }
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("Couldn't open this chat: {e}"));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            }
            if let Ok(providers) = providers.await {
                this.update(cx, |this, cx| {
                    let id = this.info.as_ref().map(|i| i.provider.clone());
                    this.provider_name = providers
                        .iter()
                        .find(|p| Some(&p.id) == id.as_ref())
                        .map(|p| p.name.clone());
                    this.providers = providers;
                    cx.notify();
                })
                .ok();
            }
            let mut stream = match updates.await {
                Ok(stream) => stream,
                Err(e) => {
                    this.update(cx, |this, cx| this.on_stream_error(e, cx)).ok();
                    return;
                }
            };
            while let Some(first) = stream.next().await {
                let (batch, failed) = drain(first, &mut stream);
                let alive = this
                    .update(cx, |this, cx| {
                        this.apply(batch, cx);
                        if let Some(e) = failed {
                            this.on_stream_error(e, cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    fn on_snapshot(&mut self, snapshot: SessionSnapshot, cx: &mut Context<Self>) {
        if self.transcript.head == 0 {
            // Until the replay arrives, show what the daemon folded.
            let t = &mut self.transcript;
            t.state = snapshot.info.state;
            t.state_reason = snapshot.info.state_reason.clone();
            t.running = snapshot.running;
            t.closed = snapshot.info.closed;
            t.plan = snapshot.plan;
            t.modes = snapshot.modes;
            t.usage = snapshot.usage;
            t.auth = snapshot.auth;
            t.commands = snapshot.available_commands;
        }
        self.info = Some(snapshot.info);
        self.ensure_rules(cx);
        cx.notify();
    }

    fn on_stream_error(&mut self, error: DaemonError, cx: &mut Context<Self>) {
        if error.code() == Some(codes::RESUME_GAP) {
            // The daemon can't replay from our seq: start over.
            self.transcript = Transcript::default();
            self.expanded.clear();
            self.list.reset(0);
            self.attach(cx);
        } else if !matches!(error, DaemonError::NotConnected) {
            self.error = Some(format!("Lost this chat's updates: {error}"));
        }
        cx.notify();
    }

    /// Apply updates and tell the list which rows changed.
    pub fn apply(&mut self, updates: Vec<(u64, SessionUpdate)>, cx: &mut Context<Self>) {
        self.note_permission_updates(&updates, cx);
        let before = self.transcript.entries.len();
        let changes = self.transcript.apply_all(updates);
        if changes.is_empty() {
            return;
        }
        self.jump_to_pending();
        let after = self.transcript.entries.len();
        if after > before {
            self.list.splice(before..before, after - before);
        }
        for ix in changes.updated {
            self.list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.transcript.running {
            return;
        }
        if let Some(daemon) = Self::daemon(cx) {
            let request = daemon.read(cx).request::<Cancel>(self.params());
            self.spawn_request(request, "Couldn't stop the turn", cx);
        }
    }

    /// Answer a permission request with one of its options; with its
    /// "remember for this workspace" ticked, the daemon also stores the
    /// request's suggested rule.
    pub fn respond(
        &mut self,
        request: PermissionRequestId,
        option: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(daemon) = Self::daemon(cx) {
            let remember = self.permissions.remember.contains(&request);
            let call = daemon
                .read(cx)
                .request::<RespondPermission>(RespondPermissionParams {
                    session: self.session.clone(),
                    request_id: request,
                    outcome: PermissionOutcome::Selected { option_id: option },
                    remember,
                });
            self.spawn_request(call, "Couldn't answer the permission request", cx);
            self.after_answer(request);
            cx.notify();
        }
    }

    fn spawn_request<T: 'static>(
        &mut self,
        request: impl Future<Output = Result<T, DaemonError>> + 'static,
        what: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            if let Err(e) = request.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{what}: {e}"));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&ix) {
            self.expanded.insert(ix);
        }
        self.list.remeasure_items(ix..ix + 1);
        cx.notify();
    }

    fn is_closed(&self) -> bool {
        self.transcript.closed || self.info.as_ref().is_some_and(|i| i.closed)
    }

    /// The latest info: from the sidebar list or the app's agent activity
    /// (both kept current by `sessions/changes`), or the snapshot.
    fn latest_info<'a>(&'a self, cx: &'a App) -> Option<&'a SessionInfo> {
        ChatList::global(cx)
            .and_then(|l| l.read(cx).info(&self.session))
            .or_else(|| AgentActivity::global(cx).and_then(|a| a.read(cx).info(&self.session)))
            .or(self.info.as_ref())
    }

    /// The agent's status line with its progress, as the chat list shows it
    /// (see [`super::status_line`]).
    #[cfg(test)]
    pub fn status_line(&self, cx: &App) -> Option<String> {
        self.latest_info(cx).and_then(super::status_line)
    }

    /// Why the session is in its state (`error`, `blocked`, `needs_input`).
    fn state_reason<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if self.transcript.head > 0 {
            self.transcript.state_reason.as_deref()
        } else {
            self.latest_info(cx).and_then(|i| i.state_reason.as_deref())
        }
    }

    fn state(&self, cx: &App) -> AgentState {
        if self.transcript.head > 0 {
            self.transcript.state
        } else {
            self.latest_info(cx).map_or(AgentState::Idle, |i| i.state)
        }
    }

    fn provider_label(&self) -> String {
        self.provider_name
            .clone()
            .or_else(|| self.info.as_ref().map(|i| i.provider.clone()))
            .unwrap_or_else(|| "Agent".into())
    }

    fn relative<'a>(&self, path: &'a str) -> &'a str {
        self.info
            .as_ref()
            .and_then(|i| path.strip_prefix(i.cwd.as_str()))
            .map(|p| p.trim_start_matches('/'))
            .filter(|p| !p.is_empty())
            .unwrap_or(path)
    }
}

/// Collect a burst of events (a replay) into one batch. Stops at an error.
fn drain(
    first: Result<Sequenced<SessionUpdate>, DaemonError>,
    stream: &mut EventStream<Sequenced<SessionUpdate>>,
) -> (Vec<(u64, SessionUpdate)>, Option<DaemonError>) {
    let mut batch = Vec::new();
    let mut next = Some(first);
    while let Some(item) = next.take() {
        match item {
            Ok(Sequenced { seq, event }) => batch.push((seq, event)),
            Err(e) => return (batch, Some(e)),
        }
        if batch.len() < 512 {
            next = stream.try_next();
        }
    }
    (batch, None)
}

/// The chat's title: the session's, or its first prompt, or "New chat".
pub fn title_of(info: Option<&SessionInfo>, transcript: &Transcript) -> SharedString {
    if let Some(title) = info
        .and_then(|i| i.title.as_deref())
        .filter(|t| !t.is_empty())
    {
        return title.to_owned().into();
    }
    let first = transcript.entries.iter().find_map(|e| match e {
        Entry::User { content } => content.iter().find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        }),
        _ => None,
    });
    match first {
        Some(text) => {
            let line = text.lines().next().unwrap_or_default().trim();
            let mut title: String = line.chars().take(40).collect();
            if line.chars().count() > 40 {
                title.push('…');
            }
            title.into()
        }
        None => "New chat".into(),
    }
}

impl TabItem for ChatTab {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, cx: &App) -> SharedString {
        title_of(self.latest_info(cx), &self.transcript)
    }

    fn icon(&self, _: &App) -> Option<Icon> {
        Some(Icon::MessageCircle)
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(self.session.0.clone().into())
    }

    fn save(&self, _: &App) -> Value {
        json!({ "session": self.session.0 })
    }

    fn render_label(&self, selected: bool, _: &mut Window, cx: &App) -> Option<AnyElement> {
        let state = self.state(cx);
        let t = cx.tokens();
        let p = &t.palette;
        let attention = attention_label(state, p);
        let icon_color = match attention {
            Some((_, color)) => color.hsla(),
            None if selected => p.text.hsla(),
            None => p.text_muted.hsla(),
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(icon(Icon::MessageCircle, px(14.), icon_color))
                .child(self.title(cx))
                .children(attention.map(|(label, color)| {
                    div()
                        .id("chat-tab-badge")
                        .test_support()
                        .text_size(t.metrics.text_label)
                        .font_weight(FontWeight::NORMAL)
                        .text_color(color.hsla())
                        .child(label)
                }))
                .into_any_element(),
        )
    }

    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        Some(self.composer.read(cx).focus_handle(cx))
    }
}

// ---- rendering ----------------------------------------------------------------

/// A row of the reading column, centred and at most [`COLUMN`] wide.
fn column(child: impl IntoElement) -> coqo_ui::gpui::Div {
    div()
        .w_full()
        .flex()
        .justify_center()
        .px(px(28.))
        .child(div().w_full().max_w(px(COLUMN)).child(child))
}

fn kind_icon(kind: ToolKind) -> Icon {
    match kind {
        ToolKind::Read => Icon::Eye,
        ToolKind::Edit => Icon::FilePen,
        ToolKind::Delete => Icon::Trash,
        ToolKind::Move => Icon::ArrowRightLeft,
        ToolKind::Search => Icon::Search,
        ToolKind::Execute => Icon::Terminal,
        ToolKind::Think => Icon::Brain,
        ToolKind::Fetch => Icon::Globe,
        ToolKind::SwitchMode => Icon::Settings,
        ToolKind::Other => Icon::Wrench,
    }
}

/// Order of permission buttons: allow before reject, once before always.
fn option_rank(kind: PermissionOptionKind) -> u8 {
    match kind {
        PermissionOptionKind::AllowOnce => 0,
        PermissionOptionKind::AllowAlways => 1,
        PermissionOptionKind::RejectOnce => 2,
        PermissionOptionKind::RejectAlways => 3,
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

impl ChatTab {
    fn render_entry(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.transcript.entries.get(ix).cloned() else {
            return div().into_any_element();
        };
        let body = match entry {
            Entry::User { content } => self.render_user(ix, &content, cx),
            Entry::TurnStart { .. } => self.render_turn_start(ix, cx),
            Entry::Agent { text } => div()
                .id(("chat-agent", ix))
                .test_support()
                .child(markdown(("chat-md", ix), text))
                .into_any_element(),
            Entry::Thought { text } => self.render_thought(ix, text, cx),
            Entry::Tool(tool) => self.render_tool(ix, &tool, cx),
            Entry::Permission {
                request,
                outcome,
                by_rule,
                remembered,
            } => self.render_permission(
                ix,
                &request,
                outcome.as_ref(),
                by_rule.as_deref(),
                remembered.as_deref(),
                cx,
            ),
            Entry::Notice { kind, text } => self.render_notice(ix, kind, text, cx),
        };
        let top = if ix == 0 { px(18.) } else { px(0.) };
        column(body).pt(top).pb(px(14.)).into_any_element()
    }

    fn render_user(&self, ix: usize, content: &[ContentBlock], cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let mut text = String::new();
        let mut chips = Vec::new();
        for block in content {
            match block {
                ContentBlock::Text { text: s } => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(s);
                }
                ContentBlock::ResourceLink { name, .. } => chips.push(format!("@{name}")),
                ContentBlock::Resource { uri, .. } => chips.push(format!("@{}", file_name(uri))),
                ContentBlock::Image { .. } => chips.push("image".into()),
            }
        }
        div()
            .flex()
            .justify_end()
            .child(
                div()
                    .id(("chat-user", ix))
                    .test_support()
                    .max_w(px(COLUMN * 0.8))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px(px(16.))
                    .py(px(12.))
                    .rounded_tl(px(18.))
                    .rounded_tr(px(18.))
                    .rounded_bl(px(18.))
                    .rounded_br(px(4.))
                    .bg(p.sage.alpha(0.16))
                    .line_height(px(21.))
                    .when(!chips.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_1()
                                .children(chips.into_iter().map(|c| {
                                    div()
                                        .px(px(6.))
                                        .py(px(1.))
                                        .rounded(px(6.))
                                        .bg(p.sage.alpha(0.22))
                                        .font_family(m.font_mono.clone())
                                        .text_size(m.text_sm)
                                        .child(c)
                                })),
                        )
                    })
                    .child(text),
            )
            .into_any_element()
    }

    fn render_turn_start(&self, ix: usize, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let provider = self.provider_label();
        let letter: String = provider.chars().take(1).collect::<String>().to_uppercase();
        let mut line = provider;
        if let Some(model) = self
            .transcript
            .usage
            .as_ref()
            .and_then(|u| u.model.as_ref())
        {
            line.push_str(" · ");
            line.push_str(model);
        }
        div()
            .id(("chat-turn", ix))
            .flex()
            .items_center()
            .gap_2()
            .text_size(m.text_sm)
            .text_color(p.text_muted.hsla())
            .child(
                div()
                    .size(px(22.))
                    .rounded(px(8.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(p.sand.alpha(0.16))
                    .text_color(p.sand.hsla())
                    .text_size(m.text_label)
                    .font_weight(FontWeight::BOLD)
                    .child(letter),
            )
            .child(line)
            .into_any_element()
    }

    fn render_thought(&self, ix: usize, text: String, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let open = self.expanded.contains(&ix);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .id(("chat-thought", ix))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(m.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child(icon(
                        if open {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        px(12.),
                        p.text_subtle.hsla(),
                    ))
                    .child(icon(Icon::Brain, px(13.), p.text_subtle.hsla()))
                    .child("Thinking")
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(ix, cx))),
            )
            .when(open, |d| {
                d.child(
                    div()
                        .id(("chat-thought-body", ix))
                        .test_support()
                        .pl(px(18.))
                        .text_size(m.text_sm)
                        .text_color(p.text_muted.hsla())
                        .child(markdown(("chat-thought-md", ix), text)),
                )
            })
            .into_any_element()
    }

    fn render_tool(&self, ix: usize, tool: &ToolEntry, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let call = &tool.call;
        let open = self.expanded.contains(&ix);
        let (added, removed) = tool.counts();
        let title = if call.title.is_empty() {
            "Tool call".to_owned()
        } else {
            call.title.clone()
        };
        let status: AnyElement = match call.status {
            ToolCallStatus::Failed => div()
                .flex()
                .items_center()
                .gap_1()
                .text_color(p.rose.hsla())
                .child(icon(Icon::CircleX, px(12.), p.rose.hsla()))
                .child("failed")
                .into_any_element(),
            ToolCallStatus::InProgress => div()
                .flex()
                .items_center()
                .gap_1()
                .text_color(p.blue.hsla())
                .child(icon(Icon::LoaderCircle, px(12.), p.blue.hsla()))
                .child("running")
                .into_any_element(),
            _ if !tool.diffs.is_empty() => div()
                .flex()
                .gap_2()
                .child(div().text_color(p.green.hsla()).child(format!("+{added}")))
                .when(removed > 0, |d| {
                    d.child(div().text_color(p.rose.hsla()).child(format!("−{removed}")))
                })
                .into_any_element(),
            ToolCallStatus::Pending => div()
                .text_color(p.text_subtle.hsla())
                .child("pending")
                .into_any_element(),
            ToolCallStatus::Completed => {
                icon(Icon::Check, px(12.), p.sage.hsla()).into_any_element()
            }
        };
        let header = div()
            .id(("chat-tool", ix))
            .test_support()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px(px(14.))
            .py(px(9.))
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_w_0()
                    .child(icon(
                        if open {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        px(12.),
                        p.text_muted.hsla(),
                    ))
                    .child(icon(kind_icon(call.kind), px(13.), p.text_muted.hsla()))
                    .child(div().truncate().child(title)),
            )
            .child(status)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(ix, cx)));

        let mut card = div()
            .flex()
            .flex_col()
            .rounded(px(12.))
            .bg(p.surface.hsla())
            .font_family(m.font_mono.clone())
            .text_size(px(12.))
            .child(header);
        // An agent's command in a daemon terminal: open it in a tab (shown
        // collapsed too, so a running command is one click away).
        let terminals: Vec<u64> = call
            .content
            .iter()
            .filter_map(|c| match c {
                ToolCallContent::Terminal { terminal_id } => terminal_id.parse().ok(),
                _ => None,
            })
            .collect();
        if !terminals.is_empty() {
            card = card.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .px(px(14.))
                    .pb(px(9.))
                    .children(terminals.into_iter().enumerate().map(|(n, id)| {
                        let focus = self.focus.clone();
                        div()
                            .id(ElementId::Name(format!("chat-terminal-{ix}-{n}").into()))
                            .test_support()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_color(p.blue.hsla())
                            .cursor_pointer()
                            .child(icon(Icon::Terminal, px(12.), p.blue.hsla()))
                            .child("Open terminal")
                            .on_click(move |_, window, cx| {
                                window.focus(&focus, cx);
                                window.dispatch_action(Box::new(OpenTerminal { id }), cx);
                            })
                    })),
            );
        }
        if open {
            let mut body = div().flex().flex_col().gap_2().px(px(14.)).pb(px(10.));
            if !call.locations.is_empty() {
                body = body.child(div().flex().flex_wrap().gap_3().children(
                    call.locations.iter().enumerate().map(|(n, loc)| {
                        let label = match loc.line {
                            Some(line) => format!("{}:{line}", self.relative(&loc.path)),
                            None => self.relative(&loc.path).to_owned(),
                        };
                        let action = OpenLocation {
                            path: loc.path.clone(),
                            line: loc.line,
                        };
                        let focus = self.focus.clone();
                        div()
                            .id(ElementId::Name(format!("chat-location-{ix}-{n}").into()))
                            .test_support()
                            .text_color(p.blue.hsla())
                            .cursor_pointer()
                            .child(label)
                            .on_click(move |_, window, cx| {
                                window.focus(&focus, cx);
                                window.dispatch_action(Box::new(action.clone()), cx);
                            })
                    }),
                ));
            }
            for diff in &tool.diffs {
                body = body.child(self.render_diff(diff, cx));
            }
            let output: Vec<String> = call
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolCallContent::Content { content } => Some(block_text(content)),
                    ToolCallContent::Terminal { .. } | ToolCallContent::Diff { .. } => None,
                })
                .collect();
            if !output.is_empty() {
                let text = output.join("\n");
                let lines: Vec<&str> = text.lines().collect();
                let shown = lines
                    .iter()
                    .take(MAX_OUTPUT_LINES)
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                body =
                    body.child(
                        div()
                            .p(px(10.))
                            .rounded(px(8.))
                            .bg(p.panel.hsla())
                            .text_color(p.text_muted.hsla())
                            .whitespace_normal()
                            .child(shown)
                            .when(lines.len() > MAX_OUTPUT_LINES, |d| {
                                d.child(div().pt_1().text_color(p.text_subtle.hsla()).child(
                                    format!("… {} more lines", lines.len() - MAX_OUTPUT_LINES),
                                ))
                            }),
                    );
            }
            card = card.child(body);
        }
        card.into_any_element()
    }

    fn render_diff(&self, diff: &DiffPreview, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let p = &t.palette;
        let header = format!(
            "{}{}",
            self.relative(&diff.path),
            if diff.created { " (new)" } else { "" }
        );
        let line = |l: &DiffLine| {
            let (bg, fg, mark): (Option<Hsla>, Hsla, &str) = match l.kind {
                LineKind::Added => (Some(p.green.alpha(0.12)), p.text.hsla(), "+"),
                LineKind::Removed => (Some(p.rose.alpha(0.12)), p.text.hsla(), "−"),
                LineKind::Context => (None, p.text_muted.hsla(), " "),
                LineKind::Gap => (None, p.text_subtle.hsla(), "⋯"),
            };
            let mark_color = match l.kind {
                LineKind::Added => p.green.hsla(),
                LineKind::Removed => p.rose.hsla(),
                _ => p.text_subtle.hsla(),
            };
            div()
                .flex()
                .px(px(8.))
                .when_some(bg, |d, bg| d.bg(bg))
                .text_color(fg)
                .child(
                    div()
                        .w(px(16.))
                        .flex_none()
                        .text_color(mark_color)
                        .child(mark),
                )
                .child(div().whitespace_nowrap().child(l.text.clone()))
        };
        div()
            .flex()
            .flex_col()
            .rounded(px(8.))
            .bg(p.panel.hsla())
            .overflow_hidden()
            .py(px(4.))
            .child(
                div()
                    .px(px(8.))
                    .pb(px(4.))
                    .text_color(p.text_subtle.hsla())
                    .child(header),
            )
            .children(diff.lines.iter().map(line))
            .when(diff.truncated > 0, |d| {
                d.child(
                    div()
                        .px(px(8.))
                        .pt(px(4.))
                        .text_color(p.text_subtle.hsla())
                        .child(format!("… {} more lines", diff.truncated)),
                )
            })
            .into_any_element()
    }

    fn render_notice(&self, ix: usize, kind: NoticeKind, text: String, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let (glyph, color) = match kind {
            NoticeKind::Error => (Icon::CircleAlert, p.rose.hsla()),
            NoticeKind::Attention => (Icon::CircleAlert, p.sand.hsla()),
            NoticeKind::Stopped => (Icon::Square, p.text_subtle.hsla()),
            NoticeKind::Closed => (Icon::X, p.text_subtle.hsla()),
        };
        div()
            .id(("chat-notice", ix))
            .test_support()
            .flex()
            .items_start()
            .gap_2()
            .text_size(m.text_sm)
            .text_color(color)
            .child(div().pt(px(2.)).child(icon(glyph, px(13.), color)))
            .child(div().flex_1().child(text))
            .into_any_element()
    }

    fn render_banners(&self, cx: &App) -> Vec<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let mut banners = Vec::new();
        let signed_out = self
            .transcript
            .auth
            .as_ref()
            .is_some_and(|a| a.kind == AuthKind::None);
        if self.transcript.sign_in.is_some() || signed_out {
            // The provider's own sign-in tool, in a terminal tab.
            let sign_in = self
                .info
                .as_ref()
                .and_then(|i| self.providers.iter().find(|p| p.id == i.provider))
                .and_then(|p| p.status.as_ref()?.sign_in.clone())
                .and_then(|a| match a.run {
                    ProviderActionRun::Terminal { program, args } => {
                        Some((a.label, RunInTerminal { program, args }))
                    }
                    ProviderActionRun::Daemon => None,
                });
            let button = sign_in.map(|(label, action)| {
                primary(div().mt_1().px(px(10.)).py(px(4.)).rounded(px(8.)), cx)
                    .id("chat-sign-in-terminal")
                    .test_support()
                    .cursor_pointer()
                    .child(label)
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(action.clone()), cx);
                    })
            });
            banners.push(
                div()
                    .id("chat-sign-in")
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(12.))
                    .rounded(px(14.))
                    .bg(p.sand.alpha(0.08))
                    .border_1()
                    .border_color(p.sand.alpha(0.3))
                    .text_size(m.text_sm)
                    .child(icon(Icon::LogIn, px(16.), p.sand.hsla()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div().font_weight(FontWeight::SEMIBOLD).child(format!(
                                    "{} needs you to sign in",
                                    self.provider_label()
                                )),
                            )
                            .child(div().text_color(p.text_muted.hsla()).child(SIGN_IN_HELP))
                            .children(button.map(|b| div().flex().child(b))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(error) = &self.error {
            banners.push(
                div()
                    .id("chat-error")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(m.text_sm)
                    .text_color(p.rose.hsla())
                    .child(icon(Icon::CircleAlert, px(13.), p.rose.hsla()))
                    .child(error.clone())
                    .into_any_element(),
            );
        }
        banners
    }

    /// The panel's first card: the state, the agent's status line (or why
    /// it waits) and its progress (the status's, else the plan's).
    fn render_status(&self, cx: &App) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (&t.palette, &t.metrics);
        let state = self.state(cx);
        let color = state_color(state, p);
        let status = self.latest_info(cx).and_then(|i| i.status.as_ref());
        let text = status
            .map(|s| s.text.clone())
            .or_else(|| self.state_reason(cx).map(str::to_owned))
            .filter(|t| !t.is_empty());
        let quiet = match state {
            AgentState::Idle => "Ready for a prompt",
            AgentState::Working => "Working on your prompt",
            AgentState::NeedsInput => "Waiting for your answer",
            AgentState::NeedsPermission => "Waiting for your permission",
            AgentState::Blocked => "Needs your help to go on",
            AgentState::Error => "The last turn failed",
            AgentState::Done => "Finished the last turn",
        };
        let plan = &self.transcript.plan;
        let progress = status
            .and_then(|s| s.progress)
            .map(|p| (p.current, p.total, false));
        let progress = progress.or_else(|| {
            let done = plan
                .iter()
                .filter(|e| e.status == PlanEntryStatus::Completed)
                .count() as u64;
            (!plan.is_empty() && state != AgentState::Idle).then_some((
                done,
                plan.len() as u64,
                true,
            ))
        });
        let mut card = div()
            .id("chat-status")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(14.))
            .bg(p.panel_raised.hsla())
            .text_size(m.text_sm)
            .child(
                div().flex().child(
                    div()
                        .id("chat-status-pill")
                        .test_support()
                        .px(px(9.))
                        .py(px(3.))
                        .rounded(px(8.))
                        .bg(color.alpha(0.18))
                        .text_color(color.hsla())
                        .text_size(px(10.5))
                        .font_weight(FontWeight::BOLD)
                        .child(state_name(state).to_uppercase()),
                ),
            )
            .child(
                div()
                    .id("chat-status-text")
                    .test_support()
                    .px_1()
                    .when_some(text.clone(), |d, text| {
                        d.text_color(p.text.hsla()).child(text)
                    })
                    .when(text.is_none(), |d| {
                        d.text_color(p.text_subtle.hsla()).child(quiet)
                    }),
            );
        if let Some((current, total, from_plan)) = progress {
            let fraction = if total == 0 {
                0.
            } else {
                (current as f32 / total as f32).clamp(0., 1.)
            };
            let label = if from_plan {
                format!("Plan: {current} of {total} done")
            } else {
                format!("{current} of {total}")
            };
            card = card.child(
                div()
                    .id("chat-status-progress")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px_1()
                    .child(
                        div()
                            .flex_1()
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(p.surface.hsla())
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(fraction))
                                    .rounded(px(2.))
                                    .bg(color.hsla()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(m.text_label)
                            .text_color(p.text_muted.hsla())
                            .child(label),
                    ),
            );
        }
        card.into_any_element()
    }

    fn render_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let card = || {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(14.))
                .py(px(12.))
                .rounded(px(14.))
                .bg(p.panel_raised.hsla())
                .text_size(m.text_sm)
        };
        let tr = &self.transcript;
        let mut plan = card()
            .id("chat-plan")
            .test_support()
            .child(section_label("Plan", cx));
        if tr.plan.is_empty() {
            plan = plan.child(
                div()
                    .px_2()
                    .text_color(p.text_subtle.hsla())
                    .child("No plan shared yet"),
            );
        }
        for (n, entry) in tr.plan.iter().enumerate() {
            let (glyph, color, text) = match entry.status {
                PlanEntryStatus::Completed => (Icon::Check, p.sage.hsla(), p.text.hsla()),
                PlanEntryStatus::InProgress => (Icon::CircleDot, p.sand.hsla(), p.text.hsla()),
                PlanEntryStatus::Pending => {
                    (Icon::Circle, p.text_muted.hsla(), p.text_muted.hsla())
                }
            };
            plan = plan.child(
                div()
                    .id(("chat-plan-entry", n))
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .px_2()
                    .text_color(text)
                    .child(div().pt(px(3.)).child(icon(glyph, px(12.), color)))
                    .child(div().flex_1().child(entry.content.clone())),
            );
        }
        let files = tr.changed_files();
        let mut changes = card().id("chat-changes").test_support().child(
            div()
                .flex()
                .justify_between()
                .child(section_label("Changes", cx))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_size(m.text_label)
                        .text_color(p.text_subtle.hsla())
                        .child(match files.len() {
                            0 => String::new(),
                            1 => "1 file".into(),
                            n => format!("{n} files"),
                        }),
                ),
        );
        if files.is_empty() {
            changes = changes.child(
                div()
                    .px_2()
                    .text_color(p.text_subtle.hsla())
                    .child("No changes yet"),
            );
        }
        for (n, (path, added, removed)) in files.into_iter().enumerate() {
            let action = OpenLocation {
                path: path.clone(),
                line: None,
            };
            let focus = self.focus.clone();
            changes = changes.child(
                div()
                    .id(("chat-change", n))
                    .test_support()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .cursor_pointer()
                    .child(div().truncate().child(file_name(&path).to_owned()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .flex_none()
                            .child(div().text_color(p.green.hsla()).child(format!("+{added}")))
                            .when(removed > 0, |d| {
                                d.child(
                                    div().text_color(p.rose.hsla()).child(format!("−{removed}")),
                                )
                            }),
                    )
                    .on_click(move |_, window, cx| {
                        window.focus(&focus, cx);
                        window.dispatch_action(Box::new(action.clone()), cx);
                    }),
            );
        }
        div()
            .id("chat-session-panel")
            .test_support()
            .flex()
            .flex_col()
            .flex_shrink(1.)
            .w(px(300.))
            .min_w_0()
            .gap(px(14.))
            .px(px(16.))
            .py(px(14.))
            .border_l_1()
            .border_color(p.surface.hsla())
            .child(self.render_status(cx))
            .child(plan)
            .child(self.render_criteria(cx))
            .child(changes)
            .child(self.render_rules(cx))
            .into_any_element()
    }
}

fn resolved_label(request: &PermissionRequest, outcome: Option<&PermissionOutcome>) -> String {
    match outcome {
        Some(PermissionOutcome::Selected { option_id }) => request
            .options
            .iter()
            .find(|o| &o.id == option_id)
            .map_or_else(
                || format!("Answered: {option_id}"),
                |o| format!("Answered: {}", o.name),
            ),
        Some(PermissionOutcome::Cancelled) | None => "Cancelled before an answer".into(),
    }
}

/// Where usage is billed, from the provider's auth status.
fn billing_label(t: &Transcript) -> Option<String> {
    let auth = t.auth.as_ref()?;
    let named = auth.label.clone().or_else(|| auth.plan.clone());
    let label = match auth.kind {
        AuthKind::Account => named.unwrap_or_else(|| "your plan".into()),
        AuthKind::ApiKey => named.unwrap_or_else(|| "API key".into()),
        AuthKind::Gateway => named.unwrap_or_else(|| "gateway".into()),
        AuthKind::External => named.unwrap_or_else(|| "cloud provider".into()),
        AuthKind::None => return Some("Signed out".into()),
        AuthKind::Unknown => return named.map(|n| format!("Billing: {n}")),
    };
    Some(format!("Billing: {label}"))
}

/// Context use, and cost where it is a real charge.
fn usage_label(t: &Transcript) -> Option<String> {
    let usage = t.usage.as_ref()?;
    let mut parts = Vec::new();
    match (usage.used_tokens, usage.context_size) {
        (Some(used), Some(size)) => parts.push(format!(
            "{} / {} tokens",
            format_tokens(used),
            format_tokens(size)
        )),
        (Some(used), None) => parts.push(format!("{} tokens", format_tokens(used))),
        _ => {}
    }
    // On a plan login the SDK's cost is notional, not a charge.
    let charged = t
        .auth
        .as_ref()
        .is_some_and(|a| matches!(a.kind, AuthKind::ApiKey | AuthKind::External));
    if let (true, Some(cost)) = (charged, &usage.cost) {
        parts.push(format!("{:.2} {}", cost.amount, cost.currency));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl Render for ChatTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_focus(window, cx);
        let banners = self.render_banners(cx);
        let composer = self.render_composer(cx);
        let panel = self.render_panel(cx);
        let state = self.state(cx);
        let t = cx.tokens();
        let p = &t.palette;
        let transcript = list(
            self.list.clone(),
            cx.processor(|this, ix: usize, _, cx| this.render_entry(ix, cx)),
        )
        .size_full();
        let empty = self.transcript.entries.is_empty();
        div()
            .id("chat-tab")
            .test_support()
            .key_context("ChatTab")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &CancelTurn, _, cx| this.cancel(cx)))
            .size_full()
            .flex()
            .bg(p.panel.hsla())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .id("chat-transcript")
                            .test_support()
                            .flex_1()
                            .min_h_0()
                            .text_size(px(13.5))
                            .line_height(px(21.))
                            .when(!empty, |d| d.child(transcript))
                            .when(empty, |d| {
                                d.child(
                                    div()
                                        .size_full()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .justify_center()
                                        .gap_2()
                                        .text_color(p.text_subtle.hsla())
                                        .child(agent_dot(state, "chat-state", cx))
                                        .child("Ask the agent anything about this workspace."),
                                )
                            }),
                    )
                    .child(
                        column(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(10.))
                                .children(banners)
                                .child(composer),
                        )
                        .pt(px(12.))
                        .pb(px(18.)),
                    ),
            )
            .child(panel)
    }
}
