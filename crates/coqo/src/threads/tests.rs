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
        AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::overlay::{ReviewState, ThreadsOverlay};
use crate::{
    daemon::{self, Daemon, DaemonConfig},
    diff::{DiffView, source::SourceVersions},
    git::diff::{self as diff_tab, DiffTab},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{AgentDaemon, TIMEOUT, TYPED_KEYS, TYPED_TEXT, TestDaemon, record_key_actions},
    workspace,
};

/// The daemon a test's UI talks to.
enum Backing {
    /// The real `coqod`.
    Full(TestDaemon),
    /// Its services with a scripted agent.
    Agent(AgentDaemon),
}

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    daemon: Backing,
    _tmp: tempfile::TempDir,
}

fn open_ui(folder: PathBuf, tmp: tempfile::TempDir) -> Ui {
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let paths = daemon.paths.clone();
    open_ui_on(folder, Backing::Full(daemon), paths, tmp)
}

fn open_ui_on(
    folder: PathBuf,
    backing: Backing,
    paths: coqo_client::DaemonPaths,
    tmp: tempfile::TempDir,
) -> Ui {
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
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
        daemon: backing,
        _tmp: tmp,
    }
}

impl Ui {
    fn full(&mut self) -> &mut TestDaemon {
        match &mut self.daemon {
            Backing::Full(d) => d,
            Backing::Agent(_) => panic!("not the full daemon"),
        }
    }

    fn agent_daemon(&self) -> &AgentDaemon {
        match &self.daemon {
            Backing::Agent(d) => d,
            Backing::Full(_) => panic!("not the scripted daemon"),
        }
    }

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
    ui.full().stop();
    ui.wait_for("the drop", |ui| !ui.connected());
    ui.full().serve();
    let listed = ui.full().with_client(async |client| {
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

/// Open the git pane's unstaged diff (first file row) and wait for its
/// review state, threads listed.
fn open_review(ui: &mut Ui, path: &str) -> (Entity<DiffTab>, Entity<ReviewState>) {
    ui.click("sidebar-git");
    ui.wait_for("the change", |ui| ui.has(("git-unstaged-row", 0usize)));
    ui.click(("git-unstaged-row", 0usize));
    ui.wait_for("the review", |ui| {
        ui.diff_tab().is_some_and(|tab| {
            ui.cx.read(|cx| {
                tab.read(cx).review().is_some()
                    && tab.read(cx).view().read(cx).file_diff(path).is_some()
            })
        })
    });
    let tab = ui.diff_tab().unwrap();
    let review = ui.cx.read(|cx| tab.read(cx).review().unwrap().clone());
    ui.wait_for("the threads", |ui| {
        ui.cx
            .read(|cx| review.read(cx).store().read(cx).is_loaded())
    });
    (tab, review)
}

/// #95 from the button to the reply: "Send 1 comment to …" opens the
/// popover with the workspace's chat chosen; sending puts the comment in
/// one prompt to that chat and clears it; the header says the agent is
/// addressing it; the agent's reply (through `review/reply`, as its
/// `reply_to_review` call) shows in the card under its name and keeps the
/// resolved thread open while the turn runs; a second send mid-turn is
/// refused and keeps its comment; the chat shows "Review sent", which opens
/// the diff; when the turn ends the thread folds as resolved.
#[test]
fn send_the_review_and_the_agents_reply_lands_in_the_thread() {
    use coqo_agents::testing::{Call, ScriptedProvider, Step};
    use coqo_protocol::methods::agents::{
        self as agm, ContentBlock, CreateSessionParams, StopReason,
    };

    let go = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider =
        ScriptedProvider::new().turn([Step::Wait(go.clone()), Step::Stop(StopReason::EndTurn)]);
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    repo.write("src/lib.rs", &lines);
    repo.commit_all("Twelve lines");
    repo.write("src/lib.rs", &lines.replace("line 8\n", "eight\n"));
    let daemon = AgentDaemon::with_review(&tmp.path().join("d"), &provider);
    let paths = daemon.paths.clone();
    let folder = repo.path().canonicalize().unwrap();
    let mut ui = open_ui_on(folder.clone(), Backing::Agent(daemon), paths, tmp);

    // A chat in the workspace.
    ui.wait_for("the workspace", |ui| {
        ui.cx.read(|cx| {
            crate::workspace::Workspaces::global(cx).is_some_and(|w| w.read(cx).current().is_some())
        })
    });
    let ws = ui.cx.read(|cx| {
        crate::workspace::Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .unwrap()
            .id
            .clone()
    });
    let (cwd, ws_id) = (folder.to_string_lossy().into_owned(), ws.0.clone());
    let session = ui.agent_daemon().with_client(async move |client| {
        client
            .request::<agm::Create>(CreateSessionParams {
                provider: "fake".into(),
                cwd,
                workspace: Some(ws_id),
                title: Some("Volume discount".into()),
                mode: None,
                from: None,
            })
            .await
            .unwrap()
            .id
    });
    ui.wait_for("the chat listed", |ui| {
        ui.cx.read(|cx| {
            crate::chat::list::ChatList::global(cx)
                .is_some_and(|l| l.read(cx).info(&session).is_some())
        })
    });

    // A pending comment on the changed line.
    let (_tab, review) = open_review(&mut ui, "src/lib.rs");
    let range = LineRange::lines("src/lib.rs", Side::New, 8, 8);
    ui.frame(|window, cx| {
        review.update(cx, |s, cx| s.open_composer(&range, window, cx));
    });
    ui.type_into(|s| s.composer_input().cloned(), &review, "Spell it **8**.");
    ui.click("thread-composer-add");
    ui.wait_for("the thread", |ui| ui.threads(&review).len() == 1);
    let id = ui.threads(&review)[0].id;
    ui.wait_for("the send button", |ui| ui.has("review-send"));

    // The popover offers the chat, chosen.
    ui.click("review-send");
    ui.wait_for("the popover", |ui| ui.has("review-send-popover"));
    assert!(ui.has(("review-target", 0usize)) && ui.has("review-target-new"));
    assert_eq!(
        ui.cx.read(|cx| review.read(cx).send_target().cloned()),
        Some(crate::questions::ask::AskTarget::Session(session.clone()))
    );
    ui.click("review-send-go");
    ui.wait_for("the send", |ui| {
        ui.cx
            .read(|cx| !review.read(cx).is_sending_open() && review.read(cx).pending_count(cx) == 0)
    });
    ui.wait_for("the addressing state", |ui| ui.has("review-addressing"));
    let prompt = provider
        .calls()
        .into_iter()
        .find_map(|c| match c {
            Call::Prompt(content) => Some(content),
            _ => None,
        })
        .expect("the review was prompted");
    let ContentBlock::Text { text } = &prompt[0] else {
        panic!("{prompt:?}")
    };
    assert!(text.contains(&format!("## Thread {id}: ")), "{text}");
    assert!(text.contains("src/lib.rs:8"), "{text}");
    assert!(text.contains("> Spell it **8**."), "{text}");
    assert!(text.contains("reply_to_review"), "{text}");

    // The agent replies and resolves: the card shows it, still open.
    let (ws_id, agent) = (ws.clone(), session.clone());
    ui.agent_daemon().with_client(async move |client| {
        client
            .request::<tm::AgentReply>(tm::AgentReplyParams {
                workspace: ws_id,
                session: agent,
                thread: id,
                body: "Now `8`.".into(),
                resolve: true,
            })
            .await
            .unwrap()
    });
    ui.wait_for("the reply in the card", |ui| {
        let t = ui.threads(&review);
        t.first().is_some_and(|t| {
            t.status == ThreadStatus::Resolved
                && t.comments.last().is_some_and(|c| {
                    matches!(c.author, Author::Agent { .. })
                        && ui.has(("thread-comment-card", c.id.0 as usize))
                })
        })
    });
    let reply = ui.threads(&review)[0].comments.last().unwrap().clone();
    let name = ui
        .cx
        .read(|cx| super::author_label(review.read(cx).store().read(cx), &reply.author, cx).1);
    assert_eq!(name, "Fake agent");
    assert!(
        ui.has(("thread-resolve", id.0 as usize)),
        "open while the turn runs"
    );
    assert!(!ui.has(("thread-show", id.0 as usize)));

    // Mid-turn, another comment cannot go yet, and stays pending.
    let store = ui.cx.read(|cx| review.read(cx).store().clone());
    let added = ui
        .cx
        .update(|cx| super::reply(&store, id, "And a test.".into(), cx));
    ui.wait_for("the new comment", |ui| {
        ui.cx.read(|cx| review.read(cx).pending_count(cx) == 1)
    });
    drop(added);
    ui.click("review-send");
    ui.wait_for("the popover", |ui| ui.has("review-send-popover"));
    ui.click("review-send-go");
    ui.wait_for("the refusal", |ui| ui.has("review-send-error"));
    let error = ui
        .cx
        .read(|cx| review.read(cx).send_error().map(str::to_owned))
        .unwrap();
    assert!(
        error.starts_with("\"Volume discount\" is in the middle of a turn"),
        "{error}"
    );
    assert_eq!(ui.cx.read(|cx| review.read(cx).pending_count(cx)), 1);
    ui.click("review-send-cancel");
    ui.wait_for("the popover closed", |ui| !ui.has("review-send-popover"));

    // The chat shows the review as a card that opens the diff.
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    let chat_session = session.clone();
    ui.frame(|window, cx| crate::chat::open_session(dock.downgrade(), chat_session, window, cx));
    let card = |ui: &mut Ui| -> Option<usize> {
        let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
        ui.cx.read(|cx| {
            let dock = dock.read(cx);
            let chat = dock
                .tree()
                .tabs()
                .into_iter()
                .find_map(|t| dock.tab(t.id)?.downcast::<crate::chat::tab::ChatTab>())?;
            chat.read(cx).transcript().entries.iter().position(|e| {
                matches!(e, crate::chat::transcript::Entry::User { content }
                    if crate::chat::tab::review_link(content).is_some())
            })
        })
    };
    ui.wait_for("the review card", |ui| {
        card(ui).is_some_and(|ix| ui.has(("chat-review", ix)))
    });
    let ix = card(&mut ui).unwrap();
    ui.click(("chat-review-open", ix));
    ui.wait_for("the diff again", |ui| {
        let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
        ui.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|t| dock.tab(t))
                .is_some_and(|t| t.downcast::<DiffTab>().is_some())
        })
    });

