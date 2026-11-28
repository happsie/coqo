# Design notes

The visual language and the per-surface descriptions live in this folder. PLAN §14 explains how they relate to the design board.

## Rules

- **No key hints in UI text.** Nothing on screen says which keys to press: no "Enter to send", "Esc to close", "Alt-click: …" or `↑↓` legends in footers, placeholders, tooltips, banners, notes or empty states, and no key caps beside an instruction. Keyboard discoverability is a separate feature.
  - **Allowed:** the key column of the command palette and the keybindings editor, which show bindings as data, and an accelerator on a control, such as the top bar's search or a menu item.
  - **Not affected:** the bindings themselves. Keys still work as before; they are just not explained in the UI.
  - A note whose message is still needed keeps its sentence and drops the key: "Save the file first: review comments are pinned to the saved file."
  - A guard test (`crates/coqo/src/no_key_hints.rs`) fails when a known hint phrase appears in a source string literal.
