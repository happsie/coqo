// SPDX-License-Identifier: Apache-2.0

//! Every operation against the `git` CLI's own answer, on fixture
//! repositories built by `git` in temporary folders.

use std::collections::BTreeSet;

use crate::testing::Fixture;
use crate::{Change, ErrorKind, Git, GitOptions, LineKind, LineSelection, LogOptions, Repo, Side};

fn fixture() -> (tempfile::TempDir, Fixture) {
    let tmp = tempfile::tempdir().unwrap();
    let f = Fixture::init(&tmp.path().join("repo"));
    (tmp, f)
}

fn open(f: &Fixture) -> Repo {
    Git::new(GitOptions::default()).open(f.path()).unwrap()
}

#[test]
fn the_mod_provides_the_git_service() {
    use coqo_mod_api::manifest::{ActivationEvent, Tier};
    use coqo_mod_api::testing::{TestHost, block_on};
    use coqo_mod_api::{Mod as _, Service as _};

    let m = crate::manifest();
    assert_eq!(m.id.as_str(), "coqo.git");
    assert_eq!(m.tier, Tier::Core);
    assert!(m.activates_on(&ActivationEvent::Startup));
    let host = TestHost::new();
    let cx = host.context(m.id.clone());
    block_on(crate::GitMod::default().activate(cx.clone())).unwrap();
    assert_eq!(host.services(), vec![(Git::service_ref(), m.id.clone())]);
    assert!(cx.consume::<Git>().is_ok());
}

// ---- status -----------------------------------------------------------------

/// `git status --porcelain=v2 -z -uall --no-renames` as `(XY, path)`, with
/// `?` entries as `("??", path)`.
fn porcelain(f: &Fixture) -> BTreeSet<(String, String)> {
    let out = f.git(&["status", "--porcelain=v2", "-z", "-uall", "--no-renames"]);
    out.split('\0')
        .filter(|l| !l.is_empty())
        .filter_map(|l| match l.as_bytes()[0] {
            b'1' => {
                let parts: Vec<&str> = l.splitn(9, ' ').collect();
                Some((parts[1].to_owned(), parts[8].to_owned()))
            }
            b'u' => {
                let parts: Vec<&str> = l.splitn(11, ' ').collect();
                Some((parts[1].to_owned(), parts[10].to_owned()))
            }
            b'?' => Some(("??".to_owned(), l[2..].to_owned())),
            _ => None,
        })
        .collect()
}

fn ours(repo: &Repo) -> BTreeSet<(String, String)> {
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
            (xy, e.path)
        })
        .collect()
}

#[test]
fn status_matches_porcelain_v2() {
    let (_tmp, f) = fixture();
    f.write("keep.txt", "keep\n");
    f.write("staged.txt", "one\n");
    f.write("both.txt", "one\n");
    f.write("gone.txt", "gone\n");
    f.write("unstaged-gone.txt", "gone\n");
    f.write("src/lib.rs", "fn a() {}\n");
    f.write("run.sh", "echo hi\n");
    f.write("link-me.txt", "file\n");
    f.write(".gitignore", "*.log\nbuild/\n");
    f.commit_all("initial");

    f.write("staged.txt", "two\n");
    f.git(&["add", "staged.txt"]);
    f.write("both.txt", "two\n");
    f.git(&["add", "both.txt"]);
    f.write("both.txt", "three\n");
    f.git(&["rm", "-q", "gone.txt"]);
    std::fs::remove_file(f.path().join("unstaged-gone.txt")).unwrap();
    f.write("src/lib.rs", "fn a() { b() }\n");
    f.write("added.txt", "new\n");
    f.git(&["add", "added.txt"]);
    f.write("untracked.txt", "u\n");
    f.write("deep/er/still.txt", "u\n");
    f.write("intent.txt", "i\n");
    f.git(&["add", "-N", "intent.txt"]);
    f.write("debug.log", "ignored\n");
    f.write("build/out.o", "ignored\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let p = f.path().join("run.sh");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_file(f.path().join("link-me.txt")).unwrap();
        std::os::unix::fs::symlink("keep.txt", f.path().join("link-me.txt")).unwrap();
    }

    let repo = open(&f);
    let expected = porcelain(&f);
    assert!(expected.len() >= 10, "{expected:?}");
    assert_eq!(ours(&repo), expected);

    let head = repo.status().unwrap().head;
    assert_eq!(head.branch.as_deref(), Some("main"));
    assert_eq!(
        head.oid.as_deref(),
        Some(f.git(&["rev-parse", "HEAD"]).trim())
    );
}

