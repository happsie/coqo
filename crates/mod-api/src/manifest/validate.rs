// SPDX-License-Identifier: Apache-2.0

//! Checks beyond what deserializing already enforces (types, id grammar,
//! semver, unknown fields).

use std::collections::HashMap;

use serde_json::Value;

use super::error::FieldPath;
use super::permissions::{check_fs_scope, check_host};
use super::{Contributes, Manifest, Tier};
use crate::contrib::{Keyed, Setting, SettingType};
use crate::id::ContributionId;

type Problems = Vec<(FieldPath, String)>;

pub(super) fn validate(m: &Manifest) -> Problems {
    let mut out = Problems::new();
    let root = FieldPath::root();

    non_empty_opt(&mut out, &root.key("name"), m.name.as_deref());
    non_empty_opt(&mut out, &root.key("description"), m.description.as_deref());
    for (i, author) in m.authors.iter().enumerate() {
        non_empty(&mut out, &root.key("authors").index(i), author);
    }
    entry(&mut out, m);

    let mut seen = HashMap::new();
    for (i, event) in m.activation.iter().enumerate() {
        if let Some(first) = seen.insert(event.to_string(), i) {
            out.push((
                root.key("activation").index(i),
                format!("duplicate activation event `{event}` (also at activation[{first}])"),
            ));
        }
    }

    let mut seen = HashMap::new();
    for (i, service) in m.provides.iter().enumerate() {
        if let Some(first) = seen.insert((&service.id, service.version.major), i) {
            out.push((
                root.key("provides").index(i),
                format!(
                    "`{}` major version {} is already provided at provides[{first}]",
                    service.id, service.version.major
                ),
            ));
        }
    }
    let mut seen = HashMap::new();
    for (i, dep) in m.consumes.iter().enumerate() {
        let s = &dep.service;
        if let Some(first) = seen.insert((&s.id, s.version.major), i) {
            out.push((
                root.key("consumes").index(i),
                format!(
                    "`{}` major version {} is already consumed at consumes[{first}]",
                    s.id, s.version.major
                ),
            ));
        }
    }

    permissions(&mut out, m);
    contributes(&mut out, &m.contributes);
    out
}

fn non_empty(out: &mut Problems, path: &FieldPath, value: &str) {
    if value.trim().is_empty() {
        out.push((path.clone(), "must not be empty".into()));
    }
}

fn non_empty_opt(out: &mut Problems, path: &FieldPath, value: Option<&str>) {
    if let Some(value) = value {
        non_empty(out, path, value);
    }
}

/// A path inside the mod's folder: relative, `/`-separated, no `..`.
fn check_relative_path(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("must not be empty".into());
    }
    if value.contains('\\') {
        return Err(format!("`{value}`: use `/` as the path separator"));
    }
    let drive = value.as_bytes().get(1) == Some(&b':');
    if value.starts_with('/') || drive {
        return Err(format!("`{value}` must be relative to the mod's folder"));
    }
    if value.split('/').any(|seg| seg == "..") {
        return Err(format!(
            "`{value}` must stay inside the mod's folder; `..` is not allowed"
        ));
    }
    Ok(())
}

fn relative_path(out: &mut Problems, path: &FieldPath, value: &str) {
    if let Err(message) = check_relative_path(value) {
        out.push((path.clone(), message));
    }
}

fn entry(out: &mut Problems, m: &Manifest) {
    let root = FieldPath::root();
    match (m.tier, &m.entry) {
        (Tier::Core, Some(_)) => out.push((
            root.key("entry"),
            "core mods are linked into Coqo, so they take no `entry`; remove it".into(),
        )),
        (Tier::Wasm, None) => out.push((
            root.key("tier"),
            "wasm mods need `entry`: the component's path, such as `mod.wasm`".into(),
        )),
        (Tier::Process, None) => out.push((
            root.key("tier"),
            "process mods need `entry`: the program to run, such as `bin/my-mod`".into(),
        )),
        (Tier::Wasm, Some(entry)) => {
            relative_path(out, &root.key("entry"), entry);
            if !entry.ends_with(".wasm") {
                out.push((
                    root.key("entry"),
                    format!("`{entry}` must be a WASM component ending in `.wasm`"),
                ));
            }
        }
        (Tier::Process, Some(entry)) => {
            if entry.contains('/') || entry.contains('\\') {
                relative_path(out, &root.key("entry"), entry);
            } else {
                non_empty(out, &root.key("entry"), entry);
            }
        }
        (Tier::Core, None) => {}
    }
    if m.tier != Tier::Process && !m.args.is_empty() {
        out.push((
            root.key("args"),
            format!(
                "`args` only applies to process mods, and this is a {} mod",
                m.tier
            ),
        ));
    }
}

