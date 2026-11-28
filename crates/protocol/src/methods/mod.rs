// SPDX-License-Identifier: Apache-2.0

//! Application methods served by the daemon, grouped by prefix.
//!
//! Each module declares its methods and topics as types implementing
//! [`Method`](crate::Method) / [`Topic`](crate::Topic), so the UI, the CLI and
//! `coqod` share one definition. The daemon implements them; clients call
//! them with [`Client::request`](crate::Client::request) and
//! [`Client::subscribe`](crate::Client::subscribe).
//!
//! - [`workspace`]: `workspace/*`, workspaces (folder roots) and their state.
//! - [`fs`]: `fs/*`, file-system access inside a workspace (listing,
//!   reading, and the file tree's create, rename and delete) and live change
//!   events.
//! - [`agents`]: `agents/*` and `sessions/*`, agent providers and the
//!   daemon-owned agent sessions (`coqo.agents@1`, PLAN §7.3).
//! - [`buffer`]: `buffer/*`, replicated text buffers (open, edit, save).
//! - [`terminal`]: `terminal/*`, daemon-owned PTYs.
//! - [`syntax`]: `syntax/*`, symbols, outline and folding of open buffers
//!   (tree-sitter, always on).
//! - [`editor`]: `editor/*`, what the user's editor windows show (open tabs,
//!   selections, visible lines) and reveal requests, for agents.
//! - [`mcp`]: `mcp/*`, the MCP tools mods contribute, for `coqod mcp`.
//! - [`questions`]: `questions/*`, questions and notes agents pin to code,
//!   and the user's answers.
//! - [`alerts`]: `alerts/*`, what needs the user across workspaces (the
//!   alert center).
//! - [`git`]: `git/*`, status, diffs, staging, commits, branches, stash,
//!   blame and log of a root's repository (the `coqo.git` mod).
//! - [`walkthroughs`]: `walkthroughs/*`, code walkthroughs agents publish
//!   after a change, and the reader's progress.
//! - [`threads`]: `threads/*` and `review/*`, comment threads on diff lines
//!   and the user's pending review.
//! - [`commands`]: `commands/*`, the commands mods contribute, for the
//!   command palette and keybindings.
//! - [`search`]: `search/*`, project search and replace (the `coqo.search`
//!   mod).

pub mod agents;
pub mod alerts;
pub mod buffer;
pub mod commands;
pub mod editor;
pub mod fs;
pub mod git;
pub mod mcp;
pub mod questions;
pub mod search;
pub mod syntax;
pub mod terminal;
pub mod threads;
pub mod walkthroughs;
pub mod workspace;

/// Error codes of the application methods.
///
/// They live outside JSON-RPC's reserved range `-32768..=-32000`.
pub mod codes {
    /// No workspace has the given id.
    pub const UNKNOWN_WORKSPACE: i64 = 1001;
    /// The workspace has no root with the given id.
    pub const UNKNOWN_ROOT: i64 = 1002;
    /// The path is not a valid workspace-relative path, or it resolves
    /// (through `..` or a symlink) outside the workspace's roots.
    pub const PATH_OUTSIDE_WORKSPACE: i64 = 1003;
    /// The path does not exist.
    pub const NOT_FOUND: i64 = 1004;
    /// The file is larger than the read limit. `data` is
    /// [`FileTooLarge`](super::fs::FileTooLarge).
    pub const FILE_TOO_LARGE: i64 = 1005;
    /// A directory was expected.
    pub const NOT_A_DIRECTORY: i64 = 1006;
    /// A regular file was expected.
    pub const NOT_A_FILE: i64 = 1007;
    /// The operation needs an open workspace.
    pub const WORKSPACE_CLOSED: i64 = 1008;
    /// A file-system error (I/O).
    pub const IO_ERROR: i64 = 1009;
    /// Something is already at the path (`fs/create`, `fs/rename`).
    pub const ALREADY_EXISTS: i64 = 1010;
    /// The operating system refused: no permission for the path.
    pub const PERMISSION_DENIED: i64 = 1011;
    /// The entry could not be moved to the OS trash (no trash on that file
    /// system, say). It was left alone: delete it with `permanent` instead.
    pub const TRASH_FAILED: i64 = 1012;
    /// No agent session has the given id.
    pub const UNKNOWN_SESSION: i64 = 1101;
    /// No agent provider has the given id, or it is not available (its mod
    /// is not active).
    pub const UNKNOWN_PROVIDER: i64 = 1102;
    /// The session is already running a turn. Wait for it to end, or
    /// `sessions/cancel` it.
    pub const SESSION_BUSY: i64 = 1103;
    /// The session was closed; its transcript can still be read.
    pub const SESSION_CLOSED: i64 = 1104;
    /// The session has no pending permission request with the given id, or
    /// the option is not one of the request's options.
    pub const UNKNOWN_PERMISSION_REQUEST: i64 = 1105;
    /// The provider does not support this operation (e.g. session modes).
    pub const UNSUPPORTED: i64 = 1106;
    /// The agent needs the user to sign in, in the agent's own tool (for
    /// Claude: run `claude` and `/login`). Coqo never handles the login.
    pub const AUTH_REQUIRED: i64 = 1107;
    /// The provider failed (process died, protocol error, …).
    pub const PROVIDER_ERROR: i64 = 1108;
    /// Reading or writing the session store failed.
    pub const STORAGE_ERROR: i64 = 1109;