#[test]
fn status_reports_conflicts_like_git() {
    let (_tmp, f) = fixture();
    f.write("both.txt", "base\n");
    f.write("theirs-deleted.txt", "base\n");
    f.write("ours-deleted.txt", "base\n");
    f.commit_all("base");
    f.git(&["switch", "-q", "-c", "topic"]);
    f.write("both.txt", "topic\n");
    f.git(&["rm", "-q", "theirs-deleted.txt"]);
    f.write("ours-deleted.txt", "topic\n");
    f.write("added.txt", "topic\n");
    f.commit_all("topic");
    f.git(&["switch", "-q", "main"]);
    f.write("both.txt", "main\n");
    f.write("theirs-deleted.txt", "main\n");
    f.git(&["rm", "-q", "ours-deleted.txt"]);
    f.write("added.txt", "main\n");
    f.commit_all("main");
    assert!(f.try_git(&["merge", "-q", "topic"], None).is_err());

    let repo = open(&f);
    let expected = porcelain(&f);
    let letters: BTreeSet<&str> = expected.iter().map(|(xy, _)| xy.as_str()).collect();
    assert!(
        letters.contains("UU") && letters.contains("AA"),
        "{expected:?}"
    );
    assert_eq!(ours(&repo), expected);
}

#[test]
fn head_upstream_and_ahead_behind_match_git() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "1\n");
    let first = f.commit_all("one");
    f.write("a.txt", "2\n");
    f.commit_all("two");
    f.write("a.txt", "3\n");
    f.commit_all("three");
    f.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/repo.git",
    ]);
    f.git(&["update-ref", "refs/remotes/origin/main", &first]);
    f.git(&["branch", "-q", "--set-upstream-to=origin/main"]);

    let out = f.git(&["status", "--porcelain=v2", "--branch"]);
    let ab = out
        .lines()
        .find_map(|l| l.strip_prefix("# branch.ab "))
        .unwrap();
    let head = open(&f).head().unwrap();
    assert_eq!(head.upstream.as_deref(), Some("origin/main"));
    assert_eq!(format!("+{} -{}", head.ahead, head.behind), ab);
}

#[test]
fn a_folder_inside_the_work_tree_opens_its_repository() {
    let (_tmp, f) = fixture();
    f.write("sub/dir/a.txt", "a\n");
    f.commit_all("one");
    let repo = Git::default().open(&f.path().join("sub/dir")).unwrap();
    assert_eq!(repo.workdir(), f.path());
    assert_eq!(repo.prefix(), "sub/dir");

    let outside = tempfile::tempdir().unwrap();
    let err = Git::default().open(outside.path()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotARepository);
}

// ---- diffs ------------------------------------------------------------------

