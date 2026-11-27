// SPDX-License-Identifier: Apache-2.0

//! Comment threads in the diff view (board 4): a [`DiffOverlay`] over a
//! workspace's [`WorkspaceThreads`].
//!
//! - **Where a thread shows.** A thread is pinned to a file *version*; the
//!   diff shows it on the side that shows that version (the new side of
//!   unstaged changes is the work tree, their old side the index), under
//!   its last line where that line is now. A thread whose code is gone
//!   (orphaned), whose line the diff does not have, or that is on the whole
//!   file shows under the file's header.
//! - **Starting one.** The `+` beside the hovered line, or at the end of a
//!   selection (drag the line numbers), opens a composer under the line.
//!   "Add to review" (or `Enter`) starts the thread with a *pending*
//!   comment.
//! - **A thread's card.** Its comments (the user's pending ones marked
//!   "Pending", with Edit), a reply field (`Enter` adds a pending reply),
//!   Resolve / Reopen, and Discard for the user's pending comments. A
//!   resolved thread folds to one line; "Show" opens it.
//! - **The review.** The header counts the pending comments of threads
//!   started in this diff and holds "Send N comments to Claude", disabled
//!   until #95 sends them; each file row shows its pending count; a note
//!   under the file list says what the pending review is.
//! - **Gutter.** A marker on each thread's first line (sand while it has
//!   pending comments, sage once resolved, blue otherwise); pending threads'
//!   lines are tinted sand.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use coqo_protocol::methods::{
    git::FileDiff,
    threads::{
        Author, Comment, CommentId, CreateParams, Thread, ThreadId, ThreadLines, ThreadSide,
        ThreadStatus,
    },
};
use coqo_ui::{
    ActiveTokens as _,
    diff::{GutterMarker, LineRange, LineRef, LineTint, Side, Slot, SlotTarget},
    gpui::Focusable as _,
    gpui::{
        AnyElement, App, AppContext as _, Context, ElementId, Entity, FontWeight,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
    },
    input::{PromptInput, PromptInputEvent},
    text::markdown,
    widgets::{FluentBuilder as _, Icon, TestSupportExt as _, icon, primary, raised},
};

use super::{WorkspaceThreads, threads_for};
use crate::{
    chat::list::ChatList,
    diff::{DiffOverlay, DiffSource, DiffView, DiffViewEvent, source::SourceVersions},
};

/// The tooltip of the disabled send button.
pub const SEND_LATER: &str = "Sending to the agent comes in #95";

/// What a slot of this overlay holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotKind {
    Thread(ThreadId),
    Composer,
}

/// The threads in a diff view. See the module docs.
pub struct ThreadsOverlay {
    state: Entity<ReviewState>,
    /// What each slot of the last render holds.
    slots: RefCell<HashMap<ElementId, SlotKind>>,
}

