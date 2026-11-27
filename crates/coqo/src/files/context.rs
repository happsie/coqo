// SPDX-License-Identifier: Apache-2.0

//! The editor context agents see (`editor/*`, PLAN §7.4, issue #70).
//!
//! - **Publishing.** Every window publishes `editor/context` for the
//!   workspace it shows: its file tabs (in tab order), which one is active
//!   (the displayed tab of the focused pane), and each tab's selections and
//!   visible lines. [`ContextPublisher`] (owned by the window's
//!   [`Shell`](crate::shell::Shell)) watches the dock, the file tabs and
//!   their editors, and publishes at most every [`PUBLISH_EVERY`], only when
//!   something changed. A window that switches workspaces or closes
//!   publishes `null` for the one it left; after a reconnect it publishes
//!   again.
//! - **Reveals.** [`RevealHub`] follows `editor/reveals` (once per
//!   connection) and hands each request to one window that shows its
//!   workspace (the active window if it does, else the first); that window
//!   opens the file and selects the range ([`FileEditor::reveal`]).
//!
//! Positions are 0-based lines and characters (Unicode scalar values).

use std::{collections::HashMap, ops::Range, time::Duration};

use coqo_buffers::Rope;
use coqo_protocol::methods::{
    editor::{
        self as edm, EditorTab, LineRange, MAX_SELECTION_TEXT, Position, PublishContextParams,
        RevealEvent, RevealsParams, Selection, WindowContext,
    },
    workspace::WorkspaceId,
};
use coqo_ui::{
    dock::{AnyTab, Dock},
    editor::CodeEditor,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global,
        Subscription, Task, Window,
    },
};
use ropey::LineType;

use super::editor::{self, FileEditor};
use crate::{
    daemon::{Daemon, DaemonEvent},
    workspace::Workspaces,
};

/// Publish at most this often (trailing: the last change is always sent).
pub const PUBLISH_EVERY: Duration = Duration::from_millis(120);

/// Lines are `\n`-separated; a `\r` before it is part of the line's text.
const LINES: LineType = LineType::LF;

// ---- positions ------------------------------------------------------------

/// The position of byte `offset` (clamped to the text, rounded down to a
/// character boundary).
pub fn position_of(text: &Rope, offset: usize) -> Position {
    let offset = text.floor_char_boundary(offset.min(text.len()));
    let line = text.byte_to_line_idx(offset, LINES);
    let start = text.line_to_byte_idx(line, LINES);
    let character = text.slice(start..offset).chars().count();
    Position::new(
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(character).unwrap_or(u32::MAX),
    )
}

/// The byte offset of `position`: a character past its line's end is the
/// end of the line (before the break), a line past the last line is the end
/// of the text.
pub fn offset_of(text: &Rope, position: Position) -> usize {
    let lines = text.len_lines(LINES);
    let line = position.line as usize;
    if line >= lines {
        return text.len();
    }
    let start = text.line_to_byte_idx(line, LINES);
    let end = if line + 1 < lines {
        text.line_to_byte_idx(line + 1, LINES)
    } else {
        text.len()
    };
    let mut offset = start;
    for (taken, ch) in text.slice(start..end).chars().enumerate() {
        if taken >= position.character as usize || ch == '\n' || ch == '\r' {
            break;
        }
        offset += ch.len_utf8();
    }
    offset
}

/// A selection or cursor of `text`, with the selected text (cut at
/// [`MAX_SELECTION_TEXT`] bytes on a character boundary).
pub fn selection_of(text: &Rope, range: Range<usize>) -> Selection {
    let (start, end) = (range.start.min(range.end), range.start.max(range.end));
    let start = text.floor_char_boundary(start.min(text.len()));
    let end = text.floor_char_boundary(end.min(text.len()));
    let mut selection = Selection {
        range: edm::Range::new(position_of(text, start), position_of(text, end)),
        text: None,
        truncated: false,
    };
    if start < end {
        let mut selected = String::new();
        for ch in text.slice(start..end).chars() {
            if selected.len() + ch.len_utf8() > MAX_SELECTION_TEXT {
                selection.truncated = true;
                break;
            }
            selected.push(ch);
        }
        selection.text = Some(selected);
    }
    selection
}

