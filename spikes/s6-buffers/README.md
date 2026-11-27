# Spike S6: UI↔daemon buffer replication

Throwaway prototype for issue #13. The findings and the decision are in
[`docs/adr/0006-buffer-replication.md`](../../docs/adr/0006-buffer-replication.md).

This directory is its own Cargo workspace, so the Coqo workspace and CI don't build it.

| Path | What it is |
|---|---|
| `src/ot.rs` | Model A building block: an ot.js-style text operation over UTF-8 byte offsets (apply, compose, transform, invert, offset mapping). |
| `src/oplog.rs` | Model A: the daemon-authoritative `Server` (revisions, log, anchors, selective revert) and the UI `Client` (optimistic apply, one change in flight, per-side undo). |
| `src/crdt.rs` | Model B: a `yrs` replica (text, delta → editor edits, `UndoManager` for the local origin, `StickyIndex` anchors, state-vector sync). |
| `src/wire.rs` | JSON-RPC 2.0 frames (u32 length prefix), JSON or MessagePack. |
| `src/main.rs` | The benchmark: a tokio daemon plus two UI clients over a real Unix socket. |
| `tests/convergence.rs` | Property tests: 3 clients + agent, random delivery order, convergence and no lost characters, reconnect, agent-edit revert. |
| `editor-map/` | A headless GPUI check of how remote edits land in gpui-component 0.11's `EditorState` (offscreen; it never touches the screen). |

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo test --release                       # unit + property tests
cargo run --release -p spike-s6-buffers    # measurements, markdown to stdout
cargo run --release -p s6-editor-map       # editor mapping checks (builds GPUI, ~2 min cold)
```
