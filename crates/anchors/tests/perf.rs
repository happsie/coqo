// SPDX-License-Identifier: Apache-2.0

//! Performance sanity: re-anchoring 1k anchors in a 10k-line file after an
//! edit. The target is well under 10 ms in release (`cargo test --release
//! -p coqo-anchors --test perf -- --nocapture`); debug builds only get a
//! loose bound.

use std::time::{Duration, Instant};

use coqo_anchors::{Anchor, Snapshot, Status};
use coqo_syntax::{Document, Language, TextEdit};

fn source(lines: usize) -> String {
    let mut s = String::new();
    let mut i = 0;
    while s.lines().count() < lines {
        s.push_str(&format!(
            "pub fn function_{i}(x: u64) -> u64 {{\n    let y = x * {i} + 1;\n    if y > 100 {{\n        return y - {i};\n    }}\n    y\n}}\n\n"
        ));
        i += 1;
    }
    s
}

#[test]
fn reanchoring_1k_anchors_in_10k_lines() {
    let before = source(10_000);
    let lang = Language::detect("src/big.rs", &before).unwrap();
    let doc = Document::new(lang, before.as_str());
    let snap = Snapshot::from_document(&doc).at_rev(1);
    let lines = snap.line_count();
    let anchors: Vec<Anchor> = (0..1_000)
        .map(|i| {
            let line = (i * 9 + 1) % (lines - 1);
            Anchor::lines("src/big.rs", &snap, line..=line).unwrap()
        })
        .collect();

    // A line typed near the top, as one buffer change.
    let at = before.find("    y\n").unwrap();
    let mut doc = doc;
    let edit = TextEdit::new(at..at, "    let z = 2;\n");
    doc.edit(std::slice::from_ref(&edit)).unwrap();
    let history = vec![vec![edit]];

    let start = Instant::now();
    let snap = Snapshot::from_document(&doc).at_rev(2);
    let resolved = snap.resolve_all(&anchors, Some(&history));
    let took = start.elapsed();
    println!("re-anchored {} anchors in {took:?}", anchors.len());

    assert!(resolved.iter().all(|r| r.status != Status::Orphaned));
    let moved = resolved
        .iter()
        .filter(|r| r.status == Status::Moved)
        .count();
    assert!(moved > 990, "{moved} moved");
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(10)
    };
    assert!(took < budget, "took {took:?}");
}

/// Without history (after a restart) every anchor is relocated by its
/// fingerprint: slower, but done once per file.
#[test]
fn relocating_1k_anchors_without_history() {
    let before = source(10_000);
    let snap = Snapshot::new(&before);
    let lines = snap.line_count();
    let anchors: Vec<Anchor> = (0..1_000)
        .map(|i| {
            // The `let y = …` lines: unique, but alike.
            let line = (i * 8 + 1) % (lines - 1);
            Anchor::lines("src/big.rs", &snap, line..=line).unwrap()
        })
        .collect();
    let after = format!("use std::fmt;\n\n{before}");

    let start = Instant::now();
    let snap = Snapshot::new(&after);
    let resolved = snap.resolve_all(&anchors, None);
    let took = start.elapsed();
    println!("relocated {} anchors in {took:?}", anchors.len());

    for (a, r) in anchors.iter().zip(&resolved) {
        assert_eq!(r.status, Status::Moved, "{:?}", a.text);
        assert_eq!(r.range.unwrap().start.line, a.range.start.line + 2);
    }
    let budget = if cfg!(debug_assertions) {
        Duration::from_secs(20)
    } else {
        Duration::from_secs(1)
    };
    assert!(took < budget, "took {took:?}");
}
