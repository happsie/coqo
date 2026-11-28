// SPDX-License-Identifier: Apache-2.0

//! The user's keybindings file, and the key syntax people write.
//!
//! `keybindings.toml` lives in Coqo's config directory (PLAN §8):
//! `$XDG_CONFIG_HOME/coqo` (`~/.config/coqo`) on Linux and
//! `~/Library/Application Support/Coqo` on macOS, or `$COQO_CONFIG_DIR`. It
//! maps command ids to keys and overrides the defaults by id:
//!
//! ```toml
//! "command_palette.open" = "mod+shift+p"   # one key sequence
//! "diff.next_hunk" = ["j", "down"]          # several
//! "terminal.new" = []                       # unbound
//! ```
//!
//! Keys use the mod manifest's syntax (`coqo_mod_api::contrib::Keybinding`):
//! chords separated by spaces, each chord modifiers and a key joined by `+`
//! (`mod+k mod+s`). `mod` is `cmd` on macOS and `ctrl` elsewhere. GPUI's
//! own spelling (`ctrl-shift-p`) is accepted too. A command keeps its own
//! context: a diff command bound here still only works in the diff.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The file's name in the config directory.
pub const FILE_NAME: &str = "keybindings.toml";

/// Coqo's per-user config directory: `$COQO_CONFIG_DIR` if set, otherwise
/// `$XDG_CONFIG_HOME/coqo` on Linux and `~/Library/Application Support/Coqo`
/// on macOS.
pub fn config_dir() -> PathBuf {
    coqo_client::config_dir()
}

/// Where the user's keybindings file is.
pub fn path() -> PathBuf {
    config_dir().join(FILE_NAME)
}

/// What a keybindings file says: keys by command id, in GPUI's syntax, and
/// what was wrong with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserKeys {
    /// Keys by command id. An empty list unbinds the command.
    pub overrides: BTreeMap<String, Vec<String>>,
    /// Lines or entries that were skipped, and why.
    pub problems: Vec<String>,
}

/// Read the file at `path`. A missing file is no overrides; one that can't
/// be read or parsed is reported in [`UserKeys::problems`] and ignored.
pub fn read(path: &Path, mac: bool) -> UserKeys {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, mac),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UserKeys::default(),
        Err(e) => UserKeys {
            overrides: BTreeMap::new(),
            problems: vec![format!("cannot read {}: {e}", path.display())],
        },
    }
}

/// Parse a keybindings file. Entries with bad values are skipped and
/// reported; the rest still apply.
pub fn parse(text: &str, mac: bool) -> UserKeys {
    let mut keys = UserKeys::default();
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(e) => {
            keys.problems
                .push(format!("{FILE_NAME} is not valid TOML: {}", e.message()));
            return keys;
        }
    };
    for (id, value) in table {
        let sources: Vec<&str> = match &value {
            toml::Value::String(s) => vec![s.as_str()],
            toml::Value::Array(items) => {
                let strings: Option<Vec<&str>> = items.iter().map(|v| v.as_str()).collect();
                match strings {
                    Some(strings) => strings,
                    None => {
                        keys.problems
                            .push(format!("`{id}`: expected keys as strings, like \"mod+k\""));
                        continue;
                    }
                }
            }
            _ => {
                keys.problems.push(format!(
                    "`{id}`: expected a key string or a list of them, like \"mod+k\""
                ));
                continue;
            }
        };
        let mut converted = Vec::new();
        for source in sources {
            match to_gpui(source, mac) {
                Ok(k) => converted.push(k),
                Err(e) => keys.problems.push(format!("`{id}`: {e}")),
            }
        }
        // A list whose every entry was bad keeps the defaults.
        if converted.is_empty() && !matches!(&value, toml::Value::Array(a) if a.is_empty()) {
            continue;
        }
        keys.overrides.insert(id, converted);
    }
    keys
}

/// Split a chord at `sep`, where a trailing empty piece means the key is
/// `sep` itself (`ctrl++`, `ctrl--`).
fn split_chord(chord: &str, sep: char) -> Vec<String> {
    let mut parts: Vec<String> = chord.split(sep).map(str::to_owned).collect();
    if parts.len() >= 3 && parts[parts.len() - 1].is_empty() && parts[parts.len() - 2].is_empty() {
        parts.truncate(parts.len() - 2);
        parts.push(sep.to_string());
    }
    parts
}

