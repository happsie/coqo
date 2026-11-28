// SPDX-License-Identifier: Apache-2.0

//! Finding the code a structured question is about, from what is shown
//! anyway: the question (text, header, options, previews), the turn's tool
//! calls and the user's selection. Nothing is asked of the skill.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use crate::refs::{self, Verdict};
use crate::workspace::Workspace;

/// The provider-neutral question #339 renders (AskUserQuestion, an ACP
/// form elicitation, a permission request with options, plan approval).
#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    #[serde(default)]
    pub header: String,
    pub text: String,
    #[serde(default)]
    pub options: Vec<QOption>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preview: Option<String>,
}

/// A tool call of the turn so far, as the chat already has it (ACP
/// `tool_call` with `locations` and `rawInput`; edits carry their hunks'
/// lines once complete).
#[derive(Debug, Clone, Deserialize)]
pub struct Call {
    pub tool: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub start: Option<u32>,
    #[serde(default)]
    pub end: Option<u32>,
}

impl Call {
    fn edits(&self) -> bool {
        matches!(
            self.tool.as_str(),
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit"
        )
    }
}

/// The user's selection (`editor/context`).
#[derive(Debug, Clone, Deserialize)]
pub struct Selection {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// A `path:line` in the question, its header or options.
    Explicit,
    /// An option's preview quotes lines that are in a file.
    Preview,
    /// Words of the question found in a file the turn read or edited.
    TurnFiles,
    /// The user's selection.
    Selection,
}

impl Source {
    pub const ALL: [Source; 4] = [
        Source::Explicit,
        Source::Preview,
        Source::TurnFiles,
        Source::Selection,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Source::Explicit => "explicit path:line",
            Source::Preview => "option preview",
            Source::TurnFiles => "files of the turn",
            Source::Selection => "selection",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub source: Source,
    /// More locations the question names ("these two places disagree").
    pub also: Vec<(String, u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub order: Vec<Source>,
    /// Use the selection only when the prompt points at it ("this") or the
    /// question's words are in it.
    pub gate_selection: bool,
    /// Lowest score for a line of a turn's file.
    pub min_score: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            order: Source::ALL.to_vec(),
            gate_selection: true,
            min_score: 3,
        }
    }
}

/// Everything the question shows, in reading order.
fn all_text(q: &Question) -> String {
    let mut s = format!("{}\n{}\n", q.header, q.text);
    for o in &q.options {
        s.push_str(&o.label);
        s.push('\n');
        s.push_str(&o.description);
        s.push('\n');
    }
    s
}

