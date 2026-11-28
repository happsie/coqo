// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use super::*;

fn query(pattern: &str) -> Query {
    Query {
        pattern: pattern.into(),
        case_sensitive: true,
        ..Query::default()
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Search `root` and return `(rel, [(line, start, end)])`, sorted.
fn run(
    m: &Matcher,
    root: &Path,
    overlay: &Overlay,
) -> (Vec<(String, Vec<(u32, u32, u32)>)>, Summary) {
    let out = Mutex::new(Vec::new());
    let root = Root::new(root.canonicalize().unwrap());
    let summary = m
        .search(&[root], overlay, 10_000, &AtomicBool::new(false), &|f| {
            let at = f.matches.iter().map(|m| (m.line, m.start, m.end)).collect();
            out.lock().unwrap().push((f.rel, at));
            true
        })
        .unwrap();
    let mut out = out.into_inner().unwrap();
    out.sort();
    (out, summary)
}

#[test]
fn the_manifest_is_valid() {
    assert_eq!(manifest().id.as_str(), "coqo.search");
}

#[test]
fn finds_literals_with_lines_and_character_columns() {
    let m = Search.compile(&query("a.b")).unwrap();
    let found = m.find("x a.b y\naxb\n  ü a.b a.b\n");
    let at: Vec<_> = found.iter().map(|m| (m.line, m.start, m.end)).collect();
    // Literal: `.` is a dot; columns count characters, not bytes.
    assert_eq!(at, vec![(0, 2, 5), (2, 4, 7), (2, 8, 11)]);
    assert_eq!(found[1].preview, "ü a.b a.b");
    assert_eq!(&found[1].preview[found[1].preview_range.clone()], "a.b");
    assert_eq!(found[1].bytes, 17..20);
}

#[test]
fn case_whole_word_and_regex() {
    let text = "Foo foo food\n";
    let mut q = query("foo");
    assert_eq!(Search.compile(&q).unwrap().find(text).len(), 2);
    q.case_sensitive = false;
    assert_eq!(Search.compile(&q).unwrap().find(text).len(), 3);
    q.whole_word = true;
    assert_eq!(Search.compile(&q).unwrap().find(text).len(), 2);
    let mut q = query(r"fo+d?\b");
    q.regex = true;
    let at: Vec<_> = Search
        .compile(&q)
        .unwrap()
        .find(text)
        .iter()
        .map(|m| (m.start, m.end))
        .collect();
    assert_eq!(at, vec![(4, 7), (8, 12)]);
}

#[test]
fn bad_queries_are_refused() {
    let mut q = query("(unclosed");
    q.regex = true;
    assert_eq!(
        Search.compile(&q).unwrap_err().kind,
        ErrorKind::InvalidQuery
    );
    assert_eq!(
        Search.compile(&query("")).unwrap_err().kind,
        ErrorKind::InvalidQuery
    );
    let mut q = query("x");
    q.include = vec!["{unclosed".into()];
    assert_eq!(
        Search.compile(&q).unwrap_err().kind,
        ErrorKind::InvalidQuery
    );
    // A literal "(" is fine.
    assert!(Search.compile(&query("(")).is_ok());
}

#[test]
fn empty_matches_are_not_reported() {
    let mut q = query("x*");
    q.regex = true;
    let found = Search.compile(&q).unwrap().find("ab\nxx\n");
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].line, found[0].start, found[0].end), (1, 0, 2));
}

#[test]
fn crlf_lines_do_not_leak_into_matches() {
    let mut q = query("b$");
    q.regex = true;
    let found = Search.compile(&q).unwrap().find("ab\r\ncb\r\n");
    let at: Vec<_> = found.iter().map(|m| (m.line, m.start, m.end)).collect();
    assert_eq!(at, vec![(0, 1, 2), (1, 1, 2)]);
    assert_eq!(found[0].preview, "ab");
}

#[test]
fn long_lines_preview_a_window() {
    let line = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
    let found = Search.compile(&query("needle")).unwrap().find(&line);
    let m = &found[0];
    assert!(m.preview.len() <= MAX_PREVIEW + 6, "{}", m.preview.len());
    assert!(m.preview.starts_with('…') && m.preview.ends_with('…'));
    assert_eq!(&m.preview[m.preview_range.clone()], "needle");
    assert_eq!((m.start, m.end), (500, 506));
}

#[test]
fn replaces_literally_or_with_capture_groups() {
    let text = "let a = f(1);\nlet b = f(22);\n";
    let m = Search.compile(&query("f(")).unwrap();
    let r = m.replace(text, "g(", None);
    assert_eq!(r.len(), 2);
    assert_eq!(apply(text, &r), "let a = g(1);\nlet b = g(22);\n");
    // `$` is literal in a literal replace.
    assert_eq!(
        apply(text, &m.replace(text, "$1(", None)),
        "let a = $1(1);\nlet b = $1(22);\n"
    );

    let mut q = query(r"f\((\d+)\)");
    q.regex = true;
    let m = Search.compile(&q).unwrap();
    assert_eq!(
        apply(text, &m.replace(text, "h(${1}0, $$)", None)),
        "let a = h(10, $);\nlet b = h(220, $);\n"
    );
    // Only the named match.
    let only = [MatchAt {
        line: 1,
        start: 8,
        end: 13,
    }];
    let r = m.replace(text, "x", Some(&only));
    assert_eq!(apply(text, &r), "let a = f(1);\nlet b = x;\n");
    // A match that moved is not replaced.
    let only = [MatchAt {
        line: 1,
        start: 9,
        end: 14,
    }];
    assert!(m.replace(text, "x", Some(&only)).is_empty());
}

