# 17. Acceptance criteria per session (`set_acceptance_criteria`)

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #96 (builds on ADR 0014 and 0015)

## Context

PLAN §7.4 names a `set_acceptance_criteria` tool: the agent states what "done" means for its task. Coqo should store the criteria and show them in the chat's session panel (board 1), and later features refer to them: a walkthrough step's `criteria_refs[]` (§7.6, #97) and a trace scenario's `criterion` (§7.10). Open points:

1. The model: what a criterion is, and how later features name one.
2. One tool or two: registering the list, then updating one criterion as it is verified.
3. Where the criteria live, and how they reach clients.
4. Whether the user can change them, and how the agent finds out.

## Decision

**The model** (`coqo_protocol::methods::agents`): `AcceptanceCriterion { id, text, status, evidence[], changedBy, updatedAt }`.

- `id` is stable within the session: the agent's own (ASCII letters, digits, `.`, `_`, `-`, at most 32) or, by default, the next free `AC<n>` (`AC1`, `AC2`, …, as the board shows). **Walkthroughs and trace scenarios refer to criteria by this id**; the session is the scope.
- `status` is `pending` (not checked yet, the default), `met`, `failed` or `unknown` (cannot be checked here).
- `evidence` is up to five `{ text, path?, line? }`: what shows it ("test `caps_at_500` passes") and optionally a file and a 1-based line. A relative path is taken from the session's `cwd` and stored absolute.
- `changedBy` is `agent` or `user`: who last changed the criterion's text, status or evidence. `updatedAt` moves with it. A call that changes nothing changes neither.
- Text is one line (whitespace collapsed), at most 300 characters (cut with `…`); evidence text at most 200; at most 50 criteria per session.

**One tool, `set_acceptance_criteria { criteria: [{ id?, text?, status?, evidence? }], replace? }`**, rather than a second `update_criterion`. An entry whose `id` names a criterion changes only the fields it gives (`evidence` replaces that criterion's evidence; `[]` clears it). Any other entry adds a criterion and needs `text`. Without `replace`, changed criteria stay in place and new ones go at the end; with `replace: true` the list is exactly the entries, in order (named ones keep what the entry does not change). One tool keeps the agent's tool list short, and "register" and "update" are the same call shape: the description tells the agent to call it at the start of a task with the whole list, then again with `{id, status, evidence}` as it verifies each one. The result lists every criterion, so the agent always sees the ids and the current state.

**A core bridge tool over `sessions/setCriteria`**, as `set_status` is (ADR 0015): the criteria live on the session in the session manager, which `coqod` wires directly and which is not a mod. The route is bound with `Access::session(|p| &p.session.0)`: a session token reaches its own session only (`FORBIDDEN` otherwise), the daemon token any session. The route sets `changedBy` from the caller's scope: a session token's changes are the agent's, the daemon token's are the user's. The tool exists only because no harness offers it: it is Coqo's own feature.

**Stored on the session, streamed as session info.** `SessionInfo.criteria` (skipped when empty, so the protocol change is additive) is saved with the session in a new JSON column (`agent_sessions.criteria`, global migration 7) and goes out on `sessions/changes`, `sessions/list` and `sessions/get`, like the status. The criteria are not part of the state machine: turns do not clear them; they last as long as the session and survive a daemon restart. The rules live in `crates/agents/src/criteria.rs` (`apply`, pure and unit-tested).

**The user can mark a criterion** by clicking its status glyph in the session panel (pending → met → failed → pending), which sends `sessions/setCriteria` with the daemon token. The agent is not interrupted: it sees the change, marked "set by the user", in the result of its next `set_acceptance_criteria` call (an empty `criteria: []` call just reads the list). Editing a criterion's text in the UI is left for later.

**UI** (board 1): an "Acceptance criteria" card in the session panel between Plan and Changes, with "n of m met" in its header; each row has the status glyph (sage met, rose failed, sand pending, subtle unknown), the id in semibold blue and the text; evidence under it, and evidence with a file opens that file at the line (`chat::OpenLocation`, which now reveals its line).

## Consequences

- **For walkthroughs (#97) and live trace (§7.10):** name criteria by id (`criteria_refs: ["AC1"]`, `scenario_begin({criterion: "AC2"})`) and look them up in the session's `SessionInfo.criteria`. A criterion dropped by `replace` leaves such a reference dangling; show it as unknown rather than failing. `trace_query` evidence can be attached with the same tool.
- A UI from before this change ignores `criteria`; a daemon from before it answers `sessions/setCriteria` with `METHOD_NOT_FOUND`, which the tool shows the agent as an error.
- `OpenLocation.line` is 0-based (as ACP tool-call locations are) and now selects that line; criterion evidence lines are 1-based on the wire and converted when opened.
- Migration 7 is append-only; a later global migration must take the next version.
