// SPDX-License-Identifier: Apache-2.0

//! The user's settings file, `settings.toml` in Coqo's config directory
//! (PLAN §8, next to `keybindings.toml`).
//!
//! Until the settings service (#26) owns it, the UI reads the few keys it
//! has itself, once at start; unknown keys are left for later:
//!
//! ```toml
//! [files]
//! auto_reveal = true   # show the active file tab's file in the tree
//!
//! [questions]
//! findings_in_gutter = true   # `file:line` findings in agent messages
//!                             # become gutter notes (#345)
//! ```
//!
//! A palette command can flip a setting for the running app
//! ([`update`]); the file sets the value Coqo starts with.

use std::path::{Path, PathBuf};

use coqo_ui::gpui::{App, Global};

use crate::commands::keyfile;

/// The file's name in the config directory.
pub const FILE_NAME: &str = "settings.toml";

/// Where the user's settings file is.
pub fn path() -> PathBuf {
    keyfile::config_dir().join(FILE_NAME)
}

/// The settings the UI reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub files: FileSettings,
    pub questions: QuestionSettings,
}

impl Global for Settings {}

/// `[files]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSettings {
    /// Switching to a file tab reveals its file in the tree (expands its
    /// folders, selects and scrolls to its row) without moving the focus.
    /// Off by default.
    pub auto_reveal: bool,
}

/// `[questions]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionSettings {
    /// Findings an agent writes as text (`file:line - issue`) become
    /// dismissible gutter notes linked to their chat message (ADR 0030).
    /// On by default.
    pub findings_in_gutter: bool,
}

impl Default for QuestionSettings {
    fn default() -> Self {
        Self {
            findings_in_gutter: true,
        }
    }
}

/// Read the settings at `path`. A missing file is the defaults; what can't
/// be read or parsed is in the problems, and the rest still applies.
pub fn read(path: &Path) -> (Settings, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), Vec::new()),
        Err(e) => (
            Settings::default(),
            vec![format!("cannot read {}: {e}", path.display())],
        ),
    }
}

/// Parse a settings file.
pub fn parse(text: &str) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut problems = Vec::new();
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(e) => {
            problems.push(format!("{FILE_NAME} is not valid TOML: {}", e.message()));
            return (settings, problems);
        }
    };
    if let Some(files) = table.get("files") {
        match files {
            toml::Value::Table(files) => {
                if let Some(value) = files.get("auto_reveal") {
                    match value.as_bool() {
                        Some(on) => settings.files.auto_reveal = on,
                        None => problems.push("`files.auto_reveal`: expected true or false".into()),
                    }
                }
            }
            _ => problems.push("`files`: expected a table, like [files]".into()),
        }
    }
    if let Some(questions) = table.get("questions") {
        match questions {
            toml::Value::Table(questions) => {
                if let Some(value) = questions.get("findings_in_gutter") {
                    match value.as_bool() {
                        Some(on) => settings.questions.findings_in_gutter = on,
                        None => problems
                            .push("`questions.findings_in_gutter`: expected true or false".into()),
                    }
                }
            }
            _ => problems.push("`questions`: expected a table, like [questions]".into()),
        }
    }
    (settings, problems)
}

/// Read the settings at `path` into the app, and log what was wrong.
pub fn load(path: &Path, cx: &mut App) {
    let (settings, problems) = read(path);
    for p in problems {
        eprintln!("coqo: settings: {p}");
    }
    cx.set_global(settings);
}

/// The app's settings (the defaults before [`load`]).
pub fn get(cx: &App) -> Settings {
    cx.try_global::<Settings>().cloned().unwrap_or_default()
}

/// Change the running app's settings; observers of [`Settings`] hear it.
pub fn update(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    let mut settings = get(cx);
    f(&mut settings);
    cx.set_global(settings);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_keys_it_knows() {
        assert_eq!(parse(""), (Settings::default(), vec![]));
        let (s, problems) = parse("[files]\nauto_reveal = true\n[editor]\nfont = 'x'\n");
        assert!(s.files.auto_reveal);
        assert!(problems.is_empty());
        let (s, problems) = parse("[files]\nauto_reveal = 'yes'\n");
        assert!(!s.files.auto_reveal);
        assert_eq!(problems, ["`files.auto_reveal`: expected true or false"]);
        let (_, problems) = parse("files = 3\n");
        assert_eq!(problems.len(), 1);
        let (s, problems) = parse("[files\n");
        assert_eq!(s, Settings::default());
        assert!(problems[0].contains("not valid TOML"), "{problems:?}");
    }

    #[test]
    fn findings_in_the_gutter_are_on_unless_turned_off() {
        assert!(parse("").0.questions.findings_in_gutter);
        let off = parse("[questions]\nfindings_in_gutter = false\n").0;
        assert!(!off.questions.findings_in_gutter);
        let (s, problems) = parse("[questions]\nfindings_in_gutter = 'no'\n");
        assert!(s.questions.findings_in_gutter);
        assert_eq!(
            problems,
            ["`questions.findings_in_gutter`: expected true or false"]
        );
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let (s, problems) = read(&dir.path().join(FILE_NAME));
        assert_eq!(s, Settings::default());
        assert!(problems.is_empty());
        std::fs::write(dir.path().join(FILE_NAME), "[files]\nauto_reveal = true\n").unwrap();
        assert!(read(&dir.path().join(FILE_NAME)).0.files.auto_reveal);
    }
}
