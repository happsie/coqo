// SPDX-License-Identifier: Apache-2.0

//! `alerts/*`: the alert engine (PLAN §7.9, issue #76, ADR 0019).
//!
//! - **One alert per source and key** ([`AlertService::raise`]): raising
//!   again replaces it. A new kind or target is a new occurrence (unread,
//!   undismissed, on top); the same kind and target only updates the text,
//!   and nothing changes if nothing did. [`AlertService::resolve`] marks it
//!   resolved (and read) when its condition clears.
//! - **Store.** Every alert is a row of `alerts` in `coqo.db` ([`store`]),
//!   so alerts raised while no UI is open are there when one attaches. Each
//!   change gets the next `alerts/changes` seq.
//! - **Core sources** ([`agents`]): agent sessions' states, from
//!   `sessions/changes`. Waiting on a permission request, a question or
//!   blocked needs you; an error is an error; a finished turn is done.
//!   Each resolves when the session moves on.
//! - **Mods' sources** ([`ModAlerts`]): a mod contributes an alert source
//!   to `alert_sources` and raises alerts through `coqo.alerts@1`.
//! - **Settings:** do-not-disturb (for the UI and desktop notifications:
//!   alerts are still recorded) and muted sources (which raise nothing).
//!
//! **Access:** every route takes the daemon token.

pub mod agents;
pub(crate) mod store;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use coqo_agents::SessionManager;
use coqo_kernel::{ContributionRegistry, ProviderKind};
use coqo_mod_api::alerts::{AlertLevel, AlertLink, Alerts, RaiseAlert};
use coqo_mod_api::{ContributionId, ModError, ProviderId, points};
use coqo_protocol::methods::agents::SessionId;
use coqo_protocol::methods::alerts::{
    self as am, Alert, AlertId, AlertKind, AlertList, AlertSettings, AlertSettingsInfo,
    AlertSourceInfo, AlertTarget, AlertsChanged, ChangesParams, DismissAllParams, DismissParams,
    FileLocation, LIST_LIMIT, ListParams, MarkReadParams, SetSettingsParams,
};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, Sequenced};
use coqo_storage::Timestamp;
use futures_util::{StreamExt as _, stream};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use self::store::{Store, StoreError};
use crate::daemon::DaemonContext;
use crate::questions::QuestionService;

/// Live changes buffered before a slow subscriber falls back to the store.
const CHANGE_CHANNEL: usize = 256;
/// Alerts read per replay page.
const PAGE: usize = 64;
/// How long dismissed alerts are kept before they are deleted.
const KEEP_DISMISSED_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Alerts `alerts/dismissAll` dismisses at most at once.
const DISMISS_ALL_LIMIT: usize = 10_000;
/// Longest title and body, in characters.
const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 2000;

const DO_NOT_DISTURB: &str = "do_not_disturb";
const MUTED: &str = "muted";

/// A core alert source.
#[derive(Debug, Clone, Copy)]
pub struct CoreSource {
    /// Id, as in [`Alert::source`].
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// What it alerts about.
    pub description: &'static str,
}

/// The core sources, in the order settings list them.
pub const CORE_SOURCES: &[CoreSource] = &[
    CoreSource {
        id: agents::PERMISSION,
        name: "Permission requests",
        description: "An agent asks before it runs a command or edits a file.",
    },
    CoreSource {
        id: agents::QUESTION,
        name: "Questions",
        description: "An agent asked you a question and waits for the answer.",
    },
    CoreSource {
        id: agents::BLOCKED,
        name: "Blocked agents",
        description: "An agent says it cannot go on without you.",
    },
    CoreSource {
        id: agents::ERROR,
        name: "Agent errors",
        description: "A turn or an agent failed.",
    },
    CoreSource {
        id: agents::DONE,
        name: "Finished turns",
        description: "An agent finished its turn.",
    },
];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn storage_error(e: &StoreError) -> ErrorObject {
    ErrorObject::new(codes::STORAGE_ERROR, format!("alerts store: {e}"))
}

/// `s` cut to `max` characters, with `…`.
fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// An alert to raise ([`AlertService::raise`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAlert {
    /// The source.
    pub source: String,
    /// The source's key.
    pub key: String,
    /// The workspace it is about.
    pub workspace: Option<WorkspaceId>,
    /// The agent session it is about.
    pub session: Option<SessionId>,
    /// How much it wants the user.
    pub kind: AlertKind,
    /// One line.
    pub title: String,
    /// More detail.
    pub body: Option<String>,
    /// Where opening it goes.
    pub target: Option<AlertTarget>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    head: u64,
    settings: AlertSettings,
}

