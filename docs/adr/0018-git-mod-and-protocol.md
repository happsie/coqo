# 18. The git mod and `git/*`

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #91 (epic #90; PLAN §7.7)

## Context

PLAN §7.7 wants a git mod: status, hunk and line staging, commit, branches, stash, blame and log with a graph. PLAN §4 already picks `gix` for reads and the `git` CLI for writes. Several things build on this API: the diff component (#93), worktrees (#94), the walkthrough (#99) and the file tree's status letters (#163). So the protocol has to be settled before they start. The open points were:

1. Where the code lives and how the daemon reaches it.
2. How faithful the reads are compared with `git` itself.
3. How line staging works.
4. How clients learn that something changed, including changes made outside Coqo.
5. Who may call these methods.

## Decision

**A mod, reached as a service.**
- `mods/git` (`coqo-mod-git`) is a built-in mod (`coqo.git`, tier core, activated at startup). It provides the `coqo.git@1` service.
- It depends only on `coqo-mod-api` and `gix` (Apache-2.0/MIT). `git2`/libgit2 is not used (GPL with a linking exception).
- The daemon's `git/*` routes (`crates/coqod/src/git.rs`) resolve the service on every call. A disabled mod turns `git/*` off with `NOT_A_REPOSITORY`, and a mod that provides `coqo.git@1` replaces the built-in one.
- Domain types stay in the mod; the daemon converts them to the protocol's types.
- Calls run on blocking threads.

**Reads with `gix`, writes with the user's `git`.**
- `gix` handles status, diffs, blame, log, branches and the stash list.
- Writes run `git`: `add`, `restore --staged`, `rm --cached` (on an unborn branch), `apply --cached`, `commit`, `switch`, `branch` and `stash`. They run with `LC_ALL=C`, no terminal prompt, `GIT_EDITOR=:`, and the inherited `GIT_DIR`/`GIT_WORK_TREE` removed.
- Pathspecs are passed with `--literal-pathspecs` per command, not through the environment, because the environment variable breaks `stash -u`.
- Hooks and signing are the user's own config. Coqo neither adds nor skips them.

**Fidelity, checked against the CLI.** Fixture repositories in temporary folders compare every read with `git status --porcelain=v2`, `git diff`, `git diff --numstat`, `git log` and `git blame --porcelain`. The deliberate differences are documented in the mod:
- Untracked files are listed one by one (`-uall`).
- Renames use git's default detection (#226) whatever `status.renames` or `diff.renames` say.
- `.gitattributes` apply as in `git diff`, except textconv (#226).
- Blame covers committed history only.
- Diffs are Myers with the indent heuristic, and use git's default funcname section headings.

**Paths** in `git/*` are relative to the repository. `git/status` returns `RepoInfo { workdir, prefix }`; the root-relative path is the git path minus `prefix + "/"`.

**Line staging.**
- `git/stage` and `git/unstage` take `lines: { removed: [old line numbers], added: [new line numbers] }` for exactly one path, numbered as in the matching `git/diff` (index → work tree to stage, `HEAD` → index to unstage).
- The mod builds a patch with recounted hunk headers and applies it with `git apply --cached` (or `--reverse` to unstage):
  - Staging turns unchosen `-` lines into context and drops unchosen `+` lines.
  - Unstaging does the reverse.
- A selection that no longer matches the diff fails with `STALE_SELECTION` (1602). Binary files fail with `UNSUPPORTED`.

**Renames (#226).**
- `git/status` pairs a deleted and an added path on the same side (`HEAD` → index for staged, index → work tree for unstaged) as git does: exact matches first (same blob; a basename match wins), then a unique basename at 75% or more, then git's similarity matrix. That matrix uses spans of up to 64 bytes or one line, a CR before an LF is skipped in text, the top 4 candidates are kept per destination, and the minimum score is 50% with a 1000 × 1000 limit.
- The entry gets `renamed` on that side and `origPath`, and the old path has no entry. As in git, a staged delete is not paired with an unstaged add.
- `git/diffFiles` pairs the same way, like `git diff -M`. `git/diff` takes an optional `origPath`: the old side is read from it, and the result echoes it.
- Line unstaging of a staged rename diffs against `HEAD:<origPath>`, so the rename stays staged.
- The fixtures compare `--porcelain=v2 --renames` and `diff -M --numstat` path by path.

**Attributes (#226).**
- Work-tree contents go through `gix`'s filter pipeline before they are compared: eol/`text`, `ident`, `filter.<driver>.clean` and `working-tree-encoding`, as `git add` would.
- `binary`, `-diff`, `diff` (forced text) and `diff.<driver>.binary` decide what counts as binary. Binary files have no hunks and no line counts, as in `git diff --numstat`'s `-`.
- `textconv` is not run, because it would execute arbitrary commands from the repository's config for a read.

**`git/discard` (#226)** throws away unstaged changes.
- `{ paths }` restores tracked files from the index (`git restore --worktree`).
- Untracked files go to the trash through the workspace's `fs/delete` path (#161). The result lists them, each with an `fs/restore` token, or why it can't be restored.
- `{ paths: [one], lines }` reverses just those lines in the work tree (`git apply --reverse`, numbered as in index → work tree). `git apply` runs the smudge and clean filters, so CRLF files keep their line ends.
- It refuses before changing anything:
  - conflicts;
  - intent-to-add files and unstaged renames (there is no index version to go back to);
  - untracked folders git didn't look into (nested repositories);
  - paths outside the root;
  - lines of binary, untracked or deleted files.
- Untracked files are trashed first. If a trash move fails, tracked files are left alone and the error names what was already moved.

**`git/changes`** sends `{ root, kinds }` per workspace, where `kinds` ⊆ `worktree | index | head | refs | stash`. It merges two sources:
- the workspace's `fs/changes` feed, which is already debounced and ignore-aware, as `worktree`;
- a `notify` watch on each root's git directory and common directory (`HEAD`, `index`, `refs/`, `logs/`, `packed-refs`, the stash; `*.lock` is ignored), debounced to 100 ms.

Writes made through `git/*` publish their event at once. The watch starts with the first subscriber and stops with the last, or when the workspace closes, which ends the feeds.

**Errors:** `NOT_A_REPOSITORY` 1601, `STALE_SELECTION` 1602, and `GIT_FAILED` 1603 (git's own message, for example "nothing to commit").

**Access:** every route is `Access::daemon()`. There are no git MCP tools: agents run their own git.

**UI.** Until the mod API carries UI contributions (#147), the Git sidebar pane and a minimal `git-diff` tab live in the app crate (`crates/coqo/src/git`), the same way Files does.
- A per-window `GitModel` holds each root's status, branches and stashes, refreshed from `git/changes`.
- `GitModel::letter(root, path)` gives the per-path letter for the file tree (#163). A rename is `R`, in `blue`.
- Discard always asks first. The card lists each file with what it loses: `+a −d` from `git/diffFiles` index → work tree, or "untracked". It says whether the change can be undone, and it names open tabs with unsaved edits to those files.
- Trashed untracked files go into the window's file history (#291, now one per window and shared by the file tree and the Git pane). The note after a discard offers Undo, and so does the file tree's undo.
- Discarded changes to tracked files can't be undone. Coqo keeps no copy of them, as with `git restore` or VS Code's Discard, and the card says so.

## Consequences

- #93 builds its diff component on `git/diff` (any two sides: work tree, index or a revision) and on line staging. The `git-diff` tab is the place it plugs in.
- Rename detection, attribute filters and discarding changes landed in #226. The blame and log views are follow-ups, and the protocol has room for them. The agent-drafted commit message is ADR 0024.
- Every call reopens the repository. That is cheap with `gix`, and it keeps the daemon stateless about repositories. A cache can come later if profiling asks for it.
- The git-directory watch follows the workspace's `workspace/events`. A root added after the subscription started has its git and common directories watched as soon as its `changed` event lands, and a removed root's directories stop being watched unless another root shares them (#227).
