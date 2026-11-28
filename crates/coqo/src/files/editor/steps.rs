// SPDX-License-Identifier: Apache-2.0

//! Walkthrough steps in an ordinary file tab (#260): the numbered badges of
//! the workspace's active walkthroughs ([`crate::walkthrough::marks`]) and
//! the step popover in place, with **Open in player** and **Mark
//! understood**. A tab the player opened for itself shows the player's
//! markers instead ([`FileEditor::set_walkthrough_host`]).

use coqo_protocol::methods::walkthroughs::{StepState, WalkthroughId};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{AppContext as _, Context, Entity, Focusable as _, Subscription, Window},
};

use super::{FileEditor, StepMark};
use crate::walkthrough::{
    self, OpenWalkthrough,
    marks::{FileStep, WalkthroughsEvent, WorkspaceWalkthroughs},
    popover::{StepEvent, StepPopover},
};

/// Badges of steps whose popover is not open are at this strength (board
/// 3's "half strength").
const DIM: f32 = 0.5;

/// The step popover open in a file tab.
pub(super) struct FileStepPopover {
    pub(super) popover: Entity<StepPopover>,
    /// The step it shows.
    walkthrough: WalkthroughId,
    step: usize,
    anchor: usize,
    _events: Subscription,
}

impl FileEditor {
    /// Follow the workspace's walkthroughs (from `new`).
    pub(super) fn follow_walkthroughs(
        store: &Entity<WorkspaceWalkthroughs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            store,
            window,
            |this, _, event: &WalkthroughsEvent, window, cx| match event {
                WalkthroughsEvent::Changed => this.walkthroughs_changed(window, cx),
            },
        )
    }

    /// This tab belongs to a walkthrough player, which sets its step
    /// markers ([`Self::set_step_marks`]): the active walkthroughs' are not
    /// shown here.
    pub fn set_walkthrough_host(&mut self, cx: &mut Context<Self>) {
        self.walkthrough_host = true;
        self.file_steps.clear();
        if self.file_step.take().is_some() {
            self.clear_step(cx);
        }
        self.set_step_marks(Vec::new(), cx);
    }

    /// The active walkthroughs' steps marked in this file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn file_steps(&self) -> &[FileStep] {
        &self.file_steps
    }

    /// The step popover open in this tab (not the player's), if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn file_step_popover(&self) -> Option<&Entity<StepPopover>> {
        self.file_step.as_ref().map(|s| &s.popover)
    }

    /// The step markers and the open popover follow the store.
    pub(super) fn walkthroughs_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.walkthrough_host {
            return;
        }
        let Some(store) = self.walkthroughs.clone() else {
            return;
        };
        let (root, path) = (self.root, self.path.clone());
        self.file_steps = store.update(cx, |store, _| store.marks_in(root, &path));
        // The popover's step may be gone (understood to the end, hidden,
        // republished without it).
        let open = self.file_step.as_ref().and_then(|s| {
            self.file_steps
                .iter()
                .find(|m| {
                    m.walkthrough == s.walkthrough && m.step == s.step && m.anchor == s.anchor
                })
                .cloned()
        });
        match open {
            Some(mark) => {
                self.sync_file_steps(cx);
                self.fill_file_step(&mark, cx);
            }
            None => {
                if self.file_step.is_some() {
                    self.close_file_step(window, cx);
                }
                self.sync_file_steps(cx);
            }
        }
    }

    /// Hand the step marks to the editor: the open step's at full strength.
    fn sync_file_steps(&mut self, cx: &mut Context<Self>) {
        let sage = cx.tokens().palette.sage.hsla();
        let open = self
            .file_step
            .as_ref()
            .map(|s| (s.walkthrough.clone(), s.step));
        let marks = self
            .file_steps
            .iter()
            .map(|m| StepMark {
                id: m.id,
                lines: m.lines.clone(),
                color: if open.as_ref() == Some(&(m.walkthrough.clone(), m.step)) {
                    sage
                } else {
                    sage.opacity(DIM)
                },
                label: m.label.clone(),
            })
            .collect();
        self.set_step_marks(marks, cx);
    }

    /// Show the popover's step as the store has it now.
    fn fill_file_step(&mut self, mark: &FileStep, cx: &mut Context<Self>) {
        let (Some(store), Some(open)) = (self.walkthroughs.clone(), self.file_step.as_ref()) else {
            return;
        };
        let questions = self.questions.clone();
        let data = store.read(cx).walkthrough(&mark.walkthrough).and_then(|w| {
            let agent = walkthrough::agent_name(w, questions.as_ref(), cx);
            walkthrough::popover::step_data(w, mark.step, mark.anchor, agent, None)
        });
        if let Some(data) = data {
            open.popover.update(cx, |p, cx| p.set(data, cx));
        }
    }

    /// A click on step marker `id`: open its popover here, with the
    /// keyboard in it. `false` if it isn't one of this tab's.
    pub(super) fn file_step_clicked(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mark) = self.file_steps.iter().find(|m| m.id == id).cloned() else {
            return false;
        };
        self.open_file_step(&mark, window, cx);
        true
    }

    /// Open the popover of `mark`'s step at its lines.
    pub fn open_file_step(&mut self, mark: &FileStep, window: &mut Window, cx: &mut Context<Self>) {
        let popover = match &self.file_step {
            Some(open) => open.popover.clone(),
            None => {
                let host = cx.entity().downgrade();
                cx.new(|cx| StepPopover::in_file_tab(host, cx))
            }
        };
        let events = cx.subscribe_in(
            &popover,
            window,
            |this, _, event: &StepEvent, window, cx| this.file_step_event(event, window, cx),
        );
        self.file_step = Some(FileStepPopover {
            popover: popover.clone(),
            walkthrough: mark.walkthrough.clone(),
            step: mark.step,
            anchor: mark.anchor,
            _events: events,
        });
        self.fill_file_step(mark, cx);
        self.sync_file_steps(cx);
        let focus = popover.focus_handle(cx);
        self.show_step(
            popover.into(),
            Some(mark.id),
            false,
            Some(focus),
            window,
            cx,
        );
    }

    /// Forget the step popover without touching the overlay (another
    /// popover takes it).
    pub(super) fn drop_file_step(&mut self, cx: &mut Context<Self>) {
        if self.file_step.take().is_some() {
            self.step = None;
            self.sync_file_steps(cx);
        }
    }

    /// Close the popover; the keyboard goes back to the code if it was in
    /// it.
    pub fn close_file_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.file_step.take() else {
            return;
        };
        let focused = open.popover.read(cx).contains_focus(window, cx);
        self.clear_step(cx);
        if focused && let Some(editor) = self.editor().cloned() {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
        self.sync_file_steps(cx);
    }

    fn file_step_event(&mut self, event: &StepEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, step)) = self
            .file_step
            .as_ref()
            .map(|s| (s.walkthrough.clone(), s.step))
        else {
            return;
        };
        match event {
            StepEvent::ToggleUnderstood => {
                let Some(store) = self.walkthroughs.clone() else {
                    return;
                };
                let state = store
                    .read(cx)
                    .walkthrough(&id)
                    .and_then(|w| w.steps.get(step).map(|s| walkthrough::step_state(w, s)));
                let next = if state == Some(StepState::Understood) {
                    StepState::Unread
                } else {
                    StepState::Understood
                };
                store.update(cx, |store, cx| store.set_state(&id, step, next, cx));
            }
            StepEvent::OpenInPlayer | StepEvent::ViewDiff | StepEvent::Anchor(_) => {
                self.open_in_player(id, step, window, cx)
            }
            StepEvent::GoTo(ix) => self.open_in_player(id, *ix, window, cx),
            StepEvent::Close => self.close_file_step(window, cx),
            // The player's.
            StepEvent::Previous | StepEvent::Next | StepEvent::Ask(_) => {}
        }
    }

    /// Close the popover and show step `ix` of `id` in its player.
    fn open_in_player(
        &mut self,
        id: WalkthroughId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = self
            .walkthroughs
            .as_ref()
            .and_then(|s| s.read(cx).walkthrough(&id))
            .and_then(|w| w.steps.get(ix))
            .map(|s| s.id.clone());
        self.close_file_step(window, cx);
        window.dispatch_action(
            Box::new(OpenWalkthrough {
                workspace: self.workspace.0.clone(),
                id: id.0,
                step,
            }),
            cx,
        );
    }
}
