# 4. Workbench layout: an own split tree, saved through a layout store

- **Status:** Accepted
- **Date:** 2026-11-27

## Context

Issue #24 needs the workbench's main tab area: tab groups with drag and drop, split right and down, resizable splits, generic tabs (files, chats, diffs, PRs, traces), and a layout that survives a UI restart. Design board 1 draws every region as a floating rounded panel on the ground with 10 px gaps (PLAN §14), so a split must show two panels with a gap between them, not one panel with a divider.

gpui-kit ships a complete dock engine (`gpui_base::dock`: `DockArea`, `PaneTree`, `TabGroup`, renderer traits, `DockAreaState` persistence) with a skin in `gpui_component::dock`. We read it first, since ADR 0002 prefers reusing gpui-component behind `coqo-ui`:

- Its split slots sit flush against each other with a 1 px divider; the divider's hit area is an absolutely positioned overlay. No renderer hook adds space between slots. A tab group's frame is forced to `size_full` and `overflow_hidden`, so a margin overflows and a panel shadow inside padding gets clipped. Getting gaps meant painting the panel in two halves (tab bar and content) or other tricks that break the drop shadow and the drop hit areas.
- It is about 10,000 lines that change with every gpui-kit release (ADR 0002 notes weekly churn), and its persisted schema (`DockAreaState`, panel names, pixel sizes) would become Coqo's on-disk format.
- The daemon, which should own the layout per workspace (PLAN §4.2, §8), doesn't exist yet (#20, #22, #27).

## Decision

- **Own layout engine in `coqo-ui` (`coqo_ui::dock`).** The layout is a plain serde tree (`PaneTree` of `Split` and `Group` nodes, sizes as fractions) with pure, unit-tested edits: insert, move, split, close, resize, normalize. `Dock` projects it into elements: each group is a `widgets::panel`, children of a split are flex items separated by gap-sized elements that are also the resize handles, and tabs use GPUI's own `on_drag` / `on_drag_move` / `on_drop`. Normalization closes empty groups (except the last), collapses one-child splits and flattens same-axis nesting.
- **Generic tabs.** A tab is any view implementing `TabItem` (kind, title, icon, key, saved state, optional custom label and focus handle), held as `AnyTab`. `register_tab_kind` maps a kind to a builder that recreates a tab from its saved state. An unknown kind becomes a `MissingTab` that keeps its state, so a layout survives a missing mod. Splitting duplicates the active tab through its builder.
- **Persistence behind a seam.** `WorkbenchLayout` (version, sidebar width and pane, dock tree) is saved through a `LayoutStore` trait. Until the daemon can hold it, `JsonFileLayoutStore` writes `<data dir>/ui/layouts/<workspace>.json` atomically, keyed by `"default"`. The shell saves 300 ms after the last change and on quit. #140 moves this into the daemon (see the update below).
- **Chrome slots.** Top bar, status bar and sidebar panes are global registries of render callbacks (`coqo_ui::slots`), so mods fill them without the shell knowing them.

## Consequences

- The look matches board 1 exactly, and the layout format is ours: small, readable JSON that we version (`LAYOUT_VERSION`); an unknown version falls back to the default layout.
- We maintain about 1,000 lines of layout code instead of reusing gpui-kit's dock. Docking into side regions, zoom and keyboard focus traversal between groups aren't built yet; add them here when needed.
- gpui-kit's `gpui_base::dock` stays available if the trade-off changes (for example if it gains gap-aware splits).
- Layouts are UI-side files until #140; a second window on the same workspace doesn't share a live layout yet.

## Update (#27, #140): layouts live with the workspace in the daemon

- Each window has its own workspace and dock. Its layout is keyed by the **workspace id** and kept in the daemon as the workspace state entry `ui.layout` (`workspace/setState`, stored in `coqo.db`; the daemon treats it as opaque, versioned JSON). The window fetches it with `workspace/getState` when it opens or switches to the workspace, so the layout follows the workspace into any window and across UI restarts.
- `DaemonLayoutStore` (`crates/coqo/src/layout.rs`) is the app's `LayoutStore`. It sends `workspace/setState` when attached and writes the `JsonFileLayoutStore` file while the daemon is unreachable; a file found on load is newer than the daemon's copy, and the next save after reattaching moves it into the daemon and deletes it. `LayoutStore` gained `remove` and `flush` (awaited on quit, so the last save reaches the daemon).
- With no workspace open, a window saves under `"default"` in the file store. The first workspace opened that has no layout of its own adopts that layout and the file is removed: an old `default.json` is imported once.
- A workspace is shown in at most one window (opening it again focuses that window), so two windows never edit one layout; live layout sync between windows isn't needed.
