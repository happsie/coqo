# 16. Anchored questions: the question store, the anchors service and the blocking tools

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #73. Builds on ADR 0012 (bridge), 0013 (anchors) and 0014 (where tools go). Amended by #229 (threads the user starts, below).

## Context

PLAN §7.5 lets an agent ask the user a question pinned to code and get the answer as its tool result, and add non-blocking notes the same way (`ask_question`, `await_answer`, `annotate`, PLAN §7.4). Questions must survive restarts and queue while no UI is open, their anchors must stay on the code through edits (ADR 0013 left "where anchors live, when they are rebased and how they reach the UI" to this issue), a waiting agent must not time out and must be able to cancel, and the session's state must show that it waits for the user. #74 builds the editor popover, gutter markers and questions panel on what this exposes.

## Decision

**One store for questions and notes, in `coqo.db`** (global migration 5, table `questions`). A note is a question of kind `annotation`. Questions belong to an agent session and feed its transcript and alerts, which live in `coqo.db` too, and one table gives one change sequence across workspaces for `questions/changes`. Each row keeps its anchors as JSON: the `coqo-anchors` anchor **as asked** (the snippet an orphan shows) and `current`, the anchor last rebased where it moved. *Alternative:* the per-workspace database PLAN §8 lists for annotations; it would split the change sequence per workspace and keep questions away from the sessions they belong to. PLAN §8 is updated.

**Kinds** (PLAN §7.5): `decision` (2 to 16 options the agent names; the daemon numbers them `1`, `2`, …; a free-text reply is allowed too), `clarification` (free text), `confirmation` (options `yes` / `no`, optional comment), `info` (option `ok`), and `annotation` (a note with severity `info` or `warning`; dismissed, never answered). An answer is `{ option?, text? }`, checked against the kind. **Status** is `open`, `answered` or `dismissed`. Whether the code is still there is the anchor's status, not the question's: an orphaned question stays open and answerable, and shows its original snippet.

**The anchors service** lives in the question service (`coqod::questions`), on `coqo-anchors`:

- *Making.* A question pins whole lines (`Anchor::lines`) of each file as it is now: the open buffer with its unsaved edits (and its revision), else the file on disk. One read and one tree-sitter parse per file.
- *Resolving.* Every time a question is served (`list`, `get`, an event) its anchors are resolved against the files as they are now, again one read and parse per file. An anchor made or rebased **in the open buffer, during this buffer's lifetime** is mapped exactly through the changes since its revision (`BufferService::anchor_view`). Any other (after a restart, a reopen, a disk change, or once the buffer log has lost the changes) is resolved without history, by its fingerprint, from `current` and, if that is orphaned, from the original. Tracked anchors are kept per buffer id, so a revision is never used across buffer lifetimes (ADR 0013).
- *Rebasing.* After `exact` or `moved` the anchor is rebased; a `moved` one is stored as `current`. A `fuzzy` match is never adopted, so the same anchor is resolved again next time (ADR 0013 leaves that to the caller; here no one has confirmed the guess).
- *Live.* While a file with open questions is open, the service follows its buffer's changes (`BufferService::follow_changes`, which does not keep the buffer loaded), lets a burst settle for 120 ms, re-resolves that buffer's questions and publishes those whose resolution changed. Following stops when the buffer unloads.

**Wire types** are in `coqo-protocol` (`methods::questions`), mirroring the anchor model without depending on `coqo-anchors`: `CodeAnchor` (root, path, rev, range of `{line, col, offset}` byte positions, fingerprint, symbol) and `AnchorResolution` (`status`: `exact` / `moved` / `fuzzy {confidence}` / `orphaned`, `range`, and the `buffer` and `rev` the range is in, absent for the file on disk). A `Question` has id, workspace, session, kind, severity, markdown body, options, anchors (each `{anchor, resolution}`, one or more, for "these two places disagree"), status, answer, `waiting`, timestamps and the change `seq` of its state.

