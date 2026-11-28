//! Checking a `coqo.mod.toml` found in a repository, and reading a Coqo
//! marketplace index (`coqo.marketplace.toml`, proposed by ADR 0033).
//!
//! The manifest is validated by the real `coqo_mod_api::Manifest`. Two
//! things the ADR proposes are not in the API yet, so the prototype handles
//! them before handing the rest over:
//!
//! - `tier = "content"`: a mod with no code. It is validated as `core`
//!   (which also takes no `entry`) and then held to the content rules.
//! - `[[contributes.agent_plugins]]`: a folder in Claude Code plugin format
//!   (skills, commands, agents, MCP servers, hooks) passed to Coqo's Claude
//!   sessions. It is taken out before validation and analysed separately.

use coqo_mod_api::Manifest;
use coqo_mod_api::manifest::Tier;
use serde::Deserialize;

use crate::claude::{self, PluginSummary};
use crate::repo::{Kind, Repo};
use crate::source::clean_subdir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModTier {
    Core,
    Wasm,
    Process,
    Content,
}

pub struct CheckedMod {
    pub manifest: Manifest,
    pub tier: ModTier,
    pub agent_plugins: Vec<(String, PluginSummary)>,
    /// Problems that block the install.
    pub errors: Vec<String>,
    /// Things the install dialog would point out.
    pub notes: Vec<String>,
}

fn join(dir: &str, path: &str) -> String {
    if dir.is_empty() {
        path.to_owned()
    } else {
        format!("{dir}/{path}")
    }
}

/// Reads and checks `<dir>/coqo.mod.toml`.
pub fn check(repo: &dyn Repo, dir: &str) -> Result<CheckedMod, String> {
    let path = join(dir, coqo_mod_api::manifest::FILE_NAME);
    let text = repo.read(&path)?;
    let mut value: toml::Table = toml::from_str(&text).map_err(|e| format!("{path}: {e}"))?;

    let content = value.get("tier").and_then(|t| t.as_str()) == Some("content");
    if content {
        value.insert("tier".into(), "core".into());
    }
    let mut plugin_dirs = Vec::new();
    if let Some(toml::Value::Table(contributes)) = value.get_mut("contributes") {
        if let Some(toml::Value::Array(list)) = contributes.remove("agent_plugins") {
            for item in list {
                let p = item
                    .get("path")
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| format!("{path}: agent_plugins entry needs `path`"))?;
                plugin_dirs.push(p.to_owned());
            }
        }
    }
    let manifest = Manifest::from_toml_str(&toml::to_string(&value).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{path}:\n{e}"))?;

    let tier = match (manifest.tier, content) {
        (_, true) => ModTier::Content,
        (Tier::Core, false) => ModTier::Core,
        (Tier::Wasm, false) => ModTier::Wasm,
        (Tier::Process, false) => ModTier::Process,
    };
    let mut errors = Vec::new();
    let mut notes = Vec::new();

    if !manifest.supports_api(&coqo_mod_api::api_version()) {
        errors.push(format!(
            "needs Coqo mod API {}, this Coqo has {}",
            manifest.engine,
            coqo_mod_api::API_VERSION
        ));
    }
    match tier {
        ModTier::Core => errors.push(
            "core mods are Rust crates compiled into Coqo; they can't be installed from a repository"
                .into(),
        ),
        ModTier::Content => content_rules(&manifest, &mut errors),
        ModTier::Wasm => {
            let entry = manifest.entry.as_deref().unwrap_or_default();
            let at = join(dir, entry);
            if repo.exists(&at) {
                notes.push(format!(
                    "prebuilt component {entry} at the pinned commit; it runs sandboxed in the WASM host"
                ));
            } else {
                notes.push(format!(
                    "{entry} is not in the repository: it would come from a release asset \
                     ({}-{}.wasm, with its sha256 in the release). Coqo never builds it on install",
                    manifest.id, manifest.version
                ));
            }
        }
        ModTier::Process => {
            let entry = manifest.entry.as_deref().unwrap_or_default();
            if entry.contains('/') {
                let at = join(dir, entry);
                match repo.files().iter().find(|e| e.path == at) {
                    Some(e) if e.kind == Kind::Executable => notes.push(format!(
                        "{entry} is a committed executable for one platform only"
                    )),
                    Some(_) => notes.push(format!("{entry} is committed but not executable")),
                    None => notes.push(format!(
                        "{entry} is not in the repository: it would come from a per-platform release \
                         asset ({}-{}-<target>.tar.gz)",
                        manifest.id, manifest.version
                    )),
                }
            } else {
                notes.push(format!(
                    "runs `{entry}` from PATH: Coqo can't see or pin what that program is"
                ));
            }
        }
    }

    // What the folder carries beyond the manifest.
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    for e in repo.files().iter().filter(|e| e.path.starts_with(&prefix)) {
        match e.kind {
            Kind::Symlink => errors.push(format!("{} is a symlink (refused)", e.path)),
            Kind::Submodule => notes.push(format!("{} is a submodule (not fetched)", e.path)),
            Kind::Executable if tier == ModTier::Content => notes.push(format!(
                "{} is executable; installed without the x bit",
                e.path
            )),
            _ => {}
        }
    }

    let mut agent_plugins = Vec::new();
    for p in plugin_dirs {
        match clean_subdir(&p) {
            Ok(sub) => {
                let d = join(dir, sub.as_deref().unwrap_or(""));
                let summary = claude::summarise(repo, &d, None, true);
                agent_plugins.push((p, summary));
            }
            Err(e) => errors.push(format!("agent_plugins path {e}")),
        }
    }

    Ok(CheckedMod {
        manifest,
        tier,
        agent_plugins,
        errors,
        notes,
    })
}

