// SPDX-License-Identifier: Apache-2.0

//! Findings an agent writes as text (`file:line - issue`), found in a chat
//! message so the chat can pin them as gutter notes (#345, ADR 0030).
//!
//! Two passes, as the spike S9 measured them:
//!
//! 1. **Syntax** ([`find`]): what looks like a reference, outside fenced
//!    code: `path:12`, `path:12-14`, `path:12:5`, `path#L12`, a bare
//!    continuation (`` `a.py:17`, `:32` ``), "`path` (line 12)" with words
//!    in between, "line 12 of `path`" and GitHub blob links.
//! 2. **The workspace** ([`Workspace::check`]): what is one. The file must
//!    exist (an exact path, or the one file the name ends with, narrowed by
//!    the files the turn touched) and the line must be in range. Without
//!    this pass 18 % of the references in real review comments were bogus
//!    (`localhost:8080`, stale names, lines past the end). Anything that
//!    doesn't pass is dropped, silently.
//!
//! The words of a finding's line or heading set its [`FindingLevel`], and
//! with it the note's colour (`docs/design/questions.md`).

use std::collections::HashMap;
use std::sync::LazyLock;

use coqo_protocol::methods::questions::FindingLevel;
use regex::Regex;

/// Longest finding text kept, in characters.
const MAX_TEXT: usize = 500;
/// Most words allowed between a path and its "(line N)".
const BETWEEN: usize = 80;

/// A reference as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRef {
    /// Byte span in the text.
    pub span: (usize, usize),
    pub path: String,
    /// 1-based.
    pub line: u32,
    /// 1-based, inclusive, if a range.
    pub end_line: Option<u32>,
    /// Inside a fenced code block.
    pub in_code: bool,
}

/// What the workspace made of a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A real location: path as the workspace lists it, 1-based lines.
    Location {
        path: String,
        start: u32,
        end: u32,
    },
    /// The file exists but has fewer lines.
    PastEnd,
    /// The file exists, but nobody knows its length (the daemon checks it
    /// when the note is pinned).
    LinesUnknown {
        path: String,
        start: u32,
        end: u32,
    },
    /// Several files end with the name and the turn's files don't pick one.
    Ambiguous,
    NoSuchFile,
}

// A path: optional drive or root, segments of word characters, dots, dashes
// and `@`, with at least one character that is not a digit (so `10:30` is
// not a path).
const PATH: &str = r"(?:[A-Za-z]:)?(?:[\w@.~\-]+/)*[\w@.\-]*[A-Za-z_\-][\w@.\-]*";

fn regex(pattern: &str) -> Regex {
    // The patterns are constants, tested below.
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad pattern {pattern}: {e}"))
}

static URL: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"https?://[^\s)\]>]+?/blob/[^/\s]+/(?P<path>[^#\s)\]>]+)#L(?P<a>\d+)(?:-L?(?P<b>\d+))?")
});
static COLON: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?P<path>/?{PATH})(?::|#L)(?P<a>\d+)(?:\s?[-–]\s?L?(?P<b>\d+))?(?::\d+)?\b"
    ))
});
/// `:32` or `, :32-35` right after a reference.
static CONTINUATION: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^`?\s*(?:,|;|&|\band\b)?\s*`?:(?P<a>\d+)(?:\s?[-–]\s?(?P<b>\d+))?\b`?")
});
static WORDS_AFTER: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)(?P<path>/?{PATH}\.[A-Za-z0-9]{{1,8}})`?\s*[(,]?\s*(?:at\s+|on\s+)?(?:lines?|l\.)\s+(?P<a>\d+)(?:\s*(?:-|–|to|and)\s*(?P<b>\d+))?"
    ))
});
static WORDS_BEFORE: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)\blines?\s+(?P<a>\d+)(?:\s*(?:-|–|to)\s*(?P<b>\d+))?\s+(?:of|in)\s+`?(?P<path>/?{PATH}\.[A-Za-z0-9]{{1,8}})"
    ))
});
/// "`CLAUDE.md` ... (line 50)": words between the path and the line.
static WORDS_BETWEEN: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)(?P<path>/?{PATH})`?(?P<between>[^\n]{{1,{BETWEEN}}}?)\(\s*(?:at\s+)?lines?\s+(?P<a>\d+)(?:\s*(?:-|–|to)\s*(?P<b>\d+))?\s*\)"
    ))
});