fn permissions(out: &mut Problems, m: &Manifest) {
    let p = &m.permissions;
    let base = FieldPath::root().key("permissions");
    for (key, scopes) in [("read", &p.fs.read), ("write", &p.fs.write)] {
        let path = base.key("fs").key(key);
        for (i, scope) in scopes.iter().enumerate() {
            if let Err(message) = check_fs_scope(scope) {
                out.push((path.index(i), message));
            }
        }
        duplicates(out, &path, scopes.iter().map(String::as_str), "path");
    }
    for (i, host) in p.network.iter().enumerate() {
        if let Err(message) = check_host(host) {
            out.push((base.key("network").index(i), message));
        }
    }
    duplicates(
        out,
        &base.key("network"),
        p.network.iter().map(String::as_str),
        "host",
    );
    for (i, program) in p.process.iter().enumerate() {
        if program.trim().is_empty() || program.chars().any(char::is_whitespace) {
            out.push((
                base.key("process").index(i),
                format!("`{program}` must be a program name or path without spaces, or `*`"),
            ));
        }
    }
    duplicates(
        out,
        &base.key("process"),
        p.process.iter().map(String::as_str),
        "program",
    );
    for (i, secret) in p.secrets.iter().enumerate() {
        if let Err(e) = ContributionId::new(secret.as_str()) {
            out.push((
                base.key("secrets").index(i),
                e.to_string().replace("contribution id", "secret name"),
            ));
        }
    }
    duplicates(
        out,
        &base.key("secrets"),
        p.secrets.iter().map(String::as_str),
        "secret",
    );
}

fn duplicates<'a>(
    out: &mut Problems,
    path: &FieldPath,
    values: impl Iterator<Item = &'a str>,
    what: &str,
) {
    let mut seen = HashMap::new();
    for (i, value) in values.enumerate() {
        if let Some(first) = seen.insert(value, i) {
            out.push((
                path.index(i),
                format!("duplicate {what} `{value}` (also at [{first}])"),
            ));
        }
    }
}

/// Reports contributions whose id repeats, across all the lists given (views
/// share one namespace across locations).
fn unique_ids(out: &mut Problems, what: &str, lists: &[(FieldPath, Vec<&ContributionId>)]) {
    let mut seen: HashMap<&ContributionId, FieldPath> = HashMap::new();
    for (path, ids) in lists {
        for (i, id) in ids.iter().enumerate() {
            let here = path.index(i);
            match seen.get(id) {
                Some(first) => out.push((
                    here.key(key_field(what)),
                    format!("duplicate {what} `{id}` (also at {first})"),
                )),
                None => {
                    seen.insert(id, here);
                }
            }
        }
    }
}

fn key_field(what: &str) -> &'static str {
    match what {
        "setting" => "key",
        "MCP tool" | "slash command" => "name",
        _ => "id",
    }
}

fn keys<T: Keyed>(values: &[T]) -> Vec<&ContributionId> {
    values.iter().map(Keyed::key).collect()
}