/// The daemon's alert engine. See the module docs.
#[derive(Debug)]
pub struct AlertService {
    inner: Mutex<Inner>,
    changes: broadcast::Sender<Sequenced<Alert>>,
    /// Where mods' alert sources are listed.
    contributions: Option<Arc<ContributionRegistry>>,
}

impl AlertService {
    /// The engine over the alerts in the global database at `db`. Mods'
    /// alert sources are read from `contributions`.
    pub fn open(
        db: &Path,
        contributions: Option<Arc<ContributionRegistry>>,
    ) -> Result<Arc<Self>, StoreError> {
        let store = Store::open(db)?;
        let pruned = store.prune_dismissed(Timestamp::now().0 - KEEP_DISMISSED_MS)?;
        if pruned > 0 {
            tracing::debug!(pruned, "deleted old dismissed alerts");
        }
        let head = store.head()?;
        let settings = AlertSettings {
            do_not_disturb: store.setting(DO_NOT_DISTURB)?.unwrap_or(false),
            muted: store.setting(MUTED)?.unwrap_or_default(),
        };
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                store,
                head,
                settings,
            }),
            changes: broadcast::channel(CHANGE_CHANNEL).0,
            contributions,
        }))
    }

    /// Write `alert` as the next change and publish it. Under the lock.
    fn publish(&self, inner: &mut Inner, mut alert: Alert, new: bool) -> Result<Alert, StoreError> {
        let seq = inner.head + 1;
        if new {
            alert = inner.store.insert(&alert, seq)?;
        } else {
            inner.store.update(&alert, seq)?;
            alert.seq = seq;
        }
        inner.head = seq;
        let _ = self.changes.send(Sequenced {
            seq,
            event: alert.clone(),
        });
        Ok(alert)
    }

    /// Raise (or replace) the alert of `new.source` under `new.key`.
    /// Returns the alert if it changed; `None` if nothing did or the source
    /// is muted.
    pub fn raise(&self, new: NewAlert) -> Result<Option<Alert>, StoreError> {
        let mut inner = lock(&self.inner);
        if inner.settings.muted.contains(&new.source) {
            return Ok(None);
        }
        let now = Timestamp::now().0;
        let title = clip(&new.title, MAX_TITLE);
        let body = new
            .body
            .as_deref()
            .map(|b| clip(b, MAX_BODY))
            .filter(|b| !b.is_empty());
        let existing = inner.store.by_key(&new.source, &new.key)?;
        let (alert, fresh_row) = match existing {
            None => (
                Alert {
                    id: AlertId(0),
                    workspace: new.workspace,
                    session: new.session,
                    source: new.source,
                    key: new.key,
                    kind: new.kind,
                    title,
                    body,
                    target: new.target,
                    created_at: now,
                    updated_at: now,
                    read: false,
                    dismissed: false,
                    resolved_at: None,
                    seq: 0,
                },
                true,
            ),
            Some(mut a) => {
                let occurrence =
                    a.resolved_at.is_some() || a.kind != new.kind || a.target != new.target;
                if !occurrence
                    && a.title == title
                    && a.body == body
                    && a.workspace == new.workspace
                    && a.session == new.session
                {
                    return Ok(None);
                }
                if occurrence {
                    a.created_at = now;
                    a.read = false;
                    a.dismissed = false;
                    a.resolved_at = None;
                }
                a.workspace = new.workspace;
                a.session = new.session;
                a.kind = new.kind;
                a.title = title;
                a.body = body;
                a.target = new.target;
                a.updated_at = now;
                (a, false)
            }
        };
        let alert = self.publish(&mut inner, alert, fresh_row)?;
        tracing::debug!(alert = %alert.id, source = %alert.source, key = %alert.key, kind = ?alert.kind, "alert raised");
        Ok(Some(alert))
    }

    /// The condition of `source`'s alert under `key` cleared: it is
    /// resolved and read. Returns the alert if it changed.
    pub fn resolve(&self, source: &str, key: &str) -> Result<Option<Alert>, StoreError> {
        let mut inner = lock(&self.inner);
        let Some(mut alert) = inner.store.by_key(source, key)? else {
            return Ok(None);
        };
        if alert.resolved_at.is_some() {
            return Ok(None);
        }
        let now = Timestamp::now().0;
        alert.resolved_at = Some(now);
        alert.read = true;
        alert.updated_at = now;
        let alert = self.publish(&mut inner, alert, false)?;
        tracing::debug!(alert = %alert.id, %source, %key, "alert resolved");
        Ok(Some(alert))
    }

    /// The current settings.
    pub fn settings(&self) -> AlertSettings {
        lock(&self.inner).settings.clone()
    }

    /// A value the engine's own sources keep (such as the last session
    /// change they handled).
    pub(crate) fn cursor(&self, key: &str) -> Option<u64> {
        match lock(&self.inner).store.setting(key) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(key, error = %e, "cannot read an alert cursor");
                None
            }
        }
    }

    pub(crate) fn set_cursor(&self, key: &str, value: u64) {
        if let Err(e) = lock(&self.inner).store.set_setting(key, &value) {
            tracing::warn!(key, error = %e, "cannot store an alert cursor");
        }
    }

    // ---- methods ---------------------------------------------------------------

    /// `alerts/list`.
    pub fn list(&self, p: ListParams) -> Result<AlertList, ErrorObject> {
        let inner = lock(&self.inner);
        let alerts = inner
            .store
            .list(p.workspace.as_ref(), LIST_LIMIT)
            .map_err(|e| storage_error(&e))?;
        Ok(AlertList {
            alerts,
            seq: inner.head,
        })
    }

    /// Apply `change` to each of `alerts` that it changes, publishing them.
    fn change_each(
        &self,
        inner: &mut Inner,
        alerts: Vec<Alert>,
        change: impl Fn(&mut Alert) -> bool,
    ) -> Result<AlertsChanged, ErrorObject> {
        let mut changed = Vec::new();
        for mut alert in alerts {
            if change(&mut alert) {
                alert.updated_at = Timestamp::now().0;
                let alert = self
                    .publish(inner, alert, false)
                    .map_err(|e| storage_error(&e))?;
                changed.push(alert.id);
            }
        }
        Ok(AlertsChanged { changed })
    }

    fn by_ids(inner: &Inner, ids: &[AlertId]) -> Result<Vec<Alert>, ErrorObject> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(a) = inner.store.get(*id).map_err(|e| storage_error(&e))? {
                out.push(a);
            }
        }
        Ok(out)
    }

    /// `alerts/markRead`.
    pub fn mark_read(&self, p: MarkReadParams) -> Result<AlertsChanged, ErrorObject> {
        let mut inner = lock(&self.inner);
        let mut alerts = Self::by_ids(&inner, &p.ids)?;
        if let Some(session) = &p.session {
            let unread = inner
                .store
                .unread_of(session)
                .map_err(|e| storage_error(&e))?;
            alerts.extend(unread.into_iter().filter(|a| !p.ids.contains(&a.id)));
        }
        self.change_each(&mut inner, alerts, |a| {
            !std::mem::replace(&mut a.read, true)
        })
    }

    /// `alerts/dismiss`.
    pub fn dismiss(&self, p: DismissParams) -> Result<AlertsChanged, ErrorObject> {
        let mut inner = lock(&self.inner);
        let alerts = Self::by_ids(&inner, &p.ids)?;
        self.change_each(&mut inner, alerts, |a| {
            a.read = true;
            !std::mem::replace(&mut a.dismissed, true)
        })
    }

    /// `alerts/dismissAll`.
    pub fn dismiss_all(&self, p: DismissAllParams) -> Result<AlertsChanged, ErrorObject> {
        let mut inner = lock(&self.inner);
        let alerts = inner
            .store
            .list(p.workspace.as_ref(), DISMISS_ALL_LIMIT)
            .map_err(|e| storage_error(&e))?;
        self.change_each(&mut inner, alerts, |a| {
            a.read = true;
            !std::mem::replace(&mut a.dismissed, true)
        })
    }

    /// The core sources and mods'.
    fn sources(&self, muted: &[String]) -> Vec<AlertSourceInfo> {
        let mut out: Vec<AlertSourceInfo> = CORE_SOURCES
            .iter()
            .map(|s| AlertSourceInfo {
                id: s.id.to_owned(),
                name: s.name.to_owned(),
                description: Some(s.description.to_owned()),
                muted: muted.iter().any(|m| m == s.id),
            })
            .collect();
        let mut mods: Vec<AlertSourceInfo> = self
            .contributions
            .as_ref()
            .and_then(|c| c.contributions_of::<points::AlertSources>().ok())
            .unwrap_or_default()
            .iter()
            .map(|c| AlertSourceInfo {
                id: c.value.id.to_string(),
                name: c.value.name.clone(),
                description: c.value.description.clone(),
                muted: muted.iter().any(|m| *m == c.value.id.as_str()),
            })
            .collect();
        mods.sort_by(|a, b| a.id.cmp(&b.id));
        out.extend(mods);
        out
    }

    /// `alerts/settings`.
    pub fn get_settings(&self) -> AlertSettingsInfo {
        let settings = self.settings();
        AlertSettingsInfo {
            sources: self.sources(&settings.muted),
            settings,
        }
    }

    /// `alerts/setSettings`.
    pub fn set_settings(&self, p: SetSettingsParams) -> Result<AlertSettingsInfo, ErrorObject> {
        {
            let mut inner = lock(&self.inner);
            let mut settings = inner.settings.clone();
            if let Some(dnd) = p.do_not_disturb {
                settings.do_not_disturb = dnd;
            }
            settings.muted.retain(|m| !p.unmute.contains(m));
            for m in p.mute {
                if !settings.muted.contains(&m) {
                    settings.muted.push(m);
                }
            }
            settings.muted.sort();
            if settings.do_not_disturb != inner.settings.do_not_disturb {
                inner
                    .store
                    .set_setting(DO_NOT_DISTURB, &settings.do_not_disturb)
                    .map_err(|e| storage_error(&e))?;
            }
            if settings.muted != inner.settings.muted {
                inner
                    .store
                    .set_setting(MUTED, &settings.muted)
                    .map_err(|e| storage_error(&e))?;
            }
            inner.settings = settings;
        }
        Ok(self.get_settings())
    }

    /// The `alerts/changes` feed: replay from the store after `since`, then
    /// live changes.
    pub fn changes(
        self: &Arc<Self>,
        p: ChangesParams,
        since: Option<u64>,
    ) -> Result<EventFeed<Alert>, ErrorObject> {
        let rx = self.changes.subscribe();
        let head = lock(&self.inner).head;
        let (after, replaying) = match since {
            None => (head, false),
            Some(n) if n > head => {
                return Err(coqo_protocol::subscription::ResumeGap {
                    needed: n.saturating_add(1),
                    oldest: 1,
                    head,
                }
                .to_error());
            }
            Some(n) => (n, n < head),
        };
        struct Cursor {
            svc: Weak<AlertService>,
            workspace: Option<WorkspaceId>,
            after: u64,
            replaying: bool,
            buf: VecDeque<Alert>,
            rx: broadcast::Receiver<Sequenced<Alert>>,
        }
        let cursor = Cursor {
            svc: Arc::downgrade(self),
            workspace: p.workspace,
            after,
            replaying,
            buf: VecDeque::new(),
            rx,
        };
        let events = stream::unfold(cursor, |mut c| async move {
            loop {
                if let Some(a) = c.buf.pop_front() {
                    if a.seq > c.after {
                        c.after = a.seq;
                        return Some((
                            Ok(Sequenced {
                                seq: a.seq,
                                event: a,
                            }),
                            c,
                        ));
                    }
                    continue;
                }
                if c.replaying {
                    let svc = c.svc.upgrade()?;
                    let page =
                        lock(&svc.inner)
                            .store
                            .changed_since(c.after, c.workspace.as_ref(), PAGE);
                    match page {
                        Ok(page) if page.is_empty() => c.replaying = false,
                        Ok(page) => c.buf.extend(page),
                        Err(e) => return Some((Err(storage_error(&e)), c)),
                    }
                    continue;
                }
                match c.rx.recv().await {
                    Ok(s) => {
                        if s.seq > c.after
                            && c.workspace
                                .as_ref()
                                .is_none_or(|w| s.event.workspace.as_ref() == Some(w))
                        {
                            c.after = s.seq;
                            return Some((Ok(s), c));
                        }
                    }
                    Err(RecvError::Lagged(_)) => c.replaying = true,
                    Err(RecvError::Closed) => return None,
                }
            }
        })
        .boxed();
        Ok(EventFeed { head, events })
    }
}

