# Coqo Protocol v0

The versioned contract between the Coqo UI (`coqo`), the CLI, `coqod mcp` and the daemon (`coqod`). See PLAN §4.4 and [ADR 0003](../docs/adr/0003-coqo-protocol-v0.md) for the reasoning behind it.

The Rust implementation is [`crates/protocol`](../crates/protocol) (`coqo-protocol`). It is the source of truth: the files in this directory are generated from it and checked by a test.

| File | What |
|---|---|
| [`schema/coqo-protocol.schema.json`](schema/coqo-protocol.schema.json) | JSON Schema (2020-12) of every method's params and result, the method table (`x-methods`) and the error codes (`x-error-codes`). |
| [`ts/coqo-protocol.ts`](ts/coqo-protocol.ts) | TypeScript types, a typed method map and a frame encoder, derived from the schema. A stub for tooling and future clients. |

Regenerate both after changing a protocol type:

```sh
COQO_BLESS=1 cargo test -p coqo-protocol schema
```

`cargo test` fails if they are out of date.

## Version

This document describes protocol version **0.0** (`major.minor`).

- Two peers are **compatible** when their `major` versions are equal.
- `minor` bumps only add things (new methods, new optional fields). A connection uses the lower of the two minors.
- Builds (`build` in the handshake) may differ. A UI and a daemon from different builds keep working as long as their protocol versions are compatible (PLAN §4.3).

## Transport and framing

Any reliable byte stream works. Locally it is a Unix domain socket (`coqod.sock`, PLAN §4.3); the protocol does not depend on it, so it could run over an SSH channel later.

Every message is one **frame**:

```
+----------------------+---------------------------+
| length: u32, BE      | payload: `length` bytes   |
+----------------------+---------------------------+
```

- The payload is exactly one JSON-RPC 2.0 message. Batches (JSON arrays) are not supported and get an `Invalid Request` error.
- Maximum payload: 64 KiB during the handshake, 16 MiB afterwards (configurable). A larger length prefix is a fatal error and closes the connection before any payload is buffered, so a peer that is not speaking the protocol fails fast instead of hanging.
- An empty payload is a valid frame and is answered with a `Parse error`.

## Encodings

| Name | Payload |
|---|---|
| `json` | UTF-8 JSON. Mandatory; always used for the handshake. |
| `msgpack` | MessagePack, structs as maps keyed by field name. Optional; for hot paths such as buffer ops and trace streams. |

The client lists the encodings it accepts in `coqo/hello`, most preferred first. The server picks the first one it also supports, or `json`, and reports its choice in the result. From the frame after the handshake response on, both directions use that encoding.

## Messages

Standard [JSON-RPC 2.0](https://www.jsonrpc.org/specification):

```jsonc
// request: exactly one response follows
{"jsonrpc": "2.0", "id": 7, "method": "workspace/open", "params": {"path": "/src/app"}}
// success / error response
{"jsonrpc": "2.0", "id": 7, "result": {"workspace": "ws-1"}}
{"jsonrpc": "2.0", "id": 7, "error": {"code": -32601, "message": "method not found: workspace/open"}}
// notification: no id, no response
{"jsonrpc": "2.0", "method": "coqo/event", "params": {"subscription": 1, "seq": 42, "event": {}}}
```

- Ids are numbers or strings. Coqo clients use increasing integers. `id: null` only appears on error responses to messages whose id could not be read.
- Methods without params omit `params`. Handlers treat a missing `params` as `null`.
- Requests on one connection run concurrently, so responses can arrive out of order.
- Method names starting with `coqo/` are reserved for the protocol itself; `rpc.` is reserved by JSON-RPC. Application methods (workspaces, buffers, sessions, …) are added by later issues under their own prefixes.
- In v0 only clients send requests. A server never sends requests, only responses and notifications.

## Handshake

The first frame on every connection is a `coqo/hello` **request** from the client, encoded as JSON. Nothing else may be sent until its response arrives. This exchange (u32 BE length prefix, JSON, `coqo/hello`) is **frozen across all protocol versions**, so any two versions can always recognise each other.

```jsonc
// client → server
{"jsonrpc": "2.0", "id": 0, "method": "coqo/hello", "params": {
  "protocol": {"major": 0, "minor": 0},
  "build": "0.1.0+3f2a9c1",
  "client": "coqo-ui",
  "encodings": ["msgpack", "json"]
}}
// server → client
{"jsonrpc": "2.0", "id": 0, "result": {
  "protocol": {"major": 0, "minor": 0},
  "negotiated": {"major": 0, "minor": 0},
  "build": "0.1.0+3f2a9c1",
  "server": "coqod",
  "encoding": "msgpack",
  "connection": 3
}}
```

Failures are explicit. None of them hangs:

| Situation | What happens |
|---|---|
| Incompatible `major` | The server answers error `-32001 INCOMPATIBLE_PROTOCOL` with `data: {"protocol": …, "build": …}` and closes. The client reports *"incompatible Coqo Protocol version: this side speaks X, the peer (build B) speaks Y"*. |
| Anything other than `coqo/hello` first | Error `-32002 NOT_INITIALIZED` (for requests), then the server closes. |
| Malformed hello params | Error `-32602 INVALID_PARAMS`, then the server closes. |
| No answer within the timeout (default 5 s) | Either side gives up with a handshake-timeout error. |
| Peer is not a Coqo endpoint | Its bytes almost always read as an oversized frame and fail immediately; otherwise the timeout applies. |
| `coqo/hello` again after the handshake | Error `-32600 INVALID_REQUEST`; the connection stays open. |

Unknown fields in `coqo/hello` and its result are ignored, so newer minors can add fields.

## Subscriptions

A **topic** is a named stream of events. Each event has a sequence number `seq` that is **strictly increasing** within the topic's source. It starts at 1, so `0` means "nothing seen". It may skip values when the topic params filter a larger log (for example one workspace's events out of the daemon event log). Sources keep seqs stable across daemon restarts; the daemon event log never reuses them.

