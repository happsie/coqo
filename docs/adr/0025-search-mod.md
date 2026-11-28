# 25. Project search and replace: the search mod and `search/*`

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #45 (epic #39; PLAN §6 `search`, M3)

## Context

PLAN §6 lists a `search` core mod for project search/replace and symbol search. Search has to see what the user sees: unsaved text in open buffers, not just the disk. Replace has to go through the buffer layer (ADR 0010) so open files stay consistent and undo works, and it must write unopened files safely. A project can be large, so results have to stream and a stale search has to stop.

Symbol search (tree-sitter tags, LSP workspace symbols with bolt on) is split off into a follow-up issue.

## Decision

**The matching lives in a mod; the daemon owns the files and buffers.**

- `mods/search` (`coqo-mod-search`) provides the `coqo.search@1` service. It compiles a query into a `Matcher` with ripgrep's crates (`grep-regex`, `grep-searcher`, `ignore`; MIT/Unlicense): case, whole word, regex or literal, include and exclude globs.
- The walk honours `.gitignore`, `.ignore` and git excludes (with or without a git repo), includes hidden files but never `.git`, skips binary files and files over 16 MiB, and runs in parallel.
- `Matcher::search(roots, overlay, max, cancel, sink)` takes an *overlay*: the text of each open buffer by real path. Overlaid files are searched from that text instead of the disk.
- `Matcher::replace(text, replacement, only)` computes the edits (regex replacements expand `$1`/`${name}`).
- Positions are 0-based lines and char columns, like the editor protocol. A match is identified by `(line, start, end)` so one match can be replaced later.
- The mod depends only on `coqo-mod-api` and the ripgrep crates.

**`search/run` is a topic: the subscription is the search.**

- Params: workspace, query, optional roots, `maxMatches` (default 20 000, capped at 100 000).
- Events: `files` batches (the first at once, then every 30 ms or 256 files), then one `done` with the totals and `limitHit`, and the stream ends.
- Unsubscribing (or the connection dropping) sets the cancel flag, and the walk stops. The UI starts a new subscription for each query and drops the old one.
- A topic rather than a method plus notifications keeps cancellation and backpressure in the existing subscription machinery.

**`search/replace` recomputes on the current text.**

- The UI sends the query, the replacement and the files, optionally with the matches to replace in each. The daemon matches again, so edits are never applied at stale positions; a match that no longer exists is skipped.
- An open buffer gets one `apply_head` change with the new origin `OriginKind::Replace`: unsaved and revertible (`buffer/revertChange`), like any other change.
- A file that is not open is read, decoded, edited, encoded back (BOM and line endings kept) and written atomically, under the buffer layer's open lock so it cannot race an open.
- **Held buffers:** the UI lists the buffers its own tabs show (`hold`). For those the daemon returns the edits and the rev instead of applying them (`held`). The tab applies them as a local edit, so they land in that tab's undo stack (`Ctrl+Z`), and sends them like typing. If the tab moved on meanwhile, the UI asks again without `hold`.
- Each file reports `buffer`, `written`, `held`, `unchanged` or `failed` with an error; one failure doesn't stop the rest.

**Access:** both routes need the daemon token. Sessions don't get them, and there is no agent MCP tool: agents have their own grep (PLAN §7.3 "editor-only tools").

**Keys:**

- `Ctrl+Shift+F` / `Cmd+Shift+F` shows the pane; `F4` / `Shift+F4` step through the matches. All are commands in the registry (ADR 0023): `search.show`, `search.next_match`, `search.previous_match`, plus `search.replace_all` and the three toggles in the pane.
- On macOS the code editor's component library binds `Cmd+Shift+F` to its in-file replace in the `Input` context. Coqo binds `search.show` there too, so the key means "find in files" everywhere, as in other editors.

## Consequences

- Search results match what the user sees, unsaved edits included.
- A replace across a hundred files is one request, and open tabs keep a single undo step for it.
- Symbol search still has to be built (follow-up issue).
- The UI lives in `crates/coqo/src/search/` until the mod API carries UI contributions (#147).
