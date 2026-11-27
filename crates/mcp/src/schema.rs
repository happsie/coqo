// SPDX-License-Identifier: Apache-2.0

//! Checks tool arguments against a tool's `input_schema` before it runs.
//!
//! A small JSON Schema validator for the keywords tool schemas use:
//! `type` (one or a list; `integer` is a number without a fraction),
//! `enum`, `const`, `properties`, `required`, `additionalProperties` (a
//! boolean or a schema), `items` (a schema), `minItems`, `maxItems`,
//! `minLength`, `maxLength` (in characters), `minimum`, `maximum`,
//! `exclusiveMinimum`, `exclusiveMaximum` (numbers), `allOf`, `anyOf`,
//! `oneOf` and `not`. Other keywords (`$ref`, `pattern`, `format`, …) are
//! ignored, so a schema that uses them is checked less strictly, never
//! wrongly refused. Boolean schemas (`true`, `false`) work.
//!
//! Errors name where they are (`` `root` ``, `` `edits[2].path` ``) and what was
//! expected, for the model to correct its call.
//!
//! ```
//! use coqo_mcp::schema::validate;
//! use serde_json::json;
//!
//! let schema = json!({
//!     "type": "object",
//!     "properties": { "path": { "type": "string" }, "line": { "type": "integer", "minimum": 1 } },
//!     "required": ["path"],
//!     "additionalProperties": false,
//! });
//! assert!(validate(&schema, &json!({ "path": "a.rs", "line": 3 })).is_ok());
//! let err = validate(&schema, &json!({ "line": 0, "extra": true })).unwrap_err();
//! assert_eq!(
//!     err,
//!     "`path` is required; `line` must be at least 1; `extra` is not a known argument"
//! );
//! ```

use serde_json::{Map, Value};

/// Most errors reported for one call.
const MAX_ERRORS: usize = 5;

/// Check `value` against `schema`. The error lists what is wrong, joined
/// with `; `.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    let mut errors = Vec::new();
    check(schema, value, &Path::Root, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        errors.truncate(MAX_ERRORS);
        Err(errors.join("; "))
    }
}

/// Check call arguments (an object) against a tool's object schema.
pub fn validate_arguments(
    schema: &Map<String, Value>,
    args: &Map<String, Value>,
) -> Result<(), String> {
    // Cloning keeps the API simple; argument objects are small.
    validate(&Value::Object(schema.clone()), &Value::Object(args.clone()))
}

enum Path<'a> {
    Root,
    Key(&'a Path<'a>, &'a str),
    Index(&'a Path<'a>, usize),
}

