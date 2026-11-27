# Coqo patches to gpui-base

This directory is **gpui-base 0.11.0** from crates.io, the crate that holds gpui-component's
editing engine (`EditorState` is `gpui_base::input::EditorState`, re-exported by gpui-component
and gpui-kit). It is wired into the build with `[patch.crates-io]` in the root `Cargo.toml`, so
the exact `gpui-base = "0.11.0"` that `gpui-kit = "=0.11.0"` pulls in is this copy. Why Coqo
patches it, instead of using public APIs, is in [ADR 0008](../../docs/adr/0008-editor-remote-edits-patch.md).

- **Upstream:** <https://github.com/longbridge/gpui-kit>, `crates/base`, commit
  `e97fe438129c5e4ccab504f2e6fafd9a4e396bba` (from `.cargo_vcs_info.json`).
- **Package:** `gpui-base-0.11.0.crate`, sha256
  `604d277dd339c63d13a3016d22a84622cc7511fdfd8e88573c0709088ede3500`.
- **Licence:** Apache-2.0, unchanged ([`LICENSE-APACHE`](LICENSE-APACHE)). Modified files carry a
  "Coqo patch" comment, as Apache-2.0 §4(b) asks.
- **Removed from the package:** only `Cargo.lock` (unused for a dependency, and it would show up
  as a second lockfile in dependency scanners) and cargo's `.cargo-ok` marker.

The vendored crate is not a workspace member (`exclude = ["third_party"]`), so Coqo's lints,
`clippy -D warnings` and `cargo test --workspace` don't run on it.

## The patch

Four changes, all additive. No upstream line is modified, so the patch carries over to a new
release by copying two files and two `mod` lines.

1. **New file `src/input/base/remote_edits.rs`.** An extra `impl<M: InputModeKind>
   InputBaseState<M>` block with three public methods:
   - `apply_remote_edits(&mut self, edits: &[(Range<usize>, String)], window, cx)`: apply edits
     made by someone else (UTF-8 byte ranges, pre-edit coordinates, disjoint). It runs the text
     change through the same `replace_text_in_ranges` path as a local multi-cursor edit, so folds,
     decorations, diagnostics, search and highlighting update exactly as for a local edit, with
     these differences:
     - undo recording and event emission are switched off for the call (`undo_manager.set_ignoring`
       and `emit_events`, the same switches `set_value` uses), so nothing enters the undo history
       and no `InputEditsEvent` or `InputEvent::Change` is emitted;
     - the undo history is then cleared, because its entries hold absolute offsets that can't be
       rebased through someone else's edit (Coqo replaces this history with its own per-side undo);
     - every selection is mapped through the edits (a cursor, or a selection start, at an insert
       point stays left of the inserted text; a selection end sticks right; a position inside
       replaced text collapses to the replacement's start, or for a selection end its end), instead
       of collapsing to one cursor per edit;
     - `last_selected_range` is moved with the active selection, so the next frame doesn't scroll
       the view to follow a cursor the user didn't move;
     - it works on read-only and disabled inputs, like other programmatic edits;
     - an IME composition in progress is committed first (as `unmark_text` does).
   - `selected_ranges(&self) -> Vec<Range<usize>>`: every selection, the active one first. Upstream
     only exposes the active one.
   - `is_composing(&self) -> bool`: whether IME text is marked.
   - A unit test for the offset mapping, for an upstream PR. The package's own test suite doesn't
     build outside the upstream repository (it includes files from it), so in Coqo the patch is
     covered by the `coqo-ui` editor tests (`crates/ui/src/editor/tests.rs`).
2. **`src/input/mod.rs`:** one `mod remote_edits;` declaration (with a `#[path]` attribute, like its
   neighbours) after `mod rope_ext;`.
3. **New file `src/input/base/overlay_geometry.rs`** (issue #74, the question markers and the
   popover anchored to code): another `impl<M: InputModeKind> InputBaseState<M>` block with two
   read-only methods, for views drawn over the editor:
   - `gutter_width(&self) -> Option<Pixels>`: the last layout's `line_number_width` (decoration
     strip, line numbers, fold icons and the gap before the text). Upstream doesn't expose it, and
     it depends on the digit count, folding and decorations.
   - `line_top(&self, line) -> Option<Pixels>`: the top of a buffer line relative to
     `text_bounds()`, through the display map (so folds count), at the scroll offset the next frame
     lays out with (`deferred_scroll_y_f64`, else the effective one), so an overlay doesn't lag a
     scroll by a frame. `None` before the first layout or while the line is folded away.
4. **`src/input/mod.rs`:** one `mod overlay_geometry;` declaration (with a `#[path]` attribute)
   after `mod remote_edits;`.

Coqo code reaches these only through `coqo_ui::editor::CodeEditor`.

## Upgrading gpui-kit

When `gpui-kit` moves past 0.11.0:

1. Check whether upstream gained an equivalent API. If it did, delete this directory and the
   `[patch.crates-io]` entry, and port `coqo_ui::editor` to it.
2. Otherwise replace this directory with the new `gpui-base` package (same version `gpui-kit`
   pins), copy `remote_edits.rs` and `overlay_geometry.rs` back, re-add the two `mod` lines,
   update the commit, checksum and version above, and fix any renamed internals
   (`replace_text_in_ranges`, `selections`, `undo_manager`, `emit_events`, `last_selected_range`,
   `ime_marked_range`, `last_layout.line_number_width`, `display_map`, `deferred_scroll_y_f64`,
   `effective_scroll_y_f64`).
3. Run `cargo tree -i gpui-base` to confirm the patch is used (cargo warns and ignores a patch whose
   version no longer matches), then the `coqo-ui` editor tests and `cargo deny check`.
