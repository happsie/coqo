// SPDX-License-Identifier: Apache-2.0

//! `settings/*`: the settings service (issue #26, PLAN §8, ADR 0032).
//!
//! The daemon owns the user's `settings.toml` (in Coqo's config directory,
//! `$COQO_CONFIG_DIR` or `~/.config/coqo`) and each workspace's
//! `.coqo/settings.toml` (in its first root), and checks both against the
//! schema mods contribute ([`schema`]). It follows the files: an edit by
//! hand shows up within [`POLL`]. A file that stops being valid TOML is
//! reported and keeps its last good values; a value that does not fit its
//! setting is reported and left out, so the next layer's applies.
//!
//! - `settings/get`: the sections, the user's file and the workspaces'
//!   files asked for (each one is followed from then on).
//! - `settings/set`: write one value with `toml_edit` ([`file`]), keeping
//!   comments and order. A broken file is never written over.
//! - `settings/changes`: live; something changed, get again.
//!
//! **Access:** the daemon token only.

pub mod file;
pub mod schema;
pub mod workbench;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use coqo_mod_api::contrib::SettingScope;
use coqo_protocol::methods::codes::{
    INVALID_SETTING, IO_ERROR, SETTINGS_FILE_INVALID, UNKNOWN_SETTING,
};
use coqo_protocol::methods::settings::{
    self as setm, FileSettings, GetParams, GetResult, SetParams, SetResult, SettingsChanged,
    SettingsProblem, WorkspaceSettings,
};
use coqo_protocol::methods::workspace::{WorkspaceId, WorkspaceParams};
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, SeqLog};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::mods::Mods;
use crate::workspace::WorkspaceService;
use file::RawValues;
use schema::Schema;

/// The settings file's name, in the config directory and in `.coqo/`.
pub const FILE_NAME: &str = "settings.toml";
/// The folder of a workspace's settings, in its first root.
pub const WORKSPACE_DIR: &str = ".coqo";
/// The variable that moves the config directory (tests, portable setups).
pub use coqo_client::CONFIG_DIR_ENV;
/// How often the files are read again for edits by hand.
pub const POLL: Duration = Duration::from_millis(500);
/// Change events kept for slow subscribers.
const CHANGE_LOG: usize = 16;

/// Coqo's per-user config directory: `$COQO_CONFIG_DIR` if set, otherwise
/// the platform's (`~/.config/coqo`, `~/Library/Application Support/Coqo`).
/// The UI's `keybindings.toml` is there too.
pub fn config_dir() -> PathBuf {
    coqo_client::config_dir()
}

/// The user's settings at `config_dir`, read once without a schema, for what
/// is set up before the mods start (Claude's options). Problems are logged.
pub fn startup_values(config_dir: &Path) -> BTreeMap<String, Value> {
    let path = config_dir.join(FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read the settings");
            return BTreeMap::new();
        }
    };
    match file::parse(FILE_NAME, &text) {
        Ok(raw) => raw.into_iter().map(|(k, v)| (k, v.value)).collect(),
        Err(problem) => {
            tracing::warn!(path = %path.display(), "{}", problem.message);
            BTreeMap::new()
        }
    }
}

/// One file the service follows.
#[derive(Debug, Clone)]
struct Tracked {
    path: PathBuf,
    /// The text last read; `None` while the file is missing.
    text: Option<String>,
    /// The values of the last valid text.
    raw: RawValues,
    /// Why the current text is not valid TOML, or cannot be read.
    syntax: Option<SettingsProblem>,
    workspace_file: bool,
}

impl Tracked {
    fn new(path: PathBuf, workspace_file: bool) -> Self {
        let mut t = Self {
            path,
            text: None,
            raw: RawValues::new(),
            syntax: None,
            workspace_file,
        };
        t.reload();
        t
    }

    fn label(&self) -> String {
        if self.workspace_file {
            format!("{WORKSPACE_DIR}/{FILE_NAME}")
        } else {
            FILE_NAME.to_owned()
        }
    }

