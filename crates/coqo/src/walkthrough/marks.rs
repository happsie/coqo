// SPDX-License-Identifier: Apache-2.0

//! Walkthrough steps in ordinary file tabs (#260, design board 3).
//!
//! [`WorkspaceWalkthroughs`] (one per workspace, [`walkthroughs_for`]) knows
//! the workspace's walkthroughs (`walkthroughs/list`, then
//! `walkthroughs/changes`) and keeps the *active* ones loaded
//! (`walkthroughs/get`, anchors resolved against the files and open buffers
//! now):
//!
//! - each publishing session's newest walkthrough that is not fully
//!   understood (a reader is most likely in the middle of that one);
//! - every walkthrough open in a player, finished or not.
//!
//! File tabs ([`crate::files::editor::FileEditor`]) take their step markers
//! from it ([`WorkspaceWalkthroughs::marks_in`]) and show the step popover in
//! place on a click; **Mark understood** goes through
//! [`WorkspaceWalkthroughs::set_state`], so the player, the session panel
//! and other windows follow. A walkthrough whose last step is marked
//! understood stops being active, and its markers go (unless a player shows
//! it).
//!
//! "Toggle walkthrough markers" (palette, [`toggle_current`]) hides or shows
//! them for the current workspace; the choice is kept in the workspace's
//! state (`ui.walkthrough.markers`).

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use coqo_protocol::methods::{
    agents::SessionId,
    codes,
    questions::AnchorStatus,
    walkthroughs::{
        self as wtm, ChangeKind, ChangesParams, ListParams, SetStepStateParams, StepState,
        Walkthrough, WalkthroughChange, WalkthroughId, WalkthroughParams, WalkthroughSummary,
    },
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    gpui::{
        App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Subscription,
        Task,
    },
    serde_json::Value,
};

use crate::{
    daemon::{Daemon, DaemonError, DaemonEvent},
    files::editor::STEP_MARKER_BASE,
    workspace::Workspaces,
};

/// The workspace state key of the toggle: `false` while hidden.
pub const SHOWN_KEY: &str = "ui.walkthrough.markers";

/// Markers per step: one per anchor (as the player's).
const MARKERS_PER_STEP: u64 = 16;
/// A walkthrough's markers in file tabs are `STEP_MARKER_BASE + (slot <<
/// SLOT_SHIFT) + step * 16 + anchor`, slot ≥ 1; the player's own editors
/// use slot 0.
const SLOT_SHIFT: u32 = 32;

/// A step's marker in a file tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStep {
    /// The marker's id (at least [`STEP_MARKER_BASE`]).
    pub id: u64,
    pub walkthrough: WalkthroughId,
    /// The step's index and its anchor's.
    pub step: usize,
    pub anchor: usize,
    /// 0-based, `end` exclusive, as the daemon resolved them.
    pub lines: Range<u32>,
    /// The step's number.
    pub label: SharedString,
}

/// What changed in a [`WorkspaceWalkthroughs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkthroughsEvent {
    /// The active walkthroughs, their steps or their progress changed, or
    /// the markers were shown or hidden.
    Changed,
}

