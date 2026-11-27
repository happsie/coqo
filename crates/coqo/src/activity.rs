// SPDX-License-Identifier: Apache-2.0

//! What the agents in every workspace are doing (PLAN §7.9), for the rail's
//! badges.
//!
//! [`AgentActivity`] (one per app) follows every open session with a
//! [`Resumable`] over `sessions/list` (all workspaces) + `sessions/changes`,
//! so background workspaces stay current and a reattach replays what
//! changed meanwhile. [`AgentActivity::of`] sums one workspace up.

use std::collections::HashMap;

use coqo_protocol::methods::agents::{
    AgentState, Changes, ChangesParams, List, ListSessionsParams, SessionId, SessionInfo,
    SessionList,
};
use coqo_ui::gpui::{App, AppContext as _, Context, Entity, Global, Subscription};

use crate::resume::{Resumable, Update};

/// Every open agent session, kept current. See the module docs.
pub struct AgentActivity {
    sessions: HashMap<SessionId, SessionInfo>,
    _feed: Entity<Resumable<SessionList, SessionInfo>>,
    _subscription: Subscription,
}

impl std::fmt::Debug for AgentActivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentActivity")
            .field("sessions", &self.sessions.len())
            .finish_non_exhaustive()
    }
}

/// One workspace's sessions, by what they are doing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceActivity {
    /// Waiting for the user: a question, a permission request, or blocked.
    pub waiting: usize,
    /// Failed.
    pub errors: usize,
    /// Running a turn.
    pub working: usize,
    /// Finished their last turn.
    pub done: usize,
}

struct GlobalActivity(Entity<AgentActivity>);

impl Global for GlobalActivity {}

/// Create the app's [`AgentActivity`]. Call after
/// [`daemon::init`](crate::daemon::init), if there is a daemon.
pub fn init(cx: &mut App) -> Entity<AgentActivity> {
    let activity = cx.new(AgentActivity::new);
    cx.set_global(GlobalActivity(activity.clone()));
    activity
}

impl AgentActivity {
    fn new(cx: &mut Context<Self>) -> Self {
        let feed = cx.new(|cx| {
            Resumable::follow::<List, Changes>(
                ListSessionsParams::default(),
                |list| list.seq,
                ChangesParams::default(),
                cx,
            )
        });
        let subscription = cx.subscribe(&feed, |this, _, update, cx| {
            match update {
                Update::Snapshot(list) => {
                    this.sessions = list
                        .sessions
                        .iter()
                        .filter(|s| !s.closed)
                        .map(|s| (s.id.clone(), s.clone()))
                        .collect();
                }
                Update::Event(event) => this.apply(event.event.clone()),
            }
            cx.notify();
        });
        Self {
            sessions: HashMap::new(),
            _feed: feed,
            _subscription: subscription,
        }
    }

    /// The app's activity, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<AgentActivity>> {
        cx.try_global::<GlobalActivity>().map(|g| g.0.clone())
    }

    /// Replace the sessions, for screenshots without a daemon.
    #[cfg(test)]
    pub fn set_for_tests(&mut self, sessions: Vec<SessionInfo>, cx: &mut Context<Self>) {
        self.sessions.clear();
        for session in sessions {
            self.apply(session);
        }
        cx.notify();
    }

    fn apply(&mut self, info: SessionInfo) {
        if info.closed {
            self.sessions.remove(&info.id);
        } else {
            self.sessions.insert(info.id.clone(), info);
        }
    }

    /// What the sessions of `workspace` (a workspace id) are doing.
    pub fn of(&self, workspace: &str) -> WorkspaceActivity {
        WorkspaceActivity::of(self.sessions.values(), workspace)
    }

    /// One open session's latest info.
    pub fn info(&self, id: &SessionId) -> Option<&SessionInfo> {
        self.sessions.get(id)
    }

    /// The open sessions of `workspace` that are doing something or wait
    /// (not idle), those that need the user first, then by most recent
    /// change.
    pub fn busy_in(&self, workspace: &str) -> Vec<&SessionInfo> {
        busy_in(self.sessions.values(), workspace)
    }
}

