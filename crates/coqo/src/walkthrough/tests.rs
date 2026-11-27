// SPDX-License-Identifier: Apache-2.0

//! The walkthrough player against a real `coqod` served in-process: the
//! walkthrough is published with `walkthroughs/create` (what the bridge's
//! `publish_walkthrough` sends) for a scripted agent's session.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_agents::{
    AgentUpdate,
    model::{ContentBlock as Block, CreateSessionParams, StopReason},
    testing::{Call, ScriptedProvider, Step as Script},
};
use coqo_client::DaemonPaths;
use coqo_protocol::methods::{
    agents::{self as agm, SessionId},
    walkthroughs::{
        self as wtm, CreateParams, SetStepStateParams, StepState, Walkthrough, WalkthroughId,
        WalkthroughParams,
    },
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::serde_json::{Value, json};
use coqo_ui::{
    Appearance,
    dock::store::MemoryLayoutStore,
    editor::OverlayAnchor,
    gpui::{
        AnyWindowHandle, App, Bounds, ElementId, Entity, Keystroke, Point, Window, WindowBounds,
        WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::{
    player::{Mode, WalkthroughPlayer},
    *,
};
use crate::{
    chat::{tab::ChatTab, transcript::Entry},
    daemon::{self, DaemonConfig},
    files::editor::{FileEditor, STEP_MARKER_BASE},
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{AgentDaemon, TIMEOUT, TestDaemon, write},
    workspace::{self, Workspaces},
};

// ---- pure -------------------------------------------------------------------------

#[test]
fn labels_read_well() {
    assert_eq!(kind_label(StepKind::EntryPoint), "Entry point");
    assert_eq!(severity_label(RiskSeverity::Med), "MEDIUM");
    assert_eq!(steps_list(&[1]), "step 1");
    assert_eq!(steps_list(&[1, 3]), "steps 1, 3");
    assert_eq!(files_label(1), "1 file");
    assert_eq!(files_label(5), "5 files");
    assert_eq!(criterion_label(None), "Not checked");
}

// ---- the fixture --------------------------------------------------------------------

const DISCOUNT: &str = "\
pub fn checkout(units: u32, subtotal: u64) -> u64 {
    let discount = volume_discount(units, subtotal);
    subtotal - discount
}

pub fn volume_discount(units: u32, subtotal: u64) -> u64 {
    if units < 100 {
        return 0;
    }
    (subtotal / 10).min(500)
}
";

const TESTS: &str = "\
#[test]
fn caps_at_500() {
    assert_eq!(super::volume_discount(200, 10_000), 500);
}
";

/// `src/discount.rs` before the change.
const BEFORE: &str = "\
pub fn checkout(units: u32, subtotal: u64) -> u64 {
    subtotal
}
";

/// A walkthrough of the discount: three steps, the second with two
/// anchors.
fn draft() -> Value {
    json!({
        "title": "Volume discount",
        "summary": "Orders of 100 units or more get 10% off, capped at 500.",
        "scope": { "kind": "worktree" },
        "acceptance_criteria": [{ "id": "AC1", "text": "100+ units get 10% off" }],
        "steps": [{
            "id": "entry",
            "title": "Checkout applies the discount",
            "kind": "entry_point",
            "anchors": [{ "path": "src/discount.rs", "start_line": 1, "end_line": 4 }],
            "what": "Checkout asks for the volume discount and takes it off the subtotal.",
            "why": "The rule stays in one function.",
            "risks": [{ "severity": "med", "text": "A discount above the subtotal would underflow." }],
        }, {
            "id": "rule",
            "title": "The threshold and the cap",
            "kind": "logic",
            "anchors": [
                { "path": "src/discount.rs", "start_line": 6, "end_line": 11 },
                { "path": "src/tests.rs", "start_line": 2, "end_line": 4 },
            ],
            "what": "Under 100 units there is no discount; else 10%, at most 500.",
            "why": "AC1 counts units, not lines.",
            "risks": [{ "severity": "high", "text": "No rounding mode." }],
            "criteria_refs": ["AC1"],
            "depends_on": ["entry"],
        }, {
            "id": "tests",
            "title": "The cap is tested",
            "kind": "test",
            "anchors": [{ "path": "src/tests.rs", "start_line": 1, "end_line": 4 }],
            "what": "One test pins the cap.",
            "why": "The cap is the edge case.",
        }],
    })
}

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
}

