// SPDX-License-Identifier: Apache-2.0

//! The ask popover (#229): ask an agent about the selected lines of a file
//! tab. It opens under them (the gutter's ask button, or `Ctrl/Cmd+Shift+/`)
//! with a text field and the chat to ask: the chat shown in the workbench,
//! else the workspace's most recent one, else a new one. `Enter` sends
//! (`questions/ask`), `Esc` cancels. Once sent the file tab shows the pinned
//! thread's popover instead.

use std::ops::Range;

use coqo_protocol::methods::{
    agents::{Create, CreateSessionParams, Providers, SessionId, SessionInfo},
    editor::LineRange,
    questions::{AskParams, NewAnchor, QuestionId},
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
        FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
        prelude::FluentBuilder as _, px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::{Icon, TestSupportExt as _, icon, popover, primary, raised},
};

use super::WorkspaceQuestions;
use crate::{
    chat::{self, list::ChatList},
    daemon::{Daemon, DaemonError},
    workspace::Workspaces,
};

/// Chats offered at most, besides a new one.
const CHOICES: usize = 4;

/// Emitted by an [`AskPopover`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskEvent {
    /// The thread was pinned: show it.
    Asked(QuestionId),
    /// Closed without asking.
    Cancel,
}

/// Which chat to ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskTarget {
    Session(SessionId),
    /// Start one.
    New,
}

/// The ask popover of one file tab. See the module docs.
pub struct AskPopover {
    workspace: WorkspaceId,
    root: RootId,
    path: String,
    /// 0-based, `end` exclusive.
    lines: Range<usize>,
    store: Entity<WorkspaceQuestions>,
    /// The chats offered, the default first.
    sessions: Vec<SessionId>,
    target: AskTarget,
    input: Entity<PromptInput>,
    focus: FocusHandle,
    sending: bool,
    error: Option<String>,
    _sending: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for AskPopover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskPopover")
            .field("path", &self.path)
            .field("lines", &self.lines)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<AskEvent> for AskPopover {}

/// The chats to offer for `workspace`: those `shown` in the workbench (in
/// that order), then the rest by last activity; open ones only.
pub fn choices(workspace: &WorkspaceId, shown: &[SessionId], cx: &App) -> Vec<SessionId> {
    let Some(list) = ChatList::global(cx) else {
        return Vec::new();
    };
    let mut sessions: Vec<&SessionInfo> = list
        .read(cx)
        .sessions()
        .iter()
        .filter(|s| !s.closed && s.workspace.as_deref() == Some(workspace.0.as_str()))
        .collect();
    sessions.sort_by_key(|s| {
        let rank = shown
            .iter()
            .position(|id| *id == s.id)
            .unwrap_or(usize::MAX);
        (rank, std::cmp::Reverse(s.updated_at))
    });
    sessions
        .into_iter()
        .take(CHOICES)
        .map(|s| s.id.clone())
        .collect()
}

impl AskPopover {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: WorkspaceId,
        root: RootId,
        path: String,
        lines: Range<usize>,
        store: Entity<WorkspaceQuestions>,
        sessions: Vec<SessionId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| PromptInput::new("Ask about these lines", 2, 8, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, _, event: &PromptInputEvent, window, cx| match event {
                    PromptInputEvent::Submit => this.send(window, cx),
                    PromptInputEvent::Cancel => cx.emit(AskEvent::Cancel),
                    PromptInputEvent::Changed => {
                        this.error = None;
                        cx.notify();
                    }
                    PromptInputEvent::Key(_) => {}
                },
            ),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        let target = sessions
            .first()
            .cloned()
            .map_or(AskTarget::New, AskTarget::Session);
        Self {
            workspace,
            root,
            path,
            lines,
            store,
            sessions,
            target,
            input,
            focus: cx.focus_handle(),
            sending: false,
            error: None,
            _sending: None,
            _subscriptions: subscriptions,
        }
    }

