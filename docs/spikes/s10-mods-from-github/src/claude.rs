//! Read-only analysis of Claude Code plugins and plugin marketplaces
//! (`.claude-plugin/plugin.json`, `.claude-plugin/marketplace.json`).
//!
//! Coqo would not interpret these itself: it would hand the plugin folder to
//! Coqo's own Claude sessions the way it already hands over its bundled
//! plugin (`crates/agent-claude/src/plugin.rs`, a `{"type": "local"}`
//! plugin in the session's Agent SDK options). What matters for the install
//! plan is what each part *does* once Claude Code loads it.

use serde_json::Value;

use crate::repo::{Kind, Repo};

/// What a Claude plugin folder contains, and what of it runs code.
#[derive(Debug, Default)]
pub struct PluginSummary {
    pub name: Option<String>,
    pub version: Option<String>,
    pub skills: Vec<String>,
    pub commands: Vec<String>,
    pub agents: Vec<String>,
    /// Skills and commands whose frontmatter pre-approves tools
    /// (`allowed-tools`): while active, those tools skip the permission
    /// prompt.
    pub allowed_tools: Vec<(String, String)>,
    /// Hook definitions: shell commands Claude Code runs on events.
    pub hooks: Vec<String>,
    /// MCP servers: programs Claude Code starts, or URLs it connects to.
    pub mcp_servers: Vec<String>,
    /// LSP servers: programs Claude Code starts.
    pub lsp_servers: Vec<String>,
    /// Other parts Coqo would pass through without a plan line of their own.
    pub other: Vec<String>,
    /// Executable files. Nothing starts them by itself (hooks and servers
    /// are listed above); the agent may run them through its tools, which
    /// Claude Code asks about.
    pub executables: Vec<String>,
    pub problems: Vec<String>,
}

impl PluginSummary {
    pub fn runs_code(&self) -> bool {
        !self.hooks.is_empty() || !self.mcp_servers.is_empty() || !self.lsp_servers.is_empty()
    }
}

fn join(prefix: &str, path: &str) -> String {
    let path = path.trim_start_matches("./");
    if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    }
}

fn names_under<'a>(repo: &'a dyn Repo, dir: &str, suffix: &str) -> Vec<&'a str> {
    let dir = format!("{dir}/");
    repo.files()
        .iter()
        .filter_map(|e| e.path.strip_prefix(&dir))
        .filter(|rest| rest.ends_with(suffix))
        .collect()
}

/// The `key: value` frontmatter fields of a Markdown file.
fn frontmatter_field(text: &str, key: &str) -> Option<String> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_owned())
    })
}

fn object_keys(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

fn describe_mcp(name: &str, spec: &Value) -> String {
    if let Some(cmd) = spec.get("command").and_then(Value::as_str) {
        let args: Vec<&str> = spec
            .get("args")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        format!("{name}: runs `{cmd} {}`", args.join(" "))
            .trim_end()
            .to_owned()
    } else if let Some(url) = spec.get("url").and_then(Value::as_str) {
        format!("{name}: connects to {url}")
    } else {
        format!("{name}: (unrecognised definition)")
    }
}

fn hook_lines(hooks: &Value) -> Vec<String> {
    // { "hooks": { "PreToolUse": [ { "matcher": "...", "hooks": [ { "type": "command", "command": "..." } ] } ] } }
    let root = hooks.get("hooks").unwrap_or(hooks);
    let mut out = Vec::new();
    if let Value::Object(events) = root {
        for (event, groups) in events {
            for group in groups.as_array().into_iter().flatten() {
                for hook in group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let what = hook
                        .get("command")
                        .and_then(Value::as_str)
                        .map(|c| format!("runs `{c}`"))
                        .unwrap_or_else(|| {
                            format!(
                                "{} hook",
                                hook.get("type").and_then(Value::as_str).unwrap_or("?")
                            )
                        });
                    out.push(format!("{event}: {what}"));
                }
            }
        }
    }
    out
}

