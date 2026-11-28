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
        AnyWindowHandle, App, Bounds, ElementId, Entity, Focusable as _, Keystroke, Point, Window,
        WindowBounds, WindowOptions, px, size,
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
    test_support::{
        AgentDaemon, TIMEOUT, TYPED_KEYS, TYPED_TEXT, TestDaemon, record_key_actions, write,
    },
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
    // Not a repository: the banner says coverage was not checked, without
    // asking the agent anything.
    assert!(ui.exists("walkthrough-coverage"));
    assert!(!ui.exists("walkthrough-coverage-ask"));
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
fn publishing_again_refreshes_the_open_player_and_updates_the_card() {
    let provider = ScriptedProvider::new();
    let (_tmp, project, daemon) = agent_fixture(&provider);
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (session, id) = published(&mut ui, &daemon, &project);
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
    ui.wait_for("step 2", |ui| {
        ui.cx.read(|cx| player.read(cx).current()) == 1
    });

    // The agent publishes it again (#253) with a step more and the tests
    // step renamed.
    let mut again = draft();
    again["steps"][2]["title"] = json!("The cap and the threshold are tested");
    again["steps"].as_array_mut().unwrap().push(json!({
        "id": "underflow",
        "title": "No underflow",
        "kind": "edge_case",
        "anchors": [{ "path": "src/discount.rs", "start_line": 3, "end_line": 3 }],
        "what": "The discount is at most the subtotal.",
        "why": "The cap keeps it below.",
    }));
    let params = CreateParams {
        workspace: ui.place().unwrap().0,
        session: session.0.clone(),
        walkthrough: again,
    };
    let w = daemon
        .with_client(async move |client| client.request::<wtm::Create>(params).await.unwrap());
    assert_eq!((&w.id, w.revision), (&id, 1));

    // The open player follows: four steps, still at step 2, the progress
    // on the unchanged step kept.
    ui.wait_for("the player refreshed", |ui| {
        ui.cx.read(|cx| {
            player
                .read(cx)
                .walkthrough()
                .is_some_and(|w| w.steps.len() == 4)
        })
    });
    assert_eq!(ui.cx.read(|cx| player.read(cx).current()), 1);
    assert_eq!(ui.progress(&player), 1);
    assert!(ui.exists(("walkthrough-step", 3usize)));
    let renamed = ui.cx.read(|cx| {
        player.read(cx).walkthrough().unwrap().steps[2]
            .title
            .clone()
    });
    assert_eq!(renamed, "The cap and the threshold are tested");

    // The chat keeps one card, now with four steps, and says it was
    // updated; the session panel lists one walkthrough.
    let chat = ui.chat().unwrap();
    ui.wait_for("the updated line", |ui| {
        ui.cx.read(|cx| {
            chat.read(cx)
                .transcript()
                .entries
                .iter()
                .any(|e| matches!(e, Entry::WalkthroughUpdated(w) if w.id == id))
        })
    });
    let (cards, line, walkthroughs) = ui.cx.read(|cx| {
        let t = chat.read(cx).transcript();
        let cards: Vec<_> = t
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Walkthrough(w) => Some(w.steps),
                _ => None,
            })
            .collect();
        let line = t
            .entries
            .iter()
            .position(|e| matches!(e, Entry::WalkthroughUpdated(_)))
            .unwrap();
        (cards, line, t.walkthroughs.len())
    });
    assert_eq!((cards, walkthroughs), (vec![4], 1));
    // Back to the chat.
    let dock = ui.dock();
    ui.frame(|window, cx| crate::chat::open_session(dock.downgrade(), session, window, cx));
    ui.wait_for("the line drawn", |ui| {
        ui.exists(("chat-walkthrough-updated", line))
    });
    assert!(!ui.exists(("chat-walkthrough-row", 1usize)));
    drop(ui);
    drop(daemon);
}