/// A workspace's walkthroughs, for file tabs. See the module docs.
pub struct WorkspaceWalkthroughs {
    workspace: WorkspaceId,
    /// Newest first.
    summaries: Vec<WalkthroughSummary>,
    /// The active ones, with anchors resolved.
    loaded: HashMap<WalkthroughId, Walkthrough>,
    /// Marker slots, stable for as long as the app runs.
    slots: HashMap<WalkthroughId, u64>,
    /// Open players, by walkthrough.
    players: HashMap<WalkthroughId, usize>,
    shown: bool,
    /// `walkthroughs/get` in flight, and those to get again when it is back.
    getting: HashMap<WalkthroughId, Task<()>>,
    stale: HashSet<WalkthroughId>,
    _listing: Option<Task<()>>,
    _changes: Option<Task<()>>,
    _saving: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for WorkspaceWalkthroughs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceWalkthroughs")
            .field("workspace", &self.workspace)
            .field("summaries", &self.summaries.len())
            .field("loaded", &self.loaded.len())
            .field("shown", &self.shown)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<WalkthroughsEvent> for WorkspaceWalkthroughs {}

#[derive(Default)]
struct WalkthroughHub(HashMap<WorkspaceId, Entity<WorkspaceWalkthroughs>>);

impl Global for WalkthroughHub {}

/// The walkthroughs of `workspace`, followed from the first call on. `None`
/// without a daemon (some tests).
pub fn walkthroughs_for(
    workspace: &WorkspaceId,
    cx: &mut App,
) -> Option<Entity<WorkspaceWalkthroughs>> {
    Daemon::global(cx)?;
    if let Some(store) = cx.default_global::<WalkthroughHub>().0.get(workspace) {
        return Some(store.clone());
    }
    let id = workspace.clone();
    let store = cx.new(|cx| WorkspaceWalkthroughs::new(id, cx));
    cx.global_mut::<WalkthroughHub>()
        .0
        .insert(workspace.clone(), store.clone());
    Some(store)
}

/// "Toggle walkthrough markers": hide or show them in the current
/// workspace's file tabs.
pub fn toggle_current(cx: &mut App) {
    let Some(workspace) =
        Workspaces::global(cx).and_then(|w| w.read(cx).current().map(|w| w.id.clone()))
    else {
        return;
    };
    if let Some(store) = walkthroughs_for(&workspace, cx) {
        store.update(cx, |store, cx| {
            let shown = !store.is_shown();
            store.set_shown(shown, cx);
        });
    }
}

impl WorkspaceWalkthroughs {
    fn new(workspace: WorkspaceId, cx: &mut Context<Self>) -> Self {
        let shown = Workspaces::global(cx)
            .and_then(|w| {
                let w = w.read(cx);
                (w.current()?.id == workspace).then(|| w.state(SHOWN_KEY).cloned())
            })
            .flatten()
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mut subscriptions = Vec::new();
        let daemon = Daemon::global(cx);
        if let Some(daemon) = &daemon {
            subscriptions.push(cx.subscribe(daemon, |this, _, event, cx| {
                if *event == DaemonEvent::Connected {
                    this.follow(cx);
                }
            }));
        }
        let mut this = Self {
            workspace,
            summaries: Vec::new(),
            loaded: HashMap::new(),
            slots: HashMap::new(),
            players: HashMap::new(),
            shown,
            getting: HashMap::new(),
            stale: HashSet::new(),
            _listing: None,
            _changes: None,
            _saving: None,
            _subscriptions: subscriptions,
        };
        if daemon.is_some_and(|d| d.read(cx).is_connected()) {
            this.follow(cx);
        }
        this
    }

    fn daemon(cx: &App) -> Option<Entity<Daemon>> {
        Daemon::global(cx).filter(|d| d.read(cx).is_connected())
    }

    // ---- the daemon -----------------------------------------------------------