/// Summarises the plugin at `dir` ("" for the repository root). Without
/// `deep` it looks only at the file list (no fetches, for marketplace
/// listings); with it, it reads plugin.json, hooks, MCP definitions and the
/// skills' and commands' frontmatter (one fetch each, capped).
pub fn summarise(repo: &dyn Repo, dir: &str, entry: Option<&Value>, deep: bool) -> PluginSummary {
    let mut s = PluginSummary::default();
    let manifest_path = join(dir, ".claude-plugin/plugin.json");
    let manifest: Option<Value> = if deep && repo.exists(&manifest_path) {
        match repo
            .read(&manifest_path)
            .map(|t| serde_json::from_str::<Value>(&t))
        {
            Ok(Ok(v)) => Some(v),
            Ok(Err(e)) => {
                s.problems.push(format!("{manifest_path}: {e}"));
                None
            }
            Err(e) => {
                s.problems.push(e);
                None
            }
        }
    } else {
        None
    };
    // A marketplace entry can carry any plugin.json field.
    let field = |key: &str| -> Option<&Value> {
        manifest
            .as_ref()
            .and_then(|m| m.get(key))
            .or_else(|| entry.and_then(|e| e.get(key)))
    };
    s.name = field("name").and_then(Value::as_str).map(str::to_owned);
    s.version = field("version").and_then(Value::as_str).map(str::to_owned);

    // Default locations.
    let skills_dir = join(dir, "skills");
    s.skills = names_under(repo, &skills_dir, "/SKILL.md")
        .into_iter()
        .map(|p| p.trim_end_matches("/SKILL.md").to_owned())
        .collect();
    let commands_dir = join(dir, "commands");
    s.commands = names_under(repo, &commands_dir, ".md")
        .into_iter()
        .map(|p| p.trim_end_matches(".md").to_owned())
        .collect();
    let agents_dir = join(dir, "agents");
    s.agents = names_under(repo, &agents_dir, ".md")
        .into_iter()
        .map(|p| p.trim_end_matches(".md").to_owned())
        .collect();
    for (key, label) in [
        ("skills", "custom skills path"),
        ("commands", "custom commands path"),
        ("agents", "custom agents path"),
    ] {
        if let Some(v) = field(key) {
            s.other.push(format!("{label}: {v}"));
        }
    }

    // Hooks: hooks/hooks.json, or inline / a path in the manifest.
    let hooks_file = join(dir, "hooks/hooks.json");
    if repo.exists(&hooks_file) && !deep {
        s.hooks
            .push(format!("{hooks_file} (shell commands on events)"));
    } else if repo.exists(&hooks_file) {
        match repo
            .read(&hooks_file)
            .map(|t| serde_json::from_str::<Value>(&t))
        {
            Ok(Ok(v)) => s.hooks.extend(hook_lines(&v)),
            _ => s
                .hooks
                .push(format!("{hooks_file} (unreadable, treated as code)")),
        }
    }
    match field("hooks") {
        Some(Value::String(p)) => s.hooks.push(format!("hooks from {p}")),
        Some(v @ Value::Object(_)) => s.hooks.extend(hook_lines(v)),
        Some(Value::Array(a)) => s.hooks.push(format!("{} hook files", a.len())),
        _ => {}
    }

    // MCP servers: .mcp.json, or inline in the manifest.
    let mcp_file = join(dir, ".mcp.json");
    if repo.exists(&mcp_file) && !deep {
        s.mcp_servers.push(format!("{mcp_file} (programs or URLs)"));
    } else if repo.exists(&mcp_file) {
        match repo
            .read(&mcp_file)
            .map(|t| serde_json::from_str::<Value>(&t))
        {
            Ok(Ok(v)) => {
                let servers = v.get("mcpServers").unwrap_or(&v);
                if let Value::Object(m) = servers {
                    s.mcp_servers
                        .extend(m.iter().map(|(n, spec)| describe_mcp(n, spec)));
                }
            }
            _ => s
                .mcp_servers
                .push(format!("{mcp_file} (unreadable, treated as code)")),
        }
    }
    match field("mcpServers") {
        Some(Value::Object(m)) => s
            .mcp_servers
            .extend(m.iter().map(|(n, spec)| describe_mcp(n, spec))),
        Some(Value::String(p)) => s.mcp_servers.push(format!("servers from {p}")),
        _ => {}
    }
    if repo.exists(&join(dir, ".lsp.json")) {
        s.lsp_servers.push(".lsp.json".into());
    }
    s.lsp_servers.extend(object_keys(field("lspServers")));
    for (key, label) in [
        ("outputStyles", "output styles"),
        ("userConfig", "asks for user config"),
        ("dependencies", "depends on other plugins"),
    ] {
        if field(key).is_some() {
            s.other.push(label.into());
        }
    }
    if repo.exists(&join(dir, "monitors/monitors.json")) {
        s.hooks
            .push("monitors/monitors.json (background monitors)".into());
    }

    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    s.executables = repo
        .files()
        .iter()
        .filter(|e| e.path.starts_with(&prefix) && e.kind == Kind::Executable)
        .map(|e| e.path.clone())
        .collect();

    if deep {
        // Each read is one fetch; cap them so a big plugin stays cheap.
        let mut budget = 25;
        let docs: Vec<String> = s
            .skills
            .iter()
            .map(|n| format!("{skills_dir}/{n}/SKILL.md"))
            .chain(s.commands.iter().map(|n| format!("{commands_dir}/{n}.md")))
            .collect();
        for path in docs {
            if budget == 0 {
                s.problems
                    .push("stopped reading frontmatter after 25 files".into());
                break;
            }
            budget -= 1;
            if let Ok(text) = repo.read(&path) {
                if let Some(tools) = frontmatter_field(&text, "allowed-tools") {
                    if !tools.is_empty() {
                        s.allowed_tools.push((path, tools));
                    }
                }
            }
        }
    }
    s
}

