// SPDX-License-Identifier: Apache-2.0

//! The `file` tab kind: an editor on the daemon's replicated buffer
//! (PLAN §7.1, ADR 0006, ADR 0010).
//!
//! A tab opens the file with `buffer/open`, shows it in a [`CodeEditor`] and
//! drives a [`ClientReplica`]:
//!
//! - **Typing** goes to the replica, which sends `buffer/edit`.
//! - **Changes from elsewhere** (`buffer/changes`: agents, the disk, other
//!   windows and splits) are applied as remote edits, so the cursors stay on
//!   their text and the user's undo history is untouched. They wait while an
//!   IME composition is in progress. Agent and disk changes are tinted for a
//!   moment ([`HIGHLIGHT_FOR`]): sage for an agent, peach for another program,
//!   sand when they overlapped unsaved edits.
//! - **Undo / redo** are the replica's: they revert only this tab's own edits.
//! - **Save** (`Ctrl S` / `Cmd S`) waits for the tab's edits to be
//!   acknowledged, then sends `buffer/save`. A file that changed on disk in
//!   the meantime (`SAVE_CONFLICT`) shows a bar: overwrite it, or reload it
//!   and drop the unsaved edits.
//! - **Reconnects** resubscribe from the last revision and resend the change
//!   in flight; when the daemon can't replay (`RESUME_GAP`), forgot the
//!   buffer (`UNKNOWN_BUFFER`) or rejected an edit, the tab reopens the
//!   buffer with the same replica id and resyncs.
//! - **Closing** the tab sends `buffer/close`.
//!
//! Binary, non-UTF-8 and very large files show a message instead.

use std::{
    collections::VecDeque,
    ops::Range,
    time::{Duration, Instant},
};

use coqo_buffers::{ClientReplica, ReplicaError, Rope, UndoGrouping, diff};
use coqo_protocol::methods::{
    buffer::{
        self, BufferChange, ChangesParams, CloseParams, EditParams, EditRejected, OpenParams,
        OpenResult, OriginKind, SaveParams, TextEdit,
    },
    codes,
    fs::{self, EntryKind, FileTooLarge, PathParams, ReadParams},
    workspace::{RootId, WorkspaceId},
};
use coqo_protocol::{ErrorObject, Sequenced, codes as ProtocolCodes};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    dock::{AnyTab, TabItem},
    editor::{self, CodeEditor, EditOrigin, EditorEdits, HistoryAction},
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, FontWeight,
        InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px,
    },
    serde_json::{Value, json},
    widgets::{Icon, TestSupportExt as _, icon, raised},
};

use crate::{
    daemon::{Daemon, DaemonError, DaemonEvent, EventStream, ServerNotification},
    workspace::{Workspaces, WorkspacesEvent},
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.

coqo_ui::gpui::actions!(
    file_editor,
    [
        /// Save the file.
        Save,
    ]
);

/// The tab kind. Persisted: never change it.
pub const KIND: &str = "file";

/// Files larger than this open as a message, not in the editor.
pub const MAX_EDITOR_BYTES: u64 = fs::DEFAULT_READ_LIMIT;
/// How long agent and disk changes stay tinted.
pub const HIGHLIGHT_FOR: Duration = Duration::from_millis(2_500);
/// Typing within this long of the previous keystroke, at the same place,
/// joins its undo step.
const UNDO_GROUP_WINDOW: Duration = Duration::from_millis(800);
/// How often held remote changes check whether a composition ended.
const COMPOSITION_POLL: Duration = Duration::from_millis(30);

const KEY_CONTEXT: &str = "FileEditor";

#[cfg(target_os = "macos")]
const SAVE_KEY: &str = "cmd-s";
#[cfg(not(target_os = "macos"))]
const SAVE_KEY: &str = "ctrl-s";

/// Bind the file tab's keys.
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(SAVE_KEY, Save, Some(KEY_CONTEXT))]);
}

/// What the tab shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Loading,
    /// The editor.
    Text,
    Binary {
        size: u64,
    },
    TooLarge {
        size: u64,
        limit: u64,
    },
    Missing,
    Error(String),
}

/// The bar above the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// `buffer/save` found the file changed on disk; the change was merged
    /// into the buffer.
    SaveConflict,
    /// Saving failed.
    SaveFailed(String),
}

/// How opening the file went.
enum Opened {
    Buffer(OpenResult),
    Message(Content),
    Failed(DaemonError),
}

/// The open buffer behind a text tab.
struct Session {
    replica: ClientReplica,
    editor: Entity<CodeEditor>,
    _edits: Subscription,
}

