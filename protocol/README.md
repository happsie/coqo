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

This document describes protocol version **0.1** (`major.minor`).

- Two peers are **compatible** when their `major` versions are equal.
- `minor` bumps only add things (new methods, new optional fields). A connection uses the lower of the two minors.
- History: **0.1** added the optional `token` in `coqo/hello` and the `UNAUTHORIZED` / `FORBIDDEN` codes ([Authentication](#authentication-and-scopes)).
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
  "protocol": {"major": 0, "minor": 1},
  "build": "0.1.0+3f2a9c1",
  "client": "coqo-ui",
  "encodings": ["msgpack", "json"],
  "token": "<64 hex digits>"   // since 0.1; see Authentication
}}
// server → client
{"jsonrpc": "2.0", "id": 0, "result": {
  "protocol": {"major": 0, "minor": 1},
  "negotiated": {"major": 0, "minor": 1},
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
| Missing or wrong `token` (servers that authenticate, such as `coqod`) | Error `-32006 UNAUTHORIZED` with a message saying what is wrong (never the expected token), then the server closes. The version check comes first, so an incompatible client still sees `-32001`. |
| No answer within the timeout (default 5 s) | Either side gives up with a handshake-timeout error. |
| Peer is not a Coqo endpoint | Its bytes almost always read as an oversized frame and fail immediately; otherwise the timeout applies. |
| `coqo/hello` again after the handshake | Error `-32600 INVALID_REQUEST`; the connection stays open. |

Unknown fields in `coqo/hello` and its result are ignored, so newer minors can add fields.

## Authentication and scopes

A server may require a **session token** in `coqo/hello` (`token`, a string). `coqod` does. It answers a missing or wrong token with `-32006 UNAUTHORIZED` and closes the connection. A server that does not authenticate ignores the field.

`coqod` accepts two kinds of token:

| Token | Where a client gets it | Scope |
|---|---|---|
| The **daemon token**: 256 random bits, hex, new at every daemon start | `coqod.token` (`0600`) in the runtime directory, next to the socket. `coqo-client` reads it automatically, after connecting. | Everything. |
| A **session token**, minted by the daemon for one agent session | Passed to the process the agent launches (`coqod mcp`) in the `COQO_SESSION_TOKEN` environment variable, never in `argv`. That one is a **one-time** token: it works for one `coqo/hello`, within a minute of being issued ([ADR 0012](../docs/adr/0012-editor-mcp-bridge.md)). | The methods and topics opened to session tokens (below), for its own workspace only. Everything else fails with `-32007 FORBIDDEN`. When the agent session closes, the daemon closes connections attached with its token. |

**Session tokens are denied by default.** Every method, notification and topic declares, when the server registers it, whether session tokens may call it and how the call is bound to the token's workspace or agent session. The server checks this before the handler runs. A session token gets `-32007 FORBIDDEN` from:

- any method or topic that is not opened to session tokens, whatever its params (they are not read);
- an opened method or topic aimed at another workspace or session than the token's.

A refused notification is dropped, since there is no response to carry the error. The daemon token is never refused.

In `coqod`, session tokens may call only these, and only for their own workspace:

| Method or topic | Bound by |
|---|---|
| `workspace/get`, `workspace/getState` | `id` |
| `workspace/events` (topic) | `workspace`; an unfiltered feed needs the daemon token |
| `fs/list`, `fs/stat`, `fs/read`, `fs/changes` (topic) | `workspace` |
| `buffer/open` | `workspace`; `replica`, if given, must be one this connection opened |
| `buffer/edit` (notification), `buffer/close` | the `replica`: only replicas this connection opened, in that `buffer` |
| `buffer/changes` (topic) | the workspace the `buffer` is open in |
| `syntax/symbols`, `syntax/outline`, `syntax/folding` | the workspace the `buffer` is open in |
| `sessions/setStatus` | `session`: its own agent session only |
| `sessions/setCriteria` | `session`: its own agent session only |
| `walkthroughs/create` | `workspace` and `session`: its own agent session only |

**Replicas belong to the connection that opened them.** A session token can edit and close only the replicas its own connection got from `buffer/open`, and can name only those in a later `buffer/open`. Any other replica (the UI's, another agent's, or an id it made up) gets `-32007 FORBIDDEN`; for `buffer/edit` that comes as `buffer/editRejected`. Replica ids appear in every change's `origin`, so knowing one proves nothing. When the connection closes (or the session's scope is revoked, which closes it), its replicas leave their buffers. `buffer/changes` and `syntax/*` stay bound to the workspace: anything they show, the token could read with `buffer/open` anyway. The daemon token may act as any replica.

Everything else, including `daemon/*`, the rest of `workspace/*`, `buffer/save`, `buffer/revertChange`, `terminal/*`, `agents/*` and the rest of `sessions/*`, needs the daemon token.

Tokens are secrets. Compare them in constant time, and never log them or put them on a command line. The server takes the token out of the hello before request handlers see it. Before the handshake, `coqod` also checks that the connecting process runs as the same user (`SO_PEERCRED` / `getpeereid`), and both sides check that the runtime directory belongs to the user and is closed to everyone else ([ADR 0007](../docs/adr/0007-daemon-lifecycle.md)).

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
| `daemon/shutdown` | `{force?: bool}` | `{stopping, busy}`. Refused (`stopping: false`, with the busy reasons) while work is busy, unless `force` is set. When stopping, the daemon answers, closes every connection and exits. Needs the daemon token (`-32007 FORBIDDEN` for a session token). |

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
| -32006 | `UNAUTHORIZED` | Handshake: missing or wrong session token. |
| -32007 | `FORBIDDEN` | The connection's token does not allow this call (for example, another workspace). |

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

**Scopes.** A session token reaches only its own workspace: `workspace/get`, `workspace/getState`, `workspace/setState`, `fs/list`, `fs/stat`, `fs/read` and `fs/changes`, and `workspace/events` with `workspace` set to it. Everything else here (`open`, `list`, `close`, `remove`, `addRoot`, `removeRoot`, `rename`, and `workspace/events` without a filter) needs the daemon token. Out-of-scope calls fail with `-32007 FORBIDDEN`.

Topic **`workspace/events`** (`{workspace?}`) carries `opened`, `closed`, `changed` (name or roots), `removed` and `stateChanged` events, tagged by `type`. It is backed by the persistent event log, so `seq`s are stable across daemon restarts and skip values that belong to other features. Reattach: `workspace/list`, then subscribe with `since` set to its `seq`.

### Files (`fs/*`)

Access for the file tree. Paths are `{workspace, root, path}` with a `/`-separated `path` relative to the root (`""` is the root). Absolute paths, `..` and paths that resolve through a symlink to outside the workspace's roots fail with `PATH_OUTSIDE_WORKSPACE`. Entries carry an `ignored` flag from `.gitignore` files, `.git/info/exclude` and the global git excludes; `.git` is always ignored.

| Method | Params → result |
|---|---|
| `fs/list` | `PathParams` → `{entries: [{name, kind, symlink, size, mtime?, ignored}]}`, directories first. `kind` is `file`, `dir` or `other` (symlinks report their target's kind; broken or escaping ones are `other`). |
| `fs/stat` | `PathParams` → `{kind, symlink, size, mtime?, ignored}` |
| `fs/read` | `{workspace, root, path, maxBytes?}` → `{text?, binary, size, mtime?}`. Default limit 4 MiB, at most 8 MiB; larger files fail with `FILE_TOO_LARGE` and `data: {size, limit}`. |
| `fs/create` | `{workspace, root, path, kind}` → `{path}` (normalised). `kind` is `file` (empty) or `dir`; missing folders on the way are created. Something already there fails with `ALREADY_EXISTS`. Daemon token only. |
| `fs/rename` | `{workspace, root, from, to}` → `{path}`. Renames or moves an entry within one root; the folder `to` goes in must exist and `to` must be free (`ALREADY_EXISTS`). A folder cannot move into itself, and a root cannot be moved. Open buffers of the files it moves follow (same buffer id, unsaved edits kept), and so do questions, review threads, walkthroughs and the editor context anchored to them, published on their changes topics before the call returns. Daemon token only. |
| `fs/delete` | `{workspace, root, path, permanent?}` → `{trashed, restore?, no_restore?}`. Moves the entry to the OS trash; if that is not possible it fails with `TRASH_FAILED` and leaves it alone, unless `permanent: true` (delete for good). Open buffers keep their text; saving writes the file again. A root cannot be deleted. Daemon token only. `restore` is a token for `fs/restore`; without one, `no_restore` says why it can't come back (deleted for good, or on macOS too large for Coqo's copy). |
| `fs/restore` | `{workspace, root, path, restore}` → `{path}`. Brings back an entry `fs/delete` deleted, with the token it gave: from the OS trash (Linux, Windows) or from the copy Coqo kept of it (macOS, up to 16 MiB; in the data dir, removed when the daemon exits or after 24 h). `NOT_FOUND` for an unknown or expired token or an entry gone from the trash, `ALREADY_EXISTS` if something is at the path now. Daemon token only. |
| `fs/find` | `{workspace, query?, limit?}` → `{matches: [{root, path, score, positions}], total, files, indexing?}`. Fuzzy-finds files by root-relative path in the daemon's index of the workspace's non-ignored files (whitespace in `query` ignored, smart case). `positions` are byte offsets in `path`; `total` counts every match, `files` the index. An empty query lists the files nearest the roots first. `limit` defaults to 50, at most 1000. While the index is still being built (it starts when the workspace opens, and the call waits up to 2 s for it), `indexing: true` says to ask again. Daemon token only. |

The file operations act on the entry itself: its parent folder is resolved and confined like any path, but a symlink at the end is renamed or deleted as a link, never followed, and new entries never follow one. They need the daemon token, since agents edit files through their own tools and `buffer/*`.

The **file index** behind `fs/find` holds every file the ignore rules keep (the same rules as `ignored` above, `.git` never), symlinks to files included and symlinked folders not followed. It is built in the background when the workspace opens and follows the workspace's watcher: each `fs/changes` batch is re-checked on disk, a changed folder or `.gitignore` is walked again, and a `rescan` walks the root again. On a generated 200k-file tree it is ready about 1.5 s after `workspace/open`, and a query takes 3–23 ms (median, release build, through a client).

Topic **`fs/changes`** (`{workspace}`, which must be open) carries `{root, paths, rescan}` batches, debounced (about 100 ms), with paths sorted and deduplicated. Changes inside `.git` and inside ignored directories are left out, but an ignored entry appearing directly in a visible directory is reported. `rescan: true` means too much changed (or the watcher lost events): re-list what you show. The stream is live only; after a reconnect, re-list and subscribe again. Closing the workspace ends it.

### Agent sessions (`agents/*`, `sessions/*`)

Service `coqo.agents@1` (PLAN §7.3; types in `coqo_protocol::methods::agents`, implemented by `crates/agents`). Sessions live in the daemon: a turn keeps running when the client that started it disconnects, a running turn keeps the daemon busy, and every session update is persisted in `coqo.db`.

| Method | Params | Result |
|---|---|---|
| `agents/providers` | none | `ProviderInfo[]`: `id`, `name`, `kind` (`acp`, `terminal`, `native`), `available`, `capabilities` (`resume`, `modes`, `images`, `embeddedContext`). |
| `sessions/list` | `{workspace?, includeClosed?}` | `{sessions: SessionInfo[], seq}`, most recently updated first; `seq` is the `sessions/changes` seq the list reflects. |
| `sessions/get` | `{session}` | `SessionSnapshot`: `info`, `head` (seq of the last update), `turns`, `running`, `pendingPermissions`, `plan`, `modes`, `configOptions` (omitted when empty), `availableCommands`, `usage`, `auth`. |
| `sessions/create` | `{provider, cwd, workspace?, title?, mode?}` | `SessionInfo`. The provider session starts right away, so modes and slash commands arrive before the first prompt. A `title` given here is the user's (see **Titles** below). |
| `sessions/prompt` | `{session, content: ContentBlock[]}` | `{turn, seq}` once the turn has started; `SESSION_BUSY` while one runs. |
| `sessions/cancel` | `{session}` | `true` if a turn was running. Pending permission requests resolve as `cancelled`. |
| `sessions/respondPermission` | `{session, requestId, outcome, remember?}` | `null`. `outcome` is `{"outcome": "selected", "optionId"}` or `{"outcome": "cancelled"}`. With `remember: true` the request's `suggestedRule` is also stored for the session's workspace, as an allow or deny rule after the chosen option's kind; the option still goes to the agent unchanged. `INVALID_PARAMS` if the request has no `suggestedRule` or the outcome is `cancelled`. |
| `sessions/setMode` | `{session, modeId}` | `null`. For an agent that also has a `mode` config option, the daemon changes it through that option. |
| `sessions/setConfigOption` | `{session, configId, value}` | `null`. Changes one of the session's config options; `value` is a select entry's `value` (a string) or a boolean for a toggle. The options the change leaves arrive as a `config_options` update (one change may change others: a new model may offer other efforts). `INVALID_PARAMS` for an option the session doesn't have or a value it doesn't take. Daemon token only. See **Config options** below. |
| `sessions/setStatus` | `{session, status, state?, progress?}` | `SessionInfo`. The agent's status line (the `set_status` MCP tool): whitespace collapsed, cut at 120 characters; `state` is `working`, `blocked` or `done`; `progress` is `{current, total}` with `total` ≥ 1. A session token may call it for its own session. See **Status** below. |
| `sessions/setCriteria` | `{session, criteria: [{id?, text?, status?, evidence?}], replace?}` | `SessionInfo`. Sets or updates the session's acceptance criteria (the `set_acceptance_criteria` MCP tool). An entry with a known `id` changes that criterion (given fields only; `evidence` replaces it); any other adds one (needs `text`; id defaults to the next `AC<n>`; status `pending`). `replace: true` makes the list exactly the entries. `status` is `pending`, `met`, `failed` or `unknown`; `evidence` is `[{text, path?, line?}]` (relative paths from the session's `cwd`, 1-based lines). At most 50 criteria and 5 evidence each. A session token may call it for its own session (the change is the agent's); the daemon token's change is the user's (`changedBy`). |
| `sessions/rename` | `{session, title}` | `SessionInfo`. Gives the session the user's title: whitespace collapsed, cut at 120 characters; `INVALID_PARAMS` if it is empty. Kept across restarts; works on closed sessions too. See **Titles** below. |
| `sessions/close` | `{session}` | `true` if it was open. Cancels a running turn; the transcript stays readable. |
| `agents/permissionRules` | `{workspace}` | `PermissionRule[]`, oldest first: `{id, workspace, tool, pattern, decision, createdAt}`. |
| `agents/deletePermissionRule` | `{id}` | `true` if it existed. |

| Topic | Params | Events |
|---|---|---|
| `sessions/updates` | `{session}` | `SessionUpdate`, tagged by `update`: `user_message`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update` (with `diff` content and `locations`), `plan`, `available_commands`, `modes`, `current_mode`, `config_options`, `usage`, `auth_status`, `permission_requested`, `permission_resolved`, `state`, `turn_started`, `turn_ended`, `question`, `walkthrough` (a published walkthrough's summary), `closed`. Seqs are per session, from 1, stable across restarts. |
| `sessions/changes` | `{workspace?}` | `SessionInfo` whenever a session's state, title, mode, status or closed flag changes. Resuming replays only each session's latest info, so seqs skip values. |

**States** (`SessionInfo.state`, since `stateSince`): `idle` → `working` → (`needs_permission` → `working`) → `done`, or `idle` after a cancel, or `error` (with `stateReason`). Providers and daemon features may also report `needs_input` and `blocked`; while a permission request waits only `error` changes the state. A turn cut off by a daemon stop or crash ends with `stopReason: interrupted` and leaves the session in `error`; the next prompt resumes the provider session. The full table is in `crates/agents/src/state.rs` ([ADR 0015](../docs/adr/0015-agent-state-and-status.md)).

**Status** (`SessionInfo.status`: `{text, state?, progress?, turn?, updatedAt}`): what the agent says it is doing. `blocked` makes the session `blocked` with the text as `stateReason`; during a turn, `working` and `done` make it `working` (the turn's end makes it `done`); between turns `done` makes it `done` and `working` changes nothing. A turn start clears the status, and a turn end clears it unless the agent claimed `blocked` or `done`. Status changes are not transcript updates; they arrive on `sessions/changes`.

**Titles** (`SessionInfo.title`, omitted until there is one): the first prompt of an untitled session gives it a title, its first line of text with markdown marks dropped, cut at a word near 50 characters (a prompt of only `@`-mentions is titled after the first one). When the agent reports its own title (ACP `session_info_update`; Claude Code titles a session from its first prompt), that replaces the derived one. The user's title (`sessions/create`, `sessions/rename`) is never replaced. Titles are not transcript updates; they arrive on `sessions/changes`.

**Config options** (`SessionSnapshot.configOptions` and the `config_options` update, which replaces the whole list): the settings an agent offers for a session, ACP's session config options, kept provider-neutral. Each is `{id, name, description?, category?, type, currentValue, options?}`: `type: "select"` has a string `currentValue` and `options: [{value, name, description?, group?}]` (grouped entries are flattened, each with its group's name), `type: "boolean"` a boolean `currentValue`. `category` is for placement only: ACP's `mode`, `model`, `model_config`, `thought_level`, `speed`, or another string. The list keeps the agent's order. Claude Code's adapter offers Model, Effort (`thought_level`), Fast mode (`speed`, a boolean), Mode and, with custom agents, Agent. A `mode` option and `modes` stay in step: a change to one updates the other. Providers without config options send none, and `sessions/setMode` works as before. The daemon persists the options with the transcript; when a session resumes on a new agent process and the agent reports a value other than the one the session had, the daemon sets it again with `session/set_config_option` (in order, if the option still takes it).

**Acceptance criteria** (`SessionInfo.criteria`, omitted when empty: `[{id, text, status, evidence?, changedBy, updatedAt}]`): what "done" means for the session's task. They last as long as the session (not cleared by turns), survive daemon restarts, and arrive on `sessions/changes`. Walkthroughs and trace scenarios refer to them by `id`.

**Permission rules** (per workspace, in `coqo.db`, kept across restarts): before a permission request reaches any UI, the daemon checks the workspace's rules. `tool` is `execute` (`pattern` is a word-wise command prefix such as `cargo test`), `edit` or `read` (`pattern` is a glob over root-relative paths such as `src/**`); `decision` is `allow` or `reject`, and a reject rule wins. A rule only matches a request Coqo can read exactly: a simple command (no shell operators, redirections, substitutions, globs, `~` or `..`, absolute paths inside the workspace only, no code flags such as `-c`, never programs such as `rm`, `sudo`, `curl` or a shell), or files that all resolve inside the workspace's roots. Such a request carries `suggestedRule` (`{tool, pattern}`). A matched request is recorded as `permission_requested` followed at once by `permission_resolved` with `byRule` (the rule), answered with the `allow_once` option (else `allow_always`) or `reject_once` (else `reject_always`); the state stays `working`. A `permission_resolved` that created a rule carries it as `remembered`. Anything else is asked: the default is ask.

**Reattach** (chat tab): `sessions/get`, then `coqo/subscribe` to `sessions/updates` with `since: 0` for the whole transcript or `since: <last seq seen>`; on `RESUME_GAP`, take a fresh snapshot.

### Buffers (`buffer/*`)

Replicated text buffers ([ADR 0006](../docs/adr/0006-buffer-replication.md), details in [ADR 0010](../docs/adr/0010-replicated-buffers.md); types in `coqo_protocol::methods::buffer`, replica in `coqo-buffers`). The daemon owns one buffer per open file (one per canonical path, so two windows or a symlink share it). Replicas apply their own edits at once and send them against a revision; the daemon transforms them past anything committed since and broadcasts every change. Offsets are UTF-8 byte offsets; `TextEdit` lists (`{start, end, text}`) are sorted, non-overlapping and in pre-edit coordinates, like LSP `TextEdit[]`.

| Method | Kind | Params → result |
|---|---|---|
| `buffer/open` | request | `{workspace, root, path, replica?}` → `{buffer, replica, rev, text, lineEnding, encoding, savedRev, dirty, lastSeq}`. Pass `replica` again when reopening so `lastSeq` says which of its changes are in `text`. Only UTF-8 files (with or without BOM) open. |
| `buffer/edit` | notification, client → server | `{buffer, replica, baseRev, clientSeq, edits}`. At most one unacknowledged change per replica; `clientSeq` increases from 1 and a resend keeps it (the daemon drops seqs it has committed). |
| `buffer/editRejected` | notification, server → client | `{buffer, replica, clientSeq, error}`: the edit could not be applied (`REVISION_GONE`, `INVALID_EDIT`, `UNKNOWN_BUFFER`; reopen and resync), or a session token edited as a replica it did not open (`FORBIDDEN`). |
| `buffer/revertChange` | request | `{buffer, rev}` → `{rev}`: commit the inverse of one change (typically an agent's), transformed past every later change. |
| `buffer/save` | request | `{buffer, force?}` → `{savedRev}`. Atomic write (temp file + rename, permissions kept, BOM kept). If the file changed on disk since it was read, the change is merged in first and the save fails with `SAVE_CONFLICT` unless `force`. |
| `buffer/close` | request | `{buffer, replica}` → `null`. |

| Topic | Params | Events |
|---|---|---|
| `buffer/changes` | `{buffer}` | `{rev, origin: {kind, id?}, clientSeq, edits, dirty, conflict?}`. **`seq` equals `rev`.** `origin.kind` is `client` (`id` = replica), `agent` (`id` = session), `disk`, `format`, `revert` (`id` = reverted rev) or `save` (no edits). A replica's own change coming back is its acknowledgement. `conflict` marks a disk or agent change that overlapped unsaved edits (both were kept). Subscribers keep the buffer loaded; unsaved edits are journaled and survive daemon restarts. |

**Flow:** `buffer/open`, then subscribe to `buffer/changes` with `since: rev`. Reconnect: subscribe with `since: <last rev>` and resend the change in flight. On `RESUME_GAP` or `UNKNOWN_BUFFER` (the buffer was unloaded or the daemon restarted), reopen with the same `replica` and rebase pending edits onto the new snapshot (`ClientReplica::resync`).

**Disk changes:** while a buffer is open the daemon follows the workspace's `fs/changes`. A changed file is diffed against the text last read or written (not the live buffer) and merged around unsaved edits; it is attributed to an agent session when it matches a tool-call diff the agent host announced, else `origin.kind` is `disk`. A clean buffer simply reloads.

### Syntax (`syntax/*`)

The always-on syntax layer of an open buffer (PLAN §7.1.1, [ADR 0011](../docs/adr/0011-syntax-layer.md); types in `coqo_protocol::methods::syntax`, model in `coqo-syntax`). The daemon parses the buffer's current text, unsaved edits included, with tree-sitter and re-parses incrementally as changes land. No language server is started, so these work whether or not the language's ⚡ bolt is on. Every result has the `rev` it describes and the `language` id (`rust`, `typescript`, `tsx`, `javascript`, `java`, `kotlin`, `json`, `toml`, `markdown`, `yaml`, `html`, `css`, `xml`), or `null` and empty lists for other files. Ranges are `{start, end}` UTF-8 byte offsets at that revision; lines are 0-based and inclusive.

| Method | Params → result |
|---|---|
| `syntax/symbols` | `{buffer}` → `{rev, language, symbols}`. Each symbol is `{name, kind, range, nameRange, startLine, endLine, container?}`; `container` names the enclosing symbols, outermost first (`["shapes", "Circle"]` for a method `area` in `impl Circle` inside `mod shapes`). Document order, containers first. |
| `syntax/outline` | `{buffer}` → `{rev, language, items}`: the same symbols as a tree, each item with `children`. |
| `syntax/folding` | `{buffer}` → `{rev, language, ranges}`: `{startLine, endLine, kind}` with `kind` `region`, `comment` (runs of line comments fold together) or `imports`. |

`kind` is one of `module`, `class`, `interface`, `trait`, `struct`, `enum`, `enumMember`, `typeAlias`, `impl`, `function`, `method`, `constructor`, `field`, `constant`, `variable`, `macro`, `heading` (Markdown), `key` (JSON, YAML, TOML), `table` (TOML), `element` (HTML, XML) or `rule` (CSS). A session token may call these for buffers in its own workspace; other buffers are `FORBIDDEN`.

### Terminals (`terminal/*`)

`terminal/*` serves PTYs owned by the daemon (types in `coqo_protocol::methods::terminal`). The emulator state, meaning the grid, 10 000 lines of scrollback and the modes, stays in the daemon, so a terminal outlives the client that opened it.

| Method | Params → result |
|---|---|
| `terminal/spawn` | `{program?, args?, cwd?, env?, size, title?}` → `TerminalInfo`. Omit `program` for the login shell. |
| `terminal/list` | → every `TerminalInfo`, running or exited |
| `terminal/snapshot` | `{id}` → `{seq, info, screen}` |
| `terminal/input` | `{id, data}`: text, key escape sequences or a paste. Scrolls the view to the bottom. |
| `terminal/resize` | `{id, size: {cols, rows}}` |
| `terminal/scroll` | `{id, scroll}`, where `scroll` is `{"lines": n}` (positive goes up), `"pageUp"`, `"pageDown"`, `"top"` or `"bottom"` |
| `terminal/close` | `{id}` → `true` if it existed. Sends SIGHUP to the process group and the PTY's foreground job, then SIGKILL after 1.5 s. |
| topic `terminal/output` | params `{id}`. Events: `frame`, `title`, `cwd` (OSC 7), `commandFinished` (OSC 133;D), `bell` and `exited`. |

**A client mirrors the screen.** A `Screen` is the rows in view as sparse style runs (`{col, text, wide?, style?}`), plus the cursor, the modes (`appCursor`, `bracketedPaste`, `altScreen`, …), `display_offset` and `history_size`. To attach, take a snapshot, subscribe from its `seq`, and apply each `frame` to the snapshot's screen (`Screen::apply` in Rust). A frame carries only the rows whose content changed since the previous frame, together with the cursor and modes, and the daemon batches frames at about 60 Hz. Rows are absolute, so applying a frame twice does no harm. A frame with `full: true` follows a resize: the client starts over from blank rows.

**Scrollback is the daemon's.** `terminal/scroll` moves the terminal's one viewport, which every attached client shares, and the next frame shows it.

**Input order.** Requests on a connection run concurrently, so a client sends the next `terminal/input` only after the previous one has answered, merging keystrokes typed in the meantime. The UI does the same for resize and scroll.

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
| 1009 | `IO_ERROR` | Another I/O error. |
| 1010 | `ALREADY_EXISTS` | Something is already at the path (`fs/create`, `fs/rename`). |
| 1011 | `PERMISSION_DENIED` | The operating system refused: no permission for the path. |
| 1012 | `TRASH_FAILED` | The entry could not be moved to the OS trash and was left alone; delete it with `permanent`. |
| 1101 | `UNKNOWN_SESSION` | No agent session has that id. |
| 1102 | `UNKNOWN_PROVIDER` | No such agent provider, or it is not available. |
| 1103 | `SESSION_BUSY` | The session is already running a turn. |
| 1104 | `SESSION_CLOSED` | The session was closed. |
| 1105 | `UNKNOWN_PERMISSION_REQUEST` | No such pending permission request, or not one of its options. |
| 1106 | `UNSUPPORTED` | The provider does not support this. |
| 1107 | `AUTH_REQUIRED` | The agent needs the user to sign in, in the agent's own tool. Coqo never handles the login. |
| 1108 | `PROVIDER_ERROR` | The provider failed. |
| 1109 | `STORAGE_ERROR` | The session store failed. |
| 1201 | `UNKNOWN_BUFFER` | No open buffer has that id; open the file again. |
| 1202 | `UNSUPPORTED_ENCODING` | The file is not UTF-8. |
| 1203 | `REVISION_GONE` | The revision is no longer in the buffer's log (or is ahead of it); reopen. |
| 1204 | `INVALID_EDIT` | Unsorted, overlapping or out-of-range edits, or a split UTF-8 character. |
| 1205 | `SAVE_CONFLICT` | The file changed on disk; the change was merged, review and save again (or `force`). |
| 1301 | `NO_SUCH_TERMINAL` | No terminal has that id. |
| 1302 | `SPAWN_FAILED` | The PTY or the process could not be started. |
| 1701 | `UNKNOWN_WALKTHROUGH` | No walkthrough has that id. |
| 1702 | `INVALID_WALKTHROUGH` | The walkthrough draft has problems; `data` is `{problems: [{path, message}]}`, every problem with its JSON path. |

### Walkthroughs (`walkthroughs/*`)

Code walkthroughs agents publish after a change (PLAN §7.6, [ADR 0020](../docs/adr/0020-walkthroughs.md)). Agents write the draft that [`schema/walkthrough.schema.json`](schema/walkthrough.schema.json) describes (generated from `WalkthroughDraft`; also the MCP resource `coqo://schemas/walkthrough.json`).

| Method | Params | Result |
|---|---|---|
| `walkthroughs/list` | `{workspace, session?}` | `{walkthroughs: WalkthroughSummary[]}`, newest first: `id`, `workspace`, `session`, `title`, `scope` (kind), `steps`, `understood`, `questions`, `start` (`{root, path, lines}` of the first step's first anchor, 0-based), `storage` (`local` or `committed`), `createdAt`. |
| `walkthroughs/get` | `{id}` | `Walkthrough`: `id`, `workspace`, `title`, `summary`, `scope {kind, root, base?, head?, baseCommit?, headCommit?}`, `acceptanceCriteria [{id, text, status?}]`, `steps [{id, title, kind, anchors [{anchor: CodeAnchor, resolution}], what, why, risks, criteriaRefs, dependsOn}]`, `trivialHunks [{root, path, lines?, reason?}]`, `createdBy {session, agent}`, `createdAt`, `storage {mode, path?, error?}`, `stepStates`. Anchors are resolved against the files as they are now. |
| `walkthroughs/setStepState` | `{id, step, state}` | `WalkthroughSummary`. `state` is `unread`, `understood` or `question`. |
| `walkthroughs/delete` | `{id, deleteFile?}` | `true` if it existed. The committed file is deleted only with `deleteFile`. |
| `walkthroughs/create` | `{workspace, session, walkthrough}` | `Walkthrough`. `walkthrough` is the draft as the agent wrote it; every problem is reported at once as `INVALID_WALKTHROUGH`. A session token may call it for its own workspace and session. In a workspace whose state has `walkthroughs.storage = "committed"`, it is also written to `.coqo/walkthroughs/<id>.json` in the scope's root (never committed). |

| Topic | Params | Events |
|---|---|---|
| `walkthroughs/changes` | `{workspace?}` | `{kind: published \| updated \| deleted, id, workspace, summary?}`. Live only: list again after subscribing. |
