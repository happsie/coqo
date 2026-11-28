// SPDX-License-Identifier: Apache-2.0

//! Agent chat (PLAN §7.3, design board 1): the `chat` tab kind and the
//! sidebar's CHATS list, over the daemon's `sessions/*` methods.
//!
//! - [`tab::ChatTab`]: one session, keyed by its id, so it is restored with
//!   the layout and reattaches to the daemon (`sessions/get`, then
//!   `sessions/updates` from seq 0).
//! - [`list::ChatList`]: the current workspace's sessions for the sidebar,
//!   with a status dot each and the agent's status line ([`status_line`]).
//! - The session panel opens with the session's state, the agent's status
//!   line (`set_status`) and its progress.
//! - [`NewChat`] (`Ctrl/Cmd+Shift+N`, and the **+** next to CHATS): a session
//!   in the open workspace with the first available provider.
//!
//! A permission request shows as a card with one button per option, Enter
//! and Esc for "allow once" and "deny", and "remember for this workspace",
//! which stores a rule in the daemon; the session panel lists the rules
//! (`tab::permission`).
//!
//! Coqo never signs anyone in: when a provider needs a login, the chat says
//! how to sign in to the agent's own tool ([`SIGN_IN_HELP`]), with a button
//! that opens that tool in a terminal tab ([`RunInTerminal`]). A provider
//! that isn't set up (Claude Code without its ACP adapter, say) shows its
//! setup message under CHATS with its setup buttons; daemon-side ones run
//! through `agents/runAction`, only when pressed.
//!
//! The composer (`tab::composer`) has @-mentions of workspace files, the
//! session's slash commands and skills, the provider and mode dropdowns and
//! prompt history.

pub mod diff;
pub mod list;
pub mod tab;
pub mod transcript;

use coqo_protocol::methods::{
    agents::{
        AgentState, Create, CreateSessionParams, ProviderAction, ProviderActionRun, ProviderInfo,
        Providers, RunAction, RunActionParams, SessionId, SessionInfo,
    },
    codes as app_codes,
    editor::{Position, Range},
    terminal::{SpawnParams, TerminalId},
};
use coqo_ui::keys;
use coqo_ui::{
    ActiveTokens as _, Rgb,
    dock::{AnyTab, Dock, register_tab_kind},
    gpui::{
        Action, Animation, AnimationExt as _, AnyElement, App, AppContext as _, ElementId, Entity,
        IntoElement as _, Styled as _, WeakEntity, Window, pulsating_between,
    },
    widgets::status_dot,
};

use crate::{
    daemon::Daemon,
    files::editor::{self, FileEditor},
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
        /// A chat tab now shows another session (a provider switch): save
        /// the layout, since the tab's key changed.
        SessionSwitched,
        /// Allow the focused permission request once (Enter).
        AllowPermission,
        /// Deny the focused permission request once (Esc).
        RejectPermission,
    ]
);

/// Open a file a tool call or a criterion's evidence names (absolute path)
/// in a file tab, at `line` (0-based) if given, selecting through line
/// `end` (0-based, inclusive) if given too (a walkthrough step's lines).
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = chat, no_json)]
pub struct OpenLocation {
    pub path: String,
    pub line: Option<u32>,
    pub end: Option<u32>,
}

/// Open daemon terminal `id`, where an agent's command runs (ACP
/// `terminal/*`, #185), in a terminal tab.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = chat, no_json)]
pub struct OpenTerminal {
    pub id: u64,
}

/// Send `prompt` to agent session `session` from outside its chat (the
/// walkthrough player's "Ask <agent> to explain it", #264): its chat tab is
/// opened (or shown) with the prompt in the composer, and sent unless a
/// turn is running there.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = chat, no_json)]
pub struct AskAgent {
    pub session: String,
    pub prompt: String,
}

/// Run a provider's sign-in program (such as `claude`, to `/login` there) in
/// a terminal tab in the workspace.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = chat, no_json)]
pub struct RunInTerminal {
    pub program: String,
    pub args: Vec<String>,
}

#[cfg(target_os = "macos")]
const NEW_CHAT_KEY: &str = "cmd-shift-n";
#[cfg(not(target_os = "macos"))]
const NEW_CHAT_KEY: &str = "ctrl-shift-n";