    /// Follow `walkthroughs/changes` and list (again): the topic is live
    /// only, so a new connection lists after subscribing.
    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let subscribe = daemon.read(cx).subscribe::<wtm::Changes>(ChangesParams {
            workspace: Some(self.workspace.clone()),
        });
        self._changes = Some(cx.spawn(async move |this, cx| {
            let Ok(mut events) = subscribe.await else {
                return;
            };
            this.update(cx, |this, cx| this.list(cx)).ok();
            while let Some(Ok(change)) = events.next().await {
                if this
                    .update(cx, |this, cx| this.changed(change, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn list(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let list = daemon.read(cx).request::<wtm::List>(ListParams {
            workspace: self.workspace.clone(),
            session: None,
        });
        self._listing = Some(cx.spawn(async move |this, cx| {
            if let Ok(list) = list.await {
                this.update(cx, |this, cx| {
                    this.summaries = list.walkthroughs;
                    // Anchors may have moved while not following: get all.
                    this.sync(true, None, cx);
                })
                .ok();
            }
        }));
    }

    fn changed(&mut self, change: WalkthroughChange, cx: &mut Context<Self>) {
        if change.workspace != self.workspace {
            return;
        }
        let id = change.id.clone();
        match (change.kind, change.summary) {
            (ChangeKind::Deleted, _) | (_, None) => {
                self.summaries.retain(|s| s.id != id);
                self.loaded.remove(&id);
            }
            (ChangeKind::Published, Some(summary)) => {
                // Published again (#253) it is the newest.
                self.summaries.retain(|s| s.id != id);
                self.summaries.insert(0, summary);
            }
            (ChangeKind::Updated, Some(summary)) => {
                match self.summaries.iter_mut().find(|s| s.id == id) {
                    Some(s) => *s = summary,
                    None => self.summaries.insert(0, summary),
                }
            }
        }
        self.sync(false, Some(&id), cx);
    }

    /// Load the active walkthroughs and forget the others: all of them again
    /// with `all`, else those not loaded and `changed`.
    fn sync(&mut self, all: bool, changed: Option<&WalkthroughId>, cx: &mut Context<Self>) {
        let active = self.active();
        self.loaded.retain(|id, _| active.contains(id));
        for id in &active {
            if all || Some(id) == changed || !self.loaded.contains_key(id) {
                self.get(id.clone(), cx);
            }
        }
        cx.emit(WalkthroughsEvent::Changed);
        cx.notify();
    }

    /// `walkthroughs/get` `id` (once more after the one in flight).
    fn get(&mut self, id: WalkthroughId, cx: &mut Context<Self>) {
        if self.getting.contains_key(&id) {
            self.stale.insert(id);
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let get = daemon
            .read(cx)
            .request::<wtm::Get>(WalkthroughParams { id: id.clone() });
        let key = id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = get.await;
            this.update(cx, |this, cx| {
                this.getting.remove(&id);
                match result {
                    Ok(w) if this.active().contains(&id) => {
                        this.loaded.insert(id.clone(), w);
                    }
                    Ok(_) => {}
                    Err(DaemonError::Rpc(e)) if e.code == codes::UNKNOWN_WALKTHROUGH => {
                        this.summaries.retain(|s| s.id != id);
                        this.loaded.remove(&id);
                    }
                    Err(e) => eprintln!("coqo: couldn't load walkthrough {}: {e}", id.0),
                }
                if this.stale.remove(&id) {
                    this.get(id, cx);
                }
                cx.emit(WalkthroughsEvent::Changed);
                cx.notify();
            })
            .ok();
        });
        self.getting.insert(key, task);
    }

    /// Get the active walkthroughs again (a file opened: their anchors are
    /// resolved against its buffer now).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        for id in self.active() {
            self.get(id, cx);
        }
    }

    // ---- what is shown ----------------------------------------------------------

    /// The active walkthroughs, newest first: each session's newest one not
    /// fully understood, and those open in a player. None while hidden.
    pub fn active(&self) -> Vec<WalkthroughId> {
        if !self.shown {
            return Vec::new();
        }
        let mut sessions = HashSet::new();
        let mut active = Vec::new();
        for s in &self.summaries {
            let unfinished = s.understood < s.steps;
            let held = self.players.contains_key(&s.id);
            if held || (unfinished && !sessions.contains(&s.session)) {
                active.push(s.id.clone());
            }
            if unfinished {
                sessions.insert(s.session.clone());
            }
        }
        // A player may show one not listed yet.
        for id in self.players.keys() {
            if !active.contains(id) {
                active.push(id.clone());
            }
        }
        active
    }

    /// Whether the markers are shown.
    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// Show or hide the markers in file tabs; kept in the workspace's state.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        if let Some(workspaces) = Workspaces::global(cx)
            && workspaces.read(cx).current().map(|w| &w.id) == Some(&self.workspace)
        {
            let value = if shown {
                Value::Null
            } else {
                Value::Bool(false)
            };
            let write = workspaces.update(cx, |w, cx| w.set_state(SHOWN_KEY, value, cx));
            self._saving = Some(cx.background_spawn(async move {
                if let Err(e) = write.await {
                    eprintln!("coqo: couldn't save the walkthrough markers' state: {e}");
                }
            }));
        }
        self.sync(false, None, cx);
    }

    /// The newest walkthrough `session` published, if any (the agents
    /// overview's **Walkthrough**, #265).
    pub fn latest_of(&self, session: &SessionId) -> Option<&WalkthroughSummary> {
        self.summaries.iter().find(|s| &s.session == session)
    }

    /// A loaded (active) walkthrough.
    pub fn walkthrough(&self, id: &WalkthroughId) -> Option<&Walkthrough> {
        self.loaded.get(id)
    }

    /// The step markers of `root`/`path`: the active walkthroughs' anchors
    /// there that were found, newest walkthrough first.
    pub fn marks_in(&mut self, root: RootId, path: &str) -> Vec<FileStep> {
        let mut marks = Vec::new();
        for id in self.active() {
            let Some(w) = self.loaded.get(&id) else {
                continue;
            };
            let next = self.slots.len() as u64 + 1;
            let slot = *self.slots.entry(id.clone()).or_insert(next);
            for (i, step) in w.steps.iter().enumerate() {
                for (j, a) in step.anchors.iter().enumerate() {
                    if a.anchor.root != root
                        || a.anchor.path != path
                        || super::anchor_status(a) == AnchorStatus::Orphaned
                    {
                        continue;
                    }
                    marks.push(FileStep {
                        id: STEP_MARKER_BASE
                            + (slot << SLOT_SHIFT)
                            + i as u64 * MARKERS_PER_STEP
                            + j as u64,
                        walkthrough: id.clone(),
                        step: i,
                        anchor: j,
                        lines: super::anchor_lines(a),
                        label: (i + 1).to_string().into(),
                    });
                }
            }
        }
        marks
    }

    // ---- progress and players -----------------------------------------------------

    /// Mark step `ix` of `id` (`walkthroughs/setStepState`); shown at once.
    pub fn set_state(
        &mut self,
        id: &WalkthroughId,
        ix: usize,
        state: StepState,
        cx: &mut Context<Self>,
    ) {
        let Some(w) = self.loaded.get_mut(id) else {
            return;
        };
        let Some(step) = w.steps.get(ix).map(|s| s.id.clone()) else {
            return;
        };
        if state == StepState::Unread {
            w.step_states.remove(&step);
        } else {
            w.step_states.insert(step.clone(), state);
        }
        let understood = super::understood_count(w) as u32;
        if let Some(s) = self.summaries.iter_mut().find(|s| &s.id == id) {
            s.understood = understood;
        }
        self.sync(false, None, cx);
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let call = daemon
            .read(cx)
            .request::<wtm::SetStepState>(SetStepStateParams {
                id: id.clone(),
                step,
                state,
            });
        self._saving = Some(cx.spawn(async move |_, _| {
            if let Err(e) = call.await {
                eprintln!("coqo: couldn't save the walkthrough's progress: {e}");
            }
        }));
    }

    /// A player of `id` opened: its markers show while it is open.
    pub fn player_opened(&mut self, id: &WalkthroughId, cx: &mut Context<Self>) {
        *self.players.entry(id.clone()).or_default() += 1;
        self.sync(false, None, cx);
    }

    /// A player of `id` closed.
    pub fn player_closed(&mut self, id: &WalkthroughId, cx: &mut Context<Self>) {
        if let Some(n) = self.players.get_mut(id) {
            *n -= 1;
            if *n == 0 {
                self.players.remove(id);
            }
        }
        self.sync(false, None, cx);
    }
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::{
        agents::SessionId,
        walkthroughs::{ScopeKind, StorageMode},
    };

    use super::*;

    fn summary(id: &str, session: &str, steps: u32, understood: u32) -> WalkthroughSummary {
        WalkthroughSummary {
            id: WalkthroughId(id.into()),
            workspace: WorkspaceId("w".into()),
            session: SessionId(session.into()),
            title: id.into(),
            scope: ScopeKind::Worktree,
            steps,
            understood,
            questions: 0,
            start: None,
            storage: StorageMode::Local,
            coverage: None,
            created_at: 0,
            revision: 0,
        }
    }

    #[test]
    fn the_active_walkthroughs_are_each_sessions_newest_unfinished_and_the_open_ones() {
        let mut store = WorkspaceWalkthroughs {
            workspace: WorkspaceId("w".into()),
            // Newest first.
            summaries: vec![
                summary("a3", "a", 3, 3), // finished
                summary("a2", "a", 3, 1), // a's newest unfinished
                summary("a1", "a", 3, 0), // older
                summary("b1", "b", 2, 0), // b's
                summary("c1", "c", 2, 2), // finished
            ],
            loaded: HashMap::new(),
            slots: HashMap::new(),
            players: HashMap::new(),
            shown: true,
            getting: HashMap::new(),
            stale: HashSet::new(),
            _listing: None,
            _changes: None,
            _saving: None,
            _subscriptions: Vec::new(),
        };
        let ids = |store: &WorkspaceWalkthroughs| -> Vec<String> {
            store.active().into_iter().map(|id| id.0).collect()
        };
        assert_eq!(ids(&store), ["a2", "b1"]);
        // A player shows a finished one too.
        store.players.insert(WalkthroughId("c1".into()), 1);
        assert_eq!(ids(&store), ["a2", "b1", "c1"]);
        store.shown = false;
        assert!(ids(&store).is_empty(), "hidden");
    }
}
