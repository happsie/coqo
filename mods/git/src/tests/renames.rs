// SPDX-License-Identifier: Apache-2.0

//! Renames and `.gitattributes` against the `git` CLI (#226).

use super::*;

/// `git status --porcelain=v2 -z -uall --renames` as `(XY, path, orig)`.
fn porcelain_renames(f: &Fixture) -> BTreeSet<(String, String, String)> {
    let out = f.git(&["status", "--porcelain=v2", "-z", "-uall", "--renames"]);
    let mut records = out.split('\0').filter(|l| !l.is_empty());
    let mut set = BTreeSet::new();
    while let Some(l) = records.next() {
        match l.as_bytes()[0] {
            b'1' => {
                let parts: Vec<&str> = l.splitn(9, ' ').collect();
                set.insert((parts[1].to_owned(), parts[8].to_owned(), String::new()));
            }
            b'2' => {
                let parts: Vec<&str> = l.splitn(10, ' ').collect();
                let orig = records.next().unwrap_or_default();
                set.insert((parts[1].to_owned(), parts[9].to_owned(), orig.to_owned()));
            }
            b'u' => {
                let parts: Vec<&str> = l.splitn(11, ' ').collect();
                set.insert((parts[1].to_owned(), parts[10].to_owned(), String::new()));
            }
            b'?' => {
                set.insert(("??".to_owned(), l[2..].to_owned(), String::new()));
            }
            _ => {}
        }
    }
    set
}

fn ours_renames(repo: &Repo) -> BTreeSet<(String, String, String)> {
    repo.status()
        .unwrap()
        .entries
        .into_iter()
        .map(|e| {
            let xy = match (e.conflict, e.staged, e.unstaged) {
                (Some(c), _, _) => c.letters().to_owned(),
                (None, None, Some(Change::Untracked)) => "??".to_owned(),
                (None, s, u) => format!(
                    "{}{}",
                    s.map_or('.', Change::letter),
                    u.map_or('.', Change::letter)
                ),
            };
            (xy, e.path, e.orig_path.unwrap_or_default())
        })
        .collect()
}

/// `git diff -M --numstat -z args…` as `(path, orig, "add\tdel")`.
fn numstat_renames(f: &Fixture, args: &[&str]) -> Vec<(String, String, String)> {
    let mut all = vec!["diff", "-M", "--numstat", "-z"];
    all.extend(args);
    let out = f.git(&all);
    let mut records = out.split('\0');
    let mut rows = Vec::new();
    while let Some(r) = records.next() {
        if r.is_empty() {
            continue;
        }
        let parts: Vec<&str> = r.splitn(3, '\t').collect();
        let stat = format!("{}\t{}", parts[0], parts[1]);
        if parts[2].is_empty() {
            // A rename: the two paths follow.
            let orig = records.next().unwrap_or_default().to_owned();
            let path = records.next().unwrap_or_default().to_owned();
            rows.push((path, orig, stat));
        } else {
            rows.push((parts[2].to_owned(), String::new(), stat));
        }
    }
    rows.sort();
    rows
}

fn our_numstat_renames(repo: &Repo, base: &Side, head: &Side) -> Vec<(String, String, String)> {
    let mut rows: Vec<_> = repo
        .changed_files(base, head)
        .unwrap()
        .into_iter()
        .map(|c| {
            let stat = if c.binary {
                "-\t-".to_owned()
            } else {
                format!("{}\t{}", c.additions, c.deletions)
            };
            (c.path, c.orig_path.unwrap_or_default(), stat)
        })
        .collect();
    rows.sort();
    rows
}