    // The turn ends: the thread folds as resolved, with the agent's reply.
    go.notify_one();
    ui.wait_for("the fold", |ui| {
        !ui.has("review-addressing") && ui.has(("thread-show", id.0 as usize))
    });
}

/// The chat tabs open in the window, by session.
fn chat_tabs(
    ui: &mut Ui,
) -> Vec<(
    coqo_ui::dock::TabId,
    coqo_protocol::methods::agents::SessionId,
)> {
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    ui.cx.read(|cx| {
        let dock = dock.read(cx);
        dock.tree()
            .tabs()
            .into_iter()
            .filter_map(|t| {
                let chat = dock.tab(t.id)?.downcast::<crate::chat::tab::ChatTab>()?;
                Some((t.id, chat.read(cx).session().clone()))
            })
            .collect()
    })
}

/// #286: the daemon records which session made a change. The agent edits
/// `src/lib.rs` (a completed tool call with its diff) in "Volume
/// discount", whose chat tab is then closed; another, newer chat is open.
/// The review's popover still defaults to "Volume discount" and says it
/// made the change, and the diff header says "made by … in \"Volume
/// discount\"".
#[test]
fn the_review_defaults_to_the_session_that_made_the_change_after_its_chat_closes() {
    use coqo_agents::AgentUpdate;
    use coqo_agents::testing::{ScriptedProvider, Step};
    use coqo_protocol::methods::agents::{
        self as agm, AgentState, ContentBlock, CreateSessionParams, PromptParams, StopReason,
        ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolKind,
    };

    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    repo.write("src/lib.rs", &lines);
    repo.commit_all("Twelve lines");
    repo.write("src/lib.rs", &lines.replace("line 8\n", "eight\n"));
    let folder = repo.path().canonicalize().unwrap();
    // The agent's edit, as Claude Code reports its `Edit`.
    let provider = ScriptedProvider::new().turn([
        Step::Update(AgentUpdate::ToolCall(ToolCall {
            id: ToolCallId("edit-1".into()),
            title: "Edit src/lib.rs".into(),
            name: Some("Edit".into()),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Completed,
            content: vec![ToolCallContent::Diff {
                path: folder.join("src/lib.rs").to_string_lossy().into_owned(),
                old_text: Some("line 8\n".into()),
                new_text: "eight\n".into(),
            }],
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
        })),
        Step::Stop(StopReason::EndTurn),
    ]);
    let daemon = AgentDaemon::with_review(&tmp.path().join("d"), &provider);
    let paths = daemon.paths.clone();
    let mut ui = open_ui_on(folder.clone(), Backing::Agent(daemon), paths, tmp);
    ui.wait_for("the workspace", |ui| {
        ui.cx.read(|cx| {
            crate::workspace::Workspaces::global(cx).is_some_and(|w| w.read(cx).current().is_some())
        })
    });
    let ws = ui.cx.read(|cx| {
        crate::workspace::Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .unwrap()
            .id
            .clone()
    });
    let create = |ui: &mut Ui, title: &str| {
        let (cwd, ws_id, title) = (
            folder.to_string_lossy().into_owned(),
            ws.0.clone(),
            title.to_owned(),
        );
        let session = ui.agent_daemon().with_client(async move |client| {
            client
                .request::<agm::Create>(CreateSessionParams {
                    provider: "fake".into(),
                    cwd,
                    workspace: Some(ws_id),
                    title: Some(title),
                    mode: None,
                    from: None,
                })
                .await
                .unwrap()
                .id
        });
        ui.wait_for("the chat listed", |ui| {
            ui.cx.read(|cx| {
                crate::chat::list::ChatList::global(cx)
                    .is_some_and(|l| l.read(cx).info(&session).is_some())
            })
        });
        let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
        let open = session.clone();
        ui.frame(|window, cx| crate::chat::open_session(dock.downgrade(), open, window, cx));
        ui.wait_for("the chat tab", |ui| {
            chat_tabs(ui).iter().any(|(_, s)| *s == session)
        });
        session
    };

    // "Volume discount" makes the change in its turn.
    let maker = create(&mut ui, "Volume discount");
    let prompted = maker.clone();
    ui.agent_daemon().with_client(async move |client| {
        client
            .request::<agm::Prompt>(PromptParams {
                session: prompted,
                content: vec![ContentBlock::text("Add the volume discount")],
            })
            .await
            .unwrap()
    });
    ui.wait_for("the turn's end", |ui| {
        ui.cx.read(|cx| {
            crate::chat::list::ChatList::global(cx)
                .and_then(|l| l.read(cx).info(&maker).map(|i| i.state))
                == Some(AgentState::Done)
        })
    });
    // Its chat tab is closed; another, newer chat is open.
    let (tab, _) = chat_tabs(&mut ui)
        .into_iter()
        .find(|(_, s)| *s == maker)
        .unwrap();
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    ui.cx
        .update(|cx| dock.update(cx, |d, cx| d.close_tab(tab, cx)));
    let other = create(&mut ui, "Something else");
    assert!(chat_tabs(&mut ui).iter().all(|(_, s)| *s != maker));

    // The diff names who made it.
    let (_tab, review) = open_review(&mut ui, "src/lib.rs");
    ui.wait_for("the provenance", |ui| {
        ui.has(("diff-summary-extra", 0usize))
    });
    assert_eq!(
        ui.cx.read(|cx| review.read(cx).made_by(cx)),
        Some("made by Fake agent in \"Volume discount\"".to_owned())
    );
    let recorded = ui.cx.read(|cx| review.read(cx).changed_by().to_vec());
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].session, maker);

    // A pending comment: the popover defaults to the session that made the
    // change, not the open chat.
    let range = LineRange::lines("src/lib.rs", Side::New, 8, 8);
    ui.frame(|window, cx| {
        review.update(cx, |s, cx| s.open_composer(&range, window, cx));
    });
    ui.type_into(|s| s.composer_input().cloned(), &review, "Spell it **8**.");
    ui.click("thread-composer-add");
    ui.wait_for("the send button", |ui| ui.has("review-send"));
    ui.click("review-send");
    ui.wait_for("the popover", |ui| ui.has("review-send-popover"));
    assert_eq!(
        ui.cx.read(|cx| review.read(cx).send_target().cloned()),
        Some(crate::questions::ask::AskTarget::Session(maker.clone()))
    );
    assert_eq!(
        ui.cx.read(|cx| review.read(cx).send_origin().cloned()),
        Some(maker.clone())
    );
    assert_ne!(maker, other);
}

