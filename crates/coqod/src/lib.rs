// SPDX-License-Identifier: Apache-2.0

//! `coqod`: the Coqo background daemon (PLAN §4.3).
//!
//! - [`auth`]: peer credentials and session tokens: who may attach.
//! - [`daemon`]: runtime directory, single-instance lock, socket, accept
//!   loop and shutdown; [`DaemonContext`] is the state services get.
//! - [`lifecycle`]: attached clients, busy guards and the idle policy.
//! - [`routes`]: **where services register their protocol routes.**
//! - [`methods`]: `daemon/status` and `daemon/shutdown`.
//! - [`mods`]: **where built-in mods are listed**; loaded and activated at
//!   startup, deactivated at shutdown.
//! - [`agents`]: daemon-owned agent sessions (`agents/*`, `sessions/*`).
//! - [`editor`]: what the user's editor windows show (`editor/*`): the
//!   context UI windows publish, and reveal requests for them.
//! - [`editor_mcp`]: the `coqo-editor` MCP server agents get (`coqod mcp`):
//!   its launch spec, one-time tokens and revocation on session close.
//! - [`mcp_tools`]: the MCP tools mods contribute (`mcp/*`), which the
//!   `coqod mcp` bridge offers next to its core tools.
//! - [`questions`]: questions and notes agents pin to code
//!   (`questions/*`), their anchors kept on the code, and the answers.
//! - [`walkthroughs`]: code walkthroughs agents publish (`walkthroughs/*`):
//!   checked, anchored, stored locally or committed to the repository.
//! - [`threads`]: comment threads on diff lines and the pending review
//!   (`threads/*`, `review/*`), anchored to the file version a diff shows.
//! - [`terminal`]: daemon-owned PTYs (`terminal/*`).
//! - [`cli`]: command-line arguments.
//! - [`git`]: `git/*`, everyday git on a root's repository, from the
//!   `coqo.git` mod, and `git/changes`.
//! - [`buffers`]: replicated text buffers (`buffer/*`): open, edit, save,
//!   disk changes and the unsaved-edit journal.
//! - [`syntax`]: symbols, outline and folding of open buffers
//!   (`syntax/*`, tree-sitter via `coqo-syntax`).
//! - [`workspace`]: workspaces (folder roots), their persisted state, and
//!   file-system access with live change events.
//! - [`file_ops`]: the file tree's create, rename and delete (`fs/create`,
//!   `fs/rename`, `fs/delete`); open buffers follow a rename.
//!
//! Clients reach the daemon through `coqo-client`, never this crate.

pub mod agents;
pub mod alerts;
pub mod auth;
pub mod buffers;
pub mod cli;
pub mod daemon;
pub mod editor;
pub mod editor_mcp;
pub mod file_ops;
pub mod git;
pub mod lifecycle;
pub mod mcp_tools;
pub mod methods;
pub mod mods;
pub mod questions;
pub mod routes;
pub mod syntax;
pub mod terminal;
pub mod threads;
pub mod walkthroughs;
pub mod workspace;

pub use daemon::DaemonContext;
pub use lifecycle::{Activity, BusyGuard, IdlePhase, IdlePolicy};
