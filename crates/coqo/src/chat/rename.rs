// SPDX-License-Identifier: Apache-2.0

//! Renaming a chat in place (#172): a name field on the chat's tab label
//! (double-click it, or F2 and "Rename chat" in the palette while the chat
//! is focused) or on its CHATS row (double-click). Enter or a click
//! elsewhere saves the title with `sessions/rename`; Esc leaves it.
//!
//! One rename runs at a time, app-wide, kept by the [`ChatList`]; the field
//! shows only in the window it started in.

use coqo_protocol::methods::agents::{Rename, RenameParams, SessionId};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
        InteractiveElement as _, IntoElement as _, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _, Subscription, Window, WindowId, div,
        prelude::FluentBuilder as _, px,
    },
    input::{PromptInput, PromptInputEvent},
    widgets::TestSupportExt as _,
};

use super::list::ChatList;
use crate::daemon::Daemon;

/// Where a chat is being renamed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// Its tab label.
    Tab,
    /// Its row under CHATS.
    Row,
}

/// A rename in progress.
pub(crate) struct Renaming {
    pub session: SessionId,
    pub place: Place,
    window: WindowId,
    /// The title it had.
    before: String,
    pub input: Entity<PromptInput>,
    /// What had the focus, given back when the field closes.
    restore: Option<FocusHandle>,
    _events: Subscription,
}

impl ChatList {
    /// Open the name field for `session`, which is titled `title`, at
    /// `place` in `window`. Replaces a rename already open (saving it).
    /// The focus goes to `restore` when the field closes (by default, what
    /// has it now; a click on a tab label has just focused the dock, so the
    /// tab passes its composer).
    pub fn start_rename(
        &mut self,
        session: SessionId,
        title: String,
        place: Place,
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.finish_rename(true, window, cx);
        let restore = restore.or_else(|| window.focused(cx));
        let input = cx.new(|cx| {
            let mut input = PromptInput::new("Chat title", 1, 1, window, cx);
            input.set_text(title.clone(), window, cx);
            input
        });
        let events = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            PromptInputEvent::Submit => this.finish_rename(true, window, cx),
            PromptInputEvent::Cancel => this.finish_rename(false, window, cx),
            PromptInputEvent::Changed | PromptInputEvent::Key(_) => {}
        });
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.renaming = Some(Renaming {
            session,
            place,
            window: window.window_handle().window_id(),
            before: title,
            input,
            restore,
            _events: events,
        });
        cx.notify();
    }

    /// Close the name field; with `accept`, save a changed, non-empty title.
    pub fn finish_rename(&mut self, accept: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        cx.notify();
        if let Some(focus) = &renaming.restore {
            window.focus(focus, cx);
        }
        let title = renaming.input.read(cx).text(cx);
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if !accept || title.is_empty() || title == renaming.before {
            return;
        }
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            self.set_error(Some("coqod isn't connected".into()), cx);
            return;
        };
        let rename = daemon.read(cx).request::<Rename>(RenameParams {
            session: renaming.session,
            title,
        });
        cx.spawn(async move |this, cx| {
            let result = rename.await;
            this.update(cx, |this, cx| match result {
                Ok(info) => this.renamed(info, cx),
                Err(e) => this.set_error(Some(format!("Couldn't rename the chat: {e}")), cx),
            })
            .ok();
        })
        .detach();
    }

    /// The name field of `session` at `place` in `window`, while it is
    /// being renamed there.
    pub fn rename_field(
        &self,
        session: &SessionId,
        place: Place,
        window: &Window,
    ) -> Option<Entity<PromptInput>> {
        self.renaming
            .as_ref()
            .filter(|r| {
                &r.session == session
                    && r.place == place
                    && r.window == window.window_handle().window_id()
            })
            .map(|r| r.input.clone())
    }

    /// The session being renamed, if any (for tests).
    #[cfg(test)]
    pub fn renaming(&self) -> Option<(&SessionId, Place)> {
        self.renaming.as_ref().map(|r| (&r.session, r.place))
    }
}

/// Start renaming `session` (titled `title`) at `place`; see
/// [`ChatList::start_rename`].
pub fn start(
    session: SessionId,
    title: String,
    place: Place,
    restore: Option<FocusHandle>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(list) = ChatList::global(cx) {
        list.update(cx, |l, cx| {
            l.start_rename(session, title, place, restore, window, cx)
        });
    }
}

/// The name field, `width` wide (else as wide as it can be), as it shows on
/// a tab label or a row. A click inside stays there (it doesn't activate the
/// tab or open the chat); a click outside saves.
pub fn field(
    input: Entity<PromptInput>,
    id: &'static str,
    width: Option<f32>,
    cx: &App,
) -> AnyElement {
    let p = &cx.tokens().palette;
    div()
        .id(id)
        .test_support()
        .map(|d| match width {
            Some(w) => d.w(px(w)),
            None => d.w_full(),
        })
        .min_w_0()
        .h(px(22.))
        .flex()
        .items_center()
        .px(px(6.))
        .rounded(px(6.))
        .bg(p.panel_raised.hsla())
        .border_1()
        .border_color(p.blue.alpha(0.6))
        .overflow_hidden()
        .on_mouse_down(coqo_ui::gpui::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation()
        })
        .on_click(|_, _, cx| cx.stop_propagation())
        .on_mouse_down_out(|_, window, cx| {
            if let Some(list) = ChatList::global(cx) {
                list.update(cx, |l, cx| l.finish_rename(true, window, cx));
            }
        })
        .child(input)
        .into_any_element()
}
