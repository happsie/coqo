// SPDX-License-Identifier: Apache-2.0

//! The `file:line` reference detector: the syntax pass (what looks like a
//! reference) and the check against the workspace (what is one).

use std::sync::LazyLock;

use regex::Regex;

use crate::workspace::{Resolved, Workspace};

/// A reference as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRef {
    /// Byte span in the text.
    pub span: (usize, usize),
    pub path: String,
    pub line: u32,
    pub end_line: Option<u32>,
    pub form: Form,
    /// Inside a fenced code block.
    pub in_code: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Form {
    /// `https://github.com/o/r/blob/<sha>/path#L10-L15`
    Url,
    /// `path:12`, `path:12-14`, `path:12:5`, `path#L12`
    Colon,
    /// `path (line 12)`, `path, lines 3-4`, `line 12 of path`
    Words,
}

/// What the check made of a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A real location: path relative to the root, lines 1-based inclusive.
    Location {
        path: String,
        start: u32,
        end: u32,
    },
    /// The file exists but has fewer lines.
    PastEnd {
        path: String,
    },
    /// The file exists; its length is unknown (corpus without the text).
    LinesUnknown {
        path: String,
        start: u32,
        end: u32,
    },
    Ambiguous(Vec<String>),
    NoSuchFile,
}

// A path: optional drive or root, segments of word characters, dots, dashes
// and `@`, at least one character that is not a digit (so `10:30` is not a
// path).
const PATH: &str = r"(?:[A-Za-z]:)?(?:[\w@.~\-]+/)*[\w@.\-]*[A-Za-z_\-][\w@.\-]*";

static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"https?://[^\s)\]>]+?/blob/[^/\s]+/(?P<path>[^#\s)\]>]+)#L(?P<a>\d+)(?:-L?(?P<b>\d+))?",
    )
    .unwrap()
});
static COLON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?P<path>/?{PATH})(?::|#L)(?P<a>\d+)(?:\s?[-–]\s?L?(?P<b>\d+))?(?::\d+)?\b"
    ))
    .unwrap()
});
static WORDS_AFTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?P<path>/?{PATH}\.[A-Za-z0-9]{{1,8}})`?\s*[(,]?\s*(?:at\s+|on\s+)?(?:lines?|l\.)\s+(?P<a>\d+)(?:\s*(?:-|–|to|and)\s*(?P<b>\d+))?"
    ))
    .unwrap()
});
static WORDS_BEFORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)\blines?\s+(?P<a>\d+)(?:\s*(?:-|–|to)\s*(?P<b>\d+))?\s+(?:of|in)\s+`?(?P<path>/?{PATH}\.[A-Za-z0-9]{{1,8}})"
    ))
    .unwrap()
});

/// Byte ranges of fenced code blocks.
fn fences(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut open: Option<usize> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            match open.take() {
                Some(start) => out.push((start, at + line.len())),
                None => open = Some(at),
            }
        }
        at += line.len();
    }
    if let Some(start) = open {
        out.push((start, text.len()));
    }
    out
}

/// Every reference-looking span, left to right, overlaps removed (a URL
/// wins over the `path#L12` inside it).
pub fn find(text: &str) -> Vec<RawRef> {
    let code = fences(text);
    let mut found: Vec<RawRef> = Vec::new();
    for (re, form) in [
        (&*URL, Form::Url),
        (&*WORDS_BEFORE, Form::Words),
        (&*WORDS_AFTER, Form::Words),
        (&*COLON, Form::Colon),
    ] {
        for c in re.captures_iter(text) {
            let m = c.get(0).unwrap();
            let span = (m.start(), m.end());
            if found.iter().any(|f| f.span.0 < span.1 && span.0 < f.span.1) {
                continue;
            }
            let line: u32 = match c["a"].parse() {
                Ok(l) => l,
                Err(_) => continue,
            };
            let end_line = c.name("b").and_then(|b| b.as_str().parse().ok());
            let path = c["path"].trim_end_matches(['.', ',']).to_owned();
            // A URL scheme or host is not a path (`http://localhost:3000`).
            let before = &text[..span.0];
            let token_start = before
                .rfind(|c: char| {
                    c.is_whitespace() || matches!(c, '(' | '[' | '<' | '`' | '"' | '\'')
                })
                .map_or(0, |i| i + 1);
            if form != Form::Url
                && (text[token_start..span.1].contains("://") || before.ends_with('@'))
            {
                continue;
            }
            found.push(RawRef {
                span,
                path,
                line,
                end_line,
                form,
                in_code: code.iter().any(|(a, b)| *a <= span.0 && span.0 < *b),
            });
        }
    }
    found.sort_by_key(|f| f.span.0);
    found
}

/// The syntax-only filter a detector without the workspace would use: the
/// path has a file extension or a directory.
pub fn plausible(r: &RawRef) -> bool {
    let name = r.path.rsplit('/').next().unwrap_or("");
    r.form == Form::Url
        || r.path.contains('/')
        || name.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && ext.chars().any(|c| c.is_ascii_alphabetic()) && ext.len() <= 8
        })
}

/// Checks a reference against the workspace.
pub fn check(r: &RawRef, ws: &Workspace, hints: &[String]) -> Verdict {
    let start = r.line.max(1);
    let end = r.end_line.filter(|e| *e >= start).unwrap_or(start);
    match ws.resolve(&r.path, hints) {
        Resolved::File(path) => match ws.line_count(&path) {
            Some(n) if (end as usize) <= n => Verdict::Location { path, start, end },
            Some(_) => Verdict::PastEnd { path },
            None => Verdict::LinesUnknown { path, start, end },
        },
        Resolved::Ambiguous(c) => Verdict::Ambiguous(c),
        Resolved::Unknown => Verdict::NoSuchFile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(text: &str) -> Vec<(String, u32, Option<u32>)> {
        find(text)
            .into_iter()
            .map(|r| (r.path, r.line, r.end_line))
            .collect()
    }

    #[test]
    fn forms() {
        assert_eq!(
            paths("see src/a.ts:12"),
            vec![("src/a.ts".into(), 12, None)]
        );
        assert_eq!(paths("(db.ts:15-42)"), vec![("db.ts".into(), 15, Some(42))]);
        assert_eq!(paths("`cart.ts:35:7`"), vec![("cart.ts".into(), 35, None)]);
        assert_eq!(
            paths("File: index-conversations:1-31"),
            vec![("index-conversations".into(), 1, Some(31))]
        );
        assert_eq!(
            paths("in auth.ts (line 30)"),
            vec![("auth.ts".into(), 30, None)]
        );
        assert_eq!(
            paths("lines 3-4 of `config.ts`"),
            vec![("config.ts".into(), 3, Some(4))]
        );
        assert_eq!(
            paths("https://github.com/o/r/blob/abc123/src/x.rs#L10-L15"),
            vec![("src/x.rs".into(), 10, Some(15))]
        );
    }

    #[test]
    fn not_references() {
        assert!(
            paths("at 10:30 on localhost:3000, ratio 1:4")
                .iter()
                .all(|(p, _, _)| p != "10")
        );
        assert!(paths("http://localhost:3000/x").is_empty());
        let r = find("Node 20.11 and localhost:8080");
        assert!(r.iter().all(|r| !plausible(r)));
    }
}
