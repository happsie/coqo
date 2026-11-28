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
//! - **Starting one** (#311, [`FileEditor::comment_on`]): "Add review
//!   comment" or Alt-click on the ask button opens the diff's composer
//!   ([`ThreadComposer`]) under the lines; "Add to review" starts a pending
//!   thread on them at `version: worktree`, in the root's unstaged diff, and
//!   its card follows. With unsaved edits it waits for a save: the daemon
//!   pins the thread to the saved file.

use std::ops::Range;

use coqo_protocol::methods::{
    git::DiffSide,
    threads::{Thread, ThreadId, ThreadLines, ThreadStatus},
    workspace::RootId,
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AppContext as _, Context, Entity, FocusHandle, Focusable as _, Hsla, Subscription, Window,
    },
};

use super::{FileEditor, StepOverlay};
use crate::threads::{
    OpenReviewDiff, ThreadsEvent, WorkspaceThreads,
    overlay::{CardEvent, ComposerEvent, FileLines, ReviewState, ThreadCard, ThreadComposer},
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

/// The composer of a new thread open in a file tab (#311).
pub(super) struct OpenComposer {
    pub(super) view: Entity<ThreadComposer>,
    /// Its field's focus, to give the keyboard back to the code on close.
    field: FocusHandle,
    _events: Subscription,
}

/// Why "Add to review" waits while the tab has unsaved edits.
pub const UNSAVED_NOTE: &str = "Save the file first: review comments are pinned to the saved file.";

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
        self.open_pending_card(window, cx);
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
        self.open_thread_card_with(thread, true, window, cx);
    }

    /// Open thread `thread`'s card at its lines, with the keyboard in its
    /// reply field with `reply`, else on the card (`Esc` closes it).
    fn open_thread_card_with(
        &mut self,
        thread: ThreadId,
        reply: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mark) = self.thread_marks.iter().find(|m| m.thread == thread) else {
            return;
        };
        let marker = mark.id;
        let Some(review) = self.review_state(window, cx) else {
            return;
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
        let focus = if reply {
            card.read(cx).focus_target(cx)
        } else {
            card.read(cx).card_focus()
        };
        self.show_step(card.into(), Some(marker), false, Some(focus), window, cx);
    }

    /// The state of this tab's cards and composer, made on first use.
    fn review_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ReviewState>> {
        let store = self.threads.clone()?;
        Some(match &self.thread_review {
            Some(review) => review.clone(),
            None => {
                let review = cx.new(|cx| ReviewState::standalone(store, window, cx));
                self.thread_review = Some(review.clone());
                review
            }
        })
    }

    // ---- starting a thread (#311) ---------------------------------------------

    /// The composer open in this tab, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn thread_composer(&self) -> Option<&Entity<ThreadComposer>> {
        self.thread_composer.as_ref().map(|c| &c.view)
    }

    /// The state behind this tab's cards and composer, once made.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn review(&self) -> Option<&Entity<ReviewState>> {
        self.thread_review.as_ref()
    }

    /// Start a review comment on the selected lines (the cursor's line
    /// without a selection): [`crate::threads::AddReviewComment`].
    pub(super) fn comment_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor() else {
            return;
        };
        let lines = editor.read(cx).selected_lines(cx);
        self.comment_on(lines, window, cx);
    }

    /// Open the diff's composer under `lines` (0-based, `end` exclusive),
    /// with the keyboard in it. "Add to review" starts a pending thread on
    /// them, on the work tree's version of the file, in the root's unstaged
    /// diff. Not in a walkthrough player's own tab, which shows no review
    /// threads.
    pub fn comment_on(&mut self, lines: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.walkthrough_host || self.editor().is_none() {
            return;
        }
        let Some(review) = self.review_state(window, cx) else {
            return;
        };
        if self.popover.is_some() {
            self.close_popover(window, cx);
        }
        self.ask = None;
        self.drop_file_step(cx);
        self.drop_thread_card(cx);
        self.drop_thread_composer(cx);
        self.card_pending = None;
        let end = lines.end.max(lines.start + 1);
        let target = FileLines {
            root: self.root,
            path: self.path.clone(),
            lines: ThreadLines {
                start: lines.start as u32 + 1,
                end: end as u32,
            },
        };
        review.update(cx, |s, cx| s.open_file_composer(target, window, cx));
        let Some(field) = review
            .read(cx)
            .composer_input()
            .map(|i| i.read(cx).focus_handle(cx))
        else {
            return;
        };
        let view = cx.new(|cx| ThreadComposer::new(review.clone(), cx));
        let events = cx.subscribe_in(
            &review,
            window,
            |this, _, event: &ComposerEvent, window, cx| this.composer_event(*event, window, cx),
        );
        self.thread_composer = Some(OpenComposer {
            view: view.clone(),
            field,
            _events: events,
        });
        self.sync_composer_blocked(cx);
        self.step = Some(StepOverlay {
            view: view.into(),
            marker: None,
            lines: Some(lines.start..end),
            reveal: false,
            focus: None,
            placed: false,
        });
        self.place_step(window, cx);
    }

    fn composer_event(
        &mut self,
        event: ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Closed => self.close_thread_composer(window, cx),
            ComposerEvent::Added(thread) => {
                // The composer gives way to the new thread's card, once it
                // is marked here.
                self.close_thread_composer(window, cx);
                self.card_pending = Some(thread);
                self.open_pending_card(window, cx);
            }
        }
    }

    /// Open the card of the thread just added here, if it is marked yet.
    fn open_pending_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.card_pending else {
            return;
        };
        if self.thread_marks.iter().any(|m| m.thread == thread) {
            self.card_pending = None;
            self.open_thread_card_with(thread, false, window, cx);
        }
    }

    /// Close the composer; the keyboard goes back to the code if it was in
    /// it.
    pub fn close_thread_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self
            .thread_composer
            .as_ref()
            .is_some_and(|c| c.field.contains_focused(window, cx));
        if !self.drop_thread_composer(cx) {
            return;
        }
        if let Some(editor) = self.editor().cloned() {
            editor.update(cx, |editor, cx| {
                editor.clear_overlay(cx);
                if focused {
                    editor.focus(window, cx);
                }
            });
        }
        cx.notify();
    }

    /// Forget the composer without touching the overlay (another popover
    /// takes it). Whether one was open.
    pub(super) fn drop_thread_composer(&mut self, cx: &mut Context<Self>) -> bool {
        if self.thread_composer.take().is_none() {
            return false;
        }
        self.step = None;
        if let Some(review) = self.thread_review.clone() {
            review.update(cx, |s, cx| {
                s.close_composer(cx);
                s.set_composer_blocked(None, cx);
            });
        }
        true
    }

    /// "Add to review" waits while the tab has unsaved edits: the daemon
    /// pins the thread to the saved file, whose lines may differ.
    pub(super) fn sync_composer_blocked(&mut self, cx: &mut Context<Self>) {
        if self.thread_composer.is_none() {
            return;
        }
        let Some(review) = self.thread_review.clone() else {
            return;
        };
        let reason = self.is_dirty().then(|| UNSAVED_NOTE.to_owned());
        review.update(cx, |s, cx| s.set_composer_blocked(reason, cx));
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
                        diff: crate::threads::unstaged_diff(self.root),
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
