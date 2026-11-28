// SPDX-License-Identifier: Apache-2.0

//! Discarding unstaged changes (#226): whole files and chosen lines go back
//! to the index version; untracked files are refused (the daemon trashes
//! them).

use super::*;

#[test]
fn discarding_whole_files_restores_the_index_version() {
    let (_tmp, f) = fixture();
    f.write("edited.txt", &numbered(5));
    f.write("deleted.txt", "gone soon\n");
    f.write("both.txt", "one\n");
    f.commit_all("base");
    f.write("edited.txt", "rewritten\n");
    std::fs::remove_file(f.path().join("deleted.txt")).unwrap();
    // Staged changes stay staged: only the work tree's go.
    f.write("both.txt", "two\n");
    f.git(&["add", "both.txt"]);
    f.write("both.txt", "three\n");
    f.write("untracked.txt", "new\n");
    let repo = open(&f);

    repo.discard(&["edited.txt".into(), "deleted.txt".into(), "both.txt".into()])
        .unwrap();
    assert_eq!(f.read("edited.txt"), numbered(5));
    assert_eq!(f.read("deleted.txt"), "gone soon\n");
    assert_eq!(f.read("both.txt"), "two\n");
    assert_eq!(staged(&f, "both.txt"), "two\n");
    assert_eq!(
        porcelain(&f),
        [("M.", "both.txt"), ("??", "untracked.txt")]
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect()
    );

    // Untracked files have nothing to go back to: refused, left alone.
    let err = repo.discard(&["untracked.txt".into()]).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert_eq!(f.read("untracked.txt"), "new\n");
    // An intent-to-add file too.
    f.git(&["add", "-N", "untracked.txt"]);
    let err = repo.discard(&["untracked.txt".into()]).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert_eq!(f.read("untracked.txt"), "new\n");
    // Nothing to discard is fine; paths stay inside the repository.
    repo.discard(&["edited.txt".into()]).unwrap();
    assert!(repo.discard(&["../outside".into()]).is_err());
}

#[test]
fn discarding_a_hunk_and_lines_keeps_the_other_changes() {
    let (_tmp, f) = fixture();
    f.write("f.txt", &numbered(40));
    f.commit_all("base");
    let edited = numbered(40)
        .replace("line 2\n", "line 2\nadded after 2\n")
        .replace("line 20\n", "line twenty\n")
        .replace("line 38\n", "line 38\nA\nB\n");
    f.write("f.txt", &edited);
    let repo = open(&f);

    // The middle hunk, whole.
    let d = repo
        .diff("f.txt", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    assert_eq!(d.hunks.len(), 3);
    repo.discard_lines("f.txt", &LineSelection::hunk(&d.hunks[1]))
        .unwrap();
    let without_twenty = numbered(40)
        .replace("line 2\n", "line 2\nadded after 2\n")
        .replace("line 38\n", "line 38\nA\nB\n");
    assert_eq!(f.read("f.txt"), without_twenty);
    assert_eq!(staged(&f, "f.txt"), numbered(40), "the index is untouched");

    // One added line of the last hunk ("A", not "B").
    let d = repo
        .diff("f.txt", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    let a = d
        .hunks
        .last()
        .unwrap()
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Added && l.text == "A")
        .unwrap();
    repo.discard_lines(
        "f.txt",
        &LineSelection {
            removed: vec![],
            added: vec![a.new_line.unwrap()],
        },
    )
    .unwrap();
    assert_eq!(
        f.read("f.txt"),
        numbered(40)
            .replace("line 2\n", "line 2\nadded after 2\n")
            .replace("line 38\n", "line 38\nB\n")
    );

    // A stale selection changes nothing.
    let err = repo
        .discard_lines(
            "f.txt",
            &LineSelection {
                removed: vec![],
                added: vec![30],
            },
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StaleSelection);

    // The rest, hunk by hunk: the file is the index's again.
    loop {
        let d = repo
            .diff("f.txt", &Side::Index, &Side::Worktree, 3)
            .unwrap();
        let Some(h) = d.hunks.first() else { break };
        repo.discard_lines("f.txt", &LineSelection::hunk(h))
            .unwrap();
    }
    assert_eq!(f.read("f.txt"), numbered(40));
    assert_eq!(git_hunks(&f, &[]), "");
}

#[test]
fn discarding_lines_keeps_crlf_line_ends_and_refuses_what_has_no_lines() {
    let (_tmp, f) = fixture();
    f.write(".gitattributes", "*.crlf text eol=crlf\n");
    std::fs::write(f.path().join("ends.crlf"), b"one\r\ntwo\r\nthree\r\n").unwrap();
    std::fs::write(f.path().join("bin.dat"), [0u8, 1, 2]).unwrap();
    f.commit_all("base");
    std::fs::write(f.path().join("ends.crlf"), b"ONE\r\ntwo\r\nTHREE\r\n").unwrap();
    let repo = open(&f);
    let d = repo
        .diff("ends.crlf", &Side::Index, &Side::Worktree, 0)
        .unwrap();
    assert_eq!(d.hunks.len(), 2, "{d:#?}");
    let d = repo
        .diff("ends.crlf", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    let ones = LineSelection {
        removed: vec![1],
        added: vec![1],
    };
    assert!(d.hunks[0].lines.iter().any(|l| l.text == "ONE"));
    repo.discard_lines("ends.crlf", &ones).unwrap();
    assert_eq!(
        std::fs::read(f.path().join("ends.crlf")).unwrap(),
        b"one\r\ntwo\r\nTHREE\r\n"
    );

    std::fs::write(f.path().join("bin.dat"), [9u8, 0, 9]).unwrap();
    f.write("new.txt", "a\n");
    let one = LineSelection {
        removed: vec![],
        added: vec![1],
    };
    for path in ["bin.dat", "new.txt"] {
        let err = repo.discard_lines(path, &one).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unsupported, "{path}");
    }
    assert_eq!(f.read("new.txt"), "a\n");
}