impl ThreadsOverlay {
    /// Show `view`'s threads in it, if its source compares versions of a
    /// root's files and a daemon is there. Returns the overlay's state.
    pub fn attach(
        view: &Entity<DiffView>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<ReviewState>> {
        let source = view.read(cx).source().clone();
        let versions = source.versions(cx)?;
        let store = threads_for(&versions.workspace, cx)?;
        Some(Self::attach_to(view, store, window, cx))
    }

    /// Show `store`'s threads in `view`.
    pub fn attach_to(
        view: &Entity<DiffView>,
        store: Entity<WorkspaceThreads>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ReviewState> {
        let state = cx.new(|cx| ReviewState::new(store, view, window, cx));
        let overlay = Rc::new(Self {
            state: state.clone(),
            slots: RefCell::default(),
        });
        let observed = state.clone();
        view.update(cx, |v, cx| {
            v.add_overlay(overlay, cx);
            cx.observe(&observed, |_, _, cx| cx.notify()).detach();
        });
        state
    }
}

/// The composer of a new thread.
struct Composer {
    range: LineRange,
    input: Entity<PromptInput>,
    _events: Subscription,
}

/// A thread placed in the shown file.
#[derive(Clone, Debug)]
struct Placed {
    thread: Thread,
    side: Side,
    /// Its lines where the diff has them; `None` shows it on the file.
    lines: Option<ThreadLines>,
}

/// The overlay's state: the composer, the reply fields, what is open.
pub struct ReviewState {
    store: Entity<WorkspaceThreads>,
    source: Rc<dyn DiffSource>,
    composer: Option<Composer>,
    replies: HashMap<ThreadId, (Entity<PromptInput>, Subscription)>,
    editing: Option<(ThreadId, CommentId, Entity<PromptInput>, Subscription)>,
    /// Resolved threads shown open.
    expanded: HashSet<ThreadId>,
    error: Option<(Option<ThreadId>, String)>,
    sending: bool,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ReviewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewState")
            .field("composer", &self.composer.as_ref().map(|c| &c.range))
            .field("replies", &self.replies.len())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl ReviewState {
    fn new(
        store: Entity<WorkspaceThreads>,
        view: &Entity<DiffView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = view.read(cx).source().clone();
        let subscriptions = vec![
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.sync_inputs(window, cx);
                cx.notify();
            }),
            // The files changed: list again for where the anchors are now.
            cx.subscribe(view, |this, _, event: &DiffViewEvent, cx| {
                if *event == DiffViewEvent::Loaded {
                    this.store.update(cx, |s, cx| s.relist(cx));
                }
            }),
        ];
        let mut this = Self {
            store,
            source,
            composer: None,
            replies: HashMap::new(),
            editing: None,
            expanded: HashSet::new(),
            error: None,
            sending: false,
            _subscriptions: subscriptions,
        };
        this.sync_inputs(window, cx);
        this
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn store(&self) -> &Entity<WorkspaceThreads> {
        &self.store
    }

    /// The new thread's text field, while the composer is open.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn composer_input(&self) -> Option<&Entity<PromptInput>> {
        self.composer.as_ref().map(|c| &c.input)
    }

    /// A thread's reply field.
    pub fn reply_input(&self, id: ThreadId) -> Option<&Entity<PromptInput>> {
        self.replies.get(&id).map(|(input, _)| input)
    }

    /// The field of the comment being edited.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn edit_input(&self) -> Option<&Entity<PromptInput>> {
        self.editing.as_ref().map(|(_, _, input, _)| input)
    }

