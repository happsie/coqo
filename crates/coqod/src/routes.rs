// SPDX-License-Identifier: Apache-2.0

//! Where services register their Coqo Protocol routes.
//!
//! Each service is a module of this crate with a
//! `register(router: Router, …) -> Router` function that adds its methods
//! and topics, built from the shared [`DaemonContext`]. Add one line per
//! service below. Method types live in `coqo-protocol` so clients share
//! them (see `crates/protocol/src/daemon.rs` for an example).

use std::sync::Arc;

use anyhow::Result;
use coqo_protocol::Router;

use crate::daemon::DaemonContext;
use crate::workspace::{self, WorkspaceService};
use crate::{agents, methods};

/// Build the daemon's router. Called once at startup; an error stops the
/// daemon before it accepts clients.
pub fn router(ctx: &DaemonContext) -> Result<Router> {
    let router = Router::new();
    let router = methods::register(router, ctx);
    // Services, one line each.
    let router = workspace::register(router, Arc::new(WorkspaceService::open(ctx.data.clone())?));
    let router = agents::register(router, agents::open(ctx)?);
    Ok(router)
}
