// SPDX-License-Identifier: Apache-2.0
// Generated from crates/protocol by `COQO_BLESS=1 cargo test -p coqo-protocol schema`.
// Do not edit. Types for the Coqo Protocol, for tooling and future clients.

export const PROTOCOL_VERSION = { major: 0, minor: 0 } as const;

/** JSON-RPC 2.0 error codes used by the protocol. */
export const ErrorCodes = {
  FILE_TOO_LARGE: 1005,
  INCOMPATIBLE_PROTOCOL: -32001,
  INTERNAL_ERROR: -32603,
  INVALID_PARAMS: -32602,
  INVALID_REQUEST: -32600,
  IO_ERROR: 1009,
  METHOD_NOT_FOUND: -32601,
  NOT_A_DIRECTORY: 1006,
  NOT_A_FILE: 1007,
  NOT_FOUND: 1004,
  NOT_INITIALIZED: -32002,
  PARSE_ERROR: -32700,
  PATH_OUTSIDE_WORKSPACE: 1003,
  REQUEST_CANCELLED: -32005,
  RESUME_GAP: -32003,
  UNKNOWN_ROOT: 1002,
  UNKNOWN_SUBSCRIPTION: -32004,
  UNKNOWN_WORKSPACE: 1001,
  WORKSPACE_CLOSED: 1008,
} as const;

export type Id = number | string;

export interface Request<P = unknown> { jsonrpc: "2.0"; id: Id; method: string; params?: P }
export interface Notification<P = unknown> { jsonrpc: "2.0"; method: string; params?: P }
export type Response<R = unknown> =
  | { jsonrpc: "2.0"; id: Id; result: R }
  | { jsonrpc: "2.0"; id: Id | null; error: ErrorObject };
export type Message = Request | Notification | Response;

/** Params of [`AddRoot`]. */
export type AddRootParams = {
  /** The workspace. */
  id: WorkspaceId;
  /** Display name (default: the folder name). */
  name?: string | null;
  /** Folder to add. Adding a folder that is already a root is a no-op. */
  path: string;
};

/** Params of the [`Changes`] topic. */
export type ChangesParams = {
  /** The workspace to watch. It must be open. */
  workspace: WorkspaceId;
};

/** Result of `daemon/status`. */
export type DaemonStatus = {
  /** The daemon's build id (also in the handshake's `Welcome.build`). */
  build: string;
  /** What keeps the daemon busy (agent sessions, traces, …). Busy work keeps the daemon alive with no clients attached. */
  busy: Array<string>;
  /** Clients attached right now, including the one asking. */
  connections: number;
  /** The idle policy and where the daemon is in it. */
  idle: IdleStatus;
  /** The daemon's process id. */
  pid: number;
  /** The protocol version the daemon speaks. */
  protocol: ProtocolVersion;
  /** When the daemon started, in milliseconds since the Unix epoch. */
  startedAtMs: number;
  /** How long the daemon has been running, in milliseconds. */
  uptimeMs: number;
  /** The daemon's version, e.g. `0.1.0`. */
  version: string;
};

/** One entry of a directory listing. */
export type DirEntry = {
  /** Whether ignore rules exclude it. */
  ignored: boolean;
  /** What it is. For a symlink, the kind of its target. */
  kind: EntryKind;
  /** Last modification, in Unix milliseconds, if known. */
  mtime?: number | null;
  /** File name (the last path component). */
  name: string;
  /** Size in bytes (of the target, for a symlink; 0 for directories). */
  size: number;
  /** Whether the entry itself is a symlink. */
  symlink: boolean;
};

/** How a frame's payload is encoded.  JSON is mandatory and is always used for the handshake. MessagePack is optional and is switched on for the rest of the connection when both sides list it in the handshake; it is intended for hot paths (buffer ops, trace streams). */
export type Encoding = "json" | "msgpack";

/** What a path points at. */
export type EntryKind = "file" | "dir" | "other";

/** A JSON-RPC 2.0 error object (§5.1). */
export type ErrorObject = {
  /** One of [`codes`] or an application-defined code. */
  code: number;
  /** Structured details, if any. */
  data?: unknown;
  /** A short human-readable description. */
  message: string;
};

/** Params of `coqo/event`. */
export type EventParams = {
  /** The event, shaped by the topic. */
  event: unknown;
  /** The event's sequence number. */
  seq: number;
  /** The subscription this event belongs to. */
  subscription: SubscriptionId;
};

/** Params of the [`Events`] topic. */
export type EventsParams = {
  /** Only events of this workspace; omit for every workspace. */
  workspace?: WorkspaceId | null;
};

