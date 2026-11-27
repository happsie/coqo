// SPDX-License-Identifier: Apache-2.0

//! Coqo's Claude Code plugin (`integrations/claude-code/coqo-plugin`): the
//! `coqo-walkthrough` skill and the `/coqo:walkthrough` command (PLAN §7.6,
//! #98).
//!
//! The plugin is compiled in. [`install`] writes it to
//! `<data dir>/claude-plugins/coqo` (Coqo's own directory, replaced whole
//! each time so only these files are in it), and every Claude session gets
//! it through the adapter's Agent SDK options ([`session_meta`]:
//! `_meta.claudeCode.options.plugins`, a local plugin, which Claude Code
//! loads with `--plugin-dir`). It is loaded for Coqo's sessions only: Coqo
//! never installs anything into the user's Claude Code configuration.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// The plugin's name (`.claude-plugin/plugin.json`), which namespaces its
/// skills and commands in Claude Code (`/coqo:walkthrough`).
pub const PLUGIN_NAME: &str = "coqo";

/// The skill that writes walkthroughs.
pub const WALKTHROUGH_SKILL: &str = "coqo-walkthrough";

/// The plugin's files: path inside the plugin, contents.
pub const PLUGIN_FILES: &[(&str, &str)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../../../integrations/claude-code/coqo-plugin/.claude-plugin/plugin.json"),
    ),
    (
        "skills/coqo-walkthrough/SKILL.md",
        include_str!(
            "../../../integrations/claude-code/coqo-plugin/skills/coqo-walkthrough/SKILL.md"
        ),
    ),
    (
        "commands/walkthrough.md",
        include_str!("../../../integrations/claude-code/coqo-plugin/commands/walkthrough.md"),
    ),
];

/// Where [`install`] puts the plugin under Coqo's data directory.
pub fn plugin_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("claude-plugins").join(PLUGIN_NAME)
}

/// Writes the plugin to `dir`, replacing whatever is there (a symlink is
/// removed, not followed), so the directory holds exactly
/// [`PLUGIN_FILES`].
pub fn install(dir: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(dir)?,
        Ok(_) => std::fs::remove_file(dir)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    for (path, contents) in PLUGIN_FILES {
        let file = dir.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, contents)?;
    }
    Ok(())
}

/// The ACP `_meta` that loads the plugin at `dir` into a Claude session:
/// the adapter passes `claudeCode.options` to the Agent SDK, whose
/// `plugins` option takes local plugin directories.
pub fn session_meta(dir: &Path) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert(
        "claudeCode".into(),
        json!({
            "options": {
                "plugins": [{"type": "local", "path": dir.display().to_string()}],
            },
        }),
    );
    meta
}