/// Register the `chat` tab kind, the chat list and the keys. Call after the
/// daemon and workspaces are initialised.
pub fn init(cx: &mut App) {
    crate::commands::register(
        cx,
        [
            crate::commands::Command::new("chat.new", "New chat", "Chat", NewChat)
                .key(NEW_CHAT_KEY),
            crate::commands::Command::new("chat.cancel_turn", "Stop the agent", "Chat", CancelTurn)
                .key("escape")
                .context("ChatTab"),
        ],
    );
    cx.bind_keys([
        keys::binding("enter", AllowPermission, Some("ChatPermission")),
        keys::binding("escape", RejectPermission, Some("ChatPermission")),
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

/// A state's name, for labels: `Working`, `Needs input`, …
pub fn state_name(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "Idle",
        AgentState::Working => "Working",
        AgentState::NeedsInput => "Needs input",
        AgentState::NeedsPermission => "Needs permission",
        AgentState::Blocked => "Blocked",
        AgentState::Error => "Error",
        AgentState::Done => "Done",
    }
}

/// What a session is doing in one line, for the chat list and the rail's
/// tooltip: the agent's status with its progress (`Running the tests · 2 of
/// 5`), else why it needs the user.
pub fn status_line(info: &SessionInfo) -> Option<String> {
    if let Some(status) = &info.status {
        return Some(match status.progress {
            Some(p) => format!("{} · {} of {}", status.text, p.current, p.total),
            None => status.text.clone(),
        });
    }
    info.state_reason
        .clone()
        .filter(|r| info.state.needs_attention() && !r.is_empty())
}

pub(crate) fn state_color(state: AgentState, p: &coqo_ui::Palette) -> Rgb {
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

/// [`AskAgent`]: show the session's chat and ask there.
pub fn ask_agent(dock: &Entity<Dock>, action: &AskAgent, window: &mut Window, cx: &mut App) {
    let session = SessionId(action.session.clone());
    open_session(dock.downgrade(), session.clone(), window, cx);
    let tab = {
        let dock = dock.read(cx);
        dock.find_tab(KIND, &session.0, cx)
            .and_then(|id| dock.tab(id))
            .and_then(AnyTab::downcast::<tab::ChatTab>)
    };
    if let Some(tab) = tab {
        let prompt = action.prompt.clone();
        tab.update(cx, |t, cx| t.ask_agent(prompt, window, cx));
    }
}

fn list_error(error: Option<String>, cx: &mut App) {
    if let Some(list) = list::ChatList::global(cx) {
        list.update(cx, |l, cx| l.set_error(error, cx));
    }
}

/// Why a new chat can't start, with what the user can do about it.
struct Problem {
    message: String,
    provider: String,
    actions: Vec<ProviderAction>,
}

impl From<String> for Problem {
    fn from(message: String) -> Self {
        Self {
            message,
            provider: String::new(),
            actions: Vec::new(),
        }
    }
}

fn list_problem(problem: Problem, cx: &mut App) {
    if let Some(list) = list::ChatList::global(cx) {
        list.update(cx, |l, cx| {
            l.set_problem(problem.message, problem.provider, problem.actions, cx)
        });
    }
}

/// Why no provider is available: the first one that reports what is
/// missing, with its setup actions.
fn unavailable(providers: &[ProviderInfo]) -> Problem {
    let Some((p, status)) = providers
        .iter()
        .find_map(|p| Some((p, p.status.as_ref().filter(|s| !s.ready)?)))
    else {
        return "No agent provider is installed yet".to_owned().into();
    };
    Problem {
        message: status
            .message
            .clone()
            .unwrap_or_else(|| format!("{} isn't set up yet", p.name)),
        provider: p.id.clone(),
        actions: status.actions.clone(),
    }
}

/// The provider's sign-in button, as a list action.
fn sign_in_actions(provider: &ProviderInfo) -> Vec<ProviderAction> {
    provider
        .status
        .as_ref()
        .and_then(|s| s.sign_in.clone())
        .into_iter()
        .collect()
}

/// Press a provider's setup button from the CHATS list: open a terminal, or
/// run the daemon action and then start the chat.
pub fn press_action(
    dock: WeakEntity<Dock>,
    provider: String,
    action: ProviderAction,
    window: &mut Window,
    cx: &mut App,
) {
    let (program, args) = match action.run {
        ProviderActionRun::Terminal { program, args } => (program, args),
        ProviderActionRun::Daemon => {
            run_daemon_action(dock, provider, action, window, cx);
            return;
        }
    };
    window.dispatch_action(Box::new(RunInTerminal { program, args }), cx);
}

fn run_daemon_action(
    dock: WeakEntity<Dock>,
    provider: String,
    action: ProviderAction,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
        list_error(Some("coqod isn't connected".into()), cx);
        return;
    };
    let run = daemon.read(cx).request::<RunAction>(RunActionParams {
        provider: provider.clone(),
        action: action.id.clone(),
    });
    list_error(Some(format!("{}…", action.label)), cx);
    if let Some(list) = list::ChatList::global(cx) {
        list.update(cx, |l, cx| l.set_creating(true, cx));
    }
    window
        .spawn(cx, async move |cx| {
            let result = run.await;
            cx.update(|window, cx| {
                if let Some(list) = list::ChatList::global(cx) {
                    list.update(cx, |l, cx| l.set_creating(false, cx));
                }
                match result {
                    Ok(info) if info.available => {
                        list_error(None, cx);
                        new_chat(dock, window, cx);
                    }
                    Ok(info) => list_problem(unavailable(&[info]), cx),
                    Err(e) => list_problem(
                        Problem {
                            message: format!("{} failed: {e}", action.label),
                            provider,
                            actions: vec![action],
                        },
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
}

/// Run `action`'s program in a terminal tab in the window's workspace.
pub fn run_in_terminal(
    dock: &Entity<Dock>,
    action: &RunInTerminal,
    window: &mut Window,
    cx: &mut App,
) {
    let cwd = Workspaces::for_window(window, cx)
        .or_else(|| Workspaces::global(cx))
        .and_then(|w| w.read(cx).current()?.roots.first().map(|r| r.path.clone()));
    let title = std::path::Path::new(&action.program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let params = SpawnParams {
        program: Some(action.program.clone()),
        args: action.args.clone(),
        cwd,
        title,
        ..SpawnParams::default()
    };
    crate::terminal::open_with(dock, params, window, cx);
}

/// Show terminal `action.id` in its tab, opening one if none shows it.
pub fn open_terminal(
    dock: &Entity<Dock>,
    action: &OpenTerminal,
    window: &mut Window,
    cx: &mut App,
) {
    let key = format!("terminal:{}", action.id);
    if let Some(id) = dock.read(cx).find_tab(crate::terminal::KIND, &key, cx) {
        dock.update(cx, |dock, cx| dock.activate_tab(id, window, cx));
        return;
    }
    let Some(tab) = crate::terminal::tab(TerminalId(action.id), cx) else {
        return;
    };
    dock.update(cx, |dock, cx| {
        dock.open_tab(tab, window, cx);
    });
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
                let providers = providers.await.map_err(|e| Problem::from(e.to_string()))?;
                let provider = providers
                    .iter()
                    .find(|p| p.available)
                    .cloned()
                    .ok_or_else(|| unavailable(&providers))?;
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
                    .map_err(|e| Problem::from(e.to_string()))?;
                create.await.map_err(|e| {
                    if e.code() == Some(app_codes::AUTH_REQUIRED) {
                        Problem {
                            message: format!(
                                "{} needs you to sign in. {SIGN_IN_HELP}",
                                provider.name
                            ),
                            provider: provider.id.clone(),
                            actions: sign_in_actions(&provider),
                        }
                    } else {
                        format!("Couldn't start a chat: {e}").into()
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
                    Err(problem) => list_problem(problem, cx),
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
    let key = editor::key(&workspace.id, root, &path);
    let file = match dock.read(cx).find_tab(editor::KIND, &key, cx) {
        Some(id) => {
            dock.update(cx, |dock, cx| dock.activate_tab(id, window, cx));
            dock.read(cx)
                .tab(id)
                .and_then(AnyTab::downcast::<FileEditor>)
        }
        None => {
            let id = workspace.id.clone();
            let tab = cx.new(|cx| FileEditor::new(id, root, path, window, cx));
            dock.update(cx, |dock, cx| {
                dock.open_tab(AnyTab::new(tab.clone()), window, cx);
            });
            Some(tab)
        }
    };
    if let (Some(file), Some(line)) = (file, location.line) {
        let at = Position::new(line, 0);
        let to = location
            .end
            .filter(|end| *end >= line)
            .map_or(at, |end| Position::new(end + 1, 0));
        file.update(cx, |file, cx| {
            file.reveal(Some(Range::new(at, to)), window, cx)
        });
    }
}

#[cfg(test)]
mod tests;
