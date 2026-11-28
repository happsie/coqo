// SPDX-License-Identifier: Apache-2.0
// Generated from crates/protocol by `COQO_BLESS=1 cargo test -p coqo-protocol schema`.
// Do not edit. Types for the Coqo Protocol, for tooling and future clients.

export const PROTOCOL_VERSION = { major: 0, minor: 1 } as const;

/** JSON-RPC 2.0 error codes used by the protocol. */
export const ErrorCodes = {
  ALREADY_EXISTS: 1010,
  AUTH_REQUIRED: 1107,
  COMMAND_FAILED: 1902,
  FILE_TOO_LARGE: 1005,
  FORBIDDEN: -32007,
  GIT_FAILED: 1603,
  INCOMPATIBLE_PROTOCOL: -32001,
  INTERNAL_ERROR: -32603,
  INVALID_EDIT: 1204,
  INVALID_PARAMS: -32602,
  INVALID_QUERY: 2001,
  INVALID_REQUEST: -32600,
  INVALID_WALKTHROUGH: 1702,
  IO_ERROR: 1009,
  METHOD_NOT_FOUND: -32601,
  NOT_A_DIRECTORY: 1006,
  NOT_A_FILE: 1007,
  NOT_A_REPOSITORY: 1601,
  NOT_FOUND: 1004,
  NOT_INITIALIZED: -32002,
  NOT_YOUR_COMMENT: 1803,
  NO_SUCH_TERMINAL: 1301,
  PARSE_ERROR: -32700,
  PATH_OUTSIDE_WORKSPACE: 1003,
  PERMISSION_DENIED: 1011,
  PROVIDER_ERROR: 1108,
  REQUEST_CANCELLED: -32005,
  RESUME_GAP: -32003,
  REVISION_GONE: 1203,
  SAVE_CONFLICT: 1205,
  SEARCH_UNAVAILABLE: 2002,
  SESSION_BUSY: 1103,
  SESSION_CLOSED: 1104,
  SPAWN_FAILED: 1302,
  STALE_SELECTION: 1602,
  STORAGE_ERROR: 1109,
  TRASH_FAILED: 1012,
  UNAUTHORIZED: -32006,
  UNKNOWN_BUFFER: 1201,
  UNKNOWN_COMMAND: 1901,
  UNKNOWN_COMMENT: 1802,
  UNKNOWN_PERMISSION_REQUEST: 1105,
  UNKNOWN_PROVIDER: 1102,
  UNKNOWN_ROOT: 1002,
  UNKNOWN_SESSION: 1101,
  UNKNOWN_SUBSCRIPTION: -32004,
  UNKNOWN_THREAD: 1801,
  UNKNOWN_TOOL: 1401,
  UNKNOWN_WALKTHROUGH: 1701,
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

/** One acceptance criterion of a session's task. */
export type AcceptanceCriterion = {
  /** Who changed it last. */
  changedBy: CriterionAuthor;
  /** Why it has that status. */
  evidence?: Array<CriterionEvidence>;
  /** Stable id within the session (`AC1`, or the agent's own), which walkthroughs and trace scenarios refer to. */
  id: string;
  /** Whether it holds. */
  status: CriterionStatus;
  /** What must hold, one sentence. */
  text: string;
  /** When it last changed, Unix milliseconds. */
  updatedAt: number;
};

/** Params of [`AddRoot`]. */
export type AddRootParams = {
  /** The workspace. */
  id: WorkspaceId;
  /** Display name (default: the folder name). */
  name?: string | null;
  /** Folder to add. Adding a folder that is already a root is a no-op. */
  path: string;
};

/** Params of [`AgentReply`]. */
export type AgentReplyParams = {
  /** The reply, in markdown. */
  body: string;
  /** Also resolve the thread (its request is done). */
  resolve?: boolean;
  /** The agent session replying. */
  session: SessionId;
  /** The thread. */
  thread: ThreadId;
  /** The workspace: the thread's. */
  workspace: WorkspaceId;
};

/** What an agent session is doing. */
export type AgentState = "idle" | "working" | "needs_input" | "needs_permission" | "blocked" | "error" | "done";

/** What the agent says it is doing (`sessions/setStatus`, the `set_status` MCP tool). */
export type AgentStatus = {
  /** Progress, if the work is countable. */
  progress?: StatusProgress | null;
  /** The state the agent claimed with it, if any. */
  state?: StatusState | null;
  /** One short line, at most [`MAX_STATUS_CHARS`] characters. */
  text: string;
  /** The turn it was set in (`None` between turns). */
  turn?: number | null;
  /** When it was set, Unix milliseconds. */
  updatedAt: number;
};

/** One alert, as it is now. */
export type Alert = {
  /** More detail (the question, the error), plain text. */
  body?: string | null;
  /** When this occurrence was raised, Unix milliseconds. */
  createdAt: number;
  /** Whether the user dismissed it (hidden from the list). */
  dismissed: boolean;
  /** Id. */
  id: AlertId;
  /** The source's key for it: raising under the same source and key replaces it. A session's id for the agent sources. */
  key: string;
  /** How much it wants the user. */
  kind: AlertKind;
  /** Whether the user has seen it (a resolved alert counts as read). */
  read: boolean;
  /** When the condition cleared, if it has: Unix milliseconds. */
  resolvedAt?: number | null;
  /** The `alerts/changes` seq of this state. */
  seq: number;
  /** The agent session it is about, if any. */
  session?: SessionId | null;
  /** The source that raised it: a core source (`agents.permission`, `agents.question`, `agents.blocked`, `agents.error`, `agents.done`) or a mod's alert source id. */
  source: string;
  /** Where opening it goes. */
  target?: AlertTarget | null;
  /** One line: "Permission requested: Run cargo test". */
  title: string;
  /** When it last changed, Unix milliseconds. */
  updatedAt: number;
  /** The workspace it is about; absent for one about no workspace. */
  workspace?: WorkspaceId | null;
};

/** Identifies an alert. Assigned by the daemon, never reused; an alert raised again under its source and key keeps its id. */
export type AlertId = number;

/** How much an alert wants the user. */
export type AlertKind = "needs_you" | "error" | "done" | "info";

/** Result of [`List`]. */
export type AlertList = {
  /** The alerts not dismissed, most recently raised first, at most [`LIST_LIMIT`]. */
  alerts: Array<Alert>;
  /** The `alerts/changes` seq this list reflects; subscribe with `since: seq` to keep it current. */
  seq: number;
};

/** The user's alert settings. */
export type AlertSettings = {
  /** Do not disturb: alerts are still recorded, but nothing pops up (no toasts, no desktop notifications). */
  doNotDisturb?: boolean;
  /** Sources that raise nothing, by id. */
  muted?: Array<string>;
};

/** Result of [`GetSettings`]. */
export type AlertSettingsInfo = {
  /** The settings. */
  settings: AlertSettings;
  /** Every source: the core ones, then mods', by id. */
  sources: Array<AlertSourceInfo>;
};

/** A source of alerts, for the per-source settings. */
export type AlertSourceInfo = {
  /** What it alerts about. */
  description?: string | null;
  /** Id, as in [`Alert::source`]. */
  id: string;
  /** Whether it is muted. */
  muted: boolean;
  /** Display name. */
  name: string;
};

/** Where opening an alert takes the user, in the alert's workspace. */
export type AlertTarget = {
  kind: "session";
  /** The session. */
  session: SessionId;
} | {
  kind: "question";
  /** Its first code location. */
  location?: FileLocation | null;
  /** The question. */
  question: QuestionId;
  /** The asking session. */
  session: SessionId;
} | {
  kind: "file";
  /** Where. */
  location: FileLocation;
};

/** Result of [`MarkRead`], [`Dismiss`] and [`DismissAll`]. */
export type AlertsChanged = {
  /** The alerts that changed (each is also an `alerts/changes` event). */
  changed: Array<AlertId>;
};

/** Lines of a file a step is about, as the file is now (open buffers with their unsaved edits). */
export type AnchorDraft = {
  /** Last line, 1-based, inclusive. Default: `start_line`. */
  end_line?: number | null;
  /** The file, relative to the root or absolute inside the workspace. */
  path: string;
  /** Root id from `editor_context`. Default: the scope's root, or the root an absolute path is in. */
  root?: RootId | null;
  /** First line, 1-based. */
  start_line: number;
  /** Code that must be in those lines (a line of it is enough; whitespace is ignored), so a stale line number is caught instead of pinning the wrong code. */
  text?: string | null;
};

/** A spot in a text: 0-based line, byte column within the line and byte offset from the start (UTF-8, as in `buffer/*`). */
export type AnchorPoint = {
  /** Byte column within the line. */
  col: number;
  /** 0-based line. */
  line: number;
  /** Byte offset from the start of the text. */
  offset: number;
};

/** Where an anchor is now. */
export type AnchorResolution = {
  /** The open buffer `range` is in, if the file is open; else `range` is in the file on disk. */
  buffer?: BufferId | null;
  /** Its range in the current text; absent when orphaned. */
  range?: AnchorSpan | null;
  /** That buffer's revision. */
  rev?: number | null;
  /** How well it was found. */
  status: AnchorStatus;
};

/** A range of a text, `end` exclusive. */
export type AnchorSpan = {
  /** Where it ends (exclusive). */
  end: AnchorPoint;
  /** Where it starts. */
  start: AnchorPoint;
};

/** How well an anchor was found in the current text. */
export type AnchorStatus = {
  kind: "exact";
} | {
  kind: "moved";
} | {
  /** How sure the match is. */
  confidence: number;
  kind: "fuzzy";
} | {
  kind: "orphaned";
};

/** The innermost symbol an anchor was in (tree-sitter, `coqo-syntax`). */
export type AnchorSymbol = {
  /** Its kind, as `syntax/*` names kinds (`method`, `function`, …). */
  kind: string;
  /** How many lines below the symbol's first line the anchor started. */
  lineOffset?: number;
  /** Its containers and name, outermost first: `["shapes", "Circle", "area"]`. */
  path: Array<string>;
};

/** Params of [`Answer`]. */
export type AnswerParams = {
  /** The question. */
  id: QuestionId;
  /** The chosen option's id. Required for a confirmation and info; a decision takes an option, `text`, or both. */
  option?: string | null;
  /** A free-text reply (markdown). Required for a clarification. */
  text?: string | null;
};

/** Params of [`Ask`]. */
export type AskParams = {
  /** The code it is about: 1 to [`MAX_ANCHORS`] locations, as for [`Create`]. */
  anchors: Array<NewAnchor>;
  /** The question, markdown. */
  body: string;
  /** The agent session to ask; it must not be running a turn. */
  session: SessionId;
  /** The workspace. */
  workspace: WorkspaceId;
};

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

/** Who wrote a comment. */
export type Author = {
  kind: "user";
} | {
  kind: "agent";
  /** The session. */
  session: SessionId;
} | {
  kind: "github";
  /** Their login. */
  login: string;
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

/** Where a thread lives besides Coqo. */
export type Backend = "local" | "github";

/** Consecutive lines last changed by one commit. */
export type BlameHunk = {
  /** Lines in the hunk. */
  lines: number;
  /** The commit (a key of [`BlameResult::commits`]). */
  oid: string;
  /** The file's path in that commit, if it was renamed since. */
  origPath?: string | null;
  /** 1-based first line in the file as that commit left it. */
  origStart: number;
  /** 1-based first line in the file. */
  start: number;
};

/** Params of [`Blame`]. */
export type BlameParams = {
  /** The file, repository-relative. */
  path: string;
  /** The revision to blame (default `HEAD`). Uncommitted changes are not blamed. */
  rev?: string | null;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`Blame`]. */
export type BlameResult = {
  /** The commits the hunks name, by id. */
  commits: Record<string, unknown>;
  /** Hunks in line order, covering every line. */
  hunks: Array<BlameHunk>;
  /** Lines in the file at that commit. */
  lineCount: number;
  /** The commit blamed. */
  rev: string;
};

/** A branch, in [`BranchesResult`]. */
export type Branch = {
  /** `HEAD` is on it. */
  current: boolean;
  /** Short name: `main`, or `origin/main` for a remote branch. */
  name: string;
  /** The commit it points at. */
  oid: string;
  /** A remote-tracking branch. */
  remote: boolean;
  /** The commit's message's first line. */
  summary: string;
  /** The commit's time, in Unix seconds. */
  time: number;
  /** Its upstream (local branches). */
  upstream?: string | null;
};

/** Result of [`Branches`]. */
export type BranchesResult = {
  /** Local branches, then remote ones, each by name. */
  branches: Array<Branch>;
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

/** Params of [`Call`]. */
export type CallParams = {
  /** Its arguments. */
  arguments?: Record<string, unknown>;
  /** The tool, as [`Tools`] names it. */
  name: string;
  /** The calling agent session. */
  session: string;
  /** The calling session's workspace. */
  workspace: WorkspaceId;
};

/** Result of [`Call`]: what the agent gets back. */
export type CallResult = {
  /** Whether the tool failed (invalid arguments, or the mod's error). The model sees `text` and can correct itself. */
  isError?: boolean;
  /** The result as JSON, if it has one. */
  structured?: unknown;
  /** Text for the model: the result, or what went wrong. */
  text: string;
};

/** What happened to a walkthrough. */
export type ChangeKind = "published" | "updated" | "deleted";

/** A file that differs, in [`DiffFilesResult`]. */
export type ChangedFile = {
  /** Added lines (0 for binary files). */
  additions: number;
  /** Either side is binary. */
  binary: boolean;
  /** `added`, `modified`, `deleted` or `typeChanged`. */
  change: FileChange;
  /** Removed lines (0 for binary files). */
  deletions: number;
  /** Repository-relative. */
  path: string;
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

/** Params of the [`Changes`] topic. */
export type ChangesParams4 = {
  /** Only this workspace's questions. */
  workspace?: WorkspaceId | null;
};

/** Params of the [`Changes`] topic. */
export type ChangesParams5 = {
  /** The workspace to watch. It must be open. */
  workspace: WorkspaceId;
};

/** Params of the [`Changes`] topic. */
export type ChangesParams6 = {
  /** Only this workspace's alerts. */
  workspace?: WorkspaceId | null;
};

/** Params of the [`Changes`] topic. */
export type ChangesParams7 = {
  /** Only this workspace's walkthroughs. */
  workspace?: WorkspaceId | null;
};

/** Params of the [`Changes`] topic. */
export type ChangesParams8 = {
  /** Only this workspace's threads. */
  workspace?: WorkspaceId | null;
};

/** Params of [`Close`]. */
export type CloseParams = {
  /** The buffer. */
  buffer: BufferId;
  /** The replica going away. */
  replica: ReplicaId;
};

/** Code a question is pinned to, as it was when the agent asked: where, and a fingerprint to find it again (ADR 0013). */
export type CodeAnchor = {
  /** Up to 3 lines after. */
  contextAfter?: Array<string>;
  /** Up to 3 lines before. */
  contextBefore?: Array<string>;
  /** `/`-separated path relative to the root. */
  path: string;
  /** The start of the first line, up to the range. */
  prefix?: string;
  /** Where the anchored text was. */
  range: AnchorSpan;
  /** The buffer revision `range` was taken at; absent for a file read from disk. */
  rev?: number | null;
  /** The root `path` is relative to. */
  root: RootId;
  /** The rest of the last line after the range. */
  suffix?: string;
  /** The enclosing symbol, if the file's language is known. */
  symbol?: AnchorSymbol | null;
  /** The anchored text as it was: what to show when it is orphaned. */
  text: string;
  /** FNV-1a 64 hash of `text`, in hex. */
  textHash: string;
};

/** A cell colour. Indexed colours are resolved by the client's theme. */
export type Color = "default" | {
  indexed: number;
} | {
  rgb: [number, number, number];
};

/** A mod-contributed command. */
export type CommandInfo = {
  /** The palette group, such as `Git`. */
  category?: string | null;
  /** The command id, such as `hello.greet`. Unique across mods. */
  id: string;
  /** The keys the mod binds to it. */
  keybindings?: Array<CommandKeybinding>;
  /** The mod that contributes it. */
  mod: string;
  /** The title shown in the palette. */
  title: string;
  /** When the command is available, as the mod declared it. */
  when?: string | null;
};

/** A key sequence a mod binds to its command (point `keybindings`), in the mod manifest's syntax: chords separated by spaces, each chord modifiers and a key joined by `+` (`mod+shift+h`, `mod+k mod+s`). */
export type CommandKeybinding = {
  /** The keys on every platform, unless `mac` overrides them. */
  key: string;
  /** The keys on macOS. */
  mac?: string | null;
};

/** One comment of a thread. */
export type Comment = {
  /** Who wrote it. */
  author: Author;
  /** The comment, in markdown. */
  body: string;
  /** Written, Unix milliseconds. */
  createdAt: number;
  /** Last edited, Unix milliseconds. */
  editedAt?: number | null;
  /** Id. */
  id: CommentId;
  /** Part of the user's pending review: not sent yet. */
  pending?: boolean;
};

/** Identifies a comment, across threads. Assigned by the daemon, never reused. */
export type CommentId = number;

/** Params of [`DeleteComment`]. */
export type CommentParams = {
  /** The comment: one of the user's. */
  comment: CommentId;
};

/** A commit blame points at. */
export type CommitInfo = {
  /** The author's name. */
  author: string;
  /** The author's email. */
  email: string;
  /** Its id. */
  oid: string;
  /** The message's first line. */
  summary: string;
  /** The author time, in Unix seconds. */
  time: number;
};

/** Params of [`Commit`]. */
export type CommitParams = {
  /** Amend the last commit instead. */
  amend?: boolean;
  /** The message. May be empty only when amending (keeps the old one). */
  message: string;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`Commit`]. */
export type CommitResult = {
  /** The new commit. */
  oid: string;
  /** Its message's first line. */
  summary: string;
};

/** One entry of a select [`SessionConfigOption`]. */
export type ConfigOptionValue = {
  /** Description. */
  description?: string | null;
  /** The name of the group it is listed under, if the agent groups its entries. Entries of one group are adjacent. */
  group?: string | null;
  /** Display name. */
  name: string;
  /** Value id, passed to `sessions/setConfigOption`. */
  value: string;
};

/** A value for `sessions/setConfigOption`: a select entry's `value`, or a boolean for a toggle. */
export type ConfigValue = boolean | string;

/** An unmerged path. */
export type Conflict = "bothDeleted" | "addedByUs" | "deletedByThem" | "addedByThem" | "deletedByUs" | "bothAdded" | "bothModified";

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

/** Params of [`CreateBranch`]. */
export type CreateBranchParams = {
  /** The new branch. */
  name: string;
  /** The root. */
  root: RootId;
  /** Where it starts (default `HEAD`). */
  start?: string | null;
  /** Switch to it. */
  switch?: boolean;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of [`Create`]. */
export type CreateParams = {
  /** [`File`](EntryKind::File) (empty) or [`Dir`](EntryKind::Dir). */
  kind: EntryKind;
  /** `/`-separated path of the new entry, relative to the root. Missing folders on the way are created. */
  path: string;
  /** The root the path is relative to. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of [`Create`]. */
export type CreateParams2 = {
  /** The code it is about: 1 to [`MAX_ANCHORS`] locations, the files as they are now (open buffers with their unsaved edits). */
  anchors: Array<NewAnchor>;
  /** The question or note, in markdown. */
  body: string;
  /** What kind of answer it wants; `annotation` for a note. */
  kind: QuestionKind;
  /** A decision's options (at least 2). Confirmation and info get theirs from the daemon; the other kinds take none. */
  options?: Array<NewOption>;
  /** The asking agent session. */
  session: string;
  /** A note's severity (default `info`). */
  severity?: Severity | null;
  /** The asking session's workspace. */
  workspace: WorkspaceId;
};

/** Params of [`Create`]. */
export type CreateParams3 = {
  /** The publishing agent session. */
  session: string;
  /** The [`WalkthroughDraft`], as the agent wrote it. Checked by the daemon, which reports every problem at once, so it is any JSON here. */
  walkthrough: WalkthroughDraft;
  /** The publishing session's workspace. */
  workspace: WorkspaceId;
};

/** Params of [`Create`]. */
export type CreateParams4 = {
  /** The first comment, in markdown. */
  body: string;
  /** The diff source's identity, so the thread can be listed by diff. */
  diff?: string | null;
  /** The lines (1-based, at most [`MAX_THREAD_LINES`]); omit to comment on the whole file. */
  lines?: ThreadLines | null;
  /** `/`-separated path of the file, relative to the root. */
  path: string;
  /** Add it to the pending review (what the review UI does) rather than as a comment already made. */
  pending?: boolean;
  /** The root `path` is relative to. */
  root: RootId;
  /** The diff side the user commented on. */
  side?: ThreadSide;
  /** The version of the file that side shows (see [`ThreadAnchor::version`]). */
  version: DiffSide;
  /** The workspace. */
  workspace: WorkspaceId;
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

/** Who last changed a criterion. */
export type CriterionAuthor = "agent" | "user";

/** An acceptance criterion a walkthrough addresses. */
export type CriterionDraft = {
  /** Its id: one of the session's (`AC1`) or a new one. */
  id: string;
  /** What must hold. Default: the session criterion's text; required for a criterion that is not the session's. */
  text?: string | null;
};

/** Why a criterion has its status: a test, a command, a place in the code. */
export type CriterionEvidence = {
  /** Line in `path`, **1-based**. */
  line?: number | null;
  /** A file that shows it: absolute (a relative path is taken from the session's `cwd`). */
  path?: string | null;
  /** What shows it, in a few words ("test `volume_discount_caps` passes"). */
  text: string;
};

/** One entry of `sessions/setCriteria`: a new criterion, or a change to the one with `id`. */
export type CriterionInput = {
  /** Replaces its evidence (`[]` clears it). */
  evidence?: Array<CriterionEvidence> | null;
  /** The criterion to change; a new one gets this id (or `AC<n>`). */
  id?: string | null;
  /** Its status; a new criterion is `pending`. */
  status?: CriterionStatus | null;
  /** Its text; required for a new criterion. */
  text?: string | null;
};

/** Whether an acceptance criterion holds. */
export type CriterionStatus = "pending" | "met" | "failed" | "unknown";

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

/** Result of [`DeleteComment`]. */
export type DeleteCommentResult = {
  /** The thread without the comment; absent when it was the last one, and the thread went with it. */
  thread?: Thread | null;
};

/** Params of [`Delete`]. */
export type DeleteParams = {
  /** The entry to delete (a folder with everything in it). A symlink is deleted itself, not its target. */
  path: string;
  /** Delete it for good instead of moving it to the OS trash. Without it, an entry that cannot go to the trash is left alone and the call fails with [`TRASH_FAILED`](super::codes::TRASH_FAILED). */
  permanent?: boolean;
  /** The root the path is relative to. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of [`Delete`]. */
export type DeleteParams2 = {
  /** Also delete its committed file, if it has one. Default: the file stays (it may be committed already; git is the user's). */
  deleteFile?: boolean;
  /** The walkthrough. */
  id: WalkthroughId;
};

/** Result of [`Delete`]. */
export type DeleteResult = {
  /** Why the entry can't be restored (deleted for good, too large for a copy where the trash can't give it back), for the user. */
  no_restore?: string | null;
  /** What [`Restore`] takes to bring the entry back (undo), while this daemon runs. Absent when it can't be restored; `no_restore` says why. */
  restore?: string | null;
  /** Whether it went to the OS trash (`false`: deleted for good). */
  trashed: boolean;
};

/** Params of [`DiffFiles`]. */
export type DiffFilesParams = {
  /** The old side. */
  base: DiffSide;
  /** The new side. */
  head: DiffSide;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`DiffFiles`]. */
export type DiffFilesResult = {
  /** The files, sorted by path (`git diff --numstat`). Untracked files are not on the `worktree` side. */
  files: Array<ChangedFile>;
};

/** One line of a [`Hunk`]. */
export type DiffLine = {
  /** What it is. */
  kind: LineKind;
  /** 1-based line on the new side (context and added lines). */
  newLine?: number | null;
  /** `false` for a last line without a line break ("\ No newline at end of file"). */
  newline: boolean;
  /** 1-based line on the old side (context and removed lines). */
  oldLine?: number | null;
  /** The text, without its line break. */
  text: string;
};

/** Params of [`Diff`]. */
export type DiffParams = {
  /** The old side. Unstaged changes: `index` → `worktree`; staged ones: `{"rev": "HEAD"}` → `index`. */
  base: DiffSide;
  /** Lines of context around changes (default 3, at most 10 000). */
  context?: number | null;
  /** The new side. */
  head: DiffSide;
  /** The file, repository-relative. */
  path: string;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** One side of a diff. */
export type DiffSide = "worktree" | "index" | {
  rev: string;
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

/** Result of [`Discard`]. */
export type DiscardResult = {
  /** How many pending comments were deleted. */
  discarded: number;
};

/** Params of [`DismissAll`]. */
export type DismissAllParams = {
  /** Only this workspace's; every alert if absent. */
  workspace?: WorkspaceId | null;
};

/** Params of [`Dismiss`]. */
export type DismissParams = {
  /** The alerts. */
  ids: Array<AlertId>;
};

/** How a graph line meets a row. */
export type EdgeKind = "through" | "in" | "out";

/** Params of [`EditComment`]. */
export type EditCommentParams = {
  /** Its new text, in markdown. */
  body: string;
  /** The comment: one of the user's. */
  comment: CommentId;
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

/** Result of [`GetContext`]. */
export type EditorContext = {
  /** The windows that show it: the focused one first, then the most recently updated. Empty when no UI shows the workspace. */
  windows: Array<WindowState>;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** A file tab open in a window. */
export type EditorTab = {
  /** Whether this is the tab the user works in: the active tab of the focused pane. At most one per window. */
  active?: boolean;
  /** Whether it has unsaved edits. */
  dirty?: boolean;
  /** `/`-separated path relative to the root. */
  path: string;
  /** The root the path is relative to. */
  root: RootId;
  /** Its selections and cursors, the primary one first. Empty if the tab shows no editor (not loaded yet, or a binary file). */
  selections?: Array<Selection>;
  /** The lines on screen, if the tab is visible. */
  visible?: LineRange | null;
};

/** How a frame's payload is encoded.  JSON is mandatory and is always used for the handshake. MessagePack is optional and is switched on for the rest of the connection when both sides list it in the handshake; it is intended for hot paths (buffer ops, trace streams). */
export type Encoding = "json" | "msgpack";

/** What a path points at. */
export type EntryKind = "file" | "dir" | "other";

/** Result of [`Create`] and [`Rename`]: the entry's path, normalised (`./a//b` is `a/b`). */
export type EntryResult = {
  /** `/`-separated path relative to the root. */
  path: string;
};

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

/** How a path changed on one side. */
export type FileChange = "added" | "modified" | "deleted" | "typeChanged" | "untracked";

/** Result of [`Diff`]. */
export type FileDiff = {
  /** Added lines. */
  additions: number;
  /** Either side is binary (then there are no hunks). */
  binary: boolean;
  /** Removed lines. */
  deletions: number;
  /** The changes; empty when the contents are equal. */
  hunks: Array<Hunk>;
  /** The new side's file; absent for a deleted file. */
  new?: FileSide | null;
  /** The old side's file; absent for an added file. */
  old?: FileSide | null;
  /** The file, repository-relative. */
  path: string;
};

/** A place in a file of the alert's workspace. */
export type FileLocation = {
  /** The 0-based line, if a line is meant. */
  line?: number | null;
  /** `/`-separated path relative to the root. */
  path: string;
  /** The root `path` is relative to. */
  root: RootId;
};

/** The matches of one file. */
export type FileMatches = {
  /** In order. */
  matches: Array<SearchMatch>;
  /** `/`-separated path relative to the root. */
  path: string;
  /** The file's root. */
  root: RootId;
};

/** One file of a [`ReplaceResult`]. */
export type FileReplaced = {
  /** Its open buffer, if it has one. */
  buffer?: BufferId | null;
  /** For `held` only: the edits, in order, against the buffer at `rev`. */
  edits?: Array<TextEdit>;
  /** For `failed`: why. */
  error?: string | null;
  /** What happened. */
  outcome: ReplaceOutcome;
  /** Its path, as given. */
  path: string;
  /** Matches replaced (or to replace, when held). */
  replaced: number;
  /** The buffer's revision after the change (`buffer`), or the one the edits apply to (`held`). */
  rev?: number | null;
  /** The file's root. */
  root: RootId;
};

/** A file as one diff side has it. */
export type FileSide = {
  /** Git's mode: `33188` (`0o100644`), `33261` (`0o100755`), `40960` (`0o120000`, a symlink) or `57344` (`0o160000`, a submodule). */
  mode: number;
  /** The blob id; absent for a work-tree file. */
  oid?: string | null;
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

/** One file [`Find`] matched. */
export type FindMatch = {
  /** `/`-separated path relative to the root. */
  path: string;
  /** Byte offsets in `path` of the characters the query matched. */
  positions: Array<number>;
  /** The root the path is relative to. */
  root: RootId;
  /** How well it matched: higher is better. Only comparable within one result. */
  score: number;
};

/** Params of [`Find`]. */
export type FindParams = {
  /** How many matches to return, best first. Default [`DEFAULT_FIND_LIMIT`], at most [`MAX_FIND_LIMIT`]. */
  limit?: number | null;
  /** What to look for: a fuzzy pattern over the root-relative path, such as `srcmain` or `tree.rs`. Whitespace is ignored. Smart case: an upper-case letter makes the match case-sensitive. Empty lists the files nearest the roots first. */
  query?: string;
  /** The workspace. It must be open. */
  workspace: WorkspaceId;
};

/** Result of [`Find`]. */
export type FindResult = {
  /** How many files the index holds. */
  files: number;
  /** The index is still being built: ask again for complete results. */
  indexing?: boolean;
  /** The best matches, best first (ties: shorter paths, then by path). */
  matches: Array<FindMatch>;
  /** How many indexed files matched in all. */
  total: number;
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

/** Params of [`GetContext`]. */
export type GetContextParams = {
  /** The workspace. */
  workspace: WorkspaceId;
};

/** What may have changed, in a [`GitChanges`] event. */
export type GitChangeKind = "worktree" | "index" | "head" | "refs" | "stash";

/** One event of [`Changes`]: a root's repository may have changed. */
export type GitChanges = {
  /** What may have changed, sorted. Status depends on `worktree`, `index` and `head`; branches on `head` and `refs`; the log on `head` and `refs`; the stash list on `stash`. */
  kinds: Array<GitChangeKind>;
  /** The root whose repository changed. */
  root: RootId;
};

/** A graph line within one row. */
export type GraphEdge = {
  /** The lane it starts in. */
  from: number;
  /** How it meets the row. */
  kind: EdgeKind;
  /** The lane it ends in. */
  to: number;
};

/** The graph at one commit's row. Rows are the same whatever the page. */
export type GraphRow = {
  /** The commit's lane (0 is leftmost). */
  column: number;
  /** The lines crossing the row. */
  edges: Array<GraphEdge>;
};

/** Where `HEAD` is. */
export type Head = {
  /** Commits the upstream lacks. */
  ahead: number;
  /** Commits on the upstream the branch lacks. */
  behind: number;
  /** The branch's short name; absent when `HEAD` is detached. */
  branch?: string | null;
  /** The commit; absent on a branch with no commits yet. */
  oid?: string | null;
  /** The branch's upstream, such as `origin/main`. */
  upstream?: string | null;
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

/** A run of changes with context, as `git diff` groups them. */
export type Hunk = {
  /** The lines. */
  lines: Array<DiffLine>;
  /** New lines in the hunk. */
  newLines: number;
  /** First new line (the one before, when `new_lines` is 0). */
  newStart: number;
  /** Old lines in the hunk. */
  oldLines: number;
  /** First old line (the one before, when `old_lines` is 0). */
  oldStart: number;
  /** The heading after `@@` (the enclosing function, roughly). */
  section?: string | null;
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

/** What a diff line is. */
export type LineKind = "context" | "added" | "removed";

/** Lines shown in a tab's viewport: 0-based, `end` inclusive. */
export type LineRange = {
  /** Last visible line. */
  end: number;
  /** First visible line. */
  start: number;
};

/** Lines of one file's diff, by their numbers in it. Choosing a whole hunk is choosing all its removed and added lines. */
export type LineSelection = {
  /** New-side numbers of added lines. */
  added?: Array<number>;
  /** Old-side numbers of removed lines. */
  removed?: Array<number>;
};

/** Result of [`ListDir`]. */
export type ListDirResult = {
  /** Entries: directories first, then files, each sorted by name. */
  entries: Array<DirEntry>;
};

/** Params of [`List`]. */
export type ListParams = {
  /** Answered and dismissed ones too (most recent first, up to 200). */
  includeClosed?: boolean;
  /** …and this file (`/`-separated, relative to `root`; in any root if `root` is omitted). */
  path?: string | null;
  /** Only questions with an anchor in this root… */
  root?: RootId | null;
  /** Only this session's. */
  session?: SessionId | null;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of [`List`]. */
export type ListParams2 = {
  /** Only this workspace's alerts. */
  workspace?: WorkspaceId | null;
};

/** Params of [`List`]. */
export type ListParams3 = {
  /** Only this session's. */
  session?: SessionId | null;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of [`List`]. */
export type ListParams4 = {
  /** Only threads started in this diff. */
  diff?: string | null;
  /** …and this file (`/`-separated, relative to `root`; in any root if `root` is omitted). */
  path?: string | null;
  /** Only threads in this root… */
  root?: RootId | null;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`List`]: a snapshot of every known workspace. */
export type ListResult = {
  /** The event-log `seq` this snapshot reflects. Subscribe to [`Events`] with `since` set to it to get every later change. */
  seq: number;
  /** Every known workspace, most recently opened first. */
  workspaces: Array<WorkspaceInfo>;
};

/** Result of [`List`]. */
export type ListResult2 = {
  /** The commands, by id. */
  commands: Array<CommandInfo>;
};

/** Params of `sessions/list`. */
export type ListSessionsParams = {
  /** Include closed sessions. */
  includeClosed?: boolean;
  /** Only this workspace's sessions. */
  workspace?: string | null;
};

/** One commit of [`LogResult`]. */
export type LogCommit = {
  /** The author's name. */
  author: string;
  /** The author's email. */
  email: string;
  /** Its row of the graph. */
  graph: GraphRow;
  /** Its id. */
  oid: string;
  /** Its parents, first parent first. */
  parents: Array<string>;
  /** Branches and tags at it (short names), and `HEAD`. */
  refs: Array<string>;
  /** The message's first line. */
  summary: string;
  /** The author time, in Unix seconds. */
  time: number;
};

/** Params of [`Log`]. */
export type LogParams = {
  /** Commits to return (default 100, at most 1000). */
  limit?: number | null;
  /** Start from these revisions (`git log a b`); empty for `HEAD`. */
  revs?: Array<string>;
  /** The root. */
  root: RootId;
  /** Commits to skip (paging). */
  skip?: number;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`Log`]. */
export type LogResult = {
  /** Newest first (by committer time, as `git log`). */
  commits: Array<LogCommit>;
  /** More commits follow: ask again with `skip` past these. */
  more: boolean;
};

/** Params of [`MarkRead`]. */
export type MarkReadParams = {
  /** These alerts. */
  ids?: Array<AlertId>;
  /** And every unread alert of this agent session (its chat is on screen). */
  session?: SessionId | null;
};

/** Where a match is: its line and character columns, as [`SearchMatch`] gives them. */
export type MatchAt = {
  /** 0-based end character (exclusive). */
  end: number;
  /** 0-based line. */
  line: number;
  /** 0-based start character. */
  start: number;
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

/** A code location to pin a question to: whole lines of a file. */
export type NewAnchor = {
  /** The lines, 0-based, `end` inclusive. */
  lines: LineRange;
  /** `/`-separated path of a file relative to the root. */
  path: string;
  /** The root `path` is relative to. */
  root: RootId;
};

/** An option as the agent offers it; the daemon numbers them. */
export type NewOption = {
  /** More detail, if any. */
  description?: string | null;
  /** The button text. */
  label: string;
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
export type OriginKind = "client" | "agent" | "disk" | "format" | "revert" | "save" | "replace";

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

/** A comment of the pending review, with what it is about. */
export type PendingComment = {
  /** The code the thread is about, resolved now. */
  anchor: ThreadAnchor;
  /** The comment. */
  comment: Comment;
  /** The diff the thread was started in. */
  diff?: string | null;
  /** Its thread. */
  thread: ThreadId;
};

/** Result of [`Pending`]. */
export type PendingReview = {
  /** They, by thread (oldest first), then oldest first. */
  comments: Array<PendingComment>;
  /** How many comments are pending. */
  count: number;
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

/** A place in a text: 0-based line and character (Unicode scalar values). */
export type Position = {
  /** 0-based character within the line. */
  character: number;
  /** 0-based line. */
  line: number;
};

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

/** Params of [`PublishContext`]. */
export type PublishContextParams = {
  /** Its context; `null` when the window closes or stops showing the workspace. */
  context?: WindowContext | null;
  /** The window, unique within the publishing connection. */
  window: string;
  /** The workspace the window shows. */
  workspace: WorkspaceId;
};

/** A question or note, as the daemon keeps it. */
export type Question = {
  /** The code it is about, at least one. */
  anchors: Array<QuestionAnchor>;
  /** The answer, once answered. */
  answer?: QuestionAnswer | null;
  /** Who asked: an agent, or the user ([`Ask`]). */
  author?: QuestionAuthor;
  /** The question or note, in markdown. */
  body: string;
  /** Answered or dismissed, Unix milliseconds. */
  closedAt?: number | null;
  /** Asked, Unix milliseconds. */
  createdAt: number;
  /** Id. */
  id: QuestionId;
  /** What kind of answer it wants (a thread the user started is a `clarification`). */
  kind: QuestionKind;
  /** The answers it offers, in order (empty for a clarification or a note). */
  options?: Array<QuestionOption>;
  /** The `questions/changes` seq of this state. */
  seq: number;
  /** The agent session that asked (or, for a thread the user started, the session asked). */
  session: SessionId;
  /** How important a note is (annotations only). */
  severity?: Severity | null;
  /** Open, answered or dismissed. */
  status: QuestionStatus;
  /** A thread the user started: their messages, oldest first, each with the turn that answers it. Empty for an agent's question. */
  thread?: Array<ThreadMessage>;
  /** Whether the agent is blocked on it right now (a waiting `ask_question` or `await_answer`). */
  waiting?: boolean;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** One code location of a question. */
export type QuestionAnchor = {
  /** The code as it was asked about. */
  anchor: CodeAnchor;
  /** Where it is now. */
  resolution: AnchorResolution;
};

/** The user's answer. */
export type QuestionAnswer = {
  /** The chosen option's id. */
  option?: string | null;
  /** A free-text reply or comment (markdown). */
  text?: string | null;
};

/** Who started a question. */
export type QuestionAuthor = "agent" | "user";

/** Identifies a question or note. Assigned by the daemon, never reused. */
export type QuestionId = number;

/** What kind of answer a question wants (PLAN §7.5), or a note. */
export type QuestionKind = "decision" | "clarification" | "confirmation" | "info" | "annotation";

/** Result of [`List`]. */
export type QuestionList = {
  /** Open questions oldest first, then (with `includeClosed`) closed ones, most recently closed first. */
  questions: Array<Question>;
  /** The `questions/changes` seq this list reflects; subscribe with `since: seq` to keep it current. */
  seq: number;
};

/** One answer a question offers. */
export type QuestionOption = {
  /** More detail, if any. */
  description?: string | null;
  /** What [`AnswerParams::option`] names: `1`, `2`, … for a decision, `yes` / `no` for a confirmation, `ok` for info. */
  id: string;
  /** The button text. */
  label: string;
};

/** Params of [`Get`] and [`Dismiss`]. */
export type QuestionParams = {
  /** The question. */
  id: QuestionId;
};

/** Where a question is in its life. */
export type QuestionStatus = "open" | "answered" | "dismissed";

/** A range of a text. `start` is not after `end`; equal is an empty range. */
export type Range = {
  /** Where it ends (exclusive). */
  end: Position;
  /** Where it starts. */
  start: Position;
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

/** Params of [`Rename`]. */
export type RenameParams2 = {
  /** The entry to rename or move. A symlink is moved itself, not its target. */
  from: string;
  /** The root both paths are relative to. */
  root: RootId;
  /** Its new path. The folder it goes in must exist, and nothing may be there yet ([`ALREADY_EXISTS`](super::codes::ALREADY_EXISTS)). */
  to: string;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of `sessions/rename`. */
export type RenameParams3 = {
  /** The session. */
  session: SessionId;
  /** The new title: whitespace collapsed, cut at 120 characters; must not be empty. */
  title: string;
};

/** A file to replace in. */
export type ReplaceFile = {
  /** Only the matches at these places; omitted: every match in the file. A place where the current text no longer has a match (it changed since the search) is skipped. */
  matches?: Array<MatchAt> | null;
  /** `/`-separated path relative to the root. */
  path: string;
  /** The file's root. */
  root: RootId;
};

/** What happened to one file of a [`Replace`]. */
export type ReplaceOutcome = "buffer" | "written" | "held" | "unchanged" | "failed";

/** Params of [`Replace`]. */
export type ReplaceParams = {
  /** The files. */
  files: Array<ReplaceFile>;
  /** Open buffers the client edits itself (its editor tabs, so the replacement joins their undo history): their edits come back as [`ReplaceOutcome::Held`] instead of being applied. */
  hold?: Array<BufferId>;
  /** The search whose matches to replace. */
  query: SearchQuery;
  /** What each match becomes. For a regular expression it may name capture groups (`$1`, `${name}`; `$$` is a `$`); otherwise it is literal. */
  replacement: string;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`Replace`]. */
export type ReplaceResult = {
  /** One entry per file asked for, in order. */
  files: Array<FileReplaced>;
  /** Matches replaced in all (held ones included). */
  replaced: number;
};

/** Identifies one replica of a buffer (one editor in one window), e.g. `r-3f2a9c1d0b7e4a65`. Assigned by [`Open`]; a replica keeps it across reconnects and reopens so the daemon can recognise its changes. */
export type ReplicaId = string;

/** Params of [`Reply`]. */
export type ReplyParams = {
  /** The follow-up, markdown. */
  body: string;
  /** The thread. */
  id: QuestionId;
};

/** Params of [`Reply`]. */
export type ReplyParams2 = {
  /** The comment, in markdown. */
  body: string;
  /** Add it to the pending review. */
  pending?: boolean;
  /** The thread. */
  thread: ThreadId;
};

/** Where a root's repository is. */
export type RepoInfo = {
  /** The root's path within the work tree (`""` when the root is the top). A git path `p` is the root-relative path `p` minus `prefix + "/"`. */
  prefix: string;
  /** The work tree's top folder, absolute. */
  workdir: string;
};

/** Params naming a workspace root, whose repository the method acts on. */
export type RepoParams = {
  /** The root; its repository is the one whose work tree contains it. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

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

/** Params of [`Restore`]. */
export type RestoreParams = {
  /** Where the entry was: the path [`Delete`] was given. */
  path: string;
  /** [`DeleteResult::restore`]. */
  restore: string;
  /** The root the path is relative to. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
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

/** One event of the [`Reveals`] topic: open `path` and select `range`. */
export type RevealEvent = {
  /** `/`-separated path relative to the root. */
  path: string;
  /** What to show and select; the top of the file if omitted. */
  range?: Range | null;
  /** The root the path is relative to. */
  root: RootId;
  /** The agent session that asked, if an agent did. */
  session?: string | null;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of [`Reveal`]. */
export type RevealParams = {
  /** `/`-separated path of a file relative to the root. */
  path: string;
  /** What to show and select; the top of the file if omitted. */
  range?: Range | null;
  /** The root the path is relative to. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`Reveal`]. */
export type RevealResult = {
  /** Whether a UI window shows the workspace, so someone will see it. */
  delivered: boolean;
};

/** Params of the [`Reveals`] topic. */
export type RevealsParams = {
  /** Only reveals in this workspace; omit for every workspace. */
  workspace?: WorkspaceId | null;
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

/** Params of [`Pending`] and [`Discard`]. */
export type ReviewParams = {
  /** Only comments on threads started in this diff. */
  diff?: string | null;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Something that could go wrong with a step's code. */
export type Risk = {
  /** How bad. */
  severity: RiskSeverity;
  /** What could go wrong, and what would show it. */
  text: string;
};

/** How bad it would be if a risk came true. */
export type RiskSeverity = "low" | "med" | "high";

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

/** Params of [`Run`]. */
export type RunParams = {
  /** Its arguments (`null` when none). */
  args?: unknown;
  /** The command id. */
  id: string;
};

/** Params of [`Run`]. */
export type RunParams2 = {
  /** Stop after this many matches (default [`DEFAULT_MAX_MATCHES`], at most [`MAX_MATCHES`]); [`SearchSummary::limit_hit`] then says there are more. */
  max_matches?: number | null;
  /** What to search for. */
  query: SearchQuery;
  /** Only these roots; omitted: every root. */
  roots?: Array<RootId> | null;
  /** The workspace to search. */
  workspace: WorkspaceId;
};

/** Result of [`Run`]: what the handler returned. */
export type RunResult = {
  /** The handler's result (`null` when it has none). */
  value?: unknown;
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

/** What a walkthrough covers. */
export type ScopeDraft = {
  /** The git revision the change is against (a commit, branch or tag). Default for `worktree`: `HEAD`. Required for `commits` and `pr`. */
  base?: string | null;
  /** The last revision of the change. Required for `commits` and `pr`; not allowed for `worktree`. */
  head?: string | null;
  /** `worktree` (uncommitted changes; the usual case right after a change), `commits` or `pr`. */
  kind: ScopeKind;
  /** The workspace root (repository) of the change, from `editor_context`. Default: the first root. */
  root?: RootId | null;
};

/** What a walkthrough covers. */
export type ScopeKind = "worktree" | "commits" | "pr";

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

/** An event of [`Run`]. */
export type SearchEvent = {
  /** Each with its matches. */
  files: Array<FileMatches>;
  kind: "files";
} | SearchSummary;

/** One match. */
export type SearchMatch = {
  /** 0-based character where it ends (exclusive), on the same line. */
  end: number;
  /** 0-based line. */
  line: number;
  /** The line without its line ending and leading whitespace; a window around the match, with `…` where cut, if the line is long. */
  preview: string;
  /** Where it ends in `preview` (bytes, exclusive). */
  preview_end: number;
  /** Where the match starts in `preview` (bytes). */
  preview_start: number;
  /** 0-based character where it starts on the line. */
  start: number;
};

/** What to search for. */
export type SearchQuery = {
  /** Match case; otherwise case is ignored. */
  case_sensitive?: boolean;
  /** Never files or folders matching these globs (`target`, `*.min.js`). */
  exclude?: Array<string>;
  /** Only files matching one of these globs (gitignore syntax, relative to the root: `*.rs`, `src/**`; a plain name such as `src` also covers what is under it). Empty: every file. */
  include?: Array<string>;
  /** The text, or a regular expression if `regex`. Must not be empty. */
  pattern: string;
  /** `pattern` is a regular expression (Rust `regex` syntax, as ripgrep); otherwise it is matched literally. */
  regex?: boolean;
  /** Only matches with a word boundary at both ends. */
  whole_word?: boolean;
};

/** How a search went: the last event of [`Run`]. */
export type SearchSummary = {
  /** How long it took, in milliseconds. */
  elapsed_ms: number;
  /** Files with matches. */
  files: number;
  /** It stopped at `max_matches`: there are more. */
  limit_hit: boolean;
  /** Matches. */
  matches: number;
  /** Files searched. */
  searched: number;
};

/** A selection or cursor in a tab. */
export type Selection = {
  /** The selected range; empty for a cursor. */
  range: Range;
  /** The selected text, for a non-empty selection, at most [`MAX_SELECTION_TEXT`] bytes. */
  text?: string | null;
  /** Whether `text` was cut short. */
  truncated?: boolean;
};

/** Params of [`SendReview`]. */
export type SendReviewParams = {
  /** Only comments on threads started in this diff. */
  diff?: string | null;
  /** The agent session to send them to: one of the workspace's, not in a turn. */
  session: SessionId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`SendReview`]. */
export type SentReview = {
  /** How many comments were sent (no longer pending). */
  count: number;
  /** The session it went to. */
  session: SessionId;
  /** Their threads, oldest first. */
  threads: Array<ThreadId>;
  /** The turn it started. */
  turn: number;
};

/** A setting the agent offers for the session (ACP session config options): the model, the reasoning effort, a fast-mode toggle, the mode, an agent persona, …. The agent decides which there are and their order; they can change at any time (a new model may offer other efforts), and every [`SessionUpdate::ConfigOptions`] replaces the whole list. */
export type SessionConfigOption = {
  /** What the option is about, for placement and icons only: ACP's [`config_category`] names (`mode`, `model`, `model_config`, `thought_level`, `speed`) or any other string. Never needed to use the option. */
  category?: string | null;
  /** Description. */
  description?: string | null;
  /** Option id, passed to `sessions/setConfigOption`. */
  id: string;
  /** Display name, such as `Model` or `Effort`. */
  name: string;
} & ({
  /** The chosen entry's `value`. */
  currentValue: string;
  /** The entries, in the agent's order. */
  options: Array<ConfigOptionValue>;
  type: "select";
} | {
  /** Whether it is on. */
  currentValue: boolean;
  type: "boolean";
});

/** Identifies an agent session. Generated by the daemon; stable forever. */
export type SessionId = string;

/** A session, as listed. */
export type SessionInfo = {
  /** Whether it was closed. */
  closed: boolean;
  /** Created, Unix milliseconds. */
  createdAt: number;
  /** The task's acceptance criteria, in order (`sessions/setCriteria`). */
  criteria?: Array<AcceptanceCriterion>;
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
  /** When `state` last changed, Unix milliseconds (how long it has waited, for alerts and the agents overview). */
  stateSince?: number | null;
  /** What the agent says it is doing, if it said so this turn (or its `blocked` or `done` outcome). */
  status?: AgentStatus | null;
  /** Title for tabs and the sidebar: the user's (`sessions/create`, `sessions/rename`), else the agent's own, else one derived from the first prompt. `None` until the first prompt. */
  title?: string | null;
  /** The tools the agent called this session and how often, in order of first use, counted from its tool calls (live ones, not replays). */
  toolUse?: Array<ToolUse>;
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
  /** The session's config options (model, effort, …), in the agent's order. Empty if the provider has none. */
  configOptions?: Array<SessionConfigOption>;
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
} | {
  /** Every option, in the agent's order. */
  configOptions: Array<SessionConfigOption>;
  update: "config_options";
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
} | Question | WalkthroughSummary | {
  update: "closed";
};

/** Params of `sessions/setConfigOption`. */
export type SetConfigOptionParams = {
  /** The option's id, from the session's config options. */
  configId: string;
  /** The session. */
  session: SessionId;
  /** The new value: a select entry's `value`, or a boolean for a toggle. */
  value: ConfigValue;
};

/** Params of `sessions/setCriteria`. */
export type SetCriteriaParams = {
  /** New criteria and changes, in order. */
  criteria: Array<CriterionInput>;
  /** The list becomes exactly `criteria`, in that order: criteria not named are dropped, named ones keep what their entry does not change. Otherwise an entry with a known id changes that criterion in place and the others are added at the end. */
  replace?: boolean;
  /** The session. */
  session: SessionId;
};

/** Params of `sessions/setMode`. */
export type SetModeParams = {
  /** The mode id, from the session's modes. */
  modeId: string;
  /** The session. */
  session: SessionId;
};

/** Params of [`SetSettings`]: what to change. */
export type SetSettingsParams = {
  /** Turn do-not-disturb on or off. */
  doNotDisturb?: boolean | null;
  /** Mute these sources. */
  mute?: Array<string>;
  /** Unmute these sources. */
  unmute?: Array<string>;
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

/** Params of `sessions/setStatus`. */
export type SetStatusParams = {
  /** Progress (`total` at least 1; `current` is capped at `total`). */
  progress?: StatusProgress | null;
  /** The session. */
  session: SessionId;
  /** The state the agent claims: `working` clears an earlier `blocked`, `blocked` asks for the user (the status says why), `done` marks the task finished (the session is `done` once the turn ends). */
  state?: StatusState | null;
  /** One short line (whitespace is collapsed; cut at [`MAX_STATUS_CHARS`]). Must not be empty. */
  status: string;
};

/** Params of [`SetStepState`]. */
export type SetStepStateParams = {
  /** The walkthrough. */
  id: WalkthroughId;
  /** Where the reader is with it. */
  state: StepState;
  /** The step's id. */
  step: string;
};

/** How important a note is. */
export type Severity = "info" | "warning";

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

/** Params of [`Stage`] and [`Unstage`]. */
export type StageParams = {
  /** Only these lines of the file's diff: for [`Stage`] the unstaged diff (`index` → `worktree`), for [`Unstage`] the staged one (`HEAD` → `index`), both with 3 lines of context. Lines that are not in the current diff fail with [`STALE_SELECTION`](super::codes::STALE_SELECTION). */
  lines?: LineSelection | null;
  /** Repository-relative paths. With `lines`, exactly one. */
  paths: Array<string>;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** A stash entry, in [`StashListResult`]. */
export type Stash = {
  /** `n` of `stash@{n}`. */
  index: number;
  /** As `git stash list` shows it: `On main: message`. */
  message: string;
  /** The stash commit. */
  oid: string;
  /** When it was made, in Unix seconds. */
  time: number;
};

/** Result of [`StashList`]. */
export type StashListResult = {
  /** Newest (`stash@{0}`) first. */
  stashes: Array<Stash>;
};

/** Params of [`StashPop`], [`StashApply`] and [`StashDrop`]. */
export type StashParams = {
  /** `n` of `stash@{n}`. */
  index: number;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of [`StashPush`]. */
export type StashPushParams = {
  /** Stash untracked files too. */
  includeUntracked?: boolean;
  /** A message (default: git's `WIP on …`). */
  message?: string | null;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Result of [`StashPush`]. */
export type StashPushResult = {
  /** `false` if there was nothing to stash. */
  created: boolean;
};

/** One changed path of [`Status`]. */
export type StatusEntry = {
  /** Set for an unmerged path (then `staged` and `unstaged` are absent). */
  conflict?: Conflict | null;
  /** Repository-relative. An untracked folder git does not look into (a nested repository) ends in `/`. */
  path: string;
  /** `HEAD` → index. */
  staged?: FileChange | null;
  /** Index → work tree. */
  unstaged?: FileChange | null;
};

/** Countable progress: `current` of `total` steps done. */
export type StatusProgress = {
  /** Steps done (at most `total`). */
  current: number;
  /** All steps (at least 1). */
  total: number;
};

/** Result of [`Status`]. */
export type StatusResult = {
  /** Changed paths, sorted. Untracked files are listed one by one; ignored files are not listed; renames show as a deletion and an addition. */
  entries: Array<StatusEntry>;
  /** Where `HEAD` is. */
  head: Head;
  /** Where the repository is. */
  repo: RepoInfo;
};

/** The state an agent claims with its status (`set_status`). */
export type StatusState = "working" | "blocked" | "done";

/** One step of a stored walkthrough. */
export type Step = {
  /** The code, at least one location. */
  anchors: Array<StepAnchor>;
  /** Acceptance criteria (ids) it helps meet. */
  criteriaRefs?: Array<string>;
  /** Steps (ids) to read first. */
  dependsOn?: Array<string>;
  /** Unique within the walkthrough. */
  id: string;
  /** What it is about. */
  kind: StepKind;
  /** What could go wrong. */
  risks?: Array<Risk>;
  /** The step in a few words. */
  title: string;
  /** What the code does (markdown). */
  what: string;
  /** Why it is done this way (markdown). */
  why: string;
};

/** One code location of a step. */
export type StepAnchor = {
  /** The code as published (ADR 0013). */
  anchor: CodeAnchor;
  /** Where it is now: set by [`Get`], absent elsewhere (and in committed files). */
  resolution?: AnchorResolution | null;
};

/** One step of a walkthrough: code to read, and what, why and the risks. */
export type StepDraft = {
  /** The code the step is about: 1 to 16 line ranges. */
  anchors: Array<AnchorDraft>;
  /** The acceptance criteria (ids) this step helps meet. */
  criteria_refs?: Array<string>;
  /** Steps (ids) to read before this one. */
  depends_on?: Array<string>;
  /** Unique within the walkthrough (letters, digits, `.`, `_`, `-`; at most 32), for `depends_on`: `s1`, `discount-rule`. */
  id: string;
  /** What the step is about. */
  kind: StepKind;
  /** What could go wrong here. */
  risks?: Array<Risk>;
  /** The step in a few words. */
  title: string;
  /** What this code does (markdown). */
  what: string;
  /** Why it is done this way: the reasoning, the alternatives turned down. */
  why: string;
};

/** What a step is about, so the reader knows where they are. */
export type StepKind = "entry_point" | "flow" | "data_model" | "logic" | "edge_case" | "test" | "config" | "migration";

/** Where the reader is with one step (the player's progress). */
export type StepState = "unread" | "understood" | "question";

/** Why a turn ended (ACP `StopReason`, plus Coqo's own). */
export type StopReason = "end_turn" | "max_tokens" | "max_turn_requests" | "refusal" | "cancelled" | "error" | "interrupted";

/** Where a walkthrough is kept. */
export type StorageMode = "local" | "committed";

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

/** Params of [`SwitchBranch`]. */
export type SwitchBranchParams = {
  /** A local branch, or a remote branch's name without the remote (git then creates a tracking branch). */
  name: string;
  /** The root. */
  root: RootId;
  /** The workspace. */
  workspace: WorkspaceId;
};

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

/** A comment thread. */
export type Thread = {
  /** The code it is about. */
  anchor: ThreadAnchor;
  /** Where it lives besides Coqo. */
  backend: Backend;
  /** The comments, oldest first; at least one. */
  comments: Array<Comment>;
  /** Started, Unix milliseconds. */
  createdAt: number;
  /** The diff it was started in (a diff source's identity, such as `git:0:index..worktree`), if any. */
  diff?: string | null;
  /** Id. */
  id: ThreadId;
  /** Resolved, Unix milliseconds. */
  resolvedAt?: number | null;
  /** The `threads/changes` seq of this state. */
  seq: number;
  /** Open or resolved. */
  status: ThreadStatus;
  /** Last changed, Unix milliseconds. */
  updatedAt: number;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** The code a thread is about. */
export type ThreadAnchor = {
  /** The lines as they were when the thread was started; absent for a thread on the whole file. */
  code?: CodeAnchor | null;
  /** The lines now, 1-based; as they were when orphaned. Absent for a thread on the whole file. */
  lines?: ThreadLines | null;
  /** `/`-separated path of the file, relative to the root. */
  path: string;
  /** Where `code` is in `version` now (absent with `code`). Its range is never in an open buffer: it is in the version as git sees it. */
  resolution?: AnchorResolution | null;
  /** The root `path` is relative to. */
  root: RootId;
  /** The diff side it was started on. */
  side: ThreadSide;
  /** The version of the file the lines are in: `worktree` for the new side of unstaged changes, `index` for their old side, `{"rev": "HEAD"}`, … A diff shows the thread on whichever side shows this version. */
  version: DiffSide;
};

/** An event of [`Changes`]. */
export type ThreadEvent = {
  /** The thread. */
  thread: Thread;
  type: "changed";
} | {
  /** Its id. */
  id: ThreadId;
  type: "deleted";
  /** Its workspace. */
  workspace: WorkspaceId;
};

/** Identifies a thread. Assigned by the daemon, never reused. */
export type ThreadId = number;

/** Lines of a file, 1-based, `end` inclusive (as diffs number them). */
export type ThreadLines = {
  /** The last line. */
  end: number;
  /** The first line. */
  start: number;
};

/** Result of [`List`]. */
export type ThreadList = {
  /** The `threads/changes` seq this list reflects; subscribe with `since: seq` to keep it current. */
  seq: number;
  /** The threads, oldest first, open and resolved. */
  threads: Array<Thread>;
};

/** One message the user sent in a thread ([`Ask`], [`Reply`]). */
export type ThreadMessage = {
  /** Sent, Unix milliseconds. */
  at: number;
  /** The message, markdown. */
  text: string;
  /** The session turn it started: the agent's reply is that turn. */
  turn: number;
};

/** Params of [`Get`], [`Resolve`] and [`Reopen`]. */
export type ThreadParams = {
  /** The thread. */
  id: ThreadId;
};

/** The side of a diff a thread was started on. */
export type ThreadSide = "old" | "new";

/** Whether a thread still needs attention. */
export type ThreadStatus = "open" | "resolved";

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
  /** The tool's name as the agent calls it, if the agent says (`Read`, `mcp__coqo-editor__ask_question`). The Claude adapter sends it as `_meta.claudeCode.toolName`. Tool-use stats count by it. */
  name?: string | null;
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

/** Which kind of tool a call used, for tool-use stats. */
export type ToolGroup = "coqo" | "mod" | "mcp" | "agent";

/** A mod-contributed tool. */
export type ToolInfo = {
  /** What it does, for the agent. */
  description: string;
  /** JSON Schema of the arguments (an object schema). */
  inputSchema: Record<string, unknown>;
  /** The mod that contributes it. */
  mod: string;
  /** The name agents see: 1 to 64 of `A-Z a-z 0-9 _ -`. */
  name: string;
  /** Whether it only reads. */
  readOnly?: boolean;
  /** Display name. */
  title?: string | null;
};

/** What kind of tool a call is, for icons and grouping (ACP `ToolKind`). */
export type ToolKind = "read" | "edit" | "delete" | "move" | "search" | "execute" | "think" | "fetch" | "switch_mode" | "other";

/** How often a session called one tool. */
export type ToolUse = {
  /** Tool calls so far. */
  count: number;
  /** Which kind of tool it is. */
  group: ToolGroup;
  /** The tool: its own name for `coqo-editor` tools (`ask_question`), else the name the agent calls it by (`Read`, `mcp__github__get_issue`). Tool calls whose agent gives no name count by their kind (`edit`). */
  name: string;
};

/** One event of [`ToolsChanged`]: the set of tools changed; list again. */
export type ToolsChangedEvent = {
  /** How many times the set changed since the daemon started. */
  generation: number;
};

/** Params of [`Tools`] and the [`ToolsChanged`] topic. */
export type ToolsParams = {
  /** The workspace the tools are for. */
  workspace: WorkspaceId;
};

/** Result of [`Tools`]. */
export type ToolsResult = {
  /** The tools, by name. */
  tools: Array<ToolInfo>;
};

/** A changed hunk the agent marked as needing no explanation. */
export type TrivialHunk = {
  /** The hunk's lines in the new file, 0-based, `end` inclusive; absent for the whole file. */
  lines?: LineRange | null;
  /** `/`-separated path relative to the root. */
  path: string;
  /** Why it is trivial. */
  reason?: string | null;
  /** The root `path` is relative to. */
  root: RootId;
};

/** A changed hunk that needs no explanation. */
export type TrivialHunkDraft = {
  /** Last line, inclusive. Default: `start_line`. */
  end_line?: number | null;
  /** The file, relative to the root or absolute inside the workspace. It may be deleted. */
  path: string;
  /** Why it is trivial ("formatting", "import order"). */
  reason?: string | null;
  /** Root id. Default: the scope's root, or the root an absolute path is in. */
  root?: RootId | null;
  /** First line of the hunk (1-based, in the new file). Default: the whole file. */
  start_line?: number | null;
};

/** A changed hunk no step covers and no trivial hunk lists. */
export type UncoveredHunk = {
  /** Lines added. */
  added: number;
  /** The first changed lines, `+` added and `-` removed, a few at most. */
  excerpt: string;
  /** Where it is in the new file, 0-based, `end` inclusive: its lines, or for lines only removed, the line before them (the first line when they were at the top). This is where to open it; `{0, 0}` for a deleted file. */
  newRange: LineRange;
  /** The removed lines in the old file, 0-based, `end` inclusive; absent when lines were only added. */
  oldRange?: LineRange | null;
  /** `/`-separated path relative to the root (the new path of a rename). */
  path: string;
  /** Lines removed. */
  removed: number;
  /** The old path of a rename. */
  renamedFrom?: string | null;
  /** The root. */
  root: RootId;
  /** Set when the hunk is a whole file (added, deleted, binary, renamed). */
  wholeFile?: WholeFile | null;
};

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

/** A published walkthrough. */
export type Walkthrough = {
  /** The acceptance criteria it addresses. */
  acceptanceCriteria?: Array<WalkthroughCriterion>;
  /** How much of the scope's diff the steps cover. Computed when published, and again by `get` for a worktree scope (the work tree changes); absent from walkthroughs published before it existed. */
  coverage?: WalkthroughCoverage | null;
  /** Published (last published again), Unix milliseconds. */
  createdAt: number;
  /** Who published it. */
  createdBy: WalkthroughAuthor;
  /** Id. */
  id: WalkthroughId;
  /** How many times it was published again, replacing itself in place: 0 the first time. */
  revision?: number;
  /** What it covers. */
  scope: WalkthroughScope;
  /** The reader's progress by step id; a step not listed is unread. Never written to committed files. */
  stepStates?: Record<string, unknown>;
  /** The steps, in reading order. */
  steps: Array<Step>;
  /** Where it is kept. */
  storage?: WalkthroughStorage;
  /** The change in a few sentences (markdown). */
  summary: string;
  /** What changed, in a few words. */
  title: string;
  /** Hunks that need no explanation. */
  trivialHunks?: Array<TrivialHunk>;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Who published a walkthrough. */
export type WalkthroughAuthor = {
  /** Its provider (`claude-code`), or `agent` if the session is unknown. */
  agent: string;
  /** The agent session. */
  session: SessionId;
};

/** An event of [`Changes`]. */
export type WalkthroughChange = {
  /** The walkthrough's id. */
  id: WalkthroughId;
  /** What happened. */
  kind: ChangeKind;
  /** It as it is now; absent when deleted. */
  summary?: WalkthroughSummary | null;
  /** Its workspace. */
  workspace: WorkspaceId;
};

/** How much of a walkthrough scope's diff its steps cover.  Every changed hunk of the scope (`git diff` without context, so each run of changed lines is one hunk; untracked files count for a worktree scope) is **covered** when a step's anchor overlaps its lines in the new file (lines only removed: an anchor on the line before or after them), else **trivial** when `trivial_hunks` lists it (by the same overlap, or the whole file), else **uncovered**. */
export type WalkthroughCoverage = {
  /** Hunks a step's anchor covers. */
  covered: number;
  /** Changed hunks in the scope. */
  total: number;
  /** Hunks `trivial_hunks` lists (and no step covers). */
  trivial: number;
  /** Why the diff could not be computed (not a git repository, git unavailable, no commits yet); the counts are 0 then. */
  unavailable?: string | null;
  /** The uncovered hunks (`total - covered - trivial` of them), at most [`MAX_UNCOVERED`] (a summary: [`SUMMARY_UNCOVERED`]). */
  uncovered?: Array<UncoveredHunk>;
};

/** An acceptance criterion a walkthrough addresses, as it was when it was published. Steps refer to it by `id`; the session's live criteria (ADR 0017) say whether it holds now. */
export type WalkthroughCriterion = {
  /** Its id, e.g. `AC1`. */
  id: string;
  /** Its status in the session when published; absent for a criterion only the walkthrough names. */
  status?: CriterionStatus | null;
  /** What must hold. */
  text: string;
};

/** A code walkthrough as an agent publishes it with `publish_walkthrough`: the change in the order a reader should follow it (entry point → flow → data → logic → edge cases → tests), every step anchored to code. Coqo fills in the id, the author and the time. Lines are 1-based. */
export type WalkthroughDraft = {
  /** The acceptance criteria the change addresses, by id. Default: every criterion of this session (`set_acceptance_criteria`). A criterion that is not the session's needs `text`. */
  acceptance_criteria?: Array<CriterionDraft> | null;
  /** The id of this session's walkthrough this one replaces (publishing it again after a fix). Default: the session's latest walkthrough with the same title and scope is replaced, if there is one; otherwise this is a new walkthrough. */
  replaces?: string | null;
  /** Refuse to publish while a changed hunk of the scope is neither in a step's anchors nor in `trivial_hunks` (each is listed). Default: publish, and list the uncovered hunks in the result and to the reader. */
  require_coverage?: boolean;
  /** What the walkthrough covers. */
  scope: ScopeDraft;
  /** The steps, in reading order: 1 to 100. */
  steps: Array<StepDraft>;
  /** The change in two to five sentences (markdown): what it does and why. */
  summary: string;
  /** What changed, in a few words ("Volume discount for bulk orders"). */
  title: string;
  /** Changed hunks no step needs to explain (formatting, imports, renames), so the coverage check does not flag them. */
  trivial_hunks?: Array<TrivialHunkDraft>;
};

/** Identifies a walkthrough: a slug of its title and a random suffix, e.g. `volume-discount-3f2a9c1b`. Assigned by the daemon; also the committed file's name. */
export type WalkthroughId = string;

/** Result of [`List`]. */
export type WalkthroughList = {
  /** Newest first. */
  walkthroughs: Array<WalkthroughSummary>;
};

/** A file location, for opening a walkthrough where it starts. */
export type WalkthroughLocation = {
  /** The anchored lines when published, 0-based, `end` inclusive. */
  lines: LineRange;
  /** `/`-separated path relative to the root. */
  path: string;
  /** The root. */
  root: RootId;
};

/** Params of [`Get`]. */
export type WalkthroughParams = {
  /** The walkthrough. */
  id: WalkthroughId;
};

/** One thing wrong with a draft. */
export type WalkthroughProblem = {
  /** What is wrong and how to fix it. */
  message: string;
  /** Where, as a JSON path into the draft: `steps[2].anchors[0].end_line` (empty for the whole draft). */
  path: string;
};

/** The `data` of [`INVALID_WALKTHROUGH`](super::codes::INVALID_WALKTHROUGH). */
export type WalkthroughProblems = {
  /** Every problem found, in document order. */
  problems: Array<WalkthroughProblem>;
};

/** What a stored walkthrough covers, with its revisions resolved. */
export type WalkthroughScope = {
  /** The base revision as the agent named it (`HEAD`, `main`). */
  base?: string | null;
  /** The base commit when published (absent outside a git repository). */
  baseCommit?: string | null;
  /** The head revision as named; absent for the working tree. */
  head?: string | null;
  /** The head commit when published; absent for the working tree. */
  headCommit?: string | null;
  /** Worktree, commits or a pull request. */
  kind: ScopeKind;
  /** The workspace root (repository) of the change. */
  root: RootId;
};

/** Where a walkthrough is kept. */
export type WalkthroughStorage = {
  /** Why the committed file could not be written; the walkthrough is kept locally anyway. */
  error?: string | null;
  /** Local, or committed too. */
  mode: StorageMode;
  /** The committed file, relative to the scope's root (`.coqo/walkthroughs/<id>.json`). */
  path?: string | null;
};

/** A walkthrough in a list, the transcript and change events. */
export type WalkthroughSummary = {
  /** How much of the diff the steps cover, with at most [`SUMMARY_UNCOVERED`] of the uncovered hunks listed. */
  coverage?: WalkthroughCoverage | null;
  /** Published (last published again), Unix milliseconds. */
  createdAt: number;
  /** Id. */
  id: WalkthroughId;
  /** How many are marked with a question. */
  questions?: number;
  /** How many times it was published again: 0 the first time. */
  revision?: number;
  /** Worktree, commits or a pull request. */
  scope: ScopeKind;
  /** The publishing session. */
  session: SessionId;
  /** The first step's first anchor, where a reader starts. */
  start?: WalkthroughLocation | null;
  /** How many steps. */
  steps: number;
  /** Local or committed. */
  storage: StorageMode;
  /** Title. */
  title: string;
  /** How many are marked understood. */
  understood?: number;
  /** The workspace. */
  workspace: WorkspaceId;
};

/** Params of the [`Watch`] topic. */
export type WatchParams = {
  /** The question. */
  id: QuestionId;
  /** The watching agent session: the one that asked. */
  session: string;
  /** The watching session's workspace. */
  workspace: WorkspaceId;
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

/** A changed file a hunk stands for as a whole. */
export type WholeFile = "added" | "deleted" | "binary" | "renamed";

/** One window's editor context for the workspace it shows. */
export type WindowContext = {
  /** Whether the window has the keyboard focus. */
  focused?: boolean;
  /** Its file tabs, in tab order. */
  tabs: Array<EditorTab>;
};

/** A window's context as the daemon keeps it. */
export type WindowState = {
  /** Whether the window has the keyboard focus. */
  focused?: boolean;
  /** Its file tabs, in tab order. */
  tabs: Array<EditorTab>;
  /** When it last published, in milliseconds since the Unix epoch. */
  updatedAtMs: number;
  /** The window, as its UI named it. */
  window: string;
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
  /** Create an empty file or a folder in a workspace root (daemon token only). (client→server) */
  "fs/create": { params: CreateParams; result: EntryResult };
  /** Rename or move an entry within a workspace root; open buffers follow it (daemon token only). (client→server) */
  "fs/rename": { params: RenameParams2; result: EntryResult };
  /** Move an entry to the OS trash, or delete it for good with permanent (daemon token only). (client→server) */
  "fs/delete": { params: DeleteParams; result: DeleteResult };
  /** Bring back an entry fs/delete deleted, from the OS trash or Coqo's copy (daemon token only). (client→server) */
  "fs/restore": { params: RestoreParams; result: EntryResult };
  /** Fuzzy-find files in an open workspace from the daemon's file index (daemon token only). (client→server) */
  "fs/find": { params: FindParams; result: FindResult };
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
  /** Change one of a session's config options (model, effort, a toggle, …). (client→server) */
  "sessions/setConfigOption": { params: SetConfigOptionParams; result: null };
  /** The agent's status line, progress and claimed state (session tokens: own session). (client→server) */
  "sessions/setStatus": { params: SetStatusParams; result: SessionInfo };
  /** Set or update the session's acceptance criteria (session tokens: own session). (client→server) */
  "sessions/setCriteria": { params: SetCriteriaParams; result: SessionInfo };
  /** Give a session the user's title (kept across restarts; the agent's title no longer replaces it). (client→server) */
  "sessions/rename": { params: RenameParams3; result: SessionInfo };
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
  /** What the windows showing a workspace have open, focused window first. (client→server) */
  "editor/getContext": { params: GetContextParams; result: EditorContext };
  /** Ask the UI to open a file and select a range. (client→server) */
  "editor/reveal": { params: RevealParams; result: RevealResult };
  /** The commands mods contribute, for the command palette and keybindings. (client→server) */
  "commands/list": { params: null; result: ListResult2 };
  /** Run a mod-contributed command. (client→server) */
  "commands/run": { params: RunParams; result: RunResult };
  /** The MCP tools mods contribute, as the coqo-editor bridge offers them. (client→server) */
  "mcp/tools": { params: ToolsParams; result: ToolsResult };
  /** Call a mod-contributed MCP tool for an agent session. (client→server) */
  "mcp/call": { params: CallParams; result: CallResult };
  /** A workspace's questions and notes, anchors resolved now, with the changes seq. (client→server) */
  "questions/list": { params: ListParams; result: QuestionList };
  /** One question, anchors resolved now. (client→server) */
  "questions/get": { params: QuestionParams; result: Question };
  /** Answer an open question. (client→server) */
  "questions/answer": { params: AnswerParams; result: Question };
  /** Close a question without an answer, or a note. (client→server) */
  "questions/dismiss": { params: QuestionParams; result: Question };
  /** The user asks an agent session about code: a thread pinned to it. (client→server) */
  "questions/ask": { params: AskParams; result: Question };
  /** A follow-up in a thread the user started. (client→server) */
  "questions/reply": { params: ReplyParams; result: Question };
  /** An agent asks a question or adds a note pinned to code. (client→server) */
  "questions/create": { params: CreateParams2; result: Question };
  /** A root's repository: branch and changed paths. (client→server) */
  "git/status": { params: RepoParams; result: StatusResult };
  /** One file's hunks between two sides (work tree, index, a revision). (client→server) */
  "git/diff": { params: DiffParams; result: FileDiff };
  /** The files that differ between two sides, with line counts. (client→server) */
  "git/diffFiles": { params: DiffFilesParams; result: DiffFilesResult };
  /** Stage whole files, or chosen lines of one file. (client→server) */
  "git/stage": { params: StageParams; result: null };
  /** Unstage whole files, or chosen lines of one file. (client→server) */
  "git/unstage": { params: StageParams; result: null };
  /** Commit what is staged, or amend. (client→server) */
  "git/commit": { params: CommitParams; result: CommitResult };
  /** Local and remote branches. (client→server) */
  "git/branches": { params: RepoParams; result: BranchesResult };
  /** Switch to a branch. (client→server) */
  "git/switchBranch": { params: SwitchBranchParams; result: null };
  /** Create a branch, and optionally switch to it. (client→server) */
  "git/createBranch": { params: CreateBranchParams; result: null };
  /** The stash, newest first. (client→server) */
  "git/stashList": { params: RepoParams; result: StashListResult };
  /** Stash the changes. (client→server) */
  "git/stashPush": { params: StashPushParams; result: StashPushResult };
  /** Apply a stash and drop it. (client→server) */
  "git/stashPop": { params: StashParams; result: null };
  /** Apply a stash and keep it. (client→server) */
  "git/stashApply": { params: StashParams; result: null };
  /** Drop a stash. (client→server) */
  "git/stashDrop": { params: StashParams; result: null };
  /** Which commit last changed each line of a file. (client→server) */
  "git/blame": { params: BlameParams; result: BlameResult };
  /** Commits newest first, a page at a time, with graph lanes. (client→server) */
  "git/log": { params: LogParams; result: LogResult };
  /** The alerts not dismissed, newest first, with the changes seq. (client→server) */
  "alerts/list": { params: ListParams2; result: AlertList };
  /** Mark alerts (or an agent session's) read. (client→server) */
  "alerts/markRead": { params: MarkReadParams; result: AlertsChanged };
  /** Hide alerts. (client→server) */
  "alerts/dismiss": { params: DismissParams; result: AlertsChanged };
  /** Hide every alert, or a workspace's. (client→server) */
  "alerts/dismissAll": { params: DismissAllParams; result: AlertsChanged };
  /** Do-not-disturb and the alert sources, muted or not. (client→server) */
  "alerts/settings": { params: null; result: AlertSettingsInfo };
  /** Change do-not-disturb or mute sources. (client→server) */
  "alerts/setSettings": { params: SetSettingsParams; result: AlertSettingsInfo };
  /** A workspace's walkthroughs, newest first. (client→server) */
  "walkthroughs/list": { params: ListParams3; result: WalkthroughList };
  /** One walkthrough, anchors resolved against the files as they are now. (client→server) */
  "walkthroughs/get": { params: WalkthroughParams; result: Walkthrough };
  /** Forget a walkthrough (and optionally its committed file). (client→server) */
  "walkthroughs/delete": { params: DeleteParams2; result: boolean };
  /** Mark a step unread, understood or with a question. (client→server) */
  "walkthroughs/setStepState": { params: SetStepStateParams; result: WalkthroughSummary };
  /** An agent publishes a walkthrough; every problem of a bad draft is reported at once. (client→server) */
  "walkthroughs/create": { params: CreateParams3; result: Walkthrough };
  /** A workspace's comment threads, anchors resolved now, with the changes seq. (client→server) */
  "threads/list": { params: ListParams4; result: ThreadList };
  /** One thread, anchors resolved now. (client→server) */
  "threads/get": { params: ThreadParams; result: Thread };
  /** Start a thread on lines of a file version (or the whole file). (client→server) */
  "threads/create": { params: CreateParams4; result: Thread };
  /** Add the user's comment to a thread. (client→server) */
  "threads/reply": { params: ReplyParams2; result: Thread };
  /** Change one of the user's comments. (client→server) */
  "threads/editComment": { params: EditCommentParams; result: Thread };
  /** Delete one of the user's comments; an empty thread goes with it. (client→server) */
  "threads/deleteComment": { params: CommentParams; result: DeleteCommentResult };
  /** Mark a thread resolved. (client→server) */
  "threads/resolve": { params: ThreadParams; result: Thread };
  /** Open a resolved thread again. (client→server) */
  "threads/reopen": { params: ThreadParams; result: Thread };
  /** The user's pending review: the comments not sent yet. (client→server) */
  "review/pending": { params: ReviewParams; result: PendingReview };
  /** Delete the pending review's comments. (client→server) */
  "review/discard": { params: ReviewParams; result: DiscardResult };
  /** Send the pending review to an agent session in one prompt, then clear pending. (client→server) */
  "review/send": { params: SendReviewParams; result: SentReview };
  /** An agent session replies in a review thread of its workspace, optionally resolving it. (client→server) */
  "review/reply": { params: AgentReplyParams; result: Thread };
  /** Replace a search's matches in chosen files: open buffers are edited, other files written. (client→server) */
  "search/replace": { params: ReplaceParams; result: ReplaceResult };
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
  /** A UI window's editor context (tabs, selections, visible lines) for its workspace. (client→server) */
  "editor/context": { params: PublishContextParams };
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
  /** Reveal requests for UIs to act on (live only). */
  "editor/reveals": { params: RevealsParams; event: RevealEvent };
  /** The mod-contributed MCP tools changed; list them again (live only). */
  "mcp/toolsChanged": { params: ToolsParams; event: ToolsChangedEvent };
  /** Questions whose state changed, as their latest state (resumable). */
  "questions/changes": { params: ChangesParams4; event: Question };
  /** One question for the agent that asked: its state now, then every change. */
  "questions/watch": { params: WatchParams; event: Question };
  /** A root's repository may have changed: work tree, index, HEAD, refs or stash (live only). */
  "git/changes": { params: ChangesParams5; event: GitChanges };
  /** Alerts whose state changed, as their latest state (resumable). */
  "alerts/changes": { params: ChangesParams6; event: Alert };
  /** Walkthroughs published, updated or deleted (live only). */
  "walkthroughs/changes": { params: ChangesParams7; event: WalkthroughChange };
  /** Threads whose state changed, as their latest state, and deleted ones (resumable). */
  "threads/changes": { params: ChangesParams8; event: ThreadEvent };
  /** Search a workspace: batches of files with matches as they are searched, then a summary; unsubscribe to cancel. */
  "search/run": { params: RunParams2; event: SearchEvent };
}

/** Encode one frame: u32 big-endian length, then the JSON payload. */
export function encodeFrame(message: Message): Uint8Array {
  const body = new TextEncoder().encode(JSON.stringify(message));
  const frame = new Uint8Array(4 + body.length);
  new DataView(frame.buffer).setUint32(0, body.length, false);
  frame.set(body, 4);
  return frame;
}
