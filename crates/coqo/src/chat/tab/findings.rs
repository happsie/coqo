// SPDX-License-Identifier: Apache-2.0

//! Findings an agent writes as text, pinned as gutter notes (#345, ADR 0030).
//!
//! When an agent message is complete, the chat looks for `file:line`
//! references in it ([`crate::questions::findings`]), checks each against
//! the workspace, and pins every valid one with `questions/create` as an
//! annotation whose [`NoteOrigin`] is the message. The notes then follow
//! edits and renames like any anchor (#288). Under the message the chat says
//! "N notes in code" (a click shows the first) and offers to dismiss them
//! all; a note's popover has "Open in chat", which scrolls back here
//! ([`ChatTab::reveal_message`]).
//!
//! Only messages that arrive while the chat is open are scanned: replaying a
//! chat's history, or reopening it, never pins anything, and a message is
//! scanned once. Notes are never made from the user's messages.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use coqo_protocol::methods::{
    editor::LineRange,
    questions::{self as qm, CreateParams, NewAnchor, NoteOrigin, QuestionKind, Severity},
    workspace::RootId,
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, div, px,
    },
    text::markdown,
    widgets::TestSupportExt as _,
};

use super::ChatTab;
use crate::{
    daemon::Daemon,
    questions::{
        self, FindingLevel, OpenQuestion, WorkspaceQuestions,
        findings::{self, Workspace},
        questions_for,
    },
    settings,
};

/// How long a message stays marked after a note's "Open in chat".
const REVEAL: Duration = Duration::from_millis(2500);

/// The workspace's files for the check, and where each is.
#[derive(Debug)]
pub struct FindingFiles {
    pub workspace: Workspace,
    /// By the name the workspace lists: the root and the root-relative path.
    pub places: HashMap<String, (RootId, String)>,
}

/// A chat tab's state for findings.
#[derive(Debug, Default)]
pub struct Findings {
    /// Messages at or before this seq were there when the chat was opened:
    /// never scanned. `None` until the first snapshot.
    from: Option<u64>,
    /// Messages scanned, by seq.
    scanned: HashSet<u64>,
    /// The workspace's notes, to count them under their messages.
    store: Option<Entity<WorkspaceQuestions>>,
    /// The notes counted under each message when last drawn.
    counts: HashMap<u64, usize>,
    /// The message "Open in chat" is bringing into view, once it is there.
    reveal: Option<u64>,
    /// The message marked as just revealed.
    marked: Option<u64>,
    _store: Option<Subscription>,
    _unmark: Option<Task<()>>,
    _pinning: Vec<Task<()>>,
}

impl ChatTab {
    /// The first snapshot says which messages are history.
    pub(super) fn start_findings(&mut self, head: u64, cx: &mut Context<Self>) {
        self.findings.from.get_or_insert(head);
        if self.findings.store.is_none()
            && let Some(workspace) = self.info.as_ref().and_then(|i| i.workspace.clone())
            && let Some(store) = questions_for(
                &coqo_protocol::methods::workspace::WorkspaceId(workspace),
                cx,
            )
        {
            self.findings._store = Some(cx.observe(&store, |this, _, cx| this.notes_changed(cx)));
            self.findings.store = Some(store);
        }
    }

    /// Pin the findings of every message that is complete by now.
    pub(super) fn scan_findings(&mut self, cx: &mut Context<Self>) {
        if !settings::get(cx).bool(settings::FINDINGS_IN_GUTTER, None) {
            return;
        }
        let Some(from) = self.findings.from else {
            return;
        };
        let due: Vec<(usize, u64)> = (0..self.transcript.entries.len())
            .filter_map(|ix| {
                let seq = self.transcript.message_seq(ix)?;
                (seq > from
                    && !self.findings.scanned.contains(&seq)
                    && self.transcript.message_done(ix))
                .then_some((ix, seq))
            })
            .collect();
        if due.is_empty() {
            return;
        }
        let Some(files) = self.finding_files() else {
            // Wait for the file index; it scans again when it is built.
            self.ensure_index(cx);
            return;
        };
        for (ix, seq) in due {
            self.findings.scanned.insert(seq);
            let Some(crate::chat::transcript::Entry::Agent { text }) =
                self.transcript.entries.get(ix)
            else {
                continue;
            };
            let hints = self.turn_hints(ix, &files.workspace);
            let found = findings::detect(text, &files.workspace, &hints);
            if !found.is_empty() {
                self.pin_findings(seq, found, &files, cx);
            }
        }
    }

    /// The files the turn before message `ix` touched, as the workspace
    /// lists them.
    fn turn_hints(&self, ix: usize, ws: &Workspace) -> Vec<String> {
        self.transcript
            .turn_paths(ix)
            .iter()
            .filter_map(|abs| {
                ws.roots.iter().find_map(|root| {
                    let rest = abs.strip_prefix(root.trim_end_matches('/'))?;
                    let rest = rest.strip_prefix('/')?;
                    ws.paths.iter().find(|p| p.as_str() == rest).cloned()
                })
            })
            .collect()
    }