fn turn_files(calls: &[Call]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Edited files first: they are what the agent is working on.
    for c in calls.iter().rev().filter(|c| c.edits()) {
        if let Some(p) = &c.path {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    for c in calls.iter().rev() {
        if let Some(p) = &c.path {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

pub fn explicit(q: &Question, calls: &[Call], ws: &Workspace) -> Option<Anchor> {
    let hints = turn_files(calls);
    let mut text = all_text(q);
    for o in &q.options {
        if let Some(p) = &o.preview {
            text.push_str(p);
            text.push('\n');
        }
    }
    let mut locs: Vec<(String, u32, u32)> = Vec::new();
    for r in refs::find(&text) {
        if let Verdict::Location { path, start, end } = refs::check(&r, ws, &hints) {
            if !locs.iter().any(|(p, s, _)| *p == path && *s == start) {
                locs.push((path, start, end));
            }
        }
    }
    let mut it = locs.into_iter();
    let (path, start, end) = it.next()?;
    Some(Anchor {
        path,
        start,
        end,
        source: Source::Explicit,
        also: it.collect(),
    })
}

/// Lines of the previews that are in a file, at least two in a row.
pub fn preview(q: &Question, calls: &[Call], ws: &Workspace) -> Option<Anchor> {
    let mut files = turn_files(calls);
    for p in &ws.paths {
        if !files.contains(p) {
            files.push(p.clone());
        }
    }
    for o in &q.options {
        let Some(pv) = &o.preview else { continue };
        let want: Vec<&str> = pv.lines().map(str::trim).filter(|l| l.len() >= 8).collect();
        if want.len() < 2 {
            continue;
        }
        for f in &files {
            let Some(lines) = ws.lines(f) else { continue };
            for (i, _) in lines.iter().enumerate() {
                let run = want
                    .iter()
                    .zip(lines[i..].iter().filter(|l| l.trim().len() >= 8))
                    .take_while(|(w, l)| l.trim() == **w)
                    .count();
                if run >= 2 {
                    return Some(Anchor {
                        path: f.clone(),
                        start: i as u32 + 1,
                        end: i as u32 + run as u32,
                        source: Source::Preview,
                        also: vec![],
                    });
                }
            }
        }
    }
    None
}

static IDENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*(?:\(\))?").unwrap()
});
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z]+|[0-9]{2,}").unwrap());

const STOP: &[&str] = &[
    "about",
    "after",
    "again",
    "also",
    "and",
    "any",
    "are",
    "because",
    "before",
    "being",
    "both",
    "but",
    "can",
    "change",
    "code",
    "could",
    "does",
    "doing",
    "done",
    "each",
    "else",
    "even",
    "every",
    "file",
    "first",
    "fix",
    "for",
    "from",
    "function",
    "have",
    "here",
    "how",
    "into",
    "just",
    "keep",
    "like",
    "make",
    "more",
    "most",
    "much",
    "need",
    "new",
    "next",
    "not",
    "now",
    "only",
    "other",
    "our",
    "out",
    "over",
    "same",
    "should",
    "some",
    "still",
    "such",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "today",
    "too",
    "use",
    "want",
    "was",
    "way",
    "well",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    "yes",
    "may",
    "might",
    "must",
    "its",
    "it's",
    "is",
    "it",
    "be",
    "or",
    "to",
    "of",
    "in",
    "on",
    "at",
    "as",
    "by",
    "an",
    "do",
    "if",
    "so",
    "we",
    "my",
    "me",
    "no",
    "up",
    "one",
    "two",
    "let",
    "get",
    "recommended",
    "skip",
    "option",
    "options",
    "approach",
    "question",
    "answer",
    "recommendation",
    "stakes",
    "net",
];

fn stem(w: &str) -> String {
    let w = w.to_ascii_lowercase();
    if w.len() > 4 {
        w[..4].to_owned()
    } else {
        w.trim_end_matches('s').to_owned()
    }
}

/// Lower-case sub-words of an identifier (`MAX_ITEMS_PER_CART` -> max,
/// items, per, cart; `applyCoupon` -> apply, coupon).
fn sub_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            cur.push(ch.to_ascii_lowercase());
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_codeish(id: &str) -> bool {
    id.contains('_')
        || id.contains('.')
        || id.ends_with("()")
        || (id.chars().skip(1).any(|c| c.is_ascii_uppercase())
            && id.chars().any(|c| c.is_ascii_lowercase()))
}

struct Terms {
    idents: BTreeSet<String>,
    words: BTreeSet<String>,
}

fn terms(text: &str) -> Terms {
    let mut idents = BTreeSet::new();
    let mut words = BTreeSet::new();
    for m in IDENT.find_iter(text) {
        let id = m.as_str().trim_end_matches("()");
        if is_codeish(m.as_str()) && id.len() >= 3 {
            idents.insert(id.rsplit('.').next().unwrap_or(id).to_owned());
        }
    }
    for m in WORD.find_iter(text) {
        for w in sub_words(m.as_str()) {
            if (w.len() >= 3 || w.chars().all(|c| c.is_ascii_digit()))
                && !STOP.contains(&w.as_str())
            {
                words.insert(stem(&w));
            }
        }
    }
    Terms { idents, words }
}

/// An import names things without being about them.
fn is_import(line: &str) -> bool {
    let l = line.trim_start();
    [
        "import ",
        "use ",
        "from ",
        "#include",
        "require(",
        "export {",
        "export * ",
    ]
    .iter()
    .any(|p| l.starts_with(p))
}

/// The distinct terms of `t` on these lines: an identifier counts 3, a
/// word 1.
fn score_lines(lines: &[String], t: &Terms) -> u32 {
    let lines: Vec<&String> = lines.iter().filter(|l| !is_import(l)).collect();
    let mut score = 0;
    for id in &t.idents {
        if lines.iter().any(|l| l.contains(id.as_str())) {
            score += 3;
        }
    }
    let have: BTreeSet<String> = lines
        .iter()
        .flat_map(|l| sub_words(l))
        .filter(|w| w.len() >= 2)
        .map(|w| stem(&w))
        .collect();
    score += t.words.iter().filter(|w| have.contains(*w)).count() as u32;
    score
}

fn score_line(line: &str, t: &Terms) -> u32 {
    score_lines(std::slice::from_ref(&line.to_owned()), t)
}

const WINDOW: usize = 3;

/// The best-scoring lines of the turn's files, edited lines preferred.
pub fn turn(q: &Question, calls: &[Call], ws: &Workspace, opts: &Options) -> Option<Anchor> {
    let t = terms(&all_text(q));
    let files = turn_files(calls);
    let mut best: Option<(u32, Anchor)> = None;
    for (rank, f) in files.iter().enumerate() {
        let Some(lines) = ws.lines(f) else { continue };
        let hunks: Vec<(u32, u32)> = calls
            .iter()
            .filter(|c| c.edits() && c.path.as_deref() == Some(f))
            .filter_map(|c| Some((c.start?, c.end.or(c.start)?)))
            .collect();
        for i in 0..lines.len() {
            let n = i as u32 + 1;
            // The line and the two after it, as one place.
            let window = &lines[i..(i + WINDOW).min(lines.len())];
            if score_line(&lines[i], &t) == 0 {
                continue;
            }
            let mut s = score_lines(window, &t) * 4;
            let last = window
                .iter()
                .rposition(|l| score_line(l, &t) > 0)
                .map_or(n, |k| n + k as u32);
            let hunk = hunks.iter().find(|(a, b)| *a <= n && n <= *b).copied();
            if hunk.is_some() {
                // A line the agent just edited counts as one more term.
                s += 4;
            }
            // Earlier files in the list (edited, then most recent) win ties.
            s = s.saturating_sub(rank.min(3) as u32);
            if s < opts.min_score * 4 {
                continue;
            }
            if best.as_ref().is_none_or(|(b, _)| s > *b) {
                let (start, end) = match hunk {
                    Some((a, b)) if b - a <= 12 => (a, b),
                    _ => (n, last),
                };
                best = Some((
                    s,
                    Anchor {
                        path: f.clone(),
                        start,
                        end,
                        source: Source::TurnFiles,
                        also: vec![],
                    },
                ));
            }
        }
    }
    best.map(|(_, a)| a)
}

static DEICTIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(this|these|here|selected|selection|highlighted|lines? above)\b").unwrap()
});