fn open_ui(project: &Path, paths: &DaemonPaths, appearance: Appearance) -> Ui {
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let config_paths = paths.clone();
    let folder: PathBuf = project.to_path_buf();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(appearance, cx);
        let mut config = DaemonConfig::at(config_paths);
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
    let mut ui = Ui { cx, window, shell };
    ui.wait_for("the workspace", |ui| ui.place().is_some());
    ui
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

    fn place(&mut self) -> Option<(WorkspaceId, RootId)> {
        self.cx.read(|cx| {
            let current = Workspaces::global(cx)?.read(cx).current()?.clone();
            Some((current.id.clone(), current.roots[0].id))
        })
    }

    fn press(&mut self, keys: &'static str) {
        self.frame(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
        });
    }

    fn click(&mut self, id: impl Into<ElementId>) {
        let id = id.into();
        self.frame(|window, cx| window.click(id, cx));
    }

    fn exists(&mut self, id: impl Into<ElementId>) -> bool {
        let id = id.into();
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn dock(&mut self) -> Entity<coqo_ui::dock::Dock> {
        self.cx.read(|cx| self.shell.read(cx).dock().clone())
    }

    /// The player tab of `id`, if open.
    fn player(&mut self, id: &WalkthroughId) -> Option<Entity<WalkthroughPlayer>> {
        let dock = self.dock();
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            let tab = dock.find_tab(player::KIND, &player::key(id), cx)?;
            dock.tab(tab)?.downcast::<WalkthroughPlayer>()
        })
    }

    fn chat(&mut self) -> Option<Entity<ChatTab>> {
        let dock = self.dock();
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter_map(|t| dock.tab(t.id)?.downcast::<ChatTab>())
                .next()
        })
    }

    /// The player's editor of `path`.
    fn editor(
        &mut self,
        player: &Entity<WalkthroughPlayer>,
        path: &str,
    ) -> Option<Entity<FileEditor>> {
        self.cx.read(|cx| player.read(cx).editor(path).cloned())
    }

    /// Where the overlay of the player's editor of `path` is.
    fn overlay(&mut self, player: &Entity<WalkthroughPlayer>, path: &str) -> Option<OverlayAnchor> {
        let editor = self.editor(player, path)?;
        self.cx.read(|cx| {
            let file = editor.read(cx);
            file.shows_step()
                .then(|| file.editor()?.read(cx).overlay_anchor())
                .flatten()
        })
    }

    /// The step labels of the markers in the player's editor of `path`.
    fn step_labels(&mut self, player: &Entity<WalkthroughPlayer>, path: &str) -> Vec<String> {
        let Some(editor) = self.editor(player, path) else {
            return Vec::new();
        };
        self.cx.read(|cx| {
            editor
                .read(cx)
                .editor()
                .map(|code| {
                    code.read(cx)
                        .markers()
                        .into_iter()
                        .filter_map(|m| m.label.map(|l| l.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    fn popover_focused(&mut self, player: &Entity<WalkthroughPlayer>) -> bool {
        let popover = self.cx.read(|cx| player.read(cx).popover().clone());
        self.frame(|window, cx| popover.read(cx).contains_focus(window, cx))
    }

    fn progress(&mut self, player: &Entity<WalkthroughPlayer>) -> usize {
        self.cx
            .read(|cx| player.read(cx).walkthrough().map_or(0, understood_count))
    }
}

fn marker(step: usize, anchor: usize) -> u64 {
    STEP_MARKER_BASE + step as u64 * 16 + anchor as u64
}

fn get(daemon: &AgentDaemon, id: &WalkthroughId) -> Walkthrough {
    let id = id.clone();
    daemon.with_client(async move |client| {
        client
            .request::<wtm::Get>(WalkthroughParams { id })
            .await
            .unwrap()
    })
}

fn say(text: &str) -> [Script; 2] {
    [
        Script::Update(AgentUpdate::AgentMessageChunk(Block::text(text))),
        Script::Stop(StopReason::EndTurn),
    ]
}

/// A chat with the scripted agent in the UI's workspace, open in a tab, and
/// a walkthrough it published.
fn published(ui: &mut Ui, daemon: &AgentDaemon, project: &Path) -> (SessionId, WalkthroughId) {
    let (ws, _) = ui.place().unwrap();
    let cwd = project.to_string_lossy().into_owned();
    let ws_id = ws.0.clone();
    let session = daemon.with_client(async move |client| {
        client
            .request::<agm::Create>(CreateSessionParams {
                provider: "fake".into(),
                cwd,
                workspace: Some(ws_id),
                title: Some("Discount".into()),
                mode: None,
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
    let dock = ui.dock();
    let open = session.clone();
    ui.frame(|window, cx| crate::chat::open_session(dock.downgrade(), open, window, cx));
    ui.wait_for("the chat tab", |ui| ui.chat().is_some());
    let params = CreateParams {
        workspace: ws,
        session: session.0.clone(),
        walkthrough: draft(),
    };
    let id = daemon
        .with_client(async move |client| client.request::<wtm::Create>(params).await.unwrap().id);
    (session, id)
}

fn agent_fixture(provider: &ScriptedProvider) -> (tempfile::TempDir, PathBuf, AgentDaemon) {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("shop");
    write(&project, "src/discount.rs", DISCOUNT);
    write(&project, "src/tests.rs", TESTS);
    let project = project.canonicalize().unwrap();
    let daemon = AgentDaemon::start(tmp.path(), provider);
    (tmp, project, daemon)
}

// ---- the player ---------------------------------------------------------------------

#[test]
fn the_chat_card_opens_the_player_with_the_step_popover_at_the_code() {
    let provider = ScriptedProvider::new().turn(say("Because AC1 counts units."));
    let (_tmp, project, daemon) = agent_fixture(&provider);
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (session, id) = published(&mut ui, &daemon, &project);

    // The card is in the transcript; a click opens the player.
    ui.wait_for("the card", |ui| {
        ui.chat().is_some_and(|chat| {
            ui.cx.read(|cx| {
                chat.read(cx)
                    .transcript()
                    .entries
                    .iter()
                    .any(|e| matches!(e, Entry::Walkthrough(w) if w.id == id))
            })
        })
    });
    let chat = ui.chat().unwrap();
    let card = ui.cx.read(|cx| {
        chat.read(cx)
            .transcript()
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Walkthrough(_)))
            .unwrap()
    });
    ui.wait_for("the card drawn", |ui| ui.exists(("chat-walkthrough", card)));
    ui.click(("chat-walkthrough", card));
    ui.wait_for("the player", |ui| ui.player(&id).is_some());
    let player = ui.player(&id).unwrap();

    // Step 1's popover at its lines, with what, why and the risk, and the
    // keyboard in it.
    ui.wait_for("step 1's popover", |ui| {
        ui.overlay(&player, "src/discount.rs") == Some(OverlayAnchor::Marker(marker(0, 0)))
    });
    assert_eq!(ui.cx.read(|cx| player.read(cx).current()), 0);
    for id in [
        "step-popover",
        "step-title",
        "step-what",
        "step-why",
        "walkthrough-header",
    ] {
        assert!(ui.exists(id), "{id} is drawn");
    }
    assert!(ui.exists(("step-risk", 0usize)));
    assert!(ui.exists(("walkthrough-step", 2usize)), "the step list");
    assert!(ui.popover_focused(&player));
    let data = ui
        .cx
        .read(|cx| player.read(cx).popover().read(cx).data().cloned())
        .unwrap();
    assert_eq!(data.step.title, "Checkout applies the discount");
    assert_eq!(data.total, 3);
    assert_eq!(data.agent, "Fake agent");
    // Numbered markers for both steps in the file.
    assert_eq!(ui.step_labels(&player, "src/discount.rs"), ["1", "2"]);

    // `n` goes to step 2, at its first anchor; the criteria and the step to
    // read first are listed.
    ui.press("n");
    ui.wait_for("step 2's popover", |ui| {
        ui.overlay(&player, "src/discount.rs") == Some(OverlayAnchor::Marker(marker(1, 0)))
    });
    assert!(ui.exists("step-criteria") && ui.exists(("step-depends-on", 0usize)));
    assert!(ui.exists("step-anchor-count"));
    // Its second anchor is in the tests file.
    ui.click("step-anchor-next");
    ui.wait_for("the second anchor", |ui| {
        ui.overlay(&player, "src/tests.rs") == Some(OverlayAnchor::Marker(marker(1, 1)))
    });
    assert_eq!(ui.cx.read(|cx| player.read(cx).anchor()), 1);
    // `p` (← too) goes back.
    ui.press("left");
    ui.wait_for("step 1 again", |ui| {
        ui.cx.read(|cx| player.read(cx).current()) == 0
    });
    ui.press("right");
    ui.wait_for("step 2 again", |ui| {
        ui.cx.read(|cx| player.read(cx).current()) == 1
    });

    // Understood: the progress moves and the daemon keeps it.
    assert_eq!(ui.progress(&player), 0);
    ui.click("step-understood");
    ui.wait_for("the progress", |ui| ui.progress(&player) == 1);
    let deadline = Instant::now() + TIMEOUT;
    while get(&daemon, &id).step_states.get("rule") != Some(&StepState::Understood) {
        assert!(Instant::now() < deadline, "the state was saved");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ui.exists(("walkthrough-step-state", 1usize)));

    // A mark made elsewhere (another window) shows live.
    let mark = SetStepStateParams {
        id: id.clone(),
        step: "tests".into(),
        state: StepState::Understood,
    };
    daemon.with_client(async move |client| {
        client.request::<wtm::SetStepState>(mark).await.unwrap();
    });
    ui.wait_for("the live update", |ui| ui.progress(&player) == 2);

    // A question about the step goes to the agent, pinned to its lines.
    ui.click("step-ask");
    ui.wait_for("the question field", |ui| {
        ui.cx
            .read(|cx| player.read(cx).popover().read(cx).question().is_some())
    });
    let field = ui
        .cx
        .read(|cx| player.read(cx).popover().read(cx).question().cloned())
        .unwrap();
    ui.frame(|window, cx| field.update(cx, |f, cx| f.set_text("Why count units?", window, cx)));
    ui.click("step-question-send");
    ui.wait_for("the question sent", |ui| ui.exists("step-asked"));
    let prompt = provider
        .calls()
        .into_iter()
        .find_map(|c| match c {
            Call::Prompt(content) => Some(content),
            _ => None,
        })
        .expect("the agent was asked");
    let Block::Text { text } = &prompt[0] else {
        panic!("{prompt:?}")
    };
    assert!(text.contains("Why count units?"), "{text}");
    assert!(text.contains("step 2 of 3"), "{text}");
    let Block::Resource { text, .. } = &prompt[1] else {
        panic!("{prompt:?}")
    };
    assert!(text.contains("volume_discount"), "{text}");
    let deadline = Instant::now() + TIMEOUT;
    while get(&daemon, &id).step_states.get("rule") != Some(&StepState::Question) {
        assert!(
            Instant::now() < deadline,
            "the step was marked with a question"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = session;
    drop(ui);
    drop(daemon);
}

#[test]
fn the_session_panel_row_opens_the_player_at_the_first_unread_step() {
    let provider = ScriptedProvider::new();
    let (_tmp, project, daemon) = agent_fixture(&provider);
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (_, id) = published(&mut ui, &daemon, &project);
    let mark = SetStepStateParams {
        id: id.clone(),
        step: "entry".into(),
        state: StepState::Understood,
    };
    daemon.with_client(async move |client| {
        client.request::<wtm::SetStepState>(mark).await.unwrap();
    });
    ui.wait_for("the row", |ui| ui.exists(("chat-walkthrough-row", 0usize)));
    ui.click(("chat-walkthrough-row", 0usize));
    ui.wait_for("the player", |ui| ui.player(&id).is_some());
    let player = ui.player(&id).unwrap();
    ui.wait_for("step 2's popover", |ui| {
        ui.overlay(&player, "src/discount.rs") == Some(OverlayAnchor::Marker(marker(1, 0)))
    });
    assert_eq!(ui.cx.read(|cx| player.read(cx).current()), 1);
    // A click on a step's marker goes there.
    ui.click(ElementId::NamedInteger("code-marker".into(), marker(0, 0)));
    ui.wait_for("step 1", |ui| {
        ui.cx.read(|cx| player.read(cx).current()) == 0
    });
    // And so does a click on the list.
    ui.click(("walkthrough-step", 2usize));
    ui.wait_for("step 3", |ui| {
        ui.overlay(&player, "src/tests.rs") == Some(OverlayAnchor::Marker(marker(2, 0)))
    });
    drop(ui);
    drop(daemon);
}

#[test]
fn diff_mode_shows_the_walkthroughs_change_with_step_markers() {
    use coqo_mod_git::testing::Fixture as Repo;

    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("shop"));
    repo.write("src/discount.rs", BEFORE);
    repo.commit_all("Initial commit");
    repo.write("src/discount.rs", DISCOUNT);
    repo.write("src/tests.rs", TESTS);
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let mut ui = open_ui(repo.path(), &daemon.paths, Appearance::Dark);
    let (ws, _) = ui.place().unwrap();
    let params = CreateParams {
        workspace: ws.clone(),
        session: "s-1".into(),
        walkthrough: draft(),
    };
    let id = daemon
        .with_client(async move |client| client.request::<wtm::Create>(params).await.unwrap().id);
    let dock = ui.dock();
    let open = id.clone();
    ui.frame(|window, cx| super::open(&dock, ws, open, window, cx));
    ui.wait_for("the player", |ui| ui.player(&id).is_some());
    let player = ui.player(&id).unwrap();
    ui.wait_for("step 1", |ui| {
        ui.overlay(&player, "src/discount.rs") == Some(OverlayAnchor::Marker(marker(0, 0)))
    });
    assert!(ui.exists("walkthrough-summary"));

    // The diff: the scope's change, with the steps' markers and the popover
    // under step 1's lines.
    ui.click("walkthrough-mode-diff");
    ui.wait_for("the diff", |ui| {
        ui.cx.read(|cx| {
            player.read(cx).diff().is_some_and(|(view, _)| {
                let v = view.read(cx);
                v.files().len() == 2 && v.file_diff("src/discount.rs").is_some()
            })
        })
    });
    assert_eq!(ui.cx.read(|cx| player.read(cx).mode()), Mode::Diff);
    let (view, overlay) = ui
        .cx
        .read(|cx| player.read(cx).diff().map(|(v, o)| (v.clone(), o.clone())))
        .unwrap();
    let files: Vec<String> = ui.cx.read(|cx| {
        view.read(cx)
            .files()
            .iter()
            .map(|f| f.path.clone())
            .collect()
    });
    assert_eq!(
        files,
        ["src/discount.rs", "src/tests.rs"],
        "untracked files too"
    );
    assert_eq!(
        ui.cx
            .read(|cx| view.read(cx).selected_file().map(str::to_owned)),
        Some("src/discount.rs".to_owned())
    );
    let current: Vec<_> = overlay.marks().into_iter().filter(|m| m.current).collect();
    assert_eq!(current.len(), 1);
    assert_eq!(
        (current[0].step, current[0].start, current[0].end),
        (0, 1, 4)
    );
    ui.wait_for("the step markers and the popover", |ui| {
        ui.exists(ElementId::NamedInteger("walkthrough-diff-step".into(), 0))
            && ui.exists("step-popover")
    });

    // Next: step 3 is in the other file, which the diff shows.
    ui.press("n");
    ui.press("n");
    ui.wait_for("the tests file", |ui| {
        ui.cx
            .read(|cx| view.read(cx).selected_file() == Some("src/tests.rs"))
    });
    assert_eq!(ui.cx.read(|cx| player.read(cx).current()), 2);
    drop(ui);
    drop(daemon);
}

// ---- screenshots ----------------------------------------------------------------------

/// Offscreen screenshots of the player (board 3): step 2 in the editor and
/// in the diff, as `coqo-walkthrough-{dark,light}.png` and
/// `coqo-walkthrough-diff-{dark,light}.png` in `$COQO_SHOT_DIR` (default:
/// the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn walkthrough_screenshots() {
    use std::sync::Arc;

    use coqo_mod_git::testing::Fixture as Repo;
    use coqo_ui::{
        CoqoAssets,
        gpui::{HeadlessAppContext, platform},
    };

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("shop"));
    repo.write("src/discount.rs", BEFORE);
    repo.commit_all("Initial commit");
    repo.write("src/discount.rs", DISCOUNT);
    repo.write("src/tests.rs", TESTS);
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let mut id: Option<WalkthroughId> = None;
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let mut cx = HeadlessAppContext::with_platform(
            platform::current_platform(true).text_system(),
            Arc::new(CoqoAssets),
            platform::current_headless_renderer,
        );
        cx.allow_parking();
        let (paths, folder) = (daemon.paths.clone(), repo.path().to_owned());
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
        let ws = |cx: &App| Some(Workspaces::global(cx)?.read(cx).current()?.id.clone());
        settle(&mut cx, &|cx| ws(cx).is_some());
        let ws = cx.update(|cx| ws(cx)).unwrap();
        let wid = match &id {
            Some(id) => id.clone(),
            None => {
                let params = CreateParams {
                    workspace: ws.clone(),
                    session: "s-shot".into(),
                    walkthrough: draft(),
                };
                let created = daemon.with_client(async move |client| {
                    client.request::<wtm::Create>(params).await.unwrap().id
                });
                let mark = SetStepStateParams {
                    id: created.clone(),
                    step: "entry".into(),
                    state: StepState::Understood,
                };
                daemon.with_client(async move |client| {
                    client.request::<wtm::SetStepState>(mark).await.unwrap();
                });
                id = Some(created.clone());
                created
            }
        };
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        let open = wid.clone();
        cx.update_window(handle, |_, window, cx| {
            super::open(&dock, ws.clone(), open, window, cx);
        })
        .unwrap();
        let player = |cx: &App| {
            let dock = dock.read(cx);
            let tab = dock.find_tab(player::KIND, &player::key(&wid), cx)?;
            dock.tab(tab)?.downcast::<WalkthroughPlayer>()
        };
        settle(&mut cx, &|cx| {
            player(cx).is_some_and(|p| {
                let p = p.read(cx);
                p.current() == 1
                    && p.editor("src/discount.rs")
                        .is_some_and(|e| e.read(cx).shows_step())
            })
        });
        let shoot = |cx: &mut HeadlessAppContext, file: String| {
            for _ in 0..20 {
                settle(cx, &|_| true);
                std::thread::sleep(Duration::from_millis(20));
            }
            let image = cx.capture_screenshot(handle).expect("screenshot");
            let path = dir.join(file);
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        };
        shoot(&mut cx, format!("coqo-walkthrough-{name}.png"));
        let p = cx.update(|cx| player(cx)).unwrap();
        cx.update_window(handle, |_, window, cx| {
            p.update(cx, |p, cx| p.set_mode(Mode::Diff, window, cx))
        })
        .unwrap();
        settle(&mut cx, &|cx| {
            p.read(cx)
                .diff()
                .is_some_and(|(v, _)| v.read(cx).file_diff("src/discount.rs").is_some())
        });
        shoot(&mut cx, format!("coqo-walkthrough-diff-{name}.png"));
    }
}