impl Path<'_> {
    fn render(&self, out: &mut String) {
        match self {
            Path::Root => {}
            Path::Key(parent, key) => {
                parent.render(out);
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Path::Index(parent, i) => {
                parent.render(out);
                out.push_str(&format!("[{i}]"));
            }
        }
    }

    fn describe(&self) -> String {
        let mut out = String::new();
        self.render(&mut out);
        if out.is_empty() {
            "the arguments".to_owned()
        } else {
            format!("`{out}`")
        }
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if is_integer(n) => "an integer",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn is_integer(n: &serde_json::Number) -> bool {
    n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
}

fn has_type(value: &Value, ty: &str) -> bool {
    match ty {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_number().is_some_and(is_integer),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        // An unknown type name: don't refuse on it.
        _ => true,
    }
}

fn article(ty: &str) -> String {
    match ty {
        "null" => "null".to_owned(),
        "integer" | "array" | "object" => format!("an {ty}"),
        _ => format!("a {ty}"),
    }
}

fn check(schema: &Value, value: &Value, path: &Path<'_>, errors: &mut Vec<String>) {
    let schema = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{} is not allowed", path.describe()));
            return;
        }
        Value::Object(o) => o,
        // Not a schema: nothing to check.
        _ => return,
    };
    if let Some(ty) = schema.get("type") {
        let types: Vec<&str> = match ty {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() && !types.iter().any(|t| has_type(value, t)) {
            let expected: Vec<_> = types.iter().map(|t| article(t)).collect();
            errors.push(format!(
                "{} must be {}, not {}",
                path.describe(),
                expected.join(" or "),
                type_name(value)
            ));
            // The other keywords would only repeat it.
            return;
        }
    }

    if let Some(Value::Array(options)) = schema.get("enum")
        && !options.contains(value)
    {
        let options: Vec<_> = options.iter().map(Value::to_string).collect();
        errors.push(format!(
            "{} must be one of {}",
            path.describe(),
            options.join(", ")
        ));
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        errors.push(format!("{} must be {expected}", path.describe()));
    }

    match value {
        Value::Object(object) => check_object(schema, object, path, errors),
        Value::Array(items) => check_array(schema, items, path, errors),
        Value::String(s) => {
            let len = s.chars().count();
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && (len as u64) < min
            {
                errors.push(format!(
                    "{} must have at least {min} characters",
                    path.describe()
                ));
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && (len as u64) > max
            {
                errors.push(format!(
                    "{} must have at most {max} characters",
                    path.describe()
                ));
            }
        }
        Value::Number(n) => check_number(schema, n.as_f64().unwrap_or(f64::NAN), path, errors),
        _ => {}
    }

    if let Some(Value::Array(all)) = schema.get("allOf") {
        for sub in all {
            check(sub, value, path, errors);
        }
    }
    if let Some(Value::Array(any)) = schema.get("anyOf")
        && !any.iter().any(|sub| passes(sub, value))
    {
        errors.push(format!(
            "{} matches none of the allowed forms",
            path.describe()
        ));
    }
    if let Some(Value::Array(one)) = schema.get("oneOf") {
        let matching = one.iter().filter(|sub| passes(sub, value)).count();
        if matching != 1 {
            errors.push(format!(
                "{} must match exactly one of the allowed forms ({matching} match)",
                path.describe()
            ));
        }
    }
    if let Some(not) = schema.get("not")
        && passes(not, value)
    {
        errors.push(format!("{} is not allowed in this form", path.describe()));
    }
}

fn passes(schema: &Value, value: &Value) -> bool {
    let mut errors = Vec::new();
    check(schema, value, &Path::Root, &mut errors);
    errors.is_empty()
}

fn check_object(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &Path<'_>,
    errors: &mut Vec<String>,
) {
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                errors.push(format!("{} is required", Path::Key(path, key).describe()));
            }
        }
    }
    for (key, value) in object {
        let here = Path::Key(path, key);
        match properties.and_then(|p| p.get(key)) {
            Some(sub) => check(sub, value, &here, errors),
            None => match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    errors.push(format!("{} is not a known argument", here.describe()));
                }
                Some(sub @ Value::Object(_)) => check(sub, value, &here, errors),
                _ => {}
            },
        }
    }
}