pub fn selection(
    q: &Question,
    prompt: &str,
    sel: Option<&Selection>,
    ws: &Workspace,
    opts: &Options,
) -> Option<Anchor> {
    let sel = sel?;
    let anchor = Anchor {
        path: sel.path.clone(),
        start: sel.start,
        end: sel.end,
        source: Source::Selection,
        also: vec![],
    };
    if !opts.gate_selection {
        return Some(anchor);
    }
    let t = terms(&all_text(q));
    let lines = ws.lines(&sel.path)?;
    let from = (sel.start.saturating_sub(1) as usize).min(lines.len());
    let to = (sel.end as usize).min(lines.len());
    let s = score_lines(&lines[from..to], &t);
    // A prompt that points at the selection ("this") needs one shared
    // term; otherwise the selection must be about what is asked.
    let need = if DEICTIC.is_match(prompt) {
        1
    } else {
        opts.min_score
    };
    (s >= need).then_some(anchor)
}

pub fn by_source(
    src: Source,
    q: &Question,
    prompt: &str,
    calls: &[Call],
    sel: Option<&Selection>,
    ws: &Workspace,
    opts: &Options,
) -> Option<Anchor> {
    match src {
        Source::Explicit => explicit(q, calls, ws),
        Source::Preview => preview(q, calls, ws),
        Source::TurnFiles => turn(q, calls, ws, opts),
        Source::Selection => selection(q, prompt, sel, ws, opts),
    }
}

/// The first source in `opts.order` that finds an anchor; `None` keeps the
/// question in the chat (with "Pin to code").
pub fn resolve(
    q: &Question,
    prompt: &str,
    calls: &[Call],
    sel: Option<&Selection>,
    ws: &Workspace,
    opts: &Options,
) -> Option<Anchor> {
    opts.order
        .iter()
        .find_map(|s| by_source(*s, q, prompt, calls, sel, ws, opts))
}
