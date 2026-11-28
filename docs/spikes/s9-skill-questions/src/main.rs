// SPDX-License-Identifier: Apache-2.0

//! The measurements of ADR 0029, as Markdown on stdout.
//!
//! `cargo run --release` runs on the samples; `-- --corpus <reviews.json>`
//! adds the real review comments that `fetch-corpus.py` collects (kept out
//! of the repository: they belong to their authors).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use spike_s9_skill_questions::anchor::{self, Anchor, Call, Options, Question, Selection, Source};
use spike_s9_skill_questions::findings;
use spike_s9_skill_questions::open_question::{self, Ending};
use spike_s9_skill_questions::refs::{self, Verdict};
use spike_s9_skill_questions::workspace::Workspace;

#[derive(Deserialize)]
struct Case {
    id: String,
    skill: String,
    tool: String,
    prompt: String,
    question: Question,
    turn: Vec<Call>,
    selection: Option<Selection>,
    truth: Vec<Truth>,
}

#[derive(Deserialize)]
struct Truth {
    #[serde(default)]
    none: bool,
    #[serde(default)]
    path: String,
    #[serde(default)]
    start: u32,
    #[serde(default)]
    end: u32,
}

impl Case {
    /// The code is the right place, and the chat alone is not.
    fn anchorable(&self) -> bool {
        !self.truth.iter().any(|t| t.none)
    }
    fn none_ok(&self) -> bool {
        self.truth.iter().any(|t| t.none)
    }
    fn hits(&self, a: &Anchor) -> bool {
        self.truth
            .iter()
            .any(|t| !t.none && t.path == a.path && a.start <= t.end + 1 && t.start <= a.end + 1)
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "–".into()
    } else {
        format!("{:.0} % ({n}/{d})", 100.0 * n as f64 / d as f64)
    }
}

/// Plans are a chat card with links: never pinned to one place.
fn run(c: &Case, ws: &Workspace, o: &Options) -> Option<Anchor> {
    if c.tool == "ExitPlanMode" {
        return None;
    }
    anchor::resolve(&c.question, &c.prompt, &c.turn, c.selection.as_ref(), ws, o)
}

