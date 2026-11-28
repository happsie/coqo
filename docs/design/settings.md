# Settings

The Settings tab (design board 8, issue #26, [ADR 0032](../adr/0032-settings.md)). The code is in `crates/coqo/src/settings/`: `page.rs` is the tab and `mod.rs` holds the values and the sync with the daemon. The daemon side is `crates/coqod/src/settings.rs`.

## Opening it

- "Open settings" in the palette (Preferences), or `Ctrl+,` (`Cmd+,` on macOS).
- A second open brings the existing tab forward.
- `OpenSettingsAt { section }` opens the tab at a section.
- The page never explains keys in its text.

## Layout

The tab is filled with `ground`, with 10 px padding and a 10 px gap. It has two columns.

**The section list** is a `panel` card 240 px wide and full height, with 14 × 10 px padding.

- "Settings" heads it: 15 px, semibold.
- Then one entry per section, 8 × 10 px padding, radius 10.
  - The chosen entry is `raised`, `text` and semibold.
  - The others are flat, in `text_muted`.
- The order is the sections' own order: Appearance 10, Files 20, Questions and notes 25, Claude Code 30, Updates 40, mods without an order after them by title. Pages of their own are placed by their order. Keyboard shortcuts is 900.
- At the bottom, in `text_xs` and `text_subtle`: "Workspace overrides are stored in `.coqo/settings.toml`", with the path in mono.

**The main column** scrolls. From the top:

1. **Scope bar.** A segmented track (`panel` fill on the ground, radius 12, 3 px inset) with **User** and **Workspace · <name>**. The chosen segment is `raised`. The Workspace segment is dimmed while the window has no workspace. The edited file's path is on the right, in mono `text_xs` `text_subtle`.
2. **Problem banner**, only while the edited file has problems that aren't shown on a row:
   - It is for file-level problems (not valid TOML, unreadable) and unknown keys.
   - Rose at 14 % on `surface`, a 35 % rose hairline, radius 12, 14 × 10 px padding.
   - One line per problem, "Line 7: `files.auto_reveel` is not a known setting".
   - For a broken file it adds, muted: "Coqo keeps using the values from before the error."
   - A failed write shows the same banner, "Couldn't save: …".
3. **The section card.** A `panel` card, 18 px padding.
   - The title is 16 px semibold, with the description under it in `text_muted`.
   - One row per field, 10 px vertical padding. On the left, the title in `text`, then the description in `text_sm` `text_muted`. The control is on the right.
   - **Reset** (`text_xs`, `text_subtle`) sits before the control when the edited file sets the value. It removes the key from the file.
   - In Workspace scope, a field the workspace doesn't set says "From your user settings". A user-only field (the theme) says "Only in your user settings" and has no control.
   - A problem with the row's key shows under it in `rose`.

## Controls by field kind

| Kind | Control |
|---|---|
| boolean | Switch, 42 × 24, radius 12. On: `sage` track, `ground` knob on the right. Off: `border_strong` track, `text_muted` knob on the left. |
| enum | Segmented choice on a `ground` track (board 8's "Mode"). Each value is a segment, labelled with the value capitalised (`on-save` → "On save"). |
| string, integer, number, string list | A single-line field, 260 px, `ground` fill, radius 9. It saves 0.7 s after the typing pauses, or on submit. A value that doesn't parse ("Enter a whole number.") shows under the row and isn't saved. Lists are comma-separated. |

Every change applies at once (the theme switches live) and is written to the file through the daemon. The file keeps its comments and order.

## Pages of their own

`settings::page::register_page(cx, CustomPage::new(id, title, order, build))` adds a page to the section list. `build` makes its view once per tab, and the same id replaces a page. The page fills the main column; there is no scope bar on it. The keyboard shortcuts page (`KEYBOARD_PAGE`) is registered this way by `settings/keybindings.rs`.

## Keyboard shortcuts page

Opened from the section list, by "Open keyboard shortcuts" in the palette (Preferences), or by `OpenSettingsAt { section: "keyboard" }`. The command has no default keys: `Ctrl/Cmd+K Ctrl/Cmd+S` is not free, because `Ctrl/Cmd+K` is bound on its own (the palette's search). A user can bind it on this page.

One `panel` card, 18 px padding, from the top:

1. **Header.** "Keyboard shortcuts" (16 px semibold) with a muted line under it, and **Reset all** (a raised button) on the right while the file overrides anything. It asks first, in a rose bar: "Reset every shortcut to its default? Your own are removed." with **Reset all** (primary) and **Cancel**.
2. **Problem banner**, as on the other pages (rose on `surface`): a failed write ("Couldn't save: …", for instance when the file is not valid TOML, which is then left untouched) and what the registry reports (bad keys, unknown ids, keys bound twice in one context).
3. **Search** on a `ground` field: "Search commands or keys". It matches the title, the category, the id and the keys as shown (`ctrl k`).
4. **The list**, every command of the registry (palette-hidden ones too), grouped by category under a `section_label`, then by title. A row has 12 × 8 px padding, radius 12:
   - the title and, under it in mono `text_xs` `text_subtle`, the command id;
   - its keys, one `kbd` cap per key sequence (a chord shows as `Ctrl K Ctrl S`), or "Unbound";
   - where they come from: a **DEFAULT** tag (`surface_strong`) or a **YOURS** tag (sage at 25 % on `surface`);
   - **Reset** (`text_xs`, `text_subtle`) when the file sets the command, and **Change**.
5. The file's path, in mono `text_sm` `text_subtle`.

**Recording.** **Change** turns the row into a `panel_raised` card with a sage hairline, with the state label "Recording…" in sage and the keystrokes so far as caps. While it lasts, every keystroke is taken by the page before any binding or text field sees it (a GPUI keystroke interceptor), so nothing fires, chords included; a modifier alone is ignored. `Escape` on its own cancels. **Save** writes the keys; **Cancel** drops them. Change replaces all of the command's keys with the recorded sequence.

**Conflicts.** If another command in the same key context has the recorded keys, a rose bar in the row says "Ctrl K is already used by Search" with **Replace** (primary) and **Cancel**. Replace removes those keys from the other command (it keeps its other keys) and gives them to this one. Nothing is written before that choice. The same keys in different contexts are no conflict.

**The file.** `keybindings.toml` in `$COQO_CONFIG_DIR` is edited with `toml_edit`: only the entries changed are touched, and the file's comments, order and other entries stay. Keys are written in the file's syntax (`mod+k mod+s`, `mod` being `cmd` on macOS and `ctrl` elsewhere), a single sequence as a string and several as a list. Reset removes the command's entry; Reset all removes every entry. After each write the registry reads the file again (`registry::load_user_keys`), so the bindings apply at once.

## Screenshots

Offscreen, from `settings::tests::settings_screenshots` (`#[ignore]`, needs a GPU), in dark and light, for Appearance and Claude Code: [`settings-dark.png`](screenshots/settings-dark.png), [`settings-light.png`](screenshots/settings-light.png), [`settings-claude-dark.png`](screenshots/settings-claude-dark.png), [`settings-claude-light.png`](screenshots/settings-claude-light.png).

Keyboard shortcuts, with a recording row that has a conflict and a command with the user's own keys: [`keyboard-dark.png`](screenshots/keyboard-dark.png), [`keyboard-light.png`](screenshots/keyboard-light.png), from `settings::keybindings::tests::keybindings_screenshots` (`#[ignore]`, needs a GPU).
