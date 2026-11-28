// SPDX-License-Identifier: Apache-2.0

//! The user's settings in the UI (issue #26, PLAN §8, ADR 0032).
//!
//! The daemon owns the settings (`settings/*`): the user's `settings.toml`
//! in Coqo's config directory, each workspace's `.coqo/settings.toml` over
//! it, and the schema mods contribute. The UI keeps the daemon's latest
//! [`GetResult`] in the [`Settings`] global and reads values from it:
//!
//! ```ignore
//! let on = settings::get(cx).bool(settings::AUTO_REVEAL, Some(&workspace.id));
//! settings::set(cx, settings::AUTO_REVEAL, Some(true.into()), None);
//! ```
//!
//! [`set`] applies the value at once (observers of [`Settings`] hear it) and
//! writes it through the daemon, which keeps the file's comments. Until the
//! first answer, the user's file is read directly ([`load`]), so the theme
//! is right from the first frame.
//!
//! The settings page is [`page`]; the theme follows `appearance.theme`
//! ([`init`]).

pub mod page;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use coqo_protocol::methods::settings::{self as setm, ChangesParams, GetParams, GetResult};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_ui::{
    ActiveTokens as _, Appearance,
    gpui::{App, AppContext as _, Context, Entity, Global, Subscription, Task},
};
use serde_json::Value;

use crate::commands::keyfile;
use crate::daemon::{Daemon, DaemonEvent};
use crate::workspace::KnownWorkspaces;

/// The file's name in the config directory.
pub const FILE_NAME: &str = "settings.toml";

/// `appearance.theme`: `system`, `dark` or `light`.
pub const THEME: &str = "appearance.theme";
/// `files.auto_reveal`: switching to a file tab reveals its file in the
/// tree (expands its folders, selects and scrolls to its row) without moving
/// the focus. Off by default.
pub const AUTO_REVEAL: &str = "files.auto_reveal";
/// `questions.findings_in_gutter`: findings an agent writes as text
/// (`file:line - issue`) become dismissible gutter notes linked to their
/// chat message (ADR 0030). On by default.
pub const FINDINGS_IN_GUTTER: &str = "questions.findings_in_gutter";

/// The defaults of the settings the UI reads, for before the daemon sent
/// the schema (they match `coqod`'s `settings::workbench`).
fn ui_default(key: &str) -> Option<Value> {
    Some(match key {
        THEME => Value::String("system".into()),
        AUTO_REVEAL => Value::Bool(false),
        FINDINGS_IN_GUTTER => Value::Bool(true),
        _ => return None,
    })
}

/// Where the user's settings file is.
pub fn path() -> PathBuf {
    keyfile::config_dir().join(FILE_NAME)
}

/// The settings the UI knows: the daemon's latest snapshot, else what the
/// user's file said at start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// The daemon's last `settings/get`.
    snapshot: Option<GetResult>,
    /// The user's file as read at start, by dotted key (no schema).
    local: BTreeMap<String, Value>,
    /// Why the last write failed, for the settings page.
    error: Option<String>,
}

impl Global for Settings {}

impl Settings {
    /// The daemon's last snapshot: the schema, the files and their problems.
    pub fn snapshot(&self) -> Option<&GetResult> {
        self.snapshot.as_ref()
    }

    /// Why the last write failed.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The value of `key` in effect in `workspace` (`None`: the user's).
    pub fn value(&self, key: &str, workspace: Option<&WorkspaceId>) -> Option<Value> {
        if let Some(snapshot) = &self.snapshot
            && let Some(value) = snapshot.effective(key, workspace)
        {
            return Some(value);
        }
        self.local.get(key).cloned().or_else(|| ui_default(key))
    }