/// One plugin listed by a marketplace.
#[derive(Debug)]
pub struct Listed {
    pub name: String,
    #[allow(dead_code)] // shown by a real install dialog
    pub description: Option<String>,
    /// `Some(dir)` for a plugin in the same repository.
    pub local_dir: Option<String>,
    /// The source as written, for plugins elsewhere.
    pub source: String,
    pub summary: Option<PluginSummary>,
}

pub struct Marketplace {
    pub name: String,
    pub plugins: Vec<Listed>,
}

pub fn read_marketplace(repo: &dyn Repo, dir: &str) -> Result<Marketplace, String> {
    let path = join(dir, ".claude-plugin/marketplace.json");
    let v: Value = serde_json::from_str(&repo.read(&path)?).map_err(|e| format!("{path}: {e}"))?;
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path}: no `name`"))?
        .to_owned();
    let root = v
        .pointer("/metadata/pluginRoot")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut plugins = Vec::new();
    for p in v
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let pname = p
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let description = p
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (local_dir, source) = match p.get("source") {
            Some(Value::String(rel)) => {
                let rel = if rel.starts_with("./") || root.is_empty() {
                    rel.clone()
                } else {
                    format!("{root}/{rel}")
                };
                match crate::source::clean_subdir(&rel) {
                    Ok(d) => (Some(join(dir, d.as_deref().unwrap_or(""))), rel),
                    Err(e) => (None, format!("refused: {e}")),
                }
            }
            Some(Value::Object(o)) => {
                let kind = o.get("source").and_then(Value::as_str).unwrap_or("?");
                let what = o
                    .get("repo")
                    .or_else(|| o.get("url"))
                    .or_else(|| o.get("package"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let pin = o
                    .get("sha")
                    .and_then(Value::as_str)
                    .map(|s| format!(" sha {}", &s[..s.len().min(12)]))
                    .or_else(|| {
                        o.get("ref")
                            .and_then(Value::as_str)
                            .map(|r| format!(" ref {r}"))
                    })
                    .unwrap_or_default();
                (None, format!("{kind}: {what}{pin}"))
            }
            _ => (None, "no source".into()),
        };
        let summary = local_dir
            .as_deref()
            .map(|d| summarise(repo, d, Some(p), false));
        plugins.push(Listed {
            name: pname,
            description,
            local_dir,
            source,
            summary,
        });
    }
    Ok(Marketplace { name, plugins })
}
