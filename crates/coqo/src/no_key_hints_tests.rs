// SPDX-License-Identifier: Apache-2.0

//! A guard for the "no key hints in UI text" rule (docs/design/README.md,
//! #372): known hint phrases must not come back in a source file's string
//! literals. It is a list of phrases, not a parser, so it catches the
//! wording that was removed, not every new way to say it.

use std::path::Path;

/// Phrases of removed hints. Each is specific enough not to match prose.
const HINTS: &[&str] = &[
    "Enter to ",
    "Esc to ",
    "Enter or Tab to",
    "Enter sends",
    "Esc cancels",
    "Esc skip",
    "Enter send",
    "↑↓ to",
    "↑↓ move",
    "1–9 pick",
    "F8 next",
    "-click: add",
    "Ctrl S first",
    "Save with",
    "\"to move\"",
    "\"to run\"",
    "\"to close\"",
    "\"to open\"",
    "\"to the side\"",
];

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn ui_text_has_no_key_hints() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut found = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        // Tests may name a phrase to assert it is gone; this file lists them.
        if name.contains("tests") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for (ix, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for hint in HINTS {
                if line.contains(hint) {
                    found.push(format!("{}:{}: {hint:?}", file.display(), ix + 1));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "key hints in UI text:\n{}",
        found.join("\n")
    );
}
