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
- `Author` is `user`, `agent { session }` or `github { login }`. **An agent's reply is just a comment whose author is `agent { session }`.** #95 adds it with `review/reply` (below) without changing the model.
- Only the user's comments can be pending, edited or deleted (`NOT_YOUR_COMMENT`). A thread whose last comment is deleted is deleted.
- **The pending review** is the set of the user's pending comments: `review/pending { workspace, diff? }` returns their count and each comment with its thread's anchor (what #95 sends), and `review/discard` deletes them. Sending (#95, below) clears `pending`.

**Storage and protocol.**

- **Storage:** global migration 12 adds `review_threads` (the anchor as JSON, as made and as last rebased) and `review_comments`. A deleted thread keeps a tombstone row, so a resumed `threads/changes` can replay the deletion.
- **Methods** (all daemon-token):
  - `threads/list { workspace, root?, path?, diff? }` and `threads/get`;
  - `threads/create { workspace, root, path, side, version, lines?, diff?, body, pending }`;
  - `threads/reply`, `threads/editComment`, `threads/deleteComment`;
  - `threads/resolve` and `threads/reopen`;
  - `review/pending` and `review/discard`.
  - `review/send` and `review/reply` (#95, below; `review/reply` is also open to session tokens).
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
  - "N pending", Discard, and "Send N comments to Claude" (#95, below);
  - a "N pending" badge on each file row;
  - the board's note on the pending review;
  - a hint in the footer.
- `WorkspaceThreads` is one store per workspace, kept current by `threads/changes` through `Resumable`.

**Sending the review to the agent (#95).**

- **`review/send { workspace, diff?, session }`** (daemon token) sends the pending comments of the workspace (of one diff, with `diff`) to an agent session in **one prompt**, then clears their `pending` (`Mirror::Sent` tells the backend). It returns `{ session, turn, count, threads }`.
  - The session must exist and belong to the workspace. Nothing pending is `INVALID_PARAMS` ("there are no pending comments to send").
  - **A session in a turn is refused** with `SESSION_BUSY` and a message saying what to do ("\"Volume discount\" is in the middle of a turn. Wait for it to finish or pick another chat; your N comments stay pending."). Nothing is cleared: the prompt is sent first and `pending` is cleared only after it was accepted.
  - **The prompt** opens with what to do ("address my code review: N comments in M threads on the unstaged changes… call `reply_to_review` with its `thread_id`… `resolve: true` once it is done; reply in every thread"), then one section per thread: `## Thread <id>: <absolute path>:<lines>`, the code it points at as a fenced block (as anchored, at most 40 lines; an orphaned anchor says so), the thread's earlier comments as context ("Earlier in this thread", **Me** / **You** / another agent / `@login`), and the new comments quoted. Paths are absolute, so the agent needs no lookup.
  - The prompt ends with a `resource_link` to `coqo://review?workspace=…&comments=N&threads=…&diff=…` (`ReviewLink`). The chat recognises it and renders the user message as a **"Review sent: N comments"** card with **Open diff**, which dispatches `OpenReviewDiff` and opens (or focuses) that diff's tab, and **Show** for the message itself. It is in the transcript, so the card survives reloads, without a new update type.
- **`review/reply { workspace, session, thread, body, resolve }`** adds a comment by `agent { session }`, and resolves the thread with `resolve`, in one change (one `threads/changes` event, `Mirror::Commented` and `Mirror::Status`).
  - **Access:** `Access::check`: a session token may reply only as its own session and only in its own workspace (`require_workspace`, `require_session`), and a thread of another workspace is `UNKNOWN_THREAD`, as if it did not exist.
  - The agent calls it through the core tool **`reply_to_review { thread_id, body, resolve? }`** of `coqo-editor` (ADR 0014), which fills in the workspace and session from its connection. The guide (v4) and the MCP `instructions` name it in one line each.
- **Which chat it goes to** is the UI's choice, not the daemon's: the send popover lists the workspace's chats as pills, the default first:
  1. the chat whose agent changed the commented files. The daemon records it (#286, see *Who changed a file* below), so this works after the chat's tab is closed or Coqo restarts; a session that was closed (ended) is skipped. When the daemon has no record, the open chat tabs' transcripts are searched (their edit tool calls' paths), as before;
  2. else the chats shown in the workbench, then the others by activity (as the ask popover, #229);
  3. "New chat", which starts one with the preferred provider and sends to it.
  The popover checks the target's state first and shows the same busy message without a round trip; the daemon's check is the one that counts.
- **While the turn runs** the UI remembers the sent review (`WorkspaceThreads::track`): the diff header shows "Claude is addressing N comments" (the provider's display name), replies appear in the cards as `threads/changes` brings them, and a thread the agent resolves **stays open** so its reply can be read. When the chat list shows the session out of its turn (state changed after the send), the review is forgotten and the resolved threads fold, their folded row reading `Resolved · "first" · Claude: last`.

- **Who changed a file** (#286) is recorded by the daemon (`coqod::provenance`), in the global database (`file_changes`, global migration 14): per workspace, root and root-relative path, the **last** session that changed the file, when, and how sure it is:
  - **`write`**: the session wrote it. Either a **completed** tool call whose `diff` content names the file, or an `edit` / `delete` / `move` tool call whose locations do (the session manager hands every recorded update to an `UpdateObserver`, so this works for any provider, ACP or native; a failed or rejected call records nothing), or a successful ACP **`fs/write_text_file`** (into an open buffer or to disk; the ACP host reports it with `AgentFs::wrote`).
  - **`turn`**: the file changed on disk while the session's turn ran (or up to 2 s after it ended), and it was the **only** session in a turn in that workspace. This covers agents that edit through tools of their own that report no diff (a shell `sed -i`, a code generator). It is weaker: the user's edits outside Coqo during the turn count too, so it is recorded only when one session can be blamed, never for the user's own `buffer/save`, and it never replaces the same session's `write` record. The workspace's `fs/changes` are followed only once a turn has started in it.
  - Records follow renames (`PathMoves`, #288) and survive restarts.
  - **`agents/changedBy {workspace, root, paths}`** (daemon token only) returns the sessions that last changed any of `paths`, most recent first, with each one's title, provider, closed flag, paths and strongest evidence.
  - The diff header ends its summary with **"made by Claude in "Volume discount""** (board 4): the most recent session with `write` evidence for the diff's files, else the most recent one.

## Consequences

- #95 sends the pending review with `review/send`, and the agent replies with `reply_to_review` (`review/reply`, `Author::Agent`).
- Provenance is per file, not per hunk: a file the agent edited and the user then edited outside Coqo (no turn running) still names the agent. Only a later change by another session replaces it.
- Threads started in the unstaged diff carry that diff's identity, so its header counts only its own pending comments. A thread shows in every diff that shows its version.
- Comments are on saved text. A thread on unsaved edits waits until they are saved. Live anchoring against open buffers (as questions have) is not needed for diffs, which show saved text.
- Re-resolving reads files on every list. That is fine for a workspace's handful of threads. Lists are grouped by file version, so each file is read once.
- Markers in the file editor's gutter are #258 (epic #90).
