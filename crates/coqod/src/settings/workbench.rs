// SPDX-License-Identifier: Apache-2.0

//! `coqo.workbench`: the settings of the UI's own features (PLAN §6
//! `workbench`), registered through the mod API like any mod's.
//!
//! The UI runs these features, but the daemon owns the settings files and
//! checks them against the schema, so their sections are contributed here.
//!
//! **Adding a core feature's settings** is one
//! [`register_settings`](coqo_mod_api::ModContext::register_settings) call in
//! [`WorkbenchSettings::activate`]: a section (id = the TOML table, title,
//! description, order) and its typed fields. A feature that is a mod of its
//! own makes the same call in its own `activate`, or declares
//! `[[contributes.settings_sections]]` and `[[contributes.settings]]` in its
//! `coqo.mod.toml`. The UI reads the values with
//! `coqo::settings::get(cx).bool(KEY)` and friends.

use coqo_mod_api::contrib::{Setting, SettingsSection};
use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError};

/// The mod's manifest.
pub const MANIFEST: &str = r#"
id = "coqo.workbench"
name = "Workbench"
version = "0.1.0"
description = "Settings of the workbench's own features: the theme, the file tree, questions and notes, updates."
engine = "^0.0"
tier = "core"
activation = ["startup"]
"#;

/// `appearance.theme`: `system`, `dark` or `light`.
pub const THEME: &str = "appearance.theme";
/// `files.auto_reveal`: switching to a file tab reveals it in the tree.
pub const AUTO_REVEAL: &str = "files.auto_reveal";
/// `questions.findings_in_gutter`: an agent's `file:line` findings become
/// gutter notes (ADR 0030).
pub const FINDINGS_IN_GUTTER: &str = "questions.findings_in_gutter";
/// `updates.check`: the daemon checks GitHub Releases (ADR 0031).
pub const UPDATES_CHECK: &str = "updates.check";
/// `updates.channel`: `stable`, or `pre` for pre-releases too.
pub const UPDATES_CHANNEL: &str = "updates.channel";

/// The workbench's settings sections.
#[derive(Debug, Default)]
pub struct WorkbenchSettings;

impl Mod for WorkbenchSettings {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            cx.register_settings(
                SettingsSection::new("appearance".parse()?, "Appearance")
                    .with_description("How Coqo looks.")
                    .with_order(10),
                [Setting::enumeration(
                    THEME.parse()?,
                    "Theme",
                    ["system", "dark", "light"],
                    "system",
                )
                .with_description("System follows your operating system's light or dark mode.")
                .user_only()],
            )?;
            cx.register_settings(
                SettingsSection::new("files".parse()?, "Files")
                    .with_description("The file tree.")
                    .with_order(20),
                [Setting::boolean(
                    AUTO_REVEAL.parse()?,
                    "Reveal the active file in the tree",
                    false,
                )
                .with_description(
                    "Switching to a file tab expands its folders in the tree and selects it, \
                     without moving the focus.",
                )],
            )?;
            cx.register_settings(
                SettingsSection::new("questions".parse()?, "Questions and notes")
                    .with_description("What agents pin to your code.")
                    .with_order(25),
                [Setting::boolean(
                    FINDINGS_IN_GUTTER.parse()?,
                    "Show findings in the gutter",
                    true,
                )
                .with_description(
                    "Findings an agent writes in a message (file:line, then the issue) become \
                     notes in the gutter of that line, linked to the message.",
                )],
            )?;
            // Read by the updater itself before every check (ADR 0031);
            // registered here so the page shows and checks them.
            cx.register_settings(
                SettingsSection::new("updates".parse()?, "Updates")
                    .with_description("Coqo checks GitHub Releases for new versions.")
                    .with_order(40),
                [
                    Setting::boolean(UPDATES_CHECK.parse()?, "Check for updates", true)
                        .with_description("Off: Coqo never contacts GitHub for updates.")
                        .user_only(),
                    Setting::enumeration(
                        UPDATES_CHANNEL.parse()?,
                        "Channel",
                        ["stable", "pre"],
                        "stable",
                    )
                    .with_description("Pre also offers pre-releases.")
                    .user_only(),
                ],
            )?;
            Ok(())
        })
    }
}