/// A fixture with renames of every kind staged and unstaged.
fn renamed_fixture() -> (tempfile::TempDir, Fixture, String) {
    let (tmp, f) = fixture();
    f.write("exact.txt", &numbered(10));
    f.write("src/similar.rs", &numbered(30));
    f.write("lib/moved.rs", &numbered(12));
    f.write("rewritten.txt", &numbered(10));
    f.write("plain.txt", &numbered(8));
    f.write("ita-from.txt", &numbered(9));
    f.write("then-edited.txt", &numbered(15));
    f.write("empty", "");
    std::fs::write(f.path().join("bin.dat"), [0u8, 1, 2, 3, 4]).unwrap();
    let base = f.commit_all("base");

    // Staged: an exact rename, a similar one, a move to another folder,
    // an empty file, a binary file.
    f.git(&["mv", "exact.txt", "exact-renamed.txt"]);
    f.git(&["mv", "src/similar.rs", "src/similar2.rs"]);
    f.write(
        "src/similar2.rs",
        &numbered(30).replace("line 4\n", "line four\n"),
    );
    f.git(&["add", "src/similar2.rs"]);
    std::fs::create_dir_all(f.path().join("app")).unwrap();
    f.git(&["mv", "lib/moved.rs", "app/moved.rs"]);
    f.git(&["mv", "empty", "empty2"]);
    f.git(&["mv", "bin.dat", "bin2.dat"]);
    // Not similar enough: a delete and an add.
    f.git(&["rm", "-q", "rewritten.txt"]);
    f.write("unrelated.txt", "something else entirely\n");
    f.git(&["add", "unrelated.txt"]);
    // Renamed and staged, then edited in the work tree: `RM`.
    f.git(&["mv", "then-edited.txt", "edited-after.txt"]);
    f.write(
        "edited-after.txt",
        &numbered(15).replace("line 9\n", "nine\n"),
    );
    // Moved in the work tree only: a deletion and an untracked file, as
    // git has it.
    std::fs::rename(f.path().join("plain.txt"), f.path().join("plain2.txt")).unwrap();
    // Moved in the work tree with the new file intent-to-add: `.R`.
    std::fs::rename(f.path().join("ita-from.txt"), f.path().join("ita-to.txt")).unwrap();
    f.git(&["add", "-N", "ita-to.txt"]);
    (tmp, f, base)
}

#[test]
fn status_pairs_renames_like_git() {
    let (_tmp, f, _) = renamed_fixture();
    let repo = open(&f);
    let expected = porcelain_renames(&f);
    let renames: Vec<_> = expected.iter().filter(|(_, _, o)| !o.is_empty()).collect();
    assert!(renames.len() >= 6, "{expected:#?}");
    assert!(
        expected.contains(&(
            "RM".into(),
            "edited-after.txt".into(),
            "then-edited.txt".into()
        )),
        "{expected:#?}"
    );
    assert!(
        expected.contains(&(".R".into(), "ita-to.txt".into(), "ita-from.txt".into())),
        "{expected:#?}"
    );
    assert_eq!(ours_renames(&repo), expected);
}

#[test]
fn changed_files_pair_renames_like_git_diff_m() {
    let (_tmp, f, base) = renamed_fixture();
    let repo = open(&f);
    let head = Side::Rev("HEAD".into());
    let cached = numstat_renames(&f, &["--cached"]);
    assert!(
        cached
            .iter()
            .any(|(p, o, _)| p == "src/similar2.rs" && o == "src/similar.rs"),
        "{cached:#?}"
    );
    assert_eq!(our_numstat_renames(&repo, &head, &Side::Index), cached);
    assert_eq!(
        our_numstat_renames(&repo, &Side::Index, &Side::Worktree),
        numstat_renames(&f, &[])
    );
    assert_eq!(
        our_numstat_renames(&repo, &head, &Side::Worktree),
        numstat_renames(&f, &["HEAD"])
    );
    // Between commits.
    f.git(&["add", "-A"]);
    let after = f.commit_all("renames");
    let repo = open(&f);
    assert_eq!(
        our_numstat_renames(&repo, &Side::Rev(base.clone()), &Side::Rev(after.clone())),
        numstat_renames(&f, &[&base, &after])
    );
}

#[test]
fn a_renamed_file_diffs_from_its_old_path() {
    let (_tmp, f, _) = renamed_fixture();
    let repo = open(&f);
    let head = Side::Rev("HEAD".into());
    let d = repo
        .diff_paths("src/similar.rs", "src/similar2.rs", &head, &Side::Index, 3)
        .unwrap();
    assert_eq!(d.orig_path.as_deref(), Some("src/similar.rs"));
    assert_eq!((d.additions, d.deletions), (1, 1));
    assert_eq!(
        d.to_unified(),
        git_hunks(
            &f,
            &["-M", "--cached", "--", "src/similar.rs", "src/similar2.rs"]
        )
    );

    // Unstaging lines of a staged rename: the rename stays, the lines go.
    let staged_diff = repo
        .diff_paths("src/similar.rs", "src/similar2.rs", &head, &Side::Index, 3)
        .unwrap();
    repo.unstage_lines(
        "src/similar2.rs",
        &LineSelection::hunk(&staged_diff.hunks[0]),
    )
    .unwrap();
    assert_eq!(staged(&f, "src/similar2.rs"), numbered(30));
    assert!(
        porcelain_renames(&f).contains(&(
            "RM".into(),
            "src/similar2.rs".into(),
            "src/similar.rs".into()
        )),
        "{:#?}",
        porcelain_renames(&f)
    );
    assert_eq!(ours_renames(&repo), porcelain_renames(&f));
}

