// SPDX-License-Identifier: Apache-2.0

//! The Git pane and diff tabs against a real `coqod` (with the built-in git
//! mod) served in-process, on a fixture repository.

use std::{
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_mod_git::testing::Fixture as Repo;
use coqo_protocol::methods::{
    git::{
        Branch, ChangedFile, DiffLine, FileChange, FileDiff, FileSide, Head, Hunk, LineKind,
        RepoInfo, Stash, StatusEntry, StatusResult,
    },
    workspace::{self as wsm, RootId, WorkspaceId, WorkspaceInfo, WorkspaceRoot},
};
use coqo_ui::{
    Appearance,
    dock::{
        AnyTab,
        store::{LayoutStore as _, MemoryLayoutStore, SidebarLayout},
    },
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Window, WindowBounds,
        WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::{
    chrome::{ChromePart, GitChrome, NO_REPOSITORY},
    diff::{self, DiffTab},
    model::{DiscardNotice, GitModel, RootGit},
    pane::{GitPane, Group},
};
use crate::{
    daemon::{self, DaemonConfig},
    diff::{DiffView, Status, source::StaticSource, viewed},
    files::history::History,
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{TIMEOUT, TestDaemon},
    workspace::{self, Workspaces},
};

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    daemon: TestDaemon,
    _tmp: tempfile::TempDir,
}

fn open_ui(folder: PathBuf, tmp: tempfile::TempDir) -> Ui {
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let paths = daemon.paths.clone();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(Some(folder), cx);
        crate::shell::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        coqo_ui::open_window(options, cx, |window, cx| {
            let store = Rc::new(MemoryLayoutStore::default());
            cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
        })
        .expect("open shell window")
    });
    Ui {
        cx,
        window,
        shell,
        daemon,
        _tmp: tmp,
    }
}

impl Ui {
    fn frame<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx
            .update_window(self.window, |_, window, cx| {
                window.render_frame(cx);
                f(window, cx)
            })
            .unwrap()
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&mut Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.cx.run_until_parked();
            self.frame(|_, _| ());
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn pane(&mut self) -> Entity<GitPane> {
        self.frame(|_, _| ());
        self.cx
            .read(|cx| self.shell.read(cx).sidebar_pane("git"))
            .and_then(|view| view.downcast::<GitPane>().ok())
            .expect("the git pane is shown")
    }

    fn rows(&mut self, group: Group) -> Vec<(String, Option<FileChange>)> {
        let pane = self.pane();
        self.cx.read(|cx| {
            pane.read(cx)
                .rows(group, cx)
                .into_iter()
                .map(|e| {
                    let change = if group == Group::Staged {
                        e.staged
                    } else {
                        e.unstaged
                    };
                    (e.path, change)
                })
                .collect()
        })
    }

    fn click(&mut self, id: impl Into<coqo_ui::gpui::ElementId>) {
        let id = id.into();
        self.frame(|window, cx| window.click(id, cx));
    }

    fn diff_tabs(&mut self) -> Vec<Entity<DiffTab>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == diff::KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<DiffTab>())
                .collect()
        })
    }
}

fn fixture_repo() -> (tempfile::TempDir, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    repo.write("README.md", "# Demo\n");
    repo.write("src/lib.rs", "pub fn f() {}\n");
    repo.commit_all("Initial commit");
    (tmp, repo)
}

#[test]
fn stage_a_file_and_commit_it_from_the_git_pane() {
    let (tmp, repo) = fixture_repo();
    repo.write("README.md", "# Demo\n\nMore words.\n");
    repo.write("notes.txt", "todo\n");
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.click("sidebar-git");
    ui.wait_for("the changes", |ui| ui.rows(Group::Unstaged).len() == 2);
    assert_eq!(
        ui.rows(Group::Unstaged),
        [
            ("README.md".to_owned(), Some(FileChange::Modified)),
            ("notes.txt".to_owned(), Some(FileChange::Untracked)),
        ]
    );
    assert!(ui.rows(Group::Staged).is_empty());

    // A file's diff opens in a tab.
    ui.click(("git-unstaged-row", 0usize));
    ui.wait_for("the diff", |ui| {
        let tabs = ui.diff_tabs();
        tabs.len() == 1
            && ui.cx.read(|cx| {
                tabs[0]
                    .read(cx)
                    .view()
                    .read(cx)
                    .file_diff("README.md")
                    .is_some()
            })
    });
    let tab = ui.diff_tabs().remove(0);
    let view = ui.cx.read(|cx| tab.read(cx).view().clone());
    ui.cx.read(|cx| {
        let v = view.read(cx);
        assert_eq!(v.selected_file(), Some("README.md"));
        // Untracked files are listed with the changes, as in the pane.
        let files: Vec<_> = v.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(files, ["README.md", "notes.txt"]);
        let d = v.file_diff("README.md").unwrap();
        assert_eq!((d.additions, d.deletions), (2, 0));
    });

    // Stage README.md with its row's button.
    ui.click(("git-stage", 0usize));
    ui.wait_for("README.md staged", |ui| ui.rows(Group::Staged).len() == 1);
    assert_eq!(
        ui.rows(Group::Staged),
        [("README.md".to_owned(), Some(FileChange::Modified))]
    );
    assert_eq!(
        repo.git(&["diff", "--cached", "--name-only"]),
        "README.md\n"
    );
    // The unstaged diff tab follows: nothing left unstaged in README.md.
    ui.wait_for("the diff to refresh", |ui| {
        ui.cx.read(|cx| {
            let v = view.read(cx);
            v.files().len() == 1 && v.selected_file() == Some("notes.txt")
        })
    });

    // Write a message and commit.
    let pane = ui.pane();
    let input = ui.cx.read(|cx| pane.read(cx).message_input().clone());
    ui.frame(|window, cx| {
        input.update(cx, |i, cx| i.set_text("Expand the README", window, cx));
    });
    ui.click("git-commit");
    ui.wait_for("the commit", |ui| ui.rows(Group::Staged).is_empty());
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s"]).trim(),
        "Expand the README"
    );
    assert_eq!(ui.cx.read(|cx| input.read(cx).text(cx)), "");
    assert_eq!(
        ui.rows(Group::Unstaged),
        [("notes.txt".to_owned(), Some(FileChange::Untracked))]
    );

    // A change made outside Coqo shows up by itself (git/changes).
    repo.git(&["add", "notes.txt"]);
    ui.wait_for("notes.txt staged elsewhere", |ui| {
        ui.rows(Group::Staged).len() == 1
    });
}