/// Whether a word looks like a path or a file name: a directory, or a
/// name with a letter extension.
fn path_like(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !(c.is_alphanumeric() || c == '/' || c == '.'));
    let name = word.rsplit('/').next().unwrap_or("");
    word.contains('/')
        || name.rsplit_once('.').is_some_and(|(_, ext)| {
            let dotfile = word.ends_with(&format!(".{ext}")) && name.starts_with('.');
            ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
                && (ext.len() <= 8 || dotfile)
        })
}

/// Byte ranges of fenced code blocks.
fn fences(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut open: Option<usize> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
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

/// The matches of `re`: non-overlapping, or (`every`) one from each start,
/// for a pattern whose leftmost match may hide a better one inside.
fn matches<'t>(re: &Regex, text: &'t str, every: bool) -> Vec<regex::Captures<'t>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at <= text.len() {
        let Some(c) = re.captures_at(text, at) else {
            break;
        };
        let Some(m) = c.get(0) else { break };
        at = if every {
            let next = text[m.start()..].chars().next().map_or(1, char::len_utf8);
            m.start() + next
        } else {
            m.end().max(at + 1)
        };
        out.push(c);
    }
    out
}

/// Every reference-looking span, left to right, overlaps removed (a URL
/// wins over the `path#L12` inside it).
pub fn find(text: &str) -> Vec<RawRef> {
    let code = fences(text);
    let mut found: Vec<RawRef> = Vec::new();
    let add = |found: &mut Vec<RawRef>, span: (usize, usize), path: String, a: u32, b| {
        if found.iter().any(|f| f.span.0 < span.1 && span.0 < f.span.1) {
            return false;
        }
        found.push(RawRef {
            span,
            path,
            line: a,
            end_line: b,
            in_code: code.iter().any(|(s, e)| *s <= span.0 && span.0 < *e),
        });
        true
    };
    for (re, url, between) in [
        (&*URL, true, false),
        (&*WORDS_BEFORE, false, false),
        (&*WORDS_AFTER, false, false),
        (&*COLON, false, false),
        (&*WORDS_BETWEEN, false, true),
    ] {
        for c in matches(re, text, between) {
            let Some(m) = c.get(0) else { continue };
            let span = (m.start(), m.end());
            let Ok(line) = c["a"].parse::<u32>() else {
                continue;
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
            if !url && (text[token_start..span.1].contains("://") || before.ends_with('@')) {
                continue;
            }
            if between {
                // Only a file name with no other file in between.
                let words = &c["between"];
                let line_before = before.rsplit('\n').next().unwrap_or("");
                let earlier = line_before.split_whitespace().rev().take(6);
                if !path_like(&path)
                    || words.split_whitespace().any(path_like)
                    || earlier.into_iter().any(path_like)
                {
                    continue;
                }
            }
            if !add(&mut found, span, path.clone(), line, end_line) || url {
                continue;
            }
            // `a.py:17`, `:32`, `:35`: the continuations name the same file.
            let mut end = span.1;
            while let Some(next) = CONTINUATION.captures(&text[end..]) {
                let (Some(m), Ok(a)) = (next.get(0), next["a"].parse::<u32>()) else {
                    break;
                };
                let b = next.name("b").and_then(|b| b.as_str().parse().ok());
                let at = (end, end + m.end());
                if !add(&mut found, at, path.clone(), a, b) {
                    break;
                }
                end = at.1;
            }
        }
    }
    found.sort_by_key(|f| f.span.0);
    found
}

/// The files a reference can point at.
#[derive(Debug, Default, Clone)]
pub struct Workspace {
    /// `/`-separated paths relative to a root, as the workspace lists them
    /// (with the root's name first when there are several roots).
    pub paths: Vec<String>,
    /// Line counts, for the files whose length is known.
    pub counts: HashMap<String, usize>,
    /// The roots' absolute paths, to strip from absolute references.
    pub roots: Vec<String>,
}

impl Workspace {
    /// The file a path as written names: exact, else the one file it is a
    /// suffix of (`cart.ts` is `src/cart/cart.ts`), narrowed by `hints`
    /// (files the turn touched) when several match.
    fn resolve(&self, raw: &str, hints: &[String]) -> Result<String, Verdict> {
        let mut p =
            raw.trim_matches(|c| matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '*'));
        for root in &self.roots {
            if let Some(rest) = p.strip_prefix(root.trim_end_matches('/'))
                && rest.starts_with('/')
            {
                p = rest;
                break;
            }
        }
        for prefix in ["./", "a/", "b/"] {
            if let Some(rest) = p.strip_prefix(prefix)
                && !self.paths.iter().any(|f| f == p)
            {
                p = rest;
            }
        }
        let p = p.trim_start_matches('/');
        if p.is_empty() {
            return Err(Verdict::NoSuchFile);
        }
        if self.paths.iter().any(|f| f == p) {
            return Ok(p.to_owned());
        }
        let suffix = format!("/{p}");
        let matches: Vec<&String> = self.paths.iter().filter(|f| f.ends_with(&suffix)).collect();
        match matches.len() {
            0 => Err(Verdict::NoSuchFile),
            1 => Ok(matches[0].clone()),
            _ => {
                let hinted: Vec<&&String> = matches.iter().filter(|m| hints.contains(m)).collect();
                match hinted.as_slice() {
                    [one] => Ok((**one).clone()),
                    _ => Err(Verdict::Ambiguous),
                }
            }
        }
    }

    /// Checks a reference against the workspace.
    pub fn check(&self, r: &RawRef, hints: &[String]) -> Verdict {
        let start = r.line.max(1);
        let end = r.end_line.filter(|e| *e >= start).unwrap_or(start);
        match self.resolve(&r.path, hints) {
            Ok(path) => match self.counts.get(&path) {
                Some(n) if (end as usize) <= *n => Verdict::Location { path, start, end },
                Some(_) => Verdict::PastEnd,
                None => Verdict::LinesUnknown { path, start, end },
            },
            Err(v) => v,
        }
    }
}

