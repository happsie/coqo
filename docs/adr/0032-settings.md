# 32. Settings: a daemon-side service, a schema contributed through the mod API, and a generic settings page

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #26. First step of the mod audit #353; the keyboard shortcuts editor (#354) builds on the page. Design: board 8, [`docs/design/settings.md`](../design/settings.md).

## Context

PLAN §8 puts the user's settings in `settings.toml` in the config directory, and a workspace's in `.coqo/settings.toml`. PLAN §5 lists a "settings schema" among the contribution points. Until #26 there was no settings store:

- The UI read `[files] auto_reveal` once at start (`crates/coqo/src/settings.rs`, #163). A palette command flipped it for the running app only.
- #345 added `[questions] findings_in_gutter` to that stand-in.
- The Claude mod declared `claude.guide`, `claude.plugin` and `claude.acp.plan-logins` in its manifest. Only environment variables could turn them off.
- The theme was always "follow the system".

The mod API already had a `settings` point (one `Setting` per key, typed, with defaults). It had no sections, nothing read values against it, and nothing showed it.

The maintainer wants this for the first release (2026-11-30). It must have a theme choice, the existing keys, and a schema that features and mods contribute to, as a first step of #353.

## Decision

**The daemon owns the settings** (`crates/coqod/src/settings.rs`).

- **Files.** The user's file is `settings.toml` in the config directory: `$COQO_CONFIG_DIR`, else `~/.config/coqo` (`$XDG_CONFIG_HOME`), else `~/Library/Application Support/Coqo` on macOS. It's the directory `keybindings.toml` is in. A workspace's file is `.coqo/settings.toml` in its first root.
- **Layering.** A value in effect is the workspace file's, else the user file's, else the field's default (`GetResult::effective`).
- **User-only settings.** A field can be user-only (`SettingScope::User`). The theme is one: it is not per workspace. A workspace file's value for it is reported and ignored.
- **Validation.** Every file is checked against the schema, with readable problems that carry the key and the line:
  - "`appearance.theme`: `blue` is not one of system, dark, light"
  - "`files.auto_reveel` is not a known setting"
  - "settings.toml is not valid TOML: line 2: …"

  An invalid value is left out, so the next layer applies. A file that stops being valid TOML is reported, and the values it had when it last parsed stay in effect. Nothing panics on a bad file.
- **Live reload.** The service reads each file it follows every 500 ms and compares the text. *Alternative:* `notify` on the files' folders. Editors save atomically (a rename), the folders may not exist yet (`.coqo/`), and there are only a few small files. Polling them is simpler and cannot miss an edit.
- **Writes keep the file as the user wrote it.**
  - `settings/set` edits one key with `toml_edit`: comments, blank lines, order and a value's trailing comment stay.
  - A new key goes into its table (`[files]`), which is appended when it is missing. A nested key gets one header (`[claude.acp]`).
  - `null` removes the key.
  - The write goes through a temporary file and a rename.
  - A file that is not valid TOML is never written over: the error is `SETTINGS_FILE_INVALID`, and the message names the line.
- **Protocol** (`coqo-protocol` `methods::settings`). All three routes need the daemon token; agents don't change the user's settings.
  - `settings/get { workspaces }` returns the sections with their fields, the user file and the asked-for workspaces' files. Each file has its valid values and problems.
  - `settings/set { key, value?, workspace? }`.
  - `settings/changes` is live only, with a generation. It fires on a file edit, a write, or a mod that added or removed settings.
  - New error codes: `UNKNOWN_SETTING` (2201), `INVALID_SETTING` (2202) and `SETTINGS_FILE_INVALID` (2203).
  - The daemon follows a workspace's file once a client asks for it. The UI asks for its open workspaces.
- **Settings read at daemon start.** The Claude mod's options are built before the mods start, so `claude.*` are read from the user's file then (`settings::startup_values`). They apply after the daemon restarts, and the section says so. The environment variables still turn them off.

**The schema is a contribution point of `coqo-mod-api`.**

- A new point, `settings_sections`, takes a `SettingsSection`: an id, a title, an optional description and an optional order. Fields stay on the existing `settings` point.
- A field belongs to the section whose id is the longest prefix of its key (`files.auto_reveal` is in `files`). A field with no such section gets a section named after its key's first part.
- In code, a feature registers a section and its fields in one call. The call checks that each key is in the section and that each default is valid:

  ```rust
  cx.register_settings(
      SettingsSection::new("files".parse()?, "Files").with_order(20),
      [Setting::boolean("files.auto_reveal".parse()?, "Reveal the active file in the tree", false)
          .with_description("…")],
  )?;
  ```

- `Setting` gained builders (`boolean`, `enumeration`, `string`, `integer`, `number`, `with_description`, `with_bounds`, `user_only`), a `scope`, `default_value()` and `check_value()`. The daemon validates with `check_value()`, so a mod's schema and the check of its values can't drift apart.
- A manifest declares the same contributions as `[[contributes.settings_sections]]` and `[[contributes.settings]]`. The Claude mod's manifest does this.
- The UI's own features register through a built-in core mod in the daemon, `coqo.workbench` (`crates/coqod/src/settings/workbench.rs`):
  - Appearance (`appearance.theme`, user-only)
  - Files (`files.auto_reveal`)
  - Questions and notes (`questions.findings_in_gutter`, #345)
  - Updates (`updates.check`, `updates.channel`, user-only; the updater of ADR 0031 still reads them itself before each check)

  Mods under `mods/` call the same `ModContext::register_settings`. The mod boundary is unchanged.

**The UI** (`crates/coqo/src/settings/`).

- `Settings` is a global that holds the daemon's last `GetResult`. Views read typed values from it (`settings::get(cx).bool(AUTO_REVEAL, Some(&workspace))`).
- `settings::set` applies a value at once and writes it through the daemon. A failed write shows on the page, and the daemon's values come back.
- Until the daemon answers, the user's file is read directly, so the first frame has the right theme.
- `settings::init` (called from `main`) follows `settings/changes` and applies `appearance.theme` live to every window.
- The palette's "Toggle auto-reveal" now writes the setting.
- **The Settings tab** (board 8) has a section list on the left and the chosen section on the right, rendered generically by field kind:
  - boolean: a switch
  - enum: a segmented choice
  - text, number and list: a text field, saved when the typing pauses
- A User / Workspace switch picks the file the page writes. Problems show as a banner (file-level and unknown keys) or under their row.
- "Open settings" in the palette and `Ctrl/Cmd+,` open the tab.
- **Pages of their own** join the section list with `settings::page::register_page(cx, CustomPage::new(id, title, order, build))`. The same id replaces a page. "Keyboard shortcuts" (`KEYBOARD_PAGE`, order 900) is the shortcuts editor (#354, `settings/keybindings.rs`).

## Consequences

- One place owns the settings files and checks them. Mods and features add settings with one call, and those settings show up on the page with nothing written for the UI.
- `#163`'s stand-in is gone. `auto_reveal` and `findings_in_gutter` now persist, and the theme can be chosen.
- `claude.*` settings apply after the daemon restarts, not live. A mod-side settings service (`coqo.settings@1`, values and change events for mods) is the next step of #353. With it, mods read values at run time instead of at construction.
- Polling costs one small file read per followed file every 500 ms. That's negligible, and it keeps the daemon awake no more than its other timers do.
- `serve_with` takes the config directory, so in-process test daemons never read or write the user's real `~/.config/coqo`.
- Keys that no mod declares (a disabled mod's settings, say) are reported as unknown on the page but stay in the file untouched.
