// SPDX-License-Identifier: Apache-2.0

//! Commands, the command palette and keybindings (issue #25, PLAN §6
//! `workbench`). The visual language is in `docs/design/palette.md`, the
//! decisions in ADR 0023.
//!
//! - [`registry`]: every command (id, title, category, default keys, the
//!   key context it works in, the GPUI action it dispatches), the user's
//!   overrides and the recently used list. Views register their commands in
//!   their `init`; the registry binds the keys.
//! - [`palette`]: the palette view and its providers (modes).
//! - [`keyfile`]: the user's `keybindings.toml`.
//! - [`mods`]: the commands mods in the daemon contribute.
//!
//! `Ctrl/Cmd+Shift+P` opens the palette on the commands (`>`); `Ctrl/Cmd+K`
//! and a click on the top bar's command search open its default mode.

use std::{path::PathBuf, rc::Rc};

use coqo_ui::gpui::{self, App, SharedString, Task, Window};

pub mod keyfile;
pub mod mods;
pub mod palette;
pub mod registry;

pub use palette::{PaletteItem, PaletteProvider, PaletteScope};
pub use registry::{Command, CommandRegistry, Origin, register};

use palette::register_provider;

gpui::actions!(
    command_palette,
    [
        /// Open the palette on the commands (`>`).
        OpenCommands,
        /// Open the palette in its default mode (the top bar's search).
        OpenSearch,
        /// Read `keybindings.toml` again.
        ReloadKeybindings,
    ]
);

/// The commands mode's prefix.
pub const COMMANDS_PREFIX: &str = ">";

/// Command id of [`OpenCommands`].
pub const OPEN_COMMANDS: &str = "command_palette.open";
/// Command id of [`OpenSearch`].
pub const OPEN_SEARCH: &str = "command_palette.search";

#[cfg(target_os = "macos")]
const OPEN_COMMANDS_KEY: &str = "cmd-shift-p";
#[cfg(not(target_os = "macos"))]
const OPEN_COMMANDS_KEY: &str = "ctrl-shift-p";
#[cfg(target_os = "macos")]
const OPEN_SEARCH_KEY: &str = "cmd-k";
#[cfg(not(target_os = "macos"))]
const OPEN_SEARCH_KEY: &str = "ctrl-k";

/// Marks [`init`] as done.
struct Initialized;
impl gpui::Global for Initialized {}

/// Where the registry keeps its files. `None` keeps a list in memory only.
#[derive(Debug, Clone, Default)]
pub struct CommandFiles {
    /// The user's keybindings file ([`keyfile::path`]).
    pub keybindings: Option<PathBuf>,
    /// The recently used commands (under the UI's data directory).
    pub recent: Option<PathBuf>,
}

/// Register the palette's commands and the workbench's own, the commands
/// provider, and the actions that open the palette. Idempotent; called by
/// `shell::init`.
pub fn init(cx: &mut App) {
    if cx.has_global::<Initialized>() {
        return;
    }
    cx.set_global(Initialized);
    register_provider(cx, Rc::new(CommandsProvider));
    cx.on_action(|_: &OpenCommands, cx| open_in_active_window(COMMANDS_PREFIX, cx));
    cx.on_action(|_: &OpenSearch, cx| open_in_active_window("", cx));
    cx.on_action(|_: &ReloadKeybindings, cx| registry::reload(cx));
    let dock = |id: &str, title: &'static str, action: Box<dyn gpui::Action>, key: &'static str| {
        Command::new_boxed(
            SharedString::from(format!("dock.{id}")),
            title,
            "View",
            action,
        )
        .key(key)
        .context(coqo_ui::dock::KEY_CONTEXT)
    };
    let k = coqo_ui::dock::KEYS;
    register(
        cx,
        [
            Command::new(OPEN_COMMANDS, "Show all commands", "Palette", OpenCommands)
                .key(OPEN_COMMANDS_KEY)
                .hidden(),
            Command::new(OPEN_SEARCH, "Search", "Palette", OpenSearch)
                .key(OPEN_SEARCH_KEY)
                .hidden(),
            Command::new(
                "command_palette.reload_keybindings",
                "Reload keybindings",
                "Preferences",
                ReloadKeybindings,
            ),
            dock(
                "split_right",
                "Split right",
                Box::new(coqo_ui::dock::SplitRight),
                k[0],
            ),
            dock(
                "split_down",
                "Split down",
                Box::new(coqo_ui::dock::SplitDown),
                k[1],
            ),
            dock(
                "close_tab",
                "Close tab",
                Box::new(coqo_ui::dock::CloseTab),
                k[2],
            ),
            dock(
                "close_group",
                "Close group",
                Box::new(coqo_ui::dock::CloseGroup),
                k[3],
            ),
        ],
    );
    mods::init(cx);
}