fn questions(ws: &Workspace) {
    let cases: Vec<Case> = serde_json::from_str(
        &std::fs::read_to_string(root().join("samples/questions.json")).unwrap(),
    )
    .unwrap();
    let anchorable = cases.iter().filter(|c| c.anchorable()).count();
    let chat_only = cases
        .iter()
        .filter(|c| c.truth.iter().all(|t| t.none))
        .count();
    println!("## 1. Structured questions: anchor sources\n");
    println!(
        "{} questions: {} belong in the code, {} in the chat only, {} either.\n",
        cases.len(),
        anchorable,
        chat_only,
        cases.len() - anchorable - chat_only
    );
    println!("Each source on its own (plans excluded, as in the cascade):\n");
    println!(
        "| Source | Fired | Right place (precision) | Recall over the {anchorable} that belong in the code |"
    );
    println!("|---|---|---|---|");
    let opts = Options::default();
    for s in Source::ALL {
        let mut fired = 0;
        let mut right = 0;
        let mut right_anchorable = 0;
        for c in &cases {
            if c.tool == "ExitPlanMode" {
                continue;
            }
            if let Some(a) = anchor::by_source(
                s,
                &c.question,
                &c.prompt,
                &c.turn,
                c.selection.as_ref(),
                ws,
                &opts,
            ) {
                fired += 1;
                if c.hits(&a) {
                    right += 1;
                    if c.anchorable() {
                        right_anchorable += 1;
                    }
                }
            }
        }
        println!(
            "| {} | {fired} | {} | {} |",
            s.name(),
            pct(right, fired),
            pct(right_anchorable, anchorable)
        );
    }
    // The selection without its gate, for comparison.
    let ungated = Options {
        gate_selection: false,
        ..Options::default()
    };
    let mut fired = 0;
    let mut right = 0;
    for c in cases.iter().filter(|c| c.tool != "ExitPlanMode") {
        if let Some(a) =
            anchor::selection(&c.question, &c.prompt, c.selection.as_ref(), ws, &ungated)
        {
            fired += 1;
            right += usize::from(c.hits(&a));
        }
    }
    println!(
        "| selection, ungated (any selection) | {fired} | {} | |",
        pct(right, fired)
    );

    println!(
        "\nFiles of the turn by the lowest score it accepts (terms: an identifier 3, a word 1, a line the turn edited 1):\n"
    );
    println!("| Lowest score | Fired | Precision | Recall |");
    println!("|---|---|---|---|");
    for min in 2..=7 {
        let o = Options {
            min_score: min,
            ..Options::default()
        };
        let (mut fired, mut right, mut right_a) = (0, 0, 0);
        for c in cases.iter().filter(|c| c.tool != "ExitPlanMode") {
            if let Some(a) = anchor::turn(&c.question, &c.turn, ws, &o) {
                fired += 1;
                if c.hits(&a) {
                    right += 1;
                    right_a += usize::from(c.anchorable());
                }
            }
        }
        println!(
            "| {min} | {fired} | {} | {} |",
            pct(right, fired),
            pct(right_a, anchorable)
        );
    }

    println!(
        "\nThe cascade (first source that finds a place; else the chat with \"Pin to code\"):\n"
    );
    println!(
        "| Order | Right | Pinned to the wrong place | Left in the chat that belonged in the code |"
    );
    println!("|---|---|---|---|");
    let variants: Vec<(&str, Options)> = vec![
        (
            "explicit → preview → turn files → selection (gated)",
            Options::default(),
        ),
        (
            "explicit → preview → selection (gated) → turn files",
            Options {
                order: vec![
                    Source::Explicit,
                    Source::Preview,
                    Source::Selection,
                    Source::TurnFiles,
                ],
                ..Options::default()
            },
        ),
        (
            "explicit → turn files → selection, ungated",
            Options {
                order: vec![Source::Explicit, Source::TurnFiles, Source::Selection],
                gate_selection: false,
                ..Options::default()
            },
        ),
        (
            "explicit and selection (gated) only",
            Options {
                order: vec![Source::Explicit, Source::Preview, Source::Selection],
                ..Options::default()
            },
        ),
        (
            "explicit only",
            Options {
                order: vec![Source::Explicit],
                ..Options::default()
            },
        ),
    ];
    let mut detail = Vec::new();
    for (i, (name, o)) in variants.iter().enumerate() {
        let (mut ok, mut wrong, mut missed) = (0, 0, 0);
        for c in &cases {
            let a = run(c, ws, o);
            let good = match &a {
                Some(a) => c.hits(a),
                None => c.none_ok(),
            };
            if good {
                ok += 1;
            } else if a.is_some() {
                wrong += 1;
            } else {
                missed += 1;
            }
            if i == 0 {
                detail.push(format!(
                    "| {} | {} | {} | {} |",
                    c.id,
                    c.skill,
                    a.as_ref().map_or("chat".into(), |a| format!(
                        "{}:{}-{} ({})",
                        a.path,
                        a.start,
                        a.end,
                        a.source.name()
                    )),
                    if good { "ok" } else { "**wrong**" }
                ));
            }
        }
        println!(
            "| {name} | {} | {} | {} |",
            pct(ok, cases.len()),
            pct(wrong, cases.len()),
            pct(missed, cases.len())
        );
    }
    println!("\nPer question (first order):\n");
    println!("| Case | Skill | Shown at | |");
    println!("|---|---|---|---|");
    for d in detail {
        println!("{d}");
    }
}

#[derive(Deserialize)]
struct FindingSample {
    workspace: String,
    truth: Vec<(String, u32)>,
}

