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
use crate::git::{self, GitService};
use crate::mcp_tools::{self, McpToolService};
use crate::path_moves::{FollowsMoves, PathMoves};
use crate::questions::{self, QuestionService};
use crate::syntax::{self, SyntaxService};
use crate::threads::{self, ThreadService};
use crate::walkthroughs::{self, WalkthroughService};
use crate::workspace::{self, WorkspaceService};
use crate::{agents, alerts, commands, file_ops, methods, path_moves, terminal};

/// Build the daemon's router. Called once at startup, inside the runtime;
/// an error stops the daemon before it accepts clients.
pub fn router(ctx: &DaemonContext) -> Result<Router> {
    let router = Router::new();
    let router = methods::register(router, ctx);
    let router = commands::register(router, ctx.mods.clone());
    let router = mcp_tools::register(
        router,
        McpToolService::new(ctx.mods.clone(), ctx.shutdown.clone()),
    );
    // Services, one line each.
    let workspaces =
        Arc::new(WorkspaceService::open(ctx.data.clone())?.with_trash(ctx.trash.clone()));
    let router = workspace::register(router, Arc::clone(&workspaces));
    // Everything keyed by a path follows a rename (issue #288).
    let moves = PathMoves::new();
    let editor_service = EditorService::new(Arc::clone(&workspaces));
    moves.follow(Arc::clone(&editor_service) as Arc<dyn FollowsMoves>);
    let router = editor::register(router, editor_service);
    let sessions = agents::open(ctx)?;
    let router = agents::register(router, sessions.clone());
    let git_service = GitService::new(Arc::clone(&workspaces), ctx.mods.clone());
    let router = git::register(router, Arc::clone(&git_service));
    let buffers = BufferService::new(
        Arc::clone(&workspaces),
        ctx.data.clone(),
        BufferConfig::default(),
    );
    ctx.agent_fs
        .set(Arc::new(agents::BufferFs(Arc::clone(&buffers))));
    buffers::flush_on_shutdown(&buffers, ctx.shutdown.clone());
    let questions = QuestionService::open(
        &ctx.data.global_db(),
        Arc::clone(&workspaces),
        Arc::clone(&buffers),
        Some(sessions.clone()),
    )?;
    moves.follow(Arc::clone(&questions) as Arc<dyn FollowsMoves>);
    let router = questions::register(router, Arc::clone(&questions));
    let router = alerts::register(router, alerts::open(ctx, sessions.clone(), questions)?);
    let thread_service = ThreadService::open(
        &ctx.data.global_db(),
        Arc::clone(&workspaces),
        Arc::clone(&git_service),
        Some(sessions.clone()),
    )?;
    moves.follow(Arc::clone(&thread_service) as Arc<dyn FollowsMoves>);
    let router = threads::register(router, thread_service);
    let walkthrough_service = WalkthroughService::open(
        &ctx.data.global_db(),
        Arc::clone(&workspaces),
        Arc::clone(&buffers),
        Some(sessions.clone()),
        Some(git_service),
    )?;
    moves.follow(Arc::clone(&walkthrough_service) as Arc<dyn FollowsMoves>);
    let router = walkthroughs::register(router, walkthrough_service);
    path_moves::follow_outside_renames(&workspaces, Arc::clone(&moves));
    let router = file_ops::register(router, workspaces, Arc::clone(&buffers), moves);
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
