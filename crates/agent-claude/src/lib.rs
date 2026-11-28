// SPDX-License-Identifier: Apache-2.0

//! `coqo.claude`: Claude Code as an agent provider, through the Claude Agent
//! ACP adapter and the user's own login (PLAN §7.3, ADR 0005).
//!
//! - **The user's own Claude Code.** Coqo finds `claude` on `PATH` and runs
//!   the adapter with `CLAUDE_CODE_EXECUTABLE` set to it (symlinks
//!   resolved). It never modifies or wraps `claude`, never reads its
//!   credentials, never runs a login and never proxies its traffic. When
//!   Claude Code is signed out the session fails with `AUTH_REQUIRED`, and
//!   the provider's `sign_in` action opens a terminal running `claude` for
//!   the user to `/login` there.
//! - **The adapter** ([`ADAPTER_PACKAGE`] at the pinned [`ADAPTER_VERSION`])
//!   is installed only when the user asks (the provider's install action,
//!   `agents/runAction`), with `npm install --omit=optional` into
//!   `<data dir>/claude-acp`. Coqo bundles neither the adapter, the Agent
//!   SDK nor a `claude` binary, and `--omit=optional` skips the SDK's own
//!   `claude` binaries. It needs Node.js [`MIN_NODE_MAJOR`] or newer.
//! - **One adapter process per workspace**, run by the ACP host
//!   (`coqo-acp-host`) under the kernel supervisor, with the daemon's
//!   environment minus Claude Code session markers
//!   ([`launch::is_session_marker`]): coqod may itself run inside a Claude
//!   Code session, and its markers would break transcript saving and
//!   resume.
//! - **Plan logins:** the setting `claude.acp.plan-logins` ("Use Claude plan
//!   logins through ACP", default on). When off, the adapter runs with
//!   `--hide-claude-auth`.
//! - **Coqo operating guide:** [`guide`] (`guide.md`, versioned) is
//!   appended to Claude Code's own system prompt (ACP `systemPrompt`, mode
//!   `append`, never `override`) on every new and resumed session, if the
//!   adapter advertises `sessionCapabilities.systemPrompt`. It says when to
//!   use each `coqo-editor` tool. Claude Code keeps its preset, the user's
//!   CLAUDE.md and settings. The setting `claude.guide` (default on) turns
//!   it off.
//! - **Coqo's Claude Code plugin:** [`plugin`] (the `coqo-walkthrough`
//!   skill and `/coqo:walkthrough`) is written to
//!   `<data dir>/claude-plugins/coqo` and loaded into every session as a
//!   local plugin through the adapter's Agent SDK options
//!   (`_meta.claudeCode.options.plugins`). Nothing goes into the user's
//!   Claude Code configuration. The setting `claude.plugin` (default on)
//!   turns it off.
//! - **Billing badge, modes and commands** come from the adapter through the
//!   ACP host: auth status reduced to kind, label and plan (no email or
//!   organization), session modes, and the available slash commands and
//!   skills.
//!
//! This is a built-in core mod, but it lives in `crates/`, not `mods/`: it
//! needs the daemon's ACP host and agent runtime, and `mods/` crates may
//! depend only on `coqo-mod-api`.

pub mod detect;
pub mod launch;
pub mod plugin;
mod provider;

use std::sync::Arc;

use coqo_agents::register_provider;
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_mod_api::{BoxFuture, Manifest, Mod, ModContext, ModError};

pub use provider::{ClaudeOptions, ClaudeProvider, status};

/// The mod's `coqo.mod.toml`.
pub const MANIFEST: &str = include_str!("../coqo.mod.toml");

/// The provider id.
pub const PROVIDER_ID: &str = coqo_agents::model::CLAUDE_PROVIDER_ID;

/// The npm package of the Claude Agent ACP adapter.
pub const ADAPTER_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";

/// The adapter version Coqo installs and runs (ADR 0005 pins it).
pub const ADAPTER_VERSION: &str = "0.113.0";

/// The adapter's executable in `node_modules/.bin`.
pub const ADAPTER_BIN: &str = "claude-agent-acp";

/// The oldest Node.js major the adapter runs on.
pub const MIN_NODE_MAJOR: u32 = 22;