/// A repository with `.gitattributes` for binary, `-diff`, a forced text
/// diff, CRLF line ends and a clean filter.
fn attributes_fixture() -> (tempfile::TempDir, Fixture) {
    let (tmp, f) = fixture();
    f.git(&["config", "--local", "filter.upper.clean", "tr a-z A-Z"]);
    f.git(&["config", "--local", "filter.upper.smudge", "cat"]);
    f.write(
        ".gitattributes",
        "*.bin binary\n*.nodiff -diff\nforced.dat diff\n*.crlf text eol=crlf\n*.up filter=upper\n",
    );
    f.write("text.bin", "plain text, but marked binary\n");
    f.write("lines.nodiff", "one\ntwo\n");
    std::fs::write(f.path().join("forced.dat"), b"a\0b\nc\n").unwrap();
    std::fs::write(f.path().join("ends.crlf"), b"one\r\ntwo\r\nthree\r\n").unwrap();
    f.write("shout.up", "hello\nworld\n");
    f.commit_all("attributes");
    (tmp, f)
}

#[test]
fn attributes_shape_diffs_and_status_like_git() {
    let (_tmp, f) = attributes_fixture();
    let repo = open(&f);
    // The CRLF file is stored with LF, the filtered one upper-case, and
    // the work tree is clean for both, as git says.
    assert_eq!(staged(&f, "ends.crlf"), "one\ntwo\nthree\n");
    assert_eq!(staged(&f, "shout.up"), "HELLO\nWORLD\n");
    assert_eq!(ours_renames(&repo), porcelain_renames(&f));
    assert_eq!(
        our_numstat_renames(&repo, &Side::Index, &Side::Worktree),
        numstat_renames(&f, &[])
    );

    f.write("text.bin", "still text, still binary\n");
    f.write("lines.nodiff", "one\nTWO\n");
    std::fs::write(f.path().join("forced.dat"), b"a\0b\nC\nd\n").unwrap();
    std::fs::write(f.path().join("ends.crlf"), b"one\r\nTWO\r\nthree\r\n").unwrap();
    f.write("shout.up", "hello\nthere\nworld\n");
    let repo = open(&f);
    assert_eq!(ours_renames(&repo), porcelain_renames(&f));
    let numstat = numstat_renames(&f, &[]);
    assert!(
        numstat.contains(&("text.bin".into(), String::new(), "-\t-".into()))
            && numstat.contains(&("lines.nodiff".into(), String::new(), "-\t-".into()))
            && numstat.contains(&("forced.dat".into(), String::new(), "2\t1".into())),
        "{numstat:#?}"
    );
    assert_eq!(
        our_numstat_renames(&repo, &Side::Index, &Side::Worktree),
        numstat
    );
    // Hunks show the cleaned work tree: no CRs, the filter's upper case.
    for path in ["ends.crlf", "shout.up"] {
        let d = repo.diff(path, &Side::Index, &Side::Worktree, 3).unwrap();
        assert!(!d.binary);
        assert_eq!(d.to_unified(), git_hunks(&f, &["--", path]), "{path}");
    }
    let d = repo
        .diff("lines.nodiff", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    assert!(d.binary && d.hunks.is_empty());
    // Lines of a binary-by-attribute file can't be staged.
    let err = repo
        .stage_lines(
            "lines.nodiff",
            &LineSelection {
                removed: vec![2],
                added: vec![2],
            },
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unsupported);

    // Staging lines of the CRLF file stores LF lines, as `git add` would.
    let d = repo
        .diff("ends.crlf", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    repo.stage_lines("ends.crlf", &LineSelection::hunk(&d.hunks[0]))
        .unwrap();
    assert_eq!(staged(&f, "ends.crlf"), "one\nTWO\nthree\n");
    assert_eq!(ours_renames(&repo), porcelain_renames(&f));
}
