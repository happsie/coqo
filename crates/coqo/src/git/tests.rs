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
        DiffLine, FileChange, FileDiff, FileSide, Head, Hunk, LineKind, RepoInfo, Stash,
        StatusEntry, StatusResult,
    },
    workspace::{RootId, WorkspaceId, WorkspaceInfo, WorkspaceRoot},
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
    diff::{self, Content, DiffTab},
    model::{GitModel, RootGit},
    pane::{GitPane, Group},
};
use crate::{
    daemon::{self, DaemonConfig},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{TIMEOUT, TestDaemon},
    workspace,
};

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    _daemon: TestDaemon,
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
        _daemon: daemon,
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
            && matches!(
                ui.cx.read(|cx| tabs[0].read(cx).content().clone()),
                Content::Diff(_)
            )
    });
    let tab = ui.diff_tabs().remove(0);
    let Content::Diff(d) = ui.cx.read(|cx| tab.read(cx).content().clone()) else {
        unreachable!()
    };
    assert_eq!(
        (d.path.as_str(), d.additions, d.deletions),
        ("README.md", 2, 0)
    );

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
        matches!(ui.cx.read(|cx| tab.read(cx).content().clone()), Content::Diff(d) if d.hunks.is_empty())
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
    ui.wait_for("the branches", |ui| {
        ui.cx.read(|cx| {
            model
                .read(cx)
                .roots()
                .first()
                .is_some_and(|r| r.branches.len() == 2)
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
    ui.wait_for("the stash", |ui| {
        ui.cx
            .read(|cx| model.read(cx).roots()[0].stashes.len() == 1)
    });
    assert!(ui.rows(Group::Unstaged).is_empty());
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

fn sample_status() -> StatusResult {
    let entry = |path: &str, staged, unstaged| StatusEntry {
        path: path.into(),
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
                let tab = cx.new(|cx| {
                    DiffTab::with_diff(
                        workspace.id.clone(),
                        RootId(0),
                        sample_diff(),
                        true,
                        window,
                        cx,
                    )
                });
                shell.dock().update(cx, |dock, cx| {
                    dock.open_tab(AnyTab::new(tab), window, cx);
                });
                // The diff tab holds the model, so the pane finds it.
                drop(model);
                shell
            })
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-git-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
