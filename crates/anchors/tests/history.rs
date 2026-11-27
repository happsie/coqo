// SPDX-License-Identifier: Apache-2.0

//! Property tests: anchors resolved through random edit sequences (the
//! exact path) are correct or orphaned, never wrong.
//!
//! The oracle tags every byte of the original text with its index and
//! carries the tags through the edits, so it knows which bytes of the
//! anchored range survived and where they are.

use coqo_anchors::{Anchor, MIN_CONFIDENCE, Snapshot, Status, map_range};
use coqo_syntax::TextEdit;
use proptest::prelude::*;

/// A byte and the index it had in the original text (`None` if inserted).
type Cell = (u8, Option<usize>);

fn text_of(cells: &[Cell]) -> String {
    String::from_utf8(cells.iter().map(|c| c.0).collect()).unwrap()
}

/// What an edit inserts.
#[derive(Clone, Debug)]
enum Insert {
    /// New text (upper case: never equal to original text by accident).
    Fresh(String),
    /// A copy of the current text from `at` (a fraction), `len` bytes: a
    /// paste, which can recreate the anchored text elsewhere.
    Copy(u32, usize),
}

/// A raw edit, fitted to the text when applied.
#[derive(Clone, Debug)]
struct RawEdit {
    at: u32,
    delete: usize,
    insert: Insert,
}

fn raw_edit() -> impl Strategy<Value = RawEdit> {
    let insert = prop_oneof![
        3 => "[XYZ \n]{0,6}".prop_map(Insert::Fresh),
        1 => (any::<u32>(), 0usize..24).prop_map(|(at, len)| Insert::Copy(at, len)),
    ];
    (
        any::<u32>(),
        prop_oneof![3 => 0usize..4, 1 => 0usize..40],
        insert,
    )
        .prop_map(|(at, delete, insert)| RawEdit { at, delete, insert })
}

/// Fit raw edits to a text of `cells`: sorted, non-overlapping, in range.
fn fit(cells: &[Cell], raw: &[RawEdit]) -> Vec<TextEdit> {
    let len = cells.len();
    let text = text_of(cells);
    let mut raw: Vec<&RawEdit> = raw.iter().collect();
    raw.sort_by_key(|r| r.at as usize % (len + 1));
    let mut out: Vec<TextEdit> = Vec::new();
    let mut floor = 0;
    for r in raw {
        let s = (r.at as usize % (len + 1)).max(floor);
        if s > len {
            break;
        }
        let t = (s + r.delete).min(len);
        let ins = match &r.insert {
            Insert::Fresh(s) => s.clone(),
            Insert::Copy(at, n) => {
                let from = *at as usize % (len + 1);
                text[from..(from + n).min(len)].to_owned()
            }
        };
        if s == t && ins.is_empty() {
            continue;
        }
        // Two inserts at one spot would be one edit.
        if let Some(last) = out.last_mut()
            && last.range.end == s
            && s == t
        {
            last.text.push_str(&ins);
            continue;
        }
        out.push(TextEdit::new(s..t, ins));
        floor = t.max(s + 1);
    }
    out
}

fn apply(cells: &mut Vec<Cell>, edits: &[TextEdit]) {
    for e in edits.iter().rev() {
        cells.splice(e.range.clone(), e.text.bytes().map(|b| (b, None)));
    }
}

fn original_text() -> impl Strategy<Value = String> {
    let word = "[a-z]{1,5}";
    let line = (0usize..3, prop::collection::vec(word, 0..5))
        .prop_map(|(indent, words)| format!("{}{}", "    ".repeat(indent), words.join(" ")));
    prop::collection::vec(line, 1..30).prop_map(|lines| lines.join("\n") + "\n")
}

#[derive(Clone, Debug)]
enum AnchorPick {
    Bytes(u32, u32),
    Lines(u32, u32),
}

fn anchor_pick() -> impl Strategy<Value = AnchorPick> {
    prop_oneof![
        (any::<u32>(), any::<u32>()).prop_map(|(a, b)| AnchorPick::Bytes(a, b)),
        (any::<u32>(), 0u32..4).prop_map(|(a, n)| AnchorPick::Lines(a, n)),
    ]
}