/// Keys in the manifest syntax (`mod+shift+p`, `mod+k mod+s`) or GPUI's
/// (`ctrl-shift-p`) as GPUI keystrokes (`ctrl-shift-p`). Modifiers are
/// `ctrl`, `alt` (or `option`), `shift`, `cmd` (or `super`, `win`) and `mod`
/// (`cmd` on macOS, `ctrl` elsewhere).
pub fn to_gpui(keys: &str, mac: bool) -> Result<String, String> {
    let mut chords = Vec::new();
    for chord in keys.split_whitespace() {
        let sep = if chord.len() > 1 && chord.contains('+') {
            '+'
        } else {
            '-'
        };
        let parts = if chord.len() > 1 {
            split_chord(chord, sep)
        } else {
            vec![chord.to_owned()]
        };
        let Some((key, modifiers)) = parts.split_last() else {
            return Err(format!("`{keys}` has an empty chord"));
        };
        if key.is_empty() {
            return Err(format!("`{keys}`: `{chord}` has no key"));
        }
        let mut out = Vec::new();
        for m in modifiers {
            let m = match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => "ctrl",
                "alt" | "option" | "opt" => "alt",
                "shift" => "shift",
                "cmd" | "command" | "super" | "win" | "meta" => "cmd",
                "fn" => "fn",
                "mod" | "secondary" if mac => "cmd",
                "mod" | "secondary" => "ctrl",
                other => return Err(format!("`{keys}`: unknown modifier `{other}`")),
            };
            out.push(m.to_owned());
        }
        let key = if key.chars().count() == 1 {
            key.to_lowercase()
        } else {
            key.to_ascii_lowercase()
        };
        out.push(key);
        chords.push(out.join("-"));
    }
    if chords.is_empty() {
        return Err("no keys".to_owned());
    }
    Ok(chords.join(" "))
}