#[test]
fn whole_word_replaces_only_the_word() {
    let mut q = query("id");
    q.whole_word = true;
    let text = "id(ids, id)\n";
    let m = Search.compile(&q).unwrap();
    assert_eq!(
        apply(text, &m.replace(text, "key", None)),
        "key(ids, key)\n"
    );
    let mut q = query(r"(i)d");
    q.regex = true;
    q.whole_word = true;
    let m = Search.compile(&q).unwrap();
    assert_eq!(
        apply(text, &m.replace(text, "${1}x", None)),
        "ix(ids, ix)\n"
    );
}

fn apply(text: &str, r: &[Replacement]) -> String {
    let mut out = text.to_owned();
    for r in r.iter().rev() {
        out.replace_range(r.bytes.clone(), &r.text);
    }
    out
}

#[test]
fn search_respects_gitignore_and_skips_binaries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "target/\n*.log\n");
    write(root, "src/main.rs", "fn main() { needle(); }\n");
    write(root, "src/lib.rs", "// no match\n");
    write(root, "target/debug/out.rs", "needle\n");
    write(root, "build.log", "needle\n");
    write(root, ".github/ci.yml", "needle: true\n");
    write(root, "nested/.gitignore", "secret.txt\n");
    write(root, "nested/secret.txt", "needle\n");
    write(root, "nested/open.txt", "needle needle\n");
    std::fs::write(root.join("blob.bin"), b"needle\0\x01\x02").unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".git/config", "needle\n");

    let m = Search.compile(&query("needle")).unwrap();
    let (found, summary) = run(&m, root, &Overlay::new());
    assert_eq!(
        found,
        vec![
            (".github/ci.yml".to_owned(), vec![(0, 0, 6)]),
            ("nested/open.txt".to_owned(), vec![(0, 0, 6), (0, 7, 13)]),
            ("src/main.rs".to_owned(), vec![(0, 12, 18)]),
        ]
    );
    assert_eq!(summary.files, 3);
    assert_eq!(summary.matches, 4);
    assert!(!summary.limit_hit && !summary.cancelled);
}

#[test]
fn include_and_exclude_globs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "needle\n");
    write(root, "src/b.ts", "needle\n");
    write(root, "docs/c.md", "needle\n");
    write(root, "src/gen/d.rs", "needle\n");
    let rels = |include: &[&str], exclude: &[&str]| {
        let mut q = query("needle");
        q.include = include.iter().map(|s| s.to_string()).collect();
        q.exclude = exclude.iter().map(|s| s.to_string()).collect();
        let m = Search.compile(&q).unwrap();
        run(&m, root, &Overlay::new())
            .0
            .into_iter()
            .map(|(rel, _)| rel)
            .collect::<Vec<_>>()
    };
    assert_eq!(rels(&["*.rs"], &[]), ["src/a.rs", "src/gen/d.rs"]);
    assert_eq!(rels(&["*.rs"], &["gen"]), ["src/a.rs"]);
    assert_eq!(rels(&["docs"], &[]), ["docs/c.md"]);
    assert_eq!(rels(&[], &["*.md", "*.ts"]), ["src/a.rs", "src/gen/d.rs"]);
}

#[test]
fn the_overlay_is_searched_instead_of_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "open.rs", "old text\n");
    write(&root, "closed.rs", "needle on disk\n");
    let mut overlay = Overlay::new();
    overlay.insert(
        root.join("open.rs"),
        "unsaved\nneedle in the buffer\n".into(),
    );
    let m = Search.compile(&query("needle")).unwrap();
    let (found, _) = run(&m, &root, &overlay);
    assert_eq!(
        found,
        vec![
            ("closed.rs".to_owned(), vec![(0, 0, 6)]),
            ("open.rs".to_owned(), vec![(1, 0, 6)]),
        ]
    );
}

#[test]
fn stops_at_the_limit_or_when_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for i in 0..20 {
        write(root, &format!("f{i}.txt"), "needle needle\n");
    }
    let m = Search.compile(&query("needle")).unwrap();
    let roots = [Root::new(root.canonicalize().unwrap())];
    let count = Mutex::new(0);
    let summary = m
        .search(&roots, &Overlay::new(), 5, &AtomicBool::new(false), &|f| {
            *count.lock().unwrap() += f.matches.len();
            true
        })
        .unwrap();
    assert!(summary.limit_hit);
    assert_eq!(summary.matches, 5);
    assert_eq!(*count.lock().unwrap(), 5);

    let summary = m
        .search(
            &roots,
            &Overlay::new(),
            1000,
            &AtomicBool::new(true),
            &|_| true,
        )
        .unwrap();
    assert!(summary.cancelled);
    assert_eq!(summary.files, 0);

    // The sink stopping it counts as cancelled.
    let summary = m
        .search(
            &roots,
            &Overlay::new(),
            1000,
            &AtomicBool::new(false),
            &|_| false,
        )
        .unwrap();
    assert!(summary.cancelled);
    assert!(summary.files >= 1);
}