fn findings_samples() {
    let samples: BTreeMap<String, FindingSample> = serde_json::from_str(
        &std::fs::read_to_string(root().join("samples/findings.json")).unwrap(),
    )
    .unwrap();
    println!("\n## 2. Findings in text: review outputs\n");
    println!(
        "`real-coqo-*` are real outputs of the skills in a temporary Coqo session; the others are written in each skill's published format.\n"
    );
    println!("| Output | Findings | Detected right | Wrong (false positive) | Missed |");
    println!("|---|---|---|---|---|");
    let (mut tt, mut tr, mut tw, mut tm) = (0, 0, 0, 0);
    let mut syntax = 0;
    let mut syntax_wrong = 0;
    for (file, sample) in &samples {
        let ws = Workspace::from_dir(&root().join("samples").join(&sample.workspace));
        let want = &sample.truth;
        let text = std::fs::read_to_string(root().join("samples/findings").join(file)).unwrap();
        let got = findings::findings(&text, &ws, &[]);
        let right = got
            .iter()
            .filter(|f| want.iter().any(|(p, l)| *p == f.path && *l == f.start))
            .count();
        let wrong: Vec<String> = got
            .iter()
            .filter(|f| !want.iter().any(|(p, l)| *p == f.path && *l == f.start))
            .map(|f| format!("{}:{}", f.path, f.start))
            .collect();
        let missed: Vec<String> = want
            .iter()
            .filter(|(p, l)| !got.iter().any(|f| f.path == *p && f.start == *l))
            .map(|(p, l)| format!("{p}:{l}"))
            .collect();
        println!(
            "| {file} | {} | {right} | {} {} | {} {} |",
            want.len(),
            wrong.len(),
            wrong.join(", "),
            missed.len(),
            missed.join(", ")
        );
        tt += want.len();
        tr += right;
        tw += wrong.len();
        tm += missed.len();
        // What a syntax-only detector (no workspace check) would have shown.
        for r in refs::find(&text)
            .iter()
            .filter(|r| refs::plausible(r) && !r.in_code)
        {
            syntax += 1;
            let ok = match refs::check(r, &ws, &[]) {
                Verdict::Location { path, start, .. } => {
                    want.iter().any(|(p, l)| *p == path && *l == start)
                }
                _ => false,
            };
            syntax_wrong += usize::from(!ok);
        }
    }
    println!("| **all** | {tt} | {tr} | {tw} | {tm} |");
    println!(
        "\nWithout the workspace check (syntax only: a path with an extension or a directory, outside code blocks): {syntax} shown, {} wrong (duplicates of one location count again).",
        pct(syntax_wrong, syntax)
    );
}

#[derive(Deserialize)]
struct Pr {
    url: String,
    files: Vec<String>,
    line_counts: HashMap<String, usize>,
    comments: Vec<String>,
}

fn corpus(path: &Path) {
    let prs: Vec<Pr> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let comments: usize = prs.iter().map(|p| p.comments.len()).sum();
    println!("\n## 2b. Findings in text: real Claude review comments\n");
    println!(
        "{} public pull requests, {} review comments by Claude.\n",
        prs.len(),
        comments
    );
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut syntax = 0;
    let mut syntax_bad = 0;
    let mut shown = 0;
    let mut unknown_lines = 0;
    let mut examples_bad: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut misses: Vec<String> = Vec::new();
    let mut endings: BTreeMap<&str, usize> = BTreeMap::new();
    let mut ending_examples: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for pr in &prs {
        let ws = Workspace::from_list(pr.files.clone(), pr.line_counts.clone());
        let bases: Vec<&str> = pr
            .files
            .iter()
            .map(|f| f.rsplit('/').next().unwrap())
            .filter(|b| b.contains('.'))
            .collect();
        for text in &pr.comments {
            let e = open_question::classify(text, &[], &ws);
            *endings.entry(e.label()).or_default() += 1;
            if matches!(e, Ending::Code { .. } | Ending::Other | Ending::Offer) {
                let tail: String = text
                    .trim_end()
                    .chars()
                    .rev()
                    .take(160)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                ending_examples
                    .entry(e.label())
                    .or_default()
                    .push(tail.replace('\n', " "));
            }
            let found = refs::find(text);
            let mut covered_lines: Vec<usize> = Vec::new();
            for r in &found {
                let v = refs::check(r, &ws, &[]);
                let key = match (&v, r.in_code) {
                    (_, true) => "in a code block",
                    (Verdict::Location { .. }, _) => "real line",
                    (Verdict::LinesUnknown { .. }, _) => "real file, length unknown",
                    (Verdict::PastEnd { .. }, _) => "past the end of the file",
                    (Verdict::Ambiguous(_), _) => "ambiguous file name",
                    (Verdict::NoSuchFile, _) => "no such file",
                };
                *counts.entry(key).or_default() += 1;
                if r.in_code {
                    continue;
                }
                let ln = text[..r.span.0].matches('\n').count();
                covered_lines.push(ln);
                if refs::plausible(r) {
                    syntax += 1;
                    if !matches!(v, Verdict::Location { .. } | Verdict::LinesUnknown { .. }) {
                        syntax_bad += 1;
                    }
                }
                match v {
                    Verdict::Location { .. } => shown += 1,
                    Verdict::LinesUnknown { .. } => unknown_lines += 1,
                    _ => {
                        let ex = examples_bad.entry(key).or_default();
                        if ex.len() < 10 {
                            let plain = if refs::plausible(r) {
                                ""
                            } else {
                                ", not plausible by syntax"
                            };
                            ex.push(format!("`{}:{}` in {}{plain}", r.path, r.line, pr.url));
                        }
                    }
                }
            }
            // Possible misses: a line naming a file of the repo next to a
            // number, with no reference found on it.
            for (i, line) in text.lines().enumerate() {
                if covered_lines.contains(&i) || line.trim_start().starts_with("```") {
                    continue;
                }
                let names = bases.iter().any(|b| line.contains(b));
                let lineish = line.contains("line ")
                    || line.contains("Line ")
                    || line.contains(":L")
                    || line.contains("#L");
                if names && lineish && misses.len() < 15 {
                    misses.push(format!(
                        "{} — {}",
                        pr.url,
                        line.trim().chars().take(140).collect::<String>()
                    ));
                }
            }
        }
    }
    println!("| What the detector found | Count |");
    println!("|---|---|");
    for (k, v) in &counts {
        println!("| {k} | {v} |");
    }
    println!(
        "\nSyntax only (no workspace check), outside code blocks: {syntax} references, {} not a real file or line.",
        pct(syntax_bad, syntax)
    );
    println!(
        "With the workspace check: {shown} shown at a checked line, {unknown_lines} at a real file whose length the corpus lacks."
    );
    for (k, ex) in &examples_bad {
        println!("\nRejected, {k}:\n");
        for e in ex {
            println!("- {e}");
        }
    }
    println!("\nLines that name a file and a line but gave no reference (possible misses):\n");
    for m in &misses {
        println!("- {m}");
    }
    println!("\n## 3. Plain-text endings of the real comments\n");
    println!("| Ending | Comments |");
    println!("|---|---|");
    for (k, v) in &endings {
        println!("| {k} | {v} |");
    }
    for (k, ex) in &ending_examples {
        println!("\n{k}:\n");
        for e in ex.iter().take(8) {
            println!("- …{e}");
        }
    }
}

