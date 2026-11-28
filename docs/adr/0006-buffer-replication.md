# 6. Buffer replication: daemon-authoritative op-log with optimistic local apply

- **Status:** Accepted
- **Date:** 2026-11-27
- **Spike:** S6, issue #13. Prototype in [`docs/spikes/s6-buffers/`](../spikes/s6-buffers/).

## Context

PLAN §4.2 puts the authoritative buffers in the daemon (`coqod`) and local replicas in the UI. Agents (through MCP tools, ACP `fs/write_text_file`, or, for Claude Code, by writing files to disk themselves; see [ADR 0005](0005-claude-integration.md)) and the user edit the same open buffer, possibly from two windows. Edits from every side must converge without losing characters, typing must feel instant (< 1 ms local apply on a 100k-line file, #40), undo must be per side (the user's Ctrl+Z never reverts the agent), anchors (questions, walkthroughs, #72) must follow edits, and unsaved edits must survive a UI restart.

Spike S6 compares two models:

- **A · Op-log:** the daemon owns the text and a revision counter. Clients apply their own edits optimistically, send ranged edits tagged with the revision they were made against, and transform incoming changes against what they still have in flight. This is the ot.js / Google Wave client-server scheme.
- **B · CRDT:** a `yrs` (Yjs port, MIT) text document on every side. Binary updates are relayed through the daemon.

## What we tested

`docs/spikes/s6-buffers` is a standalone workspace, not part of the Coqo workspace or CI. It contains:

- **Both models:**
  - Model A is `ot.rs` and `oplog.rs`: a byte-offset retain/insert/delete operation with apply, compose, transform and invert, plus the `Server` and `Client` state machines.
  - Model B is `crdt.rs`: yrs 0.28 with `OffsetKind::Bytes`, an `UndoManager` that tracks only the local origin, and `StickyIndex` anchors.
- **A benchmark over a real Unix socket** (`main.rs`). A tokio daemon serves two UI clients: window A types and window B observes. Frames are JSON-RPC 2.0 notifications with a u32 length prefix, encoded as JSON or MessagePack. Heap use is measured with a counting global allocator.
- **Property tests** (`tests/convergence.rs`): three UI clients plus a daemon-side agent, with random edits (including multi-byte characters), random undo, and random delivery order on per-link FIFO queues.
  - **Convergence:** at quiescence the daemon, every replica and every *editor mirror* (a string changed only by the edits the layer hands to the editor) must be equal.
  - **No lost characters:** in insert-only runs, every inserted character must be present.
  - **Runs:** 2,000 cases per property per model, plus reconnect and agent-revert scenarios.
  - **The tests can fail:** a deliberately broken transform makes the op-log property fail at once.
- **`editor-map/`:** a headless, offscreen GPUI program (nothing is drawn on screen). It applies remote edits to gpui-component 0.11's `EditorState` and checks what happens to the cursors, the undo history and the edit events.

**Machine:** Linux, 12 logical CPUs, release build. The 100k-line file is 2.82 MB of generated Rust.

## Findings

### Keystroke latency (100k-line file, 2,000 keystrokes, p50 / p99)

| Model | Encoding | Local apply (incl. editor rope) | Window B applies | End to end, A → coqod → B | Frame size |
|---|---|---|---|---|---|
| op-log | JSON | **0.4 / 0.7 µs** | 0.3 µs | **15 / 20 µs** | 147 B |
| op-log | MessagePack | 0.3 / 0.6 µs | 0.3 µs | 14 / 18 µs | 107 B |
| yrs | JSON (base64) | **1.1 / 1.9 µs** | 1.3 µs | **15 / 21 µs** | 115 B |
| yrs | MessagePack | 1.3 / 2.7 µs | 1.3 µs | 15 / 20 µs | 88 B |

An agent editing elsewhere at 100 edits/s while the user types changes nothing measurable (op-log p99 19 µs, yrs p99 21–26 µs). Every run converged.

- **Both models beat the < 1 ms target by about three orders of magnitude.** The socket round trip, about 15 µs, dominates the end-to-end time. Latency doesn't decide between the models.
- **MessagePack saves about 40 bytes per keystroke and no measurable time.** JSON is fine for buffer traffic. Keep MessagePack negotiable as PLAN §4.4 says, but there's no need to start with it.

### Memory

| File | op-log: daemon (rope) | op-log: UI extra | op-log: daemon after 10k random edits, log trimmed / untrimmed | yrs: doc after load | yrs: doc after 10k random edits | yrs: doc after 10k typed chars | yrs: UI total after load (doc + editor rope) |
|---|---|---|---|---|---|---|---|
| 1 MB | 1.10 MB | **0** | 2.20 / 5.84 MB | 1.00 MB | 6.02 MB | 2.07 MB | 2.10 MB |
| 100k lines (2.8 MB) | 3.10 MB | **0** | 6.01 / 9.65 MB | 2.82 MB | 7.85 MB | 3.89 MB | 5.92 MB |
| 10 MB | 11.0 MB | **0** | 17.5 / 21.2 MB | 10.0 MB | 15.0 MB | 11.1 MB | 21.0 MB |

- **An op-log UI replica *is* the editor's rope.** The client keeps only its in-flight operation, the revision and its undo stack, so the UI holds **no second copy of the text**.
- **A yrs UI must keep a full `Doc` next to the editor's own rope** (gpui-component's `InputState` owns a `ropey` rope). That roughly **doubles UI memory per open buffer** (5.9 vs 3.1 MB at 100k lines).
- **yrs grows with edit history:** about 500 bytes per scattered edit (6 MB for a 1 MB file after 10k random edits) and about 100 bytes per typed character. The only way to compact it is to rebuild the document, which changes item ids under connected peers.
- **The op-log's history is bounded by what is unacknowledged:** the daemon trims the log to the oldest revision a subscriber may still send against.
- **The rope itself fragments under scattered edits** (2.2 MB for a 1 MB file in the trimmed column). This affects both models equally, because both have the rope.
- **With UI memory already at the edge** for 100k-line files (ADR 0002: +150 MB private), avoiding a second copy matters.