/// A file tab.
pub struct FileEditor {
    workspace: WorkspaceId,
    root: RootId,
    path: String,
    content: Content,
    session: Option<Session>,
    focus: FocusHandle,
    /// Bumped per `buffer/changes` subscription; events of older ones are
    /// dropped.
    stream: u64,
    /// Remote changes waiting for an IME composition to end.
    held: VecDeque<BufferChange>,
    /// When and where the last typed edit ended, for undo grouping.
    last_typed: Option<(Instant, usize)>,
    /// A save waiting for this tab's edits to be acknowledged (`true`:
    /// force).
    pending_save: Option<bool>,
    saving: bool,
    notice: Option<Notice>,
    /// The buffer must be reopened and resynced (after a reconnect, a
    /// rejected edit, …) and that hasn't succeeded yet.
    resync_pending: bool,
    /// Another open was asked for while one was in flight.
    open_queued: bool,
    _opening: Option<Task<()>>,
    _changes: Option<Task<()>>,
    _saving: Option<Task<()>>,
    _reloading: Option<Task<()>>,
    _flushing: Option<Task<()>>,
    _unhighlight: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for FileEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEditor")
            .field("workspace", &self.workspace)
            .field("root", &self.root)
            .field("path", &self.path)
            .field("content", &self.content)
            .finish_non_exhaustive()
    }
}

/// Recreate a saved file tab (also how a split duplicates one).
pub fn restore(state: &Value, window: &mut Window, cx: &mut App) -> Option<AnyTab> {
    let workspace = WorkspaceId(state.get("workspace")?.as_str()?.to_owned());
    let root = RootId(u32::try_from(state.get("root")?.as_u64()?).ok()?);
    let path = state.get("path")?.as_str()?.to_owned();
    Some(AnyTab::new(cx.new(|cx| {
        FileEditor::new(workspace, root, path, window, cx)
    })))
}

/// The de-duplication key of a file tab.
pub fn key(workspace: &WorkspaceId, root: RootId, path: &str) -> String {
    format!("{workspace}/{root}/{path}")
}

