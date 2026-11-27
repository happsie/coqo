# 15. Agent state and the agent's status (`set_status`)

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #75 (builds on ADR 0012 and 0014)

## Context

PLAN §7.9 lists the agent states (`idle · working · needs_input · needs_permission · blocked · error · done`) and wants them shown the same in the rail, the tabs and the agents overview, and fed to alerts (#76, #79). The session manager (`coqo-agents`, #53) already set states, but the transitions were spread over the manager, and the agent had no way to say what it was doing. PLAN §7.4 names a `set_status` tool for that. Open points:

1. One explicit state machine: which event moves which state where.
2. What `set_status` takes, how its words relate to the state, and how long they last.
3. Where the tool runs: ADR 0014 said a feature that is a mod in the daemon registers its tools through `register_mcp_tool`, and listed status among them.
4. How the status reaches clients without breaking older ones.

## Decision

**One pure state machine** (`crates/agents/src/state.rs`): `next(current, event)` takes the state, its reason and the agent's status, and an event (turn started, permission asked, permissions settled, cancel requested, a reported state, a status set, turn ended, provider failed, closed), and returns the new triple. The manager turns everything that happens into an event and applies it in one place, which records a `state` transcript update when the state or its reason changes and saves the session info (a `sessions/changes` event) when anything changes. The module's table is the reference; the main rules:

- A turn start is `working`; a normal turn end is `done`, unless the agent waits (`needs_input`, `blocked`), which stays; a cancel is `idle`; a failed or interrupted turn and a provider that fails to start are `error` with the reason.
- `needs_permission` is only the manager's. While a permission request waits, nothing the agent reports or sets changes the state except `error`: the turn is stuck on the user's answer. Once every request is settled (or the turn is being cancelled) it is `working` again.
- A provider's or a daemon feature's report of `idle` or `done` during a turn means `working` (the turn's end decides). `needs_input`, `blocked` and `error` apply with their reason.

`SessionInfo` gains `state_since` (when the state last changed, for "waiting 18m") and `status`.

**The status** is `AgentStatus { text, state?, progress?, turn?, updatedAt }`: one line (whitespace collapsed, cut at 120 characters with `…`), the state the agent claimed (`working`, `blocked` or `done`), optional `{current, total}` progress (`total` ≥ 1, `current` capped at it), the turn it was set in and when. Through the state machine:

- No state: only the text changes.
- `working`: during a turn, clears an earlier `blocked` or `needs_input` (back to `working`); between turns it changes nothing (an agent outside a turn is not working, and must not hold the daemon's busy guard).
- `blocked`: the session is `blocked` with the status as its reason. This is the agent's "I need you" that is not a question.
- `done`: during a turn, `working` (the turn's end makes it `done`); between turns, `done`. The status is then the outcome.
- A turn start clears the status. A turn end clears it unless the agent claimed `blocked` or `done`, which stays as the outcome until the next turn; a cancel or failure clears it.
- Setting the same status again changes nothing and sends nothing.

`needs_input` is not something `set_status` claims: questions (#73) report it while one waits, through `SessionManager::report_state`, so that an answer clears it.

**`set_status` is a core bridge tool**, over a new method `sessions/setStatus { session, status, state?, progress? }` that returns the session's info. Session tokens may call it for their own session only (`Access::session`), so it joins the session-token allowlist with a cross-session probe. Not a mod tool, as ADR 0014 expected, because the state lives in the session manager, which `coqod` wires directly and which is not a mod: a mod would need a back door to the manager, while the bridge already knows its session and the route policy already binds a token to it. It answers at once, so it needs no progress.

**Status changes travel as session info**, on `sessions/changes` and in `sessions/list` and `sessions/get`, not as transcript updates: an agent may report progress often, and a new `SessionUpdate` variant would fail to parse in an older UI. A state change the status causes is still a `state` update in the transcript. Both new `SessionInfo` fields are optional and skipped when absent, so the protocol change is additive. The store keeps them in two new columns (global migration 5: `status` as JSON, `state_since`).

**UI.** The chat's session panel opens with a status card (the state as a pill in its status colour, the status line or why it waits, a progress bar); the CHATS rows show the status line under the title; the rail's tooltip lists the workspace's busy sessions with theirs. Colours stay as before: sand needs you, rose error, blue working, sage done.

## Consequences

- **For alerts (#76) and the agents overview (#79):** key off `SessionInfo.state` from `sessions/changes`. `needs_permission`, `needs_input` and `blocked` need the user (`AgentState::needs_attention` also counts `error`); a change into one of them, or into `error` or `done`, is an alert. `state_reason` says why (for `blocked` it is the agent's status line), `status` is the line to show, `state_since` how long it has been so.
- The agent can raise a `blocked` alert at will; that is the point, and it is limited to its own session.
- The status line is the agent's words, shown as text only; the UI never renders it as markdown or links.
- A UI from before this change ignores the new fields; a daemon from before it answers `sessions/setStatus` with `METHOD_NOT_FOUND`, which the tool shows the agent as an error.
- Migration 5 is append-only; a later global migration must take version 6.
