// SPDX-License-Identifier: Apache-2.0

//! The alert center (design board 6, PLAN §7.9, issue #76): what needs you
//! across workspaces, from the daemon's alert engine (`alerts/*`).
//!
//! - [`AlertCenter`] (one per app) follows `alerts/list` + `alerts/changes`
//!   with a [`Resumable`], so a reattach replays what changed while the UI
//!   was away, and knows do-not-disturb (`alerts/settings`).
//! - The rail's bell ([`view::bell`]) shows the unread count: `sand` when
//!   one needs you, `rose` when one failed. It opens the alert center
//!   ([`view::center`]): the alerts newest first, grouped by workspace, each
//!   opening where it points (the chat tab, the question's code) and
//!   dismissable.
//! - A **toast** ([`view::toast`]) shows a new alert that needs you in the
//!   app's active window, unless that window shows the session's chat or
//!   do-not-disturb is on. It hides after [`TOAST_FOR`].
//! - Opening an alert marks it read, and so does showing its session's chat
//!   in a focused window ([`AlertCenter::seen`]).

pub mod view;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use coqo_protocol::methods::agents::SessionId;
use coqo_protocol::methods::alerts::{
    self as am, Alert, AlertId, AlertKind, AlertList, ChangesParams, DismissAllParams,
    DismissParams, ListParams, MarkReadParams, SetSettingsParams,
};
use coqo_ui::gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task,
};

use crate::daemon::{Daemon, DaemonEvent};
use crate::resume::{Resumable, Update};

/// How long a toast stays.
pub const TOAST_FOR: Duration = Duration::from_secs(6);

/// What the bell's badge says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BellBadge {
    /// Unread alerts.
    pub unread: usize,
    /// The most pressing kind among them.
    pub kind: AlertKind,
}

/// A new occurrence of an alert, live (not from a snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raised(pub Alert);

/// Every alert not dismissed, kept current. See the module docs.
pub struct AlertCenter {
    alerts: HashMap<AlertId, Alert>,
    do_not_disturb: bool,
    /// Sessions whose alerts are being marked read.
    marking: HashSet<SessionId>,
    _feed: Entity<Resumable<AlertList, Alert>>,
    _subscriptions: Vec<Subscription>,
    _settings: Option<Task<()>>,
}

impl std::fmt::Debug for AlertCenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlertCenter")
            .field("alerts", &self.alerts.len())
            .field("do_not_disturb", &self.do_not_disturb)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<Raised> for AlertCenter {}

struct GlobalCenter(Entity<AlertCenter>);

impl Global for GlobalCenter {}

/// Create the app's [`AlertCenter`]. Call after
/// [`daemon::init`](crate::daemon::init), if there is a daemon.
pub fn init(cx: &mut App) -> Entity<AlertCenter> {
    let center = cx.new(AlertCenter::new);
    cx.set_global(GlobalCenter(center.clone()));
    center
}

