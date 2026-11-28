//! Working out what a folder is and printing the dry-run install plan.

use std::fmt::Write as _;

use crate::claude::{self, PluginSummary};
use crate::coqo::{self, CheckedMod, MarketplaceSource, ModTier};
use crate::repo::Repo;

/// How much the user is asked to trust, from least to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trust {
    /// Data only: nothing runs.
    Content,
    /// Prompts for Claude: they run nothing themselves, and Claude Code's
    /// permission prompts still guard every tool.
    Prompts,
    /// Code in the WASM sandbox, limited to the granted permissions.
    Sandboxed,
    /// Programs with the user's own rights (process mods, MCP servers,
    /// hooks, or prompts that pre-approve tools).
    Unsandboxed,
}

impl Trust {
    fn label(self) -> &'static str {
        match self {
            Self::Content => "content only: nothing runs",
            Self::Prompts => {
                "prompts for Claude: nothing runs by itself; Claude's permission prompts still apply"
            }
            Self::Sandboxed => {
                "sandboxed code: runs in the WASM host with only the permissions below"
            }
            Self::Unsandboxed => "RUNS PROGRAMS AS YOU: not sandboxed; review before enabling",
        }
    }
}

pub struct Plan {
    pub found: String,
    pub id: Option<String>,
    pub version: Option<String>,
    pub trust: Trust,
    pub contributes: Vec<String>,
    pub permissions: Vec<String>,
    pub runs: Vec<String>,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
    /// For a marketplace: what it lists, and the plan stops there.
    pub listing: Vec<String>,
    /// The folder that would be installed.
    pub dir: String,
}

impl Plan {
    fn new(found: impl Into<String>) -> Self {
        Self {
            found: found.into(),
            id: None,
            version: None,
            trust: Trust::Content,
            contributes: Vec::new(),
            permissions: Vec::new(),
            runs: Vec::new(),
            notes: Vec::new(),
            errors: Vec::new(),
            listing: Vec::new(),
            dir: String::new(),
        }
    }
}

fn join(dir: &str, path: &str) -> String {
    if dir.is_empty() {
        path.to_owned()
    } else {
        format!("{dir}/{path}")
    }
}