/// The diff view's state the shortcuts change: the mode, the file shown and
/// each file's viewed mark.
fn diff_state(ui: &mut Ui, view: &Entity<DiffView>) -> (String, Option<String>, Vec<bool>) {
    ui.cx.read(|cx| {
        let v = view.read(cx);
        (
            v.mode(cx).as_str().to_owned(),
            v.selected_file().map(str::to_owned),
            v.files().iter().map(|f| v.is_viewed(&f.path)).collect(),
        )
    })
}

#[test]
fn typing_in_a_comment_never_fires_the_diff_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    repo.write("src/a.rs", &lines);
    repo.write("src/b.rs", &lines);
    repo.commit_all("Two files");
    repo.write("src/a.rs", &lines.replace("line 8\n", "eight\n"));
    repo.write("src/b.rs", &lines.replace("line 2\n", "two\n"));
    let mut ui = open_ui(repo.path().to_owned(), tmp);

    ui.click("sidebar-git");
    ui.wait_for("the change", |ui| ui.has(("git-unstaged-row", 0usize)));
    ui.click(("git-unstaged-row", 0usize));
    ui.wait_for("the review", |ui| {
        ui.diff_tab().is_some_and(|tab| {
            ui.cx.read(|cx| {
                let t = tab.read(cx);
                t.review().is_some()
                    && t.view().read(cx).files().len() == 2
                    && t.view().read(cx).selected_file().is_some()
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
    let shown = ui
        .cx
        .read(|cx| view.read(cx).selected_file().unwrap().to_owned());
    let before = diff_state(&mut ui, &view);
    let (actions, _keys) = record_key_actions(&mut ui.cx);

    // The composer: the text arrives intact, and the view doesn't move.
    let changed = if shown == "src/a.rs" { 8 } else { 2 };
    let range = LineRange::lines(shown.clone(), Side::New, changed, changed);
    ui.frame(|window, cx| {
        review.update(cx, |s, cx| s.open_composer(&range, window, cx));
    });
    let composer = ui
        .cx
        .read(|cx| review.read(cx).composer_input().cloned())
        .unwrap();
    assert!(ui.frame(|window, cx| composer.read(cx).focus_handle(cx).is_focused(window)));
    ui.cx.simulate_keystrokes(ui.window, TYPED_KEYS);
    ui.frame(|_, _| ());
    assert_eq!(ui.cx.read(|cx| composer.read(cx).text(cx)), TYPED_TEXT);
    assert_eq!(diff_state(&mut ui, &view), before);
    // A modifier shortcut still works while typing: select all, then type
    // over it.
    let select_all = if cfg!(target_os = "macos") {
        "cmd-a"
    } else {
        "ctrl-a"
    };
    ui.cx.simulate_keystrokes(ui.window, select_all);
    ui.cx.simulate_keystrokes(ui.window, "x");
    assert_eq!(ui.cx.read(|cx| composer.read(cx).text(cx)), "x");
    ui.frame(|window, cx| composer.update(cx, |i, cx| i.set_text(TYPED_TEXT, window, cx)));
    ui.click("thread-composer-add");
    ui.wait_for("the thread", |ui| ui.threads(&review).len() == 1);
    assert_eq!(ui.threads(&review)[0].comments[0].body, TYPED_TEXT);

    // The reply field, likewise.
    let id = ui.threads(&review)[0].id;
    ui.wait_for("the reply field", |ui| {
        ui.cx.read(|cx| review.read(cx).reply_input(id).is_some())
    });
    let reply = ui
        .cx
        .read(|cx| review.read(cx).reply_input(id).cloned())
        .unwrap();
    ui.frame(|window, cx| {
        let handle = reply.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
    ui.cx.simulate_keystrokes(ui.window, TYPED_KEYS);
    ui.frame(|_, _| ());
    assert_eq!(ui.cx.read(|cx| reply.read(cx).text(cx)), TYPED_TEXT);
    assert_eq!(diff_state(&mut ui, &view), before);
    let fired: Vec<String> = actions
        .borrow()
        .iter()
        .filter(|a| a.starts_with("diff_review::"))
        .cloned()
        .collect();
    assert!(fired.is_empty(), "fired while typing: {fired:?}");

    // With the view itself focused, the same keys are shortcuts again.
    ui.frame(|window, cx| {
        let handle = view.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
    ui.cx.simulate_keystrokes(ui.window, "j k");
    assert_eq!(
        actions
            .borrow()
            .iter()
            .filter(|a| a.starts_with("diff_review::"))
            .collect::<Vec<_>>(),
        ["diff_review::NextHunk", "diff_review::PreviousHunk"]
    );
    ui.cx.simulate_keystrokes(ui.window, "m");
    let mode = ui.cx.read(|cx| view.read(cx).mode(cx).as_str().to_owned());
    assert_ne!(mode, before.0, "m switches the mode");
    ui.cx.simulate_keystrokes(ui.window, "v");
    assert!(
        ui.cx.read(|cx| view.read(cx).is_viewed(&shown)),
        "v marks it viewed"
    );
    let step = if ui.cx.read(|cx| view.read(cx).files()[0].path == shown) {
        "J"
    } else {
        "K"
    };
    ui.cx.simulate_keystrokes(ui.window, step);
    assert_ne!(
        ui.cx
            .read(|cx| view.read(cx).selected_file().map(str::to_owned)),
        Some(shown),
        "{step} shows the other file"
    );
}

// ---- offscreen --------------------------------------------------------------

/// A file tab's review-thread markers (#258): the thread started in the
/// diff on line 8 shows in `src/lib.rs`'s own tab; a click opens its card
/// there with the keyboard in the reply field; a reply there is in the
/// diff's card too; resolved, the marker goes once the card closes, and
/// "Toggle resolved review threads in files" brings it back; an agent's
/// question on the line stacks over it; "Open in diff" shows the diff.
#[test]
fn a_review_thread_shows_in_its_file_tab_and_a_reply_there_is_in_the_diff() {
    use coqo_protocol::methods::{
        editor::LineRange as EditorLines,
        questions::{self as qm, NewAnchor, QuestionKind},
    };
    use coqo_ui::{editor::OverlayAnchor, gpui::ElementId};

    use crate::files::editor::{FileEditor, threads::THREAD_MARKER_BASE};

    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    repo.write("src/lib.rs", &lines);
    repo.commit_all("Twelve lines");
    repo.write("src/lib.rs", &lines.replace("line 8\n", "eight\n"));
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    let (diff, review) = open_review(&mut ui, "src/lib.rs");

    // A thread on line 8, from the diff.
    let range = LineRange::lines("src/lib.rs", Side::New, 8, 8);
    ui.frame(|window, cx| {
        review.update(cx, |s, cx| s.open_composer(&range, window, cx));
    });
    ui.type_into(|s| s.composer_input().cloned(), &review, "Why eight?");
    ui.click("thread-composer-add");
    ui.wait_for("the thread", |ui| ui.threads(&review).len() == 1);
    let t = ui.threads(&review).remove(0);
    let (id, ix) = (t.id, t.id.0 as usize);
    let marker = THREAD_MARKER_BASE + id.0;

    // The file's own tab marks it, on line 8 (0-based 7), pending.
    let (ws, root) = (t.workspace.clone(), t.anchor.root);
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    let tab: Entity<FileEditor> = ui
        .frame(|window, cx| {
            crate::questions::navigate::open_file(&dock, &ws, root, "src/lib.rs", window, cx)
        })
        .unwrap();
    let markers = |ui: &mut Ui| {
        ui.cx.read(|cx| {
            tab.read(cx)
                .editor()
                .map(|code| code.read(cx).marker_lines(cx))
                .unwrap_or_default()
        })
    };
    ui.wait_for("the thread's marker", |ui| {
        markers(ui).contains(&(marker, 7..8))
    });
    let mark = ui.cx.read(|cx| tab.read(cx).thread_marks()[0].clone());
    let sand = ui.cx.read(|cx| {
        use coqo_ui::ActiveTokens as _;
        cx.tokens().palette.sand.hsla()
    });
    assert_eq!((mark.thread, mark.color), (id, sand), "pending: sand");
    assert!(ui.has(ElementId::NamedInteger("code-marker".into(), marker)));

    // A click opens the card in place, the keyboard in its reply field.
    ui.click(ElementId::NamedInteger("code-marker".into(), marker));
    ui.wait_for("the card", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).thread_card().is_some()
                && tab.read(cx).editor().unwrap().read(cx).overlay_anchor()
                    == Some(OverlayAnchor::Marker(marker))
        })
    });
    assert!(ui.has("thread-open-diff"));
    assert!(ui.has(("thread-reply", ix)));
    let card = ui.cx.read(|cx| tab.read(cx).thread_card().unwrap().clone());
    let state = ui.cx.read(|cx| card.read(cx).state().clone());
    let focused = ui.frame(|window, cx| {
        let input = state.read(cx).reply_input(id).unwrap().clone();
        input.read(cx).focus_handle(cx).contains_focused(window, cx)
    });
    assert!(focused, "the keyboard is in the reply field");

    // Reply there: the diff's card has it too (one store).
    ui.type_into(|s| s.reply_input(id).cloned(), &state, "And rename it");
    ui.click(("thread-reply", ix));
    ui.wait_for("the reply", |ui| {
        ui.threads(&review)
            .first()
            .is_some_and(|t| t.comments.len() == 2)
    });
    let reply = ui.threads(&review)[0].comments[1].clone();
    assert_eq!(reply.body, "And rename it");
    assert!(reply.pending);

    // An agent's question on the same line stacks over the thread.
    let params = qm::CreateParams {
        origin: None,
        workspace: ws.clone(),
        session: "s-test".into(),
        kind: QuestionKind::Clarification,
        body: "Is eight right?".into(),
        options: Vec::new(),
        severity: None,
        anchors: vec![NewAnchor {
            root,
            path: "src/lib.rs".into(),
            lines: EditorLines { start: 7, end: 7 },
        }],
    };
    let q = ui
        .full()
        .with_client(async move |client| client.request::<qm::Create>(params).await.unwrap());
    let question = q.id.0 * 16;
    ui.wait_for("the stack", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx)
                .editor()
                .unwrap()
                .read(cx)
                .marker_stacks(cx)
                .contains(&vec![question, marker])
        })
    });

    // Resolve from the card: it stays while open; closed, the resolved
    // thread is hidden until the toggle shows it.
    ui.click(("thread-resolve", ix));
    ui.wait_for("the resolve", |ui| {
        ui.threads(&review)
            .first()
            .is_some_and(|t| t.status == ThreadStatus::Resolved)
    });
    ui.wait_for("the sage marker", |ui| {
        ui.cx.read(|cx| {
            let tab = tab.read(cx);
            tab.thread_marks().len() == 1
                && tab.thread_marks()[0].resolved
                && tab.thread_card().is_some()
        })
    });
    ui.click("thread-card-close");
    ui.wait_for("the marker hidden", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).thread_marks().is_empty() && tab.read(cx).thread_card().is_none()
        })
    });
    let toggle = || Box::new(super::ToggleResolvedThreads);
    ui.frame(|window, cx| window.dispatch_action(toggle(), cx));
    ui.wait_for("the resolved marker shown", |ui| {
        markers(ui).contains(&(marker, 7..8))
    });
    let registered = ui.cx.update(|cx| {
        crate::commands::registry::registry(cx)
            .get("threads.toggle_resolved_in_files")
            .map(|c| c.title.to_string())
    });
    assert_eq!(
        registered.as_deref(),
        Some("Toggle resolved review threads in files")
    );

    // Typing above moves the marker with the code.
    let code = ui.cx.read(|cx| tab.read(cx).editor().cloned()).unwrap();
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.focus(window, cx);
            code.set_selections(std::slice::from_ref(&(0..0)), cx);
            code.type_text("// a note\n", window, cx);
        })
    });
    ui.wait_for("the marker moved", |ui| {
        markers(ui).contains(&(marker, 8..9))
    });

    // Open in diff: the card closes and the diff tab is in front.
    ui.click(ElementId::NamedInteger("code-marker".into(), question));
    ui.wait_for("the question", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).editor().unwrap().read(cx).overlay_anchor()
                == Some(OverlayAnchor::Marker(question))
        })
    });
    // Its face open, a click goes to the next in the stack: the thread.
    ui.click(ElementId::NamedInteger("code-marker".into(), question));
    ui.wait_for("the card again", |ui| {
        ui.cx.read(|cx| tab.read(cx).thread_card().is_some())
    });
    ui.click("thread-open-diff");
    ui.wait_for("the diff in front", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).thread_card().is_none()
                && dock
                    .read(cx)
                    .active_tab()
                    .and_then(|t| dock.read(cx).tab(t)?.downcast::<DiffTab>())
                    == Some(diff.clone())
        })
    });
    assert!(ui.has(("thread", ix)), "the diff shows the thread");
}

