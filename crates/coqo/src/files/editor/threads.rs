// SPDX-License-Identifier: Apache-2.0

//! Review threads in an ordinary file tab (#258, board 4): a marker on each
//! thread pinned to the work tree's version of the file, and the diff
//! view's thread card in place on a click ([`ThreadCard`]), with **Open in
//! diff**.
//!
//! - **Which threads:** the workspace's ([`WorkspaceThreads`]) whose anchor
//!   is this root and path at `version: worktree`, found (not orphaned),
//!   with lines. Threads on the index or a revision are not shown: their
//!   text is not the buffer's. Resolved threads show only while "Toggle
//!   resolved review threads in files" shows them, or while their card is
//!   open.
//! - **Where:** the daemon resolves anchors against the saved file. A
//!   marker follows edits while the daemon's lines stay what they were when
//!   it was placed; saving lists the threads again, and the new lines win.

use std::ops::Range;

use coqo_protocol::methods::{
    git::DiffSide,
    threads::{Thread, ThreadId, ThreadStatus},
    workspace::RootId,
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{AppContext as _, Context, Entity, Hsla, Subscription, Window},
};

use super::FileEditor;
use crate::threads::{
    OpenReviewDiff, ThreadsEvent, WorkspaceThreads,
    overlay::{CardEvent, ReviewState, ThreadCard},
};

/// Marker ids from here up to [`super::STEP_MARKER_BASE`] are review
/// threads' (`THREAD_MARKER_BASE + thread id`); questions' are below.
pub const THREAD_MARKER_BASE: u64 = 1 << 60;

/// The band over a pending thread's lines (as the diff's tint).
const PENDING_BAND: f32 = 0.10;

/// A review thread's marker in this file.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadMark {
    /// At least [`THREAD_MARKER_BASE`].
    pub id: u64,
    pub thread: ThreadId,
    /// 0-based, `end` exclusive, as the daemon resolved them.
    pub lines: Range<u32>,
    pub color: Hsla,
    /// The band's strength over its lines (0: none).
    pub band: f32,
    pub resolved: bool,
}

/// The thread card open in a file tab.
pub(super) struct OpenCard {
    pub(super) card: Entity<ThreadCard>,
    _events: Subscription,
}

/// The 0-based lines `t` is on in the work tree's `root`/`path`, if it
/// shows in that file's tab.
pub fn file_lines(t: &Thread, root: RootId, path: &str) -> Option<Range<u32>> {
    let a = &t.anchor;
    if a.root != root || a.path != path || a.version != DiffSide::Worktree || a.is_orphaned() {
        return None;
    }
    // 1-based, inclusive.
    let l = a.lines?;
    let start = l.start.saturating_sub(1);
    Some(start..l.end.max(start + 1))
}

impl FileEditor {
    /// Follow the workspace's review threads (from `new`).
    pub(super) fn follow_threads(
        store: &Entity<WorkspaceThreads>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            store,
            window,
            |this, _, event: &ThreadsEvent, window, cx| match event {
                ThreadsEvent::Changed => this.threads_changed(window, cx),
            },
        )
    }

    /// The review threads marked in this file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn thread_marks(&self) -> &[ThreadMark] {
        &self.thread_marks
    }

    /// The thread card open in this tab, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn thread_card(&self) -> Option<&Entity<ThreadCard>> {
        self.thread_card.as_ref().map(|c| &c.card)
    }

    /// The markers and the open card follow the store.
    pub(super) fn threads_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.walkthrough_host {
            return;
        }
        let Some(store) = self.threads.clone() else {
            return;
        };
        let open = self.thread_card.as_ref().map(|c| c.card.read(cx).thread());
        let p = cx.tokens().palette.clone();
        let (root, path) = (self.root, self.path.as_str());
        let store = store.read(cx);
        let shown = store.shows_resolved();
        let marks: Vec<ThreadMark> = store
            .all()
            .into_iter()
            .filter_map(|t| {
                let lines = file_lines(t, root, path)?;
                let resolved = t.status == ThreadStatus::Resolved;
                if resolved && !shown && open != Some(t.id) {
                    return None;
                }
                let pending = t.pending().next().is_some();
                let color = if pending {
                    p.sand
                } else if resolved {
                    p.sage
                } else {
                    p.blue
                };
                Some(ThreadMark {
                    id: THREAD_MARKER_BASE + t.id.0,
                    thread: t.id,
                    lines,
                    color: color.hsla(),
                    band: if pending { PENDING_BAND } else { 0. },
                    resolved,
                })
            })
            .collect();
        let gone = open.is_some_and(|id| !marks.iter().any(|m| m.thread == id));
        self.thread_marks = marks;
        if gone {
            self.close_thread_card(window, cx);
        } else {
            self.sync_markers(cx);
        }
        cx.notify();
    }

    /// A click on marker `id`: open its thread's card here. `false` if it
    /// isn't one of this tab's threads.
    pub(super) fn thread_clicked(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(thread) = self
            .thread_marks
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.thread)
        else {
            return false;
        };
        self.open_thread_card(thread, window, cx);
        true
    }

    /// Open thread `thread`'s card at its lines, with the keyboard in its
    /// reply field.
    pub fn open_thread_card(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.threads.clone() else {
            return;
        };
        let Some(mark) = self.thread_marks.iter().find(|m| m.thread == thread) else {
            return;
        };
        let marker = mark.id;
        let review = match &self.thread_review {
            Some(review) => review.clone(),
            None => {
                let review = cx.new(|cx| ReviewState::standalone(store, window, cx));
                self.thread_review = Some(review.clone());
                review
            }
        };
        self.drop_file_step(cx);
        let card = cx.new(|cx| ThreadCard::new(review, thread, cx));
        let events = cx.subscribe_in(&card, window, |this, _, event: &CardEvent, window, cx| {
            this.card_event(*event, window, cx)
        });
        self.thread_card = Some(OpenCard {
            card: card.clone(),
            _events: events,
        });
        let focus = card.read(cx).focus_target(cx);
        self.show_step(card.into(), Some(marker), false, Some(focus), window, cx);
    }

    /// Forget the card without touching the overlay (another popover takes
    /// it).
    pub(super) fn drop_thread_card(&mut self, _: &mut Context<Self>) {
        if self.thread_card.take().is_some() {
            self.step = None;
        }
    }

    /// Close the card; the keyboard goes back to the code if it was in it.
    pub fn close_thread_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.thread_card.take() else {
            return;
        };
        let focused = open.card.read(cx).contains_focus(window, cx);
        self.clear_step(cx);
        if focused && let Some(editor) = self.editor().cloned() {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
        // A resolved thread shown only while open goes.
        self.threads_changed(window, cx);
    }

    fn card_event(&mut self, event: CardEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            CardEvent::Close => self.close_thread_card(window, cx),
            CardEvent::OpenInDiff => {
                self.close_thread_card(window, cx);
                window.dispatch_action(
                    Box::new(OpenReviewDiff {
                        workspace: self.workspace.0.clone(),
                        diff: format!("git:{}:index..worktree", self.root.0),
                        path: Some(self.path.clone()),
                    }),
                    cx,
                );
            }
        }
    }

    /// List the threads again: the saved file is what the daemon resolves
    /// their anchors against.
    pub(super) fn relist_threads(&mut self, cx: &mut Context<Self>) {
        if let Some(store) = self.threads.clone() {
            store.update(cx, |store, cx| store.relist(cx));
        }
    }
}