/** Result of [`Stat`]. */
export type FileStat = {
  /** Whether ignore rules exclude it. */
  ignored: boolean;
  /** What it is. For a symlink, the kind of its target. */
  kind: EntryKind;
  /** Last modification, in Unix milliseconds, if known. */
  mtime?: number | null;
  /** Size in bytes (0 for directories). */
  size: number;
  /** Whether the path itself is a symlink. */
  symlink: boolean;
};

/** `data` of a [`FILE_TOO_LARGE`](super::codes::FILE_TOO_LARGE) error. */
export type FileTooLarge = {
  /** The limit that applied. */
  limit: number;
  /** The file's size in bytes. */
  size: number;
};

/** One event of the [`Changes`] topic: paths under one root that changed during a debounce window (about 100 ms).  Paths are reported, not operations: the client re-lists or re-stats them (a path that no longer exists was removed). Changes deep inside ignored directories (such as `target/` or `node_modules/`) and inside `.git` are not reported, but an ignored entry appearing or disappearing directly in a non-ignored directory is. */
export type FsChanges = {
  /** Changed paths, sorted and without duplicates. Empty when `rescan`. */
  paths: Array<string>;
  /** Too much changed to report individually (or the watcher lost events): re-list everything under this root that the client shows. */
  rescan: boolean;
  /** The root the paths are relative to. */
  root: RootId;
};

/** Params of `coqo/hello`, sent by the client. */
export type Hello = {
  /** The client's build id (version + commit), shown in errors and logs. */
  build: string;
  /** What the client is, e.g. `coqo-ui`, `coqo-cli`, `coqod-mcp`. */
  client: string;
  /** Encodings the client accepts after the handshake, in preference order. JSON is always accepted, even if not listed. */
  encodings?: Array<Encoding>;
  /** The client's protocol version. */
  protocol: ProtocolVersion;
};

/** Where the daemon is in its idle policy. */
export type IdlePhase = "active" | "idle" | "servicesStopped";

/** The idle policy (PLAN §4.3) and the daemon's current idle phase. */
export type IdleStatus = {
  /** Seconds with no clients and nothing busy before the daemon exits. `null` means never ("always on"). */
  exitAfterSecs?: number | null;
  /** The current phase. */
  phase: IdlePhase;
  /** Seconds with no clients and nothing busy before idle services (such as language servers) stop. `null` means never. */
  stopServicesAfterSecs?: number | null;
};

/** `data` of the [`codes::INCOMPATIBLE_PROTOCOL`] error. */
export type IncompatibleProtocol = {
  /** The server's build id. */
  build: string;
  /** The server's protocol version. */
  protocol: ProtocolVersion;
};

/** Result of [`ListDir`]. */
export type ListDirResult = {
  /** Entries: directories first, then files, each sorted by name. */
  entries: Array<DirEntry>;
};

/** Result of [`List`]: a snapshot of every known workspace. */
export type ListResult = {
  /** The event-log `seq` this snapshot reflects. Subscribe to [`Events`] with `since` set to it to get every later change. */
  seq: number;
  /** Every known workspace, most recently opened first. */
  workspaces: Array<WorkspaceInfo>;
};

/** Params of [`Open`]. Give exactly one of `path` and `id`. */
export type OpenParams = {
  /** Reopen a known workspace. */
  id?: WorkspaceId | null;
  /** Name for a newly created workspace (default: the folder name). */
  name?: string | null;
  /** Open the workspace for this folder, creating it if needed. A workspace whose first root is this folder (after canonicalising the path) is reused, so opening the same folder twice is idempotent. */
  path?: string | null;
};

