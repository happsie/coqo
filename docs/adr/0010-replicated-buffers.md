# 10. Replicated buffers: protocol details, disk merges and the journal

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #40. Refines the "Design for #40" section of [ADR 0006](0006-buffer-replication.md); the decision there (daemon-authoritative op-log, ot.js-style, UTF-8 byte offsets) stands.

## Context

ADR 0006 chose the op-log and sketched the `buffer/*` messages, disk-change handling, undo and the journal. Implementing it (crate `coqo-buffers`, service `crates/coqod/src/buffers`) needed decisions the sketch left open or got slightly wrong: how a reconnecting replica recognises its own changes, how a disk write is merged when the file was never equal to any buffer revision, when the log can be trimmed, what "dirty" means, and whether the UI replica can really hold no copy of the text.

## Decision

**Crates.** `coqo-buffers` holds everything that is not I/O: `TextOp` (ported from the S6 prototype, with validation so malformed edits are errors, never panics), the prefix/suffix `diff`, file decoding, `ServerBuffer` (the daemon's authoritative state) and `ClientReplica` (the UI side, no GPUI dependency). `coqod`'s `buffers` service adds files, the watcher, the journal and the protocol. One buffer per workspace and **canonical** path, so two windows and a symlink share it.

**Replica identity.** `buffer/open` returns a `replica` id (random, `r-<hex>`) next to the shared `buffer` id, and `buffer/edit` carries it, because the connection is not a stable identity across reconnects. A change's `origin` is `{kind, id?}` with kinds `client` (id = replica), `agent` (id = session), `disk`, `format`, `revert` (id = reverted rev) and `save`. The daemon remembers each replica's highest committed `client_seq` (also in the journal) and **drops edits with a seq it has committed**, so a replica can always resend its change in flight with the same seq after a reconnect. Reopening with `replica` set returns `lastSeq`, which tells the replica whether its in-flight change is already in the snapshot.

**Rejections.** `buffer/edit` stays a notification. When the daemon cannot apply one (base revision trimmed or in the future, invalid edits, unknown buffer) it sends `buffer/editRejected` to that connection; the replica reopens and resyncs.

**Unsaved edits as one operation.** The buffer keeps the file as last read or written (the *disk baseline*) and one operation `unsaved` from it to the text. Every commit is composed into `unsaved`; a save resets it. A disk change `D` (baseline → new file, from the prefix/suffix diff) is merged by `(D', U') = transform(D, unsaved)`: `D'` is committed at the head and `U'` becomes `unsaved`. This is what "diff against the buffer as of the last disk sync, applied at that base revision" in ADR 0006 meant, but it also works after a merge, when the file no longer equals any buffer revision. Overlap between `D` and `unsaved` sets `conflict` on the change; both sides are kept.

**Dirty** means `unsaved` changes something (after dropping replacements that put back the text they remove), so typing a character and deleting it, or undoing back to the file, is clean again. Each `buffer/changes` event carries `dirty`; a save is an event too (`origin: save`, no edits, `dirty: false`), so every window sees it.

**Save** writes a temporary file next to the real one (same permissions, BOM restored), fsyncs and renames it, then makes the written text the baseline, so the watcher seeing Coqo's own write finds no difference. Before writing it re-reads the file: if it changed since the baseline, the change is merged and the save fails with `SAVE_CONFLICT` (unless `force`), so a save never silently overwrites an agent's write the watcher has not delivered yet.

**Agent attribution.** The ACP host calls `BufferService::expect_agent_write(session, path, old_text, new_text)` when a `tool_call` diff arrives. A later disk change to that file is attributed to the session when the new file equals `new_text`, or when the changed region lies inside an occurrence of `new_text` and the replaced region inside one of `old_text` (Claude Code's `Edit` tool reports snippets). Unmatched changes are `disk`.

**Writes in progress.** Most writers (Node's `writeFile`, Rust's `fs::write`) truncate the file and then write it, so the watcher can read it empty in between (#207: under load, the debounce window ends between the two). Merging that moment would commit a deletion and then a re-insertion, lose the agent attribution (neither half matches the diff) and misplace unsaved edits (the user's line ends up after the re-inserted text). So an empty read of a file whose baseline is not empty is **held back**: the file is read again after `truncate_settle` (250 ms) and merged only if it is still empty. While an announced agent write of non-empty text to that file is pending, the empty read is held until the text lands, for at most 5 s (`MAX_AGENT_HOLD`), which makes agent attribution independent of how long the agent pauses mid-write. An announced write that explains the empty file (the agent emptied it) is merged at once. Partial writes of large files (several `write` calls) are not detected; writers that write aside and rename (many editors, Coqo's own save) are seen atomically anyway.

ACP `fs/write_text_file` into an open buffer goes through `agent_replace` (diffed against the live text); `fs/read_text_file` can serve unsaved text with `open_text`.

**Log trimming.** The log keeps what any known replica may still base an edit on (each replica's latest base revision), but always the last 1,024 entries (so recent agent changes stay revertible with no window open) and never more than 10,000. Older bases get `REVISION_GONE`; older reverts too.

**Replay and the journal.** `buffer/changes` is backed by an in-memory `SeqLog` of the last 4,096 changes, whose seq is the rev. The journal lives in the **workspace** database (workspace migration 2): one snapshot row per file with unsaved edits (baseline, `unsaved` as edits, rev, replicas' seqs) plus the changes since as JSON rows, written in order by one background task in batched transactions. Disk merges and saves write a new snapshot (the change rows cannot express a moved baseline); a clean buffer has no rows. On load, the journal is replayed, the replay log is refilled from it (so `since` works across a daemon restart), and the file is compared with the baseline in case it changed while the daemon was down.

**Lifetime.** `buffer/changes` subscribers keep a buffer loaded; 30 s after the last one leaves (or after an open nobody subscribes to) it is unloaded, its unsaved edits kept in the journal. Buffer ids are per load, so a stale id gets `UNKNOWN_BUFFER` and the replica reopens.

**The replica keeps its own rope** (deviation from ADR 0006's "no second copy"). Undo needs the deleted text of each local edit, and gpui-component reports edits only after applying them; a resync needs the last acknowledged text to rebase pending edits. Ropes share unchanged chunks, so the acknowledged copy costs only edited chunks, but the editor's text is held twice. If the editor wrapper can hand over deleted text (#145's facade could), the replica can be made generic over the editor's rope later.

**Encodings.** UTF-8 with or without BOM; anything else fails to open with `UNSUPPORTED_ENCODING`. Line breaks are kept byte-exact (mixed files round-trip); the detected convention is reported as `lineEnding`.

## Consequences

- #41 drives `ClientReplica`: `local_edit` on `InputEditsEvent`, `receive` on `buffer/changes` (edits go to `apply_remote_edits`), `undo`/`redo` from the intercepted actions, `resend` after a reconnect and `resync` after `RESUME_GAP`, `UNKNOWN_BUFFER` or `buffer/editRejected`.
- #54/#56 call `expect_agent_write` for every ACP diff, and use `agent_replace` / `open_text` for `fs/write_text_file` / `fs/read_text_file`.
- The property tests run three replicas, an agent and disk writes in random delivery order against `ServerBuffer`, and the same against the real service over `coqo_protocol::Server::serve`; both check convergence, editor mirrors and that no inserted character is lost.
- Not yet done: a background hunk diff for large, multi-region disk writes (the prefix/suffix scan yields one replacement, which is correct but moves cursors inside it), protocol access to anchors (#73; the model and re-anchoring are `coqo-anchors`, ADR 0013), and a dedicated conflict UI.