fn contributes(out: &mut Problems, c: &Contributes) {
    let base = FieldPath::root().key("contributes");

    let path = base.key("commands");
    unique_ids(out, "command", &[(path.clone(), keys(&c.commands))]);
    for (i, cmd) in c.commands.iter().enumerate() {
        non_empty(out, &path.index(i).key("title"), &cmd.title);
    }

    let path = base.key("keybindings");
    for (i, kb) in c.keybindings.iter().enumerate() {
        if let Err(message) = check_keys(&kb.key) {
            out.push((path.index(i).key("key"), message));
        }
        if let Some(mac) = &kb.mac
            && let Err(message) = check_keys(mac)
        {
            out.push((path.index(i).key("mac"), message));
        }
    }

    let path = base.key("settings");
    unique_ids(out, "setting", &[(path.clone(), keys(&c.settings))]);
    for (i, setting) in c.settings.iter().enumerate() {
        check_setting(out, &path.index(i), setting);
    }

    let path = base.key("settings_sections");
    unique_ids(
        out,
        "settings section",
        &[(path.clone(), keys(&c.settings_sections))],
    );
    for (i, section) in c.settings_sections.iter().enumerate() {
        non_empty(out, &path.index(i).key("title"), &section.title);
        non_empty_opt(
            out,
            &path.index(i).key("description"),
            section.description.as_deref(),
        );
    }

    let views = base.key("views");
    let v = &c.views;
    unique_ids(
        out,
        "view",
        &[
            (views.key("sidebar"), keys(&v.sidebar)),
            (views.key("panel"), keys(&v.panel)),
            (views.key("tab"), keys(&v.tab)),
            (views.key("rail"), keys(&v.rail)),
            (views.key("status"), keys(&v.status)),
        ],
    );
    for (key, list) in [
        ("sidebar", &v.sidebar),
        ("panel", &v.panel),
        ("tab", &v.tab),
        ("rail", &v.rail),
    ] {
        for (i, view) in list.iter().enumerate() {
            non_empty(out, &views.key(key).index(i).key("title"), &view.title);
        }
    }
    for (i, item) in v.status.iter().enumerate() {
        non_empty(out, &views.key("status").index(i).key("text"), &item.text);
    }

    let path = base.key("languages");
    unique_ids(out, "language", &[(path.clone(), keys(&c.languages))]);
    for (i, lang) in c.languages.iter().enumerate() {
        let here = path.index(i);
        non_empty(out, &here.key("name"), &lang.name);
        if lang.extensions.is_empty() && lang.filenames.is_empty() {
            out.push((
                here,
                "give `extensions` or `filenames`, or the language matches no files".into(),
            ));
            continue;
        }
        for (j, ext) in lang.extensions.iter().enumerate() {
            let ok = ext.len() > 1 && ext.starts_with('.') && !ext.contains(['/', '\\']);
            if !ok {
                out.push((
                    here.key("extensions").index(j),
                    format!("`{ext}` must be a file extension including the dot, such as `.java`"),
                ));
            }
        }
        for (j, name) in lang.filenames.iter().enumerate() {
            if name.is_empty() || name.contains(['/', '\\']) {
                out.push((
                    here.key("filenames").index(j),
                    format!("`{name}` must be a bare file name, such as `pom.xml`"),
                ));
            }
        }
        if let Some(grammar) = &lang.grammar {
            relative_path(out, &here.key("grammar"), grammar);
        }
    }

    let path = base.key("lsp_servers");
    unique_ids(
        out,
        "language server",
        &[(path.clone(), keys(&c.lsp_servers))],
    );
    for (i, server) in c.lsp_servers.iter().enumerate() {
        let here = path.index(i);
        non_empty_opt(out, &here.key("name"), server.name.as_deref());
        non_empty(out, &here.key("command"), &server.command);
        if server.languages.is_empty() {
            out.push((
                here.key("languages"),
                "list at least one language id".into(),
            ));
        }
    }

    let path = base.key("debug_adapters");
    unique_ids(
        out,
        "debug adapter",
        &[(path.clone(), keys(&c.debug_adapters))],
    );
    for (i, adapter) in c.debug_adapters.iter().enumerate() {
        let here = path.index(i);
        non_empty(out, &here.key("name"), &adapter.name);
        non_empty_opt(out, &here.key("command"), adapter.command.as_deref());
        if adapter.command.is_none() && !adapter.args.is_empty() {
            out.push((here.key("args"), "`args` needs a `command`".into()));
        }
    }

    let path = base.key("mcp_tools");
    unique_ids(out, "MCP tool", &[(path.clone(), keys(&c.mcp_tools))]);
    for (i, tool) in c.mcp_tools.iter().enumerate() {
        let here = path.index(i);
        let name = tool.name.as_str();
        if name.contains('.') || name.len() > 64 {
            out.push((
                here.key("name"),
                format!("MCP tool name `{name}` must be at most 64 characters of [a-z0-9_-], without dots"),
            ));
        }
        non_empty(out, &here.key("description"), &tool.description);
        let is_object_schema = tool
            .input_schema
            .as_object()
            .is_some_and(|o| o.get("type") == Some(&Value::from("object")));
        if !is_object_schema {
            out.push((
                here.key("input_schema"),
                "must be a JSON Schema object with `type = \"object\"`".into(),
            ));
        }
    }

    let path = base.key("agent_providers");
    unique_ids(
        out,
        "agent provider",
        &[(path.clone(), keys(&c.agent_providers))],
    );
    for (i, provider) in c.agent_providers.iter().enumerate() {
        non_empty(out, &path.index(i).key("name"), &provider.name);
    }

    let path = base.key("slash_commands");
    unique_ids(
        out,
        "slash command",
        &[(path.clone(), keys(&c.slash_commands))],
    );
    for (i, cmd) in c.slash_commands.iter().enumerate() {
        non_empty(out, &path.index(i).key("description"), &cmd.description);
    }

    let path = base.key("context_providers");
    unique_ids(
        out,
        "context provider",
        &[(path.clone(), keys(&c.context_providers))],
    );
    for (i, provider) in c.context_providers.iter().enumerate() {
        non_empty(out, &path.index(i).key("name"), &provider.name);
    }

    let path = base.key("alert_sources");
    unique_ids(
        out,
        "alert source",
        &[(path.clone(), keys(&c.alert_sources))],
    );
    for (i, source) in c.alert_sources.iter().enumerate() {
        non_empty(out, &path.index(i).key("name"), &source.name);
    }

    let path = base.key("themes");
    unique_ids(out, "theme", &[(path.clone(), keys(&c.themes))]);
    for (i, theme) in c.themes.iter().enumerate() {
        non_empty(out, &path.index(i).key("name"), &theme.name);
        relative_path(out, &path.index(i).key("path"), &theme.path);
    }
}

