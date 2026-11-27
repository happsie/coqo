# 8. Editor remote edits: a vendored, patched gpui-base behind `coqo_ui::editor`

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #145. Builds on [ADR 0002](0002-ui-editor-component.md) (editor choice) and [ADR 0006](0006-buffer-replication.md) (buffer replication).

## Context

ADR 0006 has the buffer replica (#40) hand ranged edits made by others (an agent, another window) to the editor. Spike S6's `editor-map` showed that gpui-component 0.11's editor can't apply them through its public API without hurting the user: the only ranged entry point, `replace_text_in_range`, moves the cursor to the edit, drops extra cursors, records the edit in the user's undo history and echoes it back as a local `InputEditsEvent`. ADR 0006 asked for an `apply_remote_edits` either upstream or as a minimal patch behind the `coqo-ui` facade.

Upstreaming is a separate, outward-facing step that isn't decided yet, and #41 shouldn't wait for it.

## Options

1. **Public API only,** in a `coqo_ui` wrapper: snapshot the selections, apply through `replace_text_in_range`, restore them shifted, and swallow `Undo`/`Redo`.
2. **Vendor the crate that owns the editor and add the method.**
3. **Our own editor element** (the ADR 0002 fallback). Far too big for this.

Option 1 fails on two points that no wrapper can fix:

- **There's no getter for the extra cursors.** `selected_range()` returns only the active selection; the rest live in a private `Selections`. A wrapper can't map what it can't read, and tracking them from the outside would mean re-implementing every mouse and keyboard path that adds a cursor.
- **The edit always goes through the undo history.** The history stores absolute offsets, which a remote edit invalidates. A wrapper can stop `Undo` from reaching it, but can neither keep the edit out nor rebase what's there.

The echoed event could be filtered with a flag and the view's scroll left alone, but not the two points above.

## Decision

**Vendor gpui-base 0.11.0 under `third_party/gpui-base/` and add a small, additive patch; expose it only through `coqo_ui::editor`.**

- **Which crate:** gpui-component's `EditorState` is `gpui_base::input::EditorState`: the editing engine lives in **gpui-base**, not gpui-component, so that is the crate we vendor. gpui-kit and gpui-component stay on crates.io.
- **How it's wired:** `[patch.crates-io] gpui-base = { path = "third_party/gpui-base" }` in the root `Cargo.toml`. gpui-kit pins `=0.11.0` exactly, and the vendored copy is that version, so the patch applies (checked with `cargo tree -i gpui-base`). `exclude = ["third_party"]` keeps it out of the workspace, so Coqo's lints, `clippy -D warnings` and workspace tests don't run on third-party code. `cargo deny check` (all-features graph) passes; the crate is Apache-2.0 and keeps its licence file.
- **The patch:** one new file, `src/input/base/remote_edits.rs`, plus one `mod` line. No upstream line changes. It adds:
  - `EditorState::apply_remote_edits(&[(Range<usize>, String)], window, cx)`: UTF-8 byte ranges in pre-edit coordinates, disjoint. It runs through the same internal `replace_text_in_ranges` path as a local multi-cursor edit, so folds, decorations, diagnostics, search and highlighting follow it, but:
    - it records no undo and emits no `InputEditsEvent` (or `Change`);
    - it maps every selection: a cursor (and a selection start) at an insert point stays left of the inserted text, and a selection end sticks right, the same rule as ADR 0006's anchors;
    - it doesn't scroll the view after the mapped cursor;
    - it clears the built-in undo history, whose absolute offsets can't be rebased. That way the built-in undo can never revert a remote edit or replay a local one in the wrong place.
  - `EditorState::selected_ranges()` (every selection) and `EditorState::is_composing()`.

  [`third_party/gpui-base/PATCHES.md`](../../third_party/gpui-base/PATCHES.md) documents the diff and the upgrade steps.
- **The facade:** `coqo_ui::editor::CodeEditor` wraps the editor. Nothing outside `coqo-ui` names gpui-component or gpui-base types.
  - `apply_remote_edits(&[TextEdit], window, cx)` calls the patched method and emits `EditorEdits { origin: Remote, .. }`.
  - Every local edit is emitted as `EditorEdits { origin: Local, edits }`, converted from `InputEditsEvent` to `(byte range, text)` pairs.
  - `set_history_handler(f)` installs Coqo's per-side undo: `Undo` and `Redo` are caught with capture-phase action listeners on the wrapper and passed to `f(editor, HistoryAction, window, cx)`, so they never reach the built-in history. Without a handler, the actions fall through to the built-in history, which is safe because remote edits clear it.
  - `selections`, `set_selections` and `is_composing` round it out for #41.

## Consequences

- **Upgrades are more work.** Each gpui-kit upgrade has to re-vendor gpui-base at the version it pins and carry the patch over (copy one file, re-add one line, fix renamed internals). If the versions drift, cargo ignores the patch with a warning and `coqo_ui::editor` stops compiling, so the problem can't go unnoticed.
- **Upstreaming stays open.** The patch is shaped as an upstream PR would be. If gpui-kit gains an equivalent API, we delete `third_party/gpui-base` and port the wrapper. Proposing it upstream needs a maintainer's go-ahead first.
- **Built-in undo is a stopgap.** Until #40/#41 install Coqo's per-side undo, a remote edit drops the user's local undo history. Agents can't edit buffers yet, so nobody hits this today.
- **IME composition vs. remote edits:** the composed text isn't reported until the composition ends, so a replica's coordinates don't include it. `apply_remote_edits` commits an active composition first, and `CodeEditor::is_composing` lets #41 hold remote edits back until the composition ends. Choosing that policy is #41's job.
- **Scroll anchoring:** the view's scroll offset stays where it was, so text inserted above the viewport pushes the visible lines down. Keeping the viewport on the same text is a follow-up.
