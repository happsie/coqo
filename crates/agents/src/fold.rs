// SPDX-License-Identifier: Apache-2.0

//! A session's folded state: what the transcript adds up to (plan, modes,
//! commands, usage, turns, open permission requests). Rebuilt from the
//! transcript when a session is loaded.

use std::collections::{BTreeMap, HashMap};

use coqo_protocol::methods::agents::{
    AuthStatus, AvailableCommand, ContentBlock, PermissionRequest, PermissionRequestId, PlanEntry,
    SessionConfigOption, SessionModes, SessionOrigin, SessionUpdate, ToolCallId, Usage,
    follow_mode,
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
    /// The latest config options, in the agent's order.
    pub config_options: Vec<SessionConfigOption>,
    pub commands: Vec<AvailableCommand>,
    pub usage: Option<Usage>,
    pub auth: Option<AuthStatus>,
    /// What each tool call is, for permission rules.
    pub tools: HashMap<ToolCallId, ToolFacts>,
    /// The last prompt, for `sessions/retry`.
    pub last_prompt: Option<Vec<ContentBlock>>,
    /// Why the agent's session can't be resumed, once it couldn't.
    pub ended: Option<String>,
    /// The transcript the session started from.
    pub started_from: Option<SessionOrigin>,
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
                follow_mode(&mut self.config_options, mode_id);
            }
            SessionUpdate::ConfigOptions { config_options } => {
                self.config_options.clone_from(config_options);
                if let Some(m) = &mut self.modes {
                    m.follow(config_options);
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
            SessionUpdate::UserMessage { content } => self.last_prompt = Some(content.clone()),
            SessionUpdate::Ended { reason } => self.ended = Some(reason.clone()),
            SessionUpdate::StartedFrom(origin) => self.started_from = Some(origin.clone()),
            SessionUpdate::UserMessageChunk { .. }
            | SessionUpdate::AgentMessageChunk { .. }
            | SessionUpdate::AgentThoughtChunk { .. }
            | SessionUpdate::State { .. }
            | SessionUpdate::Question(_)
            | SessionUpdate::Walkthrough(_)
            | SessionUpdate::Closed
            | SessionUpdate::Reopened => {}
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

    #[test]
    fn a_mode_config_option_and_the_modes_stay_in_step() {
        use coqo_protocol::methods::agents::{ConfigValue, config_category};

        let mut options = crate::testing::test_config_options();
        let mut mode = options[0].clone();
        mode.id = "mode".into();
        mode.category = Some(config_category::MODE.into());
        mode.kind = coqo_protocol::methods::agents::ConfigOptionKind::Select {
            current_value: "default".into(),
            options: ["default", "plan"]
                .map(|v| coqo_protocol::methods::agents::ConfigOptionValue {
                    value: v.into(),
                    name: v.into(),
                    description: None,
                    group: None,
                })
                .to_vec(),
        };
        options.push(mode);
        let mut f = Fold::default();
        f.apply(&SessionUpdate::Modes(crate::testing::test_modes()));
        f.apply(&SessionUpdate::ConfigOptions {
            config_options: options.clone(),
        });
        assert_eq!(f.config_options.len(), 4);
        assert_eq!(f.mode(), Some("default"));

        // The option changes: the modes follow.
        options[3].set_value(&ConfigValue::Value("plan".into()));
        f.apply(&SessionUpdate::ConfigOptions {
            config_options: options,
        });
        assert_eq!(f.mode(), Some("plan"));
        // The mode changes: the option follows.
        f.apply(&SessionUpdate::CurrentMode {
            mode_id: "default".into(),
        });
        assert_eq!(f.config_options[3].value_name(), "default");
        assert_eq!(f.mode(), Some("default"));
    }
}
