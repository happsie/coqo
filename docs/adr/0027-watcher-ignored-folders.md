# 27. The file watcher skips ignored folders, nested repositories and build caches

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #308 (follows #22, #288; shares its rules with #162 and, since #318, search)

## Context

With Coqo open on its own repository while six agents built in `.claude/worktrees/*/target` (about 25 GB each), the idle `coqod` used 12–23% CPU. The worktrees weren't gitignored then (#307), but that wasn't the real cause. The watcher put `notify`'s recursive watch on every root, and on Linux that means one inotify watch on every folder below it, ignored ones included. Every file a build wrote woke the daemon, and the event was dropped only afterwards, when batching. Two things had to be settled:

- which folders count as "not ours", even when no ignore file says so;
- how to keep the OS from watching inside them, not just drop their events.

## Decision

**Folders that stand apart.** Below a root, a folder is ignored, whatever the ignore files say, if it:
- holds a `.git` file or folder: a nested clone, a submodule or a worktree (`git worktree add`, `.claude/worktrees/*`). It is its own project. If the user wants it, they add it as a root of its own, and then its own rules apply inside it.
- holds a `CACHEDIR.TAG` file (the [cache directory tagging](https://bford.info/cachedir/) convention). Cargo tags `target/`, and other tools tag their caches too.
- is named `node_modules`.

A root itself never stands apart, so the root's own `.git` doesn't count. The rule is `coqo_mod_api::workspace::stands_apart` (it was in `coqod::workspace::ignores` until #318), so the kernel and mods share one copy that can't drift. In the daemon it is checked in `IgnoreChain::is_ignored` for folders and in `IgnoreCache::chain` for every ancestor, so every user of the ignore cache gets it: `fs/list` and `fs/stat` (`ignored: true`), the watcher, and the file index of #162. It costs up to two `stat`s per folder, made only for folders the ignore files keep.

**One watch per kept folder (Linux).** The watcher walks each root itself and puts a non-recursive inotify watch on each folder the rules keep. It never watches inside `.git`, an ignored folder or a folder that stands apart. An ignored folder is still an entry of a watched folder, so the tree sees it appear and go away. After each debounce window, before batching, the watcher brings the set up to date:
- A new folder, or one moved in, is walked and watched. Its entries are reported as changed, because files written before its watch was in place made no events.
- A removed folder, or one moved away, loses its watches (`notify` drops them), and whatever is at that path now is walked again.
- A folder is re-synced when its rules may have changed: when a `.gitignore` in it is written, or a `CACHEDIR.TAG` or `.git` appears in it. That covers Cargo creating `target/` and only then tagging it.
- After lost events (a queue overflow, or more than `max_paths` changes in a window), every root is re-synced and every watch renewed.

Folders inside a deeper root are left to that root, and its rules decide for them. Symbolic links aren't followed. A link to a folder inside the workspace is watched where it points: `resolve_file` already keys buffers by the real path as well. A link out of the workspace isn't listed, so it isn't watched either. If the OS runs out of watches (`fs.inotify.max_user_watches`), the watcher logs one warning and keeps what it has.

**macOS and Windows keep one recursive stream per root.** FSEvents and `ReadDirectoryChangesW` cost nothing per folder, so per-folder watches would only add work there. Events from inside ignored folders are still dropped when batching, as before.

**The tree.** `fs/list` still lists ignored folders, dimmed, and they can be expanded to look inside. Nothing inside them is watched or indexed, though, so an expanded ignored folder doesn't refresh live. That was already true for gitignored folders, whose events were dropped. Search (#45, `mods/search`) walks with the `ignore` crate's own gitignore rules. As a mod it can't call into the kernel's ignore code, so the rule lives in `coqo-mod-api`, the one Coqo crate mods may use (#318). It is a pure function over `std::path` that adds no dependency, and the search walk applies it in `filter_entry` to every folder below a root. Search and the tree therefore leave out the same folders, and a nested repository added as a root of its own is searched under that root.

## Measurements

These are for a private release `coqod`, with a root holding 50 source files and a `target/` (with `CACHEDIR.TAG`) of 3,000 folders and 30,000 files. A script read and rewrote every file in `target/` twice (60,000 writes, about 4 s), the way a build would. The inotify watches were counted from `/proc/<pid>/fdinfo`, and CPU is the daemon's user and system time.

| | inotify watches | CPU during the rebuild | `fs/changes` paths sent |
|---|---|---|---|
| Before, `target/` not ignored | 3,313 | 1.5 s (about 35%) | 60,000 |
| Before, `target/` in `.gitignore` | 3,313 | 1.2 s (21–30%) | 0 |
| After, either way | 7 | 0.0 s | 0 |

The idle CPU, with nothing writing, was 0% before and after. Subscribing to `fs/changes` took 87 ms before and 1 ms after. A write to a source file was still reported in every run.

## Consequences

- An idle daemon stays idle while agents build in worktrees or `target/` folders below a root, even when the repository doesn't ignore them.
- A nested clone or worktree gets no live tree updates, and the file index (#162) leaves it out, unless it is added as a root.
- Watch management is now Coqo's code, not `notify`'s. It is covered by unit tests with a fake OS watcher and by end-to-end tests on Linux.
- A folder that is un-ignored by an edit to `.git/info/exclude` or the global excludes is picked up only when its own entry changes, or after an overflow. Those files are rarely edited, and `.git` itself isn't watched.
