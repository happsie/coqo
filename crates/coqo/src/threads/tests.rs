// SPDX-License-Identifier: Apache-2.0

//! Comment threads in the diff view against a real `coqod` (with the
//! built-in git mod) served in-process, on a fixture repository: comment on
//! a line, see it pending, reply, resolve, and find it again after the
//! daemon restarts. And the overlay offscreen, for board 4.

use std::{
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_mod_git::testing::Fixture as Repo;
use coqo_protocol::methods::{
    git::DiffSide,
    questions::{AnchorSpan, AnchorStatus},
    threads::{
        self as tm, Author, Backend, Comment, CommentId, ListParams, Thread, ThreadAnchor,
        ThreadId, ThreadLines, ThreadSide, ThreadStatus,
    },
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    Appearance,
    diff::{LineRange, Side},
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Window, WindowBounds,
        WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::overlay::{ReviewState, SEND_LATER, ThreadsOverlay};
use crate::{
    daemon::{self, Daemon, DaemonConfig},
    diff::{DiffView, source::SourceVersions},
    git::diff::{self as diff_tab, DiffTab},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{TIMEOUT, TestDaemon},
    workspace,
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

    fn click(&mut self, id: impl Into<coqo_ui::gpui::ElementId>) {
        let id = id.into();
        self.frame(|window, cx| window.click(id, cx));
    }

    fn has(&mut self, id: impl Into<coqo_ui::gpui::ElementId>) -> bool {
        let id = id.into();
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn diff_tab(&mut self) -> Option<Entity<DiffTab>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == diff_tab::KIND)
                .find_map(|t| dock.tab(t.id)?.downcast::<DiffTab>())
        })
    }

    fn threads(&mut self, review: &Entity<ReviewState>) -> Vec<Thread> {
        self.cx.read(|cx| {
            review
                .read(cx)
                .store()
                .read(cx)
                .all()
                .into_iter()
                .cloned()
                .collect()
        })
    }

    fn type_into(
        &mut self,
        input: impl FnOnce(&ReviewState) -> Option<Entity<coqo_ui::input::PromptInput>>,
        review: &Entity<ReviewState>,
        text: &str,
    ) {
        let input = self
            .cx
            .read(|cx| input(review.read(cx)))
            .expect("the field is shown");
        self.frame(|window, cx| input.update(cx, |i, cx| i.set_text(text, window, cx)));
    }

    fn connected(&mut self) -> bool {
        self.cx
            .read(|cx| Daemon::global(cx).is_some_and(|d| d.read(cx).is_connected()))
    }
}