#[test]
fn branches_and_stashes_from_the_git_pane() {
    let (tmp, repo) = fixture_repo();
    repo.git(&["branch", "topic"]);
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.click("sidebar-git");
    let pane = ui.pane();
    let model = ui.cx.read(|cx| pane.read(cx).model().clone());
    // The branch button shows once the status is in too; the branches and
    // the status arrive separately.
    ui.wait_for("the branches and the status", |ui| {
        ui.cx.read(|cx| {
            model
                .read(cx)
                .roots()
                .first()
                .is_some_and(|r| r.branches.len() == 2 && r.status.is_some())
        })
    });

    // Switch with the picker.
    ui.click("git-branch");
    ui.click(("git-branch-item", 1usize));
    ui.wait_for("the switch", |ui| {
        ui.cx.read(|cx| {
            model.read(cx).roots()[0]
                .status
                .as_ref()
                .and_then(|s| s.head.branch.clone())
                .as_deref()
                == Some("topic")
        })
    });
    assert_eq!(repo.git(&["branch", "--show-current"]).trim(), "topic");

    // Stash a change, then pop it.
    repo.write("README.md", "changed\n");
    ui.wait_for("the change", |ui| ui.rows(Group::Unstaged).len() == 1);
    ui.click("git-stash-push");
    // The stash list and the status refresh arrive separately.
    ui.wait_for("the stash and the clean tree", |ui| {
        ui.cx
            .read(|cx| model.read(cx).roots()[0].stashes.len() == 1)
            && ui.rows(Group::Unstaged).is_empty()
    });
    ui.click(("git-stash-pop", 0usize));
    ui.wait_for("the pop", |ui| ui.rows(Group::Unstaged).len() == 1);
    assert_eq!(repo.read("README.md"), "changed\n");
}

#[test]
fn a_folder_that_is_not_a_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("plain");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a.txt"), "a\n").unwrap();
    let mut ui = open_ui(folder, tmp);
    ui.click("sidebar-git");
    let pane = ui.pane();
    let model = ui.cx.read(|cx| pane.read(cx).model().clone());
    ui.wait_for("the verdict", |ui| {
        ui.cx.read(|cx| {
            model
                .read(cx)
                .roots()
                .first()
                .and_then(|r| r.unavailable.clone())
                .is_some()
        })
    });
    assert_eq!(
        ui.cx
            .read(|cx| model.read(cx).roots()[0].unavailable.clone())
            .as_deref(),
        Some("Not a git repository")
    );
    // The status bar says so, and the Git tab has no badge.
    assert_eq!(ui.branch_text(), NO_REPOSITORY);
    assert_eq!(ui.badge(), None);
    assert!(ui.frame(|window, _| window.try_find("sidebar-git-badge").is_none()));
}

impl Ui {
    fn chrome(&mut self, part: ChromePart) -> Entity<GitChrome> {
        self.frame(|window, cx| GitChrome::for_window(part, window, cx))
    }

    /// The number on the sidebar's Git tab.
    fn badge(&mut self) -> Option<usize> {
        let view = self.chrome(ChromePart::Badge);
        self.cx.read(|cx| view.read(cx).changed_files(cx))
    }

    /// The status bar's branch.
    fn branch_text(&mut self) -> String {
        let view = self.chrome(ChromePart::Branch);
        self.cx.read(|cx| view.read(cx).branch_text(cx))
    }
}