/// The daemon action that installs the adapter.
pub const INSTALL_ACTION: &str = "install-adapter";

/// The setting "Use Claude plan logins through ACP".
pub const PLAN_LOGINS_SETTING: &str = "claude.acp.plan-logins";

/// The setting "Append Coqo's operating guide to Claude's system prompt".
pub const GUIDE_SETTING: &str = "claude.guide";

/// The setting "Load Coqo's Claude Code plugin" (the walkthrough skill).
pub const PLUGIN_SETTING: &str = "claude.plugin";

/// The Coqo operating guide as kept in the repo (`guide.md`). Its first
/// line names the version; `<!-- -->` lines are notes for maintainers.
pub const GUIDE_SOURCE: &str = include_str!("../guide.md");

/// The guide's version, from its `(vN)` heading.
pub fn guide_version() -> u32 {
    GUIDE_SOURCE
        .lines()
        .next()
        .and_then(|l| l.rsplit_once("(v"))
        .and_then(|(_, v)| v.strip_suffix(')'))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The guide as sent to Claude: [`GUIDE_SOURCE`] without maintainer notes.
pub fn guide() -> String {
    GUIDE_SOURCE
        .lines()
        .filter(|l| !l.trim_start().starts_with("<!--"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// The parsed, validated [`MANIFEST`].
pub fn manifest() -> Manifest {
    Manifest::from_toml_str(MANIFEST).expect("coqo.claude's manifest is valid")
}

/// The provider's metadata in `agent_providers`.
pub fn provider_info() -> AgentProviderInfo {
    AgentProviderInfo {
        id: PROVIDER_ID.parse().expect("a valid id"),
        name: "Claude Code".into(),
        kind: AgentProviderKind::Acp,
        description: Some("Your own Claude Code and login, through the ACP adapter".into()),
    }
}

/// The mod: registers [`ClaudeProvider`] as `claude-acp`.
#[derive(Debug)]
pub struct ClaudeMod {
    options: ClaudeOptions,
}

impl ClaudeMod {
    /// The mod, with the provider's `options`.
    pub fn new(options: ClaudeOptions) -> Self {
        Self { options }
    }
}

impl Mod for ClaudeMod {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            let provider = ClaudeProvider::new(self.options.clone());
            register_provider(&cx, provider_info(), Arc::new(provider))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_mod_api::manifest::Tier;

    #[test]
    fn the_manifest_is_a_core_mod_with_the_plan_logins_setting() {
        let m = manifest();
        assert_eq!(m.id.as_str(), "coqo.claude");
        assert_eq!(m.tier, Tier::Core);
        assert!(MANIFEST.contains(PLAN_LOGINS_SETTING));
        assert!(MANIFEST.contains(PLUGIN_SETTING));
    }

    #[test]
    fn the_guide_is_versioned_short_and_names_every_core_tool() {
        assert!(guide_version() >= 1, "{GUIDE_SOURCE}");
        assert!(MANIFEST.contains(GUIDE_SETTING));
        let text = guide();
        assert!(!text.contains("<!--"), "{text}");
        // It costs context on every turn.
        let words = text.split_whitespace().count();
        assert!(words <= 260, "{words} words");
        for tool in coqo_protocol::methods::mcp::CORE_TOOLS {
            assert!(text.contains(tool), "the guide does not mention {tool}");
        }
    }

    #[test]
    fn the_guide_puts_the_two_trigger_rules_before_the_tool_list() {
        // The #216 evals: set_status and ask_question were skipped while
        // their schemas were deferred, so the guide leads with when to call
        // them (#238).
        assert!(guide_version() >= 3, "{GUIDE_SOURCE}");
        let text = guide();
        let list = text.find("\n- **").expect("a tool list");
        let (rules, _) = text.split_at(list);
        for tool in ["set_acceptance_criteria", "set_status", "ask_question"] {
            assert!(rules.contains(tool), "{tool} is not in the rules: {rules}");
        }
        assert!(rules.contains("several steps"), "{rules}");
        assert!(rules.contains("do not ask in chat"), "{rules}");
        assert!(rules.contains("mcp__coqo-editor__"), "{rules}");
    }
}