// ---- mods ---------------------------------------------------------------------

/// `coqo.alerts@1` for mods: alerts of the sources mods contribute to
/// `alert_sources`.
#[derive(Debug, Clone)]
pub struct ModAlerts {
    svc: Weak<AlertService>,
}

impl ModAlerts {
    /// The service over `svc`.
    pub fn new(svc: &Arc<AlertService>) -> Self {
        Self {
            svc: Arc::downgrade(svc),
        }
    }

    fn engine(&self) -> Result<Arc<AlertService>, ModError> {
        self.svc
            .upgrade()
            .ok_or_else(|| ModError::failed("the alert engine has stopped"))
    }

    /// Checks that `source` is a contributed alert source.
    fn check_source(engine: &AlertService, source: &ContributionId) -> Result<(), ModError> {
        let known = engine
            .contributions
            .as_ref()
            .and_then(|c| c.contributions_of::<points::AlertSources>().ok())
            .is_some_and(|sources| sources.iter().any(|c| c.value.id == *source));
        if known && !CORE_SOURCES.iter().any(|s| s.id == source.as_str()) {
            Ok(())
        } else {
            Err(ModError::PermissionDenied(format!(
                "`{source}` is not an alert source: contribute it to `alert_sources` first"
            )))
        }
    }
}