/// `git diff args…` from the first `@@` on.
fn git_hunks(f: &Fixture, args: &[&str]) -> String {
    let mut all = vec!["diff"];
    all.extend(args);
    let out = f.git(&all);
    match out.find("\n@@") {
        Some(i) => out[i + 1..].to_owned(),
        None if out.starts_with("@@") => out,
        None => String::new(),
    }
}

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn diffs_match_git_diff() {
    let (_tmp, f) = fixture();
    let mut code = String::from("fn first() {\n");
    code.push_str(&numbered(30));
    code.push_str("}\n\nfn second() {\n");
    code.push_str(&numbered(30));
    code.push_str("}\n");
    f.write("code.rs", &code);
    f.write("tail.txt", "a\nb\nc");
    f.write("removed.txt", "bye\n");
    let base = f.commit_all("base");

    // Changes far apart (two hunks, with section headings), close together
    // (merged), at the start and at the end.
    let edited = code
        .replace("line 2\n", "line two\n")
        .replace("line 8\n", "line eight\nline eight and a half\n")
        .replace("line 10\n", "")
        .replace("line 28\n}\n\nfn second", "line 28\n}\n\nfn second_renamed");
    let edited = format!("// header\n{edited}// footer\n");
    f.write("code.rs", &edited);
    f.write("tail.txt", "a\nB\nc\nd");
    let repo = open(&f);
    let ours = repo
        .diff("code.rs", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    assert!(ours.hunks.len() >= 3, "{:#?}", ours.hunks);
    assert_eq!(ours.to_unified(), git_hunks(&f, &["--", "code.rs"]));
    let tail = repo
        .diff("tail.txt", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    assert_eq!(tail.to_unified(), git_hunks(&f, &["--", "tail.txt"]));
    // Other context sizes.
    for ctx in [0u32, 1, 5] {
        let flag = format!("-U{ctx}");
        assert_eq!(
            repo.diff("code.rs", &Side::Index, &Side::Worktree, ctx)
                .unwrap()
                .to_unified(),
            git_hunks(&f, &[&flag, "--", "code.rs"]),
            "context {ctx}"
        );
    }

    // Staged (HEAD → index), and HEAD → work tree.
    f.git(&["add", "code.rs"]);
    f.write("code.rs", &format!("{edited}more\n"));
    let repo = open(&f);
    assert_eq!(
        repo.diff("code.rs", &Side::Rev("HEAD".into()), &Side::Index, 3)
            .unwrap()
            .to_unified(),
        git_hunks(&f, &["--cached", "--", "code.rs"])
    );
    assert_eq!(
        repo.diff("code.rs", &Side::Rev("HEAD".into()), &Side::Worktree, 3)
            .unwrap()
            .to_unified(),
        git_hunks(&f, &["HEAD", "--", "code.rs"])
    );

    // Between revisions; a deleted file and an added one.
    f.git(&["rm", "-q", "removed.txt"]);
    f.write("new.txt", "hello\nworld\n");
    let head = f.commit_all("edit");
    let repo = open(&f);
    for path in ["code.rs", "removed.txt", "new.txt", "tail.txt"] {
        let d = repo
            .diff(path, &Side::Rev(base.clone()), &Side::Rev(head.clone()), 3)
            .unwrap();
        assert_eq!(
            d.to_unified(),
            git_hunks(&f, &[&base, &head, "--", path]),
            "{path}"
        );
    }
    let gone = repo
        .diff(
            "removed.txt",
            &Side::Rev(base.clone()),
            &Side::Rev(head.clone()),
            3,
        )
        .unwrap();
    assert!(gone.new.is_none() && gone.old.is_some());
    assert_eq!(gone.deletions, 1);

    let unknown = repo
        .diff("code.rs", &Side::Rev("no-such-rev".into()), &Side::Index, 3)
        .unwrap_err();
    assert_eq!(unknown.kind, ErrorKind::NotFound);
}

#[test]
fn an_untracked_file_diffs_as_all_added() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "a\n");
    f.commit_all("one");
    f.write("new.txt", "x\ny\n");
    let d = open(&f)
        .diff("new.txt", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    assert!(d.old.is_none());
    assert_eq!(d.additions, 2);
    assert_eq!(d.hunks[0].old_start, 0);
    assert!(d.hunks[0].lines.iter().all(|l| l.kind == LineKind::Added));
}

#[test]
fn a_file_reads_as_each_side_has_it() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "one\n");
    f.commit_all("one");
    f.write("a.txt", "two\n");
    f.git(&["add", "a.txt"]);
    f.write("a.txt", "three\n");
    let repo = open(&f);
    let read = |side: &Side| repo.read_file(side, "a.txt").unwrap();
    assert_eq!(
        read(&Side::Rev("HEAD".into())).as_deref(),
        Some(&b"one\n"[..])
    );
    assert_eq!(read(&Side::Index).as_deref(), Some(&b"two\n"[..]));
    assert_eq!(read(&Side::Worktree).as_deref(), Some(&b"three\n"[..]));
    assert_eq!(repo.read_file(&Side::Index, "missing.txt").unwrap(), None);
    assert!(repo.read_file(&Side::Index, "/etc/passwd").is_err());
}

/// `git diff --numstat args…` as `(path, "add\tdel")`.
fn numstat(f: &Fixture, args: &[&str]) -> Vec<(String, String)> {
    let mut all = vec!["diff", "--numstat"];
    all.extend(args);
    f.git(&all)
        .lines()
        .map(|l| {
            let parts: Vec<&str> = l.splitn(3, '\t').collect();
            (parts[2].to_owned(), format!("{}\t{}", parts[0], parts[1]))
        })
        .collect()
}