fn check_array(
    schema: &Map<String, Value>,
    items: &[Value],
    path: &Path<'_>,
    errors: &mut Vec<String>,
) {
    let len = items.len() as u64;
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
        && len < min
    {
        errors.push(format!(
            "{} must have at least {min} items",
            path.describe()
        ));
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
        && len > max
    {
        errors.push(format!("{} must have at most {max} items", path.describe()));
    }
    if let Some(item) = schema.get("items")
        && (item.is_object() || item.is_boolean())
    {
        for (i, value) in items.iter().enumerate() {
            check(item, value, &Path::Index(path, i), errors);
        }
    }
}

fn check_number(schema: &Map<String, Value>, n: f64, path: &Path<'_>, errors: &mut Vec<String>) {
    let bound = |key| schema.get(key).and_then(Value::as_f64);
    let show = |f: f64| {
        if f.fract() == 0.0 && f.abs() < 1e15 {
            format!("{}", f as i64)
        } else {
            f.to_string()
        }
    };
    if let Some(min) = bound("minimum")
        && n < min
    {
        errors.push(format!(
            "{} must be at least {}",
            path.describe(),
            show(min)
        ));
    }
    if let Some(max) = bound("maximum")
        && n > max
    {
        errors.push(format!("{} must be at most {}", path.describe(), show(max)));
    }
    if let Some(min) = bound("exclusiveMinimum")
        && n <= min
    {
        errors.push(format!(
            "{} must be more than {}",
            path.describe(),
            show(min)
        ));
    }
    if let Some(max) = bound("exclusiveMaximum")
        && n >= max
    {
        errors.push(format!(
            "{} must be less than {}",
            path.describe(),
            show(max)
        ));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn err(schema: Value, value: Value) -> String {
        validate(&schema, &value).unwrap_err()
    }

    #[test]
    fn types() {
        let s = json!({ "type": "integer" });
        assert!(validate(&s, &json!(3)).is_ok());
        assert!(validate(&s, &json!(3.0)).is_ok());
        assert_eq!(
            err(s, json!(3.5)),
            "the arguments must be an integer, not a number"
        );
        let s = json!({ "type": ["string", "null"] });
        assert!(validate(&s, &json!(null)).is_ok());
        assert_eq!(
            err(s, json!(1)),
            "the arguments must be a string or null, not an integer"
        );
        assert!(validate(&json!({ "type": "number" }), &json!(1)).is_ok());
        assert!(validate(&json!({ "type": "made-up" }), &json!(1)).is_ok());
    }

    #[test]
    fn objects() {
        let s = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "range": {
                    "type": "object",
                    "properties": { "line": { "type": "integer", "minimum": 1 } },
                    "required": ["line"],
                },
            },
            "required": ["path"],
            "additionalProperties": false,
        });
        assert!(validate(&s, &json!({ "path": "a" })).is_ok());
        assert_eq!(
            err(s.clone(), json!({ "path": "", "range": { "line": 0 } })),
            "`path` must have at least 1 characters; `range.line` must be at least 1"
        );
        assert_eq!(
            err(s.clone(), json!({ "range": {} })),
            "`path` is required; `range.line` is required"
        );
        assert_eq!(
            err(s.clone(), json!({ "path": 1, "x": 2 })),
            "`path` must be a string, not an integer; `x` is not a known argument"
        );
        // Additional properties are allowed unless the schema says not.
        let open = json!({ "type": "object", "properties": {} });
        assert!(validate(&open, &json!({ "x": 1 })).is_ok());
        let typed = json!({ "type": "object", "additionalProperties": { "type": "string" } });
        assert_eq!(
            err(typed, json!({ "x": 1 })),
            "`x` must be a string, not an integer"
        );
    }

    #[test]
    fn arrays_enums_and_combinators() {
        let s = json!({
            "type": "array",
            "items": { "enum": ["a", "b"] },
            "minItems": 1,
            "maxItems": 2,
        });
        assert!(validate(&s, &json!(["a", "b"])).is_ok());
        assert_eq!(
            err(s.clone(), json!([])),
            "the arguments must have at least 1 items"
        );
        assert_eq!(
            err(s.clone(), json!(["a", "c", "b"])),
            "the arguments must have at most 2 items; `[1]` must be one of \"a\", \"b\""
        );
        let any = json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }] });
        assert!(validate(&any, &json!(1)).is_ok());
        assert_eq!(
            err(any, json!(true)),
            "the arguments matches none of the allowed forms"
        );
        let one = json!({ "oneOf": [{ "type": "number" }, { "type": "integer" }] });
        assert!(validate(&one, &json!(1.5)).is_ok());
        assert!(validate(&one, &json!(1)).is_err());
        let not = json!({ "not": { "const": 0 } });
        assert!(validate(&not, &json!(1)).is_ok());
        assert!(validate(&not, &json!(0)).is_err());
        let all = json!({ "allOf": [{ "minimum": 0 }, { "exclusiveMaximum": 10 }] });
        assert!(validate(&all, &json!(9.5)).is_ok());
        assert_eq!(err(all, json!(10)), "the arguments must be less than 10");
    }

    #[test]
    fn boolean_and_unknown_keywords() {
        assert!(validate(&json!(true), &json!({ "x": 1 })).is_ok());
        assert_eq!(err(json!(false), json!(1)), "the arguments is not allowed");
        // `$ref`, `pattern` and `format` are not checked.
        let s = json!({ "$ref": "#/defs/x", "pattern": "^a$", "format": "uri" });
        assert!(validate(&s, &json!("zzz")).is_ok());
    }

    #[test]
    fn errors_are_capped() {
        let s = json!({ "type": "object", "additionalProperties": false });
        let args = json!({ "a": 1, "b": 1, "c": 1, "d": 1, "e": 1, "f": 1, "g": 1 });
        assert_eq!(
            err(s, args).matches("not a known argument").count(),
            MAX_ERRORS
        );
    }
}
