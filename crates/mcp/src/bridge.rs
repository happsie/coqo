// SPDX-License-Identifier: Apache-2.0

//! The bridge: MCP on one side, the daemon on the other.
//!
//! [`run_stdio`] is `coqod mcp`. It reads the one-time session credential
//! from [`SESSION_TOKEN_ENV`] (never `argv`; ADR 0012), connects to the
//! running daemon (it never starts one: the credential comes from a running
//! daemon), and serves MCP on stdin and stdout with the core tools.
//!
//! It exits when stdin closes (the agent stopped it), or when the daemon
//! connection closes: the daemon stopped, or the agent session was closed and
//! the daemon cut its scope. It does not reconnect. A new daemon would not
//! know the credential (they are per daemon start and single-use), and the
//! agent process that launched this bridge is a child of the daemon's ACP
//! host, so it goes away with it; the next daemon starts a fresh bridge when
//! it resumes the session.

use coqo_client::{ConnectError, DaemonPaths, SESSION_TOKEN_ENV, connect};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_protocol::{Client, ClientConfig, SessionToken};
use rmcp::ServiceExt as _;
use rmcp::service::{QuitReason, ServerInitializeError};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::registry::ToolRegistry;
use crate::server::EditorServer;
use crate::tools;

/// What the bridge serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeOptions {
    /// The agent's workspace. Only routes calls: the daemon checks every call
    /// against the credential's own scope.
    pub workspace: WorkspaceId,
    /// The agent session (Coqo's id).
    pub session: String,
    /// This build, sent in the daemon handshake.
    pub build: String,
}

/// Why the bridge stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The agent closed stdin.
    StdinClosed,
    /// The daemon connection closed: the daemon stopped, or the session was
    /// closed and its scope revoked.
    DaemonGone,
}

/// Why the bridge could not run.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// No credential in the environment.
    #[error(
        "{SESSION_TOKEN_ENV} is not set: coqod mcp is started by coqod for an agent session, \
         which passes it a one-time credential there"
    )]
    NoToken,
    /// The daemon could not be reached, or refused the credential.
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// The MCP handshake failed.
    #[error("MCP handshake failed: {0}")]
    Mcp(String),
}

/// Serve MCP on `reader` and `writer` with `registry`, calling the daemon
/// through `daemon`, until either side closes.
pub async fn serve<R, W>(
    daemon: Client,
    options: &BridgeOptions,
    registry: ToolRegistry,
    reader: R,
    writer: W,
) -> Result<Exit, BridgeError>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let server = EditorServer::new(
        registry,
        daemon.clone(),
        options.workspace.clone(),
        &options.session,
    );
    let mcp = async move {
        match server.serve((reader, writer)).await {
            Ok(running) => match running.waiting().await {
                Ok(QuitReason::JoinError(e)) | Err(e) => Err(BridgeError::Mcp(e.to_string())),
                // Closed (stdin EOF) or cancelled.
                Ok(_) => Ok(Exit::StdinClosed),
            },
            // The agent went away before (or while) initializing.
            Err(ServerInitializeError::ConnectionClosed(_)) => Ok(Exit::StdinClosed),
            Err(e) => Err(BridgeError::Mcp(e.to_string())),
        }
    };
    tokio::select! {
        exit = mcp => exit,
        () = daemon.closed() => Ok(Exit::DaemonGone),
    }
}

/// `coqod mcp`: connect with the credential in [`SESSION_TOKEN_ENV`] and
/// serve the core tools on stdin and stdout.
pub async fn run_stdio(paths: &DaemonPaths, options: &BridgeOptions) -> Result<Exit, BridgeError> {
    let token = std::env::var(SESSION_TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .map(SessionToken::new)
        .ok_or(BridgeError::NoToken)?;
    let mut config = ClientConfig::new("coqod-mcp", options.build.clone());
    config.token = Some(token);
    let (daemon, _notifications) = connect(paths, config).await?;
    tracing::debug!(workspace = %options.workspace, session = %options.session, "coqod mcp attached");
    let registry = ToolRegistry::new();
    tools::core(&registry);
    serve(
        daemon,
        options,
        registry,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}
