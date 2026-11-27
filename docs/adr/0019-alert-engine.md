# 19. Alert engine: one alert per source and key, auto-resolved, in the daemon

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #76 (builds on ADR 0007, 0015 and 0016)

## Context

PLAN §7.9 wants an alert center that tells you which agent needs you, across workspaces, with badges, do-not-disturb and per-source settings; the alert sources are a contribution point (PLAN §5). ADR 0015 says alerts key off `SessionInfo.state` (a change into `needs_permission`, `needs_input`, `blocked`, `error` or `done`), and ADR 0016 that a waiting question is the hook for "answer me" alerts. Desktop notifications (#77) and the agents overview (#79) build on what this exposes. Open points:

1. Where alerts are made and kept, so they exist while no UI is open.
2. How not to drown the user: one agent session changes state many times.
3. When an alert goes away by itself.
4. What opening an alert does (deep links).
5. How mods raise alerts.

## Decision

**The engine lives in the daemon** (`coqod::alerts`), with its rows in `coqo.db` (global migration 8: `alerts` and `alert_settings`). Alerts raised while the UI is closed are there when it attaches, and the daemon can send desktop notifications from the same place (#77).

**One alert per source and key.** A source raises under a key of its own; raising again replaces the alert, so a session that waits, works and waits again has one row, not a stream. A new *kind* or *target* is a new occurrence: unread, undismissed and back on top (`createdAt` reset). The same kind and target only updates the text and keeps the read and dismissed state; nothing at all changes if nothing did (no change seq is spent). **Resolving** sets `resolvedAt` and counts as read; the alert stays listed as history until dismissed. Dismissed alerts are deleted after 30 days.

**Core sources: agent sessions.** `AgentSource` follows `sessions/changes` and keeps at most one standing alert per session, keyed by the session id, under one source per state so each can be muted:

| State | Source | Kind | Target |
|---|---|---|---|
| `needs_permission` | `agents.permission` | `needs_you` | the chat (title: the first waiting request) |
| `needs_input` | `agents.question` | `needs_you` | the first open question, with its first code location (else the chat) |
| `blocked` | `agents.blocked` | `needs_you` | the chat |
| `error` | `agents.error` | `error` | the chat |
| `done` | `agents.done` | `done` | the chat |

Moving to another state resolves the old source's alert; `working`, `idle` and closing resolve them all. Questions are not a separate stream: a waiting question is what makes the session `needs_input` (ADR 0016), so the engine looks the question up when the session changes (`QuestionService::open_questions_of`, cheap, no anchor resolving) and the alert points at it. The next question is a new target, so it is news again. The source keeps the last `sessions/changes` seq it handled (saved when an alert changed, else every 5 s), so after a restart it replays only the sessions that changed meanwhile (a turn the restart interrupted becomes an error alert), and on the very first start it raises nothing old.

**`done` always alerts, at low priority.** The daemon does not know which chat is on screen. Instead the UI marks a session's alerts read while its chat tab is shown in a focused window (`alerts/markRead {session}`), so a turn you watched finish never counts as unread, and the bell ranks `done` last. Only `needs_you` alerts pop up as toasts.

**Protocol** (`coqo_protocol::methods::alerts`, daemon token only, per ADR 0007): `alerts/list {workspace?}` → `{alerts, seq}` (not dismissed, newest first, at most 200), `alerts/markRead {ids, session?}`, `alerts/dismiss {ids}`, `alerts/dismissAll {workspace?}`, `alerts/settings`, `alerts/setSettings {doNotDisturb?, mute, unmute}`, and the topic `alerts/changes {workspace?}`: each changed alert as its latest state, resumable from `seq` like `questions/changes` (a resume replays each alert's newest state; `RESUME_GAP` when ahead). Agents need none of it: their permission requests, questions and status already raise alerts.

**Deep links** are an `AlertTarget`: `session` (open the chat tab), `question` (session, question id, first code location as asked), `file` (root, path, line), always in the alert's `workspace`. The UI switches the window to that workspace first (or focuses the window that shows it). Until the question popover (#74) has an API to open, a question opens its code.

**Settings.** Do-not-disturb is stored in the daemon so desktop notifications can honour it; alerts are still recorded, only nothing pops up. A muted source raises nothing. `alerts/settings` lists the core sources and every mod's `alert_sources` contribution.

**Mods** raise alerts through the service `coqo.alerts@1` (`coqo_mod_api::alerts::Alerts`: `raise(RaiseAlert)` and `resolve(source, key)`), which the daemon provides in the service registry as `coqo.alerts`. The source must be contributed to `alert_sources` and may not be a core one. *Alternative:* a contribution whose value is a source object the engine polls; it would make simple "raise once" sources (CI failed) harder and give the engine a scheduler.

## Consequences

- **Desktop notifications (#77):** follow `alerts/changes` in the daemon (or subscribe to the engine's channel) and notify for a new occurrence (`createdAt` changed, unread, `needs_you`), unless do-not-disturb; the click opens the UI at the alert's target.
- **Agents overview (#79):** sessions come from `sessions/*`; the alert of a session (`key` = session id, `agents.*` sources) gives the "open" action and whether it is unread.
- The engine depends on `SessionManager` and `QuestionService`; a session state the engine does not know about yet (a new `AgentState`) needs a row in `session_alert`.
- The UI's alert list is the 200 most recent; more need paging later.
