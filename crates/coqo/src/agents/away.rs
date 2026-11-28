// SPDX-License-Identifier: Apache-2.0

//! "Welcome back" (board 6's reattach banner): what the agents did while
//! the UI was away.
//!
//! [`Presence`] (one per app) remembers when the UI was last attached to
//! `coqod`: it writes the time every [`HEARTBEAT`] while attached and when
//! the app quits, to a [`LastSeenStore`] (a small file next to the saved
//! layouts, so it survives the UI being closed). When the UI attaches (it
//! starts, or reconnects) and was last attached more than [`AWAY_AFTER`]
//! ago, it asks the daemon for the sessions, alerts and changed files
//! (`sessions/list`, `alerts/list`, `agents/changedSince`) and sums up what
//! changed since then ([`WelcomeBack`]): sessions that finished, failed,
//! need you or still work, the files agents changed, and the alerts raised
//! meanwhile that are unread. A heartbeat that comes more than
//! [`AWAY_AFTER`] late (the machine slept with the UI attached) is summed
//! up the same way ([`Away::Asleep`]). Nothing is shown after a quick
//! reconnect, or when nothing happened. The banner stays until dismissed.

use std::{cell::Cell, path::PathBuf, rc::Rc, time::Duration};

use coqo_protocol::methods::{
    agents::{self as agm, AgentState, ChangedSinceParams, ListSessionsParams, SessionInfo},
    alerts::{self as am, ListParams},
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FontWeight, Global,
        InteractiveElement as _, IntoElement as _, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
        linear_color_stop, linear_gradient, px,
    },
    serde_json,
    widgets::{Icon, TestSupportExt as _, icon, primary, raised},
};

use super::{Group, group, ordered, title, workspace_name};
use crate::{
    alerts::now_ms,
    chat::agent_dot,
    daemon::{Daemon, DaemonEvent},
};

/// Away longer than this, the UI says what happened meanwhile.
pub const AWAY_AFTER: Duration = Duration::from_secs(10 * 60);

/// How often the UI notes that it is attached.
pub const HEARTBEAT: Duration = Duration::from_secs(60);

/// Sessions the banner names at most.
const BANNER_SESSIONS: usize = 4;

/// Where the time the UI was last attached is kept.
pub trait LastSeenStore {
    /// The time, Unix milliseconds, if known.
    fn load(&self) -> Option<i64>;
    /// Remember `at`.
    fn save(&self, at: i64);
}

/// A [`LastSeenStore`] in a small JSON file.
#[derive(Debug)]
pub struct FileLastSeen(PathBuf);

impl FileLastSeen {
    /// The store in `path`.
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }
}

impl LastSeenStore for FileLastSeen {
    fn load(&self) -> Option<i64> {
        let text = std::fs::read_to_string(&self.0).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        value.get("lastSeenMs")?.as_i64()
    }

    fn save(&self, at: i64) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.0.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.0.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::json!({ "lastSeenMs": at }).to_string())?;
            std::fs::rename(tmp, &self.0)
        };
        if let Err(err) = write() {
            eprintln!(
                "coqo: couldn't save when the UI was last attached ({}): {err}",
                self.0.display()
            );
        }
    }
}

/// A [`LastSeenStore`] in memory.
#[derive(Debug, Default)]
pub struct MemoryLastSeen(Cell<Option<i64>>);

impl MemoryLastSeen {
    /// A store that remembers `at`.
    #[allow(dead_code)] // Tests use it.
    pub fn at(at: Option<i64>) -> Self {
        Self(Cell::new(at))
    }
}

impl LastSeenStore for MemoryLastSeen {
    fn load(&self) -> Option<i64> {
        self.0.get()
    }

    fn save(&self, at: i64) {
        self.0.set(Some(at));
    }
}

/// When the UI was away from, if it was away long enough to sum it up.
pub fn away_since(last_seen: Option<i64>, now: i64) -> Option<i64> {
    let after = i64::try_from(AWAY_AFTER.as_millis()).unwrap_or(i64::MAX);
    last_seen.filter(|&at| now - at > after)
}

/// How the UI was away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Away {
    /// It was closed: this is its first attach since it started.
    Closed,
    /// It lost the daemon a while, and reconnected.
    Disconnected,
    /// It stayed attached, but its heartbeat came late: the machine slept.
    Asleep,
}