#[derive(Deserialize)]
struct EndingCase {
    skill: String,
    label: String,
    #[serde(default)]
    text: String,
    /// A message under `samples/findings/` instead of `text`.
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    turn: Vec<Call>,
}

fn endings(ws: &Workspace) {
    let cases: Vec<EndingCase> = serde_json::from_str(
        &std::fs::read_to_string(root().join("samples/endings.json")).unwrap(),
    )
    .unwrap();
    println!("\n## 3. Plain-text questions at the end of a turn (samples)\n");
    println!("| Ending | Skill | Expected | Got | |");
    println!("|---|---|---|---|---|");
    let mut ok = 0;
    let mut code_right = 0;
    let mut code_got = 0;
    let mut code_want = 0;
    for c in &cases {
        let own;
        let ws = match &c.workspace {
            Some(w) => {
                own = Workspace::from_dir(&root().join("samples").join(w));
                &own
            }
            None => ws,
        };
        let text = match &c.file {
            Some(f) => std::fs::read_to_string(root().join("samples/findings").join(f)).unwrap(),
            None => c.text.clone(),
        };
        let e = open_question::classify(&text, &c.turn, ws);
        let good = e.label() == c.label;
        ok += usize::from(good);
        if e.label() == "code" {
            code_got += 1;
            code_right += usize::from(good);
        }
        code_want += usize::from(c.label == "code");
        let got = match &e {
            Ending::Code { path, start, .. } => format!("code at {path}:{start}"),
            other => other.label().to_owned(),
        };
        let short: String = match &c.file {
            Some(f) => format!("({f})"),
            None => c.text.chars().take(70).collect(),
        };
        println!(
            "| {}… | {} | {} | {got} | {} |",
            short.replace('\n', " "),
            c.skill,
            c.label,
            if good { "ok" } else { "**wrong**" }
        );
    }
    println!(
        "\nRight: {}. \"Names code\": precision {}, recall {}.",
        pct(ok, cases.len()),
        pct(code_right, code_got),
        pct(code_right, code_want)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ws = Workspace::from_dir(&root().join("samples/workspace"));
    questions(&ws);
    findings_samples();
    endings(&ws);
    if let Some(i) = args.iter().position(|a| a == "--corpus") {
        corpus(Path::new(&args[i + 1]));
    }
}
