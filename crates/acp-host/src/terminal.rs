// SPDX-License-Identifier: Apache-2.0

//! Commands an agent runs through the client: ACP `terminal/create`,
//! `terminal/output`, `terminal/wait_for_exit`, `terminal/kill` and
//! `terminal/release` (issue #185).
//!
//! In `coqod` the daemon's terminals implement [`AgentTerminals`], so every
//! command runs in a daemon-owned PTY the user can open as a terminal tab,
//! watch and kill. The host checks the working directory against the
//! session's workspace roots, applies the output byte limit and keeps track
//! of which session owns which terminal; the backend only runs processes.
//!
//! Without a backend (the [`TerminalHandle`] is empty when the agent
//! connects), the host does not advertise `clientCapabilities.terminal`, and
//! agents run commands themselves.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use coqo_agents::model::SessionId;
use coqo_mod_api::BoxFuture;

/// A command an agent asked to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRequest {
    /// The Coqo session it runs for.
    pub session: SessionId,
    /// The agent's name ([`AcpOptions::name`](crate::AcpOptions::name)).
    pub agent: String,
    /// The command. With no `args`, a command line for `/bin/sh -c`
    /// (agents send both forms); otherwise the program.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Working directory, already checked to be inside the workspace.
    pub cwd: PathBuf,
}

/// How a command ended, as ACP reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalExit {
    /// The exit code, if it exited normally.
    pub code: Option<u32>,
    /// The signal that ended it (such as `SIGKILL`), if one did.
    pub signal: Option<String>,
}

/// What a terminal printed so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalText {
    /// The output as plain text (no escape sequences).
    pub text: String,
    /// The start was already lost (the backend keeps a bounded history).
    pub truncated: bool,
}

/// One running (or finished) command.
pub trait AgentTerminal: Send + Sync + 'static {
    /// The daemon terminal's id, as `terminal/*` clients and the chat
    /// transcript (`ToolCallContent::Terminal`) name it.
    fn id(&self) -> String;

    /// The output so far.
    fn output(&self) -> TerminalText;

    /// How it ended; `None` while it runs.
    fn exit_status(&self) -> Option<TerminalExit>;

    /// Resolves when the command exits.
    fn wait(&self) -> BoxFuture<'static, TerminalExit>;

    /// Ends the command. The terminal stays, with its output.
    fn kill(&self);

    /// Ends the command if it runs and frees the terminal: it leaves the
    /// daemon's terminal list.
    fn close(&self);
}

/// Runs agents' commands.
pub trait AgentTerminals: Send + Sync + 'static {
    /// Starts `request`. The error is shown to the agent.
    fn create(&self, request: TerminalRequest) -> Result<Arc<dyn AgentTerminal>, String>;
}

/// An [`AgentTerminals`] that can be set after the provider is built, like
/// [`FsHandle`](crate::FsHandle): built-in provider mods activate before
/// the daemon's terminals exist. Cheap to clone; clones share the slot.
/// Agents that connect while it is empty are not offered terminals.
#[derive(Clone, Default)]
pub struct TerminalHandle(Arc<RwLock<Option<Arc<dyn AgentTerminals>>>>);

impl fmt::Debug for TerminalHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TerminalHandle")
            .field(&self.get().is_some())
            .finish()
    }
}

impl TerminalHandle {
    /// An empty handle (no client terminals).
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs agents' commands through `terminals` from now on.
    pub fn set(&self, terminals: Arc<dyn AgentTerminals>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Some(terminals);
    }

    pub(crate) fn get(&self) -> Option<Arc<dyn AgentTerminals>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// `text` cut from the beginning to at most `limit` bytes, at a character
/// boundary (ACP `outputByteLimit`), and whether anything was cut.
pub(crate) fn truncate(text: String, limit: Option<u64>) -> (String, bool) {
    let Some(limit) = limit.map(|l| usize::try_from(l).unwrap_or(usize::MAX)) else {
        return (text, false);
    };
    if text.len() <= limit {
        return (text, false);
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_from_the_start_at_a_char_boundary() {
        assert_eq!(truncate("hello".into(), None), ("hello".into(), false));
        assert_eq!(truncate("hello".into(), Some(5)), ("hello".into(), false));
        assert_eq!(truncate("hello".into(), Some(3)), ("llo".into(), true));
        assert_eq!(truncate("hello".into(), Some(0)), (String::new(), true));
        // "é" is two bytes: keeping 2 bytes of "aé!" would split it, so
        // only "!" is kept.
        assert_eq!(truncate("aé!".into(), Some(2)), ("!".into(), true));
        assert_eq!(truncate("aé!".into(), Some(3)), ("é!".into(), true));
    }
}
