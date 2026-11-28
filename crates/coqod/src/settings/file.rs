// SPDX-License-Identifier: Apache-2.0

//! Reading and writing one `settings.toml`.
//!
//! [`parse`] flattens the file into dotted keys (`[files] auto_reveal` is
//! `files.auto_reveal`) with the line each value is on. [`write_value`]
//! changes one key with `toml_edit`, so the user's comments, blank lines and
//! order stay as they were.

use std::collections::BTreeMap;
use std::path::Path;

use coqo_protocol::methods::settings::SettingsProblem;
use serde_json::Value;
use toml_edit::{DocumentMut, Item, TableLike};

/// A value read from a file, and the 1-based line it is on.
#[derive(Debug, Clone, PartialEq)]
pub struct RawValue {
    /// The value, as JSON.
    pub value: Value,
    /// Where it is.
    pub line: Option<u32>,
}

/// A file's values by dotted key.
pub type RawValues = BTreeMap<String, RawValue>;

fn line_of(text: &str, span: Option<std::ops::Range<usize>>) -> Option<u32> {
    let start = span?.start.min(text.len());
    let line = text[..start].matches('\n').count() + 1;
    u32::try_from(line).ok()
}

/// A file-level problem for a parse error.
fn syntax_problem(file: &str, text: &str, e: &toml_edit::TomlError) -> SettingsProblem {
    let line = line_of(text, e.span());
    let message = e.message().trim().trim_end_matches('.').to_owned();
    SettingsProblem {
        key: None,
        line,
        message: match line {
            Some(n) => format!("{file} is not valid TOML: line {n}: {message}"),
            None => format!("{file} is not valid TOML: {message}"),
        },
    }
}

/// Parse a settings file. `file` names it in messages (`settings.toml`).
pub fn parse(file: &str, text: &str) -> Result<RawValues, SettingsProblem> {
    let doc = toml_edit::Document::parse(text).map_err(|e| syntax_problem(file, text, &e))?;
    let mut out = RawValues::new();
    flatten_table(text, "", doc.as_table(), &mut out);
    Ok(out)
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn flatten_table(text: &str, prefix: &str, table: &dyn TableLike, out: &mut RawValues) {
    for (key, item) in table.iter() {
        let path = join(prefix, key);
        match item {
            Item::None => {}
            Item::Value(toml_edit::Value::InlineTable(t)) => flatten_table(text, &path, t, out),
            Item::Value(v) => {
                out.insert(
                    path,
                    RawValue {
                        value: to_json(v),
                        line: line_of(text, v.span()),
                    },
                );
            }
            Item::Table(t) => flatten_table(text, &path, t, out),
            Item::ArrayOfTables(a) => {
                let value = Value::Array(
                    a.iter()
                        .map(|t| {
                            let mut inner = RawValues::new();
                            flatten_table(text, "", t, &mut inner);
                            Value::Object(inner.into_iter().map(|(k, v)| (k, v.value)).collect())
                        })
                        .collect(),
                );
                out.insert(
                    path,
                    RawValue {
                        value,
                        line: line_of(text, a.span()),
                    },
                );
            }
        }
    }
}

fn to_json(v: &toml_edit::Value) -> Value {
    use toml_edit::Value as T;
    match v {
        T::String(s) => Value::String(s.value().clone()),
        T::Integer(i) => Value::from(*i.value()),
        T::Float(f) => serde_json::Number::from_f64(*f.value()).map_or(Value::Null, Value::Number),
        T::Boolean(b) => Value::Bool(*b.value()),
        T::Datetime(d) => Value::String(d.value().to_string()),
        T::Array(a) => Value::Array(a.iter().map(to_json).collect()),
        T::InlineTable(t) => {
            Value::Object(t.iter().map(|(k, v)| (k.to_owned(), to_json(v))).collect())
        }
    }
}

/// A JSON value as TOML, for the types settings have.
fn to_toml(v: &Value) -> Result<toml_edit::Value, String> {
    Ok(match v {
        Value::Bool(b) => (*b).into(),
        Value::String(s) => s.as_str().into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64().ok_or("the number is out of range")?.into(),
        },
        Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(to_toml(item)?);
            }
            toml_edit::Value::Array(array)
        }
        Value::Null | Value::Object(_) => return Err(format!("`{v}` can't be written")),
    })
}

/// Why [`write_value`] did not write.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteError {
    /// The file is not valid TOML.
    Syntax(SettingsProblem),
    /// The key's path is taken by something that is not a table, or the
    /// value can't be written.
    Conflict(String),
}

