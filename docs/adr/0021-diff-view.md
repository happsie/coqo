# 21. The diff view: one review surface over any diff source

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #93 (epic #90; PLAN §7.7, board 4)

## Context

PLAN §7.7 wants one diff view for local review, pull requests and walkthroughs. It should offer side by side and unified layouts, word-level highlights, collapsed unchanged regions, a file list with "viewed" checkboxes, and keyboard navigation. Comment threads (#94, #95), the walkthrough player (#100) and the PR view (#103) all build on it, so three things had to be settled first:

- where the data comes from;
- how a line is addressed;
- how other views put things on lines.

ADR 0002 already chose gpui-component's read-only, virtualized `Diff` for the body: S4 measured a 5,008-line split diff at p50 3.0 ms and p99 4.1 ms of CPU per frame. That component is fed unified patches, and it only shows the lines those patches carry. ADR 0018 gives `git/diff` (any two sides, at most 10,000 lines of context) and `git/stage` / `git/unstage` with line selections.

## Decision

**The body is `coqo_ui::diff`, a facade over gpui-component's `Diff`.**

- It uses Coqo's own types:
  - `DiffMode`: `Split` or `Unified`.
  - `Side`: `Old` or `New`.
  - `LineRef`: a path, a side and a 1-based line.
  - `LineRange`: a path plus start and end, each a side and a line, so a unified selection can run from an old line to a new one.
  - `Patch`, `GutterMarker`, `Slot`, `SlotTarget` and `LineTint`.
- `DiffViewer` is the retained state, and `DiffBody` is the element built from it on each render. Views never name gpui-component types.
- Fixed settings:
  - word-level inline emphasis;
  - three lines of context around changes;
  - disclosures that open 20 lines at a time, or a whole fold;
  - syntax highlighting with the editor's highlighter and theme. The language is `coqo-syntax`'s, chosen by path.
- gpui-component's status colours are mapped to the palette: `danger` is rose (removed lines), `success` is green (added lines), `warning` is sand and `info` is blue.

**The data comes from a `DiffSource`** (`crates/coqo/src/diff/source.rs`). The exchange type is the protocol's `FileDiff`. A source provides:

- `identity()`: a stable name for what is compared, such as `git:<root>:index..worktree`;
- `base_label()` and `head_label()`;
- `files()`: the file list;
- `file(path)`: one file's `FileDiff`;
- optionally `staging()` and `stage(path, lines)`;
- optionally `watch(changed)`.

`GitDiffSource` implements it for a root:

- unstaged (index → work tree, with untracked files from `git/status` added to the list, as in the Git pane) or staged (`HEAD` → index);
- `git/diff` with `context: 10000`;
- refreshes from `git/changes` through the window's `GitModel`.

Walkthroughs and pull requests bring their own sources.

**Full context, folded locally.** Sources fetch with full context, so every fold can open without another request: a file of up to about 20,000 lines is complete. The view turns each `FileDiff` into a unified patch, with git's path quoting and the no-newline markers. The body folds unchanged runs to three lines of context.

**The view loads every file of the diff on refresh.** That way the file list knows each file's counts, content hash and "viewed" state up front, and switching files is instant. A 300-file pull request will need lazy loading (follow-up).

**Hunks are git's.** For staging, the view groups changes the way `git diff -U3` groups hunks: runs of changes at most six unchanged lines apart form one hunk. Each hunk has a gutter button. A line selection is a drag over the line numbers.

- The view maps a selection to `git/stage`'s `{ removed, added }`: the changed lines between its ends in display order. In split mode only the selected side's lines count.
- These go whole, through `git add` or `git restore --staged`: a file's only hunk, a binary file, and an added or deleted file. A patch of lines cannot express them.

**"Viewed" is tied to the change, not the file.**

- Marks live in the workspace's state document (`workspace/setState`), so they survive restarts and follow the workspace to other windows.
- Key: `ui.diff.viewed.<fnv64(identity)>`.
- Value: `{ path: content hash }`.
- The content hash is FNV-1a 64 over the path, the sides' presence and modes, the binary flag, and every changed line with its numbers and text. Context is left out, so the same change fetched with different context hashes the same.
- A file is viewed only while its hash matches. A mark is forgotten when the change moves on or the file leaves the diff.
- The last layout is `ui.diff.mode` (`split` or `unified`) in the same document.

**Hooks for other views: `DiffOverlay`.** An overlay contributes, for the file shown:

- gutter markers (one lane, one marker per line, after the view's own staging buttons);
- slots under a line or under the file header, for comment threads;
- line tints;
- an "add" affordance. This is the `+` beside the hovered line and at the end of the selection, and it calls back with the `LineRange`.

`DiffView::scroll_to(LineRef)` shows a line: it selects the file and opens the line's fold. `DiffView` also emits `FileSelected`, `SelectionChanged` and `ViewedChanged`.

**Keys** (key context `DiffReview`):

- `j` / `k`: next and previous hunk;
- `Shift J` / `Shift K`: next and previous file;
- `v`: toggle viewed;
- `m`: toggle the layout.

The keys are plain letters because the body is read-only, and a focused text field still gets its characters first (GPUI prefers character input when the focused element accepts text).

**The `git-diff` tab kind is kept, as one tab per root and side.**

- Key: `<workspace>/<root>/staged|unstaged`.
- Saved state: `{ workspace, root, staged, path }`, where `path` is the file shown.
- Opening a file from the Git pane shows it in that tab.
- A saved per-file tab from #91 restores as its side's tab, showing that file.

## Consequences

- #94 adds comment threads as an overlay: slots under the thread's last line and `on_add` for new threads. #95 reads the threads, and the `LineRef`s give the anchors. #100 adds step markers and tints as an overlay and drives `scroll_to`. #103 implements `DiffSource` over GitHub.
- No protocol change was needed.
- Differences from board 4, recorded in `docs/design/diff.md`:
  - Rows are 13 px mono on gpui-component's 1.6 line height (20.8 px), not 12.5/23 px.
  - The "Walkthrough overlay" and "Send N comments" buttons and the pending-review note wait for #100 and #95.
- Performance: the test `a_five_thousand_line_diff_scrolls_side_by_side` scrolls a 5,000-line split diff with 100 hunks, every fold open, by 240 px per frame. Each frame includes the test platform's full re-render, the file list and the view's markers.
  - Release build, on a machine under heavy load (load average 22 on 12 cores): p50 8.1 ms, p99 9.9 ms.
  - Debug build: about 95 ms per frame.
  - The test asserts a p99 of 16 ms in release and 250 ms in debug, so CI catches pathological regressions. ADR 0002's measurement of the component alone (p50 3.0 ms) stands.
