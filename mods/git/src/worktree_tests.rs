// SPDX-License-Identifier: Apache-2.0

//! Worktrees against the `git` CLI's own listing, on fixture repositories.

use std::path::PathBuf;

use crate::testing::Fixture;
use crate::{ErrorKind, Git, GitOptions, Removal, Repo, WorktreeWork};

fn fixture() -> (tempfile::TempDir, Fixture) {
    let tmp = tempfile::tempdir().unwrap();
    let f = Fixture::init(&tmp.path().join("repo"));
    (tmp, f)
}

fn open(dir: &std::path::Path) -> Repo {
    Git::new(GitOptions::default()).open(dir).unwrap()
}

/// `git worktree list --porcelain` as `(path, branch)`, the main one first.
fn cli_worktrees(f: &Fixture) -> Vec<(PathBuf, Option<String>)> {
    let mut out = Vec::new();
    for record in f.git(&["worktree", "list", "--porcelain"]).split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in record.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p).canonicalize().unwrap());
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_owned());
            }
        }
        out.extend(path.map(|p| (p, branch)));
    }
    out
}

#[test]
fn worktrees_made_outside_coqo_are_listed_as_git_lists_them() {
    let (tmp, f) = fixture();
    f.write("a.txt", "one\n");
    f.commit_all("first");
    let outside = tmp.path().join("by-hand");
    f.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "by-hand",
        outside.to_str().unwrap(),
    ]);
    let detached = tmp.path().join("detached");
    f.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        detached.to_str().unwrap(),
    ]);
    let repo = open(f.path());
    let list = repo.worktrees().unwrap();
    let ours: Vec<_> = list
        .iter()
        .map(|w| (w.path.clone(), w.branch.clone()))
        .collect();
    assert_eq!(ours, cli_worktrees(&f));
    assert!(list[0].main && list[0].path == f.path());
    assert!(list[1..].iter().all(|w| !w.main));
    assert_eq!(list[2].branch, None, "detached");
    // Opened from a linked worktree, the listing is the same.
    let linked = open(&outside);
    assert_eq!(linked.worktrees().unwrap(), list);
    assert_eq!(linked.main_worktree_dir().as_deref(), Some(f.path()));
}

#[test]
fn adds_worktrees_beside_the_repository() {
    let (tmp, f) = fixture();
    f.write("a.txt", "one\n");
    let first = f.commit_all("first");
    f.git(&["switch", "-q", "-c", "develop"]);
    f.write("a.txt", "two\n");
    let second = f.commit_all("second");
    f.git(&["switch", "-q", "main"]);
    let repo = open(f.path());

    // A new branch from a base, in `<repo>-worktrees/<branch>`.
    let wt = repo
        .add_worktree("feature/login", Some("develop"), None)
        .unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("repo-worktrees");
    assert_eq!(wt.path, dir.join("feature-login"));
    assert_eq!(wt.branch.as_deref(), Some("feature/login"));
    assert_eq!(wt.head.as_deref(), Some(second.as_str()));
    assert_eq!(
        std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
        "two\n"
    );
    // The base defaults to HEAD.
    let wt = repo.add_worktree("fix", None, None).unwrap();
    assert_eq!(wt.head.as_deref(), Some(first.as_str()));
    // An existing branch is checked out as it is.
    f.git(&["branch", "old", &first]);
    let wt = repo.add_worktree("old", Some("develop"), None).unwrap();
    assert_eq!(wt.head.as_deref(), Some(first.as_str()));
    // A taken folder gets a number.
    std::fs::create_dir_all(dir.join("next")).unwrap();
    assert_eq!(
        repo.default_worktree_path("next").unwrap(),
        dir.join("next-2")
    );
    // git's refusals come through: a branch checked out elsewhere.
    let err = repo.add_worktree("main", None, None).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Command);
    assert!(repo.add_worktree("-b", None, None).is_err());
    assert_eq!(repo.worktrees().unwrap().len(), 4);
}

#[test]
fn removal_never_forces_and_never_takes_the_main_worktree() {
    let (_tmp, f) = fixture();
    f.write("a.txt", "one\n");
    f.commit_all("first");
    let repo = open(f.path());

    // Never the main worktree, from wherever it is asked.
    let err = repo.remove_worktree(f.path(), true).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Invalid);
    let wt = repo.add_worktree("feature", None, None).unwrap();
    let from_linked = open(&wt.path);
    assert_eq!(
        from_linked
            .remove_worktree(f.path(), true)
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
    assert!(f.path().join(".git").is_dir());

    // Uncommitted changes stop it, confirmed or not.
    std::fs::write(wt.path.join("a.txt"), "edited\n").unwrap();
    std::fs::write(wt.path.join("new.txt"), "new\n").unwrap();
    let dirty = WorktreeWork {
        changes: 2,
        unpushed: 0,
    };
    assert_eq!(
        repo.remove_worktree(&wt.path, false).unwrap(),
        Removal::HasWork(dirty)
    );
    assert_eq!(
        repo.remove_worktree(&wt.path, true).unwrap(),
        Removal::HasWork(dirty)
    );
    assert!(wt.path.join("new.txt").exists());

    // Commits only its branch has stop it until confirmed.
    for args in [&["add", "-A"][..], &["commit", "-q", "-m", "work"]] {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&wt.path)
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let unpushed = WorktreeWork {
        changes: 0,
        unpushed: 1,
    };
    assert_eq!(
        repo.remove_worktree(&wt.path, false).unwrap(),
        Removal::HasWork(unpushed)
    );
    assert!(wt.path.exists());
    assert_eq!(
        repo.remove_worktree(&wt.path, true).unwrap(),
        Removal::Removed
    );
    assert!(!wt.path.exists());
    // The branch, and so the commit, stays.
    assert!(f.git(&["branch", "--list", "feature"]).contains("feature"));

    // A clean worktree on a merged branch goes at once.
    let clean = repo.add_worktree("clean", None, None).unwrap();
    assert!(repo.worktree_work(&clean.path).unwrap().is_clean());
    assert_eq!(
        repo.remove_worktree(&clean.path, false).unwrap(),
        Removal::Removed
    );

    // One whose folder is gone is pruned; an unknown path is not found.
    let gone = repo.add_worktree("gone", None, None).unwrap();
    std::fs::remove_dir_all(&gone.path).unwrap();
    assert!(repo.worktrees().unwrap().iter().any(|w| w.prunable));
    assert_eq!(
        repo.remove_worktree(&gone.path, false).unwrap(),
        Removal::Removed
    );
    assert_eq!(repo.worktrees().unwrap().len(), 1);
    assert_eq!(
        repo.remove_worktree(&gone.path, false).unwrap_err().kind,
        ErrorKind::NotFound
    );
}