    /// No open buffer has the given id (it was unloaded, or the daemon
    /// restarted): open it again by path.
    pub const UNKNOWN_BUFFER: i64 = 1201;
    /// The file is not valid UTF-8; other encodings are not supported yet.
    pub const UNSUPPORTED_ENCODING: i64 = 1202;
    /// The revision is no longer in the buffer's log (an edit's `base_rev`,
    /// or a change to revert): reopen the buffer.
    pub const REVISION_GONE: i64 = 1203;
    /// Edits that are unsorted, overlapping, out of range or not on
    /// character boundaries.
    pub const INVALID_EDIT: i64 = 1204;
    /// The file changed on disk since it was read. The change was merged
    /// into the buffer; review it and save again, or save with `force`.
    pub const SAVE_CONFLICT: i64 = 1205;

    /// No terminal has the given id.
    pub const NO_SUCH_TERMINAL: i64 = 1301;
    /// The PTY or the process could not be started.
    pub const SPAWN_FAILED: i64 = 1302;

    /// No MCP tool has the given name (its mod was disabled, say): list the
    /// tools again.
    pub const UNKNOWN_TOOL: i64 = 1401;

    /// No question has the given id, or it is not this agent session's.
    pub const UNKNOWN_QUESTION: i64 = 1501;
    /// The question was already answered or dismissed.
    pub const QUESTION_CLOSED: i64 = 1502;
    /// The root is not in a git work tree (or the repository is bare), or
    /// the git mod is not active.
    pub const NOT_A_REPOSITORY: i64 = 1601;
    /// The chosen lines are not in the file's current diff: it changed since
    /// it was shown. Fetch the diff again.
    pub const STALE_SELECTION: i64 = 1602;
    /// `git` ran and failed (a hook, a conflict, nothing to commit); the
    /// message is what it said.
    pub const GIT_FAILED: i64 = 1603;

    /// No walkthrough has the given id (it was deleted).
    pub const UNKNOWN_WALKTHROUGH: i64 = 1701;
    /// The walkthrough draft has problems; `data` is
    /// [`WalkthroughProblems`](super::walkthroughs::WalkthroughProblems),
    /// every problem with its JSON path.
    pub const INVALID_WALKTHROUGH: i64 = 1702;

    /// No comment thread has the given id (it was deleted).
    pub const UNKNOWN_THREAD: i64 = 1801;
    /// No comment has the given id (it was deleted).
    pub const UNKNOWN_COMMENT: i64 = 1802;
    /// The comment is not the caller's to change: an agent's reply, or one
    /// from GitHub.
    pub const NOT_YOUR_COMMENT: i64 = 1803;

    /// No mod handles a command with the given id (its mod was disabled,
    /// say): list the commands again.
    pub const UNKNOWN_COMMAND: i64 = 1901;
    /// The command's handler failed; the message says why.
    pub const COMMAND_FAILED: i64 = 1902;

    /// The search's pattern or a glob does not parse, or the pattern is
    /// empty; the message says what is wrong.
    pub const INVALID_QUERY: i64 = 2001;
    /// Search is not available: the `coqo.search` mod is not active.
    pub const SEARCH_UNAVAILABLE: i64 = 2002;
}
