// SPDX-License-Identifier: Apache-2.0

//! ACP agents' commands in daemon terminals (issue #185): the ACP host's
//! [`AgentTerminals`] backend. An agent's `terminal/create` becomes a PTY
//! here, in `terminal/list` like any other, so the user can open it as a
//! terminal tab (the chat links it), watch it and kill it. The host checks
//! the working directory and applies the output limit; this only runs and
//! reads the process.

use std::fmt;
use std::sync::Arc;

use coqo_acp_host::{AgentTerminal, AgentTerminals, TerminalExit, TerminalRequest, TerminalText};
use coqo_agents::model::SessionId;
use coqo_mod_api::BoxFuture;
use coqo_protocol::methods::terminal::{TermSize, TerminalStatus};

use super::{SpawnSpec, Terminal, Terminals};

/// The PTY size agents' commands start with (a tab resizes it).
const SIZE: TermSize = TermSize {
    cols: 120,
    rows: 32,
};
/// Longest command shown in a title.
const TITLE_COMMAND: usize = 48;

/// A session's title, for terminal titles.
pub type SessionTitle = dyn Fn(&SessionId) -> Option<String> + Send + Sync;

/// Runs agents' commands in the daemon's [`Terminals`].
#[derive(Clone)]
pub struct AgentTerminalService {
    terminals: Terminals,
    title: Arc<SessionTitle>,
}

impl fmt::Debug for AgentTerminalService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTerminalService")
            .finish_non_exhaustive()
    }
}

impl AgentTerminalService {
    /// Commands run in `terminals` (the daemon's one set, shared with
    /// `terminal/*`), titled with the session's title from `title`.
    pub fn new(terminals: Terminals, title: Arc<SessionTitle>) -> Self {
        Self { terminals, title }
    }

    fn title(&self, request: &TerminalRequest) -> String {
        let mut command = std::iter::once(request.command.as_str())
            .chain(request.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        if command.chars().count() > TITLE_COMMAND {
            command = command.chars().take(TITLE_COMMAND - 1).collect();
            command.push('…');
        }
        let session = (self.title)(&request.session).unwrap_or_else(|| request.agent.clone());
        format!("{session}: {command}")
    }
}

impl AgentTerminals for AgentTerminalService {
    fn create(&self, request: TerminalRequest) -> Result<Arc<dyn AgentTerminal>, String> {
        let spec = if request.args.is_empty() {
            // A command line, as most agents send it.
            SpawnSpec::command("/bin/sh")
                .arg("-c")
                .arg(&request.command)
        } else {
            SpawnSpec::command(&request.command).args(&request.args)
        };
        // Nobody types into an agent's command: no pager waiting for keys.
        let mut spec = spec
            .cwd(&request.cwd)
            .size(SIZE)
            .title(self.title(&request))
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat");
        for (key, value) in &request.env {
            spec = spec.env(key, value);
        }
        let term = self.terminals.spawn(spec).map_err(|e| e.to_string())?;
        tracing::debug!(terminal = %term.id(), session = %request.session, "agent command started");
        Ok(Arc::new(AgentTerm {
            terminals: self.terminals.clone(),
            term,
        }))
    }
}

/// One agent command's terminal.
struct AgentTerm {
    terminals: Terminals,
    term: Terminal,
}

impl AgentTerminal for AgentTerm {
    fn id(&self) -> String {
        self.term.id().0.to_string()
    }

    fn output(&self) -> TerminalText {
        let (text, truncated) = self.term.text();
        TerminalText { text, truncated }
    }

    fn exit_status(&self) -> Option<TerminalExit> {
        exit(&self.term)
    }

    fn wait(&self) -> BoxFuture<'static, TerminalExit> {
        let term = self.term.clone();
        Box::pin(async move {
            term.wait().await;
            exit(&term).unwrap_or_default()
        })
    }

    fn kill(&self) {
        // The tab, if open, stays attached and shows the exit.
        self.term.terminate();
    }

    fn close(&self) {
        if !self.terminals.close(self.term.id()) {
            // The user closed it already.
            self.term.kill();
        }
    }
}

fn exit(term: &Terminal) -> Option<TerminalExit> {
    match term.status() {
        TerminalStatus::Running => None,
        TerminalStatus::Exited { code } => Some(TerminalExit {
            code: code.and_then(|c| u32::try_from(c).ok()),
            signal: term.exit_signal().map(|s| signal_name(&s)),
        }),
    }
}

/// `SIGKILL` for the OS's "Killed" (Linux) or "Killed: 9" (macOS).
fn signal_name(described: &str) -> String {
    let name = described.split(':').next().unwrap_or(described).trim();
    let sig = match name {
        "Hangup" => "SIGHUP",
        "Interrupt" => "SIGINT",
        "Quit" => "SIGQUIT",
        "Aborted" | "Abort trap" => "SIGABRT",
        "Killed" => "SIGKILL",
        "Segmentation fault" => "SIGSEGV",
        "Broken pipe" => "SIGPIPE",
        "Terminated" => "SIGTERM",
        _ => return described.to_owned(),
    };
    sig.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_get_their_names() {
        assert_eq!(signal_name("Killed"), "SIGKILL");
        assert_eq!(signal_name("Killed: 9"), "SIGKILL");
        assert_eq!(signal_name("Hangup: 1"), "SIGHUP");
        assert_eq!(signal_name("Signal 99"), "Signal 99");
    }
}