/** Params naming a path in a workspace. */
export type PathParams = {
  /** `/`-separated path relative to the root; `""` for the root itself. */
  path?: string;
  /** The root the path is relative to. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** A protocol version: `major.minor`. */
export type ProtocolVersion = {
  /** Incremented for breaking changes. */
  major: number;
  /** Incremented for additive changes. */
  minor: number;
};

/** Params of [`ReadFile`]. */
export type ReadParams = {
  /** Fail with [`FILE_TOO_LARGE`](super::codes::FILE_TOO_LARGE) if the file is larger. Default [`DEFAULT_READ_LIMIT`], at most [`MAX_READ_LIMIT`]. */
  maxBytes?: number | null;
  /** `/`-separated path relative to the root. */
  path: string;
  /** The root the path is relative to. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`ReadFile`]. */
export type ReadResult = {
  /** Whether the file is not valid UTF-8 (then `text` is absent). */
  binary: boolean;
  /** Last modification, in Unix milliseconds, if known. */
  mtime?: number | null;
  /** Size in bytes. */
  size: number;
  /** The contents, if they are valid UTF-8. */
  text?: string | null;
};

/** Params of [`RemoveRoot`]. */
export type RemoveRootParams = {
  /** The workspace. */
  id: WorkspaceId;
  /** The root to remove. The last root cannot be removed. */
  root: RootId;
};

/** Params of [`Rename`]. */
export type RenameParams = {
  /** The workspace. */
  id: WorkspaceId;
  /** The new display name (not empty). */
  name: string;
};

/** `data` of a [`RESUME_GAP`](codes::RESUME_GAP) error. */
export type ResumeGap = {
  /** The source's latest sequence number. */
  head: number;
  /** The first sequence number the subscriber needed. */
  needed: number;
  /** The oldest sequence number still available (`head + 1` if none). */
  oldest: number;
};

/** Identifies a root within its workspace. Stable for the root's lifetime and never reused within the workspace. */
export type RootId = number;

/** Params of [`SetState`]. */
export type SetStateParams = {
  /** The workspace. */
  id: WorkspaceId;
  /** The entry to set (1–128 characters). */
  key: string;
  /** The new value; `null` removes the entry. */
  value: unknown;
};

/** Params of `daemon/shutdown`. */
export type ShutdownParams = {
  /** Stop even if work is busy. */
  force?: boolean;
};

/** Result of `daemon/shutdown`. */
export type ShutdownResult = {
  /** When not stopping: the busy work that kept it running. */
  busy: Array<string>;
  /** Whether the daemon is stopping. It closes every connection and exits shortly after answering. */
  stopping: boolean;
};

/** Params of `coqo/subscribe`. */
export type SubscribeParams = {
  /** Topic-specific params (`null` if none). */
  params?: unknown;
  /** Resume after this sequence number; omit for live events only. */
  since?: number | null;
  /** Topic name. */
  topic: string;
};

/** Result of `coqo/subscribe`. */
export type SubscribeResult = {
  /** The source's latest sequence number when the subscription started. */
  head: number;
  /** Id used in `coqo/event` notifications and `coqo/unsubscribe`. */
  subscription: SubscriptionId;
};

/** Params of `coqo/subscriptionEnded`. */
export type SubscriptionEndedParams = {
  /** Why, if it ended abnormally. */
  error?: ErrorObject | null;
  /** The subscription that ended. */
  subscription: SubscriptionId;
};

/** Identifies a subscription on one connection. */
export type SubscriptionId = number;

/** Params of `coqo/unsubscribe`. */
export type UnsubscribeParams = {
  /** The subscription to stop. */
  subscription: SubscriptionId;
};

/** Result of `coqo/hello`, sent by the server. */
export type Welcome = {
  /** The server's build id. Equal builds are compatible by definition; different builds are compatible when the protocol versions are. */
  build: string;
  /** A server-assigned id for this connection, unique per server process. */
  connection: number;
  /** The encoding both sides use from the next frame on. */
  encoding: Encoding;
  /** The negotiated version for this connection. */
  negotiated: ProtocolVersion;
  /** The server's protocol version. */
  protocol: ProtocolVersion;
  /** What the server is, e.g. `coqod`. */
  server: string;
};

/** One event of the [`Events`] topic. */
export type WorkspaceEvent = {
  type: "opened";
  /** The workspace after the change. */
  workspace: WorkspaceInfo;
} | {
  type: "closed";
  /** The workspace after the change. */
  workspace: WorkspaceInfo;
} | {
  type: "changed";
  /** The workspace after the change. */
  workspace: WorkspaceInfo;
} | {
  /** The removed workspace. */
  id: WorkspaceId;
  type: "removed";
} | {
  /** The workspace. */
  id: WorkspaceId;
  /** The entry. */
  key: string;
  type: "stateChanged";
  /** The new value; `null` if it was removed. */
  value: unknown;
};

/** Stable identifier of a workspace, e.g. `ws-3f2a9c1d0b7e4a65`. */
export type WorkspaceId = string;

/** A workspace as the daemon knows it. */
export type WorkspaceInfo = {
  /** When the workspace was created, in Unix milliseconds. */
  createdAt: number;
  /** Stable id. */
  id: WorkspaceId;
  /** When the workspace was last opened, in Unix milliseconds. */
  lastOpenedAt: number;
  /** Display name. */
  name: string;
  /** Whether the workspace is open (watched, shown in the rail). */
  open: boolean;
  /** Folder roots, in display order. Never empty. */
  roots: Array<WorkspaceRoot>;
};

/** Params naming one workspace. */
export type WorkspaceParams = {
  /** The workspace. */
  id: WorkspaceId;
};

/** One folder root of a workspace. */
export type WorkspaceRoot = {
  /** The root's id within the workspace. */
  id: RootId;
  /** Display name (the folder name by default). */
  name: string;
  /** Absolute, canonical path of the folder. */
  path: string;
};

/** A workspace's persisted state document, e.g. `tabs`, `layout`, `settings`. The keys and values belong to the UI; the daemon only stores them. */
export type WorkspaceState = {
  /** State entries by key. */
  entries: Record<string, unknown>;
  /** The event-log `seq` this snapshot reflects (see [`ListResult::seq`]). */
  seq: number;
};

/** Requests: method name → params and result. */
export interface Requests {
  /** Handshake. Always the first request, always JSON. (client→server) */
  "coqo/hello": { params: Hello; result: Welcome };
  /** Start a subscription to a topic. (client→server) */
  "coqo/subscribe": { params: SubscribeParams; result: SubscribeResult };
  /** Stop a subscription. Result is true if it existed. (client→server) */
  "coqo/unsubscribe": { params: UnsubscribeParams; result: boolean };
  /** Daemon version, pid, uptime, attached clients, busy work and idle policy. (client→server) */
  "daemon/status": { params: null; result: DaemonStatus };
  /** Stop the daemon. Refused while work is busy unless `force` is set. (client→server) */
  "daemon/shutdown": { params: ShutdownParams; result: ShutdownResult };
  /** Open (creating if needed) the workspace for a folder, or reopen one by id. (client→server) */
  "workspace/open": { params: OpenParams; result: WorkspaceInfo };
  /** Every known workspace, with the event-log seq the snapshot reflects. (client→server) */
  "workspace/list": { params: null; result: ListResult };
  /** One workspace. (client→server) */
  "workspace/get": { params: WorkspaceParams; result: WorkspaceInfo };
  /** Close a workspace (stops its watchers). (client→server) */
  "workspace/close": { params: WorkspaceParams; result: WorkspaceInfo };
  /** Forget a workspace and its daemon-side data. Result is true if it existed. (client→server) */
  "workspace/remove": { params: WorkspaceParams; result: boolean };
  /** Add a folder root to a workspace. (client→server) */
  "workspace/addRoot": { params: AddRootParams; result: WorkspaceInfo };
  /** Remove a folder root from a workspace. (client→server) */
  "workspace/removeRoot": { params: RemoveRootParams; result: WorkspaceInfo };
  /** Rename a workspace. (client→server) */
  "workspace/rename": { params: RenameParams; result: WorkspaceInfo };
  /** A workspace's persisted state document (tabs, layout, settings overrides). (client→server) */
  "workspace/getState": { params: WorkspaceParams; result: WorkspaceState };
  /** Set (or, with null, remove) one workspace state entry. (client→server) */
  "workspace/setState": { params: SetStateParams; result: null };
  /** List a directory in a workspace root. (client→server) */
  "fs/list": { params: PathParams; result: ListDirResult };
  /** Metadata of one path in a workspace root. (client→server) */
  "fs/stat": { params: PathParams; result: FileStat };
  /** Read a file in a workspace root, up to a size limit. (client→server) */
  "fs/read": { params: ReadParams; result: ReadResult };
}

/** Notifications: method name → params. */
export interface Notifications {
  /** One event of a subscription. (server→client) */
  "coqo/event": { params: EventParams };
  /** The server ended a subscription, with an error if abnormal. (server→client) */
  "coqo/subscriptionEnded": { params: SubscriptionEndedParams };
}

/** Topics (`coqo/subscribe`): topic name → params and event. */
export interface Topics {
  /** Workspace changes, from the persistent event log (resumable with since). */
  "workspace/events": { params: EventsParams; event: WorkspaceEvent };
  /** Debounced file-system changes in an open workspace (live only). */
  "fs/changes": { params: ChangesParams; event: FsChanges };
}

/** Encode one frame: u32 big-endian length, then the JSON payload. */
export function encodeFrame(message: Message): Uint8Array {
  const body = new TextEncoder().encode(JSON.stringify(message));
  const frame = new Uint8Array(4 + body.length);
  new DataView(frame.buffer).setUint32(0, body.length, false);
  frame.set(body, 4);
  return frame;
}