fn our_numstat(repo: &Repo, base: &Side, head: &Side) -> Vec<(String, String)> {
    repo.changed_files(base, head)
        .unwrap()
        .into_iter()
        .map(|c| {
            let stat = if c.binary {
                "-\t-".to_owned()
            } else {
                format!("{}\t{}", c.additions, c.deletions)
            };
            (c.path, stat)
        })
        .collect()
}

#[test]
fn changed_files_match_numstat() {
    let (_tmp, f) = fixture();
    f.write("a.txt", &numbered(20));
    f.write("b.txt", "b\n");
    f.write("dir/c.txt", "c\n");
    std::fs::write(f.path().join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    let base = f.commit_all("base");
    f.write("a.txt", &numbered(20).replace("line 3\n", "three\n"));
    f.git(&["rm", "-q", "b.txt"]);
    f.write("dir/d.txt", "d\n");
    std::fs::write(f.path().join("bin.dat"), [0u8, 9, 9]).unwrap();
    let head = f.commit_all("head");
    f.write("dir/c.txt", "c changed\nc2\n");
    f.git(&["add", "dir/c.txt"]);
    f.write("a.txt", "rewritten\n");
    f.write("untracked.txt", "not in git diff\n");

    let repo = open(&f);
    let (b, h) = (Side::Rev(base.clone()), Side::Rev(head.clone()));
    assert_eq!(our_numstat(&repo, &b, &h), numstat(&f, &[&base, &head]));
    assert_eq!(
        our_numstat(&repo, &Side::Index, &Side::Worktree),
        numstat(&f, &[])
    );
    assert_eq!(
        our_numstat(&repo, &Side::Rev("HEAD".into()), &Side::Index),
        numstat(&f, &["--cached"])
    );
    assert_eq!(
        our_numstat(&repo, &b, &Side::Worktree),
        numstat(&f, &[&base])
    );
}

// ---- staging ----------------------------------------------------------------

/// The index's copy of `path`.
fn staged(f: &Fixture, path: &str) -> String {
    f.git(&["show", &format!(":{path}")])
}

#[test]
fn staging_hunks_and_lines_round_trips() {
    let (_tmp, f) = fixture();
    f.write("f.txt", &numbered(40));
    f.commit_all("base");
    let edited = numbered(40)
        .replace("line 2\n", "line 2\nadded after 2\n")
        .replace("line 20\n", "line twenty\n")
        .replace("line 38\n", "line 38\nA\nB\n");
    f.write("f.txt", &edited);
    let repo = open(&f);

    // Stage the middle hunk whole.
    let diff = repo
        .diff("f.txt", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    assert_eq!(diff.hunks.len(), 3);
    repo.stage_lines("f.txt", &LineSelection::hunk(&diff.hunks[1]))
        .unwrap();
    assert_eq!(
        staged(&f, "f.txt"),
        numbered(40).replace("line 20\n", "line twenty\n")
    );
    let cached = git_hunks(&f, &["--cached"]);
    assert_eq!(cached.matches("@@ -").count(), 1, "{cached}");
    assert_eq!(git_hunks(&f, &[]).matches("@@ -").count(), 2);

    // Stage one added line of the last hunk ("B", not "A").
    let diff = repo
        .diff("f.txt", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    let last = diff.hunks.last().unwrap();
    let b = last
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Added && l.text == "B")
        .unwrap();
    repo.stage_lines(
        "f.txt",
        &LineSelection {
            removed: vec![],
            added: vec![b.new_line.unwrap()],
        },
    )
    .unwrap();
    assert_eq!(
        staged(&f, "f.txt"),
        numbered(40)
            .replace("line 20\n", "line twenty\n")
            .replace("line 38\n", "line 38\nB\n")
    );

    // A stale selection (a line number that is not an added line now).
    let err = repo
        .stage_lines(
            "f.txt",
            &LineSelection {
                removed: vec![],
                added: vec![1],
            },
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StaleSelection);

    // Unstage the "twenty" change: its removed and added line.
    let staged_diff = repo
        .diff("f.txt", &Side::Rev("HEAD".into()), &Side::Index, 3)
        .unwrap();
    let twenty = staged_diff
        .hunks
        .iter()
        .find(|h| h.lines.iter().any(|l| l.text == "line twenty"))
        .unwrap();
    repo.unstage_lines("f.txt", &LineSelection::hunk(twenty))
        .unwrap();
    assert_eq!(
        staged(&f, "f.txt"),
        numbered(40).replace("line 38\n", "line 38\nB\n")
    );

    // Stage everything hunk by hunk: nothing unstaged remains, and the
    // index has the work tree's file.
    loop {
        let d = repo
            .diff("f.txt", &Side::Index, &Side::Worktree, 3)
            .unwrap();
        let Some(h) = d.hunks.first() else { break };
        repo.stage_lines("f.txt", &LineSelection::hunk(h)).unwrap();
    }
    assert_eq!(staged(&f, "f.txt"), edited);
    assert_eq!(git_hunks(&f, &[]), "");
    // And unstage it all again, hunk by hunk.
    loop {
        let d = repo
            .diff("f.txt", &Side::Rev("HEAD".into()), &Side::Index, 3)
            .unwrap();
        let Some(h) = d.hunks.first() else { break };
        repo.unstage_lines("f.txt", &LineSelection::hunk(h))
            .unwrap();
    }
    assert_eq!(git_hunks(&f, &["--cached"]), "");
    assert_eq!(f.read("f.txt"), edited, "the work tree is never touched");
}

#[test]
fn staging_lines_of_new_and_unterminated_files() {
    let (_tmp, f) = fixture();
    f.write("end.txt", "a\nb");
    f.commit_all("base");
    let repo = open(&f);

    // Part of an untracked file: the index gets a file with those lines.
    f.write("new.txt", "one\ntwo\nthree\n");
    repo.stage_lines(
        "new.txt",
        &LineSelection {
            removed: vec![],
            added: vec![1, 3],
        },
    )
    .unwrap();
    assert_eq!(staged(&f, "new.txt"), "one\nthree\n");
    // Part of a file new in the index, unstaged again.
    repo.unstage_lines(
        "new.txt",
        &LineSelection {
            removed: vec![],
            added: vec![1],
        },
    )
    .unwrap();
    assert_eq!(staged(&f, "new.txt"), "three\n");

    // A last line without a line break.
    f.write("end.txt", "a\nb\nc");
    let d = repo
        .diff("end.txt", &Side::Index, &Side::Worktree, 3)
        .unwrap();
    repo.stage_lines("end.txt", &LineSelection::hunk(&d.hunks[0]))
        .unwrap();
    assert_eq!(staged(&f, "end.txt"), "a\nb\nc");
}

#[test]
fn whole_files_stage_and_unstage() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "a\n");
    f.write("b.txt", "b\n");
    f.commit_all("base");
    f.write("a.txt", "a2\n");
    std::fs::remove_file(f.path().join("b.txt")).unwrap();
    f.write("c.txt", "c\n");
    let repo = open(&f);
    repo.stage(&["a.txt".into(), "b.txt".into(), "c.txt".into()])
        .unwrap();
    assert_eq!(
        porcelain(&f),
        [("M.", "a.txt"), ("D.", "b.txt"), ("A.", "c.txt")]
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect()
    );
    assert_eq!(ours(&repo), porcelain(&f));
    repo.unstage(&["a.txt".into(), "b.txt".into(), "c.txt".into()])
        .unwrap();
    assert_eq!(
        porcelain(&f),
        [(".M", "a.txt"), (".D", "b.txt"), ("??", "c.txt")]
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect()
    );
    assert!(repo.stage(&["../outside".into()]).is_err());
}

