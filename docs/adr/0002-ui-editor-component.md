# 2. UI editor component: gpui-component's editor, extended by Coqo

- **Status:** Accepted
- **Date:** 2026-11-26
- **Spike:** S4, issue #11. Prototype in [`spikes/s4-gpui/`](../../spikes/s4-gpui/).

## Context

PLAN §3.1 picks GPUI plus gpui-component for the UI, and the code editor is the riskiest part of that choice. Spike S4 asks whether gpui-component's editor can carry what the design boards need: a 100k-line file, 500 decorations, popovers anchored to code, inline ghost values, a side-by-side diff of a 5k-line file, and multi-cursor editing, within the PLAN §1 budgets. The options were:

1. use gpui-component's editor as-is;
2. use it and extend it;
3. build our own editor element on a rope plus tree-sitter (the S4 fallback in PLAN §11).

## What we tested

A standalone GPUI app (`spikes/s4-gpui`, not part of the workspace) on **gpui-kit 0.11.0**, gpui-component's umbrella crate. It has three scenes plus a scripted benchmark (`--bench`) and an offscreen screenshot mode (`--shot`):

- **Editor:** a generated 100k-line Rust file (3.25 MB) with tree-sitter highlighting, plus 500 decorations: 200 range fills or frames, 150 wavy-underline text decorations, 100 gutter marks and 50 overview-ruler marks. Ghost values sit after line ends, and an agent-question popover is anchored to a code range.
- **Diff:** a 5,008-line file with an agent-style patch (1,158 patch lines, about 100 hunks) in split mode.
- **Zen:** the board 1 chrome (rail, files, editor, chat) in the PLAN §14 palette, with floating 16 px panels, 10 px gaps, soft shadows and raised buttons.

**Method:**
- **Edits:** the benchmark drives edits as real keystrokes (`Window::dispatch_keystroke`) inside GPUI's next-frame callbacks. *Edit to paint* runs from the keystroke to the end of the frame's paint pass, recorded by a canvas painted last. *Edit to next frame* also includes waiting for the next vsync.
- **Scrolling:** a scripted scroll moves 37 px every frame.
- **Memory:** read from `/proc/self/status` and `smaps`.
- **Machine:** Linux (Fedora 44 / KDE Plasma, Wayland), NVIDIA RTX 3060, 60 Hz compositor, release build.
- **Not measured on macOS.** See Consequences.

## Findings

### Measurements against PLAN §1 budgets

| Budget | Result | Verdict |
|---|---|---|
| Keystroke to paint < 8 ms | Single cursor: p50 **2.2 ms** and p99 3.7 ms on a 10k-line file. On the 100k-line file: p50 **5.4 ms**, p99 8.1–9.3 ms (across runs). | ✅ normal files; ⚠️ 100k-line p99 at the edge |
| 120 fps (8.3 ms frames) | Scrolling the 100k-line file costs p50 **2.9 ms** and p99 4.8 ms of CPU per frame. Every frame hit vsync: interval p50 16.7 ms, p99 18.3 ms at 60 Hz. The diff scrolls at p50 3.0 ms and p99 4.1 ms. | ✅ CPU fits a 120 Hz frame (not observed: 60 Hz panel) |
| 100k-line file opens < 200 ms | The editor state is built in **2.4 ms**, and **37 ms** pass from open to first paint. The 500 decorations apply in 2.5 ms. | ✅ |
| Cold UI start < 500 ms | **312–343 ms** from launch to first frame, including generating the 3 MB file. | ✅ |
| UI RAM < 150 MB | **40 MB private** (RssAnon) with a small file. Total RSS is 260 MB, but about 225 MB of that is shared, file-backed GPU driver libraries (NVIDIA plus Mesa/LLVM). Opening the 100k-line file adds about **150 MB private** (193 MB total), roughly 47× the text size. | ✅ normal files; ❌ 100k-line file |

**Multi-cursor typing (edit to paint, p50 / p99):**

| Lines | 2 cursors | 10 cursors | 50 cursors |
|---|---|---|---|
| 10,000 | 2.0 / 2.8 ms | 3.3 / 4.2 ms | 9.4 / 10.9 ms |
| 100,000 | 6.6 / 8.7 ms | 13.0 / 15.8 ms | **40.3 / 44.9 ms** |

Cost grows with *cursors × file size*, about 0.7 ms per cursor at 100k lines. It is not caused by the decorations: the same run without them measured 39 ms. The editor does something proportional to the file on every cursor's edit. Profiling that was outside the time-box, so it is a follow-up.

**Other numbers:**
- A full tree-sitter parse of the 100k-line file takes **267–273 ms**.
- The diff parses its patch in 0.2 ms and paints its first frame in 6 ms.

### Capabilities