/// Decides what `dir` holds and plans its install. `pick` chooses one entry
/// of a marketplace.
pub fn plan(repo: &dyn Repo, dir: &str, pick: Option<&str>, repo_name: &str) -> Plan {
    if repo.exists(&join(dir, coqo_mod_api::manifest::FILE_NAME)) {
        return match coqo::check(repo, dir) {
            Ok(m) => {
                let mut p = plan_mod(m);
                p.dir = dir.to_owned();
                p
            }
            Err(e) => {
                let mut p = Plan::new("a coqo.mod.toml that does not validate");
                p.errors.push(e);
                p
            }
        };
    }
    if repo.exists(&join(dir, coqo::MARKETPLACE_FILE)) {
        return match coqo::read_marketplace(repo, dir) {
            Ok(m) => {
                if let Some(id) = pick {
                    match m.mods.iter().find(|e| e.id == id) {
                        Some(e) => match &e.source {
                            MarketplaceSource::Folder(f) => {
                                let sub = crate::source::clean_subdir(f).ok().flatten();
                                let mut p = plan(
                                    repo,
                                    &join(dir, sub.as_deref().unwrap_or("")),
                                    None,
                                    repo_name,
                                );
                                if p.id.as_deref() != Some(id) {
                                    p.errors.push(format!(
                                        "the marketplace lists `{id}` but the folder's manifest says `{}`",
                                        p.id.as_deref().unwrap_or("?")
                                    ));
                                }
                                p.found =
                                    format!("{} (from Coqo marketplace `{}`)", p.found, m.name);
                                p
                            }
                            MarketplaceSource::GitHub {
                                github,
                                reference,
                                sha,
                            } => {
                                let mut p = Plan::new(format!(
                                    "marketplace entry `{id}` in another repository"
                                ));
                                p.notes.push(format!(
                                    "would resolve {github}{} next, pinned to {}",
                                    reference
                                        .as_deref()
                                        .map(|r| format!("@{r}"))
                                        .unwrap_or_default(),
                                    sha.as_deref().unwrap_or("the commit that ref names now")
                                ));
                                p
                            }
                        },
                        None => {
                            let mut p = Plan::new(format!("Coqo marketplace `{}`", m.name));
                            p.errors.push(format!("it lists no mod `{id}`"));
                            p
                        }
                    }
                } else {
                    let mut p = Plan::new(format!(
                        "Coqo marketplace `{}` ({} mods)",
                        m.name,
                        m.mods.len()
                    ));
                    for e in &m.mods {
                        let src = match &e.source {
                            MarketplaceSource::Folder(f) => f.clone(),
                            MarketplaceSource::GitHub { github, .. } => format!("github {github}"),
                        };
                        p.listing.push(format!(
                            "{:<28} {} [{src}]",
                            e.id,
                            e.description.as_deref().unwrap_or("")
                        ));
                    }
                    p
                }
            }
            Err(e) => {
                let mut p = Plan::new("a coqo.marketplace.toml that does not parse");
                p.errors.push(e);
                p
            }
        };
    }
    if repo.exists(&join(dir, ".claude-plugin/marketplace.json")) {
        return match claude::read_marketplace(repo, dir) {
            Ok(m) => {
                if let Some(name) = pick {
                    match m.plugins.iter().find(|p| p.name == name) {
                        Some(l) => match &l.local_dir {
                            Some(d) => {
                                let mut p = plan_claude_plugin(repo, d, &m.name, name);
                                p.found =
                                    format!("{} (from Claude marketplace `{}`)", p.found, m.name);
                                p
                            }
                            None => {
                                let mut p =
                                    Plan::new(format!("Claude plugin `{name}` hosted elsewhere"));
                                p.notes.push(format!(
                                    "source {}: would be resolved and pinned the same way",
                                    l.source
                                ));
                                p
                            }
                        },
                        None => {
                            let mut p = Plan::new(format!("Claude marketplace `{}`", m.name));
                            p.errors.push(format!("it lists no plugin `{name}`"));
                            p
                        }
                    }
                } else {
                    let mut p = Plan::new(format!(
                        "Claude Code plugin marketplace `{}` ({} plugins)",
                        m.name,
                        m.plugins.len()
                    ));
                    let mut counts = [0usize; 4];
                    for l in &m.plugins {
                        let (what, class) = match &l.summary {
                            Some(s) => (summary_line(s), classify(s)),
                            None => (format!("elsewhere: {}", l.source), 3),
                        };
                        counts[class] += 1;
                        p.listing.push(format!("{:<26} {what}", l.name));
                    }
                    let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
                    let mut pinned = 0;
                    for l in m.plugins.iter().filter(|l| l.local_dir.is_none()) {
                        *kinds
                            .entry(l.source.split(':').next().unwrap_or("?").to_owned())
                            .or_default() += 1;
                        pinned += usize::from(l.source.contains(" sha "));
                    }
                    p.notes.push(format!(
                        "elsewhere by source kind: {}; {pinned} of them pinned to a full sha",
                        if kinds.is_empty() {
                            "none".to_owned()
                        } else {
                            kinds
                                .iter()
                                .map(|(k, n)| format!("{k} {n}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ));
                    p.notes.push(format!(
                        "read-compatible: {} content only (skills/commands/agents), {} with allowed-tools, \
                         {} running code (hooks/MCP/LSP servers), {} hosted in other repositories",
                        counts[0], counts[1], counts[2], counts[3]
                    ));
                    p
                }
            }
            Err(e) => {
                let mut p = Plan::new("a Claude marketplace.json that does not parse");
                p.errors.push(e);
                p
            }
        };
    }
    let looks_like_plugin = [
        "skills/",
        "commands/",
        "agents/",
        ".claude-plugin/plugin.json",
    ]
    .iter()
    .any(|d| {
        let d = join(dir, d);
        repo.files().iter().any(|e| e.path.starts_with(&d))
    });
    if looks_like_plugin {
        return plan_claude_plugin(
            repo,
            dir,
            repo_name,
            dir.rsplit('/').next().unwrap_or(repo_name),
        );
    }
    let mut p = Plan::new("nothing installable");
    p.errors.push(format!(
        "no coqo.mod.toml, coqo.marketplace.toml or Claude plugin in `{}`",
        if dir.is_empty() { "/" } else { dir }
    ));
    if !dir.is_empty() && !dir.contains('/') {
        p.notes.push(format!(
            "`#{dir}` names a folder; if you meant a tag or branch, write `@{dir}`"
        ));
    }
    p
}

fn classify(s: &PluginSummary) -> usize {
    if s.runs_code() {
        2
    } else if !s.allowed_tools.is_empty() {
        1
    } else {
        0
    }
}

fn summary_line(s: &PluginSummary) -> String {
    let mut parts = Vec::new();
    for (n, what) in [
        (s.skills.len(), "skills"),
        (s.commands.len(), "commands"),
        (s.agents.len(), "agents"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    if !s.hooks.is_empty() {
        parts.push("HOOKS".into());
    }
    if !s.mcp_servers.is_empty() {
        parts.push("MCP".into());
    }
    if !s.lsp_servers.is_empty() {
        parts.push("LSP".into());
    }
    if !s.executables.is_empty() {
        parts.push(format!("{} scripts", s.executables.len()));
    }
    if parts.is_empty() {
        "(empty)".into()
    } else {
        parts.join(", ")
    }
}

fn plugin_runs(s: &PluginSummary, runs: &mut Vec<String>) {
    for h in &s.hooks {
        runs.push(format!("hook (in Claude Code, as you): {h}"));
    }
    for m in &s.mcp_servers {
        runs.push(format!("MCP server (started by Claude Code, as you): {m}"));
    }
    for l in &s.lsp_servers {
        runs.push(format!("LSP server (started by Claude Code, as you): {l}"));
    }
    for (path, tools) in &s.allowed_tools {
        runs.push(format!("pre-approves tools while active ({path}): {tools}"));
    }
}

fn plugin_trust(s: &PluginSummary) -> Trust {
    if s.runs_code() || !s.allowed_tools.is_empty() {
        Trust::Unsandboxed
    } else if s.skills.is_empty() && s.commands.is_empty() && s.agents.is_empty() {
        Trust::Content
    } else {
        Trust::Prompts
    }
}

fn plan_claude_plugin(repo: &dyn Repo, dir: &str, marketplace: &str, name: &str) -> Plan {
    let s = claude::summarise(repo, dir, None, true);
    let name = s.name.clone().unwrap_or_else(|| name.to_owned());
    let mut p = Plan::new(format!("Claude Code plugin `{name}`"));
    p.dir = dir.to_owned();
    // No coqo.mod.toml: Coqo wraps it in a synthesised content mod.
    p.id = Some(format!("claude.{}.{}", slug(marketplace), slug(&name)));
    p.version = s.version.clone();
    p.contributes.push(format!(
        "1 agent plugin for Coqo's Claude sessions: {}",
        summary_line(&s)
    ));
    if !s.skills.is_empty() {
        p.contributes.push(format!("skills: {}", list(&s.skills)));
    }
    if !s.commands.is_empty() {
        p.contributes.push(format!(
            "slash commands: {}",
            list(
                &s.commands
                    .iter()
                    .map(|c| format!("/{name}:{c}"))
                    .collect::<Vec<_>>()
            )
        ));
    }
    if !s.agents.is_empty() {
        p.contributes
            .push(format!("subagents: {}", list(&s.agents)));
    }
    p.contributes.extend(s.other.iter().cloned());
    plugin_runs(&s, &mut p.runs);
    if !s.executables.is_empty() {
        p.notes.push(format!(
            "ships {} executable scripts; nothing starts them by itself, the agent runs them only \
             through tools Claude Code asks you about",
            s.executables.len()
        ));
    }
    p.trust = plugin_trust(&s);
    p.errors.extend(s.problems.iter().cloned());
    p.notes.push(
        "loaded only into Coqo's own Claude sessions (Agent SDK `plugins`, or `--plugin-dir` in terminal mode); \
         nothing is written to your Claude Code configuration"
            .into(),
    );
    p
}

fn slug(s: &str) -> String {
    let s: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    s.trim_matches('-').to_owned()
}

fn list(items: &[String]) -> String {
    if items.len() > 8 {
        format!("{} … (+{})", items[..8].join(", "), items.len() - 8)
    } else {
        items.join(", ")
    }
}

fn plan_mod(m: CheckedMod) -> Plan {
    let tier = match m.tier {
        ModTier::Core => "core",
        ModTier::Wasm => "wasm",
        ModTier::Process => "process",
        ModTier::Content => "content",
    };
    let mut p = Plan::new(format!(
        "Coqo mod `{}` ({} tier)",
        m.manifest.display_name(),
        tier
    ));
    p.id = Some(m.manifest.id.to_string());
    p.version = Some(m.manifest.version.to_string());
    let c = &m.manifest.contributes;
    for (n, what) in [
        (c.commands.len(), "commands"),
        (c.keybindings.len(), "keybindings"),
        (c.menus.len(), "menu items"),
        (c.settings.len(), "settings (a section in Settings)"),
        (c.languages.len(), "languages"),
        (c.lsp_servers.len(), "language servers"),
        (c.mcp_tools.len(), "MCP tools for agents"),
        (c.slash_commands.len(), "slash commands"),
        (c.themes.len(), "themes"),
        (c.alert_sources.len(), "alert sources"),
    ] {
        if n > 0 {
            p.contributes.push(format!("{n} {what}"));
        }
    }
    let views = toml::Value::try_from(&c.views).ok();
    if let Some(toml::Value::Table(t)) = views {
        for (loc, items) in t {
            if let toml::Value::Array(a) = items {
                p.contributes.push(format!("{} {loc} views", a.len()));
            }
        }
    }
    let perms = &m.manifest.permissions;
    for r in &perms.fs.read {
        p.permissions.push(format!("read files: {r}"));
    }
    for w in &perms.fs.write {
        p.permissions.push(format!("write files: {w}"));
    }
    for n in &perms.network {
        p.permissions.push(format!("network: {n}"));
    }
    for x in &perms.process {
        p.permissions.push(format!("start programs: {x}"));
    }
    for s in &perms.secrets {
        p.permissions.push(format!("keychain secret: {s}"));
    }
    p.trust = match m.tier {
        ModTier::Content | ModTier::Core => Trust::Content,
        ModTier::Wasm => Trust::Sandboxed,
        ModTier::Process => Trust::Unsandboxed,
    };
    match m.tier {
        ModTier::Wasm => p.runs.push(format!(
            "{} in the WASM host, on {}",
            m.manifest.entry.as_deref().unwrap_or("?"),
            activation(&m)
        )),
        ModTier::Process => p.runs.push(format!(
            "`{} {}` as a supervised process with your user rights, on {}",
            m.manifest.entry.as_deref().unwrap_or("?"),
            m.manifest.args.join(" "),
            activation(&m)
        )),
        _ => {}
    }
    if !c.lsp_servers.is_empty() {
        p.trust = p.trust.max(Trust::Unsandboxed);
        for l in &c.lsp_servers {
            p.runs.push(format!(
                "language server `{}` (started by Coqo, as you)",
                l.command
            ));
        }
    }
    for (path, s) in &m.agent_plugins {
        p.contributes
            .push(format!("agent plugin {path}: {}", summary_line(s)));
        plugin_runs(s, &mut p.runs);
        p.trust = p.trust.max(plugin_trust(s));
        p.errors.extend(s.problems.iter().cloned());
    }
    p.errors.extend(m.errors);
    p.notes.extend(m.notes);
    p
}

fn activation(m: &CheckedMod) -> String {
    if m.manifest.activation.is_empty() {
        "demand".into()
    } else {
        m.manifest
            .activation
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Renders the plan. `pinned` is the location line, `dir` the folder.
pub fn render(repo: &dyn Repo, dir: &str, p: &Plan, extra: &[String]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "DRY RUN: nothing installed, nothing executed.");
    let _ = writeln!(out, "source      {}", repo.location());
    if !p.dir.is_empty() || !dir.is_empty() {
        let _ = writeln!(
            out,
            "folder      {}",
            if p.dir.is_empty() { dir } else { &p.dir }
        );
    }
    let _ = writeln!(out, "found       {}", p.found);
    if !p.listing.is_empty() {
        let _ = writeln!(out, "lists");
        // Local plugins first: they are what this repository itself holds.
        let mut listing: Vec<&String> = p
            .listing
            .iter()
            .filter(|l| !l.contains("elsewhere:"))
            .collect();
        listing.extend(p.listing.iter().filter(|l| l.contains("elsewhere:")));
        for l in listing.iter().take(40) {
            let _ = writeln!(out, "  - {l}");
        }
        if listing.len() > 40 {
            let _ = writeln!(out, "  … {} more", listing.len() - 40);
        }
    }
    if let Some(id) = &p.id {
        let version = p.version.as_deref().unwrap_or("0.0.0");
        let short = repo
            .location()
            .split(" @ ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("local")
            .to_owned();
        let _ = writeln!(out, "mod         {id} {version}");
        let _ = writeln!(out, "install to  <data>/mods/{id}/{version}-{short}/");
        let prefix = if p.dir.is_empty() {
            String::new()
        } else {
            format!("{}/", p.dir)
        };
        let files: Vec<_> = repo
            .files()
            .iter()
            .filter(|e| e.path.starts_with(&prefix))
            .collect();
        let bytes: u64 = files.iter().map(|e| e.size).sum();
        let _ = writeln!(
            out,
            "files       {} files, {:.1} KiB",
            files.len(),
            bytes as f64 / 1024.0
        );
        for e in files.iter().take(12) {
            let _ = writeln!(out, "  {}", e.path.strip_prefix(&prefix).unwrap_or(&e.path));
        }
        if files.len() > 12 {
            let _ = writeln!(out, "  … {} more", files.len() - 12);
        }
        let _ = writeln!(out, "trust       {}", p.trust.label());
        let _ = writeln!(
            out,
            "permissions {}",
            if p.permissions.is_empty() {
                "none".to_owned()
            } else {
                p.permissions.join("; ")
            }
        );
        if !p.contributes.is_empty() {
            let _ = writeln!(out, "contributes");
            for c in &p.contributes {
                let _ = writeln!(out, "  - {c}");
            }
        }
        let _ = writeln!(
            out,
            "would run   {}",
            if p.runs.is_empty() { "nothing" } else { "" }
        );
        for r in &p.runs {
            let _ = writeln!(out, "  ! {r}");
        }
    }
    for n in p.notes.iter().chain(extra) {
        let _ = writeln!(out, "note        {n}");
    }
    for e in &p.errors {
        let _ = writeln!(out, "ERROR       {e}");
    }
    if repo.truncated() {
        let _ = writeln!(
            out,
            "ERROR       GitHub truncated the file list; the plan is incomplete"
        );
    }
    let (api, raw) = repo.requests();
    let _ = writeln!(
        out,
        "network     {api} api.github.com calls, {raw} raw file fetches"
    );
    let _ = writeln!(
        out,
        "verdict     {}",
        if !p.errors.is_empty() {
            "REFUSED"
        } else if p.id.is_some() {
            "installable after the user approves"
        } else {
            "pick an entry"
        }
    );
    out
}