/// The UTF-8 byte range of `range` in `text`.
pub fn byte_range(text: &Rope, range: &edm::Range) -> Range<usize> {
    let start = offset_of(text, range.start);
    let end = offset_of(text, range.end).max(start);
    start..end
}

// ---- publishing -----------------------------------------------------------

/// What one file tab contributes to the context, if it has an editor:
/// selections (primary first) and the visible lines.
pub fn tab_view(
    text: &Rope,
    code: &CodeEditor,
    displayed: bool,
    cx: &App,
) -> (Vec<Selection>, Option<LineRange>) {
    let selections = code
        .selections(cx)
        .into_iter()
        .map(|r| selection_of(text, r))
        .collect();
    let visible = displayed
        .then(|| code.visible_lines(cx))
        .flatten()
        .filter(|lines| !lines.is_empty())
        .map(|lines| LineRange {
            start: u32::try_from(lines.start).unwrap_or(u32::MAX),
            end: u32::try_from(lines.end - 1).unwrap_or(u32::MAX),
        });
    (selections, visible)
}

/// The file tabs of `dock` in `workspace`, in tab order.
pub fn window_context(
    dock: &Dock,
    workspace: &WorkspaceId,
    focused: bool,
    cx: &App,
) -> WindowContext {
    let active = dock.active_tab();
    let mut tabs = Vec::new();
    for group in dock.tree().groups() {
        let displayed = group.active_tab();
        for state in &group.tabs {
            let Some(file) = dock.tab(state.id).and_then(AnyTab::downcast::<FileEditor>) else {
                continue;
            };
            let file = file.read(cx);
            if file.workspace() != workspace {
                continue;
            }
            let (selections, visible) = file
                .text_and_editor()
                .map(|(text, code)| tab_view(text, code.read(cx), displayed == Some(state.id), cx))
                .unwrap_or_default();
            tabs.push(EditorTab {
                root: file.root(),
                path: file.path().to_owned(),
                active: active == Some(state.id),
                dirty: file.is_dirty(),
                selections,
                visible,
            });
        }
    }
    WindowContext { focused, tabs }
}

/// What a window last published.
type Published = (WorkspaceId, WindowContext);

/// Publishes one window's editor context. The window's
/// [`Shell`](crate::shell::Shell) calls [`schedule`](Self::schedule) on
/// every change that may matter and [`publish`](Self::publish) when the
/// throttle fires.
pub struct ContextPublisher {
    /// The window, as the daemon knows it: unique per connection.
    window: String,
    focused: bool,
    published: Option<Published>,
    pending: Option<Task<()>>,
    /// The file tabs and editors watched, by tab entity.
    watched: HashMap<EntityId, (Option<EntityId>, Vec<Subscription>)>,
}

impl std::fmt::Debug for ContextPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextPublisher")
            .field("window", &self.window)
            .field("focused", &self.focused)
            .field("published", &self.published.as_ref().map(|(ws, _)| ws))
            .finish_non_exhaustive()
    }
}

/// What owns a [`ContextPublisher`]: it says what to publish.
pub trait PublishContext: Sized + 'static {
    /// The publisher.
    fn publisher(&mut self) -> &mut ContextPublisher;
    /// The window's dock and the workspace it shows.
    fn shown(&self, cx: &App) -> Option<(Entity<Dock>, WorkspaceId)>;
}

impl ContextPublisher {
    /// The publisher of `window`.
    pub fn new(window: &Window) -> Self {
        Self {
            window: format!("w-{}", window.window_handle().window_id().as_u64()),
            focused: window.is_window_active(),
            published: None,
            pending: None,
            watched: HashMap::new(),
        }
    }

    /// The window gained or lost the focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// The connection is new: what was published is gone.
    pub fn reconnected(&mut self) {
        self.published = None;
    }