    /// A `true` / `false` setting; `false` when unknown or not a boolean.
    pub fn bool(&self, key: &str, workspace: Option<&WorkspaceId>) -> bool {
        self.value(key, workspace)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// The theme the user chose.
    pub fn theme(&self) -> Appearance {
        match self.value(THEME, None).as_ref().and_then(Value::as_str) {
            Some("dark") => Appearance::Dark,
            Some("light") => Appearance::Light,
            _ => Appearance::System,
        }
    }

    /// Put `value` into the snapshot's user or workspace file, as the
    /// daemon will once it wrote it.
    fn apply(&mut self, key: &str, value: Option<Value>, workspace: Option<&WorkspaceId>) {
        let values = match (&mut self.snapshot, workspace) {
            (Some(s), None) => Some(&mut s.user.values),
            (Some(s), Some(w)) => s
                .workspaces
                .iter_mut()
                .find(|f| &f.workspace == w)
                .map(|f| &mut f.file.values),
            (None, None) => Some(&mut self.local),
            (None, Some(_)) => None,
        };
        if let Some(values) = values {
            match value {
                Some(v) => values.insert(key.to_owned(), v),
                None => values.remove(key),
            };
        }
    }
}

/// Read the settings at `path`, without a schema: what a missing or broken
/// file has is nothing. For the first frame, before the daemon answers.
pub fn read(path: &Path) -> Settings {
    let local = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .map(|table| {
            let mut out = BTreeMap::new();
            flatten("", &table, &mut out);
            out
        })
        .unwrap_or_default();
    Settings {
        local,
        ..Settings::default()
    }
}

fn flatten(prefix: &str, table: &toml::Table, out: &mut BTreeMap<String, Value>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(t) => flatten(&path, t, out),
            other => {
                if let Ok(v) = serde_json::to_value(other) {
                    out.insert(path, v);
                }
            }
        }
    }
}

/// Read the user's settings at `path` into the app (see [`read`]).
pub fn load(path: &Path, cx: &mut App) {
    cx.set_global(read(path));
}

/// The app's settings (the defaults before [`load`]).
pub fn get(cx: &App) -> Settings {
    cx.try_global::<Settings>().cloned().unwrap_or_default()
}

fn set_global(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    let mut settings = get(cx);
    f(&mut settings);
    if cx.try_global::<Settings>() != Some(&settings) {
        cx.set_global(settings);
    }
}

/// Set `key` to `value` (`None`: remove it from the file, so the next
/// layer's value applies) in the user's file, or `workspace`'s. It applies
/// at once; the daemon writes it and says when it did (`settings/changes`).
/// A failed write is [`Settings::error`] and the daemon's values come back.
pub fn set(cx: &mut App, key: &str, value: Option<Value>, workspace: Option<WorkspaceId>) {
    set_global(cx, |s| {
        s.apply(key, value.clone(), workspace.as_ref());
        s.error = None;
    });
    let Some(daemon) = Daemon::global(cx) else {
        return;
    };
    let write = daemon.read(cx).request::<setm::Set>(setm::SetParams {
        key: key.to_owned(),
        value,
        workspace,
    });
    let key = key.to_owned();
    cx.spawn(async move |cx| {
        let result = write.await;
        cx.update(|cx| {
            if let Err(e) = result {
                let message = match &e {
                    crate::daemon::DaemonError::Rpc(e) => e.message.clone(),
                    other => format!("{other:?}"),
                };
                eprintln!("coqo: cannot save `{key}`: {message}");
                set_global(cx, |s| s.error = Some(message));
            }
            if let Some(sync) = SettingsSync::global(cx) {
                sync.update(cx, |sync, cx| sync.fetch(cx));
            }
        });
    })
    .detach();
}

/// Set `key` for the running app only, without writing it (tests).
#[cfg(test)]
pub fn set_local(cx: &mut App, key: &str, value: impl Into<Value>) {
    let value = value.into();
    set_global(cx, |s| s.apply(key, Some(value), None));
}

