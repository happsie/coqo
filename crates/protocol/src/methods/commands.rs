// SPDX-License-Identifier: Apache-2.0

//! `commands/*`: the commands mods contribute (point `commands`), for the
//! UI's command palette and keybindings (issue #25).
//!
//! Mods run in the daemon, so their command handlers do too. The UI lists
//! them with [`List`] (`commands/list`), shows them in the palette next to
//! its own commands, binds their keys, and runs one with [`Run`]
//! (`commands/run`).
//!
//! **Access:** the daemon token only. Agents don't run UI commands.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::method::Method;

/// A key sequence a mod binds to its command (point `keybindings`), in the
/// mod manifest's syntax: chords separated by spaces, each chord modifiers
/// and a key joined by `+` (`mod+shift+h`, `mod+k mod+s`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CommandKeybinding {
    /// The keys on every platform, unless `mac` overrides them.
    pub key: String,
    /// The keys on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

/// A mod-contributed command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CommandInfo {
    /// The command id, such as `hello.greet`. Unique across mods.
    pub id: String,
    /// The title shown in the palette.
    pub title: String,
    /// The palette group, such as `Git`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// When the command is available, as the mod declared it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// The keys the mod binds to it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keybindings: Vec<CommandKeybinding>,
    /// The mod that contributes it.
    #[serde(rename = "mod")]
    pub provider: String,
}

/// Result of [`List`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ListResult {
    /// The commands, by id.
    pub commands: Vec<CommandInfo>,
}

/// Params of [`Run`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RunParams {
    /// The command id.
    pub id: String,
    /// Its arguments (`null` when none).
    #[serde(default)]
    pub args: Value,
}

/// Result of [`Run`]: what the handler returned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RunResult {
    /// The handler's result (`null` when it has none).
    #[serde(default)]
    pub value: Value,
}

/// `commands/list`: the commands mods contribute.
#[derive(Debug)]
pub enum List {}
impl Method for List {
    const NAME: &'static str = "commands/list";
    type Params = ();
    type Result = ListResult;
}

/// `commands/run`: run a mod's command. A command no mod handles is
/// [`UNKNOWN_COMMAND`](super::codes::UNKNOWN_COMMAND); a handler that fails
/// is [`COMMAND_FAILED`](super::codes::COMMAND_FAILED).
#[derive(Debug)]
pub enum Run {}
impl Method for Run {
    const NAME: &'static str = "commands/run";
    type Params = RunParams;
    type Result = RunResult;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn wire_shapes() {
        let info = CommandInfo {
            id: "hello.greet".into(),
            title: "Say hello".into(),
            category: Some("Hello".into()),
            when: None,
            keybindings: vec![CommandKeybinding {
                key: "mod+alt+h".into(),
                mac: None,
            }],
            provider: "coqo.hello".into(),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({
                "id": "hello.greet",
                "title": "Say hello",
                "category": "Hello",
                "keybindings": [{"key": "mod+alt+h"}],
                "mod": "coqo.hello",
            })
        );
        let run: RunParams = serde_json::from_value(json!({"id": "hello.greet"})).unwrap();
        assert_eq!(run.args, Value::Null);
    }
}
