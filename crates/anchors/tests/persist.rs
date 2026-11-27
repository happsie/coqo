// SPDX-License-Identifier: Apache-2.0

//! Storage: anchors round-trip through JSON, keep their format, and resolve
//! after a restart (no history: the file changed meanwhile).

use coqo_anchors::{Anchor, Resolution, Snapshot, Status, text_hash};
use coqo_syntax::{Document, Language};

const BEFORE: &str = "mod shapes {\n    pub struct Circle {\n        r: f64,\n    }\n\n    impl Circle {\n        pub fn area(&self) -> f64 {\n            std::f64::consts::PI * self.r * self.r\n        }\n    }\n}\n";

fn doc(text: &str) -> Document {
    Document::new(Language::detect("src/shapes.rs", text).unwrap(), text)
}

#[test]
fn round_trips_and_resolves_after_restart() {
    let d = doc(BEFORE);
    let snap = Snapshot::from_document(&d).at_rev(7);
    let anchors = vec![
        Anchor::lines("src/shapes.rs", &snap, 7..=7).unwrap(),
        Anchor::lines("src/shapes.rs", &snap, 2..=2).unwrap(),
    ];
    assert_eq!(
        anchors[0].symbol.as_ref().unwrap().qualified_name("::"),
        "shapes::Circle::area"
    );
    let stored = serde_json::to_string(&anchors).unwrap();
    drop(snap);

    // The daemon restarts; meanwhile the file gained a doc comment and a
    // field.
    let loaded: Vec<Anchor> = serde_json::from_str(&stored).unwrap();
    assert_eq!(loaded, anchors);
    let after = BEFORE
        .replace(
            "    pub struct Circle {\n",
            "    /// A circle.\n    pub struct Circle {\n",
        )
        .replace(
            "        r: f64,\n",
            "        r: f64,\n        name: String,\n",
        );
    let d = doc(&after);
    let snap = Snapshot::from_document(&d);
    let rs = snap.resolve_all(&loaded, None);
    assert_eq!(rs[0].status, Status::Moved);
    assert_eq!(rs[0].range.unwrap().start.line, 9);
    assert_eq!(&after[rs[0].bytes().unwrap()], loaded[0].text);
    assert_eq!(rs[1].status, Status::Moved);
    assert_eq!(rs[1].range.unwrap().start.line, 3);

    // Rebasing gives a fresh anchor at the new place.
    let rebased = loaded[0].rebased(&snap, &rs[0]).unwrap();
    assert_eq!(rebased.range.start.line, 9);
    assert_eq!(rebased.text, loaded[0].text);
    assert_eq!(snap.resolve(&rebased, None).status, Status::Exact);
}

/// The stored form: changing it breaks anchors saved by earlier versions.
#[test]
fn json_format_is_stable() {
    let text = "fn main() {\n    let x = 1;\n}\n";
    let d = doc(text);
    let snap = Snapshot::from_document(&d).at_rev(3);
    let a = Anchor::new("src/main.rs", &snap, 20..25).unwrap();
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "path": "src/main.rs",
            "rev": 3,
            "range": {
                "start": { "line": 1, "col": 8, "offset": 20 },
                "end": { "line": 1, "col": 13, "offset": 25 }
            },
            "text_hash": text_hash("x = 1"),
            "text": "x = 1",
            "prefix": "    let ",
            "suffix": ";",
            "context_before": ["fn main() {"],
            "context_after": ["}", ""],
            "symbol": { "path": ["main"], "kind": "function", "line_offset": 1 }
        })
    );

    // Optional fields may be missing (a disk anchor with no parse).
    let minimal: Anchor = serde_json::from_value(serde_json::json!({
        "path": "README.md",
        "range": {
            "start": { "line": 0, "col": 0, "offset": 0 },
            "end": { "line": 0, "col": 5, "offset": 5 }
        },
        "text_hash": text_hash("Hello"),
        "text": "Hello"
    }))
    .unwrap();
    assert_eq!(minimal.rev, None);
    assert!(minimal.context_before.is_empty() && minimal.symbol.is_none());
    let r = Snapshot::new("Hello\n").resolve(&minimal, None);
    assert_eq!(r.status, Status::Exact);
}

#[test]
fn resolution_serialises() {
    let fuzzy = Resolution {
        status: Status::Fuzzy { confidence: 0.75 },
        range: None,
        rev: Some(2),
    };
    let json = serde_json::to_value(fuzzy).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "status": { "kind": "fuzzy", "confidence": 0.75 }, "rev": 2 })
    );
    let orphaned = serde_json::to_value(Status::Orphaned).unwrap();
    assert_eq!(orphaned, serde_json::json!({ "kind": "orphaned" }));
    let back: Resolution = serde_json::from_value(json).unwrap();
    assert_eq!(back, fuzzy);
}