/// #311: in a file tab with unsaved edits, select lines 3–5 and press
/// "Add review comment"'s key: the diff's composer opens under them with
/// the keyboard in it, and typing there fires no shortcut. "Add to review"
/// waits for a save; once saved it starts a pending thread on those lines
/// of the work tree, in the root's unstaged diff: a sand marker in the tab
/// with its card open (`Esc` closes it), and the diff's pending count goes
/// up. Alt-click on the ask button opens the composer too; `Esc` cancels.
#[test]
fn a_review_comment_started_in_a_file_tab_is_pending_in_the_diff() {
    use coqo_ui::{
        ActiveTokens as _,
        editor::OverlayAnchor,
        gpui::{ElementId, Modifiers},
    };

    use crate::files::editor::{
        FileEditor,
        threads::{THREAD_MARKER_BASE, UNSAVED_NOTE},
    };

    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("demo-repo"));
    let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    repo.write("src/lib.rs", &lines);
    repo.commit_all("Twelve lines");
    repo.write("src/lib.rs", &lines.replace("line 8\n", "eight\n"));
    let mut ui = open_ui(repo.path().to_owned(), tmp);
    let (diff, review) = open_review(&mut ui, "src/lib.rs");
    let count = |ui: &mut Ui| ui.cx.read(|cx| review.read(cx).pending_count(cx));
    assert_eq!(count(&mut ui), 0);
    assert!(!ui.has("review-pending-count"));
    let registered = ui.cx.update(|cx| {
        crate::commands::registry::registry(cx)
            .get("threads.add_review_comment")
            .map(|c| c.title.to_string())
    });
    assert_eq!(registered.as_deref(), Some("Add review comment"));

    // The file's tab, with an unsaved line at the end.
    let (ws, root) = ui.cx.read(|cx| {
        let current = workspace::Workspaces::global(cx)
            .and_then(|w| w.read(cx).current().cloned())
            .unwrap();
        (current.id.clone(), current.roots[0].id)
    });
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    let tab: Entity<FileEditor> = ui
        .frame(|window, cx| {
            crate::questions::navigate::open_file(&dock, &ws, root, "src/lib.rs", window, cx)
        })
        .unwrap();
    ui.wait_for("the file", |ui| {
        ui.cx.read(|cx| tab.read(cx).editor().is_some())
    });
    let code = ui.cx.read(|cx| tab.read(cx).editor().cloned()).unwrap();
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.focus(window, cx);
            let end = code.text(cx).len();
            code.set_selections(std::slice::from_ref(&(end..end)), cx);
            code.type_text("// unsaved\n", window, cx);
        })
    });
    ui.wait_for("the unsaved edit", |ui| {
        ui.cx.read(|cx| tab.read(cx).is_dirty())
    });

    // Lines 3–5 (0-based 2..5) selected, then the command's key.
    ui.frame(|_, cx| {
        code.update(cx, |code, cx| {
            let selection = code.line_start(2, cx)..code.line_start(5, cx);
            code.set_selections(std::slice::from_ref(&selection), cx);
        })
    });
    let (actions, _keys) = record_key_actions(&mut ui.cx);
    ui.cx.simulate_keystrokes(ui.window, super::ADD_COMMENT_KEY);
    ui.wait_for("the composer", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).thread_composer().is_some()
                && code.read(cx).overlay_anchor() == Some(OverlayAnchor::Lines { start: 2, end: 5 })
        })
    });
    assert!(
        actions
            .borrow()
            .contains(&"threads::AddReviewComment".to_owned())
    );
    assert!(ui.has("thread-file-composer"));
    let state = ui.cx.read(|cx| tab.read(cx).review().unwrap().clone());
    let field = ui
        .cx
        .read(|cx| state.read(cx).composer_input().cloned())
        .unwrap();
    assert!(ui.frame(|window, cx| field.read(cx).focus_handle(cx).is_focused(window)));
    assert_eq!(
        ui.cx
            .read(|cx| state.read(cx).composer_blocked().map(str::to_owned)),
        Some(UNSAVED_NOTE.to_owned())
    );
    assert!(ui.has("thread-composer-blocked"));

    // Typing there fires no shortcut and leaves the code alone.
    actions.borrow_mut().clear();
    let text = ui.cx.read(|cx| code.read(cx).text(cx));
    ui.cx.simulate_keystrokes(ui.window, TYPED_KEYS);
    ui.frame(|_, _| ());
    assert_eq!(ui.cx.read(|cx| field.read(cx).text(cx)), TYPED_TEXT);
    assert_eq!(ui.cx.read(|cx| code.read(cx).text(cx)), text);
    let fired: Vec<String> = actions
        .borrow()
        .iter()
        .filter(|a| {
            [
                "threads::",
                "questions::",
                "file_editor::",
                "walkthrough::",
                "diff_review::",
            ]
            .iter()
            .any(|p| a.starts_with(p))
        })
        .cloned()
        .collect();
    assert!(fired.is_empty(), "fired while typing: {fired:?}");

    // Unsaved: "Add to review" waits. Saving (from the field) lets it.
    ui.click("thread-composer-add");
    ui.cx.run_until_parked();
    assert!(ui.cx.read(|cx| tab.read(cx).thread_composer().is_some()));
    assert_eq!(count(&mut ui), 0);
    let save = if cfg!(target_os = "macos") {
        "cmd-s"
    } else {
        "ctrl-s"
    };
    ui.cx.simulate_keystrokes(ui.window, save);
    ui.wait_for("the save", |ui| {
        ui.cx
            .read(|cx| !tab.read(cx).is_dirty() && state.read(cx).composer_blocked().is_none())
    });
    assert!(!ui.has("thread-composer-blocked"));
    ui.click("thread-composer-add");
    ui.wait_for("the thread", |ui| ui.threads(&review).len() == 1);
    let t = ui.threads(&review).remove(0);
    assert_eq!(t.comments.len(), 1);
    assert_eq!(t.comments[0].body, TYPED_TEXT);
    assert!(t.comments[0].pending);
    assert_eq!(
        (
            t.anchor.root,
            t.anchor.path.as_str(),
            t.anchor.side,
            t.anchor.version.clone(),
            t.anchor.lines,
        ),
        (
            root,
            "src/lib.rs",
            ThreadSide::New,
            DiffSide::Worktree,
            Some(ThreadLines { start: 3, end: 5 }),
        )
    );
    assert_eq!(t.diff, Some(super::unstaged_diff(root)));

    // A sand marker on its lines, its card open in place, the keyboard on
    // the card; `Esc` closes it and the keyboard is back in the code.
    let marker = THREAD_MARKER_BASE + t.id.0;
    ui.wait_for("the marker and card", |ui| {
        ui.cx.read(|cx| {
            code.read(cx).marker_lines(cx).contains(&(marker, 2..5))
                && tab.read(cx).thread_composer().is_none()
                && tab.read(cx).thread_card().is_some()
                && code.read(cx).overlay_anchor() == Some(OverlayAnchor::Marker(marker))
        })
    });
    let sand = ui.cx.read(|cx| cx.tokens().palette.sand.hsla());
    assert_eq!(ui.cx.read(|cx| tab.read(cx).thread_marks()[0].color), sand);
    assert!(ui.has(("thread-pending", t.comments[0].id.0 as usize)));
    let card = ui.cx.read(|cx| tab.read(cx).thread_card().unwrap().clone());
    assert!(ui.frame(|window, cx| card.read(cx).card_focus().is_focused(window)));
    ui.cx.simulate_keystrokes(ui.window, "escape");
    ui.wait_for("the card closed", |ui| {
        ui.cx.read(|cx| tab.read(cx).thread_card().is_none())
    });
    assert!(ui.frame(|window, cx| code.read(cx).focus_handle(cx).contains_focused(window, cx)));

    // The diff counts it and shows it.
    assert_eq!(count(&mut ui), 1);
    ui.frame(|window, cx| {
        window.dispatch_action(
            Box::new(super::OpenReviewDiff {
                workspace: ws.0.clone(),
                diff: super::unstaged_diff(root),
                path: Some("src/lib.rs".into()),
            }),
            cx,
        )
    });
    ui.wait_for("the diff in front", |ui| {
        ui.cx.read(|cx| {
            dock.read(cx)
                .active_tab()
                .and_then(|t| dock.read(cx).tab(t)?.downcast::<DiffTab>())
                == Some(diff.clone())
        })
    });
    ui.wait_for("the thread in the diff", |ui| {
        ui.has(("thread", t.id.0 as usize)) && ui.has("review-pending-count")
    });
    assert!(ui.has("review-send"));

    // Alt-click on the ask button: the composer again, on its line (the
    // cursor's, or the one under the mouse); `Esc` in the field cancels it
    // and adds nothing.
    let tab_again: Entity<FileEditor> = ui
        .frame(|window, cx| {
            crate::questions::navigate::open_file(&dock, &ws, root, "src/lib.rs", window, cx)
        })
        .unwrap();
    assert_eq!(tab_again, tab);
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.focus(window, cx);
            let at = code.line_start(9, cx);
            code.set_selections(std::slice::from_ref(&(at..at)), cx);
        })
    });
    ui.wait_for("the ask button", |ui| ui.has("code-ask"));
    let alt = Modifiers {
        alt: true,
        ..Default::default()
    };
    ui.frame(|window, cx| window.click_with_modifiers("code-ask", alt, cx));
    ui.wait_for("the composer on one line", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).thread_composer().is_some()
                && matches!(
                    code.read(cx).overlay_anchor(),
                    Some(OverlayAnchor::Lines { start, end }) if end == start + 1
                )
        })
    });
    assert!(ui.cx.read(|cx| tab.read(cx).ask_popover().is_none()));
    ui.cx.simulate_keystrokes(ui.window, "escape");
    ui.wait_for("the composer closed", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).thread_composer().is_none() && code.read(cx).overlay_anchor().is_none()
        })
    });
    assert!(ui.frame(|window, cx| code.read(cx).focus_handle(cx).contains_focused(window, cx)));
    assert_eq!(count(&mut ui), 1);
    assert!(!ui.has(ElementId::from("thread-file-composer")));
}

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