fn check_setting(out: &mut Problems, here: &FieldPath, s: &Setting) {
    non_empty(out, &here.key("description"), &s.description);
    non_empty_opt(out, &here.key("title"), s.title.as_deref());

    let numeric = matches!(s.ty, SettingType::Integer | SettingType::Number);
    if !numeric {
        for (key, bound) in [("minimum", s.minimum), ("maximum", s.maximum)] {
            if bound.is_some() {
                out.push((
                    here.key(key),
                    "only applies to `integer` and `number` settings".into(),
                ));
            }
        }
    }
    if let (Some(min), Some(max)) = (s.minimum, s.maximum)
        && min > max
    {
        out.push((
            here.key("minimum"),
            format!("{min} is greater than `maximum` ({max})"),
        ));
    }

    if s.ty == SettingType::Enum {
        if s.values.is_empty() {
            out.push((
                here.key("type"),
                "enum settings need `values`, the allowed values".into(),
            ));
        }
        duplicates(
            out,
            &here.key("values"),
            s.values.iter().map(String::as_str),
            "value",
        );
    } else if !s.values.is_empty() {
        out.push((
            here.key("values"),
            "only applies to `type = \"enum\"`".into(),
        ));
    }

    let Some(default) = &s.default else { return };
    let path = here.key("default");
    let matches = match s.ty {
        SettingType::Boolean => default.is_boolean(),
        SettingType::String => default.is_string(),
        SettingType::Integer => default.is_i64() || default.is_u64(),
        SettingType::Number => default.is_number(),
        SettingType::Enum => default.is_string(),
        SettingType::StringList => default
            .as_array()
            .is_some_and(|a| a.iter().all(Value::is_string)),
    };
    if !matches {
        let expected = match s.ty {
            SettingType::Boolean => "true or false",
            SettingType::String | SettingType::Enum => "a string",
            SettingType::Integer => "a whole number",
            SettingType::Number => "a number",
            SettingType::StringList => "a list of strings",
        };
        out.push((
            path,
            format!("default `{default}` doesn't match the setting's type; expected {expected}"),
        ));
        return;
    }
    if s.ty == SettingType::Enum
        && let Some(value) = default.as_str()
        && !s.values.iter().any(|v| v == value)
    {
        out.push((
            path.clone(),
            format!("default `{value}` is not one of `values`"),
        ));
    }
    if let Some(n) = default.as_f64() {
        if let Some(min) = s.minimum
            && n < min
        {
            out.push((
                path.clone(),
                format!("default {n} is below `minimum` ({min})"),
            ));
        }
        if let Some(max) = s.maximum
            && n > max
        {
            out.push((path, format!("default {n} is above `maximum` ({max})")));
        }
    }
}

