// SPDX-License-Identifier: Apache-2.0

//! `commands/*`: the commands mods contribute (point `commands`), for the
//! UI's command palette and keybindings (issue #25).
//!
//! `commands/list` returns every contributed command with the keys mods
//! bind to it (point `keybindings`); `commands/run` runs one through its
//! handler, activating the mod first if it waits for `command:<id>`.
//!
//! **Access:** the daemon token only: agents don't run the user's commands.

use std::collections::BTreeMap;

use coqo_kernel::id::ContributionId;
use coqo_mod_api::{ModError, points};
use coqo_protocol::methods::codes::{COMMAND_FAILED, UNKNOWN_COMMAND};
use coqo_protocol::methods::commands::{
    self as cmdm, CommandInfo, CommandKeybinding, ListResult, RunParams, RunResult,
};
use coqo_protocol::{Access, ErrorObject, Router};

use crate::mods::Mods;

/// Every contributed command, by id, with the keys mods bind to it.
pub fn list(mods: &Mods) -> ListResult {
    let commands = match mods.contributions.contributions_of::<points::Commands>() {
        Ok(commands) => commands,
        Err(e) => {
            tracing::warn!(error = %e, "cannot list commands");
            return ListResult {
                commands: Vec::new(),
            };
        }
    };
    let mut keys: BTreeMap<String, Vec<CommandKeybinding>> = BTreeMap::new();
    match mods.contributions.contributions_of::<points::Keybindings>() {
        Ok(bindings) => {
            for b in bindings {
                keys.entry(b.value.command.to_string())
                    .or_default()
                    .push(CommandKeybinding {
                        key: b.value.key.clone(),
                        mac: b.value.mac.clone(),
                    });
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot list keybindings"),
    }
    let mut commands: Vec<CommandInfo> = commands
        .into_iter()
        .map(|c| {
            let id = c.value.id.to_string();
            CommandInfo {
                keybindings: keys.remove(&id).unwrap_or_default(),
                id,
                title: c.value.title.clone(),
                category: c.value.category.clone(),
                when: c.value.when.clone(),
                provider: c.provider.to_string(),
            }
        })
        .collect();
    commands.sort_by(|a, b| a.id.cmp(&b.id));
    ListResult { commands }
}

/// Run command `p.id` with `p.args`.
pub async fn run(mods: &Mods, p: RunParams) -> Result<RunResult, ErrorObject> {
    let id: ContributionId =
        p.id.parse()
            .map_err(|_| ErrorObject::new(UNKNOWN_COMMAND, format!("no command `{}`", p.id)))?;
    tracing::debug!(command = %id, "commands/run");
    match mods.manager.run_command(&id, p.args).await {
        Ok(value) => Ok(RunResult { value }),
        Err(ModError::UnknownCommand(_)) => Err(ErrorObject::new(
            UNKNOWN_COMMAND,
            format!("no mod handles command `{id}`"),
        )),
        Err(e) => Err(ErrorObject::new(COMMAND_FAILED, e.to_string())),
    }
}

/// Register `commands/*` on `router`.
pub fn register(router: Router, mods: Mods) -> Router {
    let listing = mods.clone();
    router
        .method_with::<cmdm::List, _, _>(Access::daemon(), move |_, ()| {
            std::future::ready(Ok(list(&listing)))
        })
        .method_with::<cmdm::Run, _, _>(Access::daemon(), move |_, p| {
            let mods = mods.clone();
            async move { run(&mods, p).await }
        })
}

#[cfg(test)]
mod tests {
    use coqo_mod_api::serde_json::{Value, json};

    use super::*;
    use crate::mods::tests::test_builtins;

    #[tokio::test]
    async fn lists_and_runs_the_hello_command() {
        let dir = tempfile::tempdir().unwrap();
        let mods = Mods::load(test_builtins(dir.path()));
        mods.start().await;

        let listed = list(&mods);
        let greet = listed
            .commands
            .iter()
            .find(|c| c.id == coqo_mod_hello::GREET)
            .expect("hello.greet is listed");
        assert_eq!(greet.title, "Say hello");
        assert_eq!(greet.category.as_deref(), Some("Hello"));
        assert_eq!(greet.provider, "coqo.hello");

        let ran = run(
            &mods,
            RunParams {
                id: coqo_mod_hello::GREET.into(),
                args: json!({ "name": "Ada" }),
            },
        )
        .await
        .unwrap();
        assert_eq!(ran.value, json!("Hello, Ada!"));

        let unknown = run(
            &mods,
            RunParams {
                id: "nobody.nothing".into(),
                args: Value::Null,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(unknown.code, UNKNOWN_COMMAND);
        let bad = run(
            &mods,
            RunParams {
                id: "Not An Id".into(),
                args: Value::Null,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad.code, UNKNOWN_COMMAND);
        mods.shutdown().await;
    }
}
