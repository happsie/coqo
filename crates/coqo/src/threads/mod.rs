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
//!   comments to <agent>", #95).
//! - **Sending** ([`send_review`], #95): `review/send` delivers a diff's
//!   pending review to an agent session in one prompt. The store remembers
//!   each [`SentReview`] until its session's turn ends (from the chat
//!   list), so the diff header says "Claude is addressing N comments" and
//!   threads the agent resolves stay open until then. The agent's replies
//!   come on the feed like any change.
//! - [`OpenReviewDiff`]: the chat's "Review sent" card opens the diff.
//! - **File tabs** (#258): `files::editor` marks the threads on a file's
//!   work-tree version and opens [`overlay::ThreadCard`] in place.
//!   "Toggle resolved review threads in files" ([`ToggleResolvedThreads`])
//!   shows the resolved ones too (per workspace, for the session).
//!
//! Every call goes to the daemon (`threads/*`, `review/*`); the store takes
//! in the thread each returns, and the feed brings the rest.

pub mod overlay;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use coqo_protocol::methods::{
    agents::{self as agm, AgentState, SessionId, SessionInfo},
    threads::{
        self as tm, Author, ChangesParams, CommentId, CommentParams, CreateParams,
        EditCommentParams, ListParams, ReplyParams, ReviewParams, SendReviewParams, Thread,
        ThreadEvent, ThreadId, ThreadList, ThreadParams,
    },
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    dock::{AnyTab, Dock},
    gpui::{
        Action, App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task,
        Window,
    },
};

use coqo_ui::gpui; // The action derives spell their paths `gpui::…`.

use crate::{
    chat::list::ChatList,
    daemon::{Daemon, DaemonError},
    git::diff::{self as diff_tab, DiffTab},
    resume::{Resumable, Update},
};

/// Open the diff a review was sent from (the chat's "Review sent" card).
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = threads, no_json)]
pub struct OpenReviewDiff {
    pub workspace: String,
    /// The diff source's identity (`git:0:index..worktree`).
    pub diff: String,
    /// The file to show in it (root-relative), if any (#258).
    pub path: Option<String>,
}

/// Show or hide resolved review threads in file tabs (#258).
#[derive(Clone, Debug, PartialEq, Eq, Default, Action)]
#[action(namespace = threads, no_json)]
pub struct ToggleResolvedThreads;

/// Register the threads' commands and keys.
pub fn init(cx: &mut App) {
    overlay::init(cx);
    crate::commands::register(
        cx,
        [crate::commands::Command::new(
            "threads.toggle_resolved_in_files",
            "Toggle resolved review threads in files",
            "Review",
            ToggleResolvedThreads,
        )],
    );
    cx.on_action(|_: &ToggleResolvedThreads, cx| {
        let Some(ws) = crate::workspace::Workspaces::global(cx)
            .and_then(|w| w.read(cx).current().map(|ws| ws.id.clone()))
        else {
            return;
        };
        if let Some(store) = threads_for(&ws, cx) {
            store.update(cx, |store, cx| {
                let shown = store.shows_resolved();
                store.set_show_resolved(!shown, cx);
            });
        }
    });
}

/// The root and side of a git diff source's identity:
/// `git:<root>:index..worktree` (unstaged) or `git:<root>:HEAD..index`
/// (staged).
pub fn git_diff_of(identity: &str) -> Option<(RootId, bool)> {
    let rest = identity.strip_prefix("git:")?;
    let (root, side) = rest.split_once(':')?;
    let root = RootId(root.parse().ok()?);
    match side {
        "index..worktree" => Some((root, false)),
        "HEAD..index" => Some((root, true)),
        _ => None,
    }
}