/// The overlay over board 4's sample, offscreen, for visual review: the
/// pending review, its send popover, and the agent addressing it (#95).
/// Needs a GPU: `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored threads_screenshots`.
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
    #[derive(Clone, Copy, PartialEq)]
    enum Shot {
        Pending,
        Send,
        Addressing,
    }
    let shots = [
        ("dark", Appearance::Dark, Shot::Pending),
        ("light", Appearance::Light, Shot::Pending),
        ("send-dark", Appearance::Dark, Shot::Send),
        ("send-light", Appearance::Light, Shot::Send),
        ("addressing-dark", Appearance::Dark, Shot::Addressing),
        ("addressing-light", Appearance::Light, Shot::Addressing),
    ];
    for (name, appearance, shot) in shots {
        let image = render_offscreen(appearance, size(1440., 900.), move |window, cx| {
            crate::diff::init(cx);
            let session = coqo_protocol::methods::agents::SessionId("s-1".into());
            let list = crate::chat::list::init(cx);
            list.update(cx, |l, cx| {
                l.upsert(
                    coqo_protocol::methods::agents::SessionInfo {
                        id: session.clone(),
                        provider: "claude-acp".into(),
                        // The list follows the current workspace: none offscreen.
                        workspace: None,
                        cwd: "/work/checkout".into(),
                        title: Some("Volume discount".into()),
                        state: if shot == Shot::Addressing {
                            coqo_protocol::methods::agents::AgentState::Working
                        } else {
                            coqo_protocol::methods::agents::AgentState::Done
                        },
                        state_reason: None,
                        state_since: None,
                        status: None,
                        criteria: Vec::new(),
                        tool_use: Vec::new(),
                        mode: None,
                        closed: false,
                        created_at: 1,
                        updated_at: 1,
                    },
                    cx,
                );
            });
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
                session: session.clone(),
            };
            let addressing = shot == Shot::Addressing;
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
                            (agent.clone(), "Wrapped it in runCatching", false),
                        ],
                    ),
                    sample_thread(
                        2,
                        k,
                        (19, 19),
                        if addressing {
                            ThreadStatus::Resolved
                        } else {
                            ThreadStatus::Open
                        },
                        {
                            let mut c = vec![(
                                Author::User,
                                "Round with `RoundingMode.HALF_EVEN` to 2 decimals before the \
                                 cap. Finance requires banker's rounding on discounts. Please \
                                 also add a test with a 3-decimal result.",
                                !addressing,
                            )];
                            if addressing {
                                c.push((
                                    agent.clone(),
                                    "Rounded with `HALF_EVEN` to 2 decimals before the cap, and \
                                     added `rounds_a_three_decimal_discount`.",
                                    false,
                                ));
                            }
                            c
                        },
                    ),
                    sample_thread(
                        3,
                        java,
                        (3, 3),
                        ThreadStatus::Open,
                        vec![(Author::User, "Null-safe?", !addressing)],
                    ),
                ],
                cx,
            );
            let review = ThreadsOverlay::attach_to(&view, store.clone(), window, cx);
            // The daemon's record of who made the change (#286).
            review.update(cx, |r, cx| {
                r.set_changed_by(
                    vec![coqo_protocol::methods::agents::SessionChange {
                        session: session.clone(),
                        provider: "claude-acp".into(),
                        title: Some("Volume discount".into()),
                        closed: false,
                        paths: vec![k.into(), java.into()],
                        at: 1,
                        strength: coqo_protocol::methods::agents::ChangeStrength::Write,
                    }],
                    cx,
                )
            });
            match shot {
                Shot::Pending => {}
                Shot::Send => review.update(cx, |r, cx| {
                    r.open_send_with(vec![session.clone()], Some(session.clone()), cx)
                }),
                Shot::Addressing => store.update(cx, |s, cx| {
                    s.track(
                        super::SentReview {
                            session: session.clone(),
                            diff: Some("screenshot".into()),
                            count: 2,
                            threads: vec![ThreadId(2), ThreadId(3)],
                            at: i64::MAX,
                        },
                        cx,
                    )
                }),
            }
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