```jsonc
// client → server
{"jsonrpc": "2.0", "id": 5, "method": "coqo/subscribe",
 "params": {"topic": "workspace/events", "params": {"workspace": "ws-1"}, "since": 41}}
// server → client: always before the first event of this subscription
{"jsonrpc": "2.0", "id": 5, "result": {"subscription": 1, "head": 43}}
// server → client
{"jsonrpc": "2.0", "method": "coqo/event", "params": {"subscription": 1, "seq": 42, "event": {}}}
{"jsonrpc": "2.0", "method": "coqo/event", "params": {"subscription": 1, "seq": 43, "event": {}}}
// later: the server ends it (no `error` means a normal end)
{"jsonrpc": "2.0", "method": "coqo/subscriptionEnded", "params": {"subscription": 1, "error": {"code": -32003, "message": "…"}}}
```

- `since` omitted: only events published after the subscription starts.
- `since: n`: every event with `seq > n` that the source still has, then live events. `since: 0` replays everything retained.
- `head` is the source's latest `seq` when the subscription started.
- If the source no longer has the events after `n`, or `n` is ahead of the source, `coqo/subscribe` fails with `-32003 RESUME_GAP` and `data: {"needed", "oldest", "head"}`. The client then takes a fresh snapshot.
- `coqo/unsubscribe {"subscription": 1}` stops it; the result is `true` if it existed. Events for a subscription the client no longer knows are ignored.
- A subscriber that falls too far behind the source gets `coqo/subscriptionEnded` with `RESUME_GAP`. A client whose own buffer overflows unsubscribes and resubscribes with `since` set to the last `seq` it delivered. Either way a slow consumer never makes the daemon buffer without bound, and no event is lost or delivered twice.

**Reattach (PLAN §4.3):** fetch a snapshot that includes the `seq` it reflects, then `coqo/subscribe` with `since` set to that `seq`. On `RESUME_GAP`, fetch a new snapshot and repeat.

## Daemon methods

`coqod` serves these besides the `coqo/` methods. Socket location, spawning and the idle policy are in [ADR 0007](../docs/adr/0007-daemon-lifecycle.md).

| Method | Params | Result |
|---|---|---|
| `daemon/status` | none | `DaemonStatus`: `version`, `build`, `protocol`, `pid`, `startedAtMs`, `uptimeMs`, `connections` (including the caller), `busy` (reasons of the work keeping it alive), `idle` (`phase`: `active`, `idle` or `servicesStopped`, plus `stopServicesAfterSecs` and `exitAfterSecs`, `null` meaning never). |
| `daemon/shutdown` | `{force?: bool}` | `{stopping, busy}`. Refused (`stopping: false`, with the busy reasons) while work is busy, unless `force` is set. When stopping, the daemon answers, closes every connection and exits. |

## Error codes

| Code | Name | Meaning |
|---|---|---|
| -32700 | `PARSE_ERROR` | The payload is not valid JSON / MessagePack. |
| -32600 | `INVALID_REQUEST` | Valid data, but not a JSON-RPC 2.0 message (includes batches). |
| -32601 | `METHOD_NOT_FOUND` | Unknown method. |
| -32602 | `INVALID_PARAMS` | Params do not match the method's schema. |
| -32603 | `INTERNAL_ERROR` | The handler failed. |
| -32001 | `INCOMPATIBLE_PROTOCOL` | Handshake: different `major` version. |
| -32002 | `NOT_INITIALIZED` | A request arrived before `coqo/hello`. |
| -32003 | `RESUME_GAP` | Cannot resume from the requested `seq`; take a fresh snapshot. |
| -32004 | `UNKNOWN_SUBSCRIPTION` | Unknown topic or subscription. |
| -32005 | `REQUEST_CANCELLED` | The request was cancelled. |

