// SPDX-License-Identifier: Apache-2.0

//! The agent state machine (PLAN §7.9), as one pure function.
//!
//! A session's [`AgentState`] is derived from what happens to it, never set
//! directly: the manager turns each provider or user event into an
//! [`Event`] and stores what [`next`] returns. The agent's own status line
//! ([`AgentStatus`], `set_status`) rides along and follows the same events.
//!
//! | Event | From | To | Status |
//! |---|---|---|---|
//! | turn started | any | `working` | cleared |
//! | permission asked (no rule answered it) | any | `needs_permission` | kept |
//! | questions asked (the agent's own question tool, #339) | any | `needs_input`, "Waiting for your answer: …" | kept |
//! | permissions settled (answered or cancelled) | `needs_permission` / `needs_input` | `working` | kept |
//! | cancel requested | `needs_permission` | `working` | kept |
//! | reported `working` | any | `working` | kept |
//! | reported `idle` / `done` during a turn | any | `working` | kept |
//! | reported `idle` / `done` between turns | any | `idle` / `done` | kept |
//! | reported `needs_input`, `blocked`, `error` | any | that, with the reason | kept |
//! | status set, no state | any | unchanged | replaced |
//! | status `working` / `done` | during a turn | `working` | replaced |
//! | status `working` | between turns | unchanged | replaced |
//! | status `done` | between turns | `done` | replaced |
//! | status `blocked` | any | `blocked`, the status as reason | replaced |
//! | turn ended normally | `needs_input` / `blocked` | unchanged | kept if `blocked`/`done` |
//! | turn ended normally | otherwise | `done` | kept if `blocked`/`done` |
//! | turn cancelled | any | `idle` | cleared |
//! | turn failed or interrupted, provider failed | any | `error` | cleared |
//! | closed | `working` / `needs_permission` | `idle` | kept |
//!
//! While a permission request waits (`needs_permission`), reports and
//! statuses do not change the state (only `error` does): the agent is
//! stuck on the user's answer whatever it says, and the state goes back to
//! `working` once the answer is in. `needs_permission` is only ever the
//! manager's; a report of it is ignored.

use coqo_protocol::methods::agents::{
    AgentState, AgentStatus, MAX_STATUS_CHARS, StatusState, StopReason,
};

/// A session's state, its reason and the agent's status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tracked {
    pub state: AgentState,
    pub reason: Option<String>,
    pub status: Option<AgentStatus>,
}

/// Something that happened to a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A turn started.
    TurnStarted,
    /// A permission request waits for the user; with `question`, the
    /// agent's own questions do (the reason to show).
    PermissionAsked { question: Option<String> },
    /// No permission request waits any more (all answered or cancelled).
    PermissionsSettled,
    /// The user asked to cancel the turn.
    CancelRequested,
    /// A provider or daemon feature reported a state.
    Reported {
        state: AgentState,
        reason: Option<String>,
        running: bool,
    },
    /// The agent set its status (`set_status`).
    StatusSet { status: AgentStatus, running: bool },
    /// The turn ended.
    TurnEnded {
        stop: StopReason,
        error: Option<String>,
    },
    /// The provider failed outside a turn (start, resume), or the daemon
    /// lost the turn.
    Failed(String),
    /// The session was closed.
    Closed,
}