    /// Read the file again. Returns whether its text changed.
    fn reload(&mut self) -> bool {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                let problem = SettingsProblem {
                    key: None,
                    line: None,
                    message: format!("cannot read {}: {e}", self.path.display()),
                };
                let changed = self.syntax.as_ref() != Some(&problem);
                self.syntax = Some(problem);
                return changed;
            }
        };
        if text == self.text && self.text.is_some() {
            return false;
        }
        let changed = text != self.text || self.syntax.is_some();
        match &text {
            None => {
                self.raw.clear();
                self.syntax = None;
            }
            Some(t) => match file::parse(&self.label(), t) {
                Ok(raw) => {
                    self.raw = raw;
                    self.syntax = None;
                }
                Err(problem) => {
                    // The last good values stay.
                    tracing::warn!(path = %self.path.display(), "{}", problem.message);
                    self.syntax = Some(problem);
                }
            },
        }
        self.text = text;
        changed
    }

    fn view(&self, schema: &Schema) -> FileSettings {
        schema.file_settings(
            self.path.display().to_string(),
            self.text.is_some(),
            &self.raw,
            self.syntax.as_ref(),
            self.workspace_file,
        )
    }
}

#[derive(Debug)]
struct State {
    schema: Schema,
    user: Tracked,
    workspaces: BTreeMap<WorkspaceId, Tracked>,
    generation: u64,
}

/// The daemon's settings service.
#[derive(Debug)]
pub struct SettingsService {
    mods: Option<Mods>,
    workspaces: Option<Arc<WorkspaceService>>,
    state: Mutex<State>,
    changes: Arc<SeqLog<SettingsChanged>>,
}

