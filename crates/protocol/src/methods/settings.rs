// SPDX-License-Identifier: Apache-2.0

//! `settings/*`: the user's settings (issue #26, PLAN §8).
//!
//! The daemon owns the settings: the user's `settings.toml` in Coqo's config
//! directory, and a workspace's `.coqo/settings.toml` (in its first root)
//! over it. The schema is what mods contribute to the `settings` and
//! `settings_sections` points: [`Get`] returns it as [`SectionInfo`]s with
//! each file's valid values and problems, [`Set`] writes one value (keeping
//! the file's comments and order), and [`Changes`] says when anything
//! changed: a file edited by hand, a write, or a mod that added settings.
//!
//! A value in effect is the workspace file's, else the user file's, else the
//! field's default ([`GetResult::effective`]). Invalid values are left out of
//! [`FileSettings::values`] and reported in its problems; a file that is not
//! valid TOML keeps its last good values.
//!
//! **Access:** the daemon token only. Agents don't change the user's
//! settings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::workspace::WorkspaceId;
use crate::method::{Method, Topic};

/// What kind of value a field holds, and so how the settings page edits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// `true` or `false`: a switch.
    Boolean,
    /// Free text: a text field.
    String,
    /// A whole number: a text field.
    Integer,
    /// Any number: a text field.
    Number,
    /// One of [`FieldInfo::values`]: a choice.
    Enum,
    /// A list of strings: a text field, comma-separated.
    StringList,
}

/// Who may set a field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FieldScope {
    /// The user's file, and each workspace's over it.
    #[default]
    Workspace,
    /// The user's file only.
    User,
}

impl FieldScope {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// One setting of a section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FieldInfo {
    /// The full key, such as `files.auto_reveal`.
    pub key: String,
    /// Title shown on its row.
    pub title: String,
    /// What it does.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The kind of value.
    pub kind: FieldKind,
    /// The value when nobody set it.
    pub default: Value,
    /// The allowed values of an `enum`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Lower bound of a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    /// Upper bound of a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    /// Who may set it.
    #[serde(default, skip_serializing_if = "FieldScope::is_default")]
    pub scope: FieldScope,
}

/// A section of the settings page, with its fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SectionInfo {
    /// Section id, the key prefix of its fields (`files`).
    pub id: String,
    /// Title in the section list.
    pub title: String,
    /// A line under the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its position as the mod gave it (lowest first); the list is already
    /// in order, this places the UI's own pages among the sections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    /// The mod that contributes the section (the first field's, for a
    /// section no mod declared).
    #[serde(rename = "mod")]
    pub provider: String,
    /// Its fields, in the order they were contributed.
    pub fields: Vec<FieldInfo>,
}

/// Something wrong in a settings file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SettingsProblem {
    /// The setting it is about; absent for the whole file (not valid TOML,
    /// cannot be read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The 1-based line, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// What is wrong, for the user: "`appearance.theme`: `blue` is not one
    /// of system, dark, light".
    pub message: String,
}

/// One settings file: where it is, its valid values and its problems.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileSettings {
    /// The file's absolute path (it may not exist yet).
    pub path: String,
    /// Whether the file exists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exists: bool,
    /// The valid values it sets, by full key. While the file is not valid
    /// TOML, the values it had when it last was.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, Value>,
    /// What is wrong in it: invalid TOML, invalid values, unknown keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<SettingsProblem>,
}

/// A workspace's own settings file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WorkspaceSettings {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Its `.coqo/settings.toml`, in its first root.
    pub file: FileSettings,
}

/// Params of [`Get`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct GetParams {
    /// Workspaces whose own files to include too. The daemon follows each
    /// one asked for from then on, and [`Changes`] reports when it changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceId>,
}

/// Result of [`Get`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct GetResult {
    /// The settings page's sections, in display order.
    pub sections: Vec<SectionInfo>,
    /// The user's `settings.toml`.
    pub user: FileSettings,
    /// The workspaces asked for that are known, in the order asked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<WorkspaceSettings>,
    /// How many times the settings changed since the daemon started, as in
    /// [`Changes`].
    pub generation: u64,
}

impl GetResult {
    /// The field with `key`.
    pub fn field(&self, key: &str) -> Option<&FieldInfo> {
        self.sections
            .iter()
            .flat_map(|s| &s.fields)
            .find(|f| f.key == key)
    }

    /// `workspace`'s own file, if it was asked for.
    pub fn workspace(&self, workspace: &WorkspaceId) -> Option<&FileSettings> {
        self.workspaces
            .iter()
            .find(|w| &w.workspace == workspace)
            .map(|w| &w.file)
    }