/// The state after `event`.
pub(crate) fn next(current: &Tracked, event: Event) -> Tracked {
    use AgentState as S;
    let waiting = current.state == S::NeedsPermission;
    let same = |status: Option<AgentStatus>| Tracked {
        state: current.state,
        reason: current.reason.clone(),
        status,
    };
    let to = |state: AgentState, reason: Option<String>, status: Option<AgentStatus>| Tracked {
        state,
        reason,
        status,
    };
    let kept = || current.status.clone();
    match event {
        Event::TurnStarted => to(S::Working, None, None),
        Event::PermissionAsked { question: None } => to(S::NeedsPermission, None, kept()),
        Event::PermissionAsked {
            question: Some(reason),
        } => to(S::NeedsInput, Some(reason), kept()),
        Event::PermissionsSettled | Event::CancelRequested if waiting => {
            to(S::Working, None, kept())
        }
        // Only sent when a request was pending, so `needs_input` was its.
        Event::PermissionsSettled if current.state == S::NeedsInput => to(S::Working, None, kept()),
        Event::PermissionsSettled | Event::CancelRequested => same(kept()),

        Event::Reported {
            state: S::NeedsPermission,
            ..
        } => same(kept()),
        Event::Reported {
            state: S::Error,
            reason,
            ..
        } => to(S::Error, reason, kept()),
        Event::Reported { .. } if waiting => same(kept()),
        Event::Reported {
            state: S::Idle | S::Done,
            running: true,
            ..
        } => to(S::Working, None, kept()),
        Event::Reported {
            state: state @ (S::Idle | S::Done | S::Working),
            ..
        } => to(state, None, kept()),
        Event::Reported { state, reason, .. } => to(state, reason, kept()),

        Event::StatusSet { status, .. } if waiting => same(Some(status)),
        Event::StatusSet { status, running } => match status.state {
            None => same(Some(status)),
            Some(StatusState::Blocked) => to(S::Blocked, Some(status.text.clone()), Some(status)),
            Some(StatusState::Working | StatusState::Done) if running => {
                to(S::Working, None, Some(status))
            }
            Some(StatusState::Done) => to(S::Done, None, Some(status)),
            Some(StatusState::Working) => same(Some(status)),
        },

        Event::TurnEnded { stop, error } => {
            let outcome = current
                .status
                .clone()
                .filter(|s| matches!(s.state, Some(StatusState::Blocked | StatusState::Done)));
            match stop {
                StopReason::Cancelled => to(S::Idle, None, None),
                StopReason::Error | StopReason::Interrupted => to(S::Error, error, None),
                StopReason::EndTurn
                | StopReason::MaxTokens
                | StopReason::MaxTurnRequests
                | StopReason::Refusal => match current.state {
                    // The agent said it waits for the user; keep that.
                    S::NeedsInput | S::Blocked => same(outcome),
                    _ => to(S::Done, None, outcome),
                },
            }
        }
        Event::Failed(reason) => to(S::Error, Some(reason), None),
        Event::Closed if matches!(current.state, S::Working | S::NeedsPermission) => {
            to(S::Idle, None, kept())
        }
        Event::Closed => same(kept()),
    }
}