#[test]
fn unstage_on_an_unborn_branch() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "a\n");
    let repo = open(&f);
    repo.stage(&["a.txt".into()]).unwrap();
    assert_eq!(ours(&repo), porcelain(&f));
    repo.unstage(&["a.txt".into()]).unwrap();
    assert_eq!(
        porcelain(&f),
        [("??".to_owned(), "a.txt".to_owned())]
            .into_iter()
            .collect()
    );
    assert_eq!(repo.status().unwrap().head.oid, None);
    assert!(repo.log(&LogOptions::default()).unwrap().commits.is_empty());
}

// ---- commit, branches, stash ------------------------------------------------

#[test]
fn commit_branches_and_switching() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "a\n");
    f.commit_all("base");
    let repo = open(&f);
    f.write("a.txt", "a2\n");
    repo.stage(&["a.txt".into()]).unwrap();
    let commit = repo.commit("Change a\n\nWith a body.\n", false).unwrap();
    assert_eq!(
        f.git(&["log", "-1", "--format=%H %s"]).trim(),
        format!("{} {}", commit.oid, commit.summary)
    );
    assert_eq!(commit.summary, "Change a");
    assert_eq!(
        repo.commit("  ", false).unwrap_err().kind,
        ErrorKind::Invalid
    );
    // Nothing staged: git refuses, and says so.
    let err = repo.commit("Nothing", false).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Command);
    // Amend.
    let amended = repo.commit("Change a, amended", true).unwrap();
    assert_ne!(amended.oid, commit.oid);
    assert_eq!(
        f.git(&["log", "-1", "--format=%s"]).trim(),
        "Change a, amended"
    );

    repo.create_branch("feature", None, false).unwrap();
    repo.create_branch("topic", Some("HEAD~1"), true).unwrap();
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "topic");
    f.git(&["remote", "add", "origin", "https://example.invalid/r.git"]);
    f.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    f.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    let expected: Vec<String> = f
        .git(&[
            "for-each-ref",
            "--format=%(refname:short) %(objectname) %(HEAD)",
            "refs/heads",
            "refs/remotes",
        ])
        .lines()
        .filter(|l| !l.starts_with("origin ") && !l.starts_with("origin/HEAD "))
        .map(str::to_owned)
        .collect();
    let ours: Vec<String> = repo
        .branches()
        .unwrap()
        .into_iter()
        .map(|b| format!("{} {} {}", b.name, b.oid, if b.current { "*" } else { " " }))
        .collect();
    assert_eq!(ours, expected);

    repo.switch_branch("main").unwrap();
    assert_eq!(f.git(&["branch", "--show-current"]).trim(), "main");
    assert_eq!(
        repo.create_branch("-x", None, false).unwrap_err().kind,
        ErrorKind::Invalid
    );
    assert!(repo.switch_branch("no-such-branch").is_err());
}

