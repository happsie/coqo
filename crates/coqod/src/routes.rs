// SPDX-License-Identifier: Apache-2.0

//! Where services register their Coqo Protocol routes.
//!
//! Each service is a module of this crate with a
//! `register(router: Router, …) -> Router` function that adds its methods
//! and topics, built from the shared [`DaemonContext`]. Add one line per
//! service below. Method types live in `coqo-protocol` so clients share
//! them (see `crates/protocol/src/daemon.rs` for an example).
//!
//! **Every route declares who may call it** with an
//! [`Access`](coqo_protocol::Access) (`method_with`, `notification_with`,
//! `topic_with`); the router checks it before the handler runs. Session
//! tokens (`coqod mcp`) are denied unless the route opens to them, and the
//! tests fail on a route without a declaration or a change to the
//! session-token allowlist (ADR 0007, *Socket security*).

use std::sync::Arc;

use anyhow::Result;
use coqo_protocol::Router;

use crate::buffers::{self, BufferConfig, BufferService};
use crate::daemon::DaemonContext;
use crate::workspace::{self, WorkspaceService};
use crate::{agents, methods, terminal};

/// Build the daemon's router. Called once at startup, inside the runtime;
/// an error stops the daemon before it accepts clients.
pub fn router(ctx: &DaemonContext) -> Result<Router> {
    let router = Router::new();
    let router = methods::register(router, ctx);
    // Services, one line each.
    let workspaces = Arc::new(WorkspaceService::open(ctx.data.clone())?);
    let router = workspace::register(router, Arc::clone(&workspaces));
    let router = agents::register(router, agents::open(ctx)?);
    let buffers = BufferService::new(workspaces, ctx.data.clone(), BufferConfig::default());
    ctx.agent_fs
        .set(Arc::new(agents::BufferFs(Arc::clone(&buffers))));
    buffers::flush_on_shutdown(&buffers, ctx.shutdown.clone());
    let router = buffers::register(router, buffers);
    let router = terminal::register(router, terminal::Terminals::for_daemon(ctx));
    Ok(router)
}

#[cfg(test)]
mod tests;