fn make_anchor(snap: &Snapshot<'_>, pick: &AnchorPick) -> Anchor {
    let len = snap.text().len();
    match *pick {
        AnchorPick::Bytes(a, b) => {
            let a = a as usize % (len + 1);
            let b = a + b as usize % (len - a + 1);
            Anchor::new("f.txt", snap, a..b).unwrap()
        }
        AnchorPick::Lines(a, n) => {
            let lines = snap.line_count();
            let first = a as usize % lines;
            let last = (first + n as usize).min(lines - 1);
            Anchor::lines("f.txt", snap, first..=last).unwrap()
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn anchors_are_right_or_orphaned(
        original in original_text(),
        pick in anchor_pick(),
        changes in prop::collection::vec(prop::collection::vec(raw_edit(), 1..4), 0..6),
    ) {
        let snap = Snapshot::new(&original).at_rev(0);
        let anchor = make_anchor(&snap, &pick);
        let (a, b) = (anchor.range.start.offset, anchor.range.end.offset);

        let mut cells: Vec<Cell> = original.bytes().enumerate().map(|(i, c)| (c, Some(i))).collect();
        let mut history = Vec::new();
        for raw in &changes {
            let edits = fit(&cells, raw);
            apply(&mut cells, &edits);
            history.push(edits);
        }
        let text = text_of(&cells);
        let now = Snapshot::new(&text).at_rev(history.len() as u64);
        let r = now.resolve(&anchor, Some(&history));
        let mapped = map_range(a..b, &history);

        let inside = |c: &Cell| c.1.is_some_and(|i| a <= i && i < b);
        let outside = |c: &Cell| c.1.is_some_and(|i| i < a || b <= i);
        let survivors: Vec<usize> = (0..cells.len()).filter(|&p| inside(&cells[p])).collect();

        // The exact mapping never takes in original text from outside the
        // range, and keeps all of what survived inside it.
        if !mapped.collapsed {
            for p in mapped.range.clone() {
                prop_assert!(!outside(&cells[p]), "mapped range takes in outside byte at {p}");
            }
            for &p in &survivors {
                prop_assert!(mapped.range.contains(&p), "surviving byte {p} left out");
            }
        } else {
            prop_assert!(survivors.is_empty());
        }

        // Untouched: exactly where the oracle says, with the same text.
        let intact = b > a
            && survivors.len() == b - a
            && survivors.windows(2).all(|w| w[1] == w[0] + 1);
        if intact {
            let expected = survivors[0]..survivors[0] + (b - a);
            prop_assert_eq!(r.bytes(), Some(expected.clone()));
            let want = if expected == (a..b) { Status::Exact } else { Status::Moved };
            prop_assert_eq!(r.status, want);
        }

        match r.status {
            Status::Exact | Status::Moved => {
                // Only ever the anchored text.
                prop_assert_eq!(&text[r.bytes().unwrap()], anchor.text.as_str());
            }
            Status::Fuzzy { confidence } => {
                prop_assert!((MIN_CONFIDENCE..1.0).contains(&confidence));
                if !mapped.swallowed {
                    // Edited in place: what is left of the anchored text,
                    // plus what was typed into it; nothing from outside.
                    prop_assert!(!mapped.collapsed);
                    let range = r.bytes().unwrap();
                    prop_assert_eq!(range, mapped.range.clone());
                }
            }
            Status::Orphaned => {
                prop_assert!(!intact);
                prop_assert!(r.range.is_none());
            }
        }
    }
}

/// A whole-file rewrite seen as one replacement (prefix/suffix scan, as
/// the daemon diffs disk writes): the anchor is relocated.
#[test]
fn whole_file_rewrite_relocates() {
    let before = "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n";
    let snap = Snapshot::new(before).at_rev(1);
    let anchor = Anchor::lines("src/lib.rs", &snap, 5..=5).unwrap();
    assert_eq!(anchor.text, "    two();\n");
    // `b` moved above `a`, and a line was added.
    let after = "// moved\nfn b() {\n    two();\n}\n\nfn a() {\n    one();\n}\n";
    let prefix = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(x, y)| x == y)
        .count();
    let suffix = before
        .bytes()
        .rev()
        .zip(after.bytes().rev())
        .take_while(|(x, y)| x == y)
        .count()
        .min(before.len() - prefix)
        .min(after.len() - prefix);
    let edit = TextEdit::new(
        prefix..before.len() - suffix,
        &after[prefix..after.len() - suffix],
    );
    let history = vec![vec![edit]];
    assert!(map_range(anchor.byte_range(), &history).swallowed);
    let now = Snapshot::new(after).at_rev(2);
    let r = now.resolve(&anchor, Some(&history));
    assert_eq!(r.status, Status::Moved);
    assert_eq!(&after[r.bytes().unwrap()], "    two();\n");
}

/// A line typed inside the anchor keeps it, fuzzily; deleting it orphans it.
#[test]
fn edits_in_place() {
    let before = "fn a() {\n    let total = price * qty;\n}\n";
    let snap = Snapshot::new(before).at_rev(1);
    let anchor = Anchor::lines("src/lib.rs", &snap, 1..=1).unwrap();

    let at = before.find("qty").unwrap();
    let edit = vec![vec![TextEdit::new(at..at + 3, "quantity")]];
    let after = before.replace("qty", "quantity");
    let r = Snapshot::new(&after)
        .at_rev(2)
        .resolve(&anchor, Some(&edit));
    assert!(matches!(r.status, Status::Fuzzy { .. }), "{r:?}");
    assert_eq!(
        &after[r.bytes().unwrap()],
        "    let total = price * quantity;\n"
    );

    let line = anchor.byte_range();
    let edit = vec![vec![TextEdit::new(line, "")]];
    let after = "fn a() {\n}\n";
    let r = Snapshot::new(after).at_rev(2).resolve(&anchor, Some(&edit));
    assert_eq!(r.status, Status::Orphaned);

    // Retyped from scratch: nothing like it, orphaned too.
    let edit = vec![vec![TextEdit::new(anchor.byte_range(), "    run();\n")]];
    let after = "fn a() {\n    run();\n}\n";
    let r = Snapshot::new(after).at_rev(2).resolve(&anchor, Some(&edit));
    assert_eq!(r.status, Status::Orphaned);

    // Cut and pasted elsewhere: an exact copy is found.
    let after = "fn a() {\n}\n\nfn b() {\n    let total = price * qty;\n}\n";
    let edit = vec![vec![
        TextEdit::new(anchor.byte_range(), ""),
        TextEdit::new(
            before.len()..before.len(),
            "\nfn b() {\n    let total = price * qty;\n}\n",
        ),
    ]];
    let r = Snapshot::new(after).at_rev(2).resolve(&anchor, Some(&edit));
    assert_eq!(r.status, Status::Moved);
    assert_eq!(&after[r.bytes().unwrap()], anchor.text);
}
