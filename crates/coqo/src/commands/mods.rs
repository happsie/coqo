// SPDX-License-Identifier: Apache-2.0

//! The commands mods contribute. Mods run in the daemon, so the UI lists
//! their commands (`commands/list`) each time a connection comes up, adds
//! them to the registry with the keys the mods bind (and the user's
//! overrides), and runs one with `commands/run`.
//!
//! A mod command's `when` is read as a key context predicate
//! (`DiffReview`, `FileEditor`): where the command works and its keys are
//! bound.

use coqo_protocol::methods::commands::{self as cmdm, CommandInfo, RunParams};
#[allow(unused_imports)]
use coqo_ui::gpui; // The action derive spells its paths `gpui::…`.
use coqo_ui::gpui::{
    Action, App, AppContext as _, Context, Entity, Global, KeyBindingContextPredicate,
    SharedString, Subscription, Task,
};

use super::{Command, Origin, registry};
use crate::daemon::{Daemon, DaemonEvent};

/// Run mod command `id` in the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = command_palette, no_json)]
pub struct RunModCommand {
    pub id: SharedString,
}

/// What a mod command's run returned, or why it failed.
pub type Outcome = Result<serde_json::Value, String>;

/// Follows the daemon's commands. A GPUI global entity.
pub struct ModCommands {
    /// The last command run and what it returned.
    last: Option<(SharedString, Outcome)>,
    _fetch: Option<Task<()>>,
    _run: Option<Task<()>>,
    _daemon: Option<Subscription>,
}

impl std::fmt::Debug for ModCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModCommands")
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
}

struct GlobalModCommands(Entity<ModCommands>);
impl Global for GlobalModCommands {}

/// Follow the daemon's commands and handle [`RunModCommand`]. Called by
/// [`super::init`].
pub(super) fn init(cx: &mut App) {
    let entity = cx.new(|cx| {
        let mut this = ModCommands {
            last: None,
            _fetch: None,
            _run: None,
            _daemon: None,
        };
        if let Some(daemon) = Daemon::global(cx) {
            this._daemon = Some(cx.subscribe(
                &daemon,
                |this: &mut ModCommands, _, event: &DaemonEvent, cx| {
                    if *event == DaemonEvent::Connected {
                        this.fetch(cx);
                    }
                },
            ));
            this.fetch(cx);
        }
        this
    });
    cx.set_global(GlobalModCommands(entity));
    cx.on_action(|action: &RunModCommand, cx| {
        if let Some(mods) = ModCommands::global(cx) {
            mods.update(cx, |mods, cx| mods.run(action.id.clone(), cx));
        }
    });
}

impl ModCommands {
    /// The app's mod commands, once [`init`] ran.
    pub fn global(cx: &App) -> Option<Entity<ModCommands>> {
        cx.try_global::<GlobalModCommands>().map(|g| g.0.clone())
    }

    /// The last command run and what it returned.
    #[cfg(test)]
    pub fn last(&self) -> Option<&(SharedString, Outcome)> {
        self.last.as_ref()
    }

    #[cfg(test)]
    pub fn clear_last_for_tests(&mut self) {
        self.last = None;
    }

    /// List the daemon's commands and put them in the registry.
    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let listed = daemon.read(cx).request::<cmdm::List>(());
        self._fetch = Some(cx.spawn(async move |_, cx| {
            let result = listed.await;
            cx.update(|cx| match result {
                Ok(list) => {
                    set_commands(list.commands, cx);
                    registry::registry(cx).complete = true;
                    registry::report(cx);
                }
                Err(e) => eprintln!("coqo: cannot list the mods' commands: {e:?}"),
            });
        }));
    }

    fn run(&mut self, id: SharedString, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx) else {
            return;
        };
        let ran = daemon.read(cx).request::<cmdm::Run>(RunParams {
            id: id.to_string(),
            args: serde_json::Value::Null,
        });
        self._run = Some(cx.spawn(async move |this, cx| {
            let outcome = ran.await.map(|r| r.value).map_err(|e| format!("{e:?}"));
            if let Err(e) = &outcome {
                eprintln!("coqo: command `{id}` failed: {e}");
            }
            this.update(cx, |this, cx| {
                this.last = Some((id, outcome));
                cx.notify();
            })
            .ok();
        }));
    }
}

/// Replace the mods' commands in the registry with `commands`.
pub fn set_commands(commands: Vec<CommandInfo>, cx: &mut App) {
    let mac = cfg!(target_os = "macos");
    let registry = registry::registry(cx);
    registry.remove_where(|c| matches!(c.origin, Origin::Mod(_)));
    for info in commands {
        if registry.get(&info.id).is_some() {
            eprintln!(
                "coqo: mod `{}` contributes command `{}`, which Coqo already has; skipped",
                info.provider, info.id
            );
            continue;
        }
        let mut keys = Vec::new();
        for binding in &info.keybindings {
            let source = match (&binding.mac, mac) {
                (Some(m), true) => m,
                _ => &binding.key,
            };
            match super::keyfile::to_gpui(source, mac) {
                Ok(k) => keys.push(SharedString::from(k)),
                Err(e) => eprintln!("coqo: command `{}`: {e}", info.id),
            }
        }
        let context = info.when.filter(|w| {
            let ok = KeyBindingContextPredicate::parse(w).is_ok();
            if !ok {
                eprintln!(
                    "coqo: command `{}`: `when` `{w}` is not a key context",
                    info.id
                );
            }
            ok
        });
        let category = info.category.unwrap_or_else(|| info.provider.clone());
        registry.add(Command {
            id: info.id.clone().into(),
            title: info.title.into(),
            category: category.into(),
            keys,
            context: context.map(Into::into),
            action: Box::new(RunModCommand { id: info.id.into() }),
            in_palette: true,
            origin: Origin::Mod(info.provider.into()),
        });
    }
    registry::apply(cx);
}