/// A content mod runs nothing: no entry, no activation, no services, no
/// permissions, and only contributions that are data.
fn content_rules(m: &Manifest, errors: &mut Vec<String>) {
    if !m.activation.is_empty() {
        errors.push("content mods have no code to activate; remove `activation`".into());
    }
    if !m.provides.is_empty() || !m.consumes.is_empty() {
        errors.push("content mods neither provide nor consume services".into());
    }
    if !m.permissions.is_empty() {
        errors.push("content mods run nothing, so they ask for no permissions".into());
    }
    let c = &m.contributes;
    for (name, n) in [
        ("commands", c.commands.len()),
        ("mcp_tools", c.mcp_tools.len()),
        ("lsp_servers", c.lsp_servers.len()),
        ("debug_adapters", c.debug_adapters.len()),
        ("agent_providers", c.agent_providers.len()),
        ("context_providers", c.context_providers.len()),
        ("alert_sources", c.alert_sources.len()),
        ("slash_commands", c.slash_commands.len()),
    ] {
        if n > 0 {
            errors.push(format!(
                "`{name}` needs code behind it; a content mod can't contribute it \
                 (slash commands go in an agent plugin)"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// coqo.marketplace.toml
// ---------------------------------------------------------------------------

pub const MARKETPLACE_FILE: &str = "coqo.marketplace.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoqoMarketplace {
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)] // shown by a real install dialog
    pub description: Option<String>,
    #[serde(default)]
    pub mods: Vec<MarketplaceMod>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceMod {
    /// Must equal the mod's own manifest id.
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source: MarketplaceSource,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MarketplaceSource {
    /// A folder in the marketplace repository.
    Folder(String),
    /// Another repository, pinned.
    GitHub {
        github: String,
        #[serde(default, rename = "ref")]
        reference: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
}

pub fn read_marketplace(repo: &dyn Repo, dir: &str) -> Result<CoqoMarketplace, String> {
    let path = join(dir, MARKETPLACE_FILE);
    let m: CoqoMarketplace =
        toml::from_str(&repo.read(&path)?).map_err(|e| format!("{path}: {e}"))?;
    for entry in &m.mods {
        if let MarketplaceSource::Folder(f) = &entry.source {
            clean_subdir(f).map_err(|e| format!("{path}: mod {}: {e}", entry.id))?;
        }
        if let MarketplaceSource::GitHub { sha: Some(s), .. } = &entry.source {
            if s.len() != 40 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!(
                    "{path}: mod {}: `sha` must be a full commit SHA",
                    entry.id
                ));
            }
        }
    }
    Ok(m)
}
