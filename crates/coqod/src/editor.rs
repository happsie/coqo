// SPDX-License-Identifier: Apache-2.0

//! `editor/*`: what the user's editor windows show, for agents (PLAN §7.4,
//! issue #70).
//!
//! - UI windows publish their context with `editor/context` (daemon token).
//!   The service keeps the latest per connection and window, and forgets a
//!   window when it publishes `null` or its connection closes.
//! - `editor/getContext` answers for one workspace: the focused window
//!   first, then the most recently updated.
//! - `editor/reveal` checks that the file is in the workspace and publishes
//!   it on `editor/reveals`, which UIs follow (daemon token).
//!
//! **Access:** a session token (an agent's `coqod mcp`) may read the context
//! of, and reveal in, its own workspace.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use coqo_protocol::methods::editor::{
    self as edm, EditorContext, GetContextParams, PublishContextParams, RevealEvent, RevealParams,
    RevealResult, RevealsParams, WindowState,
};
use coqo_protocol::methods::fs::{EntryKind, PathParams};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_protocol::{Access, ErrorObject, EventFeed, Peer, RequestContext, Router, Scope, SeqLog};
use futures_util::StreamExt as _;

use crate::workspace::WorkspaceService;

/// Reveals kept for slow subscribers.
const REVEAL_LOG: usize = 64;

/// A workspace's windows, by connection and window id.
type Windows = BTreeMap<(u64, String), Window>;

#[derive(Debug)]
struct Window {
    peer: Peer,
    state: WindowState,
}

/// The daemon's editor-context service.
#[derive(Debug)]
pub struct EditorService {
    workspaces: Arc<WorkspaceService>,
    /// By workspace, then by (connection, window).
    windows: Mutex<HashMap<WorkspaceId, Windows>>,
    reveals: Arc<SeqLog<RevealEvent>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl EditorService {
    /// A service that checks reveals against `workspaces`.
    pub fn new(workspaces: Arc<WorkspaceService>) -> Arc<Self> {
        Arc::new(Self {
            workspaces,
            windows: Mutex::default(),
            reveals: Arc::new(SeqLog::new(REVEAL_LOG)),
        })
    }

    /// `editor/context`: keep (or forget) one window's context.
    pub fn publish(&self, peer: &Peer, p: PublishContextParams) {
        let mut all = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (peer.connection(), p.window.clone());
        match p.context {
            Some(context) => {
                // A window shows one workspace: drop it from any other.
                for (ws, windows) in all.iter_mut() {
                    if *ws != p.workspace {
                        windows.remove(&key);
                    }
                }
                all.entry(p.workspace).or_default().insert(
                    key,
                    Window {
                        peer: peer.clone(),
                        state: WindowState {
                            window: p.window,
                            updated_at_ms: now_ms(),
                            context,
                        },
                    },
                );
            }
            None => {
                if let Some(windows) = all.get_mut(&p.workspace) {
                    windows.remove(&key);
                }
            }
        }
        all.retain(|_, windows| !windows.is_empty());
    }

    /// The live windows showing `workspace`, focused first, then the most
    /// recently updated. Windows of closed connections are dropped.
    fn live(&self, workspace: &WorkspaceId) -> Vec<WindowState> {
        let mut all = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        for windows in all.values_mut() {
            windows.retain(|_, w| !w.peer.is_closed());
        }
        all.retain(|_, windows| !windows.is_empty());
        let mut states: Vec<_> = all
            .get(workspace)
            .map(|windows| windows.values().map(|w| w.state.clone()).collect())
            .unwrap_or_default();
        states.sort_by(|a, b| {
            b.context
                .focused
                .cmp(&a.context.focused)
                .then(b.updated_at_ms.cmp(&a.updated_at_ms))
        });
        states
    }

    /// `editor/getContext`.
    pub fn context(&self, p: GetContextParams) -> EditorContext {
        EditorContext {
            windows: self.live(&p.workspace),
            workspace: p.workspace,
        }
    }

    /// `editor/reveal`: the file must be a file in the workspace.
    pub async fn reveal(
        &self,
        scope: &Scope,
        p: RevealParams,
    ) -> Result<RevealResult, ErrorObject> {
        if let Some(range) = &p.range
            && range.end < range.start
        {
            return Err(ErrorObject::invalid_params("range ends before it starts"));
        }
        let stat = self
            .workspaces
            .stat(PathParams {
                workspace: p.workspace.clone(),
                root: p.root,
                path: p.path.clone(),
            })
            .await?;
        if stat.kind != EntryKind::File {
            return Err(ErrorObject::new(
                coqo_protocol::methods::codes::NOT_A_FILE,
                format!("{} is not a file", p.path),
            ));
        }
        let delivered = !self.live(&p.workspace).is_empty();
        let session = match scope {
            Scope::Session { session, .. } => Some(session.clone()),
            _ => None,
        };
        self.reveals.publish(RevealEvent {
            workspace: p.workspace,
            root: p.root,
            path: p.path,
            range: p.range,
            session,
        });
        Ok(RevealResult { delivered })
    }

    /// `editor/reveals`: live reveal requests, optionally of one workspace.
    pub fn follow(&self, p: RevealsParams) -> Result<EventFeed<RevealEvent>, ErrorObject> {
        let feed = self.reveals.subscribe(None)?;
        let Some(workspace) = p.workspace else {
            return Ok(feed);
        };
        Ok(EventFeed {
            head: feed.head,
            events: feed
                .events
                .filter(move |e| {
                    std::future::ready(e.as_ref().map_or(true, |s| s.event.workspace == workspace))
                })
                .boxed(),
        })
    }
}

/// Register `editor/*` on `router`.
pub fn register(router: Router, svc: Arc<EditorService>) -> Router {
    let publish = Arc::clone(&svc);
    let get = Arc::clone(&svc);
    let reveal = Arc::clone(&svc);
    let follow = svc;
    router
        .notification_with::<edm::PublishContext, _, _>(
            Access::daemon(),
            move |ctx: RequestContext, p| {
                publish.publish(&ctx.peer, p);
                std::future::ready(())
            },
        )
        .method_with::<edm::GetContext, _, _>(
            Access::workspace(|p| &p.workspace.0),
            move |_ctx, p| std::future::ready(Ok(get.context(p))),
        )
        .method_with::<edm::Reveal, _, _>(
            Access::workspace(|p| &p.workspace.0),
            move |ctx: RequestContext, p| {
                let s = Arc::clone(&reveal);
                async move { s.reveal(&ctx.scope, p).await }
            },
        )
        .topic_with::<edm::Reveals, _, _>(Access::daemon(), move |_ctx, p, _since| {
            std::future::ready(follow.follow(p))
        })
}

#[cfg(test)]
mod tests;