#[test]
fn the_git_tab_counts_changes_and_the_status_bar_shows_the_branch() {
    let (tmp, repo) = fixture_repo();
    repo.git(&["branch", "topic"]);
    // The Files pane is shown: the badge and the branch don't need the Git
    // pane.
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.wait_for("the branch", |ui| ui.branch_text() == "main");
    assert_eq!(ui.badge(), None, "a clean tree has no badge");
    assert!(ui.frame(|window, _| window.find("status-branch").visible()));

    // Edits outside Coqo raise the count, live.
    repo.write("README.md", "# Demo\n\nEdited.\n");
    ui.wait_for("one change", |ui| ui.badge() == Some(1));
    assert!(ui.frame(|window, _| window.find("sidebar-git-badge").visible()));
    repo.write("notes.txt", "new\n");
    ui.wait_for("two changes", |ui| ui.badge() == Some(2));
    // Staged files count too, once each.
    repo.git(&["add", "README.md"]);
    repo.write("README.md", "# Demo\n\nEdited twice.\n");
    let model = {
        let view = ui.chrome(ChromePart::Badge);
        ui.cx.read(|cx| view.read(cx).model().clone())
    };
    ui.wait_for("README.md staged and edited", |ui| {
        ui.cx.read(|cx| {
            model.read(cx).roots()[0].status.as_ref().is_some_and(|s| {
                s.entries
                    .iter()
                    .any(|e| e.staged.is_some() && e.unstaged.is_some())
            })
        })
    });
    assert_eq!(ui.badge(), Some(2));

    // A switch outside Coqo moves the status bar.
    repo.git(&["stash", "-q", "-u"]);
    ui.wait_for("a clean tree again", |ui| ui.badge().is_none());
    repo.git(&["switch", "-q", "topic"]);
    ui.wait_for("the switch", |ui| ui.branch_text() == "topic");
    // And one from the Git pane does too, even while the branch list (it
    // loads apart from the status) still says `main` is current.
    ui.click("sidebar-git");
    ui.click("git-branch");
    ui.click(("git-branch-item", 0usize));
    ui.wait_for("the switch back", |ui| ui.branch_text() == "main");
}

#[test]
fn the_picker_takes_the_current_branch_from_the_status() {
    let branch = |name: &str, current| Branch {
        name: name.into(),
        remote: false,
        current,
        oid: "0".repeat(40),
        upstream: None,
        summary: String::new(),
        time: 0,
    };
    // The branch list from before a switch to `issue-91-git-mod`, which
    // the status already shows.
    let mut root = RootGit::new(RootId(0));
    root.branches = vec![branch("issue-91-git-mod", false), branch("main", true)];
    root.status = Some(sample_status());
    assert!(root.is_current(&root.branches[0]));
    assert!(!root.is_current(&root.branches[1]));
    // A remote branch of the same name is not where HEAD is.
    let mut remote = branch("issue-91-git-mod", false);
    remote.remote = true;
    assert!(!root.is_current(&remote));
    // Detached: no branch is current.
    root.status.as_mut().unwrap().head.branch = None;
    assert!(!root.is_current(&root.branches[1]));
    // Until the status is in, the list decides.
    root.status = None;
    assert!(root.is_current(&root.branches[1]));
}

#[test]
fn a_root_added_later_is_watched() {
    let (tmp, repo) = fixture_repo();
    let other = Repo::init(&tmp.path().join("other-repo"));
    other.write("lib.txt", "one\n");
    other.commit_all("Initial commit");
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.click("sidebar-git");
    let pane = ui.pane();
    let model = ui.cx.read(|cx| pane.read(cx).model().clone());
    ui.wait_for("the first root", |ui| {
        ui.cx.read(|cx| {
            model
                .read(cx)
                .roots()
                .first()
                .is_some_and(|r| r.status.is_some())
        })
    });

    let workspace = ui
        .cx
        .read(|cx| model.read(cx).workspace().map(|w| w.id.clone()))
        .expect("a workspace");
    let path = other.path().to_str().unwrap().to_owned();
    let info = ui.daemon.with_client(async move |client| {
        client
            .request::<wsm::AddRoot>(wsm::AddRootParams {
                id: workspace,
                path,
                name: None,
            })
            .await
            .unwrap()
    });
    let added = info
        .roots
        .iter()
        .find(|r| r.name == "other-repo")
        .unwrap()
        .id;
    let status = |ui: &mut Ui| {
        ui.cx
            .read(|cx| model.read(cx).root(added).and_then(|r| r.status.clone()))
    };
    ui.wait_for("the added root's status", |ui| status(ui).is_some());

    // A change made outside Coqo in the new root shows up by itself.
    other.write("lib.txt", "two\n");
    ui.wait_for("the edit", |ui| {
        status(ui).is_some_and(|s| s.entries.iter().any(|e| e.unstaged.is_some()))
    });
    other.git(&["add", "lib.txt"]);
    ui.wait_for("the staged edit", |ui| {
        status(ui).is_some_and(|s| s.entries.iter().any(|e| e.staged.is_some()))
    });
    // With two repositories the chips pick the root, and the badge and the
    // status bar follow the pick.
    ui.wait_for("the chips", |ui| {
        ui.frame(|window, _| window.try_find(("git-root", added.0 as usize)).is_some())
    });
    ui.click(("git-root", added.0 as usize));
    ui.wait_for("the badge of the added root", |ui| ui.badge() == Some(1));
    other.git(&["switch", "-q", "-c", "elsewhere"]);
    ui.wait_for("its branch", |ui| ui.branch_text() == "elsewhere");
}