const MODIFIERS: &[&str] = &["ctrl", "alt", "shift", "cmd", "mod"];
const NAMED_KEYS: &[&str] = &[
    "enter",
    "escape",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
];

/// Checks a key sequence such as `ctrl+shift+p` or `mod+k mod+s`.
pub(crate) fn check_keys(keys: &str) -> Result<(), String> {
    if keys.trim().is_empty() {
        return Err("key sequence is empty".into());
    }
    for chord in keys.split(' ') {
        if chord.is_empty() {
            return Err(format!("`{keys}`: separate chords with a single space"));
        }
        let parts: Vec<&str> = chord.split('+').collect();
        let (key, modifiers) = parts.split_last().expect("split yields at least one part");
        let mut seen = Vec::new();
        for m in modifiers {
            if !MODIFIERS.contains(m) {
                return Err(format!(
                    "unknown modifier `{m}` in `{chord}`; expected one of {}",
                    MODIFIERS.join(", ")
                ));
            }
            if seen.contains(m) {
                return Err(format!("modifier `{m}` repeats in `{chord}`"));
            }
            seen.push(m);
        }
        if key.is_empty() {
            return Err(format!("`{chord}` has no key after the last `+`"));
        }
        if MODIFIERS.contains(key) {
            return Err(format!(
                "`{chord}` ends in a modifier; add the key, such as `{key}+p`"
            ));
        }
        let single = {
            let mut chars = key.chars();
            matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_graphic() && !c.is_ascii_uppercase())
        };
        let function_key = key
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=24).contains(&n));
        if !(single || function_key || NAMED_KEYS.contains(key)) {
            let hint = if key.chars().any(|c| c.is_ascii_uppercase()) {
                format!(
                    "; keys are lowercase, so write `{}`",
                    key.to_ascii_lowercase()
                )
            } else {
                format!(
                    "; use a single character, f1-f24, or one of {}",
                    NAMED_KEYS.join(", ")
                )
            };
            return Err(format!("unknown key `{key}` in `{chord}`{hint}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_sequences() {
        for ok in [
            "ctrl+shift+p",
            "mod+k mod+s",
            "f5",
            "alt+enter",
            "cmd+,",
            "shift+f12",
        ] {
            check_keys(ok).unwrap();
        }
        let err = |k: &str| check_keys(k).unwrap_err();
        assert!(err("ctl+p").contains("unknown modifier `ctl`"));
        assert!(err("ctrl+P").contains("write `p`"));
        assert!(err("ctrl+").contains("no key"));
        assert!(err("ctrl+shift").contains("ends in a modifier"));
        assert!(err("ctrl+ctrl+p").contains("repeats"));
        assert!(err("ctrl+escape2").contains("unknown key `escape2`"));
        assert!(err("mod+k  mod+s").contains("single space"));
        assert!(err("").contains("empty"));
    }

    #[test]
    fn relative_paths() {
        check_relative_path("grammars/java.wasm").unwrap();
        assert!(
            check_relative_path("/abs")
                .unwrap_err()
                .contains("relative")
        );
        assert!(
            check_relative_path("C:/x")
                .unwrap_err()
                .contains("relative")
        );
        assert!(check_relative_path("a/../../b").unwrap_err().contains(".."));
        assert!(
            check_relative_path("a\\b")
                .unwrap_err()
                .contains("separator")
        );
    }
}