**Routes.** For the UI, daemon token only: `questions/list {workspace, root?, path?, session?, includeClosed?}` → `{questions, seq}`, `questions/get`, `questions/answer {id, option?, text?}`, `questions/dismiss`, and the topic `questions/changes {workspace?}`: each question whose state changed (asked, answered, dismissed, an anchor moved, an agent started or stopped waiting) as its latest state; resumable from `seq` like `sessions/changes` (replaying each question's newest state, so seqs skip values; `RESUME_GAP` when ahead). For the bridge, two session routes, each bound to the token's own workspace **and** session (`Access::check`): `questions/create` (questions and notes) and the topic `questions/watch {workspace, session, id}`, whose first event is the question as it is now, then each change, so a waiter never misses its answer. Another session's question is `UNKNOWN_QUESTION`. The session-token allowlist grows by these two.

**The tools are core bridge tools** (`crates/mcp/src/tools/questions.rs`), as ADR 0014 allows for blocking tools: `mcp/call` carries no progress or cancellation yet.

| Tool | Arguments (lines 1-based) | Daemon routes |
|---|---|---|
| `ask_question` | `path`, `root?`, `line` or `start_line`+`end_line`, `question`, `kind?`, `options?`, `wait?` (default true), `timeout_s?` (default 600, at most 3600) | `questions/create`, `questions/watch` |
| `await_answer` | `id`, `timeout_s?` | `questions/watch` |
| `annotate` | `path`, `root?`, `line` or `start_line`+`end_line`, `note`, `severity?` | `questions/create` |

*Waiting* is a `questions/watch` subscription, not a long poll: the call sends MCP progress every 10 s while it waits, returns the answer (or the dismissal) when it comes, and when its time runs out returns that the question is still open, for `await_answer`. A cancelled call (`notifications/cancelled`) drops the subscription, which unsubscribes, so the daemon stops the wait at once; the question stays open. While a watch is followed the question is `waiting`, which the UI and alerts can show ("Claude is waiting"). *Alternative:* a bounded long-poll method; its daemon side would run on after a cancel until the poll's bound.

**Sessions.** While a session has an open question (not a note) its state is `needs_input`, with the reason "Waiting for your answer: <first line>", which the rail and chat badges already show; when its last open question is answered or dismissed it goes back to `working` while a turn runs, else `done`. A waiting permission request is not overridden. Every question and note is also recorded in the session's transcript as a new `SessionUpdate::Question` when asked and again when answered or dismissed, so the chat can show the answer linked to its code (#74 renders it). The session manager gained `record_question` for this; the state goes through `SessionManager::report_state` and the state machine of ADR 0015, which keeps `needs_input` across the end of a turn. Only a session left in `needs_input` is moved on when its last question closes, so an error or `blocked` stays.

**Threads the user starts (#229).** The user can ask too: select lines, ask a chat session about them, and keep the conversation pinned there. This reuses the store rather than adding one, because such a thread needs everything a question has (anchors that follow edits and restarts, the markers, the popover, the pane, the transcript card). A question gains an `author`, `agent` (the default, left off the wire) or `user`, and a `thread` of the user's messages, each `{text, turn, at}`: the session turn it prompted. Global migration 11 adds the columns `author` (default `'agent'`) and `thread` (JSON). Two UI routes (daemon token only):

- `questions/ask {workspace, session, body, anchors}` pins a `clarification` with `author: user` and prompts the session (`SessionManager::prompt`) with the message, where it is pinned and the thread id, and one `resource` block per anchor (`file://…#Lstart-Lend` with the anchored text), asking it to answer in the chat and, if useful, `annotate` the lines. If the prompt fails (`SESSION_BUSY`, a closed session, …) the thread is dismissed and the error returned, so nothing is left pinned.
- `questions/reply {id, body}` prompts the same session with a follow-up ("Follow-up on thread #id about …", without the code again) and appends it to the thread. Replying to an agent's question is `INVALID_PARAMS`; to a closed thread `QUESTION_CLOSED`.
- Both prompts' text is a `ThreadPrompt` (`coqo-protocol`), so a chat can parse it back and show the message in short (#248).

The agent's replies are not stored on the question: they are the agent messages of each message's `turn` in the session's updates, which are persisted, so the popover streams them from `sessions/updates` and they survive a restart. A user's thread never makes the session `needs_input`, never shows as `waiting`, is not counted by `open_count` or the alerts, and cannot be answered (`questions/answer` is `INVALID_PARAMS`); closing it is `questions/dismiss` ("Resolve"). It is recorded in the transcript like a question, once with its first message and again as it grows or closes.

## Consequences

- Agents can ask questions and leave notes whether or not a UI is open; they queue in `coqo.db`, and a UI lists them when it attaches.
- A question asked with `wait: false` keeps the session `needs_input` while the agent goes on; the next prompt sets `working` again even if the question is still open. `set_status` (#75) may refine how the two combine.
- Resolving runs on every serve. It is one parse per file per list, and while typing at most one per 120 ms per file with open questions; ADR 0013's numbers make the resolving itself negligible. Rebased anchors written from unsaved text that is later discarded fall back to the original anchor.
- The answer reaches the agent as its tool result. The transcript entry and its rendering in the chat, the popover, the gutter markers and the questions panel are #74. OS notifications and alerts for unanswered blocking questions are follow-ups (`waiting` is the hook).
- The protocol grows `methods::questions`, two error codes (`UNKNOWN_QUESTION` 1501, `QUESTION_CLOSED` 1502) and the `question` transcript update.