/// A finding that passed the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// As the workspace lists it.
    pub path: String,
    /// 1-based, inclusive.
    pub start: u32,
    pub end: u32,
    /// Whether the line count was not known to the check.
    pub unchecked_lines: bool,
    /// The message's line it is on, markup kept (a list item).
    pub text: String,
    pub level: FindingLevel,
}

fn rank(level: FindingLevel) -> u8 {
    match level {
        FindingLevel::Critical => 2,
        FindingLevel::Warning => 1,
        FindingLevel::Note => 0,
    }
}

/// The severity words in `s`, the strongest one: critical, bug, error →
/// critical; warning, should → warning; nit, suggestion → note.
fn level_of(s: &str) -> Option<FindingLevel> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter_map(|word| {
            Some(match word.to_ascii_lowercase().as_str() {
                "critical" | "blocker" | "bug" | "bugs" | "error" | "errors" | "high" => {
                    FindingLevel::Critical
                }
                "warning" | "warnings" | "should" | "important" | "major" | "medium" => {
                    FindingLevel::Warning
                }
                "nit" | "nits" | "suggestion" | "suggestions" | "minor" | "low" => {
                    FindingLevel::Note
                }
                _ => return None,
            })
        })
        .max_by_key(|l| rank(*l))
}

fn clean(line: &str) -> String {
    let line = line.trim();
    let line = line.trim_start_matches('#').trim_start();
    let line = line
        .strip_prefix(['-', '*', '+'])
        .filter(|rest| rest.starts_with(' '))
        .unwrap_or(line);
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let line = match line[digits..].strip_prefix(['.', ')']) {
        Some(rest) if digits > 0 && rest.starts_with(' ') => rest,
        _ => line,
    };
    line.trim().chars().take(MAX_TEXT).collect()
}