#[test]
fn status_letters_for_the_file_tree() {
    let (tmp, repo) = fixture_repo();
    repo.write("src/lib.rs", "pub fn g() {}\n");
    repo.write("new.txt", "n\n");
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.click("sidebar-git");
    let pane = ui.pane();
    let model = ui.cx.read(|cx| pane.read(cx).model().clone());
    ui.wait_for("the status", |ui| ui.rows(Group::Unstaged).len() == 2);
    let root = ui.cx.read(|cx| model.read(cx).roots()[0].root);
    ui.cx.read(|cx| {
        let m = model.read(cx);
        assert_eq!(m.letter(root, "src/lib.rs"), Some('M'));
        assert_eq!(m.letter(root, "new.txt"), Some('?'));
        assert_eq!(m.letter(root, "README.md"), None);
    });
}

impl Ui {
    fn review(&mut self) -> Entity<DiffView> {
        let tab = self.diff_tabs().remove(0);
        self.cx.read(|cx| tab.read(cx).view().clone())
    }

    fn workspace_state(&mut self, key: &str) -> Option<coqo_ui::serde_json::Value> {
        let window = self.window;
        self.cx
            .update_window(window, |_, window, cx| {
                Workspaces::for_window(window, cx).and_then(|w| w.read(cx).state(key).cloned())
            })
            .unwrap()
    }
}

/// `n` numbered lines.
fn numbered(n: u32) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn review_a_diff_then_stage_a_hunk_and_a_line() {
    let (tmp, repo) = fixture_repo();
    repo.write("src/lib.rs", &numbered(40));
    repo.commit_all("Forty lines");
    // Two hunks: line 5 replaced, and line 30 replaced by two lines.
    let changed = numbered(40)
        .replace("line 5\n", "five\n")
        .replace("line 30\n", "thirty\nthirty b\n");
    repo.write("src/lib.rs", &changed);
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.click("sidebar-git");
    ui.wait_for("the change", |ui| ui.rows(Group::Unstaged).len() == 1);
    ui.click(("git-unstaged-row", 0usize));
    ui.wait_for("the review", |ui| {
        ui.diff_tabs().len() == 1 && {
            let view = ui.review();
            ui.cx.read(|cx| view.read(cx).hunks().len() == 2)
        }
    });
    let view = ui.review();
    let path = "src/lib.rs";
    ui.cx.read(|cx| {
        let v = view.read(cx);
        assert_eq!(v.status(), &Status::Ready);
        assert_eq!(v.selected_file(), Some(path));
        assert_eq!(v.mode(cx), coqo_ui::diff::DiffMode::Split);
        assert_eq!(v.viewed_count(), (0, 1));
    });

    // Unified, by the toggle; back to side by side with `m`. Remembered
    // in the workspace.
    ui.click("diff-mode-unified");
    assert_eq!(
        ui.cx.read(|cx| view.read(cx).mode(cx)),
        coqo_ui::diff::DiffMode::Unified
    );
    ui.wait_for("the mode saved", |ui| {
        ui.workspace_state(viewed::MODE_KEY)
            .as_ref()
            .and_then(|v| v.as_str())
            == Some("unified")
    });
    let focus = ui.cx.read(|cx| view.read(cx).viewer().focus_handle(cx));
    ui.frame(|window, cx| window.focus(&focus, cx));
    ui.frame(|window, cx| window.press("m", cx));
    assert_eq!(
        ui.cx.read(|cx| view.read(cx).mode(cx)),
        coqo_ui::diff::DiffMode::Split
    );

    // A collapsed unchanged region opens.
    let folds = |ui: &mut Ui| -> Vec<usize> {
        ui.frame(|window, _| {
            (0..60usize)
                .filter(|i| window.try_find(("expand", *i)).is_some())
                .collect()
        })
    };
    let before = folds(&mut ui);
    assert!(!before.is_empty(), "the unchanged lines between hunks fold");
    ui.click(("expand", before[0]));
    let after = folds(&mut ui);
    assert!(!after.contains(&before[0]), "{before:?} → {after:?}");

    // Mark it viewed (the checkbox), and find it viewed in a new view.
    ui.click(("diff-viewed", 0usize));
    assert_eq!(ui.cx.read(|cx| view.read(cx).viewed_count()), (1, 1));
    let identity = ui.cx.read(|cx| view.read(cx).source().identity());
    assert!(identity.ends_with(":index..worktree"), "{identity}");
    let key = viewed::viewed_key(&identity);
    ui.wait_for("the mark saved", |ui| {
        ui.workspace_state(&key)
            .is_some_and(|v| v.get(path).is_some())
    });
    let window = ui.window;
    let source = ui.cx.read(|cx| view.read(cx).source().clone());
    let again = ui
        .cx
        .update_window(window, |_, window, cx| {
            cx.new(|cx| DiffView::new(source, None, window, cx))
        })
        .unwrap();
    ui.wait_for("the second view", |ui| {
        ui.cx.read(|cx| again.read(cx).status() == &Status::Ready)
    });
    assert!(ui.cx.read(|cx| again.read(cx).is_viewed(path)));
    drop(again);

    // Stage the first hunk with its gutter button.
    ui.click(("diff-stage-hunk", 0usize));
    ui.wait_for("the hunk staged", |ui| {
        let v = ui.review();
        ui.cx
            .read(|cx| v.read(cx).hunks().len() == 1 && !v.read(cx).is_staging())
    });
    let cached = repo.git(&["diff", "--cached", "-U0"]);
    assert!(cached.contains("-line 5\n+five\n"), "{cached}");
    assert!(!cached.contains("thirty"), "{cached}");
    // The file's change moved on, so it is no longer viewed.
    assert_eq!(ui.cx.read(|cx| view.read(cx).viewed_count()), (0, 1));

    // Select one added line (`thirty b`, new line 31) and stage just it.
    let range = coqo_ui::diff::LineRange::lines(path, coqo_ui::diff::Side::New, 31, 31);
    view.update(&mut ui.cx, |v, cx| v.select_lines(Some(&range), cx));
    assert_eq!(
        ui.cx.read(|cx| view.read(cx).selection().cloned()),
        Some(range)
    );
    ui.click("diff-stage-lines");
    ui.wait_for("the line staged", |_| {
        repo.git(&["diff", "--cached", "-U0"]).contains("thirty b")
    });
    let cached = repo.git(&["diff", "--cached", "-U0"]);
    assert!(cached.contains("+thirty b\n"), "{cached}");
    assert!(
        !cached.contains("-line 30"),
        "only the chosen line: {cached}"
    );
    assert!(
        !cached.contains("+thirty\n"),
        "only the chosen line: {cached}"
    );
    // What is left unstaged is the rest of that hunk.
    let unstaged = repo.git(&["diff", "-U0"]);
    assert!(unstaged.contains("-line 30\n+thirty\n"), "{unstaged}");
    assert!(ui.cx.read(|cx| view.read(cx).error().is_none()));
}