impl SettingsService {
    /// The service for `config_dir`, with the schema from `mods` and the
    /// workspaces' roots from `workspaces`. Follows the files every `poll`
    /// and the schema as mods change, until `shutdown`. Needs a tokio
    /// runtime.
    pub fn open(
        config_dir: &Path,
        mods: Option<Mods>,
        workspaces: Option<Arc<WorkspaceService>>,
        poll: Duration,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let schema = mods
            .as_ref()
            .map(|m| Schema::from_registry(&m.contributions))
            .unwrap_or_default();
        let user = Tracked::new(config_dir.join(FILE_NAME), false);
        for problem in user.view(&schema).problems {
            tracing::warn!(path = %user.path.display(), "{}", problem.message);
        }
        let svc = Arc::new(Self {
            mods,
            workspaces,
            state: Mutex::new(State {
                schema,
                user,
                workspaces: BTreeMap::new(),
                generation: 0,
            }),
            changes: Arc::new(SeqLog::new(CHANGE_LOG)),
        });
        let watcher = Arc::clone(&svc);
        let mut schema_changes = svc.mods.as_ref().map(|m| m.contributions.subscribe());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(poll);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = tick.tick() => watcher.poll_files(),
                    changed = async {
                        match &mut schema_changes {
                            Some(rx) => rx.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if changed.is_err() {
                            schema_changes = None;
                        } else {
                            watcher.reload_schema();
                        }
                    }
                }
            }
        });
        svc
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bump the generation and tell subscribers.
    fn changed(&self, state: &mut State) {
        state.generation += 1;
        self.changes.publish(SettingsChanged {
            generation: state.generation,
        });
    }

    /// Read every followed file again; publish once if any changed.
    pub fn poll_files(&self) {
        let mut state = self.lock();
        let mut any = state.user.reload();
        for tracked in state.workspaces.values_mut() {
            any |= tracked.reload();
        }
        if any {
            tracing::debug!("settings files changed");
            self.changed(&mut state);
        }
    }

    /// Take the schema from the mods again; publish if it changed.
    pub fn reload_schema(&self) {
        let Some(mods) = &self.mods else { return };
        let schema = Schema::from_registry(&mods.contributions);
        let mut state = self.lock();
        if state.schema != schema {
            state.schema = schema;
            self.changed(&mut state);
        }
    }

    /// Where `workspace`'s settings file is: `.coqo/settings.toml` in its
    /// first root.
    async fn workspace_path(&self, workspace: &WorkspaceId) -> Result<PathBuf, ErrorObject> {
        let Some(workspaces) = &self.workspaces else {
            return Err(ErrorObject::new(
                coqo_protocol::methods::codes::UNKNOWN_WORKSPACE,
                format!("no workspace `{}`", workspace.0),
            ));
        };
        let info = workspaces
            .get(WorkspaceParams {
                id: workspace.clone(),
            })
            .await?;
        let root = info.roots.first().ok_or_else(|| {
            ErrorObject::new(
                coqo_protocol::methods::codes::UNKNOWN_ROOT,
                "the workspace has no roots",
            )
        })?;
        Ok(Path::new(&root.path).join(WORKSPACE_DIR).join(FILE_NAME))
    }

    /// Follow `workspace`'s file (again, if its first root moved).
    async fn track(&self, workspace: &WorkspaceId) -> Result<(), ErrorObject> {
        let path = self.workspace_path(workspace).await?;
        let mut state = self.lock();
        if state
            .workspaces
            .get(workspace)
            .is_none_or(|t| t.path != path)
        {
            state
                .workspaces
                .insert(workspace.clone(), Tracked::new(path, true));
        }
        Ok(())
    }

    /// `settings/get`.
    pub async fn get(&self, p: GetParams) -> GetResult {
        for workspace in &p.workspaces {
            if let Err(e) = self.track(workspace).await {
                tracing::debug!(workspace = %workspace.0, error = %e.message, "no workspace settings");
            }
        }
        let state = self.lock();
        GetResult {
            sections: state.schema.sections.clone(),
            user: state.user.view(&state.schema),
            workspaces: p
                .workspaces
                .iter()
                .filter_map(|w| {
                    let tracked = state.workspaces.get(w)?;
                    Some(WorkspaceSettings {
                        workspace: w.clone(),
                        file: tracked.view(&state.schema),
                    })
                })
                .collect(),
            generation: state.generation,
        }
    }

    /// `settings/set`.
    pub async fn set(&self, p: SetParams) -> Result<SetResult, ErrorObject> {
        let value = p.value.filter(|v| !v.is_null());
        {
            let state = self.lock();
            let Some(field) = state.schema.field(&p.key) else {
                return Err(ErrorObject::new(
                    UNKNOWN_SETTING,
                    format!("`{}` is not a known setting", p.key),
                ));
            };
            if p.workspace.is_some() && field.scope == SettingScope::User {
                return Err(ErrorObject::new(
                    INVALID_SETTING,
                    format!(
                        "`{}` can only be set in your user settings, not a workspace's",
                        p.key
                    ),
                ));
            }
            if let Some(v) = &value {
                field
                    .check_value(v)
                    .map_err(|e| ErrorObject::new(INVALID_SETTING, format!("`{}`: {e}", p.key)))?;
            }
        }
        if let Some(workspace) = &p.workspace {
            self.track(workspace).await?;
        }
        let mut state = self.lock();
        let tracked = match &p.workspace {
            None => &mut state.user,
            Some(w) => state.workspaces.get_mut(w).ok_or_else(|| {
                ErrorObject::internal("the workspace's settings are not followed")
            })?,
        };
        tracked.reload();
        let text = tracked.text.clone().unwrap_or_default();
        let out = match file::write_value(&tracked.label(), &text, &p.key, value.as_ref()) {
            Ok(out) => out,
            Err(file::WriteError::Syntax(problem)) => {
                return Err(ErrorObject::new(
                    SETTINGS_FILE_INVALID,
                    format!(
                        "{}; fix it first, Coqo won't write over it",
                        problem.message
                    ),
                ));
            }
            Err(file::WriteError::Conflict(message)) => {
                return Err(ErrorObject::new(INVALID_SETTING, message));
            }
        };
        if out != text || (value.is_some() && tracked.text.is_none()) {
            file::write_atomic(&tracked.path, &out).map_err(|e| {
                ErrorObject::new(
                    IO_ERROR,
                    format!("cannot write {}: {e}", tracked.path.display()),
                )
            })?;
            tracing::info!(key = %p.key, path = %tracked.path.display(), "setting written");
            if tracked.reload() {
                self.changed(&mut state);
            }
        }
        Ok(SetResult {
            generation: state.generation,
        })
    }

    /// `settings/changes`.
    pub fn follow(&self) -> Result<EventFeed<SettingsChanged>, ErrorObject> {
        self.changes.subscribe(None)
    }
}

/// Register `settings/*` on `router`.
pub fn register(router: Router, svc: Arc<SettingsService>) -> Router {
    let get = Arc::clone(&svc);
    let set = Arc::clone(&svc);
    router
        .method_with::<setm::Get, _, _>(Access::daemon(), move |_, p| {
            let svc = Arc::clone(&get);
            async move { Ok(svc.get(p).await) }
        })
        .method_with::<setm::Set, _, _>(Access::daemon(), move |_, p| {
            let svc = Arc::clone(&set);
            async move { svc.set(p).await }
        })
        .topic_with::<setm::Changes, _, _>(Access::daemon(), move |_, _, _| {
            std::future::ready(svc.follow())
        })
}