    /// The chat it will ask.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn target(&self) -> &AskTarget {
        &self.target
    }

    /// Ask `target` instead.
    pub fn set_target(&mut self, target: AskTarget, cx: &mut Context<Self>) {
        self.target = target;
        cx.notify();
    }

    /// The text field.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn input(&self) -> &Entity<PromptInput> {
        &self.input
    }

    /// Whether the keyboard is in the popover.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
            || self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Put the keyboard in the text field.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let handle = self.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Pin the thread and ask (starting a chat first for a new one).
    pub fn send(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        let body = self.input.read(cx).text(cx).trim().to_owned();
        if body.is_empty() {
            self.error = Some("Write a question first.".into());
            cx.notify();
            return;
        }
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            self.error = Some("coqod isn't connected.".into());
            cx.notify();
            return;
        };
        let anchors = vec![NewAnchor {
            root: self.root,
            path: self.path.clone(),
            lines: LineRange {
                start: self.lines.start as u32,
                end: self.lines.end.max(self.lines.start + 1) as u32 - 1,
            },
        }];
        let workspace = self.workspace.clone();
        let target = self.target.clone();
        let new_chat = match &target {
            AskTarget::New => Some(self.new_chat(&daemon, cx)),
            AskTarget::Session(_) => None,
        };
        let store = self.store.clone();
        self.sending = true;
        self.error = None;
        cx.notify();
        self._sending = Some(cx.spawn(async move |this, cx| {
            let session = match (target, new_chat) {
                (AskTarget::Session(id), _) => Ok(id),
                (AskTarget::New, Some(task)) => task.await,
                (AskTarget::New, None) => Err("Couldn't start a chat".to_owned()),
            };
            let result = match session {
                Ok(session) => {
                    let asked = cx.update(|cx| {
                        super::ask(
                            &store,
                            AskParams {
                                workspace,
                                session,
                                body,
                                anchors,
                            },
                            cx,
                        )
                    });
                    asked.await.map_err(|e| format!("Couldn't ask: {e}"))
                }
                Err(e) => Err(e),
            };
            this.update(cx, |this, cx| {
                this.sending = false;
                match result {
                    Ok(q) => cx.emit(AskEvent::Asked(q.id)),
                    Err(e) => {
                        this.error = Some(e);
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// Start a chat in the workspace and list it ([`start_chat`]).
    fn new_chat(
        &self,
        _daemon: &Entity<Daemon>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SessionId, String>> {
        start_chat(&self.workspace, cx)
    }

    fn render_targets(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let pill = |id: SharedString, ix: usize, label: String, on: bool, cx: &App| {
            let d = div()
                .id((id, ix))
                .test_support()
                .px(px(10.))
                .py(px(4.))
                .rounded(px(999.))
                .text_size(m.text_sm)
                .cursor_pointer()
                .border_1();
            let d = if on {
                d.bg(p.blue.hsla().opacity(0.16))
                    .border_color(p.blue.hsla())
                    .text_color(p.text.hsla())
            } else {
                d.border_color(p.border.hsla())
                    .text_color(p.text_muted.hsla())
            };
            let _ = cx;
            d.child(label)
        };
        let store = self.store.read(cx);
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .px(px(18.))
            .pt(px(10.))
            .child(
                div()
                    .text_size(m.text_sm)
                    .text_color(p.text_subtle.hsla())
                    .child("Ask"),
            );
        let labels: Vec<(SessionId, String)> = self
            .sessions
            .iter()
            .map(|id| (id.clone(), store.asker(id, cx)))
            .collect();
        for (ix, (id, label)) in labels.into_iter().enumerate() {
            let on = self.target == AskTarget::Session(id.clone());
            row = row.child(
                pill("ask-target".into(), ix, label, on, cx).on_click(cx.listener(
                    move |this, _, _, cx| this.set_target(AskTarget::Session(id.clone()), cx),
                )),
            );
        }
        let on = self.target == AskTarget::New;
        row.child(
            pill("ask-target-new".into(), 0, "New chat".into(), on, cx)
                .on_click(cx.listener(|this, _, _, cx| this.set_target(AskTarget::New, cx))),
        )
        .into_any_element()
    }
}

/// Start a chat in `workspace` (with Claude if it is available, else the
/// first available provider) and list it. Also the review's "New chat"
/// (#95).
pub fn start_chat(workspace: &WorkspaceId, cx: &mut App) -> Task<Result<SessionId, String>> {
    let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
        return Task::ready(Err("coqod isn't connected.".into()));
    };
    let cwd = Workspaces::global(cx).and_then(|w| {
        let w = w.read(cx);
        let ws = w.current().filter(|ws| ws.id == *workspace)?;
        ws.roots.first().map(|r| r.path.clone())
    });
    let Some(cwd) = cwd else {
        return Task::ready(Err("Open the workspace to start a chat".into()));
    };
    let workspace = workspace.clone();
    let providers = daemon.read(cx).request::<Providers>(());
    let daemon = daemon.downgrade();
    cx.spawn(async move |cx| {
        let providers = providers
            .await
            .map_err(|e| format!("Couldn't start a chat: {e}"))?;
        let provider = chat::preferred_provider(&providers)
            .ok_or_else(|| "No agent is available".to_owned())?
            .id
            .clone();
        let create = daemon
            .update(cx, |daemon, _| {
                daemon.request::<Create>(CreateSessionParams {
                    provider,
                    cwd,
                    workspace: Some(workspace.0),
                    title: None,
                    mode: None,
                })
            })
            .map_err(|e| e.to_string())?;
        let info = create
            .await
            .map_err(|e: DaemonError| format!("Couldn't start a chat: {e}"))?;
        let id = info.id.clone();
        cx.update(|cx| {
            if let Some(list) = ChatList::global(cx) {
                list.update(cx, |l, cx| l.upsert(info, cx));
            }
        });
        Ok(id)
    })
}

/// "src/cart.rs:6–7" (1-based).
fn place(path: &str, lines: &Range<usize>) -> String {
    if lines.len() > 1 {
        format!("{path}:{}–{}", lines.start + 1, lines.end)
    } else {
        format!("{path}:{}", lines.start + 1)
    }
}

impl Focusable for AskPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AskPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let targets = self.render_targets(cx);
        let error = self.error.clone().map(|e| {
            div()
                .id("ask-error")
                .test_support()
                .px(px(18.))
                .pt(px(10.))
                .text_size(m.text_sm)
                .text_color(p.rose.hsla())
                .child(SharedString::from(e))
        });
        let empty = self.input.read(cx).text(cx).trim().is_empty();
        popover(cx)
            .id("ask-popover")
            .test_support()
            .track_focus(&self.focus)
            .rounded(px(18.))
            .w_full()
            .font_family(m.font_ui.clone())
            .text_size(m.text_md)
            .text_color(p.text.hsla())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(18.))
                    .pt(px(16.))
                    .child(icon(Icon::MessageCircle, px(14.), p.blue.hsla()))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Ask about this code"),
                    )
                    .child(
                        div()
                            .text_size(m.text_sm)
                            .text_color(p.text_muted.hsla())
                            .child(place(&self.path, &self.lines)),
                    ),
            )
            .child(targets)
            .child(
                div().px(px(18.)).pt(px(12.)).child(
                    div()
                        .id("ask-input")
                        .test_support()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(12.))
                        .bg(p.panel.hsla())
                        .border_1()
                        .border_color(p.border.hsla())
                        .child(self.input.clone()),
                ),
            )
            .children(error)
            .child(
                div()
                    .px(px(18.))
                    .pt(px(12.))
                    .pb(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(m.text_sm)
                            .text_color(p.text_subtle.hsla())
                            .child("The code goes along. Enter sends, Esc cancels."),
                    )
                    .child(div().flex_1())
                    .child(
                        raised(div(), cx)
                            .id("ask-cancel")
                            .test_support()
                            .px(px(14.))
                            .py(px(8.))
                            .rounded(px(10.))
                            .text_size(m.text_sm)
                            .cursor_pointer()
                            .child("Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(AskEvent::Cancel))),
                    )
                    .child(
                        primary(div(), cx)
                            .id("ask-send")
                            .test_support()
                            .px(px(18.))
                            .py(px(8.))
                            .rounded(px(10.))
                            .text_size(m.text_sm)
                            .cursor_pointer()
                            .when(empty || self.sending, |d| d.opacity(0.55))
                            .child(if self.sending { "Asking…" } else { "Ask" })
                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_place_is_one_based() {
        assert_eq!(place("src/cart.rs", &(5..7)), "src/cart.rs:6–7");
        assert_eq!(place("src/cart.rs", &(0..1)), "src/cart.rs:1");
    }
}
