# 13. Anchors: `coqo-anchors`, exact mapping first, fingerprint relocation after a gap

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #72. Refines PLAN §7.5 (the anchor model) and ADR 0006's note on anchors.

## Context

PLAN §7.5 gives questions, walkthrough steps, review comments and trace points one anchor model, `{ path, range, text_hash, context_before[3], context_after[3], rev }`, and re-anchors it after edits in order: the exact range, the snippet nearby, a fuzzy match (diff plus tree-sitter node identity), else `orphaned`. The acceptance bar for #72 is that anchors stay correct or become orphaned, never wrong.

Two situations need different answers:

- **The buffer's history is known.** The daemon's buffer log (ADR 0006, 0010) has every change since the anchor's revision, so the range can be mapped exactly, edit by edit. Guessing would only add risk.
- **The history is gone.** The file changed on disk while no buffer was open, the daemon restarted, or the log was trimmed. Only the anchor's own fingerprint can find the code again.

`ServerBuffer` already has daemon-side anchors (byte ranges transformed through each commit, start sticking left and end sticking right, with a `collapsed` flag). They live only as long as the open buffer and know nothing about text, so they can't flag an edited anchor or relocate one after a gap.

## Decision

**A pure crate, `coqo-anchors`** (depends on `coqo-syntax`, `serde` and `similar`; no daemon or protocol types), owns the model and both kinds of resolution.

**The model** (`Anchor`, serde, the JSON form is what the daemon stores):

- `path` (workspace-relative, `/` separators), `rev` (the buffer revision of `range`, absent for a file read from disk), `range` (`Span`: `{ start, end }`, each `{ line, col, offset }`: 0-based line, byte column, byte offset).
- The fingerprint: `text` (the anchored text itself, also shown for an orphan) and its `text_hash` (FNV-1a 64 in hex, stable across versions and platforms); `prefix` and `suffix` (the rest of its first and last line); `context_before` / `context_after` (up to 3 lines each); `symbol` (the innermost enclosing `coqo-syntax` symbol: its container path plus name, its kind, and the anchor's line offset from the symbol's first line).
- New fields get serde defaults, so stored anchors stay readable.

**Resolving** (`Snapshot::resolve(anchor, history)` → `Resolution { status, range, rev }`). A `Snapshot` is a text (optionally a parsed `Document` and a revision). Its indexes (line starts, symbols, line hashes, whitespace-free text) are built once and shared by every anchor resolved with it.

- **With history** (the changes from `anchor.rev` to the snapshot, each a list of pre-change edits as `buffer/changes` and `Document::edit` use): the range is mapped exactly (`map_range`). Text inserted at either edge stays *outside* the anchor: a line typed above an anchored line does not join it. Text that replaces bytes inside it stays inside. Then:
  - untouched, or edited back to the same text → `Exact` (same range) or `Moved`;
  - edited in place → `Fuzzy` with the similarity of old and new text, or `Orphaned` below the threshold;
  - all its bytes deleted → only an exact copy elsewhere (cut and paste) is accepted, else `Orphaned`;
  - cut by one replacement across its edges that is at least as long as the anchor (a whole-file write diffed by the prefix/suffix scan) → relocated as if there were no history, since the anchored text may be anywhere in that replacement.
- **Without history**, candidates come from three tiers, and the best one wins:
  1. **Exact text:** occurrences of its whole lines (`prefix + text + suffix`) at line boundaries, then of the text alone (keeping word edges, so `total` is not found inside `subtotal`), scored by **evidence**: similarity of the rest of the line, the context lines, and the enclosing symbol (same path and kind, moved to another container, or renamed in place).
  2. **Whitespace-insensitive:** the text with all whitespace removed (reindented or reflowed code).
  3. **Fuzzy:** seed positions by voting (the anchor's lines and its context lines, weighted by rarity; symbols with the same name and kind; the old line), align the anchor's lines with each window by LCS, grow it while it gets more similar or to close brackets it opened, and score it by normalised text similarity (a Myers diff), with a stricter floor for short anchors. Partial-line anchors map their sub-range through the diff of the old and new lines.
- **Never a bad guess:** below `MIN_CONFIDENCE` (0.6) the anchor is orphaned. Two different places that score about the same leave it orphaned too. If its context lines now sit next to each other, its lines were deleted there, so only exact copies are looked for. The same text in the same place with matching surroundings is `Exact` at once.
- **Status:** `Exact`, `Moved` (same text, another range), `Fuzzy { confidence }` (changed text, or the text found with weak support), `Orphaned`.

`Anchor::rebased` makes the anchor afresh at a resolution's range (new range, revision and fingerprint). Rebasing after `Exact` or `Moved` keeps later resolves on the cheap exact path. After `Fuzzy` it adopts the guess, so that is the caller's decision.

**The crate ships without a daemon service.** Where anchors live (per question, per walkthrough), when they are rebased, and how they reach the UI are #73's question store decisions. The service there takes its snapshot from `coqod`'s syntax service (the buffer's up-to-date `Document`) and its history from the buffer log (`BufferService::syntax_view` already returns the changes since a revision, or the text when the log has lost them).

## Consequences

- One rule for revisions: a buffer reopened without a journal entry starts again at revision 0, so `rev` is only meaningful within one buffer lifetime. The daemon passes history only for anchors it has tracked since the buffer opened. After a restart or reopen it resolves with `None`, and the fingerprint does the work.
- The crate's edge rule (inserts at the edges stay outside) differs from `ServerBuffer`'s daemon-side anchors (start sticks left, end sticks right). Those remain cursor-like ranges for the replication layer. Code anchors use `coqo-anchors`.
- Tests: property tests carry a tag for every original byte through random edit sequences (pastes included) and check that the mapped range never takes in outside text and always keeps the surviving anchored bytes, that untouched anchors resolve exactly where the oracle says, that exact and moved anchors point at exactly their text, and that edited ones are the mapped range or orphaned. Golden tests relocate anchors without history after realistic refactors in Rust, TypeScript, Java and Kotlin (lines inserted above, function moved, renamed, body reformatted, edited in place, anchored line deleted → orphaned, ambiguous copies → orphaned). Persistence tests pin the JSON form and resolve after a simulated restart.
- Performance (release, 10,000-line Rust file, 1,000 single-line anchors): re-anchoring through one change takes about 0.15 ms. Relocating all of them without history takes about 22 ms. The perf test asserts under 10 ms and 1 s.
- Fuzzy relocation is heuristic by nature. The thresholds (`MIN_CONFIDENCE` 0.6, exact-text matches need 0.85 to count as `Moved`) are constants in `resolve.rs`, and the golden tests pin their behaviour.