### Large agent edit (replace 5,000 lines ≈ 140 KB in the 100k-line file, with a user keystroke in flight)

| Step | op-log | yrs |
|---|---|---|
| Daemon applies the edit | **0.10 ms** | 1.95 ms |
| Broadcast size, JSON / MessagePack | 145 / 140 KB | 187 / 140 KB |
| UI replica transforms or integrates it | **0.09 ms** | 1.71 ms |
| Edits handed to the editor | 1 | 1 |
| Heap growth on the replica | (a log entry, freed on trim) | 1.07 MB |

**A whole-file write must be diffed first.** Agents deliver whole new file contents, and Claude Code (ADR 0005) never calls `fs/write_text_file` at all: it writes to disk, and the daemon's workspace file watcher sees the result. ACP `fs/write_text_file` sends the entire new file too. Replacing the buffer wholesale would wipe concurrent user edits, cursors and anchors, so the daemon has to turn the new content into edits before it applies them. This is the same in both models.

- **Prefix/suffix scan:** 0.84 ms on 3.3 MB. It yields one edit, which is exact when the agent changed one region.
- **Full line diff** (`similar`, Myers): 137 ms. On repetitive code it fragments into 874 small edits.

So: scan first, and run a hunk-level diff on a background task only when the prefix/suffix gap is large. The background diff is based on a known revision, so it can go through the normal transform path.

### Convergence, undo and reconnect

- **Convergence:** both models pass every property: convergence, editor mirrors matching, and no lost characters, over 2,000 random interleavings each with three clients plus an agent.
  - The op-log's tie-break is deterministic: the earlier-committed insert goes first.
  - yrs orders concurrent inserts at the same spot by client id.
- **Per-side undo:** both work.
  - **Op-log:** the client keeps inverse ops and transforms its undo and redo stacks through every remote change (property-tested with random undo).
  - **yrs:** `UndoManager` tracks only the local origin.
- **Agent edits:**
  - **Op-log:** the daemon stores each log entry's inverse, and "reject this agent change" is that inverse transformed through later changes. It's tested with user edits on both sides of the agent's change.
  - **yrs:** an `UndoManager` per agent origin.
- **Reconnect with unsaved edits** (500 offline keystrokes against 500 daemon-side agent edits):
  - **Op-log, 15.7 ms:** replay the missed revisions, then resend the pending change.
  - **yrs, 4.4 ms:** exchange state vectors and diffs.
  - Both converge.
  - **UI restart:** with either model the unsaved text lives in the daemon, so a restarted UI just takes a snapshot. Opening is about 1–5 ms for 100k lines in both.
- **Anchors:** both are exact inside a live buffer.
  - **Op-log:** daemon-side byte offsets are transformed through every committed op, with the start sticking left and the end sticking right.
  - **yrs:** `StickyIndex`.
  - **Neither replaces `coqo-anchors`' fuzzy re-anchoring** for changes made outside a live buffer.
- **yrs papercuts:**
  - `UndoManager` is `!Send` unless the `sync` feature is on.
  - The binary update needs base64 in JSON.
  - A TypeScript client needs Yjs.
  - The licence is fine (MIT).

### Mapping onto gpui-component 0.11's editor

