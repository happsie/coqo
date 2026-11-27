# 3. Coqo Protocol v0: wire format, handshake and subscriptions

- **Status:** Accepted
- **Date:** 2026-11-27

## Context

PLAN §4.4 calls for JSON-RPC 2.0 with length-prefixed frames over the daemon's Unix socket, optional MessagePack for hot paths, a version and build handshake, subscriptions with sequence numbers, and a documented schema "with generated Rust types and a TypeScript client". Issue #16 implements v0 in `crates/protocol`. Every later daemon feature (#20 lifecycle, #22 workspaces, #27 reattach, #40 buffers, #46 terminals) builds on it, so the wire details need to be fixed and written down. The full spec is [`protocol/README.md`](../../protocol/README.md).

## Decision

**Framing.** A u32 big-endian payload length, then one JSON-RPC 2.0 message. No batches. Maximum payload is 64 KiB during the handshake and 16 MiB afterwards. An oversized length prefix is a fatal error raised before any payload is buffered.

**Encodings.** JSON is mandatory. MessagePack (`rmp-serde`, structs as maps) is chosen in the handshake when both sides list it, and applies from the frame after the handshake response. Params, results and events are carried as dynamic values (`serde_json::Value`) in both encodings. That keeps one code path. MessagePack still removes JSON's text parsing and escaping cost. Raw binary payloads (for example buffer chunks) can be added in a later minor version if measurements show they are needed.

**Handshake.** The first request is always `coqo/hello` (protocol version, build id, client name, accepted encodings), and both it and its response are always JSON. This exchange is frozen across all versions, so any two versions can tell each other apart. Versions are `major.minor`: equal majors are compatible and minors only add things. An incompatible major is answered with error `-32001` carrying the server's version and build, and the client turns that into an `IncompatibleVersion` error naming both versions. Both sides enforce a handshake timeout (default 5 s). That, the frame-size check and the frozen hello mean the handshake never hangs.

**Methods.** A method is declared once as a Rust type implementing `Method`, `NotificationMethod` or `Topic` (name plus params, result or event types). The `Client` and the server `Router` use these types, so names and types cannot drift between the two sides. `coqo/` names are reserved for the protocol. In v0 only clients send requests; servers push data through notifications.

**Subscriptions.** `coqo/subscribe {topic, params, since?}` returns `{subscription, head}`. Events arrive as `coqo/event {subscription, seq, event}`, and `coqo/subscriptionEnded` ends a subscription with an optional error. `seq` is strictly increasing per source and starts at 1. It may skip values when params filter a larger log, and stays stable across restarts (the storage event log from #17 never reuses seqs). `since: n` replays everything after `n` and then goes live. If that is impossible, the subscribe fails with `-32003 RESUME_GAP` and the client re-snapshots. Slow consumers are never buffered without bound. A server-side subscriber that falls behind its source's retention gets `RESUME_GAP`. A client whose local buffer overflows unsubscribes and resubscribes from its last delivered `seq`, which loses nothing and repeats nothing. `SeqLog` is an in-memory source with these semantics, and the persistent event log exposes the same contract (`read_since` plus `EventsPruned`).

**Schema and TypeScript.** For v0, the Rust types are the source of truth and the schema is generated *from* them, not the other way round as PLAN §4.4 suggested. A test emits `protocol/schema/coqo-protocol.schema.json` (JSON Schema 2020-12 via `schemars`, a dev-dependency) and `protocol/ts/coqo-protocol.ts` (types, a typed method map and a frame encoder). The same test fails if either file is out of date. No Node toolchain runs in CI.

## Consequences

- The UI, CLI and daemon share one crate and cannot disagree about message shapes. Other languages get the JSON Schema and the TS stub.
- Generating Rust types from a schema can still be added later if non-Rust implementations need a neutral source of truth. Until then, PLAN §4.4 is updated to say the schema is generated from the Rust types.
- Using `Value` for payloads costs one extra conversion per message. This is acceptable for v0; buffer ops (#40) should measure it before adding binary payloads.
- Socket placement, `0600` permissions, peer-credential checks and the compatible-build policy belong to the daemon lifecycle (#20). This crate only offers `connect_unix` and `serve_unix`.
- Server-to-client requests, request cancellation and keepalive pings are not in v0. Each can be added in a minor version under the `coqo/` prefix.
