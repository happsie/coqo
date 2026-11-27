// SPDX-License-Identifier: Apache-2.0

//! A session's folded state: what the transcript adds up to (plan, modes,
//! commands, usage, turns, open permission requests). Rebuilt from the
//! transcript when a session is loaded.

use std::collections::{BTreeMap, HashMap};

use coqo_protocol::methods::agents::{
    AuthStatus, AvailableCommand, PermissionRequest, PermissionRequestId, PlanEntry, SessionModes,
    SessionUpdate, ToolCallId, Usage,
};

use crate::rules::ToolFacts;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Fold {
    /// Turns started.
    pub turns: u64,
    /// The turn started but not ended, if any.
    pub open_turn: Option<u64>,
    /// Permission requests not resolved yet.
    pub pending: BTreeMap<PermissionRequestId, PermissionRequest>,
    /// The highest permission request id used.
    pub last_permission: u64,
    pub plan: Vec<PlanEntry>,
    pub modes: Option<SessionModes>,
    pub commands: Vec<AvailableCommand>,
    pub usage: Option<Usage>,
    pub auth: Option<AuthStatus>,
    /// What each tool call is, for permission rules.
    pub tools: HashMap<ToolCallId, ToolFacts>,
}

impl Fold {
    pub fn apply(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::TurnStarted { turn } => {
                self.turns = self.turns.max(*turn);
                self.open_turn = Some(*turn);
            }
            SessionUpdate::TurnEnded { turn, .. } => {
                if self.open_turn == Some(*turn) {
                    self.open_turn = None;
                }
            }
            SessionUpdate::PermissionRequested(r) => {
                self.last_permission = self.last_permission.max(r.id.0);
                self.pending.insert(r.id, r.clone());
            }
            SessionUpdate::PermissionResolved { request_id, .. } => {
                self.pending.remove(request_id);
            }
            SessionUpdate::Plan { entries } => self.plan.clone_from(entries),
            SessionUpdate::AvailableCommands { commands } => self.commands.clone_from(commands),
            SessionUpdate::Modes(m) => self.modes = Some(m.clone()),
            SessionUpdate::CurrentMode { mode_id } => {
                if let Some(m) = &mut self.modes {
                    m.current_mode_id.clone_from(mode_id);
                }
            }
            SessionUpdate::Usage(u) => self.usage = Some(u.clone()),
            SessionUpdate::AuthStatus(a) => self.auth = Some(a.clone()),
            SessionUpdate::ToolCall(c) => {
                self.tools.insert(c.id.clone(), ToolFacts::of_call(c));
            }
            SessionUpdate::ToolCallUpdate(u) => {
                self.tools.entry(u.id.clone()).or_default().apply(u);
            }
            SessionUpdate::UserMessage { .. }
            | SessionUpdate::UserMessageChunk { .. }
            | SessionUpdate::AgentMessageChunk { .. }
            | SessionUpdate::AgentThoughtChunk { .. }
            | SessionUpdate::State { .. }
            | SessionUpdate::Question(_)
            | SessionUpdate::Closed => {}
        }
    }

    /// The current mode id, if the session has modes.
    pub fn mode(&self) -> Option<&str> {
        self.modes.as_ref().map(|m| m.current_mode_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_protocol::methods::agents::{
        PermissionOutcome, SessionMode, StopReason, ToolCallUpdate,
    };

    #[test]
    fn folds_turns_permissions_and_modes() {
        let mut f = Fold::default();
        let req = PermissionRequest {
            id: PermissionRequestId(3),
            title: "Write".into(),
            tool_call: Some(ToolCallUpdate::default()),
            options: vec![],
            suggested_rule: None,
        };
        for u in [
            SessionUpdate::Modes(SessionModes {
                current_mode_id: "default".into(),
                available_modes: vec![SessionMode {
                    id: "plan".into(),
                    name: "Plan".into(),
                    description: None,
                }],
            }),
            SessionUpdate::TurnStarted { turn: 1 },
            SessionUpdate::PermissionRequested(req.clone()),
            SessionUpdate::CurrentMode {
                mode_id: "plan".into(),
            },
        ] {
            f.apply(&u);
        }
        assert_eq!(f.open_turn, Some(1));
        assert_eq!(f.pending.len(), 1);
        assert_eq!(f.last_permission, 3);
        assert_eq!(f.mode(), Some("plan"));
        f.apply(&SessionUpdate::PermissionResolved {
            request_id: req.id,
            outcome: PermissionOutcome::Cancelled,
            by_rule: None,
            remembered: None,
        });
        f.apply(&SessionUpdate::TurnEnded {
            turn: 1,
            stop_reason: StopReason::EndTurn,
            error: None,
        });
        assert_eq!((f.turns, f.open_turn, f.pending.len()), (1, None, 0));
    }
}