impl Alerts for ModAlerts {
    fn raise(&self, alert: RaiseAlert) -> Result<(), ModError> {
        let engine = self.engine()?;
        Self::check_source(&engine, &alert.source)?;
        let kind = match alert.level {
            AlertLevel::NeedsYou => AlertKind::NeedsYou,
            AlertLevel::Error => AlertKind::Error,
            AlertLevel::Done => AlertKind::Done,
            AlertLevel::Info => AlertKind::Info,
        };
        let (session, target) = match alert.link {
            None => (None, None),
            Some(AlertLink::Session { session }) => {
                let session = SessionId(session);
                (
                    Some(session.clone()),
                    Some(AlertTarget::Session { session }),
                )
            }
            Some(AlertLink::File { root, path, line }) => (
                None,
                Some(AlertTarget::File {
                    location: FileLocation {
                        root: RootId(root),
                        path,
                        line,
                    },
                }),
            ),
        };
        engine
            .raise(NewAlert {
                source: alert.source.to_string(),
                key: alert.key,
                workspace: alert.workspace.map(WorkspaceId),
                session,
                kind,
                title: alert.title,
                body: alert.body,
                target,
            })
            .map(drop)
            .map_err(ModError::other)
    }

    fn resolve(&self, source: &ContributionId, key: &str) -> Result<(), ModError> {
        let engine = self.engine()?;
        Self::check_source(&engine, source)?;
        engine
            .resolve(source.as_str(), key)
            .map(drop)
            .map_err(ModError::other)
    }
}