The editor-map program checks what happens when a remote edit is applied to `EditorState` through today's public API.

**Local edits are easy to capture.** `InputEditsEvent` reports each edit as UTF-8 byte ranges plus the text after it. That is exactly the replica's input, and it is the same for both models.

**Remote edits are the problem, and it's the same problem for both models**, because both end up handing the editor ranged edits. gpui-component 0.11 has no public "apply an external edit" API. The only ranged entry point is `EntityInputHandler::replace_text_in_range` (`set_value` and `replace_all` reset the selection, and `set_value` also clears history). Measured in `editor-map`, a remote edit through `replace_text_in_range`:

1. **moves the user's cursor** to the end of the inserted text (17 → 25, where 42 was expected);
2. **drops the extra cursors** (a second cursor didn't survive);
3. **is recorded in the user's undo history**, so Ctrl+Z reverted the *agent's* header and kept the user's edit;
4. **echoes back** as a local `InputEditsEvent`.

**The workaround stops at multi-cursor.** Saving the primary selection, applying the edit, and restoring it shifted does restore the primary selection. But there is no public getter for the other cursors, and the editor's undo entries store absolute offsets that nothing rebases.

Folds, diagnostics and the decoration collections *do* shift with edits (`adjust_annotations`, `adjust_folds_for_edit`), so decorations are fine.

**What Coqo needs from the editor:**

- **A small `EditorState::apply_remote_edits(&[(Range<usize>, String)])`, upstream or in a thin patch.** It must not record undo, must transform every selection through the edits (as `transform_offset` does), and must not emit `InputEditsEvent`, or must flag the event as remote.
- **Coqo-owned undo.** The editor mod intercepts `Undo` and `Redo` with a capture-phase action handler and routes them to the replica's per-side undo stack. That stack is needed anyway to keep agent edits out of the user's undo, so gpui-component's own history goes unused.

## Decision

**Go: model A, a daemon-authoritative op-log with optimistic local apply.** This is the ot.js client-server scheme over UTF-8 byte offsets, with plain ranged edits on the wire.

Both models meet every latency and convergence requirement. Model A wins on the rest:

- **Memory:** there is no second copy of the text in the UI, and history is bounded by unacknowledged work, not by edit count.
- **Large agent edits** are about 20× cheaper on both the daemon and the UI.
- **It matches PLAN §4.2:** the daemon is authoritative and agents edit at head with no transform.
- **The wire format** is human-readable JSON edits that any client (CLI, TypeScript tooling) can produce.

Model B's real advantage is multi-master and offline sync, which Coqo doesn't need: there is a single daemon per machine, and remote development keeps one authoritative daemon (PLAN §4.4).

**Fallback: model B (yrs).** We switch to it if the op-log transform turns out to be a correctness sink in the multi-cursor or IME paths, or if Coqo ever needs several authoritative daemons for one buffer. Both models hand the editor ranged edits and take ranged edits from `InputEditsEvent`, so the fallback is contained in the replica layer behind the `buffer/*` protocol (with yrs updates carried as base64 or a later binary payload). PLAN §11 already names "daemon-authoritative with optimistic local apply" as the S6 fallback. The spike confirms it as the primary choice, and the yrs prototype stays as the fallback.

## Design for #40 (replicated buffers)

**Who is authoritative.**
- **The daemon:** one buffer actor per open file owns the rope, the revision `rev: u64`, the log, and the daemon-side anchors.
- **Agent writes, formatting and external reloads** are daemon-side edits at head.
- **Claude Code edits arrive from disk** (ADR 0005). The workspace file watcher notices the write. The buffer actor reads the file and diffs it against the buffer *as it was at the last disk sync*, not against the live buffer, so unsaved user edits aren't mistaken for agent changes. It then applies the diff as a daemon change at that base revision, through the same transform path as `buffer/edit`, with `origin: {kind: "agent", id: <session>}`.
  - Attribution comes from matching the ACP `tool_call` diff (path, old text, new text, received before the write). An unmatched change is `origin.kind = "disk"`.
  - The user's unsaved edits stay in the buffer and the agent's change merges around them. If the file is saved while there are unsaved edits *and* an agent wrote the same region, the existing conflict UI (#40 scope) takes over.
- **UI clients** are optimistic replicas that hold *only* their editor's rope plus in-flight state. There is at most one unacknowledged change per client; further edits are composed into a buffer and sent on the ack.

**Offsets.** UTF-8 byte offsets, which is what ropey and gpui-component use. Edits are `{start, end, text}` in pre-edit coordinates, sorted and disjoint, applied together like an LSP `TextEdit[]`. UTF-16 conversion happens only at the LSP and platform boundary.

**Protocol messages.** These use the Coqo Protocol v0 types ([ADR 0003](0003-coqo-protocol-v0.md)): JSON-RPC 2.0, client requests and notifications, and server pushes as subscription events. `buffer` is the daemon's buffer id, and the client id comes from the connection.

| Message | Kind | Payload |
|---|---|---|
| `buffer/open` | request | `{path}` → `{buffer, rev, text, line_ending, encoding, saved_rev, dirty}` |
| `buffer/changes` | topic (`coqo/subscribe {topic, params: {buffer}, since?}`) | Events `{rev, origin: {kind: "client"\|"agent"\|"disk"\|"format", id}, client_seq, edits: [{start, end, text}]}`. The origin's own copy, with matching client and `client_seq`, is its ack |
| `buffer/edit` | notification | `{buffer, base_rev, client_seq, edits}` |
| `buffer/revertChange` | request | `{buffer, rev}`: reject one agent change (its inverse, transformed through later revisions) |
| `buffer/save`, `buffer/close` | requests | `{buffer, rev}` → `{saved_rev}` / `{}` |

**Versioning.**
- **Revisions:** one monotonic `rev` per buffer, which **is the topic's subscription `seq`**: it is strictly increasing, starts at 1, and is stable across daemon restarts because of the journal.
- **Opening a buffer:** the client calls `buffer/open`, then subscribes with `since: rev`.
- **Reconnecting:** the client subscribes with `since: <last rev>`, so ADR 0003's replay does the catch-up.
- **Acks:** `client_seq` identifies a client's own changes when they come back.
- **Log trimming:** the daemon trims its log to the minimum `base_rev` it has seen from each subscriber.
- **Persistence:** the buffer actor persists a snapshot plus the edits since it, as a journal in the daemon's SQLite store (#17), so unsaved edits survive a daemon restart. `rev` carries on after a restart.
- **Resync past the trimmed log:** if the replay is impossible (`-32003 RESUME_GAP`), the client calls `buffer/open` again and re-applies its pending change as a diff against the new snapshot. This is rare; it only happens after a long disconnect.
- **Payloads stay JSON values**, as in ADR 0003. Measured cost: about 0.3 µs to encode a keystroke and 0.1 ms for a 140 KB agent edit, so no binary payload extension is needed. (yrs would have needed one, or base64.)

**Undo.**
- **The user's undo** lives in the UI replica as inverse ops, transformed through every remote change. Grouping follows gpui-component's typing rules (word or time boundaries).
- **Ctrl+Z and Ctrl+Shift+Z** are intercepted by the editor mod and never reach gpui-component's history.
- **Agent edits never enter the user's undo stack.** The daemon keeps each log entry's inverse, so the UI can offer "reject agent change" through `buffer/revertChange`. Accepting a change is just dropping the offer.
- **Two windows** each have their own undo stack. That matches editors that share a buffer between panes, and it's simple. A shared per-user stack can come later if wanted.

**Anchors.**
- **Daemon-side anchors are byte ranges** transformed through every committed op: the start sticks left, the end sticks right. If an op deletes the whole range, it collapses, and `coqo-anchors` (#72) flags it through its `text_hash` and marks it orphaned instead of pointing at the wrong code.
- **Anchors carry the `rev` they were resolved at.** The UI receives anchor updates with the revision they belong to, and maps them through any not-yet-acknowledged local edits with the same offset transform.
- **External changes** (disk writes by Claude Code or other tools seen by the file watcher, `fs/write_text_file`, reloads) are diffed into edits first (prefix/suffix scan, then a background hunk diff), so anchors and cursors follow them too.

**Editor integration (#41).**
- **Local edits:** subscribe to `InputEditsEvent` and feed the edits to the replica, ignoring events caused by remote applies.
- **Remote edits:** apply them through `apply_remote_edits` (see the mapping findings above), which preserves every selection and records no undo.

## Consequences

- #40 implements model A in the daemon (one buffer actor per open file) plus a UI replica crate. The property tests in `docs/spikes/s6-buffers/tests/convergence.rs` are the template for #40's acceptance test ("never lose characters").
- #41 needs `apply_remote_edits` in gpui-component, preferably upstream, otherwise a minimal patch behind the `coqo-ui` facade. It also needs Coqo-owned undo and redo. Track the editor API in its own issue so #41 doesn't stall on upstream. (Done in #145 as a vendored gpui-base patch behind `coqo_ui::editor`; see [ADR 0008](0008-editor-remote-edits-patch.md).)
- The buffer journal needs a table in the daemon's SQLite store.
- `yrs` is not added to the workspace. The prototype stays in `docs/spikes/` as the documented fallback.