#[test]
fn a_saved_diff_tab_restores_as_its_sides_tab() {
    use coqo_ui::{dock::TabItem as _, serde_json::json};
    let mut cx = TestAppContext::single();
    let (window, _) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        coqo_ui::open_window(WindowOptions::default(), cx, |_, cx| {
            cx.new(|_| coqo_ui::gpui::Empty)
        })
        .unwrap()
    });
    // A per-file tab saved before #93, and one saved since.
    for (state, path) in [
        (
            json!({"workspace": "ws", "root": 0, "path": "src/lib.rs", "staged": true}),
            Some("src/lib.rs"),
        ),
        (
            json!({"workspace": "ws", "root": 0, "path": null, "staged": false}),
            None,
        ),
    ] {
        cx.update_window(window, |_, window, cx| {
            let tab = diff::restore(&state, window, cx).expect("restores");
            let staged = state["staged"].as_bool().unwrap();
            assert_eq!(
                tab.key(cx).as_deref(),
                Some(diff::key(&WorkspaceId("ws".into()), RootId(0), staged).as_str())
            );
            let tab = tab.downcast::<DiffTab>().unwrap();
            let view = tab.read(cx).view().clone();
            assert_eq!(view.read(cx).selected_file(), path);
            assert_eq!(tab.read(cx).save(cx), state);
        })
        .unwrap();
    }
}

fn sample_status() -> StatusResult {
    let entry = |path: &str, staged, unstaged| StatusEntry {
        path: path.into(),
        orig_path: None,
        staged,
        unstaged,
        conflict: None,
    };
    StatusResult {
        repo: RepoInfo {
            workdir: "/home/you/code/coqo".into(),
            prefix: String::new(),
        },
        head: Head {
            branch: Some("issue-91-git-mod".into()),
            oid: Some("2bee2eb000000000000000000000000000000000".into()),
            upstream: Some("origin/issue-91-git-mod".into()),
            ahead: 2,
            behind: 0,
        },
        entries: vec![
            entry(
                "crates/coqod/src/routes.rs",
                Some(FileChange::Modified),
                None,
            ),
            entry("mods/git/src/lib.rs", Some(FileChange::Added), None),
            StatusEntry {
                orig_path: Some("mods/git/src/read.rs".into()),
                ..entry("mods/git/src/reader.rs", Some(FileChange::Renamed), None)
            },
            entry(
                "crates/coqod/src/workspace/mod.rs",
                None,
                Some(FileChange::Modified),
            ),
            entry(
                "docs/PLAN.md",
                Some(FileChange::Modified),
                Some(FileChange::Modified),
            ),
            entry("scratch.txt", None, Some(FileChange::Untracked)),
        ],
    }
}

