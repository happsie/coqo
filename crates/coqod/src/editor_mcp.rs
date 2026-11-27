// SPDX-License-Identifier: Apache-2.0

//! The `coqo-editor` MCP server for agent sessions (PLAN §7.4, ADR 0012):
//! how the daemon launches `coqod mcp` and what it hands it.
//!
//! When a session's provider starts (or resumes), [`EditorMcp::servers`]
//! issues a **one-time token** for that session's scope
//! ([`Auth::issue_ticket`]) and describes the server: `coqod mcp
//! --workspace <id> --session <id>`, with the token in `COQO_SESSION_TOKEN`
//! and the runtime directory in `COQO_RUNTIME_DIR`. Nothing on `argv` is
//! secret. The Claude adapter still puts the environment on Claude Code's
//! command line (ADR 0005), so the token is good for one connection, within
//! [`TICKET_TTL`]: by the time anyone could read it there, it is spent.
//!
//! [`revoke_on_close`] ends a session's scope when the session closes: its
//! tokens stop working and the bridge's connection is cut, so the bridge
//! exits.
//!
//! Every agent session with a workspace gets the server ([`source`]):
//! each start and resume, and the fresh start after a failed resume. Set
//! `COQO_EDITOR_MCP=0` to leave it out, for debugging an agent without it.
//! Providers merge it with the agent's own MCP configuration: the Claude
//! adapter loads the user's Claude Code settings (`settingSources` user,
//! project and local) and adds Coqo's servers to them (ADR 0012).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use coqo_agents::model::SessionInfo;
use coqo_agents::{McpServer, McpServerSource, SessionManager};
use coqo_client::{DaemonPaths, RUNTIME_DIR_ENV, SESSION_TOKEN_ENV};
use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::auth::{Auth, TICKET_TTL};
use crate::daemon::DaemonContext;

/// Set to `0` (or `false`, `off`) to leave the server out of agent
/// sessions. It is on by default.
pub const SWITCH_ENV: &str = "COQO_EDITOR_MCP";

/// Whether a [`SWITCH_ENV`] value turns the server off.
pub fn switched_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let v = v.trim();
        ["0", "false", "off", "no"]
            .iter()
            .any(|off| v.eq_ignore_ascii_case(off))
    })
}

/// The session manager's MCP server source for this daemon
/// (`ManagerOptions::mcp_servers`): `coqo-editor` for every session, unless
/// [`SWITCH_ENV`] turns it off.
pub fn source(ctx: &DaemonContext) -> Result<Option<McpServerSource>> {
    if switched_off(std::env::var(SWITCH_ENV).ok().as_deref()) {
        tracing::info!("{SWITCH_ENV} is off: agent sessions get no coqo-editor MCP server");
        return Ok(None);
    }
    Ok(Some(EditorMcp::for_daemon(ctx)?.source()))
}

/// The running `coqod`, as an absolute path an agent can launch.
///
/// `current_exe` is already resolved on Linux (`/proc/self/exe`) but can be
/// a symlink on macOS, so it is canonicalized. When the binary was replaced
/// while the daemon runs (a rebuild under `target/release`), Linux names it
/// `<path> (deleted)`. The bridge is then launched from the new binary at
/// the same path, which speaks the same protocol unless the rebuild changed
/// it.
pub fn coqod_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot find the coqod binary")?;
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(path) if !exe.exists() => {
            tracing::warn!(
                path,
                "coqod was replaced while running; agents launch the new binary"
            );
            PathBuf::from(path)
        }
        _ => exe,
    };
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// How the daemon launches `coqod mcp` for a session.
#[derive(Debug, Clone)]
pub struct EditorMcp {
    /// The `coqod` binary.
    pub program: PathBuf,
    /// This daemon's runtime directory, so the bridge finds its socket.
    pub paths: DaemonPaths,
    /// Where the one-time tokens are issued.
    pub auth: Auth,
    /// How long a token stays valid ([`TICKET_TTL`]).
    pub ttl: Duration,
}

impl EditorMcp {
    /// For this daemon: its own binary, runtime directory and tokens.
    pub fn for_daemon(ctx: &DaemonContext) -> Result<Self> {
        Ok(Self {
            program: coqod_binary()?,
            paths: ctx.paths.clone(),
            auth: ctx.auth.clone(),
            ttl: TICKET_TTL,
        })
    }

    /// The servers for `info`, each start or resume: `coqo-editor` with a
    /// fresh one-time token. None for a session without a workspace, since
    /// a token is scoped to one.
    pub fn servers(&self, info: &SessionInfo) -> Vec<McpServer> {
        let Some(workspace) = &info.workspace else {
            return Vec::new();
        };
        let session = &info.id.0;
        let token = match self.auth.issue_ticket(workspace, session, self.ttl) {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!(%session, error = %e, "cannot issue a token for coqod mcp");
                return Vec::new();
            }
        };
        vec![McpServer {
            name: coqo_mcp::SERVER_NAME.to_owned(),
            command: self.program.clone(),
            args: vec![
                "mcp".to_owned(),
                "--workspace".to_owned(),
                workspace.clone(),
                "--session".to_owned(),
                session.clone(),
            ],
            env: vec![
                (SESSION_TOKEN_ENV.to_owned(), token.expose().to_owned()),
                (
                    RUNTIME_DIR_ENV.to_owned(),
                    self.paths.runtime_dir().to_string_lossy().into_owned(),
                ),
            ],
        }]
    }

    /// [`Self::servers`] as the session manager's MCP server source
    /// (`ManagerOptions::mcp_servers`).
    pub fn source(self) -> McpServerSource {
        Arc::new(move |info| self.servers(info))
    }
}

/// Revoke each agent session's scope when it closes, until `shutdown`.
pub fn revoke_on_close(manager: &SessionManager, auth: Auth, shutdown: CancellationToken) {
    let manager = manager.clone();
    tokio::spawn(async move {
        let mut since = None;
        loop {
            let feed = match manager.changes(None, since) {
                Ok(feed) => feed,
                Err(e) => {
                    tracing::error!(error = %e.message, "cannot follow agent sessions; closed sessions keep their tokens");
                    return;
                }
            };
            since = Some(since.unwrap_or(feed.head));
            let mut events = feed.events;
            loop {
                let event = tokio::select! {
                    () = shutdown.cancelled() => return,
                    event = events.next() => event,
                };
                match event {
                    Some(Ok(change)) => {
                        since = Some(change.seq);
                        let info = change.event;
                        if info.closed
                            && let Some(workspace) = &info.workspace
                        {
                            let n = auth.revoke_session(workspace, &info.id.0);
                            tracing::debug!(session = %info.id, tokens = n, "agent session closed; scope revoked");
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e.message, "agent session feed failed; resuming");
                        break;
                    }
                    None => break,
                }
            }
            // Resume after the last change seen; don't spin on a failing feed.
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    });
}

/// `coqod mcp`: serve the coqo-editor MCP server on stdin and stdout.
pub async fn run(args: &crate::cli::McpArgs) -> Result<coqo_mcp::Exit> {
    let paths = DaemonPaths::from_env()?;
    let options = coqo_mcp::BridgeOptions {
        workspace: args.workspace.as_str().into(),
        session: args.session.clone(),
        build: crate::daemon::BUILD.to_owned(),
    };
    Ok(coqo_mcp::run_stdio(&paths, &options).await?)
}

#[cfg(test)]
mod tests;