/// Use `files`: read the user's keybindings and the recent list. Call once
/// at start; tests leave it out.
pub fn use_files(files: CommandFiles, cx: &mut App) {
    if let Some(recent) = files.recent {
        registry::registry(cx).set_recent_path(recent);
    }
    if let Some(keys) = files.keybindings {
        registry::load_user_keys(keys, cx);
    }
    // Without a daemon no more commands arrive: every id is known now.
    if crate::daemon::Daemon::global(cx).is_none() {
        registry::registry(cx).complete = true;
    }
    registry::report(cx);
}

/// How command `id`'s first key is shown (`Ctrl K`, `⌘K`), if it has one.
pub fn keys_label(id: &str, cx: &App) -> Option<SharedString> {
    let registry = cx.try_global::<CommandRegistry>()?;
    let keys = registry.keys_of(id).into_iter().next()?;
    Some(keyfile::display(&keys, cfg!(target_os = "macos")).into())
}

/// Run command `id` where `scope`'s focus was: give the focus back there and
/// dispatch the command's action. Remembers it as recently used.
pub fn run(id: &str, scope: &PaletteScope, window: &mut Window, cx: &mut App) {
    let Some(command) = cx.try_global::<CommandRegistry>().and_then(|r| r.get(id)) else {
        return;
    };
    let (id, action) = (command.id.clone(), command.action.boxed_clone());
    registry::registry(cx).touch(&id);
    if let Some(focus) = &scope.focus {
        window.focus(focus, cx);
    }
    window.dispatch_action(action, cx);
}

/// Whether the palette lists `command` for `scope`: it's a palette command,
/// its context matches where the focus was, and something there handles
/// its action (a mod's command always is: the app runs it in the daemon).
pub fn is_available(command: &Command, scope: &PaletteScope) -> bool {
    command.in_palette
        && command.matches_context(&scope.context)
        && (matches!(command.origin, Origin::Mod(_)) || scope.handles(command.action_type()))
}

/// Open the palette with `text` in the active window's workbench. The
/// workbench handles the actions itself while the focus is in it; this is
/// for when nothing in the window has the focus.
fn open_in_active_window(text: &'static str, cx: &mut App) {
    let Some(handle) = cx.active_window() else {
        return;
    };
    // The window may be dispatching this action right now.
    cx.defer(move |cx| {
        let Some(shell) = crate::shell::Shell::of_window(handle, cx) else {
            return;
        };
        handle
            .update(cx, |_, window, cx| {
                shell.update(cx, |shell, cx| shell.toggle_palette(text, window, cx))
            })
            .ok();
    });
}

/// The commands, ranked for `query`: with no query, the recently used
/// first, then the rest by category and title; with one, fuzzy matches on
/// "Category: Title", recently used ones a little ahead.
pub fn rank(query: &str, scope: &PaletteScope, cx: &App) -> Vec<PaletteItem> {
    let Some(registry) = cx.try_global::<CommandRegistry>() else {
        return Vec::new();
    };
    let mac = cfg!(target_os = "macos");
    let recent = registry.recent();
    let recency = |id: &SharedString| recent.iter().position(|r| r == id);
    let mut ranked: Vec<(i64, Option<usize>, PaletteItem)> = registry
        .commands()
        .iter()
        .filter(|c| is_available(c, scope))
        .filter_map(|c| {
            let label = format!("{}: {}", c.category, c.title);
            let (score, positions) = crate::chat::tab::composer::fuzzy(query, &label)?;
            let title_at = c.category.len() + 2;
            let mut item = PaletteItem::new(c.id.clone(), c.title.clone());
            item.prefix = Some(c.category.clone());
            item.highlights = positions
                .into_iter()
                .filter_map(|p| p.checked_sub(title_at))
                .collect();
            item.keys = registry
                .keys_of(&c.id)
                .first()
                .map(|k| keyfile::display(k, mac).into());
            let r = recency(&c.id);
            item.recent = r.is_some();
            let bonus = r.map_or(0, |r| 12 - r.min(12) as i64);
            Some((score + bonus, r, item))
        })
        .collect();
    if query.trim().is_empty() {
        ranked.sort_by(|a, b| match (a.1, b.1) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => (&a.2.prefix, &a.2.title).cmp(&(&b.2.prefix, &b.2.title)),
        });
    } else {
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| (&a.2.prefix, &a.2.title).cmp(&(&b.2.prefix, &b.2.title)))
        });
    }
    ranked.into_iter().map(|(_, _, item)| item).collect()
}

/// The commands mode (`>`), and the default mode until a file finder
/// claims it.
#[derive(Debug)]
pub struct CommandsProvider;

impl PaletteProvider for CommandsProvider {
    fn prefix(&self) -> &'static str {
        COMMANDS_PREFIX
    }

    fn placeholder(&self) -> SharedString {
        "Search commands".into()
    }

    fn empty_text(&self) -> SharedString {
        "No command matches here".into()
    }

    fn search(&self, query: &str, scope: &PaletteScope, cx: &mut App) -> Task<Vec<PaletteItem>> {
        Task::ready(rank(query, scope, cx))
    }

    fn confirm(&self, item: &PaletteItem, scope: &PaletteScope, window: &mut Window, cx: &mut App) {
        run(&item.id, scope, window, cx);
    }
}

#[cfg(test)]
mod tests;
