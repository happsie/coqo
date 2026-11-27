// SPDX-License-Identifier: Apache-2.0
// Generated from crates/protocol by `COQO_BLESS=1 cargo test -p coqo-protocol schema`.
// Do not edit. Types for the Coqo Protocol, for tooling and future clients.

export const PROTOCOL_VERSION = { major: 0, minor: 1 } as const;

/** JSON-RPC 2.0 error codes used by the protocol. */
export const ErrorCodes = {
  AUTH_REQUIRED: 1107,
  FILE_TOO_LARGE: 1005,
  FORBIDDEN: -32007,
  INCOMPATIBLE_PROTOCOL: -32001,
  INTERNAL_ERROR: -32603,
  INVALID_EDIT: 1204,
  INVALID_PARAMS: -32602,
  INVALID_REQUEST: -32600,
  IO_ERROR: 1009,
  METHOD_NOT_FOUND: -32601,
  NOT_A_DIRECTORY: 1006,
  NOT_A_FILE: 1007,
  NOT_FOUND: 1004,
  NOT_INITIALIZED: -32002,
  NO_SUCH_TERMINAL: 1301,
  PARSE_ERROR: -32700,
  PATH_OUTSIDE_WORKSPACE: 1003,
  PROVIDER_ERROR: 1108,
  REQUEST_CANCELLED: -32005,
  RESUME_GAP: -32003,
  REVISION_GONE: 1203,
  SAVE_CONFLICT: 1205,
  SESSION_BUSY: 1103,
  SESSION_CLOSED: 1104,
  SPAWN_FAILED: 1302,
  STORAGE_ERROR: 1109,
  UNAUTHORIZED: -32006,
  UNKNOWN_BUFFER: 1201,
  UNKNOWN_PERMISSION_REQUEST: 1105,
  UNKNOWN_PROVIDER: 1102,
  UNKNOWN_ROOT: 1002,
  UNKNOWN_SESSION: 1101,
  UNKNOWN_SUBSCRIPTION: -32004,
  UNKNOWN_WORKSPACE: 1001,
  UNSUPPORTED: 1106,
  UNSUPPORTED_ENCODING: 1202,
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

/** What an agent session is doing. */
export type AgentState = "idle" | "working" | "needs_input" | "needs_permission" | "blocked" | "error" | "done";

/** Where the agent's usage is billed (from ACP `auth/status`). */
export type AuthKind = "account" | "api_key" | "gateway" | "external" | "none" | "unknown";

/** The billing source, for the usage indicator. Carries no identity: Coqo drops account emails and organizations. */
export type AuthStatus = {
  /** What kind of credential is in use. */
  kind: AuthKind;
  /** Display label, such as `Claude Max`. */
  label?: string | null;
  /** Plan name, if a plan login. */
  plan?: string | null;
};

/** A slash command or skill the agent offers (ACP `available_commands`). */
export type AvailableCommand = {
  /** What it does. */
  description?: string;
  /** Placeholder for its argument, if it takes one. */
  inputHint?: string | null;
  /** Name without the slash. */
  name: string;
};

/** One event of the [`Changes`] topic: a committed change. The event's subscription `seq` equals `rev`. */
export type BufferChange = {
  /** The replica's `client_seq` for client changes, else 0. */
  clientSeq?: number;
  /** A disk or agent change that overlapped unsaved edits; both were kept, and the user should look at the result. */
  conflict?: boolean;
  /** Whether the text differs from the file on disk after this change. */
  dirty: boolean;
  /** The edits, sorted and non-overlapping, against the text at `rev - 1`. */
  edits: Array<TextEdit>;
  /** Who made it. */
  origin: Origin;
  /** The revision this change produced (the previous one plus 1). */
  rev: number;
};

/** Identifies an open buffer in the running daemon. Not stable across daemon restarts or after a buffer is unloaded: reopen by path then. */
export type BufferId = number;

/** A byte range of the buffer text. */
export type ByteRange = {
  /** One past the last byte. */
  end: number;
  /** First byte. */
  start: number;
};

/** Params of the [`Changes`] topic. */
export type ChangesParams = {
  /** The workspace to watch. It must be open. */
  workspace: WorkspaceId;
};

/** Params of `sessions/changes`. */
export type ChangesParams2 = {
  /** Only this workspace's sessions. */
  workspace?: string | null;
};

/** Params of the [`Changes`] topic. */
export type ChangesParams3 = {
  /** The buffer. */
  buffer: BufferId;
};

/** Params of [`Close`]. */
export type CloseParams = {
  /** The buffer. */
  buffer: BufferId;
  /** The replica going away. */
  replica: ReplicaId;
};

/** A cell colour. Indexed colours are resolved by the client's theme. */
export type Color = "default" | {
  indexed: number;
} | {
  rgb: [number, number, number];
};

/** A piece of message content (ACP `ContentBlock`). */
export type ContentBlock = {
  /** The text. */
  text: string;
  type: "text";
} | {
  /** Base64 data. */
  data: string;
  /** MIME type, such as `image/png`. */
  mimeType: string;
  type: "image";
  /** Where it came from, if anywhere. */
  uri?: string | null;
} | {
  /** MIME type. */
  mimeType?: string | null;
  /** Display name. */
  name: string;
  /** Size in bytes. */
  size?: number | null;
  type: "resource_link";
  /** The resource's URI, such as `file:///src/app/main.rs`. */
  uri: string;
} | {
  /** MIME type. */
  mimeType?: string | null;
  /** Its text. */
  text: string;
  type: "resource";
  /** The resource's URI. */
  uri: string;
};

/** A cost figure. */
export type Cost = {
  /** Amount. */
  amount: number;
  /** ISO currency code, such as `USD`. */
  currency: string;
};

/** Params of `sessions/create`. */
export type CreateSessionParams = {
  /** The agent's working directory (absolute; the workspace root). */
  cwd: string;
  /** Initial mode id, if the provider has modes. */
  mode?: string | null;
  /** Provider id, from `agents/providers`. */
  provider: string;
  /** Title. */
  title?: string | null;
  /** The workspace it belongs to. */
  workspace?: string | null;
};

/** The cursor, in screen coordinates. */
export type Cursor = {
  col: number;
  row: number;
  shape?: CursorShape;
};

/** Cursor shapes. */
export type CursorShape = "block" | "underline" | "beam" | "hollowBlock";

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

/** Params of the [`Edit`] notification: one change made by a replica. */
export type EditParams = {
  /** The revision the edits were made against. */
  baseRev: number;
  /** The buffer. */
  buffer: BufferId;
  /** Per-replica sequence number, strictly increasing from 1. Resending a change keeps its seq; the daemon ignores seqs it has committed. */
  clientSeq: number;
  /** The edits, sorted and non-overlapping, against the text at `base_rev`. */
  edits: Array<TextEdit>;
  /** The replica that made the change. */
  replica: ReplicaId;
};

/** Params of the [`EditRejected`] notification. */
export type EditRejectedParams = {
  /** The buffer. */
  buffer: BufferId;
  /** The rejected change's seq. */
  clientSeq: number;
  /** Why, e.g. [`REVISION_GONE`](super::codes::REVISION_GONE) or [`INVALID_EDIT`](super::codes::INVALID_EDIT). */
  error: ErrorObject;
  /** The replica whose change was rejected. */
  replica: ReplicaId;
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

/** What a fold holds (LSP's `FoldingRangeKind`). */
export type FoldKind = "region" | "comment" | "imports";

/** Lines that can be folded: `start_line` stays visible, the lines after it through `end_line` are hidden. */
export type FoldingRange = {
  /** Last line folded (0-based, inclusive). */
  endLine: number;
  /** What the fold holds. */
  kind: FoldKind;
  /** First line (0-based). */
  startLine: number;
};

/** Result of [`Folding`]. */
export type FoldingResult = {
  /** The language id, or `null` for plain text. */
  language?: string | null;
  /** The ranges, sorted by start line, at most one per line. */
  ranges: Array<FoldingRange>;
  /** The revision the ranges describe. */
  rev: number;
};

/** What changed on screen since the previous frame. */
export type Frame = {
  cursor?: Cursor | null;
  display_offset?: number;
  /** The screen starts over (after a resize): rows left out are blank. */
  full?: boolean;
  history_size?: number;
  modes?: Modes;
  /** The changed rows. */
  rows?: Array<RowUpdate>;
  size: TermSize;
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
  /** The session token that authenticates this client (since 0.1). A server that authenticates (`coqod` does) rejects a missing or wrong one with [`codes::UNAUTHORIZED`]. Never logged; servers drop it from the [`Hello`] they hand to request handlers. */
  token?: SessionToken | null;
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

/** Params of [`Input`]. */
export type InputParams = {
  /** Bytes for the PTY, as text: typed characters, escape sequences for keys, or pasted text (already wrapped for bracketed paste). */
  data: string;
  id: TerminalId;
};

/** The file's line-ending convention, detected from its first line break.  The buffer text keeps line breaks exactly as they are on disk (mixed files round-trip unchanged); this tells the editor which break to insert. */
export type LineEnding = "lf" | "crlf";

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

/** Params of `sessions/list`. */
export type ListSessionsParams = {
  /** Include closed sessions. */
  includeClosed?: boolean;
  /** Only this workspace's sessions. */
  workspace?: string | null;
};

/** Terminal modes that change how a client encodes input. */
export type Modes = {
  /** The alternate screen is active (full-screen programs such as `top`); it has no scrollback, so a client turns wheel scrolling into arrow keys. */
  altScreen?: boolean;
  /** DECCKM: arrow keys send `ESC O A` instead of `ESC [ A`. */
  appCursor?: boolean;
  /** DECKPAM: application keypad. */
  appKeypad?: boolean;
  /** Wrap pasted text in `ESC [200~` … `ESC [201~`. */
  bracketedPaste?: boolean;
  /** The program asked for mouse reports. */
  mouse?: boolean;
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

/** Params of [`Open`]. */
export type OpenParams2 = {
  /** `/`-separated path of a file relative to the root. */
  path: string;
  /** Reuse this replica id (when reopening after a reconnect or a resume gap), so the daemon reports which of its changes it already has. */
  replica?: ReplicaId | null;
  /** The root the path is relative to. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`Open`]: a snapshot of the buffer. */
export type OpenResult = {
  /** The buffer. The same file opened twice (from two windows, or through a symlink) is the same buffer. */
  buffer: BufferId;
  /** Whether the text differs from the file on disk. */
  dirty: boolean;
  /** Encoding on disk. */
  encoding: TextEncoding;
  /** The highest `client_seq` of `replica` the daemon has committed (0 if none): a pending change with a seq at or below it is already in `text`. */
  lastSeq: number;
  /** Line-ending convention. */
  lineEnding: LineEnding;
  /** This replica's id: the one asked for, or a new one. */
  replica: ReplicaId;
  /** The revision `text` reflects. Subscribe to [`Changes`] with `since` set to it. */
  rev: number;
  /** Revision of the last save or reload from disk. */
  savedRev: number;
  /** The buffer text, including unsaved edits. */
  text: string;
};

/** Who made a change, with an id whose meaning depends on the kind. */
export type Origin = {
  /** The source's id, if it has one. */
  id?: string | null;
  /** What kind of source. */
  kind: OriginKind;
};

/** Who made a change. */
export type OriginKind = "client" | "agent" | "disk" | "format" | "revert" | "save";

/** A symbol and the symbols inside it. */
export type OutlineItem = {
  /** The symbols directly inside it, in document order. */
  children?: Array<OutlineItem>;
  /** Last line (0-based, inclusive). */
  endLine: number;
  /** What it is. */
  kind: SymbolKind;
  /** The name. */
  name: string;
  /** The name. */
  nameRange: ByteRange;
  /** The whole definition. */
  range: ByteRange;
  /** First line (0-based). */
  startLine: number;
};

/** Result of [`Outline`]. */
export type OutlineResult = {
  /** The top-level symbols. */
  items: Array<OutlineItem>;
  /** The language id, or `null` for plain text. */
  language?: string | null;
  /** The revision the outline describes. */
  rev: number;
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

/** One answer to a permission request. */
export type PermissionOption = {
  /** Option id, passed back in the outcome. */
  id: string;
  /** What it means. */
  kind: PermissionOptionKind;
  /** Label. */
  name: string;
};

/** What choosing a permission option means (ACP `PermissionOptionKind`). Auto-approve rules select options by kind, never by label. */
export type PermissionOptionKind = "allow_once" | "allow_always" | "reject_once" | "reject_always";

/** The answer to a permission request. */
export type PermissionOutcome = {
  /** The option's id. */
  optionId: string;
  outcome: "selected";
} | {
  outcome: "cancelled";
};

/** The agent asks the user to allow something, usually a tool call. */
export type PermissionRequest = {
  /** Id within the session, for `sessions/respondPermission`. */
  id: PermissionRequestId;
  /** The possible answers. */
  options: Array<PermissionOption>;
  /** The workspace rule that answering with `remember: true` creates. Only offered for requests a rule may answer (a simple command, or files inside the workspace's roots), and only in a workspace. */
  suggestedRule?: PermissionRuleSpec | null;
  /** What is asked, such as `Write pong.txt`. */
  title: string;
  /** The tool call it is about, with its diff if it edits a file. */
  toolCall?: ToolCallUpdate | null;
};

/** Identifies a permission request within a session. */
export type PermissionRequestId = number;

/** A per-workspace auto-approve (or auto-reject) rule. Rules answer a permission request before it reaches the UI; a request no rule matches is asked. A reject rule wins over an allow rule. */
export type PermissionRule = {
  /** Created, Unix milliseconds. */
  createdAt: number;
  /** Allow or reject. */
  decision: RuleDecision;
  /** Id. */
  id: PermissionRuleId;
  /** The command prefix or path glob. */
  pattern: string;
  /** The tool kind: `execute`, `edit` or `read`. */
  tool: ToolKind;
  /** The workspace it belongs to. */
  workspace: string;
};

/** Identifies a workspace permission rule. Generated by the daemon. */
export type PermissionRuleId = number;

/** What a permission rule matches.  - `execute`: `pattern` is a command prefix, word by word (`cargo test`   matches `cargo test -p shop` but not `cargo testing`). Only simple   commands match: no shell operators, redirections, substitutions,   globs or `~`, no `..`, every absolute path inside the workspace, and   never programs such as `rm`, `sudo`, `curl` or a shell. - `edit` and `read`: `pattern` is a glob over paths relative to a   workspace root (`src/**`, `docs/*.md`); every file the request touches   must be inside the workspace's roots and match. */
export type PermissionRuleSpec = {
  /** The command prefix or path glob. */
  pattern: string;
  /** The tool kind: `execute`, `edit` or `read`. */
  tool: ToolKind;
};

/** One entry of the agent's plan (todo list). */
export type PlanEntry = {
  /** What the step is. */
  content: string;
  /** Priority. */
  priority?: PlanPriority;
  /** Status. */
  status?: PlanEntryStatus;
};

/** Status of a plan entry. */
export type PlanEntryStatus = "pending" | "in_progress" | "completed";

/** Priority of a plan entry. */
export type PlanPriority = "high" | "medium" | "low";

/** Params of `sessions/prompt`. */
export type PromptParams = {
  /** The prompt: text, `@`-mentions as resource links, embedded selections. */
  content: Array<ContentBlock>;
  /** The session. */
  session: SessionId;
};

/** Result of `sessions/prompt`. */
export type PromptResult = {
  /** Seq of the turn's `user_message` update. */
  seq: number;
  /** The turn number. */
  turn: number;
};

/** A protocol version: `major.minor`. */
export type ProtocolVersion = {
  /** Incremented for breaking changes. */
  major: number;
  /** Incremented for additive changes. */
  minor: number;
};

/** A button for a provider's setup. */
export type ProviderAction = {
  /** Action id, for `agents/runAction`. */
  id: string;
  /** Button label, such as `Install Claude ACP adapter (npm)`. */
  label: string;
  /** What pressing it does. */
  run: ProviderActionRun;
};

/** What a [`ProviderAction`] does. */
export type ProviderActionRun = {
  kind: "daemon";
} | {
  /** Its arguments. */
  args?: Array<string>;
  kind: "terminal";
  /** The program (absolute path). */
  program: string;
};

/** What a provider supports. */
export type ProviderCapabilities = {
  /** Prompts may contain embedded resources (file contents). */
  embeddedContext?: boolean;
  /** Prompts may contain images. */
  images?: boolean;
  /** Sessions have modes (`sessions/setMode`). */
  modes?: boolean;
  /** Sessions can be reattached after the daemon restarts (ACP `session/resume` or `session/load`). */
  resume?: boolean;
};

/** An agent provider, for the chat's provider picker. */
export type ProviderInfo = {
  /** Whether sessions can be created now: the providing mod is active and the provider is set up (`status.ready`, when it reports a status). */
  available: boolean;
  /** What it supports; `null` when not available. */
  capabilities?: ProviderCapabilities | null;
  /** Short description for the picker. */
  description?: string | null;
  /** Provider id, such as `claude-acp`. */
  id: string;
  /** How the agent runs. */
  kind: ProviderKind;
  /** Display name. */
  name: string;
  /** Whether it is set up, and what the user can do about it; `null` when the provider has nothing to report (always ready). */
  status?: ProviderStatus | null;
};

/** How an agent provider runs (mirrors the mod API's `AgentProviderKind`). */
export type ProviderKind = "acp" | "terminal" | "native";

/** Whether a provider can start sessions on this machine, such as Claude Code being installed and the ACP adapter set up. */
export type ProviderStatus = {
  /** Setup steps the user can take now, such as installing the agent's adapter. Shown when not ready. Never run without the user asking. */
  actions?: Array<ProviderAction>;
  /** What is missing or wrong, for the user; `null` when ready. */
  message?: string | null;
  /** Sessions can start now. */
  ready: boolean;
  /** How the user signs in to the agent's own tool, offered when a session fails with `AUTH_REQUIRED`. Coqo never runs a login itself. */
  signIn?: ProviderAction | null;
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

/** Identifies one replica of a buffer (one editor in one window), e.g. `r-3f2a9c1d0b7e4a65`. Assigned by [`Open`]; a replica keeps it across reconnects and reopens so the daemon can recognise its changes. */
export type ReplicaId = string;

/** Params of [`Resize`]. */
export type ResizeParams = {
  id: TerminalId;
  size: TermSize;
};

/** Params of `sessions/respondPermission`. */
export type RespondPermissionParams = {
  /** The answer. */
  outcome: PermissionOutcome;
  /** Also create the request's `suggestedRule` in the session's workspace, allowing or rejecting as the chosen option does. The chosen option still goes to the agent unchanged. */
  remember?: boolean;
  /** The request. */
  requestId: PermissionRequestId;
  /** The session. */
  session: SessionId;
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

/** Params of [`RevertChange`]. */
export type RevertParams = {
  /** The buffer. */
  buffer: BufferId;
  /** The revision whose change to undo. */
  rev: number;
};

/** Result of [`RevertChange`]. */
export type RevertResult = {
  /** The revision of the reverting change. */
  rev: number;
};

/** Identifies a root within its workspace. Stable for the root's lifetime and never reused within the workspace. */
export type RootId = number;

/** One row of the screen, as sparse spans in column order. */
export type Row = {
  spans?: Array<Span>;
};

/** One row of a [`Frame`]. */
export type RowUpdate = {
  /** Row index, 0 at the top of the viewport. */
  row: number;
  spans?: Array<Span>;
};

/** Whether a permission rule allows or rejects what it matches. */
export type RuleDecision = "allow" | "reject";

/** Params of `agents/deletePermissionRule`. */
export type RuleParams = {
  /** The rule. */
  id: PermissionRuleId;
};

/** Params of `agents/runAction`. */
export type RunActionParams = {
  /** A daemon action's id, from the provider's status. */
  action: string;
  /** Provider id. */
  provider: string;
};

/** Params of [`Save`]. */
export type SaveParams = {
  /** The buffer. */
  buffer: BufferId;
  /** Overwrite the file even if it changed on disk since it was last read. */
  force?: boolean;
};

/** Result of [`Save`]. */
export type SaveResult = {
  /** The revision of the save event; the file holds the text as of it. */
  savedRev: number;
};

/** The rows in view, the cursor and the modes: what a client draws. */
export type Screen = {
  /** `None` when hidden or scrolled out of view. */
  cursor?: Cursor | null;
  /** How many lines the viewport is scrolled up into the scrollback. */
  display_offset?: number;
  /** Lines of scrollback above the screen. */
  history_size?: number;
  modes?: Modes;
  /** Exactly `size.rows` rows, top to bottom. */
  rows: Array<Row>;
  size: TermSize;
};

/** Params of [`Scroll`]. */
export type ScrollParams = {
  id: TerminalId;
  scroll: ScrollTo;
};

/** How to move a terminal's viewport through its scrollback. */
export type ScrollTo = "pageUp" | "pageDown" | "top" | "bottom" | {
  lines: number;
};

/** Identifies an agent session. Generated by the daemon; stable forever. */
export type SessionId = string;

/** A session, as listed. */
export type SessionInfo = {
  /** Whether it was closed. */
  closed: boolean;
  /** Created, Unix milliseconds. */
  createdAt: number;
  /** The agent's working directory (the workspace root). */
  cwd: string;
  /** Id. */
  id: SessionId;
  /** Current mode id, if the provider has modes. */
  mode?: string | null;
  /** Provider id. */
  provider: string;
  /** What it is doing. */
  state: AgentState;
  /** Why, for `error`, `blocked` and `needs_input`. */
  stateReason?: string | null;
  /** Title for tabs and the sidebar. */
  title?: string | null;
  /** Last change to this info, Unix milliseconds. */
  updatedAt: number;
  /** The workspace it belongs to, if any. */
  workspace?: string | null;
};

/** Result of `sessions/list`. */
export type SessionList = {
  /** The `sessions/changes` seq this list reflects; subscribe with `since: seq` to keep it current. */
  seq: number;
  /** Most recently updated first. */
  sessions: Array<SessionInfo>;
};

/** A session mode, such as Claude Code's `default` ("Manual"), `acceptEdits` or `plan`. */
export type SessionMode = {
  /** Description. */
  description?: string | null;
  /** Mode id, passed to `sessions/setMode`. */
  id: string;
  /** Display name. */
  name: string;
};

/** The session's modes. Whether a permission prompt appears depends on the mode, so the UI always shows it. */
export type SessionModes = {
  /** Every mode. */
  availableModes: Array<SessionMode>;
  /** The current mode's id. */
  currentModeId: string;
};

/** Params naming one session. */
export type SessionParams = {
  /** The session. */
  session: SessionId;
};

/** One session's folded state: what a chat tab shows before replaying the transcript. */
export type SessionSnapshot = {
  /** The billing source. */
  auth?: AuthStatus | null;
  /** The latest slash commands. */
  availableCommands: Array<AvailableCommand>;
  /** Seq of its last update (0 if none). Subscribe to `sessions/updates` with `since: head` to continue from here. */
  head: number;
  /** The session. */
  info: SessionInfo;
  /** The session's modes. */
  modes?: SessionModes | null;
  /** Permission requests waiting for an answer. */
  pendingPermissions: Array<PermissionRequest>;
  /** The latest plan. */
  plan: Array<PlanEntry>;
  /** Whether a turn is running. */
  running: boolean;
  /** Number of turns started. */
  turns: number;
  /** The latest usage. */
  usage?: Usage | null;
};

/** A secret that authenticates a connection: the daemon's per-boot token, or a token scoped to one workspace and agent session.  It is a string on the wire. `Debug` prints `SessionToken(<redacted>)` and equality is constant-time, so the token neither leaks into logs nor through comparison timing. Use [`SessionToken::expose`] only to send or store it. */
export type SessionToken = string;

/** One entry of a session's transcript: an event of `sessions/updates`.  Updates from the agent mirror ACP `session/update`; the rest are written by the daemon (prompts, turns, permissions, state). */
export type SessionUpdate = {
  /** The prompt. */
  content: Array<ContentBlock>;
  update: "user_message";
} | {
  /** The chunk. */
  content: ContentBlock;
  update: "user_message_chunk";
} | {
  /** The chunk. */
  content: ContentBlock;
  update: "agent_message_chunk";
} | {
  /** The chunk. */
  content: ContentBlock;
  update: "agent_thought_chunk";
} | ToolCall | ToolCallUpdate | {
  /** Every entry. */
  entries: Array<PlanEntry>;
  update: "plan";
} | {
  /** Every command. */
  commands: Array<AvailableCommand>;
  update: "available_commands";
} | SessionModes | {
  /** The new mode's id. */
  modeId: string;
  update: "current_mode";
} | Usage | AuthStatus | PermissionRequest | {
  /** The workspace rule that answered it without asking. It follows its `permission_requested` at once; the state stays `working`. */
  byRule?: PermissionRule | null;
  /** The answer. */
  outcome: PermissionOutcome;
  /** The rule the user's answer created ("remember for this workspace"). */
  remembered?: PermissionRule | null;
  /** The request. */
  requestId: PermissionRequestId;
  update: "permission_resolved";
} | {
  /** Why, for `error`, `blocked` and `needs_input`. */
  reason?: string | null;
  /** The new state. */
  state: AgentState;
  update: "state";
} | {
  /** Turn number, from 1. */
  turn: number;
  update: "turn_started";
} | {
  /** The error, for `stop_reason: error`. */
  error?: string | null;
  /** Why. */
  stopReason: StopReason;
  /** Turn number. */
  turn: number;
  update: "turn_ended";
} | {
  update: "closed";
};

/** Params of `sessions/setMode`. */
export type SetModeParams = {
  /** The mode id, from the session's modes. */
  modeId: string;
  /** The session. */
  session: SessionId;
};

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

/** A run of cells with one style, starting at column `col`. Each character of `text` takes one cell, or two in a `wide` span (CJK, emoji). Blank default-styled cells are left out. */
export type Span = {
  col: number;
  style?: Style;
  text: string;
  /** Every character is two cells wide. */
  wide?: boolean;
};

/** Params of [`Spawn`]. */
export type SpawnParams = {
  /** Arguments to `program`. */
  args?: Array<string>;
  /** Working directory. Omit for the user's home directory. */
  cwd?: string | null;
  /** Extra environment variables, on top of the daemon's environment. */
  env?: Record<string, unknown>;
  /** The program to run. Omit for the user's login shell. */
  program?: string | null;
  /** Initial size. */
  size?: TermSize;
  /** Tab title until the program sets one. Defaults to the program name. */
  title?: string | null;
};

/** Why a turn ended (ACP `StopReason`, plus Coqo's own). */
export type StopReason = "end_turn" | "max_tokens" | "max_turn_requests" | "refusal" | "cancelled" | "error" | "interrupted";

/** How a run of cells is drawn. */
export type Style = {
  bg?: Color;
  fg?: Color;
  /** Bits from [`flags`]. */
  flags?: number;
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

/** A named definition: a function, method, type, module, heading, key, … */
export type Symbol = {
  /** Names of the symbols that contain this one, outermost first: a method `area` in `impl Circle` inside `mod shapes` has `["shapes", "Circle"]`. */
  container?: Array<string>;
  /** Last line of the definition (0-based, inclusive). */
  endLine: number;
  /** What it is. */
  kind: SymbolKind;
  /** The name, with runs of whitespace collapsed to one space. */
  name: string;
  /** The name. */
  nameRange: ByteRange;
  /** The whole definition. */
  range: ByteRange;
  /** First line of the definition (0-based). */
  startLine: number;
};

/** What a symbol is. */
export type SymbolKind = "module" | "class" | "interface" | "trait" | "struct" | "enum" | "enumMember" | "typeAlias" | "impl" | "function" | "method" | "constructor" | "field" | "constant" | "variable" | "macro" | "heading" | "key" | "table" | "element" | "rule";

/** Result of [`Symbols`]. */
export type SymbolsResult = {
  /** The language id (`rust`, `tsx`, …), or `null` for plain text. */
  language?: string | null;
  /** The revision the symbols describe. */
  rev: number;
  /** Every symbol, in document order (a container before what it holds). */
  symbols: Array<Symbol>;
};

/** Params of every `syntax/*` method. */
export type SyntaxParams = {
  /** An open buffer ([`buffer/open`](super::buffer::Open)). */
  buffer: BufferId;
};

/** A terminal's size in cells. */
export type TermSize = {
  cols: number;
  rows: number;
};

/** An event of [`Output`]. */
export type TerminalEvent = Frame | {
  title: string;
  type: "title";
} | {
  cwd: string;
  type: "cwd";
} | {
  code?: number | null;
  type: "commandFinished";
} | {
  type: "bell";
} | {
  code?: number | null;
  type: "exited";
};

/** Identifies a terminal for the lifetime of the daemon. */
export type TerminalId = number;

/** What the daemon knows about a terminal. */
export type TerminalInfo = {
  /** The working directory: as reported by the shell (OSC 7), else the process's own (where the OS can tell), else the spawn directory. */
  cwd?: string | null;
  id: TerminalId;
  /** Exit code of the last shell command, if the shell reports it (OSC 133;D). */
  last_command_exit?: number | null;
  /** Process id of the program (also its process group). */
  pid?: number | null;
  /** The program that was started. */
  program: string;
  size: TermSize;
  status: TerminalStatus;
  /** The title: set by the program (OSC 0/2), else the spawn title. */
  title: string;
};

/** Names one terminal. */
export type TerminalRef = {
  id: TerminalId;
};

/** The result of [`Snapshot`]. */
export type TerminalSnapshot = {
  info: TerminalInfo;
  screen: Screen;
  /** The [`Output`] seq this snapshot reflects: subscribe with `since: seq`. */
  seq: number;
};

/** Whether a terminal's process is still running. */
export type TerminalStatus = {
  state: "running";
} | {
  code?: number | null;
  state: "exited";
};

/** Replace the bytes `start..end` with `text`. */
export type TextEdit = {
  /** One past the last byte replaced; `start` for a pure insert. */
  end: number;
  /** First byte replaced (UTF-8 offset, on a character boundary). */
  start: number;
  /** The replacement. */
  text: string;
};

/** The file's text encoding. Only UTF-8 is supported for now; other files fail to open with [`UNSUPPORTED_ENCODING`](super::codes::UNSUPPORTED_ENCODING). */
export type TextEncoding = "utf8" | "utf8Bom";

/** A tool call, as first reported. */
export type ToolCall = {
  /** Output or proposed changes. */
  content?: Array<ToolCallContent>;
  /** Id, unique within the session. */
  id: ToolCallId;
  /** Tool kind. */
  kind?: ToolKind;
  /** Files it touches. */
  locations?: Array<ToolCallLocation>;
  /** The tool's raw input, provider-specific. */
  rawInput?: unknown;
  /** The tool's raw output, provider-specific. */
  rawOutput?: unknown;
  /** Status. */
  status?: ToolCallStatus;
  /** Human-readable title, such as `Write pong.txt`. */
  title: string;
};

/** What a tool call produced or proposes. */
export type ToolCallContent = {
  /** The content. */
  content: ContentBlock;
  type: "content";
} | {
  /** The text after. */
  newText: string;
  /** The text before; `null` for a new file. */
  oldText?: string | null;
  /** Absolute path of the file. */
  path: string;
  type: "diff";
} | {
  /** The terminal's id. */
  terminalId: string;
  type: "terminal";
};

/** Identifies a tool call within a session. */
export type ToolCallId = string;

/** A file location a tool call touches, for follow-along. */
export type ToolCallLocation = {
  /** 1-based line, if known. */
  line?: number | null;
  /** Absolute path. */
  path: string;
};

/** Where a tool call is in its lifecycle. */
export type ToolCallStatus = "pending" | "in_progress" | "completed" | "failed";

/** A change to a tool call. Fields left out are unchanged; `content` and `locations` replace the earlier lists when present. */
export type ToolCallUpdate = {
  /** New content (replaces). */
  content?: Array<ToolCallContent> | null;
  /** The call to update. */
  id: ToolCallId;
  /** New kind. */
  kind?: ToolKind | null;
  /** New locations (replace). */
  locations?: Array<ToolCallLocation> | null;
  /** Raw input. */
  rawInput?: unknown;
  /** Raw output. */
  rawOutput?: unknown;
  /** New status. */
  status?: ToolCallStatus | null;
  /** New title. */
  title?: string | null;
};

/** What kind of tool a call is, for icons and grouping (ACP `ToolKind`). */
export type ToolKind = "read" | "edit" | "delete" | "move" | "search" | "execute" | "think" | "fetch" | "switch_mode" | "other";

/** Params of `coqo/unsubscribe`. */
export type UnsubscribeParams = {
  /** The subscription to stop. */
  subscription: SubscriptionId;
};

/** Context and cost usage (ACP `usage_update`). */
export type Usage = {
  /** The model's context size in tokens. */
  contextSize?: number | null;
  /** Cost reported by the agent. On a subscription plan this is the SDK's notional figure, **not** a charge; show it only with [`AuthStatus::kind`] `api_key` / `external`. */
  cost?: Cost | null;
  /** Provider-specific extras (rate-limit windows, …). */
  meta?: unknown;
  /** Model in use. */
  model?: string | null;
  /** Tokens of context in use. */
  usedTokens?: number | null;
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

/** Params of `agents/permissionRules`. */
export type WorkspaceRulesParams = {
  /** The workspace id. */
  workspace: string;
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
  /** Every agent provider, with whether it is available. (client→server) */
  "agents/providers": { params: null; result: Array<ProviderInfo> };
  /** Run a provider's setup action (such as installing its adapter) at the user's request. (client→server) */
  "agents/runAction": { params: RunActionParams; result: ProviderInfo };
  /** Agent sessions, with the sessions/changes seq the list reflects. (client→server) */
  "sessions/list": { params: ListSessionsParams; result: SessionList };
  /** One session's folded state and the seq of its last update. (client→server) */
  "sessions/get": { params: SessionParams; result: SessionSnapshot };
  /** Start an agent session with a provider. (client→server) */
  "sessions/create": { params: CreateSessionParams; result: SessionInfo };
  /** Start a turn; it runs in the daemon and streams on sessions/updates. (client→server) */
  "sessions/prompt": { params: PromptParams; result: PromptResult };
  /** Cancel the running turn. Result is true if one was running. (client→server) */
  "sessions/cancel": { params: SessionParams; result: boolean };
  /** Answer a pending permission request. (client→server) */
  "sessions/respondPermission": { params: RespondPermissionParams; result: null };
  /** Switch a session's mode. (client→server) */
  "sessions/setMode": { params: SetModeParams; result: null };
  /** End a session; its transcript stays readable. Result is true if it was open. (client→server) */
  "sessions/close": { params: SessionParams; result: boolean };
  /** A workspace's permission rules (auto-approve and auto-reject), oldest first. (client→server) */
  "agents/permissionRules": { params: WorkspaceRulesParams; result: Array<PermissionRule> };
  /** Delete a permission rule. Result is true if it existed. (client→server) */
  "agents/deletePermissionRule": { params: RuleParams; result: boolean };
  /** Open (or join) the buffer of a file: text, rev and a replica id. (client→server) */
  "buffer/open": { params: OpenParams2; result: OpenResult };
  /** Undo one change (typically an agent's), transformed past later changes. (client→server) */
  "buffer/revertChange": { params: RevertParams; result: RevertResult };
  /** Write the buffer to its file atomically. (client→server) */
  "buffer/save": { params: SaveParams; result: SaveResult };
  /** A replica is done with a buffer; it unloads once unsubscribed. (client→server) */
  "buffer/close": { params: CloseParams; result: null };
  /** Every symbol of an open buffer (tree-sitter tags), flat, with container paths. (client→server) */
  "syntax/symbols": { params: SyntaxParams; result: SymbolsResult };
  /** The symbols of an open buffer as a tree. (client→server) */
  "syntax/outline": { params: SyntaxParams; result: OutlineResult };
  /** The foldable line ranges of an open buffer. (client→server) */
  "syntax/folding": { params: SyntaxParams; result: FoldingResult };
  /** Start a program (or the login shell) in a new daemon-owned PTY. (client→server) */
  "terminal/spawn": { params: SpawnParams; result: TerminalInfo };
  /** Every terminal, running or exited. (client→server) */
  "terminal/list": { params: null; result: Array<TerminalInfo> };
  /** A terminal's screen and the terminal/output seq it reflects. (client→server) */
  "terminal/snapshot": { params: TerminalRef; result: TerminalSnapshot };
  /** Write to a terminal's PTY. Send the next one after this one's response. (client→server) */
  "terminal/input": { params: InputParams; result: null };
  /** Resize a terminal, in cells. (client→server) */
  "terminal/resize": { params: ResizeParams; result: null };
  /** Move a terminal's viewport through its scrollback. (client→server) */
  "terminal/scroll": { params: ScrollParams; result: null };
  /** Kill a terminal's process group and forget it. True if it existed. (client→server) */
  "terminal/close": { params: TerminalRef; result: boolean };
}

/** Notifications: method name → params. */
export interface Notifications {
  /** One event of a subscription. (server→client) */
  "coqo/event": { params: EventParams };
  /** The server ended a subscription, with an error if abnormal. (server→client) */
  "coqo/subscriptionEnded": { params: SubscriptionEndedParams };
  /** A replica's change against base_rev; acknowledged by its own buffer/changes event. (client→server) */
  "buffer/edit": { params: EditParams };
  /** A buffer/edit the daemon could not apply; reopen the buffer. (server→client) */
  "buffer/editRejected": { params: EditRejectedParams };
}

/** Topics (`coqo/subscribe`): topic name → params and event. */
export interface Topics {
  /** Workspace changes, from the persistent event log (resumable with since). */
  "workspace/events": { params: EventsParams; event: WorkspaceEvent };
  /** Debounced file-system changes in an open workspace (live only). */
  "fs/changes": { params: ChangesParams; event: FsChanges };
  /** One session's persisted transcript (resumable with since; seqs per session). */
  "sessions/updates": { params: SessionParams; event: SessionUpdate };
  /** The latest info of each session whose state, title or mode changed (resumable). */
  "sessions/changes": { params: ChangesParams2; event: SessionInfo };
  /** Every committed change of a buffer; seq is the buffer rev (resumable with since). */
  "buffer/changes": { params: ChangesParams3; event: BufferChange };
  /** A terminal's changed rows (batched at ~60 Hz), title, cwd and exit. */
  "terminal/output": { params: TerminalRef; event: TerminalEvent };
}

/** Encode one frame: u32 big-endian length, then the JSON payload. */
export function encodeFrame(message: Message): Uint8Array {
  const body = new TextEncoder().encode(JSON.stringify(message));
  const frame = new Uint8Array(4 + body.length);
  new DataView(frame.buffer).setUint32(0, body.length, false);
  frame.set(body, 4);
  return frame;
}