/// GPUI keys (`ctrl-shift-p`, `cmd-k cmd-s`) as people read them:
/// `Ctrl Shift P` elsewhere, `⌘⇧P` on macOS.
pub fn display(keys: &str, mac: bool) -> String {
    keys.split_whitespace()
        .map(|chord| {
            let parts = if chord.len() > 1 {
                split_chord(chord, '-')
            } else {
                vec![chord.to_owned()]
            };
            let (key, modifiers) = parts
                .split_last()
                .map_or(("", &[][..]), |(k, m)| (k.as_str(), m));
            let mut shown: Vec<String> = modifiers
                .iter()
                .map(|m| {
                    match (m.as_str(), mac) {
                        ("ctrl", true) => "⌃",
                        ("alt", true) => "⌥",
                        ("shift", true) => "⇧",
                        ("cmd", true) => "⌘",
                        ("ctrl", false) => "Ctrl",
                        ("alt", false) => "Alt",
                        ("shift", false) => "Shift",
                        ("cmd", false) => "Super",
                        ("fn", _) => "Fn",
                        (other, _) => other,
                    }
                    .to_owned()
                })
                .collect();
            shown.push(key_name(key));
            shown.join(if mac { "" } else { " " })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// GPUI keys (`ctrl-k ctrl-s`) in the file's own syntax (`mod+k mod+s`):
/// `mod` stands for the platform's main modifier (`cmd` on macOS, `ctrl`
/// elsewhere), so a file travels between machines.
pub fn to_file_syntax(keys: &str, mac: bool) -> String {
    keys.split_whitespace()
        .map(|chord| {
            let parts = if chord.len() > 1 {
                split_chord(chord, '-')
            } else {
                vec![chord.to_owned()]
            };
            let Some((key, modifiers)) = parts.split_last() else {
                return chord.to_owned();
            };
            let main = if mac { "cmd" } else { "ctrl" };
            let mut out: Vec<String> = modifiers
                .iter()
                .map(|m| {
                    if m == main {
                        "mod".to_owned()
                    } else {
                        m.clone()
                    }
                })
                .collect();
            out.push(key.clone());
            out.join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Set command `id`'s keys in the file at `path` (`Some`: one entry, a
/// string for one key sequence and a list for several, an empty list
/// unbinds) or remove its entry (`None`: back to the defaults). The rest of
/// the file, comments included, is kept as it is. `keys` are in the file's
/// syntax ([`to_file_syntax`]).
pub fn write_entry(path: &Path, id: &str, keys: Option<&[String]>) -> Result<(), String> {
    edit(path, |doc| match keys {
        None => {
            doc.remove(id);
        }
        Some(keys) => {
            let mut new = match keys {
                [one] => toml_edit::value(one.as_str()),
                many => toml_edit::value(
                    many.iter()
                        .map(String::as_str)
                        .collect::<toml_edit::Array>(),
                ),
            };
            // The old value's spacing and trailing comment stay.
            if let (Some(old), Some(value)) =
                (doc.get(id).and_then(|i| i.as_value()), new.as_value_mut())
            {
                *value.decor_mut() = old.decor().clone();
            }
            doc[id] = new;
        }
    })
}

/// Remove every entry of the file at `path`, keeping its comments.
pub fn remove_all(path: &Path) -> Result<(), String> {
    edit(path, |doc| {
        let ids: Vec<String> = doc.iter().map(|(k, _)| k.to_owned()).collect();
        for id in ids {
            doc.remove(&id);
        }
    })
}

fn edit(path: &Path, change: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        format!(
            "{FILE_NAME} is not valid TOML, fix it by hand first: {}",
            e.message()
        )
    })?;
    change(&mut doc);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn key_name(key: &str) -> String {
    match key {
        "enter" => "Enter".into(),
        "escape" => "Esc".into(),
        "tab" => "Tab".into(),
        "space" => "Space".into(),
        "backspace" => "Backspace".into(),
        "delete" => "Del".into(),
        "insert" => "Ins".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        k if k.starts_with('f') && k.len() > 1 && k[1..].bytes().all(|b| b.is_ascii_digit()) => {
            k.to_uppercase()
        }
        k => k.to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_syntax_becomes_gpui_keys() {
        assert_eq!(to_gpui("mod+shift+p", false).unwrap(), "ctrl-shift-p");
        assert_eq!(to_gpui("mod+shift+p", true).unwrap(), "cmd-shift-p");
        assert_eq!(to_gpui("mod+k mod+s", false).unwrap(), "ctrl-k ctrl-s");
        assert_eq!(to_gpui("Ctrl+Alt+H", false).unwrap(), "ctrl-alt-h");
        assert_eq!(to_gpui("ctrl++", false).unwrap(), "ctrl-+");
        assert_eq!(to_gpui("ctrl+-", false).unwrap(), "ctrl--");
        assert_eq!(to_gpui("j", false).unwrap(), "j");
        assert_eq!(to_gpui("+", false).unwrap(), "+");
        assert_eq!(to_gpui("f8", false).unwrap(), "f8");
        assert_eq!(to_gpui("shift+f8", false).unwrap(), "shift-f8");
        // GPUI's own spelling passes through.
        assert_eq!(to_gpui("ctrl-shift-p", false).unwrap(), "ctrl-shift-p");
        assert_eq!(to_gpui("mod-k", true).unwrap(), "cmd-k");
        assert!(to_gpui("hyper+k", false).is_err());
        assert!(to_gpui("ctrl+", false).is_err());
        assert!(to_gpui("   ", false).is_err());
    }

    #[test]
    fn keys_are_shown_the_platform_way() {
        assert_eq!(display("ctrl-shift-p", false), "Ctrl Shift P");
        assert_eq!(display("cmd-shift-p", true), "⌘⇧P");
        assert_eq!(display("cmd-k", true), "⌘K");
        assert_eq!(display("ctrl-k ctrl-s", false), "Ctrl K Ctrl S");
        assert_eq!(display("shift-f8", false), "Shift F8");
        assert_eq!(display("j", false), "J");
        assert_eq!(display("right", false), "→");
        assert_eq!(display("ctrl-`", false), "Ctrl `");
        assert_eq!(display("ctrl--", false), "Ctrl -");
        assert_eq!(display("ctrl-alt-\\", false), "Ctrl Alt \\");
    }

    #[test]
    fn a_file_overrides_by_id_and_reports_what_it_skips() {
        let keys = parse(
            r#"
            "command_palette.open" = "mod+shift+p"
            "diff.next_hunk" = ["j", "down"]
            "terminal.new" = []
            "bad.modifier" = "hyper+x"
            "bad.type" = 3
            "bad.list" = ["mod+k", 4]
            "#,
            false,
        );
        assert_eq!(
            keys.overrides.get("command_palette.open").unwrap(),
            &vec!["ctrl-shift-p".to_owned()]
        );
        assert_eq!(
            keys.overrides.get("diff.next_hunk").unwrap(),
            &vec!["j".to_owned(), "down".to_owned()]
        );
        assert_eq!(
            keys.overrides.get("terminal.new").unwrap(),
            &Vec::<String>::new()
        );
        assert!(!keys.overrides.contains_key("bad.modifier"));
        assert!(!keys.overrides.contains_key("bad.type"));
        assert!(!keys.overrides.contains_key("bad.list"));
        assert_eq!(keys.problems.len(), 3, "{:?}", keys.problems);
        for id in ["bad.list", "bad.modifier", "bad.type"] {
            assert!(
                keys.problems.iter().any(|p| p.contains(id)),
                "{id}: {:?}",
                keys.problems
            );
        }

        let broken = parse("this is = not [toml", false);
        assert!(broken.overrides.is_empty());
        assert_eq!(broken.problems.len(), 1);
    }

    #[test]
    fn a_missing_file_is_no_overrides() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read(&dir.path().join(FILE_NAME), false),
            UserKeys::default()
        );
        std::fs::write(dir.path().join(FILE_NAME), "\"a.b\" = \"mod+b\"").unwrap();
        let keys = read(&dir.path().join(FILE_NAME), true);
        assert_eq!(keys.overrides["a.b"], vec!["cmd-b".to_owned()]);
    }

    #[test]
    fn keys_are_written_in_the_files_syntax() {
        assert_eq!(to_file_syntax("ctrl-shift-p", false), "mod+shift+p");
        assert_eq!(to_file_syntax("cmd-k cmd-s", true), "mod+k mod+s");
        assert_eq!(to_file_syntax("ctrl-k", true), "ctrl+k");
        assert_eq!(to_file_syntax("ctrl--", false), "mod+-");
        assert_eq!(to_file_syntax("ctrl-+", false), "mod++");
        assert_eq!(to_file_syntax("j", false), "j");
        for keys in [
            "ctrl-shift-p",
            "ctrl-k ctrl-s",
            "ctrl--",
            "ctrl-+",
            "alt-j",
            "shift-f8",
            "-",
            "+",
        ] {
            assert_eq!(to_gpui(&to_file_syntax(keys, false), false).unwrap(), keys);
        }
    }

    #[test]
    fn an_entry_is_written_and_removed_keeping_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            "# my keys\n\"a.b\" = \"mod+b\"  # keep\n\n# more\n\"c.d\" = [\"j\", \"down\"]\n\n# the end\n",
        )
        .unwrap();
        write_entry(&path, "a.b", Some(&["mod+x".into()])).unwrap();
        write_entry(&path, "e.f", Some(&["mod+k mod+s".into(), "f1".into()])).unwrap();
        write_entry(&path, "g.h", Some(&[])).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# my keys") && text.contains("# keep") && text.contains("# more"),
            "{text}"
        );
        let keys = parse(&text, false);
        assert_eq!(keys.overrides["a.b"], vec!["ctrl-x".to_owned()]);
        assert_eq!(
            keys.overrides["c.d"],
            vec!["j".to_owned(), "down".to_owned()]
        );
        assert_eq!(
            keys.overrides["e.f"],
            vec!["ctrl-k ctrl-s".to_owned(), "f1".to_owned()]
        );
        assert!(keys.overrides["g.h"].is_empty());
        write_entry(&path, "c.d", None).unwrap();
        assert!(
            !parse(&std::fs::read_to_string(&path).unwrap(), false)
                .overrides
                .contains_key("c.d")
        );
        remove_all(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            parse(&text, false).overrides.is_empty() && text.contains("# the end"),
            "{text}"
        );
        std::fs::write(&path, "not [toml").unwrap();
        assert!(write_entry(&path, "a.b", None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not [toml");
    }
}