/// `text` with `key` set to `value` (or removed, with `None`), keeping
/// everything else as written. A new key goes into its table (`[files]` for
/// `files.auto_reveal`), which is added at the end when missing.
pub fn write_value(
    file: &str,
    text: &str,
    key: &str,
    value: Option<&Value>,
) -> Result<String, WriteError> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| WriteError::Syntax(syntax_problem(file, text, &e)))?;
    let segments: Vec<&str> = key.split('.').collect();
    let Some((last, tables)) = segments.split_last() else {
        return Err(WriteError::Conflict("the key is empty".into()));
    };
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for (i, segment) in tables.iter().enumerate() {
        if value.is_none() && !table.contains_key(segment) {
            // Nothing to remove.
            return Ok(doc.to_string());
        }
        let entry = table.entry(segment).or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            // `[claude.acp]` alone, not an empty `[claude]` before it.
            t.set_implicit(i + 1 < tables.len());
            Item::Table(t)
        });
        let path = segments[..=i].join(".");
        table = entry.as_table_like_mut().ok_or_else(|| {
            WriteError::Conflict(format!("`{path}` is set to a value, not a table"))
        })?;
    }
    match value {
        None => {
            table.remove(last);
        }
        Some(value) => {
            let mut new = to_toml(value).map_err(WriteError::Conflict)?;
            match table.get_mut(last) {
                Some(Item::Value(old)) => {
                    // Keep the comment after the old value.
                    *new.decor_mut() = old.decor().clone();
                    *old = new;
                }
                Some(Item::None) | None => {
                    table.insert(last, Item::Value(new));
                }
                Some(_) => {
                    return Err(WriteError::Conflict(format!(
                        "`{key}` is a table in the file, not a value"
                    )));
                }
            }
        }
    }
    Ok(doc.to_string())
}

/// Write `text` to `path` through a temporary file in the same folder, so a
/// reader never sees half a file. Creates the folder.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the path has no folder"))?;
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    if let Ok(meta) = std::fs::metadata(path) {
        // Keep the user's permissions.
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn flattens_tables_into_dotted_keys_with_lines() {
        let text = "# mine\n[files]\nauto_reveal = true\n\n[claude.acp]\nplan-logins = false\n[x]\ny = { z = 1 }\n";
        let values = parse("settings.toml", text).unwrap();
        assert_eq!(values["files.auto_reveal"].value, json!(true));
        assert_eq!(values["files.auto_reveal"].line, Some(3));
        assert_eq!(values["claude.acp.plan-logins"].value, json!(false));
        assert_eq!(values["x.y.z"].value, json!(1));
        let dotted = parse("s", "files.auto_reveal = true\n").unwrap();
        assert_eq!(dotted["files.auto_reveal"].value, json!(true));
    }

    #[test]
    fn a_syntax_error_names_the_line() {
        let e = parse("settings.toml", "[files]\nauto_reveal = \n").unwrap_err();
        assert_eq!(e.line, Some(2), "{e:?}");
        assert!(
            e.message
                .starts_with("settings.toml is not valid TOML: line 2"),
            "{e:?}"
        );
    }

    #[test]
    fn writes_keep_comments_and_order() {
        let text = "# My settings\n\n[appearance]\n# dark at night\ntheme = \"dark\" # was light\n\n[editor]\nfont = \"x\"\n";
        let out = write_value(
            "settings.toml",
            text,
            "appearance.theme",
            Some(&json!("light")),
        )
        .unwrap();
        assert_eq!(
            out,
            "# My settings\n\n[appearance]\n# dark at night\ntheme = \"light\" # was light\n\n[editor]\nfont = \"x\"\n"
        );
        let out = write_value(
            "settings.toml",
            &out,
            "files.auto_reveal",
            Some(&json!(true)),
        )
        .unwrap();
        assert!(
            out.ends_with("font = \"x\"\n\n[files]\nauto_reveal = true\n"),
            "{out}"
        );
        let out = write_value("settings.toml", &out, "files.auto_reveal", None).unwrap();
        assert!(!out.contains("auto_reveal"), "{out}");
        assert!(out.starts_with("# My settings\n"), "{out}");
        // Removing what isn't there changes nothing.
        assert_eq!(write_value("s", text, "nope.key", None).unwrap(), text);
    }

    #[test]
    fn nested_keys_get_one_table() {
        let out = write_value("s", "", "claude.acp.plan-logins", Some(&json!(false))).unwrap();
        assert_eq!(out, "[claude.acp]\nplan-logins = false\n");
        let out = write_value("s", "", "a.list", Some(&json!(["x", "y"]))).unwrap();
        assert_eq!(out, "[a]\nlist = [\"x\", \"y\"]\n");
    }

    #[test]
    fn never_writes_over_a_broken_file_or_a_value() {
        assert!(matches!(
            write_value("s", "[files\n", "files.a", Some(&json!(true))),
            Err(WriteError::Syntax(_))
        ));
        assert!(matches!(
            write_value("s", "files = 3\n", "files.a", Some(&json!(true))),
            Err(WriteError::Conflict(_))
        ));
    }

    #[test]
    fn atomic_writes_create_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".coqo/settings.toml");
        write_atomic(&path, "a = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 1\n");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }
}
