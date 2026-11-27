// SPDX-License-Identifier: Apache-2.0

//! Language detection, incremental re-parsing and performance.

use std::time::{Duration, Instant};

use coqo_syntax::{Document, FoldKind, Language, SymbolKind, TextEdit};

fn lang(id: &str) -> &'static Language {
    Language::by_id(id).unwrap()
}

fn id(lang: Option<&'static Language>) -> Option<&'static str> {
    lang.map(Language::id)
}

#[test]
fn languages_by_path() {
    assert_eq!(id(Language::for_path("src/main.rs")), Some("rust"));
    assert_eq!(id(Language::for_path("C:\\src\\App.TSX")), Some("tsx"));
    assert_eq!(id(Language::for_path("index.mjs")), Some("javascript"));
    assert_eq!(id(Language::for_path("view.jsx")), Some("javascript"));
    assert_eq!(id(Language::for_path("build.gradle.kts")), Some("kotlin"));
    assert_eq!(id(Language::for_path("Main.java")), Some("java"));
    assert_eq!(id(Language::for_path("tsconfig.json")), Some("json"));
    assert_eq!(id(Language::for_path(".eslintrc")), Some("json"));
    assert_eq!(id(Language::for_path("Cargo.lock")), Some("toml"));
    assert_eq!(id(Language::for_path("README.md")), Some("markdown"));
    assert_eq!(id(Language::for_path(".github/ci.yml")), Some("yaml"));
    assert_eq!(id(Language::for_path("index.html")), Some("html"));
    assert_eq!(id(Language::for_path("site.css")), Some("css"));
    assert_eq!(id(Language::for_path("pom.xml")), Some("xml"));
    assert_eq!(id(Language::for_path("icon.svg")), Some("xml"));
    // No extension, a dotfile, or an unknown extension.
    assert_eq!(id(Language::for_path("Makefile")), None);
    assert_eq!(id(Language::for_path(".json")), None);
    assert_eq!(id(Language::for_path("notes.txt")), None);
}

#[test]
fn languages_by_shebang() {
    assert_eq!(
        id(Language::for_shebang("#!/usr/bin/env node")),
        Some("javascript")
    );
    assert_eq!(
        id(Language::for_shebang("#!/usr/local/bin/node18")),
        Some("javascript")
    );
    assert_eq!(
        id(Language::for_shebang(
            "#!/usr/bin/env -S deno run --allow-read"
        )),
        Some("typescript")
    );
    assert_eq!(
        id(Language::for_shebang(
            "#!/usr/bin/env -S cargo +nightly -Zscript"
        )),
        Some("rust")
    );
    assert_eq!(
        id(Language::for_shebang("#!/usr/bin/env kotlin")),
        Some("kotlin")
    );
    assert_eq!(id(Language::for_shebang("#!/bin/sh")), None);
    assert_eq!(id(Language::for_shebang("// not a shebang")), None);
    // The path wins; the shebang is the fallback.
    assert_eq!(
        id(Language::detect(
            "bin/tool",
            "#!/usr/bin/env node\nconsole.log(1)"
        )),
        Some("javascript")
    );
    assert_eq!(
        id(Language::detect("x.rs", "#!/usr/bin/env node")),
        Some("rust")
    );
}

#[test]
fn ids_are_unique_and_known_to_the_registry() {
    let all = coqo_syntax::languages();
    for (i, l) in all.iter().enumerate() {
        assert_eq!(Language::by_id(l.id()), Some(l));
        assert!(
            all[..i].iter().all(|o| o.id() != l.id()),
            "{} twice",
            l.id()
        );
        for ext in l.extensions() {
            let owner = Language::for_path(&format!("file.{ext}")).unwrap();
            assert_eq!(owner, l, ".{ext} belongs to two languages");
        }
    }
}

/// `doc` gives the same tree, symbols and folds as a fresh parse of its text.
fn assert_matches_fresh_parse(doc: &Document) {
    let fresh = Document::new(doc.language(), doc.text());
    assert_eq!(
        doc.tree().root_node().to_sexp(),
        fresh.tree().root_node().to_sexp()
    );
    assert_eq!(doc.symbols(), fresh.symbols());
    assert_eq!(doc.folding_ranges(), fresh.folding_ranges());
}

const RUST: &str = "\
mod shapes {
    pub struct Circle {
        radius: f64,
    }

    impl Circle {
        fn area(&self) -> f64 {
            3.14 * self.radius * self.radius
        }
    }
}
";

fn names(doc: &Document) -> Vec<String> {
    doc.symbols()
        .iter()
        .map(|s| s.qualified_name("::"))
        .collect()
}

#[test]
fn edits_reparse_incrementally() {
    let mut doc = Document::new(lang("rust"), RUST);
    assert_eq!(
        names(&doc),
        [
            "shapes",
            "shapes::Circle",
            "shapes::Circle::radius",
            "shapes::Circle",
            "shapes::Circle::area"
        ]
    );

    // Rename a method.
    let at = doc.text().find("area").unwrap();
    doc.edit(&[TextEdit::new(at..at + 4, "surface")]).unwrap();
    assert_eq!(names(&doc)[4], "shapes::Circle::surface");
    assert_eq!(doc.incremental_parses(), 1);
    assert_matches_fresh_parse(&doc);

    // Add a method after it, typed one character at a time.
    let mut at = doc.text().rfind("    }\n}").unwrap();
    for ch in "\n    fn zero() -> Self { todo!() }".chars() {
        doc.edit(&[TextEdit::new(at..at, ch)]).unwrap();
        at += ch.len_utf8();
    }
    assert_matches_fresh_parse(&doc);
    assert!(names(&doc).contains(&"shapes::Circle::zero".to_owned()));

    // Two edits in one change, both in the coordinates before it: rename
    // the module and delete the struct's field.
    let module = doc.text().find("shapes").unwrap();
    let field = doc.text().find("        radius: f64,\n").unwrap();
    let field_end = field + "        radius: f64,\n".len();
    doc.edit(&[
        TextEdit::new(module..module + "shapes".len(), "geometry"),
        TextEdit::new(field..field_end, ""),
    ])
    .unwrap();
    assert_matches_fresh_parse(&doc);
    let names = names(&doc);
    assert_eq!(names[0], "geometry");
    assert!(!names.iter().any(|n| n.ends_with("radius")), "{names:?}");
}