#[test]
fn comment_on_a_line_reply_resolve_and_find_it_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    repo.write("src/lib.rs", &lines);
    repo.commit_all("Twelve lines");
    repo.write("src/lib.rs", &lines.replace("line 8\n", "eight\n"));
    let mut ui = open_ui(repo.path().to_owned(), tmp);

    // The change in the review surface, threads attached.
    ui.click("sidebar-git");
    ui.wait_for("the change", |ui| ui.has(("git-unstaged-row", 0usize)));
    ui.click(("git-unstaged-row", 0usize));
    ui.wait_for("the review", |ui| {
        ui.diff_tab().is_some_and(|tab| {
            ui.cx.read(|cx| {
                tab.read(cx).review().is_some()
                    && tab
                        .read(cx)
                        .view()
                        .read(cx)
                        .file_diff("src/lib.rs")
                        .is_some()
            })
        })
    });
    let tab = ui.diff_tab().unwrap();
    let (view, review) = ui.cx.read(|cx| {
        let t = tab.read(cx);
        (t.view().clone(), t.review().unwrap().clone())
    });
    ui.wait_for("the threads", |ui| {
        ui.cx
            .read(|cx| review.read(cx).store().read(cx).is_loaded())
    });
    assert!(ui.has("review-note"), "the pending review is explained");
    assert!(!ui.has("review-send"), "nothing pending yet");

    // "+" on the changed line (the body calls `on_add` with it).
    let range = LineRange::lines("src/lib.rs", Side::New, 8, 8);
    ui.frame(|window, cx| {
        review.update(cx, |s, cx| s.open_composer(&range, window, cx));
    });
    assert!(ui.has("thread-composer"));
    ui.type_into(|s| s.composer_input().cloned(), &review, "Why **eight**?");
    ui.click("thread-composer-add");
    ui.wait_for("the thread", |ui| ui.threads(&review).len() == 1);
    let t = ui.threads(&review).remove(0);
    assert_eq!(t.anchor.path, "src/lib.rs");
    assert_eq!(t.anchor.version, DiffSide::Worktree);
    assert_eq!(t.anchor.side, ThreadSide::New);
    assert_eq!(t.anchor.lines, Some(ThreadLines { start: 8, end: 8 }));
    assert_eq!(t.comments[0].body, "Why **eight**?");
    assert!(t.comments[0].pending);
    assert_eq!(
        t.diff.as_deref(),
        Some(ui.cx.read(|cx| view.read(cx).source().identity()).as_str())
    );
    ui.wait_for("the card", |ui| ui.has(("thread", t.id.0 as usize)));
    assert!(!ui.has("thread-composer"), "the composer closed");
    // Pending: on the comment, in the header (with the send button, for
    // #95) and on the file's row.
    assert!(ui.has(("thread-pending", t.comments[0].id.0 as usize)));
    assert!(ui.has("review-pending-count"));
    assert!(ui.has("review-send"));
    assert_eq!(ui.cx.read(|cx| review.read(cx).pending_count(cx)), 1);
    assert!(ui.has(("thread-marker", t.id.0 as usize)));
    assert_eq!(SEND_LATER, "Sending to the agent comes in #95");

    // Reply.
    let id = t.id;
    ui.type_into(|s| s.reply_input(id).cloned(), &review, "Also rename it");
    ui.click(("thread-reply", id.0 as usize));
    ui.wait_for("the reply", |ui| {
        ui.threads(&review)
            .first()
            .is_some_and(|t| t.comments.len() == 2)
    });
    assert_eq!(ui.cx.read(|cx| review.read(cx).pending_count(cx)), 2);
    let reply_text = ui
        .cx
        .read(|cx| review.read(cx).reply_input(id).unwrap().read(cx).text(cx));
    assert_eq!(reply_text, "", "the field is cleared");

    // Edit the first comment in place.
    let first = ui.threads(&review)[0].comments[0].id;
    ui.click(("thread-edit", first.0 as usize));
    ui.wait_for("the edit field", |ui| {
        ui.cx.read(|cx| review.read(cx).edit_input().is_some())
    });
    ui.type_into(|s| s.edit_input().cloned(), &review, "Why **8**?");
    ui.click("thread-edit-save");
    ui.wait_for("the edit", |ui| {
        ui.threads(&review).first().is_some_and(|t| {
            t.comments[0].body == "Why **8**?" && t.comments[0].edited_at.is_some()
        })
    });
    assert!(ui.cx.read(|cx| review.read(cx).edit_input().is_none()));

    // Resolve: the card folds to one line.
    ui.click(("thread-resolve", id.0 as usize));
    ui.wait_for("the resolve", |ui| {
        ui.threads(&review)
            .first()
            .is_some_and(|t| t.status == ThreadStatus::Resolved)
    });
    ui.wait_for("the folded card", |ui| {
        ui.has(("thread-show", id.0 as usize))
    });
    assert!(ui.cx.read(|cx| review.read(cx).error().is_none()));

    // The daemon restarts: the thread is still there, resolved, with both
    // comments, and the UI follows it again.
    ui.daemon.stop();
    ui.wait_for("the drop", |ui| !ui.connected());
    ui.daemon.serve();
    let listed = ui.daemon.with_client(async |client| {
        client
            .request::<tm::List>(ListParams {
                workspace: t.workspace.clone(),
                root: None,
                path: None,
                diff: None,
            })
            .await
            .unwrap()
    });
    assert_eq!(listed.threads.len(), 1);
    let after = &listed.threads[0];
    assert_eq!((after.id, after.status), (id, ThreadStatus::Resolved));
    assert_eq!(after.comments.len(), 2);
    assert!(after.comments.iter().all(|c| c.pending));
    ui.wait_for("a new connection", |ui| {
        if !ui.connected() {
            ui.cx.update(|cx| {
                if let Some(d) = Daemon::global(cx) {
                    d.update(cx, |d, cx| d.retry(cx));
                }
            });
        }
        ui.connected()
    });
    // Reopen it from the folded card, after the reconnect.
    ui.click(("thread-show", id.0 as usize));
    ui.wait_for("the card again", |ui| {
        ui.has(("thread-resolve", id.0 as usize))
    });
    ui.click(("thread-resolve", id.0 as usize));
    ui.wait_for("the reopen", |ui| {
        ui.threads(&review)
            .first()
            .is_some_and(|t| t.status == ThreadStatus::Open)
    });
}