/// Offscreen screenshots of review threads in an ordinary file tab (#258):
/// `src/cart.rs` with a pending thread's card open and an open thread
/// further down, as `coqo-file-threads-{dark,light}.png` in
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn file_threads_screenshots() {
    file_tab_screenshots(false);
}

/// `src/cart.rs` in its file tab with two review threads, and the pending
/// one's card open, or (`composer`) the composer of a new one.
fn file_tab_screenshots(composer: bool) {
    use std::sync::Arc;

    use coqo_ui::{
        CoqoAssets,
        editor::OverlayAnchor,
        gpui::{HeadlessAppContext, platform},
    };

    use crate::{files::editor::threads::THREAD_MARKER_BASE, workspace::Workspaces};

    const BEFORE: &str = "\
use crate::money::Cents;

/// A line of the cart.
pub struct Line {
    pub price: Cents,
    pub quantity: u32,
}

/// The cart's total before tax.
pub fn subtotal(lines: &[Line]) -> Cents {
    lines.iter().map(|l| l.price * l.quantity).sum()
}

/// The total with `discount` (a fraction) taken off.
pub fn total(lines: &[Line], discount: f64) -> Cents {
    let sub = subtotal(lines);
    sub - sub * discount
}
";
    let after = BEFORE
        .replace(
            "    sub - sub * discount\n",
            "    let off = (sub as f64 * discount.clamp(0.0, 0.5)).round() as Cents;\n    sub - off\n",
        )
        .replace(
            "    lines.iter().map(|l| l.price * l.quantity).sum()\n",
            "    lines\n        .iter()\n        .map(|l| l.price * Cents::from(l.quantity))\n        .sum()\n",
        );

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("shop"));
    repo.write("src/cart.rs", BEFORE);
    repo.commit_all("Cart");
    repo.write("src/cart.rs", &after);
    let project = repo.path().canonicalize().unwrap();
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let mut threads: Option<(ThreadId, ThreadId)> = None;
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let mut cx = HeadlessAppContext::with_platform(
            platform::current_platform(true).text_system(),
            Arc::new(CoqoAssets),
            platform::current_headless_renderer,
        );
        cx.allow_parking();
        let (paths, folder) = (daemon.paths.clone(), project.clone());
        let (handle, shell) = cx
            .update(|cx| {
                coqo_ui::init(appearance, cx);
                let mut config = DaemonConfig::at(paths);
                config.spawn = false;
                daemon::init(config, cx);
                workspace::init(Some(folder), cx);
                crate::shell::init(cx);
                coqo_ui::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Point::default(),
                            size: size(px(1440.), px(900.)),
                        })),
                        focus: false,
                        show: false,
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        let store = Rc::new(MemoryLayoutStore::default());
                        cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
                    },
                )
            })
            .expect("open window");
        let settle = |cx: &mut HeadlessAppContext, done: &dyn Fn(&App) -> bool| {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                    .unwrap();
                cx.run_until_parked();
                if cx.update(|cx| done(cx)) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let place = |cx: &App| {
            let current = Workspaces::global(cx)?.read(cx).current()?.clone();
            Some((current.id.clone(), current.roots[0].id))
        };
        settle(&mut cx, &|cx| place(cx).is_some());
        let (ws, root) = cx.update(|cx| place(cx)).unwrap();
        let (pending, _open) = *threads.get_or_insert_with(|| {
            let create = |lines: (u32, u32), body: &str, pending: bool| tm::CreateParams {
                workspace: ws.clone(),
                root,
                path: "src/cart.rs".into(),
                side: ThreadSide::New,
                version: DiffSide::Worktree,
                lines: Some(ThreadLines {
                    start: lines.0,
                    end: lines.1,
                }),
                diff: Some(format!("git:{}:index..worktree", root.0)),
                body: body.into(),
                pending,
            };
            let first = create(
                (11, 14),
                "Can this overflow for a big quantity? `Cents` is an `i64`, but the \
                 product isn't checked.",
                true,
            );
            let second = create(
                (19, 20),
                "Why cap the discount at 50%? Worth a comment, or a constant.",
                false,
            );
            daemon.with_client(async move |client| {
                let a = client.request::<tm::Create>(first).await.unwrap().id;
                let b = client.request::<tm::Create>(second).await.unwrap().id;
                (a, b)
            })
        });
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        let tab = cx
            .update_window(handle, |_, window, cx| {
                crate::questions::navigate::open_file(&dock, &ws, root, "src/cart.rs", window, cx)
            })
            .unwrap()
            .unwrap();
        settle(&mut cx, &|cx| {
            let tab = tab.read(cx);
            tab.thread_marks().len() == 2
                && tab
                    .editor()
                    .is_some_and(|code| code.read(cx).markers().len() == 2)
        });
        let shot = if composer {
            // `struct Line` (lines 4–7) selected, "Add review comment".
            cx.update_window(handle, |_, window, cx| {
                tab.update(cx, |tab, cx| {
                    let code = tab.editor().unwrap().clone();
                    code.update(cx, |code, cx| {
                        let selection = code.line_start(3, cx)..code.line_start(7, cx);
                        code.set_selections(std::slice::from_ref(&selection), cx);
                    });
                    tab.comment_on(3..7, window, cx);
                    let field = tab
                        .review()
                        .and_then(|r| r.read(cx).composer_input().cloned())
                        .unwrap();
                    field.update(cx, |f, cx| {
                        f.set_text(
                            "Could `quantity` be a `NonZeroU32`? A zero line shouldn't be in the cart.",
                            window,
                            cx,
                        )
                    });
                })
            })
            .unwrap();
            settle(&mut cx, &|cx| {
                tab.read(cx).editor().is_some_and(|code| {
                    code.read(cx).overlay_anchor()
                        == Some(OverlayAnchor::Lines { start: 3, end: 7 })
                })
            });
            "comment"
        } else {
            cx.update_window(handle, |_, window, cx| {
                tab.update(cx, |tab, cx| tab.open_thread_card(pending, window, cx))
            })
            .unwrap();
            let marker = THREAD_MARKER_BASE + pending.0;
            settle(&mut cx, &|cx| {
                tab.read(cx).editor().is_some_and(|code| {
                    code.read(cx).overlay_anchor() == Some(OverlayAnchor::Marker(marker))
                })
            });
            "threads"
        };
        for _ in 0..20 {
            settle(&mut cx, &|_| true);
            std::thread::sleep(Duration::from_millis(20));
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-file-{shot}-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}

/// Offscreen screenshots of a review comment started in a file tab (#311):
/// `src/cart.rs` with lines 4–7 selected and the composer open under them,
/// as `coqo-file-comment-{dark,light}.png` in `$COQO_SHOT_DIR`.
#[test]
#[ignore = "needs a GPU; run manually"]
fn file_comment_screenshots() {
    file_tab_screenshots(true);
}