    /// The value of `key` in effect in `workspace` (or for the user, with
    /// `None`): the workspace file's (unless the field is user-only), else
    /// the user file's, else the default. `None` for a key no section has.
    pub fn effective(&self, key: &str, workspace: Option<&WorkspaceId>) -> Option<Value> {
        let field = self.field(key)?;
        let own = workspace
            .filter(|_| field.scope == FieldScope::Workspace)
            .and_then(|w| self.workspace(w))
            .and_then(|f| f.values.get(key));
        Some(
            own.or_else(|| self.user.values.get(key))
                .cloned()
                .unwrap_or_else(|| field.default.clone()),
        )
    }
}

/// Params of [`Set`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SetParams {
    /// The full key, such as `files.auto_reveal`.
    pub key: String,
    /// The new value; `null` (or absent) removes the key from the file, so
    /// the next layer's value applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Write the workspace's `.coqo/settings.toml` instead of the user's
    /// file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// Result of [`Set`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SetResult {
    /// The generation after the write.
    pub generation: u64,
}

/// Params of [`Changes`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {}

/// One event of [`Changes`]: something changed; [`Get`] again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SettingsChanged {
    /// The new generation.
    pub generation: u64,
}

/// `settings/get`: the schema, the user's file and the workspaces' files.
#[derive(Debug)]
pub enum Get {}
impl Method for Get {
    const NAME: &'static str = "settings/get";
    type Params = GetParams;
    type Result = GetResult;
}

/// `settings/set`: write one value to the user's or a workspace's file,
/// keeping its comments and order. A key no section has is
/// [`UNKNOWN_SETTING`](super::codes::UNKNOWN_SETTING); a value of the wrong
/// type, or a user-only key in a workspace file, is
/// [`INVALID_SETTING`](super::codes::INVALID_SETTING); a file that is not
/// valid TOML is not overwritten:
/// [`SETTINGS_FILE_INVALID`](super::codes::SETTINGS_FILE_INVALID).
#[derive(Debug)]
pub enum Set {}
impl Method for Set {
    const NAME: &'static str = "settings/set";
    type Params = SetParams;
    type Result = SetResult;
}

/// `settings/changes`: the settings changed. Live only.
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "settings/changes";
    type Params = ChangesParams;
    type Event = SettingsChanged;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn snapshot() -> GetResult {
        let field = |key: &str, scope| FieldInfo {
            key: key.into(),
            title: key.into(),
            description: String::new(),
            kind: FieldKind::Boolean,
            default: json!(false),
            values: vec![],
            minimum: None,
            maximum: None,
            scope,
        };
        let ws = WorkspaceId("w1".into());
        GetResult {
            sections: vec![SectionInfo {
                id: "files".into(),
                title: "Files".into(),
                description: None,
                order: None,
                provider: "coqo.workbench".into(),
                fields: vec![
                    field("files.a", FieldScope::Workspace),
                    field("files.b", FieldScope::User),
                    field("files.c", FieldScope::Workspace),
                ],
            }],
            user: FileSettings {
                values: [
                    ("files.a".into(), json!(true)),
                    ("files.b".into(), json!(true)),
                ]
                .into(),
                ..FileSettings::default()
            },
            workspaces: vec![WorkspaceSettings {
                workspace: ws,
                file: FileSettings {
                    values: [
                        ("files.a".into(), json!(false)),
                        ("files.b".into(), json!(false)),
                    ]
                    .into(),
                    ..FileSettings::default()
                },
            }],
            generation: 1,
        }
    }

    #[test]
    fn effective_values_layer_workspace_over_user_over_default() {
        let s = snapshot();
        let ws = WorkspaceId("w1".into());
        assert_eq!(s.effective("files.a", None), Some(json!(true)));
        assert_eq!(s.effective("files.a", Some(&ws)), Some(json!(false)));
        // User-only: the workspace's value is ignored.
        assert_eq!(s.effective("files.b", Some(&ws)), Some(json!(true)));
        assert_eq!(s.effective("files.c", Some(&ws)), Some(json!(false)));
        assert_eq!(s.effective("files.nope", None), None);
        let other = WorkspaceId("w2".into());
        assert_eq!(s.effective("files.a", Some(&other)), Some(json!(true)));
    }

    #[test]
    fn wire_shapes() {
        let set: SetParams = serde_json::from_value(json!({"key": "files.auto_reveal"})).unwrap();
        assert_eq!(set.value, None);
        assert_eq!(
            serde_json::to_value(FieldInfo {
                key: "appearance.theme".into(),
                title: "Theme".into(),
                description: String::new(),
                kind: FieldKind::Enum,
                default: json!("system"),
                values: vec!["system".into(), "dark".into()],
                minimum: None,
                maximum: None,
                scope: FieldScope::User,
            })
            .unwrap(),
            json!({
                "key": "appearance.theme",
                "title": "Theme",
                "kind": "enum",
                "default": "system",
                "values": ["system", "dark"],
                "scope": "user",
            })
        );
    }
}