// ---- offscreen --------------------------------------------------------------

fn sample_thread(
    id: u64,
    path: &str,
    lines: (u32, u32),
    status: ThreadStatus,
    comments: Vec<(Author, &str, bool)>,
) -> Thread {
    Thread {
        id: ThreadId(id),
        workspace: WorkspaceId("ws".into()),
        backend: Backend::Local,
        diff: Some("screenshot".into()),
        anchor: ThreadAnchor {
            root: RootId(0),
            path: path.into(),
            side: ThreadSide::New,
            version: DiffSide::Worktree,
            code: None,
            resolution: Some(coqo_protocol::methods::questions::AnchorResolution {
                status: AnchorStatus::Exact,
                range: Some(AnchorSpan::default()),
                buffer: None,
                rev: None,
            }),
            lines: Some(ThreadLines {
                start: lines.0,
                end: lines.1,
            }),
        },
        status,
        comments: comments
            .into_iter()
            .enumerate()
            .map(|(i, (author, body, pending))| Comment {
                id: CommentId(id * 10 + i as u64),
                author,
                body: body.into(),
                pending,
                created_at: 0,
                edited_at: None,
            })
            .collect(),
        created_at: 0,
        updated_at: 0,
        resolved_at: None,
        seq: 1,
    }
}

/// The overlay over board 4's sample, offscreen, for visual review. Needs
/// a GPU: `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored threads_screenshots`.
#[test]
#[ignore = "needs a GPU; run manually"]
fn threads_screenshots() {
    use coqo_ui::{
        ActiveTokens as _,
        gpui::{Context, IntoElement, ParentElement as _, Render, Styled as _, div},
        test::render_offscreen,
    };

    use crate::diff::{source::StaticSource, tests::view::board_sample};

    struct Root(Entity<DiffView>);
    impl Render for Root {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(cx.tokens().palette.ground.hsla())
                .font_family(cx.tokens().metrics.font_ui.clone())
                .child(self.0.clone())
        }
    }

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let k = "checkout-core/src/main/kotlin/com/acme/checkout/pricing/DiscountService.kt";
    let java = "checkout-core/src/main/java/com/acme/checkout/CheckoutService.java";
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
            crate::diff::init(cx);
            let source = StaticSource {
                identity: "screenshot".into(),
                base: "main".into(),
                head: "feature/volume-discount".into(),
                files: board_sample(),
                versions: Some(SourceVersions {
                    workspace: WorkspaceId("ws".into()),
                    root: RootId(0),
                    prefix: String::new(),
                    base: DiffSide::Rev("main".into()),
                    head: DiffSide::Worktree,
                }),
            };
            let (list, diffs) = (source.list(), source.files.clone());
            let view = cx.new(|cx| DiffView::new(Rc::new(source), None, window, cx));
            let agent = Author::Agent {
                session: coqo_protocol::methods::agents::SessionId("s-1".into()),
            };
            let store = super::WorkspaceThreads::fixed(
                WorkspaceId("ws".into()),
                vec![
                    sample_thread(
                        1,
                        k,
                        (15, 15),
                        ThreadStatus::Resolved,
                        vec![
                            (Author::User, "Can isMember throw?", false),
                            (agent, "Wrapped it in runCatching", false),
                        ],
                    ),
                    sample_thread(
                        2,
                        k,
                        (19, 19),
                        ThreadStatus::Open,
                        vec![(
                            Author::User,
                            "Round with `RoundingMode.HALF_EVEN` to 2 decimals before the cap. \
                             Finance requires banker's rounding on discounts. Please also add a \
                             test with a 3-decimal result.",
                            true,
                        )],
                    ),
                    sample_thread(
                        3,
                        java,
                        (3, 3),
                        ThreadStatus::Open,
                        vec![(Author::User, "Null-safe?", true)],
                    ),
                ],
                cx,
            );
            ThreadsOverlay::attach_to(&view, store, window, cx);
            view.update(cx, |v, cx| {
                v.loaded(list, diffs.into_iter().map(Ok).collect(), cx);
                let paths: Vec<String> = v.files().iter().map(|f| f.path.clone()).collect();
                v.set_viewed(&paths[1], true, cx);
                v.set_viewed(&paths[3], true, cx);
                v.scroll_to(&coqo_ui::diff::LineRef::new(k, Side::New, 12), cx);
            });
            cx.new(|_| Root(view))
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-threads-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
