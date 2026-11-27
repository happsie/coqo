# Architecture Decision Records

Architecture decisions for Coqo, one file per decision. See [ADR 0001](0001-record-architecture-decisions.md) for the format and when to write one.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-ui-editor-component.md) | UI editor component: gpui-component's editor, extended by Coqo | Accepted |
| [0003](0003-coqo-protocol-v0.md) | Coqo Protocol v0: wire format, handshake and subscriptions | Accepted |
| [0004](0004-workbench-layout.md) | Workbench layout: an own split tree, saved through a layout store | Accepted |
| [0005](0005-claude-integration.md) | Claude integration: the Claude Agent ACP adapter on the user's own `claude`, with terminal mode beside it | Accepted |
| [0006](0006-buffer-replication.md) | Buffer replication: daemon-authoritative op-log with optimistic local apply | Accepted |
| [0007](0007-daemon-lifecycle.md) | Daemon lifecycle: socket, single-instance lock, spawn-on-demand, idle policy and socket security | Accepted |
| [0008](0008-editor-remote-edits-patch.md) | Editor remote edits: a vendored, patched gpui-base behind `coqo_ui::editor` | Accepted |
| [0009](0009-daemon-terminals.md) | Daemon terminals: PTY and emulator in `coqod`, the UI mirrors rows | Accepted |
| [0010](0010-replicated-buffers.md) | Replicated buffers: protocol details, disk merges and the journal (refines 0006) | Accepted |
| [0011](0011-syntax-layer.md) | Syntax layer: `coqo-syntax`, shared by the daemon and the UI | Accepted |