/// What happened while the UI was away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeBack {
    /// Away from, Unix milliseconds.
    pub since: i64,
    /// Back at.
    pub at: i64,
    /// How it was away.
    pub away: Away,
    /// The sessions it is about, in overview order.
    pub sessions: Vec<SessionInfo>,
    /// Alerts raised meanwhile and still unread.
    pub unread_alerts: usize,
    /// Files agents changed meanwhile (`agents/changedSince`).
    pub files_changed: u32,
}

/// Counts of what the sessions did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AwayCounts {
    pub finished: usize,
    pub answer: usize,
    pub permission: usize,
    pub blocked: usize,
    pub working: usize,
    pub errors: usize,
}

impl WelcomeBack {
    /// Sum up `sessions`, `alerts` and the `files_changed` as of `at`, for
    /// a UI away since `since`. `None` if nothing happened worth telling.
    ///
    /// A session counts if it needs you or works now (whenever that
    /// began), or finished or failed while the UI was away.
    pub fn build(
        since: i64,
        at: i64,
        away: Away,
        sessions: &[SessionInfo],
        alerts: &[am::Alert],
        files_changed: u32,
    ) -> Option<Self> {
        let sessions: Vec<SessionInfo> = ordered(sessions)
            .into_iter()
            .filter(|s| match group(s.state) {
                Group::NeedsYou | Group::Working => true,
                Group::Error => super::since(s) >= since,
                Group::Done => s.state == AgentState::Done && super::since(s) >= since,
            })
            .cloned()
            .collect();
        let unread_alerts = alerts
            .iter()
            .filter(|a| a.is_unread() && a.created_at >= since)
            .count();
        (!sessions.is_empty() || unread_alerts > 0).then_some(Self {
            since,
            at,
            away,
            sessions,
            unread_alerts,
            files_changed,
        })
    }

    /// How many sessions did what.
    pub fn counts(&self) -> AwayCounts {
        let mut c = AwayCounts::default();
        for s in &self.sessions {
            match s.state {
                AgentState::Done => c.finished += 1,
                AgentState::NeedsInput => c.answer += 1,
                AgentState::NeedsPermission => c.permission += 1,
                AgentState::Blocked => c.blocked += 1,
                AgentState::Working => c.working += 1,
                AgentState::Error => c.errors += 1,
                AgentState::Idle => {}
            }
        }
        c
    }

    /// The banner's title.
    pub fn headline(&self) -> &'static str {
        match self.away {
            Away::Closed => "Welcome back. Your agents kept working while Coqo was closed.",
            Away::Disconnected => {
                "Welcome back. Your agents kept working while Coqo was disconnected."
            }
            Away::Asleep => "Welcome back. Your agents kept working while you were away.",
        }
    }

    /// The banner's sentence: "In the last 2 h 14 m the daemon ran 5
    /// sessions: 1 finished, 1 is waiting on your answer, … and 1 hit an
    /// error. 12 files changed. 3 alerts are unread."
    pub fn sentence(&self) -> String {
        let c = self.counts();
        let n = |count: usize, one: &str, many: &str| match count {
            0 => None,
            1 => Some(format!("1 {one}")),
            n => Some(format!("{n} {many}")),
        };
        let parts: Vec<String> = [
            n(c.finished, "finished", "finished"),
            n(
                c.answer,
                "is waiting on your answer",
                "are waiting on your answer",
            ),
            n(c.permission, "needs permission", "need permission"),
            n(c.blocked, "is blocked", "are blocked"),
            n(c.working, "is still working", "are still working"),
            n(c.errors, "hit an error", "hit an error"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let away = away_for(self.at - self.since);
        let mut out = if self.sessions.is_empty() {
            format!("In the last {away} nothing changed for your agents.")
        } else {
            let sessions = match self.sessions.len() {
                1 => "1 session".to_owned(),
                n => format!("{n} sessions"),
            };
            let mut out = format!(
                "In the last {away} the daemon ran {sessions}: {}.",
                list(&parts)
            );
            match self.files_changed {
                0 => {}
                1 => out.push_str(" 1 file changed."),
                n => out.push_str(&format!(" {n} files changed.")),
            }
            out
        };
        match self.unread_alerts {
            0 => {}
            1 => out.push_str(" 1 alert is unread."),
            n => out.push_str(&format!(" {n} alerts are unread.")),
        }
        out
    }

    /// The session to go to first: the one that needs you the longest,
    /// else the first in overview order.
    pub fn first(&self) -> Option<&SessionInfo> {
        self.sessions.first()
    }
}

/// "a", "a and b", "a, b and c".
fn list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// How long the UI was away: "2 h 14 m", "45 m", "3 d 2 h".
pub fn away_for(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    let (d, h, m) = (mins / 1440, (mins / 60) % 24, mins % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m} m")
    } else {
        format!("{m} m")
    }
}

