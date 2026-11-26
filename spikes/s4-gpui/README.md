# Spike S4: GPUI + gpui-component editor (issue #11)

Throwaway prototype behind [ADR 0002](../../docs/adr/0002-ui-editor-component.md). It isn't part of the Coqo workspace and CI doesn't build it.

## Run

```sh
cargo run --release                          # interactive window, editor scene
cargo run --release -- --scene diff          # or: zen
cargo run --release -- --bench               # scripted measurements, prints JSON, then exits
cargo run --release -- --scene zen --shot out.png   # render offscreen to a PNG (no window)
```

Environment knobs:
- `SPIKE_LINES`: editor file size (default 100000).
- `SPIKE_CURSORS`: cursors for the multi-cursor run (default 50).
- `SPIKE_NO_DECOR=1`: skip the 500 decorations.
- `SPIKE_SHOT_WAIT_MS`: extra wait before `--shot` captures.

Linux needs the system packages listed in the ADR.
