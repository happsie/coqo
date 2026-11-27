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
use crate::editor::{self, EditorService};
use crate::mcp_tools::{self, McpToolService};
use crate::questions::{self, QuestionService};
use crate::syntax::{self, SyntaxService};
use crate::workspace::{self, WorkspaceService};
use crate::{agents, methods, terminal};

/// Build the daemon's router. Called once at startup, inside the runtime;
/// an error stops the daemon before it accepts clients.
pub fn router(ctx: &DaemonContext) -> Result<Router> {
    let router = Router::new();
    let router = methods::register(router, ctx);
    let router = mcp_tools::register(
        router,
        McpToolService::new(ctx.mods.clone(), ctx.shutdown.clone()),
    );
    // Services, one line each.
    let workspaces = Arc::new(WorkspaceService::open(ctx.data.clone())?);
    let router = workspace::register(router, Arc::clone(&workspaces));
    let router = editor::register(router, EditorService::new(Arc::clone(&workspaces)));
    let sessions = agents::open(ctx)?;
    let router = agents::register(router, sessions.clone());
    let buffers = BufferService::new(
        Arc::clone(&workspaces),
        ctx.data.clone(),
        BufferConfig::default(),
    );
    ctx.agent_fs
        .set(Arc::new(agents::BufferFs(Arc::clone(&buffers))));
    buffers::flush_on_shutdown(&buffers, ctx.shutdown.clone());
    let router = questions::register(
        router,
        QuestionService::open(
            &ctx.data.global_db(),
            workspaces,
            Arc::clone(&buffers),
            Some(sessions.clone()),
        )?,
    );
    let router = syntax::register(router, SyntaxService::new(Arc::clone(&buffers)));
    let router = buffers::register(router, buffers);
    // One set of terminals: the `terminal/*` routes and ACP agents' commands
    // (issue #185) share it, so agents' commands show in `terminal/list`.
    let terminals = terminal::Terminals::for_daemon(ctx);
    let title: Arc<terminal::SessionTitle> =
        Arc::new(move |id| sessions.snapshot(id).ok()?.info.title);
    ctx.agent_terminals
        .set(Arc::new(terminal::AgentTerminalService::new(
            terminals.clone(),
            title,
        )));
    let router = terminal::register(router, terminals);
    Ok(router)
}

#[cfg(test)]
pub(crate) mod tests;