    /// Publish soon (at most every [`PUBLISH_EVERY`]).
    pub fn schedule<T: PublishContext>(owner: &mut T, cx: &mut Context<T>) {
        if owner.publisher().pending.is_some() {
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PUBLISH_EVERY).await;
            this.update(cx, |this, cx| {
                this.publisher().pending = None;
                Self::publish(this, cx);
            })
            .ok();
        });
        owner.publisher().pending = Some(task);
    }

    /// Publish now, if anything changed.
    pub fn publish<T: PublishContext>(owner: &mut T, cx: &mut Context<T>) {
        let shown = owner.shown(cx);
        let focused = owner.publisher().focused;
        let next = shown
            .as_ref()
            .map(|(dock, ws)| (ws.clone(), window_context(dock.read(cx), ws, focused, cx)));
        if let Some((dock, _)) = &shown {
            Self::watch(owner, &dock.clone(), cx);
        }
        let this = owner.publisher();
        if next == this.published {
            return;
        }
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let daemon = daemon.read(cx);
        if let Some((left, _)) = this
            .published
            .as_ref()
            .filter(|(ws, _)| next.as_ref().is_none_or(|(n, _)| n != ws))
        {
            // Best effort: the daemon forgets it with the connection too.
            daemon
                .notify::<edm::PublishContext>(PublishContextParams {
                    workspace: left.clone(),
                    window: this.window.clone(),
                    context: None,
                })
                .ok();
        }
        this.published = match next {
            Some((workspace, context)) => daemon
                .notify::<edm::PublishContext>(PublishContextParams {
                    workspace: workspace.clone(),
                    window: this.window.clone(),
                    context: Some(context.clone()),
                })
                .is_ok()
                .then_some((workspace, context)),
            None => None,
        };
    }

    /// Withdraw the context: the window is closing.
    pub fn withdraw(&mut self, cx: &App) {
        self.pending = None;
        let (Some((workspace, _)), Some(daemon)) = (self.published.take(), Daemon::global(cx))
        else {
            return;
        };
        daemon
            .read(cx)
            .notify::<edm::PublishContext>(PublishContextParams {
                workspace,
                window: self.window.clone(),
                context: None,
            })
            .ok();
    }

    /// Watch every file tab of `dock` and its editor, and stop watching
    /// closed ones.
    fn watch<T: PublishContext>(owner: &mut T, dock: &Entity<Dock>, cx: &mut Context<T>) {
        let files: Vec<Entity<FileEditor>> = {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == editor::KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
                .collect()
        };
        let mut watched = std::mem::take(&mut owner.publisher().watched);
        watched.retain(|id, _| files.iter().any(|f| f.entity_id() == *id));
        for file in files {
            let code = file.read(cx).editor().cloned();
            let code_id = code.as_ref().map(Entity::entity_id);
            if watched
                .get(&file.entity_id())
                .is_some_and(|(seen, _)| *seen == code_id)
            {
                continue;
            }
            let mut subscriptions = vec![cx.observe(&file, |this, _, cx| Self::schedule(this, cx))];
            if let Some(code) = &code {
                subscriptions.push(CodeEditor::observe_state(code, cx, |this, cx| {
                    Self::schedule(this, cx)
                }));
            }
            watched.insert(file.entity_id(), (code_id, subscriptions));
        }
        owner.publisher().watched = watched;
    }
}

// ---- reveals --------------------------------------------------------------

/// A reveal for one window.
#[derive(Debug, Clone)]
pub struct RevealRequest {
    /// The window that should show it.
    pub window: AnyWindowHandle,
    /// What to show.
    pub event: RevealEvent,
}

/// Follows `editor/reveals` and routes each request to a window.
pub struct RevealHub {
    _follow: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for RevealHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevealHub")
            .field("following", &self._follow.is_some())
            .finish()
    }
}

impl EventEmitter<RevealRequest> for RevealHub {}

struct GlobalRevealHub(Entity<RevealHub>);

impl Global for GlobalRevealHub {}