#[test]
fn stash_push_list_pop() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "a\n");
    f.commit_all("base");
    let repo = open(&f);
    assert!(!repo.stash_push(None, false).unwrap(), "nothing to stash");
    f.write("a.txt", "first\n");
    assert!(repo.stash_push(Some("first change"), false).unwrap());
    f.write("a.txt", "second\n");
    f.write("u.txt", "untracked\n");
    assert!(repo.stash_push(None, true).unwrap());
    assert_eq!(f.read("a.txt"), "a\n");
    assert!(!f.path().join("u.txt").exists());

    let expected: Vec<String> = f
        .git(&["stash", "list", "--format=%gd %H %gs"])
        .lines()
        .map(str::to_owned)
        .collect();
    let ours: Vec<String> = repo
        .stashes()
        .unwrap()
        .into_iter()
        .map(|s| format!("stash@{{{}}} {} {}", s.index, s.oid, s.message))
        .collect();
    assert_eq!(ours, expected);
    assert_eq!(ours.len(), 2);

    repo.stash_apply(1).unwrap();
    assert_eq!(f.read("a.txt"), "first\n");
    f.git(&["checkout", "-q", "--", "a.txt"]);
    repo.stash_drop(1).unwrap();
    repo.stash_pop(0).unwrap();
    assert_eq!(f.read("a.txt"), "second\n");
    assert_eq!(f.read("u.txt"), "untracked\n");
    assert!(repo.stashes().unwrap().is_empty());
    assert_eq!(repo.stash_pop(0).unwrap_err().kind, ErrorKind::NotFound);
}

// ---- blame ------------------------------------------------------------------