impl FileEditor {
    pub fn new(
        workspace: WorkspaceId,
        root: RootId,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(
                cx.subscribe_in(&daemon, window, |this, _, event, window, cx| {
                    if *event == DaemonEvent::Connected {
                        this.reconnected(window, cx);
                    }
                }),
            );
            let notifications = daemon.read(cx).notifications();
            subscriptions.push(cx.subscribe_in(
                &notifications,
                window,
                |this, _, note: &ServerNotification, window, cx| this.notified(note, window, cx),
            ));
        }
        if let Some(workspaces) = Workspaces::for_window(window, cx) {
            subscriptions.push(cx.subscribe_in(
                &workspaces,
                window,
                |this, workspaces, event, window, cx| match event {
                    WorkspacesEvent::CurrentChanged => {
                        let ours =
                            workspaces.read(cx).current().map(|w| &w.id) == Some(&this.workspace);
                        if ours && (this.session.is_none() || this.resync_pending) {
                            this.open(window, cx);
                        }
                    }
                    WorkspacesEvent::FsChanged { workspace, changes } => {
                        // The daemon follows the file while it's open; a
                        // message (missing, binary, …) may be stale.
                        if *workspace == this.workspace
                            && changes.root == this.root
                            && this.session.is_none()
                            && (changes.rescan || changes.paths.contains(&this.path))
                        {
                            this.open(window, cx);
                        }
                    }
                },
            ));
        }
        subscriptions.push(cx.on_release(|this, cx| this.close(cx)));
        let mut this = Self {
            workspace,
            root,
            path,
            content: Content::Loading,
            session: None,
            focus: cx.focus_handle(),
            stream: 0,
            held: VecDeque::new(),
            last_typed: None,
            pending_save: None,
            saving: false,
            notice: None,
            resync_pending: false,
            open_queued: false,
            _opening: None,
            _changes: None,
            _saving: None,
            _reloading: None,
            _flushing: None,
            _unhighlight: None,
            _subscriptions: subscriptions,
        };
        this.open(window, cx);
        this
    }

    /// What the tab shows.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn content(&self) -> &Content {
        &self.content
    }

    /// The editor, once the file is open as text.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn editor(&self) -> Option<&Entity<CodeEditor>> {
        self.session.as_ref().map(|s| &s.editor)
    }

    /// Whether the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.replica.is_dirty())
    }

    /// Whether every edit of this tab reached the daemon.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_synced(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.replica.is_synced())
    }

    /// The bar above the editor, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    fn daemon(cx: &App) -> Option<Entity<Daemon>> {
        Daemon::global(cx).filter(|d| d.read(cx).is_connected())
    }

    // ---- opening ----------------------------------------------------------

    /// Open the buffer (or reopen it with the same replica, to resync).
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._opening.is_some() {
            self.open_queued = true;
            return;
        }
        if Self::daemon(cx).is_none() {
            return;
        }
        let path = PathParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: self.path.clone(),
        };
        let params = OpenParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: self.path.clone(),
            replica: self.session.as_ref().map(|s| s.replica.replica().clone()),
        };
        let first = self.session.is_none();
        self._opening = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = async {
                if first {
                    // Keep very large files and folders out of the editor.
                    let stat = this
                        .update(cx, |_, cx| {
                            Self::daemon(cx).map(|d| d.read(cx).request::<fs::Stat>(path))
                        })
                        .ok()
                        .flatten()?;
                    match stat.await {
                        Ok(stat) if stat.kind != EntryKind::File => {
                            return Some(Opened::Failed(DaemonError::Rpc(ErrorObject::new(
                                codes::NOT_A_FILE,
                                "not a file",
                            ))));
                        }
                        Ok(stat) if stat.size > MAX_EDITOR_BYTES => {
                            return Some(Opened::Message(Content::TooLarge {
                                size: stat.size,
                                limit: MAX_EDITOR_BYTES,
                            }));
                        }
                        Ok(_) => {}
                        Err(e) => return Some(Opened::Failed(e)),
                    }
                }
                let open = this
                    .update(cx, |_, cx| {
                        Self::daemon(cx).map(|d| d.read(cx).request::<buffer::Open>(params))
                    })
                    .ok()
                    .flatten()?;
                Some(match open.await {
                    Ok(open) => Opened::Buffer(open),
                    Err(e) => Opened::Failed(e),
                })
            };
            let outcome = outcome.await;
            this.update_in(cx, |this, window, cx| {
                this._opening = None;
                match outcome {
                    Some(Opened::Buffer(open)) => this.opened(open, window, cx),
                    Some(Opened::Message(content)) => this.show(content, cx),
                    Some(Opened::Failed(e)) => this.open_failed(e, cx),
                    // The connection dropped; `reconnected` opens again.
                    None => {}
                }
                if std::mem::take(&mut this.open_queued) {
                    this.open(window, cx);
                }
            })
            .ok();
        }));
    }

    fn show(&mut self, content: Content, cx: &mut Context<Self>) {
        self.content = content;
        cx.notify();
    }

    fn open_failed(&mut self, error: DaemonError, cx: &mut Context<Self>) {
        if self.session.is_some() {
            // A resync failed; keep the text, the next reconnect tries again.
            eprintln!("coqo: cannot reopen {}: {error}", self.path);
            return;
        }
        let content = match &error {
            DaemonError::Rpc(e) if e.code == codes::UNSUPPORTED_ENCODING => {
                Content::Binary { size: 0 }
            }
            DaemonError::Rpc(e) if e.code == codes::FILE_TOO_LARGE => {
                let data = e
                    .data
                    .clone()
                    .and_then(|d| coqo_ui::serde_json::from_value::<FileTooLarge>(d).ok());
                Content::TooLarge {
                    size: data.as_ref().map_or(0, |d| d.size),
                    limit: data.map_or(MAX_EDITOR_BYTES, |d| d.limit),
                }
            }
            DaemonError::Rpc(e) if e.code == codes::NOT_FOUND => Content::Missing,
            // Not reachable yet: a reconnect or the workspace opening retries.
            DaemonError::NotConnected => return,
            DaemonError::Rpc(e)
                if e.code == codes::WORKSPACE_CLOSED || e.code == codes::UNKNOWN_WORKSPACE =>
            {
                return;
            }
            e => Content::Error(e.to_string()),
        };
        if let Content::Binary { .. } = content {
            self.binary_size(cx);
        }
        self.show(content, cx);
    }

    /// Fill in a binary file's size for its message.
    fn binary_size(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let stat = daemon.read(cx).request::<fs::Stat>(PathParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: self.path.clone(),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(stat) = stat.await {
                this.update(cx, |this, cx| {
                    if let Content::Binary { size } = &mut this.content {
                        *size = stat.size;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn opened(&mut self, open: OpenResult, window: &mut Window, cx: &mut Context<Self>) {
        let rev = open.rev;
        self.held.clear();
        self.resync_pending = false;
        match &mut self.session {
            None => {
                let editor = cx.new(|cx| {
                    let mut editor = CodeEditor::new(open.text.clone(), window, cx);
                    editor.set_language(editor::language_for_path(&self.path), cx);
                    editor
                });
                let weak = cx.entity().downgrade();
                editor.update(cx, |editor, _| {
                    editor.set_history_handler(move |editor, action, window, cx| {
                        let edits = weak
                            .update(cx, |this, cx| this.history(action, cx))
                            .ok()
                            .flatten();
                        if let Some(edits) = edits {
                            apply_own(editor, &edits, window, cx);
                        }
                    });
                });
                let edits = cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
                    this.editor_edited(event, window, cx)
                });
                if self.focus.is_focused(window) {
                    editor.update(cx, |editor, cx| editor.focus(window, cx));
                }
                self.session = Some(Session {
                    replica: ClientReplica::new(&open),
                    editor,
                    _edits: edits,
                });
                self.content = Content::Text;
            }
            Some(session) => match session.replica.resync(&open) {
                Ok(applied) => {
                    let edits = to_editor(&applied.edits);
                    session.editor.update(cx, |editor, cx| {
                        editor.apply_remote_edits(&edits, window, cx)
                    });
                    if let Some(out) = applied.outgoing {
                        Self::send(out, cx);
                    }
                }
                Err(e) => {
                    // Should not happen; start over from the daemon's text.
                    eprintln!("coqo: resync of {} failed ({e}); reloading", self.path);
                    let current = Rope::from_str(&session.editor.read(cx).text(cx));
                    let edits = to_editor(&diff(&current, &Rope::from_str(&open.text)).to_edits());
                    session.editor.update(cx, |editor, cx| {
                        editor.apply_remote_edits(&edits, window, cx)
                    });
                    session.replica = ClientReplica::new(&open);
                }
            },
        }
        self.subscribe(rev, window, cx);
        self.maybe_save(cx);
        cx.notify();
    }

    // ---- buffer/changes ---------------------------------------------------

    /// Subscribe to `buffer/changes` after `since`.
    fn subscribe(&mut self, since: u64, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(daemon), Some(session)) = (Self::daemon(cx), self.session.as_ref()) else {
            return;
        };
        self.stream += 1;
        let stream = self.stream;
        let subscribe = daemon.read(cx).subscribe_since::<buffer::Changes>(
            ChangesParams {
                buffer: session.replica.buffer(),
            },
            since,
        );
        self._changes = Some(cx.spawn_in(window, async move |this, cx| {
            let mut events: EventStream<Sequenced<BufferChange>> = match subscribe.await {
                Ok(events) => events,
                Err(e) => {
                    this.update_in(cx, |this, window, cx| {
                        this.stream_ended(stream, Some(e), window, cx)
                    })
                    .ok();
                    return;
                }
            };
            this.update(cx, |this, cx| this.subscribed(stream, cx)).ok();
            let error = loop {
                match events.next().await {
                    Some(Ok(Sequenced { event: change, .. })) => {
                        let alive = this.update_in(cx, |this, window, cx| {
                            this.received(stream, change, window, cx)
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                    Some(Err(e)) => break Some(e),
                    None => break None,
                }
            };
            this.update_in(cx, |this, window, cx| {
                this.stream_ended(stream, error, window, cx)
            })
            .ok();
        }));
    }

    /// The subscription is live: resend the change still in flight (the
    /// daemon drops it if the first copy arrived).
    fn subscribed(&mut self, stream: u64, cx: &mut Context<Self>) {
        if stream != self.stream {
            return;
        }
        if let Some(out) = self.session.as_ref().and_then(|s| s.replica.resend()) {
            Self::send(out, cx);
        }
    }

    fn stream_ended(
        &mut self,
        stream: u64,
        error: Option<DaemonError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if stream != self.stream || self.session.is_none() {
            return;
        }
        self._changes = None;
        match error {
            // The daemon can't replay, or forgot the buffer: reopen.
            Some(DaemonError::Rpc(e))
                if e.code == ProtocolCodes::RESUME_GAP || e.code == codes::UNKNOWN_BUFFER =>
            {
                self.resync(window, cx);
            }
            // The connection dropped: `reconnected` resyncs.
            Some(DaemonError::NotConnected) => {}
            // Fell behind, or the subscription ended: resume from our rev.
            _ => {
                if Self::daemon(cx).is_some() {
                    self.resume(window, cx);
                }
            }
        }
    }

    /// Resubscribe from the last revision received.
    fn resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rev) = self.session.as_ref().map(|s| s.replica.rev()) else {
            return;
        };
        self.held.clear();
        self.subscribe(rev, window, cx);
    }

    /// Reopen the buffer with the same replica and catch up from the new
    /// snapshot; pending edits are rebased and sent again.
    fn resync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stream += 1;
        self._changes = None;
        self.held.clear();
        self.resync_pending = true;
        self.open(window, cx);
    }

    /// A new connection. It is usually a new daemon (the old one stopped or
    /// crashed), whose buffer ids say nothing about the old ones, so the tab
    /// reopens its buffer rather than resubscribing by id.
    fn reconnected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_some() {
            self.resync(window, cx);
        } else {
            self.open(window, cx);
        }
    }

    fn notified(&mut self, note: &ServerNotification, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rejected) = note.parse::<EditRejected>() else {
            return;
        };
        let ours = self.session.as_ref().is_some_and(|s| {
            s.replica.buffer() == rejected.buffer && *s.replica.replica() == rejected.replica
        });
        if ours {
            eprintln!(
                "coqo: an edit to {} was rejected ({}); resyncing",
                self.path, rejected.error.message
            );
            self.resync(window, cx);
        }
    }

    fn received(
        &mut self,
        stream: u64,
        change: BufferChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if stream != self.stream {
            return;
        }
        self.held.push_back(change);
        self.flush_held(window, cx);
    }

    /// Apply the held changes, unless a composition is in progress: its text
    /// isn't in the replica's coordinates yet.
    fn flush_held(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            self.held.clear();
            return;
        };
        if self.held.is_empty() {
            return;
        }
        if session.editor.read(cx).is_composing(cx) {
            if self._flushing.is_none() {
                self._flushing = Some(cx.spawn_in(window, async move |this, cx| {
                    loop {
                        cx.background_executor().timer(COMPOSITION_POLL).await;
                        let done = this.update_in(cx, |this, window, cx| {
                            let composing = this
                                .session
                                .as_ref()
                                .is_some_and(|s| s.editor.read(cx).is_composing(cx));
                            if !composing {
                                this._flushing = None;
                                this.flush_held(window, cx);
                            }
                            !composing
                        });
                        if done.unwrap_or(true) {
                            return;
                        }
                    }
                }));
            }
            return;
        }
        while let Some(change) = self.held.pop_front() {
            if !self.apply_change(change, window, cx) {
                self.held.clear();
                return;
            }
        }
    }

    /// Feed one change to the replica and the editor. `false` if the stream
    /// must be restarted.
    fn apply_change(
        &mut self,
        change: BufferChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        match session.replica.receive(&change) {
            Ok(incoming) => {
                if !incoming.edits.is_empty() {
                    let edits = to_editor(&incoming.edits);
                    let editor = session.editor.clone();
                    editor.update(cx, |editor, cx| {
                        editor.apply_remote_edits(&edits, window, cx)
                    });
                    // Someone else's edit ends the user's typing run.
                    self.last_typed = None;
                    if let Some(color) = tint(&change, cx) {
                        self.highlight(&editor, &incoming.edits, color, window, cx);
                    }
                }
                if let Some(out) = incoming.outgoing {
                    Self::send(out, cx);
                }
                if incoming.acknowledged {
                    self.maybe_save(cx);
                }
                cx.notify();
                true
            }
            Err(ReplicaError::Gap { .. }) => {
                self.resume(window, cx);
                false
            }
            Err(ReplicaError::Op(e)) => {
                eprintln!(
                    "coqo: a change to {} doesn't apply ({e}); resyncing",
                    self.path
                );
                self.resync(window, cx);
                false
            }
        }
    }

    fn highlight(
        &mut self,
        editor: &Entity<CodeEditor>,
        edits: &[TextEdit],
        color: Rgb,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ranges = post_ranges(edits);
        editor.update(cx, |editor, cx| {
            editor.highlight_changes(&ranges, color.hsla(), cx)
        });
        let editor = editor.downgrade();
        self._unhighlight = Some(cx.spawn_in(window, async move |_, cx| {
            cx.background_executor().timer(HIGHLIGHT_FOR).await;
            editor
                .update(cx, |editor, cx| editor.clear_change_highlights(cx))
                .ok();
        }));
    }

    // ---- local edits, undo ------------------------------------------------

    fn editor_edited(&mut self, event: &EditorEdits, window: &mut Window, cx: &mut Context<Self>) {
        if event.origin != EditOrigin::Local {
            return;
        }
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let now = Instant::now();
        for (ix, (range, text)) in event.edits.iter().enumerate() {
            // Some paths (an IME commit) report a replacement of far more
            // than changed; keep it to what did, so concurrent edits around
            // it survive and undo stays small.
            let (range, text) = &shrink(session.replica.text(), range.clone(), text);
            let grouping = match self.last_typed {
                _ if ix > 0 => UndoGrouping::Extend,
                Some((at, end))
                    if now.duration_since(at) < UNDO_GROUP_WINDOW
                        && (range.start == end || range.end == end)
                        && !text.contains('\n') =>
                {
                    UndoGrouping::Extend
                }
                _ => UndoGrouping::NewGroup,
            };
            let edit = TextEdit::new(range.clone(), text.clone());
            match session.replica.local_edit(&[edit], grouping) {
                Ok(Some(out)) => Self::send(out, cx),
                Ok(None) => {}
                Err(e) => {
                    eprintln!(
                        "coqo: an edit to {} doesn't apply ({e}); repairing",
                        self.path
                    );
                    self.repair(window, cx);
                    return;
                }
            }
            self.last_typed = Some((now, range.start + text.len()));
        }
        cx.notify();
        // A composition that just ended may have held changes back.
        self.flush_held(window, cx);
    }

    /// Undo or redo this tab's own edits. Returns the edits to apply to the
    /// editor (it is being updated, so the caller applies them).
    fn history(&mut self, action: HistoryAction, cx: &mut Context<Self>) -> Option<Vec<TextEdit>> {
        let session = self.session.as_mut()?;
        let applied = match action {
            HistoryAction::Undo => session.replica.undo(),
            HistoryAction::Redo => session.replica.redo(),
        };
        let applied = match applied {
            Ok(applied) => applied?,
            Err(e) => {
                eprintln!("coqo: undo in {} failed: {e}", self.path);
                return None;
            }
        };
        if let Some(out) = applied.outgoing {
            Self::send(out, cx);
        }
        self.last_typed = None;
        cx.notify();
        Some(applied.edits)
    }

    /// Make the editor show the replica's text again (they diverged).
    fn repair(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let editor = session.editor.clone();
        let current = Rope::from_str(&editor.read(cx).text(cx));
        let edits = to_editor(&diff(&current, session.replica.text()).to_edits());
        editor.update(cx, |editor, cx| {
            editor.apply_remote_edits(&edits, window, cx)
        });
        self.last_typed = None;
    }

    fn send(edit: EditParams, cx: &App) {
        if let Some(daemon) = Daemon::global(cx) {
            // Not connected: the replica keeps it in flight, and it is resent
            // after the reconnect.
            let _ = daemon.read(cx).notify::<buffer::Edit>(edit);
        }
    }

    // ---- saving -----------------------------------------------------------

    fn save(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.session.is_none() {
            return;
        }
        self.pending_save = Some(force || self.pending_save.unwrap_or(false));
        self.maybe_save(cx);
    }

    /// Send a pending save once every edit of this tab is in the daemon.
    fn maybe_save(&mut self, cx: &mut Context<Self>) {
        let Some(force) = self.pending_save else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };
        if self.saving || !session.replica.is_synced() {
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        self.pending_save = None;
        self.saving = true;
        let save = daemon.read(cx).request::<buffer::Save>(SaveParams {
            buffer: session.replica.buffer(),
            force,
        });
        self._saving = Some(cx.spawn(async move |this, cx| {
            let result = save.await;
            this.update(cx, |this, cx| this.saved(result, cx)).ok();
        }));
    }

    fn saved(&mut self, result: Result<buffer::SaveResult, DaemonError>, cx: &mut Context<Self>) {
        self.saving = false;
        self._saving = None;
        self.notice = match result {
            Ok(_) => None,
            Err(e) if e.code() == Some(codes::SAVE_CONFLICT) => Some(Notice::SaveConflict),
            Err(e) => Some(Notice::SaveFailed(e.to_string())),
        };
        self.maybe_save(cx);
        cx.notify();
    }

    /// From the conflict bar: write the buffer over the file.
    fn overwrite(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        self.save(true, cx);
        cx.notify();
    }

    /// From the conflict bar: drop the unsaved edits and show the file as it
    /// is on disk. Undo brings the edits back.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        self.notice = None;
        let read = daemon.read(cx).request::<fs::ReadFile>(ReadParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: self.path.clone(),
            max_bytes: Some(fs::MAX_READ_LIMIT),
        });
        self._reloading = Some(cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                this._reloading = None;
                match result {
                    Ok(fs::ReadResult {
                        text: Some(text), ..
                    }) => this.replace_text(&text, window, cx),
                    Ok(_) => this.notice = Some(Notice::SaveFailed("the file isn't text".into())),
                    Err(e) => this.notice = Some(Notice::SaveFailed(e.to_string())),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Replace the whole text as one undoable edit of this tab.
    fn replace_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let edits = diff(session.replica.text(), &Rope::from_str(text)).to_edits();
        if edits.is_empty() {
            return;
        }
        match session.replica.local_edit(&edits, UndoGrouping::NewGroup) {
            Ok(out) => {
                let editor = session.editor.clone();
                editor.update(cx, |editor, cx| apply_own(editor, &edits, window, cx));
                if let Some(out) = out {
                    Self::send(out, cx);
                }
            }
            Err(e) => eprintln!("coqo: cannot reload {}: {e}", self.path),
        }
        self.last_typed = None;
    }

    // ---- closing ----------------------------------------------------------

    fn close(&mut self, cx: &mut App) {
        let Some(session) = self.session.take() else {
            return;
        };
        if let Some(daemon) = Self::daemon(cx) {
            // The request runs on the connection's runtime by itself.
            drop(daemon.read(cx).request::<buffer::Close>(CloseParams {
                buffer: session.replica.buffer(),
                replica: session.replica.replica().clone(),
            }));
        }
    }

    // ---- rendering --------------------------------------------------------

    fn render_notice(&self, notice: &Notice, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let button = |id: &'static str, label: &'static str, cx: &App| {
            raised(div(), cx)
                .id(id)
                .test_support()
                .px(px(12.))
                .py(px(5.))
                .rounded(m.radius_control)
                .text_size(m.text_sm)
                .text_color(p.text.hsla())
                .cursor_pointer()
                .child(label)
        };
        let (color, text) = match notice {
            Notice::SaveConflict => (
                p.sand,
                format!(
                    "{} changed on disk. The change was merged into your unsaved edits.",
                    self.file_name()
                ),
            ),
            Notice::SaveFailed(e) => (p.rose, format!("Couldn't save {}: {e}", self.file_name())),
        };
        let mut bar = div()
            .id("file-notice")
            .test_support()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .mx(px(12.))
            .mt(px(6.))
            .px(px(14.))
            .py(px(8.))
            .rounded(m.radius_field)
            .bg(p.surface.hsla())
            .text_size(m.text_sm)
            .child(icon(Icon::CircleAlert, px(14.), color.hsla()))
            .child(div().flex_1().text_color(p.text.hsla()).child(text));
        if *notice == Notice::SaveConflict {
            bar = bar
                .child(
                    button("file-conflict-reload", "Reload from disk", cx)
                        .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                )
                .child(
                    button("file-conflict-overwrite", "Overwrite", cx)
                        .on_click(cx.listener(|this, _, _, cx| this.overwrite(cx))),
                );
        } else {
            bar = bar.child(
                button("file-notice-dismiss", "Dismiss", cx).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.notice = None;
                        cx.notify();
                    },
                )),
            );
        }
        bar.into_any_element()
    }
}