Application methods use codes outside `-32768..=-32000`.

## Application methods

The daemon's own methods and topics are declared in `coqo_protocol::methods` and implemented in `coqod`. The schema files list every params, result and event type.

### Workspaces (`workspace/*`)

A workspace is one or more folder roots with a stable id (`ws-…`), a name, `createdAt`/`lastOpenedAt` (Unix ms) and an `open` flag, plus a key/value state document the UI owns (open tabs, layout hints, settings overrides). They are stored in `coqo.db` and survive UI and daemon restarts.

| Method | Params → result |
|---|---|
| `workspace/open` | `{path}` or `{id}`, optional `name` → `WorkspaceInfo`. Idempotent per canonical folder: a workspace whose first root is that folder is reused. |
| `workspace/list` | → `{workspaces, seq}`, most recently opened first. `seq` is the event-log position the snapshot reflects. |
| `workspace/get`, `workspace/close` | `{id}` → `WorkspaceInfo` |
| `workspace/remove` | `{id}` → `true` if it existed. Deletes daemon-side data only, never the folders. |
| `workspace/addRoot`, `workspace/removeRoot` | `{id, path, name?}` / `{id, root}` → `WorkspaceInfo`. Root ids are never reused; the last root cannot be removed. |
| `workspace/rename` | `{id, name}` → `WorkspaceInfo` |
| `workspace/getState` | `{id}` → `{entries, seq}` |
| `workspace/setState` | `{id, key, value}` → `null`. `value: null` removes the entry. |

Topic **`workspace/events`** (`{workspace?}`) carries `opened`, `closed`, `changed` (name or roots), `removed` and `stateChanged` events, tagged by `type`. It is backed by the persistent event log, so `seq`s are stable across daemon restarts and skip values that belong to other features. Reattach: `workspace/list`, then subscribe with `since` set to its `seq`.

### Files (`fs/*`)

Read-only access for the file tree. Paths are `{workspace, root, path}` with a `/`-separated `path` relative to the root (`""` is the root). Absolute paths, `..` and paths that resolve through a symlink to outside the workspace's roots fail with `PATH_OUTSIDE_WORKSPACE`. Entries carry an `ignored` flag from `.gitignore` files, `.git/info/exclude` and the global git excludes; `.git` is always ignored.

| Method | Params → result |
|---|---|
| `fs/list` | `PathParams` → `{entries: [{name, kind, symlink, size, mtime?, ignored}]}`, directories first. `kind` is `file`, `dir` or `other` (symlinks report their target's kind; broken or escaping ones are `other`). |
| `fs/stat` | `PathParams` → `{kind, symlink, size, mtime?, ignored}` |
| `fs/read` | `{workspace, root, path, maxBytes?}` → `{text?, binary, size, mtime?}`. Default limit 4 MiB, at most 8 MiB; larger files fail with `FILE_TOO_LARGE` and `data: {size, limit}`. |

Topic **`fs/changes`** (`{workspace}`, which must be open) carries `{root, paths, rescan}` batches, debounced (about 100 ms), with paths sorted and deduplicated. Changes inside `.git` and inside ignored directories are left out, but an ignored entry appearing directly in a visible directory is reported. `rescan: true` means too much changed (or the watcher lost events): re-list what you show. The stream is live only; after a reconnect, re-list and subscribe again. Closing the workspace ends it.

| Code | Name | Meaning |
|---|---|---|
| 1001 | `UNKNOWN_WORKSPACE` | No workspace has that id. |
| 1002 | `UNKNOWN_ROOT` | The workspace has no root with that id. |
| 1003 | `PATH_OUTSIDE_WORKSPACE` | Not a workspace-relative path, or it resolves outside the roots. |
| 1004 | `NOT_FOUND` | The path does not exist. |
| 1005 | `FILE_TOO_LARGE` | Above the read limit; `data: {size, limit}`. |
| 1006 | `NOT_A_DIRECTORY` | A directory was expected. |
| 1007 | `NOT_A_FILE` | A regular file was expected. |
| 1008 | `WORKSPACE_CLOSED` | The operation needs an open workspace. |
| 1009 | `IO_ERROR` | Permissions or another I/O error. |
