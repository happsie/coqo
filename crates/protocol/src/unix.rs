// SPDX-License-Identifier: Apache-2.0

//! Unix domain socket helpers (macOS and Linux).
//!
//! Socket placement, permissions (0600) and peer-credential checks belong to
//! the daemon lifecycle (`coqod`); these helpers only connect and serve.

use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use crate::client::{Client, ClientConfig, Notifications};
use crate::error::ProtocolError;
use crate::server::Server;

/// Connect to a Coqo Protocol server on a Unix socket and handshake.
pub async fn connect_unix(
    path: impl AsRef<Path>,
    config: ClientConfig,
) -> Result<(Client, Notifications), ProtocolError> {
    let stream = UnixStream::connect(path).await?;
    Client::connect(stream, config).await
}

/// Accept connections forever, serving each on its own task.
///
/// Per-connection errors are logged, not returned; only a failing
/// `accept` ends the loop.
pub async fn serve_unix(server: Server, listener: UnixListener) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = server.serve(stream).await {
                tracing::warn!(error = %e, "connection ended with an error");
            }
        });
    }
}