    fn pin_findings(
        &mut self,
        message: u64,
        found: Vec<findings::Finding>,
        files: &FindingFiles,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.info.as_ref().and_then(|i| i.workspace.clone()) else {
            return;
        };
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let Some(store) = self.findings.store.clone() else {
            return;
        };
        let session = self.session.clone();
        if store.read(cx).has_message_notes(&session, message) {
            return;
        }
        let calls: Vec<_> = found
            .into_iter()
            .filter_map(|f| {
                let (root, path) = files.places.get(&f.path)?.clone();
                let params = CreateParams {
                    workspace: coqo_protocol::methods::workspace::WorkspaceId(workspace.clone()),
                    session: session.0.clone(),
                    kind: QuestionKind::Annotation,
                    body: f.text,
                    options: Vec::new(),
                    severity: Some(match f.level {
                        FindingLevel::Critical => Severity::Warning,
                        _ => Severity::Info,
                    }),
                    anchors: vec![NewAnchor {
                        root,
                        path,
                        lines: LineRange {
                            start: f.start - 1,
                            end: f.end - 1,
                        },
                    }],
                    origin: Some(NoteOrigin {
                        message,
                        level: f.level,
                    }),
                };
                Some(daemon.read(cx).request::<qm::Create>(params))
            })
            .collect();
        let store = store.downgrade();
        let task = cx.spawn(async move |_, cx| {
            for call in calls {
                // The daemon checks the file and the line against the file as
                // it is: what it refuses (a line past the end, a file that
                // went) is not a finding, and nobody is told.
                if let Ok(question) = call.await {
                    store
                        .update(cx, |store, cx| store.merge(vec![question], cx))
                        .ok();
                }
            }
        });
        self.findings._pinning.push(task);
    }

    /// The open notes made from message `seq`.
    pub fn message_note_ids(&self, seq: u64, cx: &App) -> Vec<qm::QuestionId> {
        let Some(store) = &self.findings.store else {
            return Vec::new();
        };
        store
            .read(cx)
            .message_notes(&self.session, seq)
            .iter()
            .map(|q| q.id)
            .collect()
    }

    /// The store changed: draw the messages whose note counts changed again.
    fn notes_changed(&mut self, cx: &mut Context<Self>) {
        let seqs: Vec<(usize, u64)> = (0..self.transcript.entries.len())
            .filter_map(|ix| Some((ix, self.transcript.message_seq(ix)?)))
            .collect();
        for (ix, seq) in seqs {
            let now = self.message_note_ids(seq, cx).len();
            let before = self.findings.counts.insert(seq, now).unwrap_or(0);
            if now != before {
                self.list.remeasure_items(ix..ix + 1);
            }
        }
        cx.notify();
    }

    /// Bring message `seq` into view and mark it for a moment ("Open in
    /// chat" on one of its notes). A message that isn't loaded yet is shown
    /// as soon as it is.
    pub fn reveal_message(&mut self, seq: u64, cx: &mut Context<Self>) {
        self.findings.reveal = Some(seq);
        self.reveal_pending();
        cx.notify();
    }

    /// The message "Open in chat" last brought into view, while it is marked.
    #[cfg(test)]
    pub fn marked_message(&self) -> Option<u64> {
        self.findings.marked
    }

    pub(super) fn reveal_pending(&mut self) {
        let Some(seq) = self.findings.reveal else {
            return;
        };
        if let Some(ix) = self.transcript.message_index(seq) {
            self.findings.reveal = None;
            self.findings.marked = Some(seq);
            self.list.scroll_to_reveal_item(ix);
            self.list.remeasure_items(ix..ix + 1);
        }
    }

    /// An agent message, with "N notes in code" under it when it has notes.
    pub(super) fn render_agent(
        &mut self,
        ix: usize,
        text: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let seq = self.transcript.message_seq(ix);
        let notes = seq
            .map(|s| self.message_note_ids(s, cx))
            .unwrap_or_default();
        let marked = seq.is_some() && seq == self.findings.marked;
        if marked && self.findings._unmark.is_none() {
            self.findings._unmark = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REVEAL).await;
                this.update(cx, |this, cx| {
                    if let Some(ix) = this
                        .findings
                        .marked
                        .take()
                        .and_then(|s| this.transcript.message_index(s))
                    {
                        this.list.remeasure_items(ix..ix + 1);
                    }
                    this.findings._unmark = None;
                    cx.notify();
                })
                .ok();
            }));
        }
        let footer = (!notes.is_empty()).then(|| {
            let count = notes.len();
            let first = notes[0].0;
            let (session, message) = (self.session.clone(), seq.unwrap_or(0));
            div()
                .id(("chat-notes", ix))
                .test_support()
                .flex()
                .items_center()
                .gap(px(10.))
                .pt(px(6.))
                .text_size(m.text_sm)
                .child(
                    div()
                        .id(("chat-notes-show", ix))
                        .test_support()
                        .text_color(p.blue.hsla())
                        .cursor_pointer()
                        .child(if count == 1 {
                            "1 note in code".to_owned()
                        } else {
                            format!("{count} notes in code")
                        })
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(Box::new(OpenQuestion { id: first }), cx)
                        }),
                )
                .child(
                    div()
                        .id(("chat-notes-dismiss", ix))
                        .test_support()
                        .text_color(p.text_subtle.hsla())
                        .cursor_pointer()
                        .child(if count == 1 { "Dismiss" } else { "Dismiss all" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dismiss_message_notes(&session, message, cx)
                        })),
                )
        });
        div()
            .id(("chat-agent", ix))
            .test_support()
            .when(marked, |d| {
                d.border_l_2()
                    .border_color(p.blue.hsla())
                    .pl(px(10.))
                    .ml(px(-12.))
            })
            .child(markdown(("chat-md", ix), text))
            .children(footer)
            .into_any_element()
    }

    fn dismiss_message_notes(
        &mut self,
        session: &coqo_protocol::methods::agents::SessionId,
        message: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.findings.store.clone() {
            questions::dismiss_message(&store, session, message, cx).detach();
        }
    }
}

use coqo_ui::gpui::prelude::FluentBuilder as _;