/// The fixture's project as a git repository: `BEFORE` committed, then the
/// change and two files no step of [`draft`] explains (`README.md`,
/// `src/lib.rs`), with the git mod in the daemon so coverage is checked.
fn git_fixture(provider: &ScriptedProvider) -> (tempfile::TempDir, PathBuf, AgentDaemon) {
    use coqo_mod_git::testing::Fixture as Repo;

    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo::init(&tmp.path().join("shop"));
    repo.write("src/discount.rs", BEFORE);
    repo.commit_all("Initial commit");
    repo.write("src/discount.rs", DISCOUNT);
    repo.write("src/tests.rs", TESTS);
    repo.write(
        "src/lib.rs",
        "pub mod discount;\n#[cfg(test)]\nmod tests;\n",
    );
    repo.write("README.md", "# Shop\n\nVolume discounts.\n");
    let project = repo.path().canonicalize().unwrap();
    let daemon = AgentDaemon::with_review(tmp.path(), provider);
    (tmp, project, daemon)
}

#[test]
fn the_player_shows_the_coverage_banner_with_links_and_the_ask_button() {
    let provider = ScriptedProvider::new().turn(say("I'll add them."));
    let (_tmp, project, daemon) = git_fixture(&provider);
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (session, id) = published(&mut ui, &daemon, &project);
    let (ws, root) = ui.place().unwrap();
    ui.wait_for("the row", |ui| ui.exists(("chat-walkthrough-row", 0usize)));
    ui.click(("chat-walkthrough-row", 0usize));
    ui.wait_for("the player", |ui| ui.player(&id).is_some());
    let player = ui.player(&id).unwrap();

    // Board 3's banner under the header: the counts, each uncovered hunk
    // and the ask button.
    ui.wait_for("the banner", |ui| {
        ui.exists("walkthrough-coverage") && ui.exists(("walkthrough-coverage-hunk", 1usize))
    });
    assert!(ui.exists("walkthrough-coverage-ask"));
    assert!(!ui.exists(("walkthrough-coverage-hunk", 2usize)));
    let paths: Vec<String> = ui.cx.read(|cx| {
        let c = player
            .read(cx)
            .walkthrough()
            .unwrap()
            .coverage
            .clone()
            .unwrap();
        assert_eq!(c.uncovered_count(), 2, "{c:#?}");
        assert_eq!(
            coverage::banner_label(&c).0,
            format!("{} of {} changed hunks covered.", c.total - 2, c.total)
        );
        c.uncovered.iter().map(|h| h.path.clone()).collect()
    });
    let link = |path: &str| {
        let k = paths.iter().position(|p| p == path).unwrap();
        ("walkthrough-coverage-hunk", k)
    };

    // In the editor, a hunk opens its file in a tab of its own.
    ui.click(link("src/lib.rs"));
    let key = crate::files::editor::key(&ws, root, "src/lib.rs");
    let dock = ui.dock();
    ui.wait_for("the hunk's file", |ui| {
        ui.cx.read(|cx| {
            dock.read(cx)
                .find_tab(crate::files::editor::KIND, &key, cx)
                .is_some()
        })
    });

    // In diff mode, it goes to the hunk in the walkthrough's diff.
    let open = id.clone();
    ui.frame(|window, cx| super::open(&dock, ws.clone(), open, None, window, cx));
    ui.cx
        .update_window(ui.window, |_, window, cx| {
            player.update(cx, |p, cx| p.set_mode(Mode::Diff, window, cx))
        })
        .unwrap();
    ui.wait_for("the diff", |ui| {
        ui.cx.read(|cx| {
            player
                .read(cx)
                .diff()
                .is_some_and(|(v, _)| !v.read(cx).files().is_empty())
        })
    });
    ui.click(link("README.md"));
    ui.wait_for("the hunk in the diff", |ui| {
        ui.cx.read(|cx| {
            player
                .read(cx)
                .diff()
                .is_some_and(|(v, _)| v.read(cx).selected_file() == Some("README.md"))
        })
    });

    // "Ask Fake agent to explain it" asks the publishing session in its
    // chat, as the chat card's button does.
    assert!(ui.exists("walkthrough-coverage-ask"));
    ui.click("walkthrough-coverage-ask");
    let chat = ui.chat().unwrap();
    assert_eq!(ui.cx.read(|cx| chat.read(cx).session().clone()), session);
    ui.wait_for("the request sent", |ui| {
        ui.cx.read(|cx| {
            chat.read(cx).transcript().entries.iter().any(|e| {
                matches!(e, Entry::User { content } if format!("{content:?}").contains("Your walkthrough \\\"Volume discount\\\""))
            })
        })
    });
    let sent = ui.cx.read(|cx| {
        format!(
            "{:?}",
            chat.read(cx)
                .transcript()
                .entries
                .iter()
                .find(|e| matches!(e, Entry::User { .. }))
                .unwrap()
        )
    });
    assert!(sent.contains("README.md:1-3 (new file, 3 lines)"), "{sent}");
    assert!(
        sent.contains("src/lib.rs:1-3 (new file, 3 lines)"),
        "{sent}"
    );

    // Explained (published again): the banner goes, live.
    let mut fixed = draft();
    fixed["trivial_hunks"] = json!([
        { "path": "README.md", "reason": "docs" },
        { "path": "src/lib.rs", "reason": "module list" },
    ]);
    let params = CreateParams {
        workspace: ws.clone(),
        session: session.0.clone(),
        walkthrough: fixed,
    };
    let w = daemon
        .with_client(async move |client| client.request::<wtm::Create>(params).await.unwrap());
    assert!(w.coverage.unwrap().is_complete());
    let open = id.clone();
    ui.frame(|window, cx| super::open(&dock, ws.clone(), open, None, window, cx));
    ui.wait_for("the banner gone", |ui| {
        ui.exists("walkthrough-header") && !ui.exists("walkthrough-coverage")
    });
    drop(ui);
    drop(daemon);
}

