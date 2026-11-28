# Git pane and diff tabs

This page covers the Git sidebar pane (board 1's "Git" tab) and the `git-diff` tab kind, from PLAN §7.7, #91 and #93.

- **Code:** `crates/coqo/src/git/`. It moves into the git mod once the mod API carries UI contributions (#147).
- **Data:** the daemon's `git/*` methods and `git/changes` (ADR 0018).
- **Screenshots:** offscreen renders from `git_screenshots`.

![The Git pane with a staged diff open](screenshots/git-dark.png)

## The pane

From top to bottom:

- **Root chips:** shown only when the workspace has more than one root that is a repository. The chip for the root being shown is raised.
- **Branch button:** a raised button with the `git-branch` icon, the branch name in semibold `text`, and `↑n ↓m` against the upstream in `text_subtle` at `text_xs`.
  - A detached `HEAD` shows `detached at <short oid>`.
  - Clicking the button opens the picker below it:
    - the local branches, with a sage check on the current one; clicking another branch switches to it;
    - a "New branch name" field; `Enter` creates the branch at `HEAD` and switches to it.
- **Commit box:** a `PromptInput` (2–8 rows) on `surface`, with the field radius and a `border` hairline.
  - `Enter` commits and `Shift+Enter` adds a newline.
  - Below it is the button:
    - With something staged and a message typed, it is the sage primary button "Commit N files".
    - Otherwise it is raised, in `text_subtle`.
- **Groups:** an upper-case section label with the count (`STAGED · 3`, `CHANGES · 3`, and `CONFLICTS · n` only when there are conflicts), plus a "Stage all" or "Unstage all" link in `text_subtle` on the right.
- **File rows:**
  - Sizing: 26 px tall, `text_sm`, radius 9, with a `surface` hover.
  - Status letter: mono semibold, 14 px wide. `M` is `sand`, `A` is `green`, `D` and `U` are `rose`, `T` is `lavender`, and `?` is `text_subtle`.
  - After the letter comes the file name in `text`, then its folder in `text_subtle` at `text_xs`, then a 20 px `+` (stage) or `−` (unstage) button.
  - Clicking a row opens its diff tab.
- **Stashes:** `STASHES · n`, a "Stash changes" link (untracked files included), and one row per stash with Apply, Pop and Drop links.
- **Errors:** a failed write shows git's message on a `rose` 12% band above everything else. Clicking the band dismisses it.

The pane keeps itself current from `git/changes`, which covers edits, `git` runs in a terminal and other tools. A window's `GitModel` holds the state, and `GitModel::letter(root, path)` gives the same letters to the file tree (#163).

## Tab badge and status bar

These describe the root the pane shows (`GitModel::shown_root`: the picked root chip, else the first repository). Both update live, even while another sidebar pane is open (#227, `crates/coqo/src/git/chrome.rs`).

- **Tab badge:** the sidebar's "Git" tab reads "Git N", where N is the number of changed files (staged, unstaged, untracked or conflicted, each file once) in `sand`, 4 px after the title. A clean tree, or a folder that isn't a repository, shows no number.
- **Status bar:** the `branch` item at the start of the status bar shows the `git-branch` icon and the branch, with `↑n ↓m` against the upstream (`detached at <short oid>` for a detached `HEAD`). Without a repository it reads "No repository".

## Diff tab

The `git-diff` tab kind shows a root's changes in the review surface of board 4 ([diff.md](diff.md), #93): one tab for the unstaged changes (index → working tree, untracked files included) and one for the staged ones (`HEAD` → index).

- **Opening:** clicking a file row in the pane opens its side's tab, or switches to it, and shows that file.
- **Label:** the `git-branch` icon, "Changes" or "Staged changes", and `viewed/total` in `text_subtle`.
- **Staging:** hunk buttons and line selections stage (in "Changes") or unstage (in "Staged changes") through `git/stage` / `git/unstage`.
- **Updates:** the tab reloads when `git/changes` reports the sides it shows.