#[test]
fn blame_matches_git_blame_porcelain() {
    let (_tmp, f) = fixture();
    f.write("old-name.txt", &numbered(10));
    f.commit_all("one");
    f.write("old-name.txt", &numbered(10).replace("line 3\n", "three\n"));
    f.commit_all("two");
    f.git(&["mv", "old-name.txt", "f.txt"]);
    f.commit_all("rename");
    f.write(
        "f.txt",
        &numbered(10)
            .replace("line 3\n", "three\n")
            .replace("line 7\n", "seven\nseven and a half\n"),
    );
    f.commit_all("three");
    f.write("f.txt", "uncommitted\n");

    // `<sha> <orig line> <final line>` for every line.
    let out = f.git(&["blame", "--porcelain", "HEAD", "--", "f.txt"]);
    let expected: Vec<(String, u32, u32)> = out
        .lines()
        .filter_map(|l| {
            let parts: Vec<&str> = l.split(' ').collect();
            (parts.len() >= 3
                && parts[0].len() == 40
                && parts[0].bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| {
                (
                    parts[0].to_owned(),
                    parts[1].parse().unwrap(),
                    parts[2].parse().unwrap(),
                )
            })
        })
        .collect();
    let blame = open(&f).blame("f.txt", None).unwrap();
    let ours: Vec<(String, u32, u32)> = blame
        .hunks
        .iter()
        .flat_map(|h| (0..h.lines).map(move |i| (h.oid.clone(), h.orig_start + i, h.start + i)))
        .collect();
    assert_eq!(ours, expected);
    assert_eq!(blame.line_count, 11);
    let first = &blame.commits[&blame.hunks[0].oid];
    assert_eq!(first.author, "Fixture Author");
    assert_eq!(first.summary, "one");
    assert!(
        blame
            .hunks
            .iter()
            .any(|h| h.orig_path.as_deref() == Some("old-name.txt"))
    );
    assert_eq!(
        open(&f).blame("missing.txt", None).unwrap_err().kind,
        ErrorKind::NotFound
    );
}

// ---- log --------------------------------------------------------------------

#[test]
fn log_matches_git_log() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "1\n");
    f.commit_all("one");
    f.write("a.txt", "2\n");
    f.commit_all("two");
    f.git(&["switch", "-q", "-c", "feature"]);
    f.write("b.txt", "b\n");
    f.commit_all("feature work");
    f.write("b.txt", "b2\n");
    f.commit_all("more feature work");
    f.git(&["switch", "-q", "main"]);
    f.write("a.txt", "3\n");
    f.commit_all("three");
    f.git(&["merge", "-q", "--no-ff", "-m", "Merge feature", "feature"]);
    f.git(&["switch", "-q", "-c", "side", "HEAD~2"]);
    f.write("c.txt", "c\n");
    f.commit_all("side");
    f.git(&["switch", "-q", "main"]);
    f.git(&["tag", "v1"]);

    let repo = open(&f);
    let format = "--format=%H %P|%an|%s";
    let line = |c: &crate::LogCommit| {
        format!(
            "{} {}|{}|{}",
            c.oid,
            c.parents.join(" "),
            c.author,
            c.summary
        )
    };
    let git_lines =
        |args: &[&str]| -> Vec<String> { f.git(args).lines().map(str::to_owned).collect() };

    let all = repo
        .log(&LogOptions {
            revs: vec!["main".into(), "side".into()],
            skip: 0,
            limit: 100,
        })
        .unwrap();
    assert!(!all.more);
    assert_eq!(
        all.commits.iter().map(line).collect::<Vec<_>>(),
        git_lines(&["log", format, "main", "side"])
    );
    let page = repo
        .log(&LogOptions {
            revs: vec![],
            skip: 2,
            limit: 3,
        })
        .unwrap();
    assert!(page.more);
    assert_eq!(
        page.commits.iter().map(line).collect::<Vec<_>>(),
        git_lines(&["log", format, "--skip=2", "-n", "3"])
    );
    // Rows are the same whatever the page.
    assert_eq!(
        page.commits[0].graph,
        repo.log(&LogOptions::default()).unwrap().commits[2].graph
    );
    let main = f.git(&["rev-parse", "main"]);
    let tip = all.commits.iter().find(|c| c.oid == main.trim()).unwrap();
    assert!(tip.refs.contains(&"HEAD".to_owned()));
    assert!(tip.refs.contains(&"main".to_owned()));
    assert!(tip.refs.contains(&"v1".to_owned()));
    // The merge forks into two lanes.
    let merge = all
        .commits
        .iter()
        .find(|c| c.summary == "Merge feature")
        .unwrap();
    let outs = merge
        .graph
        .edges
        .iter()
        .filter(|e| e.kind == crate::EdgeKind::Out)
        .count();
    assert_eq!(outs, 2);
}

mod discard;
mod renames;
