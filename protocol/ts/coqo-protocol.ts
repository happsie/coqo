// SPDX-License-Identifier: Apache-2.0
// Generated from crates/protocol by `COQO_BLESS=1 cargo test -p coqo-protocol schema`.
// Do not edit. Types for the Coqo Protocol, for tooling and future clients.

export const PROTOCOL_VERSION = { major: 0, minor: 0 } as const;

/** JSON-RPC 2.0 error codes used by the protocol. */
export const ErrorCodes = {
  INCOMPATIBLE_PROTOCOL: -32001,
  INTERNAL_ERROR: -32603,
  INVALID_PARAMS: -32602,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  NOT_INITIALIZED: -32002,
  PARSE_ERROR: -32700,
  REQUEST_CANCELLED: -32005,
  RESUME_GAP: -32003,
  UNKNOWN_SUBSCRIPTION: -32004,
} as const;

export type Id = number | string;

export interface Request<P = unknown> { jsonrpc: "2.0"; id: Id; method: string; params?: P }
export interface Notification<P = unknown> { jsonrpc: "2.0"; method: string; params?: P }
export type Response<R = unknown> =
  | { jsonrpc: "2.0"; id: Id; result: R }
  | { jsonrpc: "2.0"; id: Id | null; error: ErrorObject };
export type Message = Request | Notification | Response;

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

/** How a frame's payload is encoded.  JSON is mandatory and is always used for the handshake. MessagePack is optional and is switched on for the rest of the connection when both sides list it in the handshake; it is intended for hot paths (buffer ops, trace streams). */
export type Encoding = "json" | "msgpack";

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

/** A protocol version: `major.minor`. */
export type ProtocolVersion = {
  /** Incremented for breaking changes. */
  major: number;
  /** Incremented for additive changes. */
  minor: number;
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
}

/** Notifications: method name → params. */
export interface Notifications {
  /** One event of a subscription. (server→client) */
  "coqo/event": { params: EventParams };
  /** The server ended a subscription, with an error if abnormal. (server→client) */
  "coqo/subscriptionEnded": { params: SubscriptionEndedParams };
}

/** Encode one frame: u32 big-endian length, then the JSON payload. */
export function encodeFrame(message: Message): Uint8Array {
  const body = new TextEncoder().encode(JSON.stringify(message));
  const frame = new Uint8Array(4 + body.length);
  new DataView(frame.buffer).setUint32(0, body.length, false);
  frame.set(body, 4);
  return frame;
}