fn sample_diff() -> FileDiff {
    let line = |kind, old: Option<u32>, new: Option<u32>, text: &str| DiffLine {
        kind,
        old_line: old,
        new_line: new,
        text: text.into(),
        newline: true,
    };
    use LineKind::{Added as A, Context as C, Removed as R};
    FileDiff {
        orig_path: None,
        path: "crates/coqod/src/routes.rs".into(),
        old: Some(FileSide {
            mode: 0o100644,
            oid: None,
        }),
        new: Some(FileSide {
            mode: 0o100644,
            oid: None,
        }),
        binary: false,
        additions: 5,
        deletions: 1,
        hunks: vec![
            Hunk {
                old_start: 12,
                old_lines: 4,
                new_start: 12,
                new_lines: 5,
                section: Some("use crate::workspace::{self, WorkspaceService};".into()),
                lines: vec![
                    line(
                        C,
                        Some(12),
                        Some(12),
                        "use crate::editor::{self, EditorService};",
                    ),
                    line(A, None, Some(13), "use crate::git::{self, GitService};"),
                    line(C, Some(13), Some(14), "use crate::mods::Mods;"),
                    line(
                        C,
                        Some(14),
                        Some(15),
                        "use crate::terminal::{self, TerminalService};",
                    ),
                    line(C, Some(15), Some(16), ""),
                ],
            },
            Hunk {
                old_start: 48,
                old_lines: 4,
                new_start: 49,
                new_lines: 7,
                section: Some(
                    "pub fn register(router: Router, ctx: &RouteContext) -> Router {".into(),
                ),
                lines: vec![
                    line(
                        C,
                        Some(48),
                        Some(49),
                        "    let router = editor::register(router, editor);",
                    ),
                    line(
                        R,
                        Some(49),
                        None,
                        "    let buffers = BufferService::new(workspaces);",
                    ),
                    line(A, None, Some(50), "    let router = git::register("),
                    line(A, None, Some(51), "        router,"),
                    line(
                        A,
                        None,
                        Some(52),
                        "        GitService::new(Arc::clone(&workspaces), ctx.mods.clone()),",
                    ),
                    line(A, None, Some(53), "    );"),
                    line(
                        A,
                        None,
                        Some(54),
                        "    let buffers = BufferService::new(workspaces);",
                    ),
                    line(
                        C,
                        Some(50),
                        Some(55),
                        "    buffers::register(router, buffers)",
                    ),
                ],
            },
        ],
    }
}

/// Renders the Git pane and a diff tab offscreen for visual review. Needs
/// a GPU: `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored git_screenshots`.
#[test]
#[ignore = "needs a GPU; run manually"]
fn git_screenshots() {
    use coqo_ui::test::render_offscreen;

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
            crate::shell::init(cx);
            // The default layout, with the Git pane shown.
            let scratch = cx.new(|cx| {
                Shell::new(
                    Rc::new(MemoryLayoutStore::default()),
                    DEFAULT_WORKSPACE,
                    window,
                    cx,
                )
            });
            let mut layout = scratch.read(cx).layout(cx);
            drop(scratch);
            layout.sidebar = SidebarLayout {
                width: 300.,
                pane: Some("git".into()),
            };
            let store = Rc::new(MemoryLayoutStore::default());
            store.save(DEFAULT_WORKSPACE, &layout).unwrap();
            let workspace = WorkspaceInfo {
                id: WorkspaceId("ws".into()),
                name: "coqo".into(),
                roots: vec![WorkspaceRoot {
                    id: RootId(0),
                    path: "/home/you/code/coqo".into(),
                    name: "coqo".into(),
                }],
                created_at: 0,
                last_opened_at: 0,
                open: true,
                project: None,
            };
            let model = GitModel::for_window(window, cx);
            model.update(cx, |m, cx| {
                let mut root = RootGit::new(RootId(0));
                root.name = "coqo".into();
                root.status = Some(sample_status());
                root.stashes = vec![Stash {
                    index: 0,
                    message: "On main: try the lanes layout".into(),
                    oid: "0".repeat(40),
                    time: 0,
                }];
                m.set_for_tests(workspace.clone(), vec![root], cx);
            });
            cx.new(|cx| {
                let shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                let source = StaticSource {
                    identity: "screenshot".into(),
                    base: "HEAD".into(),
                    head: "index".into(),
                    files: vec![sample_diff()],
                    versions: None,
                };
                let view = cx.new(|cx| DiffView::new(Rc::new(source), None, window, cx));
                let tab =
                    cx.new(|_| DiffTab::with_view(workspace.id.clone(), RootId(0), true, view));
                shell.dock().update(cx, |dock, cx| {
                    dock.open_tab(AnyTab::new(tab), window, cx);
                });
                // Keep the model alive for the frame, so the pane finds it.
                std::mem::forget(model);
                shell
            })
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-git-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}