/// Apply this tab's own undo, redo or reload to the editor, and put the
/// cursor at the end of the change.
fn apply_own(
    editor: &mut CodeEditor,
    edits: &[TextEdit],
    window: &mut Window,
    cx: &mut Context<CodeEditor>,
) {
    editor.apply_remote_edits(&to_editor(edits), window, cx);
    if let Some(last) = post_ranges(edits).last() {
        let cursor = last.end..last.end;
        editor.set_selections(std::slice::from_ref(&cursor), cx);
    }
}

/// `range` → `text` of `old` without the unchanged text at either end.
fn shrink(old: &Rope, range: Range<usize>, text: &str) -> (Range<usize>, String) {
    if range.is_empty() || text.is_empty() || range.end > old.len() {
        return (range, text.to_owned());
    }
    let replaced = old.slice(range.clone()).to_string();
    let (a, b) = (replaced.as_bytes(), text.as_bytes());
    let mut prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    while !(replaced.is_char_boundary(prefix) && text.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let max_suffix = a.len().min(b.len()) - prefix;
    let mut suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    while !(replaced.is_char_boundary(a.len() - suffix) && text.is_char_boundary(b.len() - suffix))
    {
        suffix -= 1;
    }
    (
        range.start + prefix..range.end - suffix,
        text[prefix..b.len() - suffix].to_owned(),
    )
}

fn to_editor(edits: &[TextEdit]) -> Vec<editor::TextEdit> {
    edits
        .iter()
        .map(|e| (e.start..e.end, e.text.clone()))
        .collect()
}

/// Where each edit's new text ends up (`edits` sorted, in pre-edit
/// coordinates).
fn post_ranges(edits: &[TextEdit]) -> Vec<Range<usize>> {
    let (mut inserted, mut removed) = (0usize, 0usize);
    edits
        .iter()
        .map(|e| {
            let start = e.start + inserted - removed;
            inserted += e.text.len();
            removed += e.end - e.start;
            start..start + e.text.len()
        })
        .collect()
}

/// The tint for a change made outside this editor, if it gets one.
fn tint(change: &BufferChange, cx: &App) -> Option<Rgb> {
    let p = &cx.tokens().palette;
    if change.conflict {
        return Some(p.sand);
    }
    match change.origin.kind {
        OriginKind::Agent => Some(p.sage),
        OriginKind::Disk => Some(p.peach),
        _ => None,
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024. && unit < UNITS.len() - 1 {
        size /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

impl TabItem for FileEditor {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        self.file_name().to_owned().into()
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(key(&self.workspace, self.root, &self.path).into())
    }

    fn save(&self, _: &App) -> Value {
        json!({ "workspace": self.workspace.0, "root": self.root.0, "path": self.path })
    }

    fn render_label(&self, _: bool, _: &mut Window, cx: &App) -> Option<AnyElement> {
        let p = &cx.tokens().palette;
        let mut label = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(super::glyph::render(self.file_name(), cx))
            .child(self.file_name().to_owned());
        if self.is_dirty() {
            label = label.child(
                div()
                    .id("file-tab-dirty")
                    .test_support()
                    .size(px(7.))
                    .flex_none()
                    .rounded_full()
                    .bg(p.text_muted.hsla()),
            );
        }
        Some(label.into_any_element())
    }

    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match &self.session {
            Some(session) => Some(session.editor.read(cx).focus_handle(cx)),
            None => Some(self.focus.clone()),
        }
    }
}

impl Render for FileEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let message = |title: String, detail: String| {
            div()
                .id("file-message")
                .test_support()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(icon(Icon::File, px(24.), p.text_subtle.hsla()))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    div()
                        .text_size(m.text_sm)
                        .text_color(p.text_muted.hsla())
                        .child(detail),
                )
                .into_any_element()
        };
        let name = self.file_name().to_owned();
        let body = match (&self.content, &self.session) {
            (Content::Text, Some(session)) => div()
                .id("file-editor-text")
                .test_support()
                .flex_1()
                .min_h_0()
                .pt(px(6.))
                .child(session.editor.clone())
                .into_any_element(),
            (Content::Text, None) | (Content::Loading, _) => {
                message("Loading…".into(), self.path.clone())
            }
            (Content::Binary { size }, _) => {
                let size = if *size > 0 {
                    format!(" ({})", human_size(*size))
                } else {
                    String::new()
                };
                message(
                    "Binary file".into(),
                    format!("{name} isn't UTF-8 text{size}, so it can't be shown."),
                )
            }
            (Content::TooLarge { size, limit }, _) => message(
                "File too large to open".into(),
                format!(
                    "{name} is {}; the editor opens files up to {}.",
                    human_size(*size),
                    human_size(*limit)
                ),
            ),
            (Content::Missing, _) => message(
                "File not found".into(),
                format!("{} was moved or deleted.", self.path),
            ),
            (Content::Error(e), _) => message("Couldn't open the file".into(), e.clone()),
        };
        let notice = self
            .notice
            .clone()
            .map(|notice| self.render_notice(&notice, cx));
        div()
            .id("file-editor")
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Save, _, cx| this.save(false, cx)))
            .size_full()
            .flex()
            .flex_col()
            .children(notice)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_ranges_follow_earlier_edits() {
        let edits = [
            TextEdit::new(0..2, "abcd"),
            TextEdit::delete(5..8),
            TextEdit::insert(10, "xy"),
        ];
        // +2 after the first edit, -3 after the second.
        assert_eq!(post_ranges(&edits), vec![0..4, 7..7, 9..11]);
    }

    #[test]
    fn shrink_keeps_only_what_changed() {
        let old = Rope::from_str("fn main() {}\n");
        // A whole-text replacement that only appended "你".
        assert_eq!(
            shrink(&old, 0..13, "fn main() {}\n你"),
            (13..13, "你".to_owned())
        );
        // A replacement in the middle.
        assert_eq!(
            shrink(&old, 0..13, "fn mine() {}\n"),
            (4..7, "ine".to_owned())
        );
        // Multi-byte characters stay whole.
        let old = Rope::from_str("aéb");
        assert_eq!(shrink(&old, 0..4, "aèb"), (1..3, "è".to_owned()));
        // Inserts and deletions are left alone.
        assert_eq!(shrink(&old, 1..1, "x"), (1..1, "x".to_owned()));
        assert_eq!(shrink(&old, 1..3, ""), (1..3, String::new()));
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(human_size(12), "12 bytes");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