/// `text` as one status line: whitespace collapsed to single spaces, cut
/// at [`MAX_STATUS_CHARS`] with `…`. Empty if it has no visible text.
pub(crate) fn status_line(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let line = words.join(" ");
    if line.chars().count() <= MAX_STATUS_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(MAX_STATUS_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::agents::StatusProgress;

    use super::*;
    use AgentState as S;

    fn at(state: AgentState) -> Tracked {
        Tracked {
            state,
            reason: None,
            status: None,
        }
    }

    fn status(text: &str, state: Option<StatusState>) -> AgentStatus {
        AgentStatus {
            text: text.into(),
            state,
            progress: None,
            turn: Some(1),
            updated_at: 7,
        }
    }

    fn with_status(state: AgentState, s: AgentStatus) -> Tracked {
        Tracked {
            status: Some(s),
            ..at(state)
        }
    }

    fn reported(state: AgentState, running: bool) -> Event {
        Event::Reported {
            state,
            reason: Some("why".into()),
            running,
        }
    }

    fn set(s: AgentStatus, running: bool) -> Event {
        Event::StatusSet { status: s, running }
    }

    fn ended(stop: StopReason) -> Event {
        Event::TurnEnded {
            stop,
            error: Some("boom".into()),
        }
    }

    const ALL: [AgentState; 7] = [
        S::Idle,
        S::Working,
        S::NeedsInput,
        S::NeedsPermission,
        S::Blocked,
        S::Error,
        S::Done,
    ];

    #[test]
    fn a_turn_start_works_and_clears_the_status() {
        for from in ALL {
            let before = Tracked {
                reason: Some("old".into()),
                ..with_status(from, status("old", Some(StatusState::Done)))
            };
            assert_eq!(
                next(&before, Event::TurnStarted),
                at(S::Working),
                "{from:?}"
            );
        }
    }

    #[test]
    fn permissions_wait_and_settle() {
        let s = status("Editing", None);
        let asked = next(
            &with_status(S::Working, s.clone()),
            Event::PermissionAsked { question: None },
        );
        assert_eq!(asked, with_status(S::NeedsPermission, s.clone()));
        assert_eq!(
            next(&asked, Event::PermissionsSettled),
            with_status(S::Working, s.clone())
        );
        assert_eq!(
            next(&asked, Event::CancelRequested),
            with_status(S::Working, s)
        );
        // Settling outside a wait changes nothing.
        for from in [S::Working, S::Blocked, S::Done, S::Idle] {
            assert_eq!(next(&at(from), Event::PermissionsSettled), at(from));
            assert_eq!(next(&at(from), Event::CancelRequested), at(from));
        }
        assert_eq!(
            next(&at(S::NeedsInput), Event::CancelRequested),
            at(S::NeedsInput)
        );
    }

    #[test]
    fn questions_wait_for_input_and_settle() {
        let s = status("Editing", None);
        let asked = next(
            &with_status(S::Working, s.clone()),
            Event::PermissionAsked {
                question: Some("Waiting for your answer: Which colour?".into()),
            },
        );
        assert_eq!(
            asked,
            Tracked {
                reason: Some("Waiting for your answer: Which colour?".into()),
                ..with_status(S::NeedsInput, s.clone())
            }
        );
        assert_eq!(
            next(&asked, Event::PermissionsSettled),
            with_status(S::Working, s)
        );
    }

    #[test]
    fn reports_set_the_state() {
        for running in [false, true] {
            for from in [S::Idle, S::Working, S::NeedsInput, S::Blocked, S::Done] {
                let cx = format!("{from:?} running={running}");
                let r = |state| next(&at(from), reported(state, running));
                let with_reason = |state| Tracked {
                    reason: Some("why".into()),
                    ..at(state)
                };
                assert_eq!(r(S::Working), at(S::Working), "{cx}");
                assert_eq!(r(S::NeedsInput), with_reason(S::NeedsInput), "{cx}");
                assert_eq!(r(S::Blocked), with_reason(S::Blocked), "{cx}");
                assert_eq!(r(S::Error), with_reason(S::Error), "{cx}");
                // Only the manager asks for permission.
                assert_eq!(r(S::NeedsPermission), at(from), "{cx}");
                // Idle and done mean nothing while a turn runs.
                let (idle, done) = if running {
                    (at(S::Working), at(S::Working))
                } else {
                    (at(S::Idle), at(S::Done))
                };
                assert_eq!(r(S::Idle), idle, "{cx}");
                assert_eq!(r(S::Done), done, "{cx}");
            }
        }
    }

    #[test]
    fn a_waiting_permission_holds_the_state() {
        let waiting = at(S::NeedsPermission);
        for state in [S::Idle, S::Working, S::NeedsInput, S::Blocked, S::Done] {
            assert_eq!(next(&waiting, reported(state, true)), waiting, "{state:?}");
        }
        assert_eq!(
            next(&waiting, reported(S::Error, true)),
            Tracked {
                reason: Some("why".into()),
                ..at(S::Error)
            }
        );
        let blocked = status("Need a token", Some(StatusState::Blocked));
        assert_eq!(
            next(&waiting, set(blocked.clone(), true)),
            with_status(S::NeedsPermission, blocked)
        );
    }

    #[test]
    fn a_status_overlays_the_state() {
        let plain = status("Running the tests", None);
        let working = status("Retrying", Some(StatusState::Working));
        let blocked = status("Need the API key", Some(StatusState::Blocked));
        let done = status("Tests pass", Some(StatusState::Done));
        let blocked_state = |s: AgentStatus| Tracked {
            reason: Some("Need the API key".into()),
            ..with_status(S::Blocked, s)
        };
        for from in [
            S::Idle,
            S::Working,
            S::NeedsInput,
            S::Blocked,
            S::Done,
            S::Error,
        ] {
            let before = Tracked {
                reason: Some("r".into()),
                ..at(from)
            };
            let unchanged = |s: AgentStatus| Tracked {
                status: Some(s),
                ..before.clone()
            };
            // No state: only the text changes.
            assert_eq!(
                next(&before, set(plain.clone(), true)),
                unchanged(plain.clone())
            );
            assert_eq!(
                next(&before, set(plain.clone(), false)),
                unchanged(plain.clone())
            );
            // During a turn, working and done mean working.
            assert_eq!(
                next(&before, set(working.clone(), true)),
                with_status(S::Working, working.clone())
            );
            assert_eq!(
                next(&before, set(done.clone(), true)),
                with_status(S::Working, done.clone())
            );
            // Between turns, working changes nothing and done is done.
            assert_eq!(
                next(&before, set(working.clone(), false)),
                unchanged(working.clone())
            );
            assert_eq!(
                next(&before, set(done.clone(), false)),
                with_status(S::Done, done.clone())
            );
            // Blocked, with the status as the reason.
            for running in [false, true] {
                assert_eq!(
                    next(&before, set(blocked.clone(), running)),
                    blocked_state(blocked.clone())
                );
            }
        }
    }

    #[test]
    fn a_normal_turn_end_is_done_unless_the_agent_waits() {
        let normal = [
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::MaxTurnRequests,
            StopReason::Refusal,
        ];
        for stop in normal {
            for from in [S::Working, S::Idle, S::Done, S::Error] {
                assert_eq!(
                    next(&at(from), ended(stop)),
                    at(S::Done),
                    "{stop:?} {from:?}"
                );
            }
            for from in [S::NeedsInput, S::Blocked] {
                let before = Tracked {
                    reason: Some("q".into()),
                    ..at(from)
                };
                assert_eq!(next(&before, ended(stop)), before, "{stop:?} {from:?}");
            }
        }
    }

    #[test]
    fn a_turn_end_keeps_only_an_outcome() {
        let end = ended(StopReason::EndTurn);
        let mut progress = status("Step 3", None);
        progress.progress = Some(StatusProgress {
            current: 3,
            total: 5,
        });
        assert_eq!(
            next(&with_status(S::Working, progress), end.clone()),
            at(S::Done)
        );
        let working = status("Retrying", Some(StatusState::Working));
        assert_eq!(
            next(&with_status(S::Working, working), end.clone()),
            at(S::Done)
        );
        let done = status("Tests pass", Some(StatusState::Done));
        assert_eq!(
            next(&with_status(S::Working, done.clone()), end.clone()),
            with_status(S::Done, done)
        );
        let blocked = status("Need a key", Some(StatusState::Blocked));
        let before = Tracked {
            reason: Some("Need a key".into()),
            ..with_status(S::Blocked, blocked)
        };
        assert_eq!(next(&before, end), before);
    }

    #[test]
    fn cancels_and_failures_clear_everything() {
        let done = status("Tests pass", Some(StatusState::Done));
        for from in ALL {
            let before = with_status(from, done.clone());
            assert_eq!(next(&before, ended(StopReason::Cancelled)), at(S::Idle));
            let error = Tracked {
                reason: Some("boom".into()),
                ..at(S::Error)
            };
            assert_eq!(next(&before, ended(StopReason::Error)), error);
            assert_eq!(next(&before, ended(StopReason::Interrupted)), error);
            assert_eq!(next(&before, Event::Failed("boom".into())), error);
        }
    }

    #[test]
    fn closing_stops_work() {
        let s = status("Editing", None);
        for from in [S::Working, S::NeedsPermission] {
            assert_eq!(
                next(&with_status(from, s.clone()), Event::Closed),
                with_status(S::Idle, s.clone())
            );
        }
        for from in [S::Idle, S::NeedsInput, S::Blocked, S::Error, S::Done] {
            let before = Tracked {
                reason: Some("r".into()),
                ..with_status(from, s.clone())
            };
            assert_eq!(next(&before, Event::Closed), before);
        }
    }

    #[test]
    fn status_lines_are_one_short_line() {
        assert_eq!(
            status_line("  Running\n the   tests\t"),
            "Running the tests"
        );
        assert_eq!(status_line(" \n\t"), "");
        let long = "word ".repeat(40);
        let line = status_line(&long);
        assert_eq!(line.chars().count(), MAX_STATUS_CHARS);
        assert!(line.ends_with("word…"), "{line}");
        // A cut right after a space drops the space.
        let spaced = format!("{} tail", "x".repeat(MAX_STATUS_CHARS - 2));
        assert_eq!(
            status_line(&spaced),
            format!("{}…", "x".repeat(MAX_STATUS_CHARS - 2))
        );
        let exact = "x".repeat(MAX_STATUS_CHARS);
        assert_eq!(status_line(&exact), exact);
        let wide = "é".repeat(MAX_STATUS_CHARS + 1);
        assert_eq!(status_line(&wide).chars().count(), MAX_STATUS_CHARS);
    }
}