/// Flip a `true` / `false` setting in the user's file.
pub fn toggle(cx: &mut App, key: &str) {
    let on = get(cx).bool(key, None);
    set(cx, key, Some(Value::Bool(!on)), None);
}

/// Follows the daemon's settings into [`Settings`]. A GPUI global entity.
pub struct SettingsSync {
    /// The open workspaces the last fetch asked for.
    asked: Vec<WorkspaceId>,
    _fetch: Option<Task<()>>,
    _follow: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for SettingsSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsSync")
            .field("asked", &self.asked)
            .finish_non_exhaustive()
    }
}

struct GlobalSync(Entity<SettingsSync>);
impl Global for GlobalSync {}

/// Follow the daemon's settings, and apply `appearance.theme` whenever it
/// changes. Called from `main` after [`load`]; idempotent.
pub fn init(cx: &mut App) {
    if cx.has_global::<GlobalSync>() {
        return;
    }
    let sync = cx.new(|cx| {
        let mut subscriptions = Vec::new();
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(
                cx.subscribe(&daemon, |this: &mut SettingsSync, _, event, cx| {
                    if *event == DaemonEvent::Connected {
                        this.follow(cx);
                        this.fetch(cx);
                    }
                }),
            );
        }
        if let Some(known) = KnownWorkspaces::global(cx) {
            subscriptions.push(cx.observe(&known, |this: &mut SettingsSync, _, cx| {
                if this.asked != open_workspaces(cx) {
                    this.fetch(cx);
                }
            }));
        }
        let mut sync = SettingsSync {
            asked: Vec::new(),
            _fetch: None,
            _follow: None,
            _subscriptions: subscriptions,
        };
        sync.follow(cx);
        sync.fetch(cx);
        sync
    });
    cx.set_global(GlobalSync(sync));
    cx.observe_global::<Settings>(apply_theme).detach();
    apply_theme(cx);
}

/// Apply the chosen theme to every window, if it is not the one in use.
fn apply_theme(cx: &mut App) {
    let choice = get(cx).theme();
    if cx.tokens().choice == choice {
        return;
    }
    let window = cx.windows().into_iter().next();
    let applied = window.is_some_and(|w| {
        w.update(cx, |_, window, cx| {
            coqo_ui::theme::apply(choice, Some(window), cx)
        })
        .is_ok()
    });
    if !applied {
        coqo_ui::theme::apply(choice, None, cx);
    }
}

/// The open workspaces the UI knows, for `settings/get`.
fn open_workspaces(cx: &App) -> Vec<WorkspaceId> {
    let mut ids: Vec<WorkspaceId> = KnownWorkspaces::global(cx)
        .map(|k| {
            k.read(cx)
                .all()
                .iter()
                .filter(|w| w.open)
                .map(|w| w.id.clone())
                .collect()
        })
        .unwrap_or_default();
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    ids
}

impl SettingsSync {
    /// The app's follower, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSync>().map(|g| g.0.clone())
    }

    /// Get the settings again.
    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        self.asked = open_workspaces(cx);
        let got = daemon.read(cx).request::<setm::Get>(GetParams {
            workspaces: self.asked.clone(),
        });
        self._fetch = Some(cx.spawn(async move |_, cx| {
            let result = got.await;
            cx.update(|cx| match result {
                Ok(snapshot) => set_global(cx, |s| s.snapshot = Some(snapshot)),
                Err(e) => eprintln!("coqo: cannot get the settings: {e:?}"),
            });
        }));
    }

    /// Follow `settings/changes`: each change is a fetch.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let subscribe = daemon.read(cx).subscribe::<setm::Changes>(ChangesParams {});
        self._follow = Some(cx.spawn(async move |this, cx| {
            let Ok(mut events) = subscribe.await else {
                return;
            };
            while let Some(Ok(_)) = events.next().await {
                if this.update(cx, |this, cx| this.fetch(cx)).is_err() {
                    break;
                }
            }
        }));
    }
}