/// Handle [`OpenReviewDiff`] in the window's dock: show (or open) the diff
/// tab of that root and side.
pub fn open_review_diff(
    dock: &Entity<Dock>,
    action: &OpenReviewDiff,
    window: &mut Window,
    cx: &mut App,
) {
    let Some((root, staged)) = git_diff_of(&action.diff) else {
        return;
    };
    let workspace = WorkspaceId(action.workspace.clone());
    let key = diff_tab::key(&workspace, root, staged);
    if let Some(open) = dock.read(cx).find_tab(diff_tab::KIND, &key, cx) {
        let tab = dock
            .read(cx)
            .tab(open)
            .and_then(|t| t.downcast::<DiffTab>());
        if let (Some(tab), Some(path)) = (tab, &action.path) {
            let view = tab.read(cx).view().clone();
            view.update(cx, |view, cx| view.select_file(path, cx));
        }
        dock.update(cx, |dock, cx| dock.activate_tab(open, window, cx));
        return;
    }
    let select = action.path.clone();
    let tab: Entity<DiffTab> =
        cx.new(|cx| DiffTab::new(workspace, root, staged, select, window, cx));
    dock.update(cx, |dock, cx| dock.open_tab(AnyTab::new(tab), window, cx));
}

/// A review sent to an agent session, followed until its turn ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentReview {
    /// The session it went to.
    pub session: SessionId,
    /// The diff it was sent from.
    pub diff: Option<String>,
    /// How many comments.
    pub count: u32,
    /// Their threads.
    pub threads: Vec<ThreadId>,
    /// When it was sent, Unix milliseconds.
    pub at: i64,
}

/// Now, Unix milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Whether a session is in a turn, as the chat list has it.
pub fn in_turn(info: &SessionInfo) -> bool {
    !info.closed
        && matches!(
            info.state,
            AgentState::Working | AgentState::NeedsInput | AgentState::NeedsPermission
        )
}