// ---- presence -------------------------------------------------------------------

/// When the UI was last attached, and the welcome back to show. See the
/// module docs.
pub struct Presence {
    store: Rc<dyn LastSeenStore>,
    last_seen: Option<i64>,
    /// Whether the UI has attached since it started.
    attached_before: bool,
    welcome: Option<WelcomeBack>,
    _heartbeat: Option<Task<()>>,
    _summary: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for Presence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presence")
            .field("last_seen", &self.last_seen)
            .field("welcome", &self.welcome.is_some())
            .finish_non_exhaustive()
    }
}

struct GlobalPresence(Entity<Presence>);

impl Global for GlobalPresence {}

/// Create the app's [`Presence`] over `store`. Call after
/// [`daemon::init`](crate::daemon::init), before the first connection is up.
pub fn init(store: Rc<dyn LastSeenStore>, cx: &mut App) -> Entity<Presence> {
    let presence = cx.new(|cx| Presence::new(store, cx));
    cx.set_global(GlobalPresence(presence.clone()));
    presence
}

impl Presence {
    fn new(store: Rc<dyn LastSeenStore>, cx: &mut Context<Self>) -> Self {
        let last_seen = store.load();
        let mut this = Self {
            store,
            last_seen,
            attached_before: false,
            welcome: None,
            _heartbeat: None,
            _summary: None,
            _subscriptions: vec![cx.on_app_quit(|this, _| {
                this.touch(now_ms());
                async {}
            })],
        };
        if let Some(daemon) = Daemon::global(cx) {
            this._subscriptions
                .push(cx.subscribe(&daemon, |this, _, event, cx| match event {
                    DaemonEvent::Connected => this.attached(now_ms(), cx),
                    DaemonEvent::Disconnected => this._heartbeat = None,
                }));
            if daemon.read(cx).is_connected() {
                this.attached(now_ms(), cx);
            }
        }
        this
    }

