# 28. Worktrees are workspaces; the rail shows one icon per project

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #92 (epic #90; PLAN §7.9)

## Context

PLAN §7.9 promised "New workspace from worktree" so that agents can work on several tasks at once without editing each other's files. The rail had one icon per workspace, so every worktree would add an icon, and three worktrees of one repository would look like three unrelated projects. The maintainer's decision on #92:

- the rail shows one icon per repository (project), and `Ctrl/Cmd+1..9` switches projects;
- a project's icon sums the alerts and running agents of all its worktrees;
- a worktree switcher at the top of the sidebar, under the project's name, shows the current worktree and branch and lists the worktrees, each with its branch, change count and agent state, plus **New worktree…** and **Remove**;
- each worktree is its own workspace, and switching keeps the other worktrees' agents running;
- removal warns about uncommitted or unpushed work and never forces;
- the palette gets **Switch worktree** and **New worktree**.

This ADR settles how worktrees are grouped, where the grouping is stored, where new worktrees go, what a multi-root workspace belongs to, who runs `git worktree`, and what "never forces" means.

## Decision

**A project is a repository, identified by its git common directory.** Every worktree of a repository shares one common directory (`git rev-parse --git-common-dir`: the main worktree's `.git`, or the bare repository). The daemon finds it without running git (`coqod::workspace::project`): it walks up from the folder to the first `.git`, follows a `gitdir:` file to the worktree's admin folder, then that folder's `commondir`, and canonicalises the result. A test checks it against `git rev-parse` for a main worktree, a subfolder, a linked worktree and a folder in no repository. Because the grouping comes from git's own files, worktrees made outside Coqo (`git worktree add`, `.claude/worktrees/*`, another tool) join their project as soon as a workspace is opened on them.

**The first root decides.** A workspace with several roots belongs to the project of its first root. Its other roots don't change where it sits on the rail. A workspace whose first root is in no repository is a project of its own.

**Stored per workspace (global migration 15).** `workspaces.project` (TEXT) holds the canonical common directory, `''` for no repository, and NULL for not yet computed (rows from before the migration). The daemon recomputes it when a workspace is opened and when its first root changes (`addRoot`, `removeRoot`), and for every workspace at start-up, keeping the old value when the folder is gone (an unmounted disk shouldn't move a workspace on the rail). It reaches clients as `WorkspaceInfo.project: { id, name, mainWorktree? }`. `id` is the common directory. `name` is the main worktree's folder, or the bare repository's name without `.git`. `mainWorktree` is the main worktree's folder, if it has one. The field is optional and left out for no repository, so older clients are unaffected.

**The rail groups by `project.id`** (`crates/coqo/src/workspace.rs`, `RailProject`). Projects are in the order their first workspace was added, so `Ctrl/Cmd+1..9` stays put. The icon is named after the main worktree's workspace, so renaming that workspace renames the icon. Without one, it is named after the repository. Clicking a project shows its most recently opened worktree. Clicking the window's own project does nothing. The badges and the tooltip add up the sessions of every workspace in the project. The context menu acts on the whole project: **Remove from list** forgets all of its workspaces and never touches a folder.

**New worktrees go beside the repository**, in `<parent>/<repo>-worktrees/<branch-slug>`. The slug replaces everything but letters, digits, `.`, `_` and `-` with `-`, and appends `-2`, `-3`… when the folder is taken. A sibling folder, not one inside the repository, so that:

- the main worktree's file tree, search, watcher and build tools never see it (ADR 0027 would skip it anyway, but other tools wouldn't);
- `git clean` and IDE indexers in the main worktree leave it alone;
- the path stays short and readable.

`git/addWorktree` takes an explicit `path` for callers that want somewhere else. A setting for the folder is left for later. An existing branch is checked out as it is, and the base is then ignored. A new branch is created from the base, which defaults to the current branch (`git worktree add -b <branch> <path> [<base>]`).

**Writes go through the git CLI, in the git mod; the daemon owns them.** `mods/git` (`coqo.git@1`) gains `worktrees`, `add_worktree`, `worktree_work` and `remove_worktree`. Listing uses `git worktree list --porcelain -z`, and the writes are `git worktree add`, `remove` and `prune`, run like the mod's other writes (`LC_ALL=C`, repository environment variables cleared, arguments checked). As with every mod, it depends only on `coqo-mod-api` and third-party crates. The daemon exposes three daemon-only methods:

- `git/worktrees`: the list, with each worktree's changed-file count, and the default folder for new worktrees;
- `git/addWorktree`: adds the worktree and opens a workspace on it, returning both;
- `git/removeWorktree`: see below.

None of them is open to agent sessions (no `SESSION_ROUTES` entry).

**Removal never forces.** `git/removeWorktree` never passes `--force`, and:

- refuses the main worktree (`INVALID_PARAMS`, "the main worktree can't be removed") and a locked worktree;
- prunes a worktree whose folder is already gone;
- with **uncommitted changes** (tracked or untracked), returns `removed: false` with the counts, even when confirmed. Removing them would need `--force`, so the user has to commit or stash first;
- with **unpushed commits** (ones no other branch, remote-tracking branch or tag has: `git rev-list --count HEAD --not --exclude=<branch> --branches --remotes --tags`), returns `removed: false` unless `confirm` is set. Confirming is safe because the branch is kept, so the commits survive.

Otherwise it runs `git worktree remove <path>` from the main worktree. The daemon then forgets the workspaces whose first root is in the removed folder (`workspace/remove`) and returns their ids. The branch is never deleted.

**The switcher** (`crates/coqo/src/worktrees.rs`) is per window, at the top of the sidebar. Choosing a worktree switches the window to its workspace, opening one on the folder the first time. If the window's own worktree is removed, the window shows the main worktree. The palette commands are `worktree.switch` and `worktree.new`.

## Consequences

- Worktrees cost no rail space, so running agents on several branches at once is practical. Their agents keep running in the daemon while another worktree is on screen, and the project's icon shows what they do.
- Grouping depends on the filesystem, not on Coqo's history, so it survives worktrees made by hand or by other tools and needs no repair when the user moves things around. The cost is a few `stat`s and two small reads per workspace at start-up.
- A multi-root workspace whose roots are in different repositories appears under its first root's project only. That's predictable, and the first root is already the one that names the workspace and seeds its git state.
- Uncommitted work can't be thrown away from Coqo. The user has to commit, stash or use git directly, which is deliberate.
- The sibling folder can surprise someone who expects worktrees inside the repository. The path is shown in the **New worktree** form, and a setting can come later without a protocol change, since `path` is already a parameter.

## Alternatives considered

- **Group by the main worktree's path, or by remote URL.** The path breaks for bare repositories and when the main worktree moves. A remote URL would join unrelated clones of one repository and split a repository with no remote. The common directory is what git itself uses.
- **Run `git rev-parse` per workspace.** It's correct, but it spawns a process for every workspace at start-up. The filesystem walk gives the same answer and is tested against it.
- **Put worktrees inside the repository (`.coqo/worktrees`, like `.claude/worktrees`).** Every tool working on the main worktree would then see nested checkouts. ADR 0027 already had to work around that for the watcher.
- **One workspace with several worktrees as roots.** That would share one chat list, dock and git state between branches, which is exactly the interference worktrees are meant to prevent.
- **`--force` behind a confirmation.** It loses work for good. Refusing costs one `git stash` and keeps "never forces" literal.