/// Whether the turn a review sent at `at` started is still running, as far
/// as `info` (the chat list's) tells: it is in a turn, or has not changed
/// state since the review was sent (the turn's start is on its way).
/// Without a chat list (offscreen), it counts as running.
fn still_running(at: i64, info: Option<&SessionInfo>, listed: bool) -> bool {
    if !listed {
        return true;
    }
    let Some(info) = info else {
        return false;
    };
    in_turn(info) || (!info.closed && info.state_since.unwrap_or(info.updated_at) < at)
}

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
    /// Reviews whose turn is still running.
    sent: Vec<SentReview>,
    /// Provider names by id, for agents' replies.
    providers: HashMap<String, String>,
    /// File tabs show resolved threads too (#258).
    show_resolved: bool,
    _feed: Entity<Resumable<ThreadList, ThreadEvent>>,
    _relist: Option<Task<()>>,
    _providers: Option<Task<()>>,
    /// Follows the chat list while a review is out.
    _chats: Option<Subscription>,
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
        let mut this = Self {
            workspace,
            threads: HashMap::new(),
            loaded: false,
            sent: Vec::new(),
            providers: HashMap::new(),
            show_resolved: false,
            _feed: feed,
            _relist: None,
            _providers: None,
            _chats: None,
            _subscriptions: subscriptions,
        };
        this.load_providers(cx);
        this
    }

    fn load_providers(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let providers = daemon.read(cx).request::<agm::Providers>(());
        self._providers = Some(cx.spawn(async move |this, cx| {
            if let Ok(list) = providers.await {
                this.update(cx, |this, cx| {
                    this.providers = list.into_iter().map(|p| (p.id, p.name)).collect();
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// The name an agent session's replies show: its provider's ("Claude").
    pub fn agent_name(&self, session: &SessionId, cx: &App) -> String {
        let provider =
            ChatList::global(cx).and_then(|l| l.read(cx).info(session).map(|i| i.provider.clone()));
        provider_name(provider.as_deref(), &self.providers)
    }

    /// The name of `provider` ("Claude"), for the send button.
    pub fn provider_label(&self, provider: Option<&str>) -> String {
        provider_name(provider, &self.providers)
    }

    /// The review sent from `diff` whose turn still runs.
    pub fn sent_for(&self, diff: Option<&str>) -> Option<&SentReview> {
        self.sent.iter().rev().find(|s| s.diff.as_deref() == diff)
    }

    /// Whether thread `id` is in a review whose turn still runs: the agent
    /// may still answer it, so it stays open even when resolved.
    pub fn addressing(&self, id: ThreadId) -> bool {
        self.sent.iter().any(|s| s.threads.contains(&id))
    }

    /// Follow `review` until its turn ends.
    pub fn track(&mut self, review: SentReview, cx: &mut Context<Self>) {
        self.sent.retain(|s| s.diff != review.diff);
        self.sent.push(review);
        if self._chats.is_none()
            && let Some(list) = ChatList::global(cx)
        {
            self._chats = Some(cx.observe(&list, |this, _, cx| this.check_sent(cx)));
        }
        cx.emit(ThreadsEvent::Changed);
        cx.notify();
    }

    /// Forget reviews whose turn ended.
    fn check_sent(&mut self, cx: &mut Context<Self>) {
        let list = ChatList::global(cx);
        let before = self.sent.len();
        self.sent.retain(|s| {
            let info = list.as_ref().and_then(|l| l.read(cx).info(&s.session));
            still_running(s.at, info, list.is_some())
        });
        if self.sent.is_empty() {
            self._chats = None;
        }
        if self.sent.len() != before {
            cx.emit(ThreadsEvent::Changed);
            cx.notify();
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
                sent: Vec::new(),
                providers: [("claude-acp".to_owned(), "Claude".to_owned())].into(),
                show_resolved: false,
                _feed: feed,
                _relist: None,
                _providers: None,
                _chats: None,
                _subscriptions: Vec::new(),
            }
        })
    }

    pub fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// Whether file tabs show resolved threads (#258).
    pub fn shows_resolved(&self) -> bool {
        self.show_resolved
    }

    /// Show or hide resolved threads in file tabs.
    pub fn set_show_resolved(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.show_resolved != shown {
            self.show_resolved = shown;
            cx.emit(ThreadsEvent::Changed);
            cx.notify();
        }
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

/// A provider's display name: from `names` (the daemon's), else its id
/// capitalized, else "Agent".
fn provider_name(provider: Option<&str>, names: &HashMap<String, String>) -> String {
    let Some(provider) = provider else {
        return "Agent".into();
    };
    if let Some(name) = names.get(provider) {
        return name.clone();
    }
    let base = provider.strip_suffix("-acp").unwrap_or(provider);
    let mut c = base.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => "Agent".into(),
    }
}

/// Who wrote a comment, as shown: the avatar's letter and the name.
pub fn author_label(store: &WorkspaceThreads, author: &Author, cx: &App) -> (String, String) {
    match author {
        Author::User => ("Y".into(), "You".into()),
        Author::Agent { session } => {
            let name = store.agent_name(session, cx);
            (name.chars().next().unwrap_or('A').to_string(), name)
        }
        Author::Github { login } => (
            login.chars().next().unwrap_or('?').to_uppercase().collect(),
            login.clone(),
        ),
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

/// Send the pending review of `diff` to `session`, and follow its turn.
pub fn send_review(
    store: &Entity<WorkspaceThreads>,
    diff: Option<String>,
    session: SessionId,
    cx: &mut App,
) -> Task<Result<tm::SentReview, DaemonError>> {
    let Some(daemon) = Daemon::global(cx) else {
        return Task::ready(Err(DaemonError::NotConnected));
    };
    let workspace = store.read(cx).workspace().clone();
    let at = now_ms();
    let call = daemon.read(cx).request::<tm::SendReview>(SendReviewParams {
        workspace,
        diff: diff.clone(),
        session,
    });
    let store = store.downgrade();
    cx.spawn(async move |cx| {
        let sent = call.await?;
        store
            .update(cx, |store, cx| {
                store.track(
                    SentReview {
                        session: sent.session.clone(),
                        diff,
                        count: sent.count,
                        threads: sent.threads.clone(),
                        at,
                    },
                    cx,
                );
                store.relist(cx);
            })
            .ok();
        Ok(sent)
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
