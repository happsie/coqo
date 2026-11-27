// SPDX-License-Identifier: Apache-2.0

//! Agent chat (PLAN §7.3, design board 1): the `chat` tab kind and the
//! sidebar's CHATS list, over the daemon's `sessions/*` methods.
//!
//! - [`tab::ChatTab`]: one session, keyed by its id, so it is restored with
//!   the layout and reattaches to the daemon (`sessions/get`, then
//!   `sessions/updates` from seq 0).
//! - [`list::ChatList`]: the current workspace's sessions for the sidebar,
//!   with a status dot each.
//! - [`NewChat`] (`Ctrl/Cmd+Shift+N`, and the **+** next to CHATS): a session
//!   in the open workspace with the first available provider.
//!
//! Coqo never signs anyone in: when a provider needs a login, the chat says
//! how to sign in to the agent's own tool ([`SIGN_IN_HELP`]).
//!
//! Hooks for the composer work (#64: @-mentions, slash commands, the
//! provider and model picker) are the [`PromptInput`](coqo_ui::input::PromptInput)
//! in [`tab::ChatTab::composer`] and the transcript's `commands`.

pub mod diff;
pub mod list;
pub mod tab;
pub mod transcript;

use coqo_protocol::methods::{
    agents::{AgentState, Create, CreateSessionParams, Providers, SessionId},
    codes as app_codes,
};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    dock::{AnyTab, Dock, register_tab_kind},
    gpui::{
        Action, Animation, AnimationExt as _, AnyElement, App, AppContext as _, ElementId,
        IntoElement as _, KeyBinding, Styled as _, WeakEntity, Window, pulsating_between,
    },
    widgets::status_dot,
};

use crate::{
    daemon::Daemon,
    files::preview::{self, FilePreview},
    workspace::Workspaces,
};

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derives spell their paths `gpui::…`.

/// The tab kind. Persisted; never change it.
pub const KIND: &str = "chat";

/// What the chat says when a provider needs a login. Coqo never handles it.
pub const SIGN_IN_HELP: &str = "Sign in to the agent in its own tool: for Claude Code, open a \
terminal, run `claude` and use `/login`. Then send your message again. Coqo never sees or \
stores your login.";

coqo_ui::gpui::actions!(
    chat,
    [
        /// Start a chat in the open workspace.
        NewChat,
        /// Stop the chat's running turn.
        CancelTurn,
    ]
);

/// Open a file a tool call touched (absolute path) in a file tab.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = chat, no_json)]
pub struct OpenLocation {
    pub path: String,
    pub line: Option<u32>,
}

#[cfg(target_os = "macos")]
const NEW_CHAT_KEY: &str = "cmd-shift-n";
#[cfg(not(target_os = "macos"))]
const NEW_CHAT_KEY: &str = "ctrl-shift-n";

/// Register the `chat` tab kind, the chat list and the keys. Call after the
/// daemon and workspaces are initialised.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(NEW_CHAT_KEY, NewChat, None),
        KeyBinding::new("escape", CancelTurn, Some("ChatTab")),
    ]);
    register_tab_kind(cx, KIND, |state, window, cx| {
        tab::restore(state, window, cx).map(AnyTab::new)
    });
    if list::ChatList::global(cx).is_none() {
        list::init(cx);
    }
}

/// An agent state's status dot: sage when idle or done, sand when it needs
/// the user, rose on error, a pulsing blue while working.
pub fn agent_dot(state: AgentState, id: impl Into<ElementId>, cx: &App) -> AnyElement {
    let p = &cx.tokens().palette;
    let color = state_color(state, p);
    let dot = status_dot(color, false).size(coqo_ui::gpui::px(7.));
    if state == AgentState::Working {
        dot.with_animation(
            id,
            Animation::new(std::time::Duration::from_millis(1200))
                .repeat()
                .with_easing(pulsating_between(0.35, 1.)),
            |dot, delta| dot.opacity(delta),
        )
        .into_any_element()
    } else {
        dot.into_any_element()
    }
}

fn state_color(state: AgentState, p: &coqo_ui::Palette) -> Rgb {
    match state {
        AgentState::Idle | AgentState::Done => p.sage,
        AgentState::Working => p.blue,
        AgentState::NeedsInput | AgentState::NeedsPermission | AgentState::Blocked => p.sand,
        AgentState::Error => p.rose,
    }
}

/// The tab badge for a state that needs the user.
pub fn attention_label(state: AgentState, p: &coqo_ui::Palette) -> Option<(&'static str, Rgb)> {
    match state {
        AgentState::NeedsInput => Some(("needs input", p.sand)),
        AgentState::NeedsPermission => Some(("needs permission", p.sand)),
        AgentState::Blocked => Some(("blocked", p.sand)),
        AgentState::Error => Some(("error", p.rose)),
        _ => None,
    }
}