/// The YAML frontmatter of a skill or command file: its `key: value` lines
/// between the leading `---` lines (values unquoted). `None` without one.
pub fn frontmatter(text: &str) -> Option<Vec<(String, String)>> {
    let rest = text.strip_prefix("---\n")?;
    let (head, _) = rest.split_once("\n---\n")?;
    let mut fields = Vec::new();
    for line in head.lines() {
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        fields.push((key.trim().to_owned(), value.to_owned()));
    }
    Some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> &'static str {
        PLUGIN_FILES
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, c)| *c)
            .unwrap()
    }

    fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Claude Code's rule for plugin, skill and command names.
    fn kebab(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && !name.starts_with('-')
            && !name.ends_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    #[test]
    fn the_shipped_files_are_the_plugin_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../integrations/claude-code/coqo-plugin");
        let mut on_disk = Vec::new();
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    let rel = path.strip_prefix(&root).unwrap();
                    on_disk.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let mut shipped: Vec<String> = PLUGIN_FILES.iter().map(|(p, _)| (*p).into()).collect();
        shipped.sort();
        assert_eq!(
            on_disk, shipped,
            "PLUGIN_FILES must list the plugin's files"
        );
    }

    #[test]
    fn the_manifest_names_the_plugin() {
        let manifest: Value = serde_json::from_str(file(".claude-plugin/plugin.json")).unwrap();
        assert_eq!(manifest["name"], PLUGIN_NAME);
        assert!(kebab(PLUGIN_NAME));
        let version = manifest["version"].as_str().unwrap();
        assert_eq!(version.split('.').count(), 3, "{version}");
        assert!(version.split('.').all(|n| n.parse::<u32>().is_ok()));
        assert!(!manifest["description"].as_str().unwrap().is_empty());
        assert_eq!(manifest["license"], "Apache-2.0");
    }

    #[test]
    fn the_skill_has_a_precise_trigger_and_covers_the_schema() {
        let skill = file("skills/coqo-walkthrough/SKILL.md");
        let fields = frontmatter(skill).expect("frontmatter");
        assert_eq!(field(&fields, "name"), Some(WALKTHROUGH_SKILL));
        assert!(kebab(WALKTHROUGH_SKILL));
        let description = field(&fields, "description").unwrap();
        // Claude Code's limit, and what it decides from.
        assert!(description.len() <= 1024, "{} bytes", description.len());
        for words in ["publish_walkthrough", "walk through", "explain", "Not for"] {
            assert!(description.contains(words), "{words}: {description}");
        }
        // Every step kind and draft field the reader needs, by name.
        let schema: Value =
            serde_json::from_str(coqo_protocol::methods::walkthroughs::SCHEMA_JSON).unwrap();
        let defs = &schema["$defs"];
        let kinds = defs["StepKind"]["oneOf"].as_array().unwrap();
        assert!(kinds.len() >= 8);
        for kind in kinds {
            let kind = kind["const"].as_str().unwrap();
            assert!(skill.contains(&format!("`{kind}`")), "{kind}");
        }
        let mut fields: Vec<&str> = Vec::new();
        for def in ["StepDraft", "AnchorDraft", "TrivialHunkDraft"] {
            fields.extend(
                defs[def]["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|k| k.as_str()),
            );
        }
        for top in [
            "title",
            "summary",
            "scope",
            "steps",
            "trivial_hunks",
            "acceptance_criteria",
        ] {
            fields.push(top);
        }
        for f in fields {
            // `root` defaults to the scope's: the skill need not name it.
            if f != "root" {
                assert!(skill.contains(f), "the skill does not name `{f}`");
            }
        }
        assert!(skill.contains(coqo_protocol::methods::walkthroughs::SCHEMA_URI));
        // It is loaded into context when used: keep it tight.
        let words = skill.split_whitespace().count();
        assert!(words <= 900, "{words} words");
    }

    #[test]
    fn the_command_runs_the_skill() {
        let command = file("commands/walkthrough.md");
        let fields = frontmatter(command).expect("frontmatter");
        assert!(!field(&fields, "description").unwrap().is_empty());
        assert!(command.contains(WALKTHROUGH_SKILL));
        assert!(command.contains("$ARGUMENTS"));
    }

    #[test]
    fn frontmatter_needs_its_fences() {
        assert_eq!(frontmatter("no frontmatter"), None);
        assert_eq!(frontmatter("---\nname: x\n"), None);
        assert_eq!(
            frontmatter("---\nname: x\nhint: \"[a]\"\n---\nbody"),
            Some(vec![
                ("name".into(), "x".into()),
                ("hint".into(), "[a]".into())
            ])
        );
    }

    #[test]
    fn install_replaces_the_directory_and_the_meta_points_at_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = plugin_dir(tmp.path());
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(dir.join("hooks/hooks.json"), "{}").unwrap();
        install(&dir).unwrap();
        assert!(!dir.join("hooks").exists(), "stray files are removed");
        for (path, contents) in PLUGIN_FILES {
            assert_eq!(std::fs::read_to_string(dir.join(path)).unwrap(), *contents);
        }
        // Again, over itself.
        install(&dir).unwrap();

        let meta = Value::Object(session_meta(&dir));
        assert_eq!(
            meta["claudeCode"]["options"]["plugins"],
            json!([{"type": "local", "path": dir.display().to_string()}])
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_removes_a_symlink_instead_of_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep"), "x").unwrap();
        let dir = plugin_dir(tmp.path());
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        install(&dir).unwrap();
        assert!(elsewhere.join("keep").exists());
        assert!(!std::fs::symlink_metadata(&dir).unwrap().is_symlink());
        assert!(dir.join(".claude-plugin/plugin.json").is_file());
    }
}