impl RevealHub {
    /// The app's hub, created on first use (once the daemon exists).
    pub fn global(cx: &mut App) -> Option<Entity<Self>> {
        if let Some(hub) = cx.try_global::<GlobalRevealHub>() {
            return Some(hub.0.clone());
        }
        let daemon = Daemon::global(cx)?;
        let hub = cx.new(|cx| {
            let mut hub = Self {
                _follow: None,
                _subscriptions: vec![cx.subscribe(&daemon, |this: &mut Self, _, event, cx| {
                    if *event == DaemonEvent::Connected {
                        this.follow(cx);
                    }
                })],
            };
            hub.follow(cx);
            hub
        });
        cx.set_global(GlobalRevealHub(hub.clone()));
        Some(hub)
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let subscribe = daemon
            .read(cx)
            .subscribe::<edm::Reveals>(RevealsParams::default());
        self._follow = Some(cx.spawn(async move |this, cx| {
            let Ok(mut events) = subscribe.await else {
                return;
            };
            while let Some(Ok(event)) = events.next().await {
                let routed = this.update(cx, |_, cx| {
                    if let Some(window) = window_for(&event.workspace, cx) {
                        cx.emit(RevealRequest { window, event });
                    }
                });
                if routed.is_err() {
                    break;
                }
            }
        }));
    }
}

/// The window to show a reveal in `workspace`: the active window if it shows
/// it, else the first that does.
fn window_for(workspace: &WorkspaceId, cx: &App) -> Option<AnyWindowHandle> {
    let showing = Workspaces::windows_showing(workspace, cx);
    let active = cx.active_window();
    showing
        .iter()
        .find(|w| Some(**w) == active)
        .or_else(|| showing.first())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_characters_per_line() {
        let text = Rope::from_str("fn é() {\n\tx = \"日本\";\n}");
        assert_eq!(position_of(&text, 0), Position::new(0, 0));
        // `é` is two bytes, one character.
        assert_eq!(position_of(&text, 5), Position::new(0, 4));
        // Inside a multi-byte character: rounded down.
        assert_eq!(position_of(&text, 4), Position::new(0, 3));
        assert_eq!(position_of(&text, 9), Position::new(0, 8));
        assert_eq!(position_of(&text, 10), Position::new(1, 0));
        let nihon = text.slice(..).to_string().find('日').unwrap();
        assert_eq!(position_of(&text, nihon), Position::new(1, 6));
        assert_eq!(position_of(&text, 1000), Position::new(2, 1));

        assert_eq!(offset_of(&text, Position::new(0, 4)), 5);
        assert_eq!(offset_of(&text, Position::new(1, 6)), nihon);
        // Past the end of a line: its end, before the break.
        assert_eq!(offset_of(&text, Position::new(0, 99)), 9);
        assert_eq!(offset_of(&text, Position::new(99, 0)), text.len());
        assert_eq!(offset_of(&text, Position::new(2, 5)), text.len());
        for offset in [0, 5, 9, 10, nihon, text.len()] {
            assert_eq!(offset_of(&text, position_of(&text, offset)), offset);
        }
    }

    #[test]
    fn selections_carry_their_text_up_to_the_cap() {
        let text = Rope::from_str("alpha\nbeta\n");
        let s = selection_of(&text, 6..10);
        assert_eq!(
            s.range,
            edm::Range::new(Position::new(1, 0), Position::new(1, 4))
        );
        assert_eq!(s.text.as_deref(), Some("beta"));
        assert!(!s.truncated);
        // A cursor has no text; a reversed range is normalised.
        assert_eq!(selection_of(&text, 3..3).text, None);
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = 10..6;
        assert_eq!(selection_of(&text, reversed).text.as_deref(), Some("beta"));

        let long = Rope::from_str(&"é".repeat(MAX_SELECTION_TEXT));
        let s = selection_of(&long, 0..long.len());
        assert!(s.truncated);
        let cut = s.text.unwrap();
        assert!(cut.len() <= MAX_SELECTION_TEXT && cut.len() > MAX_SELECTION_TEXT - 2);
        assert!(cut.chars().all(|c| c == 'é'));
    }
}