impl AlertCenter {
    fn new(cx: &mut Context<Self>) -> Self {
        let feed = cx.new(|cx| {
            Resumable::follow::<am::List, am::Changes>(
                ListParams::default(),
                |list| list.seq,
                ChangesParams::default(),
                cx,
            )
        });
        let mut subscriptions = vec![cx.subscribe(&feed, |this, _, update, cx| {
            match update {
                Update::Snapshot(list) => {
                    this.alerts = list.alerts.iter().map(|a| (a.id, a.clone())).collect();
                }
                Update::Event(event) => this.apply(event.event.clone(), cx),
            }
            cx.notify();
        })];
        let mut this = Self {
            alerts: HashMap::new(),
            do_not_disturb: false,
            marking: HashSet::new(),
            _feed: feed,
            _subscriptions: Vec::new(),
            _settings: None,
        };
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| {
                if *event == DaemonEvent::Connected {
                    this.fetch_settings(cx);
                }
            }));
            if daemon.read(cx).is_connected() {
                this.fetch_settings(cx);
            }
        }
        this._subscriptions = subscriptions;
        this
    }

    /// The app's alert center, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<AlertCenter>> {
        cx.try_global::<GlobalCenter>().map(|g| g.0.clone())
    }

    /// Replace the alerts, for screenshots without a daemon.
    #[cfg(test)]
    pub fn set_for_tests(&mut self, alerts: Vec<Alert>, cx: &mut Context<Self>) {
        self.alerts = alerts.into_iter().map(|a| (a.id, a)).collect();
        cx.notify();
    }

    fn daemon(cx: &App) -> Option<Entity<Daemon>> {
        Daemon::global(cx).filter(|d| d.read(cx).is_connected())
    }

    fn fetch_settings(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let settings = daemon.read(cx).request::<am::GetSettings>(());
        self._settings = Some(cx.spawn(async move |this, cx| match settings.await {
            Ok(info) => {
                this.update(cx, |this, cx| {
                    this.do_not_disturb = info.settings.do_not_disturb;
                    cx.notify();
                })
                .ok();
            }
            Err(err) => eprintln!("coqo: couldn't read the alert settings: {err}"),
        }));
    }

    fn apply(&mut self, alert: Alert, cx: &mut Context<Self>) {
        if alert.dismissed {
            self.alerts.remove(&alert.id);
            return;
        }
        let before = self.alerts.get(&alert.id);
        let new = before.is_none_or(|b| b.created_at != alert.created_at || !b.is_active());
        if new && alert.is_unread() {
            cx.emit(Raised(alert.clone()));
        }
        self.alerts.insert(alert.id, alert);
    }

    /// The alerts not dismissed, most recently raised first.
    pub fn alerts(&self) -> Vec<&Alert> {
        let mut list: Vec<_> = self.alerts.values().collect();
        list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        list
    }

    /// One alert.
    pub fn get(&self, id: AlertId) -> Option<&Alert> {
        self.alerts.get(&id)
    }

    /// Whether do-not-disturb is on.
    pub fn do_not_disturb(&self) -> bool {
        self.do_not_disturb
    }

    /// What the bell says, if anything is unread.
    pub fn badge(&self) -> Option<BellBadge> {
        let unread: Vec<_> = self.alerts.values().filter(|a| a.is_unread()).collect();
        let rank = |k: AlertKind| match k {
            AlertKind::NeedsYou => 0,
            AlertKind::Error => 1,
            AlertKind::Info => 2,
            AlertKind::Done => 3,
        };
        let kind = unread.iter().map(|a| a.kind).min_by_key(|k| rank(*k))?;
        Some(BellBadge {
            unread: unread.len(),
            kind,
        })
    }

    /// Mark `ids` read now, and in the daemon.
    pub fn mark_read(&mut self, ids: Vec<AlertId>, cx: &mut Context<Self>) {
        let ids: Vec<_> = ids
            .into_iter()
            .filter(|id| self.alerts.get(id).is_some_and(|a| !a.read))
            .collect();
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            if let Some(a) = self.alerts.get_mut(id) {
                a.read = true;
            }
        }
        cx.notify();
        self.send::<am::MarkRead>(
            MarkReadParams { ids, session: None },
            "mark alerts read",
            cx,
        );
    }

    /// The chats of `sessions` are on screen in a focused window: their
    /// alerts are read.
    pub fn seen(&mut self, sessions: &[SessionId], cx: &mut Context<Self>) {
        for session in sessions {
            let unread = self
                .alerts
                .values()
                .any(|a| a.is_unread() && a.session.as_ref() == Some(session));
            if !unread || !self.marking.insert(session.clone()) {
                continue;
            }
            let Some(daemon) = Self::daemon(cx) else {
                self.marking.remove(session);
                continue;
            };
            let marked = daemon.read(cx).request::<am::MarkRead>(MarkReadParams {
                ids: Vec::new(),
                session: Some(session.clone()),
            });
            let session = session.clone();
            cx.spawn(async move |this, cx| {
                if let Err(err) = marked.await {
                    eprintln!("coqo: couldn't mark alerts read: {err}");
                }
                this.update(cx, |this, _| this.marking.remove(&session))
                    .ok();
            })
            .detach();
        }
    }

    /// Dismiss alert `id`.
    pub fn dismiss(&mut self, id: AlertId, cx: &mut Context<Self>) {
        if self.alerts.remove(&id).is_some() {
            cx.notify();
        }
        self.send::<am::Dismiss>(DismissParams { ids: vec![id] }, "dismiss the alert", cx);
    }

    /// Dismiss every alert.
    pub fn dismiss_all(&mut self, cx: &mut Context<Self>) {
        self.alerts.clear();
        cx.notify();
        self.send::<am::DismissAll>(DismissAllParams::default(), "dismiss the alerts", cx);
    }

    /// Turn do-not-disturb on or off.
    pub fn set_do_not_disturb(&mut self, on: bool, cx: &mut Context<Self>) {
        self.do_not_disturb = on;
        cx.notify();
        self.send::<am::SetSettings>(
            SetSettingsParams {
                do_not_disturb: Some(on),
                ..SetSettingsParams::default()
            },
            "change do-not-disturb",
            cx,
        );
    }

    fn send<M: coqo_protocol::Method>(
        &self,
        params: M::Params,
        what: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(daemon) = Self::daemon(cx) else {
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
}

/// How long ago `then` (Unix milliseconds) was, short: `now`, `4m`, `2h`,
/// `3d`.
pub fn age(then: i64, now: i64) -> String {
    let secs = (now - then).max(0) / 1000;
    match secs {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Now, Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
