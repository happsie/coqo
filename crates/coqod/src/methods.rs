// SPDX-License-Identifier: Apache-2.0

//! `daemon/status` and `daemon/shutdown`.

use std::time::UNIX_EPOCH;

use coqo_protocol::daemon::{DaemonStatus, IdleStatus, Shutdown, ShutdownResult, Status};
use coqo_protocol::{Access, PROTOCOL_VERSION, Router};

use crate::daemon::{BUILD, DaemonContext, VERSION};

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

pub fn status(ctx: &DaemonContext) -> DaemonStatus {
    let activity = ctx.activity.snapshot();
    DaemonStatus {
        version: VERSION.to_owned(),
        build: BUILD.to_owned(),
        protocol: PROTOCOL_VERSION,
        pid: std::process::id(),
        started_at_ms: ctx
            .started_at
            .duration_since(UNIX_EPOCH)
            .map(millis)
            .unwrap_or(0),
        uptime_ms: millis(ctx.started.elapsed()),
        connections: activity.connections,
        busy: activity.busy,
        idle: IdleStatus {
            phase: *ctx.idle.borrow(),
            stop_services_after_secs: ctx.idle_policy.stop_services_after.map(|d| d.as_secs()),
            exit_after_secs: ctx.idle_policy.exit_after.map(|d| d.as_secs()),
        },
    }
}

/// Register the daemon's own methods. Both need the daemon token: the
/// status lists every busy session, and a session token must not stop the
/// daemon.
pub fn register(router: Router, ctx: &DaemonContext) -> Router {
    let status_ctx = ctx.clone();
    let shutdown_ctx = ctx.clone();
    router
        .method_with::<Status, _, _>(Access::daemon(), move |_, ()| {
            let status = status(&status_ctx);
            async move { Ok(status) }
        })
        .method_with::<Shutdown, _, _>(Access::daemon(), move |req, params| {
            let busy = shutdown_ctx.activity.snapshot().busy;
            let stopping = params.force || busy.is_empty();
            if stopping {
                tracing::info!(
                    connection = req.peer.connection(),
                    client = %req.hello.client,
                    force = params.force,
                    "daemon/shutdown"
                );
                shutdown_ctx.shutdown.cancel();
            }
            let result = ShutdownResult {
                stopping,
                busy: if stopping { Vec::new() } else { busy },
            };
            std::future::ready(Ok(result))
        })
}
