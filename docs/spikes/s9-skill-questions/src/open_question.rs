// SPDX-License-Identifier: Apache-2.0

//! A turn that ends with a question in plain text ("Should I use X or Y?").

use std::sync::LazyLock;

use regex::Regex;

use crate::anchor::{self, Call, Options, Question};
use crate::refs::{self, Verdict};
use crate::workspace::Workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// No question at the end.
    NoQuestion,
    /// An offer to go on ("Want me to fix these?"): about the work, not a
    /// place in the code.
    Offer,
    /// A question that names code: where it would be pinned.
    Code { path: String, start: u32, end: u32 },
    /// A question about something else (the product, a preference).
    Other,
}

impl Ending {
    pub fn label(&self) -> &'static str {
        match self {
            Ending::NoQuestion => "none",
            Ending::Offer => "offer",
            Ending::Code { .. } => "code",
            Ending::Other => "other",
        }
    }
}

static OFFER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(would you like|do you want me to|want me to|shall i|should i (go ahead|proceed|continue|start|implement|fix|apply|commit|push|open|create|make|update|add|address)|let me know|anything else|ready to (proceed|merge)|any (other )?questions|(can|could) i help)",
    )
    .unwrap()
});

/// The last question, and the paragraphs it ends.
fn last_question(message: &str) -> Option<(String, String)> {
    let mut paras: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut cur = String::new();
    for line in message.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if line.trim().is_empty() {
            if !cur.trim().is_empty() {
                paras.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    if !cur.trim().is_empty() {
        paras.push(cur);
    }
    // A sign-off line ("---", a footer link) does not count as the end.
    while paras.last().is_some_and(|p| {
        let t = p.trim();
        t.starts_with("---") || t.starts_with('[') || t.starts_with("<sub>") || t.starts_with('_')
    }) {
        paras.pop();
    }
    let tail = paras
        .iter()
        .rev()
        .take(2)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if !tail.trim_end().ends_with('?')
        && !tail
            .lines()
            .last()
            .is_some_and(|l| l.trim_end().ends_with('?'))
    {
        return None;
    }
    let tail = tail.trim_end();
    let cut = tail[..tail.len() - 1]
        .rfind(['.', '!', '?', '\n'])
        .map_or(0, |i| i + 1);
    Some((tail[cut..].trim().to_owned(), tail.to_owned()))
}

pub fn classify(message: &str, calls: &[Call], ws: &Workspace) -> Ending {
    let Some((q, tail)) = last_question(message) else {
        return Ending::NoQuestion;
    };
    if OFFER.is_match(&q) {
        return Ending::Offer;
    }
    let hints: Vec<String> = calls.iter().filter_map(|c| c.path.clone()).collect();
    for r in refs::find(&tail) {
        if let Verdict::Location { path, start, end } = refs::check(&r, ws, &hints) {
            return Ending::Code { path, start, end };
        }
    }
    let question = Question {
        header: String::new(),
        text: tail,
        options: vec![],
    };
    match anchor::turn(&question, calls, ws, &Options::default()) {
        Some(a) => Ending::Code {
            path: a.path,
            start: a.start,
            end: a.end,
        },
        None => Ending::Other,
    }
}
