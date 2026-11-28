# 23. Commands, the command palette and keybindings

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #25 (epic #15; PLAN §5 contribution points, §6 `workbench`, §8 config; board 1)

## Context

PLAN §6 gives the `workbench` mod a command palette and keybindings, and PLAN §5 lists commands and keybindings among the contribution points. When #25 was picked up, Coqo had about thirty shortcuts. Each view bound its own keys in its `init`, through `coqo_ui::keys::binding` (#266), and the key strings lived in per-module constants. Nothing listed them, the user couldn't change any of them, and the top bar's command search did nothing. Mods can already contribute commands (`hello.greet`), but those run in the daemon, and the UI never saw them.

Five things had to be settled:

- what a command is in the UI, and who owns its key bindings;
- how the palette decides what to list;
- how the palette leaves room for the file finder (#162) and other modes;
- how the user rebinds keys;
- how commands from daemon mods reach the palette and the keyboard.

## Decision

**A command is an id, a title, a category, default keys, a key context and a GPUI action.**

- The registry is `crates/coqo/src/commands/registry.rs`, a GPUI global. Views register their commands in their `init`, in place of the `bind_keys` calls they made before.
- The action is the handler. Running a command gives the focus back to where it was before the palette opened, then dispatches the action there. The view that handled the shortcut handles the palette run with the same code.
- The key context (`DiffReview`, `FileEditor`, `Dock`) does two jobs: it is where the keys are bound, and it is where the command is available.
- Ids are mod-style contribution ids (`diff.next_hunk`, `command_palette.open`). The user's file names commands by these ids.

**The registry owns its commands' bindings.**

- `apply` rebuilds the keymap. It drops every binding whose action belongs to a registered command, keeps the rest in their order (the component library's keys, and list navigation that isn't a command), and binds each command's effective keys again.
- The effective keys are the user's keys for that id, or else the defaults. The user's keys are bound last, so at the same context depth they win.
- Every binding goes through `coqo_ui::keys::load`, the runtime twin of `keys::binding`. Plain keys still never fire inside a text field (#266), and the guard test still covers them.
- Gpui's `Unbind` and `NoAction` don't fit. They would disable keys rather than move a command, and a reload would pile them up.

**The palette lists what would work where the focus was.**

- When the palette opens, it captures a `PaletteScope`: the focus, the key contexts on its path, and the action types that path handles (`window.available_actions`).
- A command is listed when its context matches and its action is handled there. A diff command doesn't show in a chat. A dock command doesn't show while the focus is in the sidebar.
- A mod command is always listed, because the app handles `RunModCommand` itself.
- The palette's own openers are hidden from the list.

**Providers are the palette's modes, picked by prefix.**

- A `PaletteProvider` has a prefix, a placeholder, `search(query, scope) -> Task<Vec<PaletteItem>>` (so the daemon can do the work) and `confirm(item, scope)`.
- `>` is the commands. A provider registered with the empty prefix is the default mode. #162 registers the file finder there, bound to `Ctrl/Cmd+P`. `@` and `#` are free for symbols and the rest.
- Until a default provider exists, the commands serve the default mode as well. So `Ctrl/Cmd+K` and a click on the top bar's search open a working palette now.
- `Ctrl/Cmd+Shift+P` opens straight on `>`.

**User keybindings live in `keybindings.toml`.**

- The file is in the config directory (PLAN §8): `~/.config/coqo` on Linux, `~/Library/Application Support/Coqo` on macOS, or `$COQO_CONFIG_DIR`. It is one table of command id to keys:

  ```toml
  "command_palette.open" = "mod+shift+p"
  "diff.next_hunk" = ["j", "down"]
  "terminal.new" = []        # unbound
  ```

- Keys use the mod manifest's syntax (`contrib::Keybinding`): `+` inside a chord, a space between chords, and `mod` for `cmd` on macOS and `ctrl` elsewhere. Users and mods write keys the same way. GPUI's `ctrl-shift-p` is accepted too.
- A command keeps its context. Rebinding a diff command doesn't make it work in a chat.
- The file is read at start. **Reload keybindings** (a palette command) reads it again.
- A bad file is never fatal. Bad TOML, bad values, unknown modifiers, unknown ids and conflicts are skipped and logged once (`coqo: keybindings: …`), the rest still apply, and the palette shows a sand line counting them.
- A conflict is two commands bound to the same keys in the same context. The last one bound wins.
- Unknown ids are reported only once the daemon's commands are in, so a mod command bound before the daemon answers isn't flagged.

**Mod commands cross the protocol as `commands/list` and `commands/run`.**

- `commands/list` returns each command contributed to the `commands` point, with the keys mods bind to it through `keybindings`.
- `commands/run` runs a command through `ModManager::run_command`, activating its mod first. It returns `UNKNOWN_COMMAND` (1901) or `COMMAND_FAILED` (1902).
- Both are daemon-token only. Agents don't run the user's commands.
- The UI lists the commands on every connect. It registers each one as a `RunModCommand { id }` command in the mod's category, with the mod's keys (`mac` on macOS) and the user's overrides.
- A mod's `when` is read as a key context predicate.
- A mod command whose id clashes with a core command is skipped.

**Recently used commands come first.** The last 30 ids are kept in `<data dir>/ui/recent-commands.json`. With an empty query they lead the list, labelled "recently used". With a query they get a small ranking bonus.

## Consequences

- New shortcuts are commands. A view adds a `Command` to `commands::register` in place of `cx.bind_keys`. It then shows in the palette, can be rebound, and is checked for conflicts. Keys that aren't commands (list navigation, a permission prompt's Enter/Esc, the terminal's pass-through `NoAction`s) still use `keys::binding`.
- The dock's keys are bound in `coqo-ui` and re-registered as commands by `coqo`. That crate can't depend on the registry, so the defaults come from the public `coqo_ui::dock::KEYS`.
- `available_actions` can't build actions that carry data, so a data action handled only by a global listener would be hidden. That's why mod commands skip the action check. A core command with data would need the same treatment.
- The file is not watched. Reloading is a command, which keeps start-up cheap and avoids half-written files. A settings UI (board 8's Keymap page) can write the same file later.
- There's no chord display beyond the first binding: the palette shows a command's first key sequence.