    /// The app's presence, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<Presence>> {
        cx.try_global::<GlobalPresence>().map(|g| g.0.clone())
    }

    /// The welcome back to show, if any.
    pub fn welcome(&self) -> Option<&WelcomeBack> {
        self.welcome.as_ref()
    }

    /// Hide the welcome back.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.welcome.take().is_some() {
            cx.notify();
        }
    }

    /// Show `welcome`, for screenshots.
    #[cfg(test)]
    pub fn set_for_tests(&mut self, welcome: Option<WelcomeBack>, cx: &mut Context<Self>) {
        self.welcome = welcome;
        cx.notify();
    }

    fn touch(&mut self, at: i64) {
        self.last_seen = Some(at);
        self.store.save(at);
    }

    /// The UI attached at `now`.
    fn attached(&mut self, now: i64, cx: &mut Context<Self>) {
        let away = if self.attached_before {
            Away::Disconnected
        } else {
            Away::Closed
        };
        self.attached_before = true;
        if let Some(since) = away_since(self.last_seen, now) {
            self.summarise(since, now, away, cx);
        }
        self.touch(now);
        self._heartbeat = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HEARTBEAT).await;
                if this.update(cx, |this, cx| this.beat(now_ms(), cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// The heartbeat, at `now` (wall clock). Timers don't run while the
    /// machine sleeps, so a heartbeat [`AWAY_AFTER`] late means the UI
    /// stayed attached through a sleep: sum it up like an absence.
    fn beat(&mut self, now: i64, cx: &mut Context<Self>) {
        if let Some(since) = away_since(self.last_seen, now) {
            self.summarise(since, now, Away::Asleep, cx);
        }
        self.touch(now);
    }

    /// Pretend the UI was last seen at `at`, as if the next heartbeat came
    /// late.
    #[cfg(test)]
    pub fn set_last_seen_for_tests(&mut self, at: i64) {
        self.last_seen = Some(at);
    }

    fn summarise(&mut self, since: i64, now: i64, away: Away, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let daemon = daemon.read(cx);
        let sessions = daemon.request::<agm::List>(ListSessionsParams::default());
        let alerts = daemon.request::<am::List>(ListParams::default());
        let files = daemon.request::<agm::ChangedSince>(ChangedSinceParams {
            since,
            workspace: None,
        });
        self._summary = Some(cx.spawn(async move |this, cx| {
            let sessions = match sessions.await {
                Ok(list) => list.sessions,
                Err(err) => {
                    eprintln!("coqo: couldn't list the sessions for the welcome back: {err}");
                    return;
                }
            };
            // Alerts and files are a bonus: without them, still sum up the
            // sessions.
            let alerts = alerts.await.map(|l| l.alerts).unwrap_or_default();
            let files = files.await.map(|f| f.files).unwrap_or_default();
            let welcome = WelcomeBack::build(since, now, away, &sessions, &alerts, files);
            this.update(cx, |this, cx| {
                if welcome.is_some() {
                    this.welcome = welcome;
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

// ---- the banner -----------------------------------------------------------------

type OnSession = Rc<dyn Fn(&SessionInfo, &mut Window, &mut App)>;
type OnClick = Rc<dyn Fn(&mut Window, &mut App)>;

/// What the banner's links and buttons do.
#[derive(Clone)]
pub struct BannerActions {
    /// A session was clicked (or **Go to the first one**).
    pub open: OnSession,
    /// The unread alerts were clicked.
    pub alerts: OnClick,
    /// **Dismiss**.
    pub dismiss: OnClick,
}

/// The welcome back banner (board 6's "While you were away").
pub fn banner(welcome: &WelcomeBack, actions: &BannerActions, cx: &App) -> AnyElement {
    let t = cx.tokens();
    let (p, m) = (&t.palette, &t.metrics);
    let chips = welcome
        .sessions
        .iter()
        .take(BANNER_SESSIONS)
        .enumerate()
        .map(|(ix, s)| {
            let open = Rc::clone(&actions.open);
            let info = s.clone();
            let place = workspace_name(s, cx)
                .map(|w| format!(" · {w}"))
                .unwrap_or_default();
            div()
                .id(("welcome-back-session", ix))
                .test_support()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .py(px(4.))
                .rounded(px(9.))
                .bg(p.surface.hsla())
                .cursor_pointer()
                .hover(|s| s.bg(p.surface_selected.hsla()))
                .text_size(m.text_sm)
                .child(agent_dot(s.state, ("welcome-back-dot", ix), cx))
                .child(
                    div()
                        .flex()
                        .child(div().font_weight(FontWeight::MEDIUM).child(title(s)))
                        .child(div().text_color(p.text_muted.hsla()).child(place)),
                )
                .on_click(move |_, window, cx| open(&info, window, cx))
        });
    let alerts = (welcome.unread_alerts > 0).then(|| {
        let open = Rc::clone(&actions.alerts);
        let label = match welcome.unread_alerts {
            1 => "1 unread alert".to_owned(),
            n => format!("{n} unread alerts"),
        };
        div()
            .id("welcome-back-alerts")
            .test_support()
            .px(px(10.))
            .py(px(4.))
            .rounded(px(9.))
            .cursor_pointer()
            .text_size(m.text_sm)
            .text_color(p.blue.hsla())
            .hover(|s| s.bg(p.surface.hsla()))
            .child(label)
            .on_click(move |_, window, cx| open(window, cx))
    });
    let first = welcome.first().cloned();
    let go = first.map(|info| {
        let open = Rc::clone(&actions.open);
        let dismiss = Rc::clone(&actions.dismiss);
        primary(div(), cx)
            .id("welcome-back-go")
            .test_support()
            .px(px(16.))
            .py(px(8.))
            .rounded(px(10.))
            .cursor_pointer()
            .text_size(px(12.5))
            .child("Go to the first one")
            .on_click(move |_, window, cx| {
                open(&info, window, cx);
                dismiss(window, cx);
            })
    });
    let dismiss = Rc::clone(&actions.dismiss);
    // Board 6: a sage-tinted corner fading into the panel.
    let tint = p.panel_raised.mix(p.sage, 0.1);
    div()
        .id("welcome-back")
        .test_support()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(16.))
        .px(px(20.))
        .py(px(18.))
        .rounded(px(18.))
        .bg(linear_gradient(
            135.,
            linear_color_stop(tint.hsla(), 0.),
            linear_color_stop(p.panel_raised.hsla(), 0.7),
        ))
        .shadow(vec![coqo_ui::gpui::BoxShadow {
            color: coqo_ui::gpui::hsla(0., 0., 0., 0.25),
            offset: coqo_ui::gpui::point(px(0.), px(4.)),
            blur_radius: px(14.),
            spread_radius: px(0.),
            inset: false,
        }])
        .text_color(p.text.hsla())
        .child(icon(Icon::History, px(28.), p.sage.hsla()))
        .child(
            div()
                .flex_1()
                .min_w(px(380.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(welcome.headline()),
                )
                .child(
                    div()
                        .id("welcome-back-summary")
                        .test_support()
                        .text_color(p.text_muted.hsla())
                        .line_height(px(19.5))
                        .child(welcome.sentence()),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .pt(px(4.))
                        .children(chips)
                        .children(alerts),
                ),
        )
        .child(
            div().flex().gap(px(8.)).children(go).child(
                raised(div(), cx)
                    .id("welcome-back-dismiss")
                    .test_support()
                    .px(px(14.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .child("Dismiss")
                    .on_click(move |_, window, cx| dismiss(window, cx)),
            ),
        )
        .into_any_element()
}

#[cfg(test)]
mod unit_tests {
    use coqo_protocol::methods::agents::AgentState;

    use super::{Away, WelcomeBack, away_for, away_since};
    use crate::agents::unit_tests::session;

    const MIN: i64 = 60_000;

    #[test]
    fn only_a_long_absence_is_summed_up() {
        let now = 1000 * MIN;
        assert_eq!(away_since(None, now), None, "first start ever");
        assert_eq!(
            away_since(Some(now - 2 * MIN), now),
            None,
            "a quick reconnect"
        );
        assert_eq!(away_since(Some(now - 10 * MIN), now), None);
        assert_eq!(
            away_since(Some(now - 134 * MIN), now),
            Some(now - 134 * MIN)
        );
    }

    #[test]
    fn says_what_happened_while_away() {
        let now = 1000 * MIN;
        let since = now - 134 * MIN;
        let sessions = [
            session("done", AgentState::Done, now - 60 * MIN),
            session("answer", AgentState::NeedsInput, now - 18 * MIN),
            session("permission", AgentState::NeedsPermission, now - 4 * MIN),
            session("working", AgentState::Working, now - 7 * MIN),
            session("error", AgentState::Error, now - 41 * MIN),
            // Before the UI left: old news.
            session("done-before", AgentState::Done, since - MIN),
            session("idle", AgentState::Idle, now - MIN),
        ];
        let welcome = WelcomeBack::build(since, now, Away::Closed, &sessions, &[], 0).unwrap();
        let ids: Vec<_> = welcome.sessions.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, ["answer", "permission", "error", "working", "done"]);
        assert_eq!(welcome.first().unwrap().id.0, "answer");
        assert_eq!(
            welcome.sentence(),
            "In the last 2 h 14 m the daemon ran 5 sessions: 1 finished, 1 is waiting on \
             your answer, 1 needs permission, 1 is still working and 1 hit an error."
        );
        assert!(
            WelcomeBack::build(since, now, Away::Closed, &sessions[5..], &[], 0).is_none(),
            "nothing happened"
        );
        // The files agents changed meanwhile (#265).
        let welcome = WelcomeBack::build(since, now, Away::Asleep, &sessions[..1], &[], 12);
        let welcome = welcome.unwrap();
        assert_eq!(
            welcome.sentence(),
            "In the last 2 h 14 m the daemon ran 1 session: 1 finished. 12 files changed."
        );
        assert_eq!(
            welcome.headline(),
            "Welcome back. Your agents kept working while you were away."
        );
        assert_eq!(away_for(45 * MIN), "45 m");
        assert_eq!(away_for(26 * 60 * MIN), "1 d 2 h");
    }
}
