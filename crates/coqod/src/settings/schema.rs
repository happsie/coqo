// SPDX-License-Identifier: Apache-2.0

//! The settings schema: what mods contribute to the `settings` and
//! `settings_sections` points, grouped into the settings page's sections,
//! and the check of a file's values against it.

use std::collections::BTreeMap;

use coqo_kernel::ContributionRegistry;
use coqo_mod_api::contrib::{Setting, SettingScope, SettingType, SettingsSection};
use coqo_mod_api::points;
use coqo_protocol::methods::settings::{
    FieldInfo, FieldKind, FieldScope, FileSettings, SectionInfo, SettingsProblem,
};
use serde_json::Value;

use super::file::RawValues;

/// The schema in effect.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    /// The sections, in display order.
    pub sections: Vec<SectionInfo>,
    /// Every field, by key.
    fields: BTreeMap<String, Setting>,
}

/// "claude" → "Claude", "lang-web" → "Lang web".
fn title_of(id: &str) -> String {
    let text = id.replace(['-', '_'], " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn field_info(s: &Setting) -> FieldInfo {
    FieldInfo {
        key: s.key.to_string(),
        title: s.title_or_key().to_owned(),
        description: s.description.clone(),
        kind: match s.ty {
            SettingType::Boolean => FieldKind::Boolean,
            SettingType::String => FieldKind::String,
            SettingType::Integer => FieldKind::Integer,
            SettingType::Number => FieldKind::Number,
            SettingType::Enum => FieldKind::Enum,
            SettingType::StringList => FieldKind::StringList,
        },
        default: s.default_value(),
        values: s.values.clone(),
        minimum: s.minimum,
        maximum: s.maximum,
        scope: match s.scope {
            SettingScope::Workspace => FieldScope::Workspace,
            SettingScope::User => FieldScope::User,
        },
    }
}

impl Schema {
    /// The schema from `sections` and `settings`, each with the mod that
    /// contributes it. A setting goes to the section whose id is the
    /// longest prefix of its key; one without gets a section named after the
    /// key's first part.
    pub fn build(
        sections: Vec<(SettingsSection, String)>,
        settings: Vec<(Setting, String)>,
    ) -> Self {
        struct Building {
            info: SectionInfo,
        }
        let mut by_id: BTreeMap<String, Building> = sections
            .into_iter()
            .map(|(s, provider)| {
                let id = s.id.to_string();
                let building = Building {
                    info: SectionInfo {
                        id: id.clone(),
                        title: s.title,
                        description: s.description,
                        order: s.order,
                        provider,
                        fields: Vec::new(),
                    },
                };
                (id, building)
            })
            .collect();
        let mut fields = BTreeMap::new();
        for (setting, provider) in settings {
            let key = setting.key.as_str();
            let home = by_id
                .keys()
                .filter(|id| {
                    key.strip_prefix(id.as_str())
                        .is_some_and(|r| r.starts_with('.'))
                })
                .max_by_key(|id| id.len())
                .cloned();
            let home = home.unwrap_or_else(|| {
                let first = key.split('.').next().unwrap_or(key).to_owned();
                by_id.entry(first.clone()).or_insert_with(|| Building {
                    info: SectionInfo {
                        id: first.clone(),
                        title: title_of(&first),
                        description: None,
                        order: None,
                        provider,
                        fields: Vec::new(),
                    },
                });
                first
            });
            if let Some(section) = by_id.get_mut(&home) {
                section.info.fields.push(field_info(&setting));
            }
            fields.insert(key.to_owned(), setting);
        }
        let mut list: Vec<Building> = by_id
            .into_values()
            .filter(|b| !b.info.fields.is_empty())
            .collect();
        list.sort_by(|a, b| {
            let rank = |s: &Building| (s.info.order.is_none(), s.info.order.unwrap_or(0));
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.info.title.cmp(&b.info.title))
        });
        Self {
            sections: list.into_iter().map(|b| b.info).collect(),
            fields,
        }
    }

    /// The schema the mods in `registry` contribute.
    pub fn from_registry(registry: &ContributionRegistry) -> Self {
        let sections = registry
            .contributions_of::<points::SettingsSections>()
            .inspect_err(|e| tracing::warn!(error = %e, "cannot list settings sections"))
            .unwrap_or_default()
            .into_iter()
            .map(|c| ((*c.value).clone(), c.provider.to_string()))
            .collect();
        let settings = registry
            .contributions_of::<points::Settings>()
            .inspect_err(|e| tracing::warn!(error = %e, "cannot list settings"))
            .unwrap_or_default()
            .into_iter()
            .map(|c| ((*c.value).clone(), c.provider.to_string()))
            .collect();
        Self::build(sections, settings)
    }

    /// The field with `key`.
    pub fn field(&self, key: &str) -> Option<&Setting> {
        self.fields.get(key)
    }

    /// Check a file's `raw` values: the valid ones, and a problem for each
    /// other one. In a workspace file, user-only settings are problems too.
    pub fn check(
        &self,
        raw: &RawValues,
        workspace_file: bool,
    ) -> (BTreeMap<String, Value>, Vec<SettingsProblem>) {
        let mut values = BTreeMap::new();
        let mut problems = Vec::new();
        for (key, raw) in raw {
            let problem = |message: String| SettingsProblem {
                key: Some(key.clone()),
                line: raw.line,
                message,
            };
            let Some(field) = self.field(key) else {
                problems.push(problem(format!("`{key}` is not a known setting")));
                continue;
            };
            if workspace_file && field.scope == SettingScope::User {
                problems.push(problem(format!(
                    "`{key}` can only be set in your user settings, not a workspace's"
                )));
                continue;
            }
            match field.check_value(&raw.value) {
                Ok(()) => {
                    values.insert(key.clone(), raw.value.clone());
                }
                Err(e) => problems.push(problem(format!("`{key}`: {e}"))),
            }
        }
        (values, problems)
    }

    /// [`check`](Self::check) into a [`FileSettings`] at `path`, with the
    /// file's own `syntax` problem first.
    pub fn file_settings(
        &self,
        path: String,
        exists: bool,
        raw: &RawValues,
        syntax: Option<&SettingsProblem>,
        workspace_file: bool,
    ) -> FileSettings {
        let (values, mut problems) = self.check(raw, workspace_file);
        if let Some(syntax) = syntax {
            problems.insert(0, syntax.clone());
        }
        FileSettings {
            path,
            exists,
            values,
            problems,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::file::parse;
    use super::*;

    fn id(s: &str) -> coqo_mod_api::ContributionId {
        s.parse().unwrap()
    }

    fn schema() -> Schema {
        Schema::build(
            vec![
                (
                    SettingsSection::new(id("files"), "Files").with_order(20),
                    "coqo.workbench".into(),
                ),
                (
                    SettingsSection::new(id("appearance"), "Appearance").with_order(10),
                    "coqo.workbench".into(),
                ),
                (SettingsSection::new(id("empty"), "Empty"), "x".into()),
            ],
            vec![
                (
                    Setting::boolean(id("files.auto_reveal"), "Reveal", false),
                    "coqo.workbench".into(),
                ),
                (
                    Setting::enumeration(
                        id("appearance.theme"),
                        "Theme",
                        ["system", "dark", "light"],
                        "system",
                    )
                    .user_only(),
                    "coqo.workbench".into(),
                ),
                (
                    Setting::boolean(id("claude.acp.plan-logins"), "Plan", true),
                    "coqo.claude".into(),
                ),
            ],
        )
    }

    #[test]
    fn groups_fields_into_ordered_sections() {
        let s = schema();
        let ids: Vec<_> = s.sections.iter().map(|s| s.id.as_str()).collect();
        // Ordered ones first; `claude` has no section of its own; `empty`
        // has no fields.
        assert_eq!(ids, ["appearance", "files", "claude"]);
        assert_eq!(s.sections[2].title, "Claude");
        assert_eq!(s.sections[2].provider, "coqo.claude");
        assert_eq!(s.sections[0].fields[0].scope, FieldScope::User);
        assert_eq!(s.sections[0].fields[0].default, json!("system"));
    }

    #[test]
    fn checks_values_with_readable_problems() {
        let s = schema();
        let raw = parse(
            "settings.toml",
            "[appearance]\ntheme = \"blue\"\n[files]\nauto_reveal = true\nauto_reveel = true\n",
        )
        .unwrap();
        let (values, problems) = s.check(&raw, false);
        assert_eq!(
            values,
            [("files.auto_reveal".to_owned(), json!(true))].into()
        );
        let messages: Vec<_> = problems
            .iter()
            .map(|p| (p.line, p.message.as_str()))
            .collect();
        assert_eq!(
            messages,
            [
                (
                    Some(2),
                    "`appearance.theme`: `blue` is not one of system, dark, light"
                ),
                (Some(5), "`files.auto_reveel` is not a known setting"),
            ]
        );
        let raw = parse("s", "[appearance]\ntheme = \"dark\"\n").unwrap();
        let (values, problems) = s.check(&raw, true);
        assert!(values.is_empty());
        assert!(
            problems[0]
                .message
                .contains("only be set in your user settings")
        );
    }
}