/// See [`AgentActivity::busy_in`].
pub fn busy_in<'a>(
    sessions: impl IntoIterator<Item = &'a SessionInfo>,
    workspace: &str,
) -> Vec<&'a SessionInfo> {
    let rank = |state: AgentState| match state {
        AgentState::NeedsInput | AgentState::NeedsPermission | AgentState::Blocked => 0,
        AgentState::Error => 1,
        AgentState::Working => 2,
        AgentState::Done | AgentState::Idle => 3,
    };
    let mut busy: Vec<_> = sessions
        .into_iter()
        .filter(|s| {
            !s.closed && s.state != AgentState::Idle && s.workspace.as_deref() == Some(workspace)
        })
        .collect();
    busy.sort_by(|a, b| {
        rank(a.state)
            .cmp(&rank(b.state))
            .then(b.updated_at.cmp(&a.updated_at))
            .then(a.id.cmp(&b.id))
    });
    busy
}

impl WorkspaceActivity {
    /// Sum up the sessions of `workspace` among `sessions`.
    pub fn of<'a>(sessions: impl IntoIterator<Item = &'a SessionInfo>, workspace: &str) -> Self {
        let mut activity = Self::default();
        for session in sessions
            .into_iter()
            .filter(|s| !s.closed && s.workspace.as_deref() == Some(workspace))
        {
            match session.state {
                AgentState::NeedsInput | AgentState::NeedsPermission | AgentState::Blocked => {
                    activity.waiting += 1;
                }
                AgentState::Error => activity.errors += 1,
                AgentState::Working => activity.working += 1,
                AgentState::Done => activity.done += 1,
                AgentState::Idle => {}
            }
        }
        activity
    }
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::agents::{AgentState, SessionId, SessionInfo};

    use super::{WorkspaceActivity, busy_in};

    fn session(id: &str, workspace: &str, state: AgentState) -> SessionInfo {
        SessionInfo {
            id: SessionId(id.into()),
            provider: "fake".into(),
            workspace: Some(workspace.into()),
            cwd: "/tmp".into(),
            title: None,
            state,
            state_reason: None,
            state_since: None,
            status: None,
            criteria: Vec::new(),
            tool_use: Vec::new(),
            mode: None,
            closed: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn sums_up_one_workspace() {
        let mut closed = session("e", "ws-a", AgentState::NeedsInput);
        closed.closed = true;
        let sessions = [
            session("a", "ws-a", AgentState::NeedsPermission),
            session("b", "ws-a", AgentState::NeedsInput),
            session("c", "ws-a", AgentState::Working),
            session("d", "ws-a", AgentState::Idle),
            closed,
            session("f", "ws-b", AgentState::Error),
            session("g", "ws-b", AgentState::Done),
        ];
        assert_eq!(
            WorkspaceActivity::of(&sessions, "ws-a"),
            WorkspaceActivity {
                waiting: 2,
                errors: 0,
                working: 1,
                done: 0
            }
        );
        assert_eq!(
            WorkspaceActivity::of(&sessions, "ws-b"),
            WorkspaceActivity {
                waiting: 0,
                errors: 1,
                working: 0,
                done: 1
            }
        );
        assert_eq!(
            WorkspaceActivity::of(&sessions, "ws-c"),
            WorkspaceActivity::default()
        );
        // The tooltip's list: not idle, not closed, waiting ones first.
        let ids: Vec<_> = busy_in(&sessions, "ws-a")
            .into_iter()
            .map(|s| s.id.0.as_str())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
        let ids: Vec<_> = busy_in(&sessions, "ws-b")
            .into_iter()
            .map(|s| s.id.0.as_str())
            .collect();
        assert_eq!(ids, ["f", "g"]);
    }
}