#[test]
fn edits_with_multibyte_text() {
    let mut doc = Document::new(lang("rust"), "fn größe() {}\nfn b() {}\n");
    // Insert before a multi-byte name, then replace inside it.
    doc.edit(&[TextEdit::new(0..0, "/// ä\n")]).unwrap();
    let start = doc.text().find("größe").unwrap();
    doc.edit(&[TextEdit::new(start..start + "größe".len(), "maß")])
        .unwrap();
    assert_matches_fresh_parse(&doc);
    let symbols = doc.symbols();
    assert_eq!(symbols[0].name, "maß");
    assert_eq!(symbols[0].start_line, 1);
    assert_eq!(&doc.text()[symbols[0].name_range.clone()], "maß");
}

#[test]
fn invalid_edits_change_nothing() {
    let mut doc = Document::new(lang("rust"), "fn é() {}\n");
    let before = doc.text().to_owned();
    // Out of range, inside a character, overlapping, unsorted.
    assert!(doc.edit(&[TextEdit::new(5..100, "")]).is_err());
    assert!(doc.edit(&[TextEdit::new(4..4, "x")]).is_err());
    assert!(
        doc.edit(&[TextEdit::new(0..2, ""), TextEdit::new(1..3, "")])
            .is_err()
    );
    assert!(
        doc.edit(&[TextEdit::new(5..6, ""), TextEdit::new(0..1, "")])
            .is_err()
    );
    assert_eq!(doc.text(), before);
    assert_eq!(doc.incremental_parses(), 0);
    doc.edit(&[]).unwrap();
    assert_eq!(doc.text(), before);
}

#[test]
fn symbols_at_an_offset_are_outermost_first() {
    let doc = Document::new(lang("rust"), RUST);
    let at = RUST.find("3.14").unwrap();
    let path: Vec<_> = doc
        .symbols_at(at)
        .iter()
        .map(|s| (s.kind, s.name.clone()))
        .collect();
    assert_eq!(
        path,
        [
            (SymbolKind::Module, "shapes".to_owned()),
            (SymbolKind::Impl, "Circle".to_owned()),
            (SymbolKind::Method, "area".to_owned()),
        ]
    );
    assert!(doc.symbols_at(RUST.len()).is_empty());
}

#[test]
fn line_comments_and_imports_fold_as_runs() {
    let text = "use a;\nuse b;\nuse c;\n\n// one\n// two\n// three\nfn f() {\n    1\n}\n";
    let doc = Document::new(lang("rust"), text);
    let folds: Vec<_> = doc
        .folding_ranges()
        .iter()
        .map(|f| (f.start_line, f.end_line, f.kind))
        .collect();
    assert_eq!(
        folds,
        [
            (0, 2, FoldKind::Imports),
            (4, 6, FoldKind::Comment),
            (7, 9, FoldKind::Region),
        ]
    );
}

/// A Rust file of about `lines` lines.
fn big_rust(lines: usize) -> String {
    let mut text = String::new();
    let mut i = 0;
    while text.lines().count() < lines {
        text.push_str(&format!(
            "/// Item {i}.\n\
             pub struct S{i} {{\n    value: u64,\n    name: String,\n}}\n\n\
             impl S{i} {{\n    pub fn get(&self) -> u64 {{\n        \
             if self.value > {i} {{ self.value * 2 }} else {{ self.value + 1 }}\n    \
             }}\n}}\n\n"
        ));
        i += 1;
    }
    text
}

#[test]
fn a_10k_line_file_parses_fast() {
    let text = big_rust(10_000);
    let lines = text.lines().count();
    let started = Instant::now();
    let mut doc = Document::new(lang("rust"), text);
    let parse = started.elapsed();

    let started = Instant::now();
    let symbols = doc.symbols();
    let folds = doc.folding_ranges();
    let query = started.elapsed();
    assert!(symbols.len() > 2_000, "{}", symbols.len());
    assert!(!folds.is_empty());

    let at = doc.text().len() / 2;
    let at = (0..at)
        .rev()
        .find(|&i| doc.text().is_char_boundary(i))
        .unwrap();
    let started = Instant::now();
    doc.edit(&[TextEdit::new(at..at, "\n")]).unwrap();
    let edit = started.elapsed();
    println!("{lines} lines: parse {parse:?}, symbols + folds {query:?}, edit {edit:?}");

    // The budget is for optimised builds; debug builds compile the grammars
    // without optimisation.
    let budget = if cfg!(debug_assertions) {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(100)
    };
    assert!(parse < budget, "parsing {lines} lines took {parse:?}");
    assert!(
        edit < parse,
        "an incremental edit ({edit:?}) beat a full parse ({parse:?})"
    );
}