/// The Git pane alone, on its panel, for the discard screenshots.
struct PaneShot(Entity<GitPane>);

impl coqo_ui::gpui::Render for PaneShot {
    fn render(
        &mut self,
        _: &mut Window,
        cx: &mut coqo_ui::gpui::Context<Self>,
    ) -> impl coqo_ui::gpui::IntoElement {
        use coqo_ui::{
            ActiveTokens as _,
            gpui::{ParentElement as _, Styled as _, div},
        };
        let p = cx.tokens().palette.clone();
        div().size_full().bg(p.ground.hsla()).p(px(12.)).child(
            div()
                .size_full()
                .p(px(12.))
                .rounded(px(18.))
                .bg(p.panel.hsla())
                .child(self.0.clone()),
        )
    }
}

/// Renders the Git pane asking before "Discard all", with the note of an
/// earlier discard (#226). Needs a GPU:
/// `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored git_discard_screenshots`.
#[test]
#[ignore = "needs a GPU; run manually"]
fn git_discard_screenshots() {
    use coqo_ui::{dock::Dock, test::render_offscreen};

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let image = render_offscreen(appearance, size(380., 900.), |window, cx| {
            let workspace = WorkspaceInfo {
                id: WorkspaceId("ws".into()),
                name: "coqo".into(),
                roots: vec![WorkspaceRoot {
                    id: RootId(0),
                    path: "/home/you/code/coqo".into(),
                    name: "coqo".into(),
                }],
                created_at: 0,
                last_opened_at: 0,
                open: true,
                project: None,
            };
            let model = GitModel::for_window(window, cx);
            model.update(cx, |m, cx| {
                let mut root = RootGit::new(RootId(0));
                root.name = "coqo".into();
                root.status = Some(sample_status());
                m.set_for_tests(workspace, vec![root], cx);
                m.set_notice_for_tests(
                    DiscardNotice {
                        root: RootId(0),
                        text: "Moved “draft.md” to the trash".into(),
                        ops: Vec::new(),
                    },
                    cx,
                );
            });
            let dock = cx.new(Dock::new);
            let pane = cx.new(|cx| GitPane::new(dock.downgrade(), window, cx));
            pane.update(cx, |pane, cx| {
                let entries = pane.rows(Group::Unstaged, cx);
                pane.request_discard(entries, cx);
                if let Some(p) = pane.pending_discard_mut() {
                    let stat = |path: &str, additions, deletions| ChangedFile {
                        path: path.into(),
                        change: FileChange::Modified,
                        orig_path: None,
                        additions,
                        deletions,
                        binary: false,
                    };
                    p.stats = Some(vec![
                        stat("crates/coqod/src/workspace/mod.rs", 12, 3),
                        stat("docs/PLAN.md", 4, 1),
                    ]);
                }
            });
            std::mem::forget(model);
            std::mem::forget(dock);
            cx.new(|_| PaneShot(pane))
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-git-discard-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}

impl Ui {
    fn model(&mut self) -> Entity<GitModel> {
        let pane = self.pane();
        self.cx.read(|cx| pane.read(cx).model().clone())
    }

    fn shows(&mut self, id: &'static str) -> bool {
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn history(&mut self) -> Entity<History> {
        self.frame(History::for_window)
    }
}

/// Every path under `dir`.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        }
        out.push(path);
    }
    out
}

#[test]
fn discard_files_and_untracked_ones_from_the_git_pane() {
    let (tmp, repo) = fixture_repo();
    repo.write("README.md", "# Demo\n\nMore words.\n");
    repo.write("notes.txt", "todo\n");
    repo.git(&["mv", "src/lib.rs", "src/core.rs"]);
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.click("sidebar-git");
    ui.wait_for("the changes", |ui| ui.rows(Group::Unstaged).len() == 2);
    // The rename is one row, with where it came from.
    let pane = ui.pane();
    let staged = ui.cx.read(|cx| pane.read(cx).rows(Group::Staged, cx));
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].path, "src/core.rs");
    assert_eq!(staged[0].staged, Some(FileChange::Renamed));
    assert_eq!(staged[0].orig_path.as_deref(), Some("src/lib.rs"));
    let model = ui.model();
    let root = ui.cx.read(|cx| model.read(cx).roots()[0].root);
    assert_eq!(
        ui.cx.read(|cx| model.read(cx).letter(root, "src/core.rs")),
        Some('R')
    );

    // Discard asks first, and says what goes; Cancel leaves it.
    ui.click(("git-discard", 0usize));
    assert!(ui.shows("git-discard-confirm"));
    ui.wait_for("the line counts", |ui| {
        let pane = ui.pane();
        ui.cx.read(|cx| {
            pane.read(cx)
                .pending_discard()
                .is_some_and(|p| p.stats.is_some())
        })
    });
    let stats = ui.cx.read(|cx| {
        pane.read(cx)
            .pending_discard()
            .and_then(|p| p.stats.clone())
            .unwrap()
    });
    let readme = stats.iter().find(|f| f.path == "README.md").unwrap();
    assert_eq!((readme.additions, readme.deletions), (2, 0));
    ui.click("git-discard-cancel");
    assert!(!ui.shows("git-discard-confirm"));
    assert_eq!(repo.read("README.md"), "# Demo\n\nMore words.\n");

    // Yes: back to the index version, and the rename is untouched.
    ui.click(("git-discard", 0usize));
    ui.click("git-discard-ok");
    ui.wait_for("README.md discarded", |ui| {
        ui.rows(Group::Unstaged).len() == 1
    });
    assert_eq!(repo.read("README.md"), "# Demo\n");
    assert_eq!(ui.rows(Group::Staged).len(), 1);
    let notice = ui.cx.read(|cx| model.read(cx).notice().cloned()).unwrap();
    assert_eq!(notice.text, "Discarded the changes to “README.md”");
    assert!(notice.ops.is_empty());
    assert!(!ui.shows("git-discard-undo"), "nothing to bring back");

    // An untracked file goes to the (test) trash, and Undo brings it back.
    ui.click(("git-discard", 0usize));
    ui.click("git-discard-ok");
    ui.wait_for("notes.txt trashed", |ui| {
        ui.rows(Group::Unstaged).is_empty()
    });
    assert!(!repo.path().join("notes.txt").exists());
    let trash = ui.daemon.trash();
    assert!(
        walk(&trash).iter().any(|p| p.ends_with("notes.txt")),
        "in the trash folder"
    );
    let history = ui.history();
    let op = ui
        .cx
        .read(|cx| history.read(cx).next_undo().cloned())
        .unwrap();
    assert_eq!(op.describe(), "delete of “notes.txt”");
    assert!(op.cannot_undo().is_none());
    assert!(ui.shows("git-discard-undo"));
    ui.click("git-discard-undo");
    ui.wait_for("notes.txt back", |ui| ui.rows(Group::Unstaged).len() == 1);
    assert_eq!(repo.read("notes.txt"), "todo\n");
    assert!(ui.cx.read(|cx| history.read(cx).next_undo().is_none()));
    assert!(ui.cx.read(|cx| history.read(cx).next_redo().is_some()));
    assert!(ui.cx.read(|cx| model.read(cx).error().is_none()));
}

