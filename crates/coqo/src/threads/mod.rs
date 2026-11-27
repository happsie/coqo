// SPDX-License-Identifier: Apache-2.0

//! Comment threads on diff lines and the pending review (PLAN §7.7, issue
//! #94, ADR 0022, design board 4).
//!
//! - [`WorkspaceThreads`]: a workspace's threads, from `threads/list`, kept
//!   current by `threads/changes` through a [`Resumable`]. One per
//!   workspace, shared by the windows ([`threads_for`]). A diff view lists
//!   again when it reloads its files, so anchors that moved with the code
//!   are shown where they are now.
//! - [`overlay::ThreadsOverlay`]: the threads in the diff view (gutter
//!   markers, inline cards with replies and resolve, the composer behind
//!   the `+`, "Pending" marks, the header's pending count and "Send N
//!   comments", which #95 wires to the agent).
//!
//! Every call goes to the daemon (`threads/*`, `review/*`); the store takes
//! in the thread each returns, and the feed brings the rest.

pub mod overlay;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use coqo_protocol::methods::{
    threads::{
        self as tm, ChangesParams, CommentId, CommentParams, CreateParams, EditCommentParams,
        ListParams, ReplyParams, ReviewParams, Thread, ThreadEvent, ThreadId, ThreadList,
        ThreadParams,
    },
    workspace::WorkspaceId,
};
use coqo_ui::gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task,
};

use crate::{
    daemon::{Daemon, DaemonError},
    resume::{Resumable, Update},
};

/// What changed in a [`WorkspaceThreads`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadsEvent {
    /// Threads were added, changed, deleted or listed again.
    Changed,
}

/// A workspace's comment threads. See the module docs.
pub struct WorkspaceThreads {
    workspace: WorkspaceId,
    threads: HashMap<ThreadId, Thread>,
    loaded: bool,
    _feed: Entity<Resumable<ThreadList, ThreadEvent>>,
    _relist: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for WorkspaceThreads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceThreads")
            .field("workspace", &self.workspace)
            .field("threads", &self.threads.len())
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<ThreadsEvent> for WorkspaceThreads {}

#[derive(Default)]
struct ThreadHub(HashMap<WorkspaceId, Entity<WorkspaceThreads>>);

impl Global for ThreadHub {}

/// The threads of `workspace`, followed from the first call on. `None`
/// without a daemon (some tests).
pub fn threads_for(workspace: &WorkspaceId, cx: &mut App) -> Option<Entity<WorkspaceThreads>> {
    Daemon::global(cx)?;
    if let Some(store) = cx.default_global::<ThreadHub>().0.get(workspace) {
        return Some(store.clone());
    }
    let id = workspace.clone();
    let store = cx.new(|cx| WorkspaceThreads::new(id, cx));
    cx.global_mut::<ThreadHub>()
        .0
        .insert(workspace.clone(), store.clone());
    Some(store)
}

fn list_params(workspace: &WorkspaceId) -> ListParams {
    ListParams {
        workspace: workspace.clone(),
        root: None,
        path: None,
        diff: None,
    }
}

impl WorkspaceThreads {
    fn new(workspace: WorkspaceId, cx: &mut Context<Self>) -> Self {
        let topic = ChangesParams {
            workspace: Some(workspace.clone()),
        };
        let list = list_params(&workspace);
        let feed =
            cx.new(|cx| Resumable::follow::<tm::List, tm::Changes>(list, |l| l.seq, topic, cx));
        let subscriptions = vec![cx.subscribe(&feed, |this, _, update, cx| match update {
            Update::Snapshot(list) => this.replace(list.threads.clone(), cx),
            Update::Event(event) => this.apply(event.event.clone(), cx),
        })];
        Self {
            workspace,
            threads: HashMap::new(),
            loaded: false,
            _feed: feed,
            _relist: None,
            _subscriptions: subscriptions,
        }
    }