/// The findings of a completed message: references that pass the
/// workspace check outside code blocks, one per location. `hints` are the
/// files the turn touched.
pub fn detect(message: &str, ws: &Workspace, hints: &[String]) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for r in find(message) {
        if r.in_code {
            continue;
        }
        let (path, start, end, unchecked_lines) = match ws.check(&r, hints) {
            Verdict::Location { path, start, end } => (path, start, end, false),
            Verdict::LinesUnknown { path, start, end } => (path, start, end, true),
            _ => continue,
        };
        if out.iter().any(|f| f.path == path && f.start == start) {
            continue;
        }
        let line_start = message[..r.span.0].rfind('\n').map_or(0, |i| i + 1);
        let line_end = message[r.span.1..]
            .find('\n')
            .map_or(message.len(), |i| r.span.1 + i);
        let line = &message[line_start..line_end];
        let heading = message[..line_start]
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with('#'))
            .unwrap_or("");
        out.push(Finding {
            level: level_of(heading)
                .or_else(|| level_of(line))
                .unwrap_or_default(),
            path,
            start,
            end,
            unchecked_lines,
            text: clean(line),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn refs(text: &str) -> Vec<(String, u32, Option<u32>)> {
        find(text)
            .into_iter()
            .map(|r| (r.path, r.line, r.end_line))
            .collect()
    }

    fn ws(files: &[(&str, usize)]) -> Workspace {
        Workspace {
            paths: files.iter().map(|(p, _)| (*p).to_owned()).collect(),
            counts: files.iter().map(|(p, n)| ((*p).to_owned(), *n)).collect(),
            roots: vec!["/work/shop".into()],
        }
    }

    #[test]
    fn the_forms() {
        assert_eq!(refs("see src/a.ts:12"), [("src/a.ts".into(), 12, None)]);
        assert_eq!(refs("(db.ts:15-42)"), [("db.ts".into(), 15, Some(42))]);
        assert_eq!(refs("`cart.ts:35:7`"), [("cart.ts".into(), 35, None)]);
        assert_eq!(refs("a.rs#L7"), [("a.rs".into(), 7, None)]);
        assert_eq!(refs("in auth.ts (line 30)"), [("auth.ts".into(), 30, None)]);
        assert_eq!(
            refs("lines 3-4 of `config.ts`"),
            [("config.ts".into(), 3, Some(4))]
        );
        assert_eq!(
            refs("https://github.com/o/r/blob/abc123/src/x.rs#L10-L15"),
            [("src/x.rs".into(), 10, Some(15))]
        );
    }

    #[test]
    fn bare_continuations_name_the_same_file() {
        assert_eq!(
            refs("File: `shop/orders.py:17-18`, `:32`, `:35`"),
            [
                ("shop/orders.py".into(), 17, Some(18)),
                ("shop/orders.py".into(), 32, None),
                ("shop/orders.py".into(), 35, None),
            ]
        );
        assert_eq!(
            refs("a.py:1 and :4-5; :9"),
            [
                ("a.py".into(), 1, None),
                ("a.py".into(), 4, Some(5)),
                ("a.py".into(), 9, None)
            ]
        );
        // A colon that starts something else is not a continuation.
        assert_eq!(refs("a.py:1, :next"), [("a.py".into(), 1, None)]);
    }

    #[test]
    fn words_between_the_path_and_the_line() {
        assert_eq!(
            refs("the `.gitignore` change (line 157) adds it"),
            [(".gitignore".into(), 157, None)]
        );
        assert_eq!(
            refs("`CLAUDE.md` still says the old thing (line 50)"),
            [("CLAUDE.md".into(), 50, None)]
        );
        assert_eq!(
            refs("`src/a.rs` and `src/b.rs` differ (lines 3-4)"),
            [],
            "two files in between: ambiguous"
        );
        assert_eq!(refs("it was fixed (line 12) already"), []);
    }

    #[test]
    fn not_references() {
        assert!(
            refs("at 10:30 on localhost:3000, ratio 1:4")
                .iter()
                .all(|(p, ..)| p != "10")
        );
        assert_eq!(refs("http://localhost:3000/x"), []);
        // The syntax pass lets `localhost:8080` through; the workspace drops it.
        let w = ws(&[("src/a.rs", 10)]);
        assert_eq!(detect("see localhost:8080 and Node 20.11", &w, &[]), []);
        assert_eq!(detect("bpm:120", &w, &[]), []);
    }

    #[test]
    fn code_blocks_are_skipped() {
        let w = ws(&[("src/a.rs", 10)]);
        let text = "Trace:\n```\nsrc/a.rs:3\n```\nbut src/a.rs:4 matters";
        let found = detect(text, &w, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, 4);
    }

    #[test]
    fn the_workspace_check() {
        let w = ws(&[
            ("src/cart/cart.ts", 50),
            ("src/api/auth.ts", 30),
            ("src/db/index.ts", 5),
            ("src/api/index.ts", 5),
        ]);
        let one = |t: &str, hints: &[String]| detect(t, &w, hints);
        // Exact, absolute and unique-suffix paths resolve.
        assert_eq!(one("src/cart/cart.ts:48", &[])[0].path, "src/cart/cart.ts");
        assert_eq!(
            one("/work/shop/src/api/auth.ts:30", &[])[0].path,
            "src/api/auth.ts"
        );
        assert_eq!(one("cart.ts:2", &[])[0].path, "src/cart/cart.ts");
        // Past the end, and no such file: dropped.
        assert_eq!(one("src/cart/cart.ts:51", &[]), []);
        assert_eq!(one("src/cart/cart.ts:40-60", &[]), []);
        assert_eq!(one("src/gone.ts:3", &[]), []);
        // Two files end with the name: the turn's files pick, or it is dropped.
        assert_eq!(one("index.ts:2", &[]), []);
        let hint = ["src/db/index.ts".to_owned()];
        assert_eq!(one("index.ts:2", &hint)[0].path, "src/db/index.ts");
        // The same place twice is one finding.
        assert_eq!(one("cart.ts:4 and src/cart/cart.ts:4", &[]).len(), 1);
    }

    #[test]
    fn an_unknown_length_is_left_to_the_daemon() {
        let w = Workspace {
            paths: vec!["a.rs".into()],
            ..Workspace::default()
        };
        let found = detect("a.rs:900", &w, &[]);
        assert_eq!(found.len(), 1);
        assert!(found[0].unchecked_lines);
    }

    #[test]
    fn severity_words_set_the_level() {
        let w = ws(&[("a.rs", 99)]);
        let level = |t: &str| detect(t, &w, &[])[0].level;
        assert_eq!(level("- a.rs:3 critical: leaks"), FindingLevel::Critical);
        assert_eq!(level("- a.rs:3 this is a bug"), FindingLevel::Critical);
        assert_eq!(level("- a.rs:3 error is swallowed"), FindingLevel::Critical);
        assert_eq!(level("- a.rs:3 you should check"), FindingLevel::Warning);
        assert_eq!(level("- a.rs:3 warning: slow"), FindingLevel::Warning);
        assert_eq!(level("- a.rs:3 nit: naming"), FindingLevel::Note);
        assert_eq!(level("- a.rs:3 a suggestion"), FindingLevel::Note);
        assert_eq!(level("- a.rs:3 uses a map"), FindingLevel::Note);
        // A word inside another word is not one.
        assert_eq!(level("- a.rs:3 debugging aid"), FindingLevel::Note);
        // The nearest heading sets it first.
        assert_eq!(
            level("### Critical (must fix)\n- a.rs:3 uses a map"),
            FindingLevel::Critical
        );
        assert_eq!(
            level("### Minor\n- a.rs:3 this is a bug"),
            FindingLevel::Note
        );
        assert_eq!(
            level("### Review\n- a.rs:3 should be a const"),
            FindingLevel::Warning
        );
    }

    #[test]
    fn the_text_is_the_line() {
        let w = ws(&[("a.rs", 99)]);
        let f = &detect(
            "Intro\n\n2. **Bug** at `a.rs:3` — drops the lock\nnext",
            &w,
            &[],
        )[0];
        assert_eq!(f.text, "**Bug** at `a.rs:3` — drops the lock");
        let f = &detect("- `a.rs:3` — x", &w, &[])[0];
        assert_eq!(f.text, "`a.rs:3` — x");
    }

    macro_rules! sample {
        ($($name:literal),*) => {
            [$(($name, include_str!(concat!("findings/samples/", $name)))),*]
        };
    }

    /// The seven review outputs of spike S9 (ADR 0030), five written in each
    /// skill's published format and two real runs: every finding detected,
    /// nothing else.
    #[test]
    fn the_spikes_samples() {
        let index: Value =
            serde_json::from_str(include_str!("findings/samples/index.json")).unwrap();
        let truths = index["truth"].as_object().unwrap();
        let samples = sample!(
            "gstack-review.md",
            "superpowers-code-reviewer.md",
            "pr-review-toolkit.md",
            "code-review-plugin.md",
            "feature-dev-review.md",
            "real-coqo-brainstorming.md",
            "real-coqo-requesting-code-review.md"
        );
        assert_eq!(samples.len(), truths.len());
        let mut total = 0;
        for (name, text) in samples {
            let truth = &truths[name];
            let files = index["workspaces"][truth["workspace"].as_str().unwrap()]
                .as_object()
                .unwrap();
            let w = Workspace {
                paths: files.keys().cloned().collect(),
                counts: files
                    .iter()
                    .map(|(p, n)| (p.clone(), n.as_u64().unwrap() as usize))
                    .collect(),
                roots: Vec::new(),
            };
            let truth: Vec<(String, u32)> = truth["truth"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    (
                        t[0].as_str().unwrap().to_owned(),
                        t[1].as_u64().unwrap() as u32,
                    )
                })
                .collect();
            let got: Vec<(String, u32)> = detect(text, &w, &[])
                .into_iter()
                .map(|f| (f.path, f.start))
                .collect();
            for want in &truth {
                assert!(got.contains(want), "{name}: missed {want:?}, got {got:?}");
            }
            for g in &got {
                assert!(truth.contains(g), "{name}: false positive {g:?}");
            }
            total += truth.len();
        }
        assert_eq!(total, 35, "all 34 of the spike's, and the bare `:32`");
    }
}