#[test]
fn discard_a_hunk_and_a_line_from_the_review() {
    let (tmp, repo) = fixture_repo();
    repo.write("src/lib.rs", &numbered(40));
    repo.commit_all("Forty lines");
    let changed = numbered(40)
        .replace("line 5\n", "five\n")
        .replace("line 30\n", "thirty\nthirty b\n");
    repo.write("src/lib.rs", &changed);
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    ui.click("sidebar-git");
    ui.wait_for("the change", |ui| ui.rows(Group::Unstaged).len() == 1);
    ui.click(("git-unstaged-row", 0usize));
    ui.wait_for("the review", |ui| {
        ui.diff_tabs().len() == 1 && {
            let view = ui.review();
            ui.cx.read(|cx| view.read(cx).hunks().len() == 2)
        }
    });
    let view = ui.review();
    let path = "src/lib.rs";

    // A hunk: asked first, then gone from the work tree only.
    view.update(&mut ui.cx, |v, cx| v.request_discard_hunk(0, cx));
    assert!(ui.shows("diff-discard-confirm"));
    ui.click("diff-discard-cancel");
    assert!(ui.cx.read(|cx| view.read(cx).pending_discard().is_none()));
    view.update(&mut ui.cx, |v, cx| v.request_discard_hunk(0, cx));
    ui.click("diff-discard-ok");
    ui.wait_for("the hunk discarded", |ui| {
        let v = ui.review();
        ui.cx
            .read(|cx| v.read(cx).hunks().len() == 1 && !v.read(cx).is_staging())
    });
    assert_eq!(
        repo.read(path),
        numbered(40).replace("line 30\n", "thirty\nthirty b\n")
    );
    assert_eq!(repo.git(&["diff", "--cached"]), "");

    // One added line, chosen by selection: the header offers it.
    let range = coqo_ui::diff::LineRange::lines(path, coqo_ui::diff::Side::New, 31, 31);
    view.update(&mut ui.cx, |v, cx| v.select_lines(Some(&range), cx));
    ui.click("diff-discard-lines");
    assert!(ui.shows("diff-discard-confirm"));
    ui.click("diff-discard-ok");
    ui.wait_for("the line discarded", |_| {
        !repo.read(path).contains("thirty b")
    });
    assert_eq!(
        repo.read(path),
        numbered(40).replace("line 30\n", "thirty\n")
    );
    assert!(ui.cx.read(|cx| view.read(cx).error().is_none()));

    // A source that can't discard offers nothing.
    assert!(!crate::diff::DiffSource::can_discard(
        &StaticSource::default()
    ));
}