    /// A store with `threads`, without a daemon (screenshots).
    #[cfg(test)]
    pub fn fixed(workspace: WorkspaceId, threads: Vec<Thread>, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let feed = cx.new(|cx| {
                Resumable::new(
                    |_| Box::pin(async { Err(DaemonError::NotConnected) }),
                    |_, _| Box::pin(async { Err(DaemonError::NotConnected) }),
                    cx,
                )
            });
            Self {
                workspace,
                threads: threads.into_iter().map(|t| (t.id, t)).collect(),
                loaded: true,
                _feed: feed,
                _relist: None,
                _subscriptions: Vec::new(),
            }
        })
    }

    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// Whether the first list arrived.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn get(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.get(&id)
    }

    /// Every thread, oldest first.
    pub fn all(&self) -> Vec<&Thread> {
        let mut all: Vec<_> = self.threads.values().collect();
        all.sort_by_key(|t| t.id);
        all
    }

    /// Pending comments in threads started in `diff` (every thread with
    /// `None`).
    pub fn pending_count(&self, diff: Option<&str>) -> usize {
        self.threads
            .values()
            .filter(|t| diff.is_none_or(|d| t.diff.as_deref() == Some(d)))
            .map(|t| t.pending().count())
            .sum()
    }

    /// Take in threads listed or returned by a call: each replaces what is
    /// known unless that is a newer state.
    pub fn merge(&mut self, threads: Vec<Thread>, cx: &mut Context<Self>) {
        let mut changed = false;
        for t in threads {
            if t.workspace != self.workspace {
                continue;
            }
            let newer = self.threads.get(&t.id).is_none_or(|old| t.seq >= old.seq);
            if newer && self.threads.get(&t.id) != Some(&t) {
                self.threads.insert(t.id, t);
                changed = true;
            }
        }
        if changed {
            cx.emit(ThreadsEvent::Changed);
            cx.notify();
        }
    }

    /// Forget a thread a call deleted.
    fn forget(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        if self.threads.remove(&id).is_some() {
            cx.emit(ThreadsEvent::Changed);
            cx.notify();
        }
    }

    fn replace(&mut self, threads: Vec<Thread>, cx: &mut Context<Self>) {
        self.threads = threads.into_iter().map(|t| (t.id, t)).collect();
        self.loaded = true;
        cx.emit(ThreadsEvent::Changed);
        cx.notify();
    }

    fn apply(&mut self, event: ThreadEvent, cx: &mut Context<Self>) {
        match event {
            ThreadEvent::Changed { thread } => {
                if self
                    .threads
                    .get(&thread.id)
                    .is_some_and(|old| old.seq > thread.seq)
                {
                    return;
                }
                self.threads.insert(thread.id, *thread);
            }
            ThreadEvent::Deleted { id, .. } => {
                self.threads.remove(&id);
            }
        }
        cx.emit(ThreadsEvent::Changed);
        cx.notify();
    }

    /// List again, for where anchors are now (the files changed).
    pub fn relist(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let list = daemon
            .read(cx)
            .request::<tm::List>(list_params(&self.workspace));
        self._relist = Some(cx.spawn(async move |this, cx| {
            if let Ok(list) = list.await {
                this.update(cx, |this, cx| this.merge(list.threads, cx))
                    .ok();
            }
        }));
    }
}

// ---- calls ------------------------------------------------------------------

/// Send `M` and take the thread it returns into `store`.
fn call<M>(
    store: &Entity<WorkspaceThreads>,
    params: M::Params,
    cx: &mut App,
) -> Task<Result<Thread, DaemonError>>
where
    M: coqo_protocol::Method<Result = Thread>,
{
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let call = daemon.read(cx).request::<M>(params);
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let t = call.await?;
        store
            .update(cx, |store, cx| store.merge(vec![t.clone()], cx))
            .ok();
        Ok(t)
    })
}

/// Start a thread.
pub fn create(
    store: &Entity<WorkspaceThreads>,
    params: CreateParams,
    cx: &mut App,
) -> Task<Result<Thread, DaemonError>> {
    call::<tm::Create>(store, params, cx)
}

/// Add the user's comment to a thread, pending.
pub fn reply(
    store: &Entity<WorkspaceThreads>,
    thread: ThreadId,
    body: String,
    cx: &mut App,
) -> Task<Result<Thread, DaemonError>> {
    call::<tm::Reply>(
        store,
        ReplyParams {
            thread,
            body,
            pending: true,
        },
        cx,
    )
}

/// Change one of the user's comments.
pub fn edit(
    store: &Entity<WorkspaceThreads>,
    comment: CommentId,
    body: String,
    cx: &mut App,
) -> Task<Result<Thread, DaemonError>> {
    call::<tm::EditComment>(store, EditCommentParams { comment, body }, cx)
}

/// Resolve or reopen a thread.
pub fn set_resolved(
    store: &Entity<WorkspaceThreads>,
    id: ThreadId,
    resolved: bool,
    cx: &mut App,
) -> Task<Result<Thread, DaemonError>> {
    if resolved {
        call::<tm::Resolve>(store, ThreadParams { id }, cx)
    } else {
        call::<tm::Reopen>(store, ThreadParams { id }, cx)
    }
}

/// Delete the user's `comments` of thread `thread` (its pending ones, say).
pub fn delete_comments(
    store: &Entity<WorkspaceThreads>,
    thread: ThreadId,
    comments: Vec<CommentId>,
    cx: &mut App,
) -> Task<Result<(), DaemonError>> {
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let calls: Vec<_> = comments
        .into_iter()
        .map(|comment| {
            daemon
                .read(cx)
                .request::<tm::DeleteComment>(CommentParams { comment })
        })
        .collect();
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let mut last = None;
        for call in calls {
            last = Some(call.await?);
        }
        store
            .update(cx, |store, cx| match last.and_then(|r| r.thread) {
                Some(t) => store.merge(vec![t], cx),
                None => store.forget(thread, cx),
            })
            .ok();
        Ok(())
    })
}

/// Discard the pending review of `diff`.
pub fn discard(
    store: &Entity<WorkspaceThreads>,
    diff: Option<String>,
    cx: &mut App,
) -> Task<Result<u32, DaemonError>> {
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let workspace = store.read(cx).workspace().clone();
    let call = daemon
        .read(cx)
        .request::<tm::Discard>(ReviewParams { workspace, diff });
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let result = call.await?;
        store.update(cx, |store, cx| store.relist(cx)).ok();
        Ok(result.discarded)
    })
}