/// Open (or show) the chat tab of `session`.
pub fn open_session(dock: WeakEntity<Dock>, session: SessionId, window: &mut Window, cx: &mut App) {
    let Some(dock) = dock.upgrade() else { return };
    if let Some(id) = dock.read(cx).find_tab(KIND, &session.0, cx) {
        dock.update(cx, |dock, cx| dock.activate_tab(id, window, cx));
        return;
    }
    let tab = cx.new(|cx| tab::ChatTab::new(session, window, cx));
    dock.update(cx, |dock, cx| {
        dock.open_tab(AnyTab::new(tab), window, cx);
    });
}

fn list_error(error: Option<String>, cx: &mut App) {
    if let Some(list) = list::ChatList::global(cx) {
        list.update(cx, |l, cx| l.set_error(error, cx));
    }
}

/// Start a chat in the open workspace with the first available provider,
/// and open its tab. Failures show under CHATS in the sidebar.
pub fn new_chat(dock: WeakEntity<Dock>, window: &mut Window, cx: &mut App) {
    let workspace = Workspaces::for_window(window, cx)
        .or_else(|| Workspaces::global(cx))
        .and_then(|w| w.read(cx).current().cloned());
    let Some((workspace, root)) =
        workspace.and_then(|w| w.roots.first().map(|r| (w.id.clone(), r.path.clone())))
    else {
        list_error(Some("Open a folder to start a chat".into()), cx);
        return;
    };
    let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
        list_error(Some("coqod isn't connected".into()), cx);
        return;
    };
    list_error(None, cx);
    if let Some(list) = list::ChatList::global(cx) {
        list.update(cx, |l, cx| l.set_creating(true, cx));
    }
    let providers = daemon.read(cx).request::<Providers>(());
    window
        .spawn(cx, async move |cx| {
            let result = async {
                let providers = providers.await.map_err(|e| e.to_string())?;
                let provider = providers
                    .into_iter()
                    .find(|p| p.available)
                    .ok_or_else(|| "No agent provider is installed yet".to_owned())?;
                let create = cx
                    .update(|_, cx| {
                        daemon.read(cx).request::<Create>(CreateSessionParams {
                            provider: provider.id.clone(),
                            cwd: root,
                            workspace: Some(workspace.0),
                            title: None,
                            mode: None,
                        })
                    })
                    .map_err(|e| e.to_string())?;
                create.await.map_err(|e| {
                    if e.code() == Some(app_codes::AUTH_REQUIRED) {
                        format!("{} needs you to sign in. {SIGN_IN_HELP}", provider.name)
                    } else {
                        format!("Couldn't start a chat: {e}")
                    }
                })
            }
            .await;
            cx.update(|window, cx| {
                if let Some(list) = list::ChatList::global(cx) {
                    list.update(cx, |l, cx| l.set_creating(false, cx));
                }
                match result {
                    Ok(info) => {
                        let id = info.id.clone();
                        if let Some(list) = list::ChatList::global(cx) {
                            list.update(cx, |l, cx| l.upsert(info, cx));
                        }
                        open_session(dock, id, window, cx);
                    }
                    Err(error) => list_error(Some(error), cx),
                }
            })
            .ok();
        })
        .detach();
}

/// Open the file at an absolute `path` (from a tool call) in a file tab, if
/// it is inside one of the current workspace's roots.
pub fn open_location(
    dock: WeakEntity<Dock>,
    location: &OpenLocation,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(dock) = dock.upgrade() else { return };
    let workspaces = Workspaces::for_window(window, cx).or_else(|| Workspaces::global(cx));
    let Some(workspace) = workspaces
        .as_ref()
        .and_then(|w| w.read(cx).current().cloned())
    else {
        return;
    };
    let found = workspace.roots.iter().find_map(|root| {
        let rest = location.path.strip_prefix(root.path.as_str())?;
        let rel = rest.strip_prefix('/')?;
        (!rel.is_empty()).then(|| (root.id, rel.to_owned()))
    });
    let Some((root, path)) = found else {
        eprintln!("coqo: {} is outside the workspace", location.path);
        return;
    };
    let key = preview::key(&workspace.id, root, &path);
    if let Some(id) = dock.read(cx).find_tab(preview::KIND, &key, cx) {
        dock.update(cx, |dock, cx| dock.activate_tab(id, window, cx));
        return;
    }
    let id = workspace.id.clone();
    let tab = cx.new(|cx| FilePreview::new(workspaces.as_ref(), id, root, path, cx));
    dock.update(cx, |dock, cx| {
        dock.open_tab(AnyTab::new(tab), window, cx);
    });
}

#[cfg(test)]
mod tests;