/// Who provides `coqo.alerts@1` in the service registry.
pub const PROVIDER: &str = "coqo.alerts";

/// The daemon's alert engine on its global database: follows `sessions`
/// (with `questions` for the question a session waits on) and provides
/// `coqo.alerts@1` to mods.
pub fn open(
    ctx: &DaemonContext,
    sessions: SessionManager,
    questions: Arc<QuestionService>,
) -> anyhow::Result<Arc<AlertService>> {
    let svc = AlertService::open(
        &ctx.data.global_db(),
        Some(Arc::clone(&ctx.mods.contributions)),
    )?;
    agents::AgentSource::new(Arc::clone(&svc), sessions, Some(questions))
        .spawn(ctx.shutdown.clone());
    let provider: ProviderId = PROVIDER.parse()?;
    let sink: Arc<dyn Alerts> = Arc::new(ModAlerts::new(&svc));
    ctx.mods.services.provide(
        provider,
        ProviderKind::Builtin,
        <dyn Alerts as coqo_mod_api::Service>::service_ref(),
        sink,
    );
    Ok(svc)
}

/// Register `alerts/*` on `router`.
pub fn register(router: Router, svc: Arc<AlertService>) -> Router {
    let list = Arc::clone(&svc);
    let read = Arc::clone(&svc);
    let dismiss = Arc::clone(&svc);
    let dismiss_all = Arc::clone(&svc);
    let get_settings = Arc::clone(&svc);
    let set_settings = Arc::clone(&svc);
    let changes = svc;
    router
        .method_with::<am::List, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(list.list(p))
        })
        .method_with::<am::MarkRead, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(read.mark_read(p))
        })
        .method_with::<am::Dismiss, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(dismiss.dismiss(p))
        })
        .method_with::<am::DismissAll, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(dismiss_all.dismiss_all(p))
        })
        .method_with::<am::GetSettings, _, _>(Access::daemon(), move |_ctx, ()| {
            std::future::ready(Ok(get_settings.get_settings()))
        })
        .method_with::<am::SetSettings, _, _>(Access::daemon(), move |_ctx, p| {
            std::future::ready(set_settings.set_settings(p))
        })
        .topic_with::<am::Changes, _, _>(Access::daemon(), move |_ctx, p, since| {
            std::future::ready(changes.changes(p, since))
        })
}