#[test]
fn typing_a_question_never_fires_the_player_keys() {
    let provider = ScriptedProvider::new();
    let (_tmp, project, daemon) = agent_fixture(&provider);
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (_, id) = published(&mut ui, &daemon, &project);
    ui.wait_for("the row", |ui| ui.exists(("chat-walkthrough-row", 0usize)));
    ui.click(("chat-walkthrough-row", 0usize));
    ui.wait_for("the player", |ui| ui.player(&id).is_some());
    let player = ui.player(&id).unwrap();
    ui.wait_for("step 1's popover", |ui| {
        ui.overlay(&player, "src/discount.rs") == Some(OverlayAnchor::Marker(marker(0, 0)))
    });
    let state = |ui: &mut Ui| {
        let progress = ui.progress(&player);
        ui.cx.read(|cx| {
            let p = player.read(cx);
            (p.current(), p.anchor(), p.mode(), progress)
        })
    };
    let before = state(&mut ui);
    let (actions, _keys) = record_key_actions(&mut ui.cx);

    // "Ask about this step": the text arrives intact; the step, its
    // understood mark and the mode stay.
    ui.click("step-ask");
    ui.wait_for("the question field", |ui| {
        ui.cx
            .read(|cx| player.read(cx).popover().read(cx).question().is_some())
    });
    let field = ui
        .cx
        .read(|cx| player.read(cx).popover().read(cx).question().cloned())
        .unwrap();
    ui.frame(|window, cx| {
        let handle = field.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
    let window = ui.window;
    ui.cx.simulate_keystrokes(window, TYPED_KEYS);
    ui.cx.simulate_keystrokes(window, "left right");
    ui.frame(|_, _| ());
    assert_eq!(ui.cx.read(|cx| field.read(cx).text(cx)), TYPED_TEXT);
    assert_eq!(state(&mut ui), before);
    let fired: Vec<String> = actions
        .borrow()
        .iter()
        .filter(|a| a.starts_with("walkthrough::"))
        .cloned()
        .collect();
    assert!(fired.is_empty(), "fired while typing: {fired:?}");

    // With the popover focused, they are the player's keys again.
    ui.frame(|window, cx| {
        let handle = player.read(cx).popover().read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
    ui.cx.simulate_keystrokes(window, "u");
    ui.wait_for("the understood mark", |ui| ui.progress(&player) == 1);
    ui.cx.simulate_keystrokes(window, "n");
    ui.wait_for("step 2", |ui| {
        ui.cx.read(|cx| player.read(cx).current()) == 1
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
    ui.frame(|window, cx| super::open(&dock, ws, open, None, window, cx));
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

// ---- in ordinary file tabs (#260) ---------------------------------------------------

impl Ui {
    /// Open `path` in a file tab of its own and wait for its editor.
    fn open_file(&mut self, path: &str) -> Entity<FileEditor> {
        let (ws, root) = self.place().unwrap();
        let dock = self.dock();
        let path = path.to_owned();
        let tab = self
            .frame(|window, cx| {
                crate::questions::navigate::open_file(&dock, &ws, root, &path, window, cx)
            })
            .unwrap();
        self.wait_for("the editor", |ui| {
            ui.cx.read(|cx| tab.read(cx).editor().is_some())
        });
        tab
    }

    /// The file tab's walkthrough steps: (label, marker id).
    fn file_steps(&mut self, tab: &Entity<FileEditor>) -> Vec<(String, u64)> {
        self.cx.read(|cx| {
            tab.read(cx)
                .file_steps()
                .iter()
                .map(|m| (m.label.to_string(), m.id))
                .collect()
        })
    }

    /// The file tab's step popover's step and walkthrough title.
    fn file_step(&mut self, tab: &Entity<FileEditor>) -> Option<(usize, String)> {
        self.cx.read(|cx| {
            let popover = tab.read(cx).file_step_popover()?;
            let data = popover.read(cx).data()?;
            Some((data.ix, data.walkthrough.clone()))
        })
    }

    fn file_overlay(&mut self, tab: &Entity<FileEditor>) -> Option<OverlayAnchor> {
        self.cx
            .read(|cx| tab.read(cx).editor()?.read(cx).overlay_anchor())
    }

    fn file_markers(&mut self, tab: &Entity<FileEditor>) -> Vec<(u64, std::ops::Range<usize>)> {
        self.cx.read(|cx| {
            tab.read(cx)
                .editor()
                .map(|code| code.read(cx).marker_lines(cx))
                .unwrap_or_default()
        })
    }
}

fn mark(daemon: &AgentDaemon, id: &WalkthroughId, step: &str, state: StepState) {
    let mark = SetStepStateParams {
        id: id.clone(),
        step: step.into(),
        state,
    };
    daemon.with_client(async move |client| {
        client.request::<wtm::SetStepState>(mark).await.unwrap();
    });
}

fn wait_state(daemon: &AgentDaemon, id: &WalkthroughId, step: &str, state: StepState) {
    let deadline = Instant::now() + TIMEOUT;
    while get(daemon, id).step_states.get(step) != Some(&state) {
        assert!(Instant::now() < deadline, "{step} was marked {state:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_file_tab_shows_the_steps_and_opens_the_popover_in_place() {
    let provider = ScriptedProvider::new();
    let (_tmp, project, daemon) = agent_fixture(&provider);
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (_, id) = published(&mut ui, &daemon, &project);

    // No player: the file's own tab shows both steps anchored in it.
    let tab = ui.open_file("src/discount.rs");
    ui.wait_for("the step badges", |ui| ui.file_steps(&tab).len() == 2);
    let steps = ui.file_steps(&tab);
    assert_eq!(
        steps.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
        ["1", "2"]
    );
    let (first, second) = (steps[0].1, steps[1].1);
    assert_eq!(
        ui.file_markers(&tab),
        vec![(first, 0..4), (second, 5..11)],
        "at the anchored lines"
    );
    assert!(ui.exists(ElementId::NamedInteger("code-marker".into(), first)));

    // A click opens step 1's popover there, with the keyboard in it.
    ui.click(ElementId::NamedInteger("code-marker".into(), first));
    ui.wait_for("step 1's popover", |ui| {
        ui.file_overlay(&tab) == Some(OverlayAnchor::Marker(first))
    });
    assert_eq!(ui.file_step(&tab), Some((0, "Volume discount".to_owned())));
    for id in ["step-popover", "step-what", "step-why", "step-open-player"] {
        assert!(ui.exists(id), "{id} is drawn");
    }
    assert!(!ui.exists("step-next"), "Next is the player's");
    let popover = ui
        .cx
        .read(|cx| tab.read(cx).file_step_popover().cloned())
        .unwrap();
    assert!(ui.frame(|window, cx| popover.read(cx).contains_focus(window, cx)));

    // Understood: the daemon keeps it, and the popover says so.
    ui.click("step-understood");
    wait_state(&daemon, &id, "entry", StepState::Understood);
    ui.wait_for("the popover's state", |ui| {
        ui.cx.read(|cx| {
            popover
                .read(cx)
                .data()
                .is_some_and(|d| d.state == StepState::Understood)
        })
    });

    // Esc closes it; the badges stay.
    ui.press("escape");
    ui.wait_for("the popover closed", |ui| ui.file_overlay(&tab).is_none());
    assert_eq!(ui.file_steps(&tab).len(), 2);

    // A mark made elsewhere shows live in the popover.
    mark(&daemon, &id, "rule", StepState::Understood);
    ui.click(ElementId::NamedInteger("code-marker".into(), second));
    ui.wait_for("step 2's popover, understood", |ui| {
        ui.file_step(&tab).is_some_and(|(ix, _)| ix == 1)
            && ui.cx.read(|cx| {
                popover
                    .read(cx)
                    .data()
                    .is_some_and(|d| d.state == StepState::Understood)
            })
    });

    // Typing above step 2 moves its badge with the code.
    let code = ui.cx.read(|cx| tab.read(cx).editor().cloned()).unwrap();
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.focus(window, cx);
            code.set_selections(std::slice::from_ref(&(0..0)), cx);
            code.type_text("// a note\n", window, cx);
        })
    });
    ui.wait_for("the badges moved", |ui| {
        ui.file_markers(&tab) == vec![(first, 1..5), (second, 6..12)]
    });

    // Open in player: the player at that step, and the popover closes.
    ui.click(ElementId::NamedInteger("code-marker".into(), second));
    ui.wait_for("step 2's popover", |ui| {
        ui.file_overlay(&tab) == Some(OverlayAnchor::Marker(second))
    });
    ui.click("step-open-player");
    ui.wait_for("the player at step 2", |ui| {
        ui.player(&id)
            .is_some_and(|p| ui.cx.read(|cx| p.read(cx).current()) == 1)
    });
    assert!(ui.file_overlay(&tab).is_none());

    // The last step understood: done, but the player still shows it.
    mark(&daemon, &id, "tests", StepState::Understood);
    let ws = ui.place().unwrap().0;
    let store = ui.cx.update(|cx| marks::walkthroughs_for(&ws, cx)).unwrap();
    ui.wait_for("the summary", |ui| {
        ui.cx.read(|cx| {
            store
                .read(cx)
                .walkthrough(&id)
                .is_some_and(|w| understood_count(w) == 3)
        })
    });
    assert_eq!(ui.file_steps(&tab).len(), 2, "held by the player");
    // Closed, a finished walkthrough's markers go.
    let dock = ui.dock();
    let player_tab = ui
        .cx
        .read(|cx| dock.read(cx).find_tab(player::KIND, &player::key(&id), cx))
        .unwrap();
    ui.frame(|_, cx| dock.update(cx, |dock, cx| dock.close_tab(player_tab, cx)));
    ui.wait_for("the markers gone", |ui| ui.file_steps(&tab).is_empty());
    drop(ui);
    drop(daemon);
}

#[test]
fn the_toggle_hides_the_markers_and_stacked_markers_cycle() {
    use coqo_protocol::methods::{
        editor::LineRange,
        questions::{self as qm, NewAnchor, QuestionKind},
    };

    let provider = ScriptedProvider::new();
    let (_tmp, project, daemon) = agent_fixture(&provider);
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (session, _) = published(&mut ui, &daemon, &project);
    let tab = ui.open_file("src/discount.rs");
    ui.wait_for("the step badges", |ui| ui.file_steps(&tab).len() == 2);
    let step = ui.file_steps(&tab)[0].1;

    // An agent's question on step 1's first line: it shows over the step.
    let (ws, root) = ui.place().unwrap();
    let params = qm::CreateParams {
        workspace: ws.clone(),
        session: session.0.clone(),
        kind: QuestionKind::Clarification,
        body: "Should the discount round?".into(),
        options: Vec::new(),
        severity: None,
        anchors: vec![NewAnchor {
            root,
            path: "src/discount.rs".into(),
            lines: LineRange { start: 0, end: 1 },
        }],
    };
    let q =
        daemon.with_client(async move |client| client.request::<qm::Create>(params).await.unwrap());
    let question = q.id.0 * 16;
    ui.wait_for("the stack", |ui| {
        ui.cx.read(|cx| {
            tab.read(cx).editor().is_some_and(|code| {
                code.read(cx).marker_stacks(cx).first() == Some(&vec![question, step])
            })
        })
    });
    assert!(ui.exists(ElementId::NamedInteger("code-marker".into(), question)));
    assert!(!ui.exists(ElementId::NamedInteger("code-marker".into(), step)));
    assert!(ui.exists(ElementId::NamedInteger("code-marker-more".into(), question)));
    // A click opens the question; another, the step under it; a third, the
    // question again.
    ui.click(ElementId::NamedInteger("code-marker".into(), question));
    ui.wait_for("the question", |ui| {
        ui.file_overlay(&tab) == Some(OverlayAnchor::Marker(question))
    });
    ui.click(ElementId::NamedInteger("code-marker".into(), question));
    ui.wait_for("the step", |ui| {
        ui.file_overlay(&tab) == Some(OverlayAnchor::Marker(step)) && ui.file_step(&tab).is_some()
    });
    ui.click(ElementId::NamedInteger("code-marker".into(), step));
    ui.wait_for("the question again", |ui| {
        ui.file_overlay(&tab) == Some(OverlayAnchor::Marker(question))
    });
    ui.press("escape");
    ui.wait_for("closed", |ui| ui.file_overlay(&tab).is_none());

    // The palette's "Toggle walkthrough markers" hides them, and shows them
    // again.
    let toggle = |ui: &mut Ui| {
        ui.frame(|window, cx| window.dispatch_action(Box::new(ToggleWalkthroughMarkers), cx))
    };
    let registered = ui.cx.update(|cx| {
        crate::commands::registry::registry(cx)
            .get("walkthrough.toggle_markers")
            .map(|c| c.title.to_string())
    });
    assert_eq!(registered.as_deref(), Some("Toggle walkthrough markers"));
    toggle(&mut ui);
    ui.wait_for("hidden", |ui| ui.file_steps(&tab).is_empty());
    let saved = ui.cx.read(|cx| {
        Workspaces::global(cx).and_then(|w| w.read(cx).state(marks::SHOWN_KEY).cloned())
    });
    assert_eq!(saved, Some(Value::Bool(false)), "kept with the workspace");
    toggle(&mut ui);
    ui.wait_for("shown", |ui| ui.file_steps(&tab).len() == 2);

    drop(ui);
    drop(daemon);
}

/// Renamed (#288), a file keeps its steps: the daemon moves the anchors
/// and the tab follows the file.
#[test]
fn a_file_tabs_steps_follow_a_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("shop");
    write(&project, "src/discount.rs", DISCOUNT);
    write(&project, "src/tests.rs", TESTS);
    let project = project.canonicalize().unwrap();
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let mut ui = open_ui(&project, &daemon.paths, Appearance::Dark);
    let (ws, root) = ui.place().unwrap();
    let params = CreateParams {
        workspace: ws.clone(),
        session: "s-rename".into(),
        walkthrough: draft(),
    };
    daemon.with_client(async move |client| {
        client.request::<wtm::Create>(params).await.unwrap();
    });
    let tab = ui.open_file("src/discount.rs");
    ui.wait_for("the step badges", |ui| ui.file_steps(&tab).len() == 2);
    let step = ui.file_steps(&tab)[0].1;

    let rename = coqo_protocol::methods::fs::RenameParams {
        workspace: ws.clone(),
        root,
        from: "src/discount.rs".into(),
        to: "src/pricing.rs".into(),
    };
    daemon.with_client(async move |client| {
        client
            .request::<coqo_protocol::methods::fs::Rename>(rename)
            .await
            .unwrap();
    });
    // What the tree does after a rename.
    let moved = crate::files::ops::FileEvent::Moved {
        workspace: ws.clone(),
        root,
        from: "src/discount.rs".into(),
        to: "src/pricing.rs".into(),
    };
    ui.cx
        .update(|cx| crate::files::ops::FileEvents::announce(moved, cx));
    ui.wait_for("the steps on the renamed file", |ui| {
        ui.cx.read(|cx| tab.read(cx).path() == "src/pricing.rs")
            && ui.file_steps(&tab).len() == 2
            && ui
                .file_markers(&tab)
                .iter()
                .any(|(id, lines)| *id == step && *lines == (0..4))
    });
    drop(ui);
    drop(daemon);
}

// ---- screenshots ----------------------------------------------------------------------

/// Offscreen screenshots of the player (board 3): step 2 in the editor and
/// in the diff, with the coverage banner (no step explains `src/lib.rs`),
/// as `coqo-walkthrough-{dark,light}.png` and
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
    repo.write(
        "src/lib.rs",
        "pub mod discount;\n#[cfg(test)]\nmod tests;\n",
    );
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
            super::open(&dock, ws.clone(), open, None, window, cx);
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

/// Offscreen screenshots of a walkthrough's steps in an ordinary file tab
/// (#260): `src/discount.rs` with step 2's popover open and an agent's
/// question stacked over step 1, as `coqo-file-steps-{dark,light}.png` in
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn file_steps_screenshots() {
    use std::sync::Arc;

    use coqo_protocol::methods::{
        editor::LineRange,
        questions::{self as qm, NewAnchor, QuestionKind},
    };
    use coqo_ui::{
        CoqoAssets,
        gpui::{HeadlessAppContext, platform},
    };

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("shop");
    write(&project, "src/discount.rs", DISCOUNT);
    write(&project, "src/tests.rs", TESTS);
    let project = project.canonicalize().unwrap();
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let mut made = false;
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
        if !made {
            made = true;
            let params = CreateParams {
                workspace: ws.clone(),
                session: "s-shot".into(),
                walkthrough: draft(),
            };
            let created = daemon.with_client(async move |client| {
                client.request::<wtm::Create>(params).await.unwrap().id
            });
            let mark = SetStepStateParams {
                id: created,
                step: "entry".into(),
                state: StepState::Understood,
            };
            daemon.with_client(async move |client| {
                client.request::<wtm::SetStepState>(mark).await.unwrap();
            });
            let ask = qm::CreateParams {
                workspace: ws.clone(),
                session: "s-shot".into(),
                kind: QuestionKind::Clarification,
                body: "Should checkout round the discount?".into(),
                options: Vec::new(),
                severity: None,
                anchors: vec![NewAnchor {
                    root,
                    path: "src/discount.rs".into(),
                    lines: LineRange { start: 0, end: 0 },
                }],
            };
            daemon.with_client(async move |client| {
                client.request::<qm::Create>(ask).await.unwrap();
            });
        }
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        let tab = cx
            .update_window(handle, |_, window, cx| {
                crate::questions::navigate::open_file(
                    &dock,
                    &ws,
                    root,
                    "src/discount.rs",
                    window,
                    cx,
                )
            })
            .unwrap()
            .unwrap();
        settle(&mut cx, &|cx| {
            let tab = tab.read(cx);
            tab.file_steps().len() == 2
                && tab
                    .editor()
                    .is_some_and(|code| code.read(cx).markers().len() == 3)
        });
        let step = cx.update(|cx| tab.read(cx).file_steps()[1].clone());
        cx.update_window(handle, |_, window, cx| {
            tab.update(cx, |tab, cx| tab.open_file_step(&step, window, cx))
        })
        .unwrap();
        settle(&mut cx, &|cx| {
            tab.read(cx).editor().is_some_and(|code| {
                code.read(cx).overlay_anchor() == Some(OverlayAnchor::Marker(step.id))
            })
        });
        for _ in 0..20 {
            settle(&mut cx, &|_| true);
            std::thread::sleep(Duration::from_millis(20));
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-file-steps-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
