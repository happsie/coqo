// SPDX-License-Identifier: Apache-2.0

//! `[updates]` in `settings.toml`, which the daemon reads before every
//! check (so a change applies without a restart):
//!
//! ```toml
//! [updates]
//! check = true        # false: never contact GitHub
//! channel = "stable"  # "pre": offer pre-releases too
//! ```

use std::path::Path;

use crate::Channel;

/// The update settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateSettings {
    /// Check for updates at all. On by default.
    pub check: bool,
    /// Which releases to offer.
    pub channel: Channel,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check: true,
            channel: Channel::Stable,
        }
    }
}

impl UpdateSettings {
    /// Read them from the settings file at `path`. A missing file is the
    /// defaults; problems are returned, and the rest still applies.
    pub fn read(path: &Path) -> (Self, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), Vec::new()),
            Err(e) => (
                Self::default(),
                vec![format!("cannot read {}: {e}", path.display())],
            ),
        }
    }

    /// Parse a settings file's `[updates]` table; other tables are not
    /// ours.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut settings = Self::default();
        let mut problems = Vec::new();
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(e) => {
                problems.push(format!("settings.toml is not valid TOML: {}", e.message()));
                return (settings, problems);
            }
        };
        let Some(updates) = table.get("updates") else {
            return (settings, problems);
        };
        let Some(updates) = updates.as_table() else {
            problems.push("`updates`: expected a table, like [updates]".into());
            return (settings, problems);
        };
        if let Some(check) = updates.get("check") {
            match check.as_bool() {
                Some(on) => settings.check = on,
                None => problems.push("`updates.check`: expected true or false".into()),
            }
        }
        if let Some(channel) = updates.get("channel") {
            match channel.as_str() {
                Some("stable") => settings.channel = Channel::Stable,
                Some("pre") => settings.channel = Channel::Pre,
                _ => problems.push("`updates.channel`: expected \"stable\" or \"pre\"".into()),
            }
        }
        (settings, problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_the_keys() {
        assert_eq!(
            UpdateSettings::parse(""),
            (UpdateSettings::default(), vec![])
        );
        let (s, p) = UpdateSettings::parse("[files]\nauto_reveal = true\n");
        assert!(s.check && s.channel == Channel::Stable && p.is_empty());
        let (s, p) = UpdateSettings::parse("[updates]\ncheck = false\nchannel = \"pre\"\n");
        assert!(!s.check && s.channel == Channel::Pre && p.is_empty());
        let (s, p) = UpdateSettings::parse("[updates]\ncheck = 'no'\nchannel = \"nightly\"\n");
        assert!(s.check, "a bad value keeps the default");
        assert_eq!(s.channel, Channel::Stable);
        assert_eq!(p.len(), 2);
        let (s, p) = UpdateSettings::parse("[updates\n");
        assert!(s.check && p.len() == 1);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            UpdateSettings::read(&dir.path().join("settings.toml")),
            (UpdateSettings::default(), vec![])
        );
    }
}
