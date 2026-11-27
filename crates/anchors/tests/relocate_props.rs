// SPDX-License-Identifier: Apache-2.0

//! Property tests for relocation without history: an unchanged text
//! resolves exactly, and a found anchor always carries its own claim
//! (exact and moved anchors point at exactly the anchored text, fuzzy ones
//! have a confidence in range).

use coqo_anchors::{Anchor, MIN_CONFIDENCE, Snapshot, Status};
use proptest::prelude::*;

fn source() -> impl Strategy<Value = String> {
    let word = prop_oneof!["[a-z]{1,6}", Just("{".to_owned()), Just("}".to_owned())];
    let line = (0usize..3, prop::collection::vec(word, 0..6))
        .prop_map(|(indent, words)| format!("{}{}", "    ".repeat(indent), words.join(" ")));
    prop::collection::vec(line, 1..40).prop_map(|lines| lines.join("\n") + "\n")
}

/// Random line-level edits: delete, duplicate, insert or change a line.
fn edit_lines(text: &str, ops: &[(u8, usize)]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for &(op, at) in ops {
        if lines.is_empty() {
            lines.push(String::new());
        }
        let i = at % lines.len();
        match op % 4 {
            0 => {
                lines.remove(i);
            }
            1 => {
                let l = lines[i].clone();
                lines.insert((at / 7) % (lines.len() + 1), l);
            }
            2 => lines.insert(i, "    NEW LINE".to_owned()),
            _ => lines[i].push_str(" CHANGED"),
        }
    }
    lines.join("\n") + "\n"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn unchanged_text_is_exact(text in source(), at in any::<u32>(), n in 0usize..3) {
        let snap = Snapshot::new(&text);
        let first = at as usize % snap.line_count();
        let last = (first + n).min(snap.line_count() - 1);
        let anchor = Anchor::lines("f.rs", &snap, first..=last).unwrap();
        let r = snap.resolve(&anchor, None);
        prop_assert_eq!(r.status, Status::Exact);
        prop_assert_eq!(r.bytes(), Some(anchor.byte_range()));
    }

    #[test]
    fn found_anchors_keep_their_claim(
        text in source(),
        at in any::<u32>(),
        n in 0usize..3,
        ops in prop::collection::vec((any::<u8>(), any::<usize>()), 1..6),
    ) {
        let snap = Snapshot::new(&text);
        let first = at as usize % snap.line_count();
        let last = (first + n).min(snap.line_count() - 1);
        let anchor = Anchor::lines("f.rs", &snap, first..=last).unwrap();
        let after = edit_lines(&text, &ops);
        let r = Snapshot::new(&after).resolve(&anchor, None);
        match r.status {
            Status::Exact => {
                prop_assert_eq!(r.bytes(), Some(anchor.byte_range()));
                prop_assert_eq!(&after[anchor.byte_range()], anchor.text.as_str());
            }
            Status::Moved => {
                prop_assert_eq!(&after[r.bytes().unwrap()], anchor.text.as_str());
            }
            Status::Fuzzy { confidence } => {
                prop_assert!((MIN_CONFIDENCE..1.0).contains(&confidence));
                prop_assert!(after.is_char_boundary(r.bytes().unwrap().start));
            }
            Status::Orphaned => prop_assert!(r.range.is_none()),
        }
    }
}
