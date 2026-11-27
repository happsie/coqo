# 22. Review threads: comments on diff lines, the pending review, and backends

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #94 (epic #90; PLAN §7.7 and §7.8, board 4)

## Context

PLAN §7.7 wants the user to select lines in a diff and comment on them. The comments collect as a *pending review*, which "Send to agent" (#95) delivers in one message, and the agent's replies land in the same threads. §7.8 wants the same threads on pull requests, backed by GitHub. Before #95 and the PR view (#101, #103) build on it, four things had to be settled:

- what a thread is pinned to, so it can be found again after the files change;
- the shape of a thread, its comments and the pending review;
- where threads are kept, and how the UI follows them;
- what a backend is, so GitHub can come later without another model.

The diff view (ADR 0021) already gives overlays gutter markers, line slots, tints and a `+` per line. Anchors (ADR 0013) give a fingerprint anchor that is resolved exactly, moved, fuzzily or not at all. The question store (ADR 0016) shows how to keep anchors in `coqo.db` and publish changes on a resumable topic.

## Decision

**A thread is pinned to a file *version*, not to a diff.**

- `ThreadAnchor` holds:
  - `root` and a root-relative `path`;
  - `side` (`old` or `new`): the diff side it was started on;
  - `version`: the git `DiffSide` the commented text is read from (`worktree`, `index`, or `rev`);
  - `code`: the `CodeAnchor` (fingerprint) of the lines, or none for a comment on the whole file;
  - `resolution` and 1-based `lines`, filled in each time the daemon serves the thread.
- The old side of the unstaged diff is the index, and the new side of the staged diff is the index too. Pinning to a version means a thread started on either diff shows in both, on whichever side shows that version.
- **Re-resolving.** On each `threads/list` (and before each change), the daemon reads the version (the work tree from disk, the index or a revision through the git mod's new `Repo::read_file`) and resolves every anchor in it. A moved anchor is rebased and saved, so it keeps up with later moves. An anchor whose code is gone is `orphaned`. The thread keeps its comments, and the UI shows it on the file with the original snippet ("Outdated").
- There is no live tracking of unsaved buffers: a diff shows saved text, and the UI lists the threads again whenever its diff reloads.

**Threads and comments.**

- `Thread`: `id`, `workspace`, `backend` (`local` or `github`), `diff` (the identity of the diff source it was started in), `anchor`, `status` (`open` or `resolved`), `comments`, and times. `seq` is the last change's sequence number.
- `Comment`: `id`, `author`, a markdown `body`, `pending`, `created_at`, and `edited_at`.
- `Author` is `user`, `agent { session }` or `github { login }`. **An agent's reply is just a comment whose author is `agent { session }`.** #95 adds it through the same `reply` path (`ThreadService::reply`) without changing the model.
- Only the user's comments can be pending, edited or deleted (`NOT_YOUR_COMMENT`). A thread whose last comment is deleted is deleted.
- **The pending review** is the set of the user's pending comments: `review/pending { workspace, diff? }` returns their count and each comment with its thread's anchor (what #95 sends), and `review/discard` deletes them. Sending (#95) will clear `pending`.

**Storage and protocol.**

- **Storage:** global migration 12 adds `review_threads` (the anchor as JSON, as made and as last rebased) and `review_comments`. A deleted thread keeps a tombstone row, so a resumed `threads/changes` can replay the deletion.
- **Methods** (all daemon-token):
  - `threads/list { workspace, root?, path?, diff? }` and `threads/get`;
  - `threads/create { workspace, root, path, side, version, lines?, diff?, body, pending }`;
  - `threads/reply`, `threads/editComment`, `threads/deleteComment`;
  - `threads/resolve` and `threads/reopen`;
  - `review/pending` and `review/discard`.
- **Topic:** `threads/changes { workspace? }` sends `changed { thread }` and `deleted { id, workspace }`. It is resumable by `seq`, like `questions/changes`.
- **Errors:** `UNKNOWN_THREAD` (1801), `UNKNOWN_COMMENT` (1802), `NOT_YOUR_COMMENT` (1803).

**Backends.**

- `ThreadBackend` (in `coqod::threads::backend`) is told each change after it is stored (`Mirror::Created`, `Commented`, `Edited`, `Deleted`, `Status`).
- `LocalBackend` does nothing, because the store is the record.
- The GitHub backend is #257, under epic #101. The module docs map the model onto GitHub's review threads:
  - `side` maps to `LEFT` and `RIGHT`;
  - `lines` maps to `startLine` and `line`;
  - a file-level thread maps to `subjectType: FILE`;
  - the pending review maps to GitHub's pending review, submitted as Approve, Request changes or Comment;
  - resolve and reopen map to `resolveReviewThread` and `unresolveReviewThread`;
  - GitHub's `isOutdated` maps to an orphaned anchor.
- The GitHub backend needs remote ids, which a later migration adds.

**UI.**

- `ThreadsOverlay` is a `DiffOverlay` that `DiffTab` attaches when the source can name its versions (`DiffSource::versions`).
- The overlay draws:
  - gutter markers, in sand while pending, sage once resolved, blue otherwise;
  - a sand tint on pending lines;
  - the `+`, which opens a composer under the line;
  - thread cards with comments, Pending pills, Edit, Reply, Resolve and Discard;
  - resolved threads folded to one row with a Show button.
- `DiffOverlay` gains `header`, `file_badge`, `files_note` and `hint`. With them the overlay adds:
  - "N pending", Discard, and a disabled "Send N comments to Claude" with the tooltip "Sending to the agent comes in #95";
  - a "N pending" badge on each file row;
  - the board's note on the pending review;
  - a hint in the footer.
- `WorkspaceThreads` is one store per workspace, kept current by `threads/changes` through `Resumable`.

## Consequences

- #95 reads `review/pending`, sends the batch to the session, clears `pending`, and adds agent replies with `Author::Agent`. The send button and its count already exist.
- Threads started in the unstaged diff carry that diff's identity, so its header counts only its own pending comments. A thread shows in every diff that shows its version.
- Comments are on saved text. A thread on unsaved edits waits until they are saved. Live anchoring against open buffers (as questions have) is not needed for diffs, which show saved text.
- Re-resolving reads files on every list. That is fine for a workspace's handful of threads. Lists are grouped by file version, so each file is read once.
- Markers in the file editor's gutter are #258 (epic #90).