    /// The last failure, as shown.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn error(&self) -> Option<&str> {
        self.error.as_ref().map(|(_, e)| e.as_str())
    }

    fn versions(&self, cx: &App) -> Option<SourceVersions> {
        self.source.versions(cx)
    }

    /// Pending comments of the threads started in this diff.
    pub fn pending_count(&self, cx: &App) -> usize {
        let identity = self.source.identity();
        self.store.read(cx).pending_count(Some(&identity))
    }

    /// Pending comments of this diff's threads in the diff path `path`.
    fn pending_in(&self, path: &str, cx: &App) -> usize {
        let (Some(v), identity) = (self.versions(cx), self.source.identity()) else {
            return 0;
        };
        self.store
            .read(cx)
            .all()
            .into_iter()
            .filter(|t| t.diff.as_deref() == Some(identity.as_str()))
            .filter(|t| t.anchor.root == v.root && v.diff_path(&t.anchor.path) == path)
            .map(|t| t.pending().count())
            .sum()
    }

    /// The threads shown in the diff path `path` of `diff`, oldest first.
    fn placed(&self, path: &str, diff: &FileDiff, cx: &App) -> Vec<Placed> {
        let Some(v) = self.versions(cx) else {
            return Vec::new();
        };
        self.store
            .read(cx)
            .all()
            .into_iter()
            .filter_map(|t| place(t, &v, path, diff))
            .collect()
    }

    /// Keep a reply field for each thread, and drop those of threads gone.
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<ThreadId> = self.store.read(cx).all().iter().map(|t| t.id).collect();
        self.replies.retain(|id, _| ids.contains(id));
        for id in ids {
            if self.replies.contains_key(&id) {
                continue;
            }
            let input = cx.new(|cx| PromptInput::new("Add to this thread", 1, 6, window, cx));
            let events = cx.subscribe_in(
                &input,
                window,
                move |this, _, event: &PromptInputEvent, window, cx| match event {
                    PromptInputEvent::Submit => this.send_reply(id, window, cx),
                    PromptInputEvent::Changed
                        if this.error.as_ref().is_some_and(|(t, _)| *t == Some(id)) =>
                    {
                        this.error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            );
            self.replies.insert(id, (input, events));
        }
        if self
            .editing
            .as_ref()
            .is_some_and(|(t, ..)| self.store.read(cx).get(*t).is_none())
        {
            self.editing = None;
        }
    }

    // ---- actions --------------------------------------------------------------

    /// Open the composer for `range` (diff coordinates) and focus it.
    pub fn open_composer(
        &mut self,
        range: &LineRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| PromptInput::new("Leave a comment", 2, 8, window, cx));
        let events = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &PromptInputEvent, window, cx| match event {
                PromptInputEvent::Submit => this.submit_composer(window, cx),
                PromptInputEvent::Cancel => this.close_composer(cx),
                _ => {}
            },
        );
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.composer = Some(Composer {
            range: range.clone(),
            input,
            _events: events,
        });
        self.error = None;
        cx.notify();
    }

    pub fn close_composer(&mut self, cx: &mut Context<Self>) {
        if self.composer.take().is_some() {
            cx.notify();
        }
    }

    /// Start the composer's thread, with its text as a pending comment.
    pub fn submit_composer(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else {
            return;
        };
        let body = composer.input.read(cx).text(cx);
        if body.trim().is_empty() || self.sending {
            return;
        }
        let Some(params) = self.create_params(&composer.range, body, cx) else {
            self.error = Some((None, "This file is outside the workspace root".into()));
            cx.notify();
            return;
        };
        self.sending = true;
        let task = super::create(&self.store, params, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.sending = false;
                match result {
                    Ok(_) => this.composer = None,
                    Err(e) => this.error = Some((None, e.to_string())),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn create_params(&self, range: &LineRange, body: String, cx: &App) -> Option<CreateParams> {
        let v = self.versions(cx)?;
        let path = v.root_path(&range.path)?;
        // A unified selection can run from an old line to a new one; a
        // thread is on one version, the end's.
        let side = range.end.0;
        let start = if range.start.0 == side {
            range.start.1.min(range.end.1)
        } else {
            range.end.1
        };
        let end = range.end.1.max(start);
        let (side, version) = match side {
            Side::Old => (ThreadSide::Old, v.base.clone()),
            Side::New => (ThreadSide::New, v.head.clone()),
        };
        Some(CreateParams {
            workspace: v.workspace,
            root: v.root,
            path,
            side,
            version,
            lines: Some(ThreadLines { start, end }),
            diff: Some(self.source.identity()),
            body,
            pending: true,
        })
    }

    /// Add the reply field's text to thread `id`, pending.
    pub fn send_reply(&mut self, id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = self.replies.get(&id) else {
            return;
        };
        let input = input.clone();
        let body = input.read(cx).text(cx);
        if body.trim().is_empty() || self.sending {
            return;
        }
        self.sending = true;
        let task = super::reply(&self.store, id, body, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(_) => input.update(cx, |i, cx| i.set_text("", window, cx)),
                    Err(e) => this.error = Some((Some(id), e.to_string())),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Edit comment `comment` of thread `id` in place.
    pub fn start_edit(
        &mut self,
        id: ThreadId,
        comment: CommentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(body) = self
            .store
            .read(cx)
            .get(id)
            .and_then(|t| t.comments.iter().find(|c| c.id == comment))
            .map(|c| c.body.clone())
        else {
            return;
        };
        let input = cx.new(|cx| PromptInput::new("Edit your comment", 2, 8, window, cx));
        input.update(cx, |i, cx| i.set_text(body, window, cx));
        let events = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &PromptInputEvent, window, cx| match event {
                PromptInputEvent::Submit => this.save_edit(window, cx),
                PromptInputEvent::Cancel => {
                    this.editing = None;
                    cx.notify();
                }
                _ => {}
            },
        );
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.editing = Some((id, comment, input, events));
        cx.notify();
    }

    pub fn save_edit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let Some((id, comment, input, _)) = &self.editing else {
            return;
        };
        let (id, comment) = (*id, *comment);
        let body = input.read(cx).text(cx);
        if body.trim().is_empty() || self.sending {
            return;
        }
        self.sending = true;
        let task = super::edit(&self.store, comment, body, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.sending = false;
                match result {
                    Ok(_) => this.editing = None,
                    Err(e) => this.error = Some((Some(id), e.to_string())),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Resolve thread `id`, or reopen it.
    pub fn toggle_resolved(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        let Some(resolved) = self
            .store
            .read(cx)
            .get(id)
            .map(|t| t.status == ThreadStatus::Resolved)
        else {
            return;
        };
        let task = super::set_resolved(&self.store, id, !resolved, cx);
        self.expanded.remove(&id);
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                this.update(cx, |this, cx| {
                    this.error = Some((Some(id), e.to_string()));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// Delete the user's pending comments of thread `id` (the thread too,
    /// if nothing else is left).
    pub fn discard_pending(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        let Some(ids) = self.store.read(cx).get(id).map(|t| {
            t.comments
                .iter()
                .filter(|c| c.pending && c.author == Author::User)
                .map(|c| c.id)
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        let task = super::delete_comments(&self.store, id, ids, cx);
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                this.update(cx, |this, cx| {
                    this.error = Some((Some(id), e.to_string()));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Discard this diff's pending review: the user's pending comments,
    /// and the threads they leave empty.
    pub fn discard_review(&mut self, cx: &mut Context<Self>) {
        let task = super::discard(&self.store, Some(self.source.identity()), cx);
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                this.update(cx, |this, cx| {
                    this.error = Some((None, e.to_string()));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Show a resolved thread open, or folded again.
    pub fn toggle_expanded(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }
}

/// Where thread `t` shows in the diff path `path` of `diff`, if it does.
fn place(t: &Thread, v: &SourceVersions, path: &str, diff: &FileDiff) -> Option<Placed> {
    let a = &t.anchor;
    if a.root != v.root || v.diff_path(&a.path) != path {
        return None;
    }
    let (old, new) = (a.version == v.base, a.version == v.head);
    let side = match a.side {
        ThreadSide::New if new => Side::New,
        ThreadSide::Old if old => Side::Old,
        _ if new => Side::New,
        _ if old => Side::Old,
        _ => return None,
    };
    let lines = a
        .lines
        .filter(|_| !a.is_orphaned())
        .filter(|l| has_line(diff, side, l.end));
    Some(Placed {
        thread: t.clone(),
        side,
        lines,
    })
}

/// Whether `diff` has line `line` (1-based) of `side`.
fn has_line(diff: &FileDiff, side: Side, line: u32) -> bool {
    diff.hunks.iter().any(|h| {
        h.lines.iter().any(|l| match side {
            Side::Old => l.old_line == Some(line),
            Side::New => l.new_line == Some(line),
        })
    })
}

/// "line 19", "lines 3–4", "file".
fn lines_label(lines: Option<ThreadLines>) -> String {
    match lines {
        Some(l) if l.start == l.end => format!("line {}", l.start),
        Some(l) => format!("lines {}–{}", l.start, l.end),
        None => "file".to_owned(),
    }
}

/// The first line of a comment, cut short.
fn first_line(body: &str) -> String {
    const MAX: usize = 60;
    let line = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = line.chars().take(MAX).collect();
    if line.chars().count() > MAX {
        out.push('…');
    }
    out
}

/// Who wrote a comment, as shown: the avatar's letter and the name.
fn author_label(author: &Author, cx: &App) -> (String, String) {
    match author {
        Author::User => ("Y".into(), "You".into()),
        Author::Agent { session } => {
            let name = ChatList::global(cx)
                .and_then(|l| l.read(cx).info(session).map(|i| i.provider.clone()))
                .map(|p| {
                    let mut c = p.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                })
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Agent".into());
            (name.chars().next().unwrap_or('A').to_string(), name)
        }
        Author::Github { login } => (
            login.chars().next().unwrap_or('?').to_uppercase().collect(),
            login.clone(),
        ),
    }
}

/// A small flat button.
fn flat(
    id: impl Into<ElementId>,
    label: &'static str,
    cx: &App,
) -> coqo_ui::gpui::Stateful<coqo_ui::gpui::Div> {
    let p = &cx.tokens().palette;
    div()
        .id(id)
        .flex_none()
        .px(px(10.))
        .py(px(5.))
        .rounded(px(9.))
        .border_1()
        .border_color(p.border.hsla())
        .text_size(px(12.))
        .text_color(p.text_muted.hsla())
        .cursor_pointer()
        .hover(|d| d.bg(p.surface_strong.hsla()))
        .child(label)
}

/// A small raised button.
fn raised_button(
    id: impl Into<ElementId>,
    label: &'static str,
    cx: &App,
) -> coqo_ui::gpui::Stateful<coqo_ui::gpui::Div> {
    let p = &cx.tokens().palette;
    raised(div(), cx)
        .id(id)
        .flex_none()
        .px(px(12.))
        .py(px(5.))
        .rounded(px(9.))
        .text_size(px(12.))
        .text_color(p.text.hsla())
        .cursor_pointer()
        .child(label)
}

impl ThreadsOverlay {
    fn render_comment(
        &self,
        t: &Thread,
        c: &Comment,
        first: bool,
        lines: Option<ThreadLines>,
        state: &ReviewState,
        cx: &App,
    ) -> AnyElement {
        let tk = cx.tokens();
        let (p, m) = (&tk.palette, &tk.metrics);
        let (letter, name) = author_label(&c.author, cx);
        let avatar_color = match c.author {
            Author::User => p.sage,
            Author::Agent { .. } => p.lavender,
            Author::Github { .. } => p.blue,
        };
        let editing = state
            .editing
            .as_ref()
            .filter(|(_, id, ..)| *id == c.id)
            .map(|(_, _, input, _)| input.clone());
        let (thread, comment) = (t.id, c.id);
        let weak = self.state.downgrade();
        let edit = (c.author == Author::User && editing.is_none()).then(|| {
            div()
                .id(("thread-edit", c.id.0 as usize))
                .test_support()
                .text_size(px(11.))
                .text_color(p.text_subtle.hsla())
                .cursor_pointer()
                .hover(|d| d.text_color(p.text.hsla()))
                .child("Edit")
                .on_click(move |_, window, cx| {
                    weak.update(cx, |s, cx| s.start_edit(thread, comment, window, cx))
                        .ok();
                })
        });
        let body = match editing {
            Some(input) => {
                let save = self.state.downgrade();
                let cancel = self.state.downgrade();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .rounded(px(10.))
                            .border_1()
                            .border_color(p.border.hsla())
                            .bg(p.panel.hsla())
                            .px(px(10.))
                            .py(px(6.))
                            .child(input),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                raised_button("thread-edit-save", "Save", cx)
                                    .test_support()
                                    .on_click(move |_, window, cx| {
                                        save.update(cx, |s, cx| s.save_edit(window, cx)).ok();
                                    }),
                            )
                            .child(
                                flat("thread-edit-cancel", "Cancel", cx)
                                    .test_support()
                                    .on_click(move |_, _, cx| {
                                        cancel
                                            .update(cx, |s, cx| {
                                                s.editing = None;
                                                cx.notify();
                                            })
                                            .ok();
                                    }),
                            ),
                    )
                    .into_any_element()
            }
            None => div()
                .text_size(px(12.5))
                .text_color(p.text.hsla())
                .child(markdown(
                    ("thread-comment", c.id.0 as usize),
                    c.body.clone(),
                ))
                .into_any_element(),
        };
        div()
            .id(("thread-comment-card", c.id.0 as usize))
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(14.))
            .py(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .size(px(22.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.))
                            .bg(avatar_color.alpha(0.18))
                            .text_color(avatar_color.hsla())
                            .text_size(px(11.))
                            .font_weight(FontWeight::BOLD)
                            .child(letter),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child(name),
                    )
                    .when(first, |d| {
                        d.child(
                            div()
                                .text_color(p.text_subtle.hsla())
                                .child(lines_label(lines)),
                        )
                    })
                    .when(c.edited_at.is_some(), |d| {
                        d.child(div().text_color(p.text_subtle.hsla()).child("edited"))
                    })
                    .child(div().flex_1())
                    .children(edit)
                    .when(c.pending, |d| {
                        d.child(
                            div()
                                .id(("thread-pending", c.id.0 as usize))
                                .test_support()
                                .px(px(8.))
                                .py(px(2.))
                                .rounded(px(7.))
                                .bg(p.sand.hsla())
                                .text_color(p.ground.hsla())
                                .text_size(m.text_label)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("PENDING"),
                        )
                    }),
            )
            .child(body)
            .into_any_element()
    }

    fn render_thread(&self, placed: &Placed, cx: &App) -> AnyElement {
        let state = self.state.read(cx);
        let t = &placed.thread;
        let tk = cx.tokens();
        let (p, m) = (&tk.palette, &tk.metrics);
        let id = t.id;
        let ix = id.0 as usize;
        let resolved = t.status == ThreadStatus::Resolved;
        if resolved && !state.expanded.contains(&id) {
            let first = t
                .comments
                .first()
                .map(|c| first_line(&c.body))
                .unwrap_or_default();
            let last = t
                .comments
                .last()
                .filter(|_| t.comments.len() > 1)
                .map(|c| format!(" · {}", first_line(&c.body)))
                .unwrap_or_default();
            let weak = self.state.downgrade();
            return div()
                .id(("thread", ix))
                .test_support()
                .mx(px(14.))
                .my(px(6.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(12.))
                .bg(p.surface.hsla())
                .flex()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.5))
                .text_color(p.text_muted.hsla())
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(p.sage.hsla())
                        .child("✓"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(format!("Resolved · \"{first}\"{last}")),
                )
                .child(
                    flat(("thread-show", ix), "Show", cx)
                        .test_support()
                        .on_click(move |_, _, cx| {
                            weak.update(cx, |s, cx| s.toggle_expanded(id, cx)).ok();
                        }),
                )
                .into_any_element();
        }
        let comments: Vec<AnyElement> = t
            .comments
            .iter()
            .enumerate()
            .map(|(i, c)| self.render_comment(t, c, i == 0, t.anchor.lines, state, cx))
            .collect();
        let outdated = placed.lines.is_none() && t.anchor.code.is_some();
        let snippet = t.anchor.code.as_ref().filter(|_| outdated).map(|code| {
            let text: String = code.text.lines().take(4).collect::<Vec<_>>().join("\n");
            div()
                .px(px(14.))
                .pt(px(10.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(m.text_label)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.sand.hsla())
                        .child(if t.anchor.is_orphaned() {
                            "OUTDATED · the code it was on changed"
                        } else {
                            "Not in this diff"
                        }),
                )
                .child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .bg(p.panel.hsla())
                        .font_family(m.font_mono.clone())
                        .text_size(px(12.))
                        .text_color(p.text_muted.hsla())
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(text),
                )
        });
        let has_pending = t
            .comments
            .iter()
            .any(|c| c.pending && c.author == Author::User);
        let error = state
            .error
            .as_ref()
            .filter(|(t, _)| *t == Some(id))
            .map(|(_, e)| e.clone());
        let input = state.reply_input(id).cloned();
        let (reply, resolve, discard, hide) = (
            self.state.downgrade(),
            self.state.downgrade(),
            self.state.downgrade(),
            self.state.downgrade(),
        );
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .py(px(10.))
            .bg(p.panel_raised.hsla())
            .children(input.map(|input| {
                div()
                    .flex_1()
                    .min_w(px(200.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(p.border.hsla())
                    .bg(p.panel.hsla())
                    .px(px(10.))
                    .py(px(6.))
                    .child(input)
            }))
            .child(
                raised_button(("thread-reply", ix), "Reply", cx)
                    .test_support()
                    .on_click(move |_, window, cx| {
                        reply.update(cx, |s, cx| s.send_reply(id, window, cx)).ok();
                    }),
            )
            .child(
                flat(
                    ("thread-resolve", ix),
                    if resolved { "Reopen" } else { "Resolve" },
                    cx,
                )
                .test_support()
                .on_click(move |_, _, cx| {
                    resolve.update(cx, |s, cx| s.toggle_resolved(id, cx)).ok();
                }),
            )
            .when(has_pending, |d| {
                d.child(
                    flat(("thread-discard", ix), "Discard", cx)
                        .test_support()
                        .on_click(move |_, _, cx| {
                            discard.update(cx, |s, cx| s.discard_pending(id, cx)).ok();
                        }),
                )
            })
            .when(resolved, |d| {
                d.child(
                    flat(("thread-hide", ix), "Hide", cx)
                        .test_support()
                        .on_click(move |_, _, cx| {
                            hide.update(cx, |s, cx| s.toggle_expanded(id, cx)).ok();
                        }),
                )
            });
        div()
            .id(("thread", ix))
            .test_support()
            .mx(px(14.))
            .my(px(8.))
            .flex()
            .flex_col()
            .rounded(px(16.))
            .bg(p.surface.hsla())
            .border_1()
            .border_color(if has_pending {
                p.sand.alpha(0.5)
            } else {
                p.border_strong.hsla()
            })
            .overflow_hidden()
            .children(snippet)
            .children(comments)
            .children(error.map(|e| {
                div()
                    .px(px(14.))
                    .pb(px(8.))
                    .text_size(px(12.))
                    .text_color(p.rose.hsla())
                    .child(e)
            }))
            .child(footer)
            .into_any_element()
    }

    fn render_composer(&self, cx: &App) -> AnyElement {
        let state = self.state.read(cx);
        let Some(composer) = &state.composer else {
            return div().into_any_element();
        };
        let tk = cx.tokens();
        let p = &tk.palette;
        let r = &composer.range;
        let start = if r.start.0 == r.end.0 {
            r.start.1.min(r.end.1)
        } else {
            r.end.1
        };
        let lines = Some(ThreadLines {
            start,
            end: r.end.1.max(start),
        });
        let error = state
            .error
            .as_ref()
            .filter(|(t, _)| t.is_none())
            .map(|(_, e)| e.clone());
        let (add, cancel) = (self.state.downgrade(), self.state.downgrade());
        div()
            .id("thread-composer")
            .test_support()
            .mx(px(14.))
            .my(px(8.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(16.))
            .bg(p.surface.hsla())
            .border_1()
            .border_color(p.border_strong.hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .size(px(22.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.))
                            .bg(p.sage.alpha(0.18))
                            .text_color(p.sage.hsla())
                            .text_size(px(11.))
                            .font_weight(FontWeight::BOLD)
                            .child("Y"),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text.hsla())
                            .child("You"),
                    )
                    .child(
                        div()
                            .text_color(p.text_subtle.hsla())
                            .child(lines_label(lines)),
                    ),
            )
            .child(
                div()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(p.border.hsla())
                    .bg(p.panel.hsla())
                    .px(px(10.))
                    .py(px(6.))
                    .child(composer.input.clone()),
            )
            .children(error.map(|e| div().text_size(px(12.)).text_color(p.rose.hsla()).child(e)))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        raised_button("thread-composer-add", "Add to review", cx)
                            .test_support()
                            .on_click(move |_, window, cx| {
                                add.update(cx, |s, cx| s.submit_composer(window, cx)).ok();
                            }),
                    )
                    .child(
                        flat("thread-composer-cancel", "Cancel", cx)
                            .test_support()
                            .on_click(move |_, _, cx| {
                                cancel.update(cx, |s, cx| s.close_composer(cx)).ok();
                            }),
                    ),
            )
            .into_any_element()
    }
}

/// A plain tooltip.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        coqo_ui::widgets::popover(cx)
            .px(px(10.))
            .py(px(6.))
            .text_size(t.metrics.text_xs)
            .text_color(t.palette.text.hsla())
            .child(self.0.clone())
    }
}

impl DiffOverlay for ThreadsOverlay {
    fn markers(&self, path: &str, diff: &FileDiff, cx: &App) -> Vec<GutterMarker> {
        self.state
            .read(cx)
            .placed(path, diff, cx)
            .into_iter()
            .filter_map(|pl| {
                let lines = pl.lines?;
                Some(GutterMarker {
                    id: ("thread-marker", pl.thread.id.0 as usize).into(),
                    at: LineRef::new(path.to_owned(), pl.side, lines.start),
                    label: "Comment thread".into(),
                })
            })
            .collect()
    }

    fn render_marker(&self, marker: &GutterMarker, _: &mut Window, cx: &mut App) -> AnyElement {
        let state = self.state.read(cx);
        let p = cx.tokens().palette.clone();
        let id = match &marker.id {
            ElementId::NamedInteger(_, n) => ThreadId(*n),
            _ => return div().into_any_element(),
        };
        let Some(t) = state.store.read(cx).get(id) else {
            return div().into_any_element();
        };
        let color = if t.pending().next().is_some() {
            p.sand
        } else if t.status == ThreadStatus::Resolved {
            p.sage
        } else {
            p.blue
        };
        let weak = self.state.downgrade();
        let resolved = t.status == ThreadStatus::Resolved;
        div()
            .id(marker.id.clone())
            .test_support()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|d| d.bg(p.surface_strong.hsla()))
            .child(icon(Icon::MessageCircle, px(12.), color.hsla()))
            .on_click(move |_, window, cx| {
                weak.update(cx, |s, cx| {
                    if resolved {
                        s.toggle_expanded(id, cx);
                    } else if let Some(input) = s.reply_input(id) {
                        let handle = input.read(cx).focus_handle(cx);
                        window.focus(&handle, cx);
                    }
                })
                .ok();
            })
            .into_any_element()
    }

    fn slots(&self, path: &str, diff: &FileDiff, cx: &App) -> Vec<Slot> {
        let state = self.state.read(cx);
        let mut kinds = self.slots.borrow_mut();
        kinds.clear();
        let mut slots: Vec<Slot> = state
            .placed(path, diff, cx)
            .into_iter()
            .map(|pl| {
                let id: ElementId = ("thread-slot", pl.thread.id.0 as usize).into();
                kinds.insert(id.clone(), SlotKind::Thread(pl.thread.id));
                Slot {
                    id,
                    at: match pl.lines {
                        Some(l) => SlotTarget::Line(LineRef::new(path.to_owned(), pl.side, l.end)),
                        None => SlotTarget::File(path.to_owned().into()),
                    },
                }
            })
            .collect();
        if let Some(c) = state.composer.as_ref().filter(|c| c.range.path == path) {
            let id: ElementId = "thread-composer-slot".into();
            kinds.insert(id.clone(), SlotKind::Composer);
            slots.push(Slot {
                id,
                at: SlotTarget::Line(c.range.last()),
            });
        }
        slots
    }

    fn render_slot(&self, slot: &Slot, _: &mut Window, cx: &mut App) -> AnyElement {
        let kind = self.slots.borrow().get(&slot.id).copied();
        match kind {
            Some(SlotKind::Composer) => self.render_composer(cx),
            Some(SlotKind::Thread(id)) => {
                let state = self.state.read(cx);
                let Some(v) = state.versions(cx) else {
                    return div().into_any_element();
                };
                let placed = state.store.read(cx).get(id).map(|t| {
                    let lines = match &slot.at {
                        SlotTarget::Line(_) => t.anchor.lines,
                        SlotTarget::File(_) => None,
                    };
                    let side = match &slot.at {
                        SlotTarget::Line(at) => at.side,
                        SlotTarget::File(_) => {
                            if t.anchor.version == v.base && t.anchor.version != v.head {
                                Side::Old
                            } else {
                                Side::New
                            }
                        }
                    };
                    Placed {
                        thread: t.clone(),
                        side,
                        lines,
                    }
                });
                match placed {
                    Some(placed) => self.render_thread(&placed, cx),
                    None => div().into_any_element(),
                }
            }
            None => div().into_any_element(),
        }
    }

    fn tints(&self, path: &str, diff: &FileDiff, cx: &App) -> Vec<LineTint> {
        let sand = cx.tokens().palette.sand.alpha(0.10);
        self.state
            .read(cx)
            .placed(path, diff, cx)
            .into_iter()
            .filter(|pl| pl.thread.pending().next().is_some())
            .filter_map(|pl| {
                let l = pl.lines?;
                Some(LineTint {
                    range: LineRange::lines(path.to_owned(), pl.side, l.start, l.end),
                    color: sand,
                })
            })
            .collect()
    }

    fn wants_add(&self) -> bool {
        true
    }

    fn on_add(&self, range: &LineRange, window: &mut Window, cx: &mut App) {
        self.state
            .update(cx, |s, cx| s.open_composer(range, window, cx));
    }

    fn header(&self, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let n = self.state.read(cx).pending_count(cx);
        if n == 0 {
            return None;
        }
        let p = cx.tokens().palette.clone();
        let comments = if n == 1 { "comment" } else { "comments" };
        let discard = self.state.downgrade();
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .id("review-pending-count")
                        .test_support()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.sand.hsla())
                        .child(format!("{n} pending")),
                )
                .child(
                    flat("review-discard", "Discard", cx)
                        .test_support()
                        .on_click(move |_, _, cx| {
                            discard.update(cx, |s, cx| s.discard_review(cx)).ok();
                        }),
                )
                .child(
                    primary(div(), cx)
                        .id("review-send")
                        .test_support()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(16.))
                        .py(px(8.))
                        .rounded(px(10.))
                        .text_size(px(12.5))
                        .opacity(0.5)
                        .child(format!("Send {n} {comments} to Claude"))
                        .child(icon(Icon::ArrowUp, px(14.), p.ground.hsla()))
                        .tooltip(|_, cx| cx.new(|_| Tip(SEND_LATER.into())).into()),
                )
                .into_any_element(),
        )
    }

    fn file_badge(&self, path: &str, cx: &App) -> Option<AnyElement> {
        let n = self.state.read(cx).pending_in(path, cx);
        (n > 0).then(|| {
            div()
                .text_color(cx.tokens().palette.sand.hsla())
                .child(format!("{n} pending"))
                .into_any_element()
        })
    }

    fn files_note(&self, cx: &App) -> Option<AnyElement> {
        let tk = cx.tokens();
        let (p, m) = (&tk.palette, &tk.metrics);
        Some(
            div()
                .id("review-note")
                .test_support()
                .flex_none()
                .p_3()
                .rounded(px(12.))
                .bg(p.surface.alpha(0.5))
                .text_size(m.text_xs)
                .text_color(p.text_muted.hsla())
                .child(
                    "Comments collect as a pending review. Sending delivers them to the agent \
                     in one message, with the code they point at and the surrounding diff. \
                     Claude's replies appear in the same threads.",
                )
                .into_any_element(),
        )
    }

    fn hint(&self, _: &App) -> Option<SharedString> {
        Some("Press + beside a line, or after selecting lines, to comment".into())
    }
}