| Need | gpui-component 0.11 | Coqo approach |
|---|---|---|
| 100k-line file | Rope-based and virtualized. Opening and scrolling are fast. | As-is |
| Decorations | A Monaco-style API: text decorations (`HighlightStyle`), range fill and frame decorations, gutter marks and overview-ruler marks. Each comes as an independently owned collection whose ranges track edits. | As-is. It maps directly onto the editor contribution API (#43). |
| Anchored popover | No code-anchored popover widget, but `EditorState::range_to_bounds(range)` plus GPUI's `deferred(anchored())` gives a popover that follows a code range. | Extend: a small Coqo widget |
| Inline ghost values | **No virtual text or inlay hints.** Ghost text exists only for LSP inline completions. An overlay positioned from `range_to_bounds` works for end-of-line values. It renders correctly, but it uses the previous frame's layout and can't push text aside. | Extend: overlay now; upstream or own virtual-text support before live trace (M9) |
| Side-by-side diff | A built-in, read-only, virtualized `Diff` component: split and unified modes, word-level highlights, syntax highlighting, hunk folding, gutter markers, conflict resolution and line selection. It's fed from a unified patch. | As-is for review (M7, M8). Comment threads go in its decoration and gutter-marker hooks. |
| Multi-cursor | Built in: add cursor above/below, and edits apply at every cursor through the input handler. Note that `EditorState::insert` writes only at the primary cursor. | As-is; fix scaling (follow-up) |
| Folding, indent guides, diagnostics, LSP hooks (completion, hover, code actions, definition) | Built in | As-is |
| Syntax highlighting | tree-sitter. Files over 256 KB are parsed on a background thread after a debounce, so their first paint is unhighlighted. | As-is |
| Zen styling | Rounded floating panels, soft shadows, raised buttons and custom palette tokens all render as designed (`Theme::global_mut`). | As-is |

### Dependency and supply chain

- **Where GPUI comes from:** gpui-component consumes GPUI as **`gpui-pre`**, a weekly crates.io snapshot of Zed's GPUI (0.3.14 is zed@f126950), together with `gpui-base`, `gpui-component` and `gpui-kit`. All are Apache-2.0, and all are published by the gpui-kit maintainer. The upstream `gpui` crate on crates.io (0.2.2) is a year old. **Pinning a Zed git revision, as #11 assumed, isn't how this ecosystem works.** We pin `gpui-kit = "=0.11.0"`, which in turn pins the exact `gpui-pre` snapshot.
- **Churn:** releases ship weekly (0.8 → 0.11 in four weeks), and any of them may change APIs. The `coqo-ui` facade (PLAN §11) is the mitigation, and upgrades become deliberate PRs.
- **Licences:** the transitive tree passes our licence gate except for 8 crates:
  - **MPL-2.0:** `cbindgen` (build), `dwrote` (Windows only), `option-ext`.
  - **0BSD:** `enum-iterator`, `enum-iterator-derive`.
  - **CC0-1.0:** `notify`, `tiny-keccak`.
  - **NCSA:** `libfuzzer-sys` (fuzzing, pulled in by `all-features`).

  **No GPL.** The first PR that adds GPUI to the workspace (#23) adds these as reviewed per-crate exceptions in `deny.toml`.
- **Bus factor:** a single publisher controls the snapshot crates, which is a supply-chain risk. Mitigations: exact pins, `Cargo.lock`, cargo-deny, and the option of vendoring the snapshot if the project stalls.

### Other observations

- **Unfocused windows:** GPUI throttles them to 30 fps by default (`WindowOptions::inactive_frame_interval`). That's a good power default; benchmarks must turn it off.
- **Headless screenshots work:** `HeadlessAppContext` plus `capture_screenshot` render offscreen on Linux (WGPU) without touching the screen, which is usable for visual tests of static UI in CI. Background-timer work such as large-file highlighting doesn't progress there, and `Window::render_to_image` isn't implemented for real Wayland windows.
- **System dependencies (Fedora):** building on Linux needs `clang fontconfig-devel freetype-devel wayland-devel libxkbcommon-devel libxkbcommon-x11-devel libxcb-devel libX11-devel openssl-devel libzstd-devel alsa-lib-devel vulkan-loader-devel`.
- **Build time:** a cold release build of the GPUI stack takes about 2 minutes on 12 cores.

## Decision

**Go: use gpui-component's editor (via `gpui-kit`, exact-pinned) and extend it.** Don't build our own editor element now.

- Use the editor, decorations, folding, diagnostics, LSP hooks, multi-cursor and `Diff` component as they are, behind the `coqo-ui` facade, so mods and kernel code never name gpui-component types directly.
- Build Coqo's anchored popover on `range_to_bounds` plus `anchored()`.
- Show ghost values as an end-of-line overlay for now. Before live trace (M9), get real virtual-text or inlay support into the editor, preferably upstream, otherwise in a thin wrapper element.
- Track the large-file costs (per-cursor edit scaling and memory) as a follow-up to profile and fix upstream. They only matter beyond about 10k lines with many cursors, and for very large files.

**Fallback (unchanged from PLAN §11):** our own editor element on a rope plus tree-sitter. We switch to it if a gpui-kit release breaks us in a way the facade can't absorb, if the snapshot crates stop being published, or if virtual text can't be added within the component.

## Consequences

- #23 (UI shell) depends on `gpui-kit = "=0.11.0"`, adds the licence exceptions above to `deny.toml`, and installs the Linux system packages in CI.
- The `coqo-ui` facade is mandatory from the first UI PR, not an optional nicety.
- Follow-up issues: large-file editing performance (multi-cursor scaling and memory), and inline virtual text for ghost values (feeding #43 and the trace UI).
- **macOS is unmeasured.** The first macOS developer, or the macOS CI job, should run `cargo run --release -- --bench` in `spikes/s4-gpui` and add the numbers here.
- PLAN §3.1 and §11 are updated: GPUI comes from the `gpui-pre` snapshots via `gpui-kit`, pinned exactly, rather than from a Zed git revision.
