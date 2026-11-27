// SPDX-License-Identifier: Apache-2.0

//! Golden tests: the symbols, outline and folding ranges of a small fixture
//! per language, checked against `tests/golden/<fixture>.txt`.
//!
//! Regenerate with `COQO_BLESS=1 cargo test -p coqo-syntax --test golden`,
//! then review the diff.

use std::fmt::Write as _;
use std::path::Path;

use coqo_syntax::{Document, Language, OutlineItem};

/// Every fixture and the language it must be detected as.
const FIXTURES: &[(&str, &str)] = &[
    ("sample.rs", "rust"),
    ("sample.ts", "typescript"),
    ("sample.tsx", "tsx"),
    ("sample.js", "javascript"),
    ("Sample.java", "java"),
    ("Sample.kt", "kotlin"),
    ("sample.json", "json"),
    ("sample.toml", "toml"),
    ("sample.md", "markdown"),
    ("sample.yaml", "yaml"),
    ("sample.html", "html"),
    ("sample.css", "css"),
    ("pom.xml", "xml"),
];

fn render_outline(items: &[OutlineItem], depth: usize, out: &mut String) {
    for item in items {
        let s = &item.symbol;
        let _ = writeln!(
            out,
            "{}{} {} [{}-{}]",
            "  ".repeat(depth),
            s.kind,
            s.name,
            s.start_line + 1,
            s.end_line + 1
        );
        render_outline(&item.children, depth + 1, out);
    }
}

fn render(doc: &Document) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# language: {}", doc.language().id());
    out.push_str("\n# outline (kind name [lines])\n");
    render_outline(&doc.outline(), 0, &mut out);
    out.push_str("\n# symbols (kind qualified name)\n");
    for s in doc.symbols() {
        let _ = writeln!(out, "{} {}", s.kind, s.qualified_name("."));
        // The ranges must point at the name.
        assert_eq!(
            doc.text()[s.name_range.clone()]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            s.name
        );
        assert!(s.range.start <= s.name_range.start && s.name_range.end <= s.range.end);
    }
    out.push_str("\n# folds (lines kind)\n");
    for f in doc.folding_ranges() {
        let _ = writeln!(
            out,
            "{}-{} {}",
            f.start_line + 1,
            f.end_line + 1,
            f.kind.as_str()
        );
    }
    out
}

#[test]
fn fixtures_match_their_golden_files() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let bless = std::env::var_os("COQO_BLESS").is_some();
    let mut failures = Vec::new();
    for &(fixture, language) in FIXTURES {
        let text = std::fs::read_to_string(dir.join("fixtures").join(fixture)).unwrap();
        let lang =
            Language::detect(fixture, &text).unwrap_or_else(|| panic!("no language for {fixture}"));
        assert_eq!(lang.id(), language, "{fixture}");
        let doc = Document::new(lang, text);
        assert!(
            !doc.tree().root_node().has_error(),
            "{fixture} has syntax errors: {}",
            doc.tree().root_node().to_sexp()
        );
        let actual = render(&doc);
        let golden = dir.join("golden").join(format!("{fixture}.txt"));
        if bless {
            std::fs::write(&golden, &actual).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&golden).unwrap_or_default();
        if expected != actual {
            failures.push(format!(
                "{fixture}: output differs from {}\n--- actual\n{actual}",
                golden.display()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}\nRegenerate with COQO_BLESS=1 cargo test -p coqo-syntax --test golden",
        failures.join("\n")
    );
}

#[test]
fn every_language_has_a_fixture() {
    for lang in coqo_syntax::languages() {
        assert!(
            FIXTURES.iter().any(|&(_, id)| id == lang.id()),
            "{} has no golden fixture",
            lang.id()
        );
    }
}
