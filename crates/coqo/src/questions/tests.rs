// SPDX-License-Identifier: Apache-2.0

//! The question UI against a real `coqod` served in-process: questions are
//! asked with `questions/create` (what the bridge's `ask_question` sends) and
//! an agent's wait is a `questions/watch` subscription, as the bridge's.

use std::{
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_client::connect;
use coqo_protocol::{
    Client, ClientConfig,
    methods::{
        agents::SessionUpdate,
        editor::LineRange,
        questions::{CreateParams, NewAnchor, NewOption, WatchParams},
    },
};
use coqo_ui::{
    Appearance,
    dock::{AnyTab, store::MemoryLayoutStore},
    editor::{CodeEditor, OverlayAnchor},
    gpui::{
        AnyWindowHandle, App, Bounds, ElementId, Entity, Keystroke, Point, Window, WindowBounds,
        WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use futures_util::StreamExt as _;

use coqo_ui::gpui::Focusable as _;

use super::{popover::QuestionPopover, *};
use crate::{
    chat::{tab::ChatTab, transcript::Entry},
    daemon::{self, DaemonConfig},
    files::editor::FileEditor,
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{Fixture, TIMEOUT, write},
    workspace::{self, Workspaces},
};

// ---- pure ---------------------------------------------------------------------

fn nav(id: u64, path: &str, line: u32) -> NavItem {
    NavItem {
        id: QuestionId(id),
        root: RootId(0),
        path: path.into(),
        line,
    }
}

#[test]
fn f8_steps_through_files_and_lines_and_wraps() {
    let items = [nav(3, "a.rs", 4), nav(1, "a.rs", 10), nav(2, "b.rs", 0)];
    // From an open popover: the next and previous one, wrapping.
    assert_eq!(
        step(&items, &NavFrom::Question(QuestionId(1)), true),
        Some(QuestionId(2))
    );
    assert_eq!(
        step(&items, &NavFrom::Question(QuestionId(2)), true),
        Some(QuestionId(3))
    );
    assert_eq!(
        step(&items, &NavFrom::Question(QuestionId(3)), false),
        Some(QuestionId(2))
    );
    // From a cursor: the first after it, or the last before it.
    let here = NavFrom::Place(RootId(0), "a.rs".into(), 5);
    assert_eq!(step(&items, &here, true), Some(QuestionId(1)));
    assert_eq!(step(&items, &here, false), Some(QuestionId(3)));
    let end = NavFrom::Place(RootId(0), "z.rs".into(), 0);
    assert_eq!(step(&items, &end, true), Some(QuestionId(3)), "wraps");
    assert_eq!(step(&items, &NavFrom::Nowhere, false), Some(QuestionId(2)));
    assert_eq!(step(&[], &NavFrom::Nowhere, true), None);
}

#[test]
fn whole_line_spans_end_at_the_next_line() {
    let point = |line, col, offset| qm::AnchorPoint { line, col, offset };
    let span = |a, b| qm::AnchorSpan { start: a, end: b };
    // Lines 2–3: from the start of line 2 to the start of line 4.
    assert_eq!(span_lines(&span(point(2, 0, 10), point(4, 0, 30))), 2..4);
    // The last line, without a newline.
    assert_eq!(span_lines(&span(point(5, 0, 40), point(5, 9, 49))), 5..6);
    // Empty: its line.
    assert_eq!(span_lines(&span(point(1, 3, 7), point(1, 3, 7))), 1..2);
}

// ---- against coqod ----------------------------------------------------------------

const CART: &str = "\
pub struct Cart {
    lines: Vec<Line>,
}

pub fn total(cart: &Cart) -> u64 {
    let sum = cart.lines.iter().map(|l| l.price).sum();
    sum.min(500)
}
";

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    /// An agent's connection (the bridge's).
    agent: Client,
}

fn open_ui(fixture: &Fixture) -> Ui {
    open_ui_at(
        &fixture.project,
        &fixture.daemon.paths,
        &fixture.daemon.runtime,
    )
}

/// The workbench on `project` against the daemon at `paths`.
fn open_ui_at(
    project: &std::path::Path,
    paths: &coqo_client::DaemonPaths,
    runtime: &tokio::runtime::Runtime,
) -> Ui {
    write(project, "src/cart.rs", CART);
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let config_paths = paths.clone();
    let folder: PathBuf = project.to_path_buf();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
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
    let paths = paths.clone();
    let (agent, _) = runtime
        .block_on(connect(&paths, ClientConfig::new("coqo-tests", "agent")))
        .unwrap();
    let mut ui = Ui {
        cx,
        window,
        shell,
        agent,
    };
    ui.wait_for("the workspace", |ui| ui.place().is_some());
    ui
}

/// A workspace and root.
type Place = (WorkspaceId, RootId);

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

    fn place(&mut self) -> Option<Place> {
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

    /// Open `path` in a file tab and wait for its editor.
    fn open_file(&mut self, path: &str) -> Entity<FileEditor> {
        let (ws, root) = self.place().unwrap();
        let dock = self.dock();
        let path = path.to_owned();
        let tab = self
            .cx
            .update_window(self.window, |_, window, cx| {
                navigate::open_file(&dock, &ws, root, &path, window, cx)
            })
            .unwrap()
            .unwrap();
        self.wait_for("the editor", |ui| ui.code(&tab).is_some());
        tab
    }

    /// The file tab of `path`, if open.
    fn file_tab(&mut self, path: &str) -> Option<Entity<FileEditor>> {
        let dock = self.dock();
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter_map(|t| dock.tab(t.id)?.downcast::<FileEditor>())
                .find(|f| f.read(cx).path() == path)
        })
    }

    fn code(&mut self, tab: &Entity<FileEditor>) -> Option<Entity<CodeEditor>> {
        self.cx.read(|cx| tab.read(cx).editor().cloned())
    }

    fn marker_lines(&mut self, tab: &Entity<FileEditor>) -> Vec<(u64, std::ops::Range<usize>)> {
        let code = self.code(tab).unwrap();
        self.cx.read(|cx| code.read(cx).marker_lines(cx))
    }

    fn popover(&mut self, tab: &Entity<FileEditor>) -> Option<Entity<QuestionPopover>> {
        self.cx.read(|cx| tab.read(cx).popover().cloned())
    }

    fn popover_focused(&mut self, tab: &Entity<FileEditor>) -> bool {
        let Some(popover) = self.popover(tab) else {
            return false;
        };
        self.frame(|window, cx| popover.read(cx).contains_focus(window, cx))
    }

    fn overlay(&mut self, tab: &Entity<FileEditor>) -> Option<OverlayAnchor> {
        let code = self.code(tab).unwrap();
        self.cx.read(|cx| code.read(cx).overlay_anchor())
    }

    /// The editor's markers.
    fn markers(&mut self, tab: &Entity<FileEditor>) -> Vec<coqo_ui::editor::CodeMarker> {
        let code = self.code(tab).unwrap();
        self.cx.read(|cx| code.read(cx).markers())
    }

    fn store(&mut self) -> Entity<WorkspaceQuestions> {
        let (ws, _) = self.place().unwrap();
        self.cx.update(|cx| questions_for(&ws, cx)).unwrap()
    }

    fn focus_code(&mut self, tab: &Entity<FileEditor>) {
        let code = self.code(tab).unwrap();
        self.frame(|window, cx| code.update(cx, |code, cx| code.focus(window, cx)));
    }
}

/// The marker id of a question's first anchor.
fn marker_of(q: &Question) -> u64 {
    q.id.0 * 16
}

/// Ask a question in the UI's workspace on `lines` (0-based, inclusive) of
/// `path`, as an agent's `ask_question` does.
fn ask(
    fixture: &Fixture,
    ui: &mut Ui,
    kind: QuestionKind,
    body: &str,
    options: &[&str],
    path: &str,
    lines: (u32, u32),
) -> Question {
    let (ws, root) = ui.place().unwrap();
    let params = CreateParams {
        origin: None,
        workspace: ws.clone(),
        session: "s-test".into(),
        kind,
        body: body.into(),
        options: options
            .iter()
            .map(|label| NewOption {
                label: (*label).into(),
                description: Some(format!("Why {label}")),
            })
            .collect(),
        severity: None,
        anchors: vec![NewAnchor {
            root,
            path: path.into(),
            lines: LineRange {
                start: lines.0,
                end: lines.1,
            },
        }],
    };
    let agent = ui.agent.clone();
    let q = fixture
        .daemon
        .runtime
        .block_on(async move { agent.request::<qm::Create>(params).await })
        .unwrap();
    // The window's question store hears of it on its own subscription:
    // wait for that, so what the test does next (opening it from a card,
    // say) finds it.
    ui.wait_for("the question in the window's store", |ui| {
        ui.cx.update(|cx| {
            questions_for(&ws, cx).is_some_and(|store| store.read(cx).get(q.id).is_some())
        })
    });
    q
}

/// The daemon's state of question `id`.
fn get(fixture: &Fixture, ui: &Ui, id: QuestionId) -> Question {
    let agent = ui.agent.clone();
    fixture
        .daemon
        .runtime
        .block_on(async move { agent.request::<qm::Get>(QuestionParams { id }).await })
        .unwrap()
}

/// Wait for question `id`'s answer as a blocked agent does
/// (`questions/watch`); the question is `waiting` meanwhile.
fn agent_waits(
    fixture: &Fixture,
    ui: &mut Ui,
    id: QuestionId,
) -> tokio::task::JoinHandle<Question> {
    let (workspace, _) = ui.place().unwrap();
    let agent = ui.agent.clone();
    fixture.daemon.runtime.spawn(async move {
        let mut watch = agent
            .subscribe::<qm::Watch>(
                WatchParams {
                    workspace,
                    session: "s-test".into(),
                    id,
                },
                None,
            )
            .await
            .unwrap();
        while let Some(Ok(event)) = watch.next().await {
            if event.event.status != QuestionStatus::Open {
                return event.event;
            }
        }
        panic!("the watch ended before an answer");
    })
}

fn answer_of(fixture: &Fixture, waiting: tokio::task::JoinHandle<Question>) -> Question {
    fixture
        .daemon
        .runtime
        .block_on(async move { tokio::time::timeout(TIMEOUT, waiting).await })
        .expect("the agent got its answer in time")
        .unwrap()
}

#[test]
fn a_question_shows_at_its_line_and_the_answer_reaches_the_waiting_agent() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture);
    let tab = ui.open_file("src/cart.rs");
    ui.focus_code(&tab);
    let q = ask(
        &fixture,
        &mut ui,
        QuestionKind::Decision,
        "Should the 500 cap apply before the loyalty bonus?\n\nThe code caps last.",
        &["Cap last", "Cap first", "Separate caps"],
        "src/cart.rs",
        (6, 6),
    );
    let marker = marker_of(&q);
    // A sand marker on `sum.min(500)`, the line asked about.
    ui.wait_for("the marker", |ui| {
        ui.marker_lines(&tab) == vec![(marker, 6..7)]
    });
    let sand = ui.cx.read(|cx| cx.tokens().palette.sand.hsla());
    assert_eq!(ui.markers(&tab)[0].color, sand);
    assert!(!ui.markers(&tab)[0].pulse, "nobody waits yet");
    assert!(ui.exists(ElementId::NamedInteger("code-marker".into(), marker)));
    assert!(ui.popover(&tab).is_none(), "no popover until asked for");

    // F8 opens it at the marker, with the keyboard in it.
    ui.press("f8");
    ui.wait_for("the popover", |ui| ui.popover(&tab).is_some());
    assert_eq!(ui.overlay(&tab), Some(OverlayAnchor::Marker(marker)));
    assert!(ui.popover_focused(&tab));
    assert!(ui.exists("question-popover"));
    assert!(ui.exists(("question-option", 2usize)), "three options");

    // Esc closes it without answering.
    ui.press("escape");
    ui.wait_for("the popover closed", |ui| ui.popover(&tab).is_none());
    assert_eq!(get(&fixture, &ui, q.id).status, QuestionStatus::Open);
    assert_eq!(ui.marker_lines(&tab), vec![(marker, 6..7)]);

    // The agent blocks on it: the marker pulses and the popover opens by
    // itself, without taking the keyboard from the code.
    ui.focus_code(&tab);
    let waiting = agent_waits(&fixture, &mut ui, q.id);
    ui.wait_for("the waiting question's popover", |ui| {
        ui.popover(&tab).is_some() && ui.markers(&tab).first().is_some_and(|m| m.pulse)
    });
    assert!(!ui.popover_focused(&tab), "it never steals the keyboard");
    assert!(ui.exists("question-waiting"));

    // Choose B and send: the agent gets option 2.
    ui.click(("question-option", 1usize));
    let popover = ui.popover(&tab).unwrap();
    assert_eq!(
        ui.cx
            .read(|cx| popover.read(cx).selected().map(str::to_owned)),
        Some("2".to_owned())
    );
    ui.click("question-send");
    let answered = answer_of(&fixture, waiting);
    assert_eq!(answered.status, QuestionStatus::Answered);
    assert_eq!(answered.answer.and_then(|a| a.option).as_deref(), Some("2"));
    ui.wait_for("the marker and popover gone", |ui| {
        ui.marker_lines(&tab).is_empty() && ui.popover(&tab).is_none()
    });
}

#[test]
fn edits_above_the_anchor_move_the_marker() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture);
    let tab = ui.open_file("src/cart.rs");
    let q = ask(
        &fixture,
        &mut ui,
        QuestionKind::Clarification,
        "What does `total` return for an empty cart?",
        &[],
        "src/cart.rs",
        (4, 7),
    );
    let marker = marker_of(&q);
    ui.wait_for("the marker", |ui| {
        ui.marker_lines(&tab) == vec![(marker, 4..8)]
    });
    // Two lines typed at the top push it down two lines at once…
    let code = ui.code(&tab).unwrap();
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.focus(window, cx);
            code.set_selections(std::slice::from_ref(&(0..0)), cx);
            code.type_text("// Carts.\n\n", window, cx);
        })
    });
    ui.wait_for("the marker moved", |ui| {
        ui.marker_lines(&tab) == vec![(marker, 6..10)]
    });
    // …and the daemon, following the buffer, agrees.
    let store = ui.store();
    ui.wait_for("the daemon's resolution", |ui| {
        ui.cx.read(|cx| {
            store
                .read(cx)
                .get(q.id)
                .and_then(|q| q.anchors[0].resolution.range)
                .is_some_and(|r| r.start.line == 6)
        })
    });
    ui.frame(|_, _| ());
    assert_eq!(ui.marker_lines(&tab), vec![(marker, 6..10)]);
}

#[test]
fn an_orphaned_question_shows_the_code_it_was_asked_about() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture);
    let q = ask(
        &fixture,
        &mut ui,
        QuestionKind::Confirmation,
        "Remove the 500 cap?",
        &[],
        "src/cart.rs",
        (6, 6),
    );
    // The code goes away before the file is opened.
    write(
        &fixture.project,
        "src/cart.rs",
        "// Rewritten from scratch.\nfn nothing() {}\n",
    );
    let tab = ui.open_file("src/cart.rs");
    let store = ui.store();
    ui.wait_for("the anchor orphaned", |ui| {
        ui.cx.read(|cx| {
            store
                .read(cx)
                .get(q.id)
                .is_some_and(|q| q.anchors[0].resolution.status == AnchorStatus::Orphaned)
        })
    });
    assert!(ui.marker_lines(&tab).is_empty(), "no code to mark");
    // Opening it (the pane, a chat card) shows it at the top, with the
    // snippet as asked.
    ui.frame(|window, cx| window.dispatch_action(Box::new(OpenQuestion { id: q.id.0 }), cx));
    ui.wait_for("the popover", |ui| ui.popover(&tab).is_some());
    assert_eq!(ui.overlay(&tab), Some(OverlayAnchor::Top));
    assert!(ui.exists("question-orphaned"));
    // Yes / No, then send.
    ui.click(("question-option", 0usize));
    ui.click("question-send");
    ui.wait_for("answered", |ui| ui.popover(&tab).is_none());
    let answered = get(&fixture, &ui, q.id);
    assert_eq!(
        answered.answer.and_then(|a| a.option).as_deref(),
        Some("yes")
    );
}

#[test]
fn the_chat_card_and_the_questions_pane_open_the_popover() {
    let fixture = Fixture::new();
    let mut ui = open_ui(&fixture);
    let q = ask(
        &fixture,
        &mut ui,
        QuestionKind::Clarification,
        "Which rounding mode for money?",
        &[],
        "src/cart.rs",
        (1, 1),
    );
    // The chat of the asking session shows the question as a card.
    let dock = ui.dock();
    let chat = ui
        .cx
        .update_window(ui.window, |_, window, cx| {
            let chat = cx.new(|cx| {
                ChatTab::new(
                    coqo_protocol::methods::agents::SessionId("s-test".into()),
                    window,
                    cx,
                )
            });
            dock.update(cx, |dock, cx| {
                dock.open_tab(AnyTab::new(chat.clone()), window, cx)
            });
            chat
        })
        .unwrap();
    let update = SessionUpdate::Question(Box::new(q.clone()));
    ui.cx
        .update(|cx| chat.update(cx, |chat, cx| chat.apply(vec![(1, update)], cx)));
    ui.wait_for("the card", |ui| ui.exists(("chat-question", 0usize)));
    ui.click(("chat-question", 0usize));
    ui.wait_for("the file and its popover", |ui| {
        ui.file_tab("src/cart.rs")
            .is_some_and(|tab| ui.popover(&tab).is_some())
    });
    let tab = ui.file_tab("src/cart.rs").unwrap();
    assert!(ui.popover_focused(&tab), "the reply field has the keyboard");

    // The Questions pane lists it; a click on its card shows it too.
    ui.press("escape");
    ui.wait_for("closed", |ui| ui.popover(&tab).is_none());
    ui.click("sidebar-questions");
    let card = ("question-card", q.id.0 as usize);
    ui.wait_for("the pane's card", |ui| ui.exists(card));
    ui.click(card);
    ui.wait_for("the popover again", |ui| ui.popover(&tab).is_some());

    // A typed reply, sent with Enter.
    let popover = ui.popover(&tab).unwrap();
    let reply = ui.cx.read(|cx| popover.read(cx).reply().cloned().unwrap());
    ui.frame(|window, cx| reply.update(cx, |reply, cx| reply.set_text("HALF_EVEN", window, cx)));
    ui.press("enter");
    ui.wait_for("answered", |ui| ui.popover(&tab).is_none());
    let answered = get(&fixture, &ui, q.id);
    assert_eq!(
        answered.answer.and_then(|a| a.text).as_deref(),
        Some("HALF_EVEN")
    );
    // The pane moved it to the answered history.
    ui.wait_for("gone from the open list", |ui| !ui.exists(card));
    ui.click("questions-answered");
    ui.wait_for("in the history", |ui| ui.exists(card));
}

/// Renders a waiting decision question in its file, with the Questions pane
/// open, offscreen to PNGs. Needs a GPU:
/// `cargo test -p coqo -- --ignored questions_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn questions_screenshots() {
    use std::sync::Arc;

    use coqo_ui::{
        CoqoAssets,
        gpui::{HeadlessAppContext, platform},
    };

    const SHOT: &str = "\
use crate::money::Cents;

/// A line of the cart.
pub struct Line {
    pub price: Cents,
    pub quantity: u32,
}

pub struct Cart {
    lines: Vec<Line>,
    loyalty: Option<Cents>,
}

/// What the customer pays: the lines, less the loyalty bonus, capped.
pub fn total(cart: &Cart) -> Cents {
    let sum: Cents = cart.lines.iter().map(|l| l.price * l.quantity).sum();
    let capped = sum.min(Cents(50_000));
    capped - cart.loyalty.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_at_500() {
        let cart = Cart { lines: vec![], loyalty: None };
        assert_eq!(total(&cart), Cents(0));
    }
}

/// Rounds line 1 of an invoice.
pub fn round_1(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}

/// Rounds line 2 of an invoice.
pub fn round_2(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}

/// Rounds line 3 of an invoice.
pub fn round_3(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}

/// Rounds line 4 of an invoice.
pub fn round_4(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}

/// Rounds line 5 of an invoice.
pub fn round_5(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}

/// Rounds line 6 of an invoice.
pub fn round_6(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}

/// Rounds line 7 of an invoice.
pub fn round_7(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}

/// Rounds line 8 of an invoice.
pub fn round_8(c: Cents) -> Cents {
    Cents((c.0 + 50) / 100 * 100)
}
";

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let fixture = Fixture::new();
    write(&fixture.project, "src/cart.rs", SHOT);
    let paths = fixture.daemon.paths.clone();
    let (agent, _) = fixture
        .daemon
        .runtime
        .block_on(connect(&paths, ClientConfig::new("coqo-tests", "agent")))
        .unwrap();
    let mut asked: Option<Question> = None;
    let mut waiting = None;
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let mut cx = HeadlessAppContext::with_platform(
            platform::current_platform(true).text_system(),
            Arc::new(CoqoAssets),
            platform::current_headless_renderer,
        );
        cx.allow_parking();
        let (paths, folder) = (fixture.daemon.paths.clone(), fixture.project.clone());
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
        let q = match &asked {
            Some(q) => q.clone(),
            None => {
                let create = |params: CreateParams| {
                    let agent = agent.clone();
                    fixture
                        .daemon
                        .runtime
                        .block_on(async move { agent.request::<qm::Create>(params).await })
                        .unwrap()
                };
                let q = create(CreateParams {
                    origin: None,
                    workspace: ws.clone(),
                    session: "s-shot".into(),
                    kind: QuestionKind::Decision,
                    body: "Should the 500 cap apply before or after the loyalty bonus?\n\n\
                           Right now `total` caps the sum first, so a customer at the cap \
                           gets less from their bonus than one just under it."
                        .into(),
                    options: [
                        ("Cap after the bonus", "Bonus first, then `min(50_000)`."),
                        ("Keep capping first", "Today's behaviour; document it."),
                        ("Cap each separately", "Two caps: lines and bonus."),
                    ]
                    .into_iter()
                    .map(|(label, why)| NewOption {
                        label: label.into(),
                        description: Some(why.into()),
                    })
                    .collect(),
                    severity: None,
                    anchors: vec![NewAnchor {
                        root,
                        path: "src/cart.rs".into(),
                        lines: LineRange { start: 15, end: 16 },
                    }],
                });
                // A note on another line of the same file.
                create(CreateParams {
                    origin: None,
                    workspace: ws.clone(),
                    session: "s-shot".into(),
                    kind: QuestionKind::Annotation,
                    body: "`Cents` multiplication saturates; see money.rs.".into(),
                    options: vec![],
                    severity: Some(qm::Severity::Info),
                    anchors: vec![NewAnchor {
                        root,
                        path: "src/cart.rs".into(),
                        lines: LineRange { start: 4, end: 4 },
                    }],
                });
                // The agent blocks on the question.
                let (agent, workspace, id) = (agent.clone(), ws.clone(), q.id);
                waiting = Some(fixture.daemon.runtime.spawn(async move {
                    let mut watch = agent
                        .subscribe::<qm::Watch>(
                            WatchParams {
                                workspace,
                                session: "s-shot".into(),
                                id,
                            },
                            None,
                        )
                        .await
                        .unwrap();
                    while watch.next().await.is_some() {}
                }));
                asked = Some(q.clone());
                q
            }
        };
        let store = cx.update(|cx| questions_for(&ws, cx)).unwrap();
        settle(&mut cx, &|cx| {
            store.read(cx).get(q.id).is_some_and(|q| q.waiting)
        });
        // The Questions pane, and the question open in its file.
        cx.update_window(handle, |_, window, cx| {
            window.click("sidebar-questions", cx);
            window.dispatch_action(Box::new(OpenQuestion { id: q.id.0 }), cx);
        })
        .unwrap();
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        let tab = |cx: &App| {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
        };
        settle(&mut cx, &|cx| {
            tab(cx).is_some_and(|t| t.read(cx).popover().is_some())
        });
        for _ in 0..20 {
            settle(&mut cx, &|_| true);
            std::thread::sleep(Duration::from_millis(20));
        }
        let image = cx.capture_screenshot(handle).expect("screenshot");
        let path = dir.join(format!("coqo-questions-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
    if let Some(waiting) = waiting {
        waiting.abort();
    }
}

// ---- the user asks (#229) -------------------------------------------------------

#[test]
fn asking_about_selected_lines_pins_a_thread_with_the_agents_reply() {
    use coqo_agents::{
        AgentUpdate,
        model::{ContentBlock as Block, CreateSessionParams, StopReason},
        testing::{Call, ScriptedProvider, Step},
    };
    use coqo_protocol::methods::agents as agm;

    use super::ask::{AskPopover, AskTarget};
    use crate::{chat::transcript::Entry, test_support::AgentDaemon};

    let say = |text: &str| {
        [
            Step::Update(AgentUpdate::AgentMessageChunk(Block::text(text))),
            Step::Stop(StopReason::EndTurn),
        ]
    };
    let provider = ScriptedProvider::new()
        .turn(say("It caps the total at 500."))
        .turn(say("Yes, keep the cap."));
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("demo-project");
    write(&project, "src/main.rs", "fn main() {}\n");
    let project = project.canonicalize().unwrap();
    let daemon = AgentDaemon::start(tmp.path(), &provider);
    let mut ui = open_ui_at(&project, &daemon.paths, &daemon.runtime);
    let (ws, _) = ui.place().unwrap();

    // A chat in the workspace, started elsewhere: the one asked by default.
    let cwd = project.to_string_lossy().into_owned();
    let ws_id = ws.0.clone();
    let session = daemon.with_client(async move |client| {
        client
            .request::<agm::Create>(CreateSessionParams {
                provider: "fake".into(),
                cwd,
                workspace: Some(ws_id),
                title: Some("Cart work".into()),
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

    let tab = ui.open_file("src/cart.rs");
    let code = ui.code(&tab).unwrap();
    let ask_popover = |ui: &mut Ui| -> Option<Entity<AskPopover>> {
        ui.cx.read(|cx| tab.read(cx).ask_popover().cloned())
    };
    // `let sum = …` and `sum.min(500)` selected: the ask button shows.
    let start = CART.find("    let sum").unwrap();
    let stop = CART.rfind("}\n").unwrap();
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.focus(window, cx);
            code.set_selections(std::slice::from_ref(&(start..stop)), cx);
        })
    });
    ui.wait_for("the ask button", |ui| ui.exists("code-ask"));

    // The hotkey opens the ask popover on those lines; Esc closes it.
    ui.press(ASK_KEYS[0]);
    ui.wait_for("the ask popover", |ui| ask_popover(ui).is_some());
    assert_eq!(
        ui.overlay(&tab),
        Some(OverlayAnchor::Tracked { start: 5, end: 7 })
    );
    ui.press("escape");
    ui.wait_for("the ask popover closed", |ui| ask_popover(ui).is_none());
    assert_eq!(ui.overlay(&tab), None);

    // So does the button, asking the chat by default.
    ui.focus_code(&tab);
    ui.wait_for("the ask button", |ui| ui.exists("code-ask"));
    ui.click("code-ask");
    ui.wait_for("the ask popover", |ui| ask_popover(ui).is_some());
    let ask = ask_popover(&mut ui).unwrap();
    assert_eq!(
        ui.cx.read(|cx| ask.read(cx).target().clone()),
        AskTarget::Session(session.clone())
    );
    assert!(ui.exists(("ask-target", 0usize)) && ui.exists(("ask-target-new", 0usize)));
    let input = ui.cx.read(|cx| ask.read(cx).input().clone());
    ui.frame(|window, cx| {
        input.update(cx, |i, cx| {
            i.set_text("Why cap the total at 500?", window, cx)
        })
    });
    ui.click("ask-send");

    // A blue thread pinned there, its popover open, the code sent along.
    ui.wait_for("the thread's popover", |ui| ui.popover(&tab).is_some());
    let store = ui.store();
    let id = ui.cx.read(|cx| tab.read(cx).popover_question(cx)).unwrap();
    let q = ui.cx.read(|cx| store.read(cx).get(id).cloned()).unwrap();
    assert!(!q.author.is_agent());
    assert_eq!(q.session, session);
    assert_eq!(ui.marker_lines(&tab), vec![(marker_of(&q), 5..7)]);
    assert!(ask_popover(&mut ui).is_none());
    let prompt = provider
        .calls()
        .into_iter()
        .find_map(|c| match c {
            Call::Prompt(content) => Some(content),
            _ => None,
        })
        .expect("the chat was prompted");
    let Block::Text { text } = &prompt[0] else {
        panic!("{prompt:?}")
    };
    assert!(text.ends_with("Why cap the total at 500?"), "{text}");
    assert!(text.contains("src/cart.rs:6-7"), "{text}");
    let Block::Resource { text, .. } = &prompt[1] else {
        panic!("{prompt:?}")
    };
    assert!(text.contains("sum.min(500)"), "{text}");
    // F8 is for the agents' questions.
    assert!(ui.cx.read(|cx| store.read(cx).navigation().is_empty()));
    assert_eq!(ui.cx.read(|cx| store.read(cx).open_count()), 0);

    // The reply streams into the thread.
    let popover = ui.popover(&tab).unwrap();
    let reply_of = |ui: &mut Ui, ix: usize| -> Option<String> {
        let q = ui.cx.read(|cx| store.read(cx).get(id).cloned())?;
        let turn = q.thread.get(ix)?.turn;
        ui.cx.read(|cx| {
            let replies = popover.read(cx).replies()?.read(cx);
            replies
                .reply(turn)
                .filter(|r| r.done)
                .map(|r| r.text.clone())
        })
    };
    ui.wait_for("the reply", |ui| {
        reply_of(ui, 0).as_deref() == Some("It caps the total at 500.")
    });
    assert!(ui.exists(("thread-reply", 0usize)));

    // A follow-up continues it.
    let field = ui.cx.read(|cx| popover.read(cx).reply().cloned()).unwrap();
    ui.frame(|window, cx| field.update(cx, |f, cx| f.set_text("Should it stay?", window, cx)));
    ui.click("question-send");
    ui.wait_for("the second reply", |ui| {
        reply_of(ui, 1).as_deref() == Some("Yes, keep the cap.")
    });
    assert_eq!(ui.cx.read(|cx| field.read(cx).text(cx)), "");

    // Lines typed above move it.
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.focus(window, cx);
            code.set_selections(std::slice::from_ref(&(0..0)), cx);
            code.type_text("// Carts.\n", window, cx);
        })
    });
    ui.wait_for("the marker moved", |ui| {
        ui.marker_lines(&tab) == vec![(marker_of(&q), 6..8)]
    });

    // Open in chat: the card, and the agent's replies in the transcript.
    ui.click("question-open-chat");
    let chat = |ui: &mut Ui| -> Option<Entity<ChatTab>> {
        let dock = ui.dock();
        ui.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter_map(|t| dock.tab(t.id)?.downcast::<ChatTab>())
                .next()
        })
    };
    ui.wait_for("the chat", |ui| {
        chat(ui).is_some_and(|chat| {
            ui.cx.read(|cx| {
                let entries = &chat.read(cx).transcript().entries;
                let card = entries
                    .iter()
                    .any(|e| matches!(e, Entry::Question(q) if q.id == id));
                let reply = entries
                    .iter()
                    .any(|e| matches!(e, Entry::Agent { text } if text == "Yes, keep the cap."));
                card && reply
            })
        })
    });

    // The question in short, the prompt and the code behind Show (#248).
    let chat = chat(&mut ui).unwrap();
    let asked: Vec<(usize, String)> = ui.cx.read(|cx| {
        chat.read(cx)
            .transcript()
            .entries
            .iter()
            .enumerate()
            .filter_map(|(ix, e)| match e {
                Entry::User { content } => {
                    let text = content.iter().find_map(|b| match b {
                        agm::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })?;
                    let prompt = qm::ThreadPrompt::parse(text)?;
                    Some((ix, crate::chat::tab::asked_summary(&prompt)))
                }
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        asked.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
        [
            "You asked about src/cart.rs:6–7: Why cap the total at 500?",
            "Follow-up on src/cart.rs:6–7: Should it stay?",
        ]
    );
    let ix = asked[0].0;
    ui.wait_for("the short question", |ui| ui.exists(("chat-asked", ix)));
    assert!(!ui.exists(("chat-user", ix)));
    assert!(!ui.exists(("chat-asked-prompt", ix)));
    ui.click(("chat-asked-show", ix));
    ui.wait_for("the whole prompt", |ui| {
        ui.exists(("chat-asked-prompt", ix))
    });
    ui.click(("chat-asked-show", ix));
    ui.wait_for("it hidden", |ui| !ui.exists(("chat-asked-prompt", ix)));
    drop(daemon);
}

/// #248: the ask popover's lines follow an edit above them before it is
/// sent; the thread stays at the end of a reply as it streams in, until the
/// user scrolls up; and the ask button says what Alt-click does.
#[test]
fn an_asked_thread_tracks_its_lines_and_follows_the_streaming_reply() {
    use std::sync::Arc;

    use coqo_agents::{
        AgentUpdate,
        model::{ContentBlock as Block, CreateSessionParams, StopReason},
        testing::{Call, ScriptedProvider, Step},
    };
    use coqo_protocol::methods::agents as agm;
    use coqo_ui::gpui::{ScrollDelta, point};
    use tokio::sync::Notify;

    use crate::test_support::AgentDaemon;

    let chunk = |text: &str| Step::Update(AgentUpdate::AgentMessageChunk(Block::text(text)));
    let (more, done) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let long: String = (1..=24)
        .map(|i| format!("Point {i} of the answer.\n\n"))
        .collect();
    let provider = ScriptedProvider::new().turn([
        chunk(&long),
        Step::Wait(more.clone()),
        chunk("Another point, while you read.\n\n"),
        Step::Wait(done.clone()),
        chunk("The end."),
        Step::Stop(StopReason::EndTurn),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("demo-project");
    write(&project, "src/main.rs", "fn main() {}\n");
    let project = project.canonicalize().unwrap();
    let daemon = AgentDaemon::start(tmp.path(), &provider);
    let mut ui = open_ui_at(&project, &daemon.paths, &daemon.runtime);
    let (ws, _) = ui.place().unwrap();
    let cwd = project.to_string_lossy().into_owned();
    let ws_id = ws.0.clone();
    let session = daemon.with_client(async move |client| {
        client
            .request::<agm::Create>(CreateSessionParams {
                provider: "fake".into(),
                cwd,
                workspace: Some(ws_id),
                title: Some("Cart work".into()),
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

    let tab = ui.open_file("src/cart.rs");
    let code = ui.code(&tab).unwrap();
    // The button's tooltip names no key (#372).
    let tip = ui.cx.read(|cx| code.read(cx).ask_tip().cloned()).unwrap();
    assert_eq!(tip, "Ask an agent");

    // Ask about `let sum = …` and `sum.min(500)`, then type a line above
    // them before sending: the popover and its lines move with them.
    ui.frame(|window, cx| tab.update(cx, |tab, cx| tab.ask_about(5..7, window, cx)));
    ui.wait_for("the ask popover", |ui| {
        ui.cx.read(|cx| tab.read(cx).ask_popover().is_some())
    });
    let ask = ui
        .cx
        .read(|cx| tab.read(cx).ask_popover().cloned())
        .unwrap();
    ui.frame(|window, cx| {
        code.update(cx, |code, cx| {
            code.set_selections(std::slice::from_ref(&(0..0)), cx);
            code.type_text("// Carts.\n", window, cx);
        })
    });
    ui.wait_for("the popover moved", |ui| {
        ui.overlay(&tab) == Some(OverlayAnchor::Tracked { start: 6, end: 8 })
    });
    assert_eq!(ui.cx.read(|cx| ask.read(cx).lines().clone()), 6..8);
    let input = ui.cx.read(|cx| ask.read(cx).input().clone());
    ui.frame(|window, cx| {
        input.update(cx, |i, cx| {
            i.set_text("Why cap the total at 500?", window, cx)
        })
    });
    ui.click("ask-send");
    ui.wait_for("the thread's popover", |ui| ui.popover(&tab).is_some());
    let prompt = provider
        .calls()
        .into_iter()
        .find_map(|c| match c {
            Call::Prompt(content) => Some(content),
            _ => None,
        })
        .expect("the chat was prompted");
    let Block::Text { text } = &prompt[0] else {
        panic!("{prompt:?}")
    };
    assert!(text.contains("src/cart.rs:7-8"), "{text}");
    let Block::Resource { text, .. } = &prompt[1] else {
        panic!("{prompt:?}")
    };
    assert!(
        text.contains("let sum") && text.contains("sum.min(500)"),
        "{text}"
    );
    let store = ui.store();
    let id = ui.cx.read(|cx| tab.read(cx).popover_question(cx)).unwrap();
    let q = ui.cx.read(|cx| store.read(cx).get(id).cloned()).unwrap();
    assert_eq!(ui.marker_lines(&tab), vec![(marker_of(&q), 6..8)]);

    // The first part of the reply overflows the thread: it shows its end.
    let popover = ui.popover(&tab).unwrap();
    let reply = |ui: &mut Ui| -> String {
        let turn = ui.cx.read(|cx| {
            store
                .read(cx)
                .get(id)
                .and_then(|q| q.thread.first().map(|m| m.turn))
        });
        ui.cx.read(|cx| {
            turn.and_then(|turn| {
                let replies = popover.read(cx).replies()?.read(cx);
                replies.reply(turn).map(|r| r.text.clone())
            })
            .unwrap_or_default()
        })
    };
    let scroll = ui.cx.read(|cx| popover.read(cx).thread_scroll().clone());
    let at_end = |ui: &mut Ui| {
        ui.frame(|_, _| ());
        let (offset, max) = (scroll.offset().y, scroll.max_offset().y);
        max > px(0.) && (offset + max).abs() < px(2.)
    };
    ui.wait_for("the first part, at its end", |ui| {
        reply(ui).contains("Point 24") && at_end(ui)
    });

    // More of it keeps it at the end.
    let before = scroll.max_offset().y;
    more.notify_one();
    ui.wait_for("more of the reply, at its end", |ui| {
        reply(ui).contains("Another point") && scroll.max_offset().y > before && at_end(ui)
    });

    // Scrolled up, it stays where the user is as the rest comes in.
    ui.frame(|window, cx| {
        window.scroll("thread", ScrollDelta::Pixels(point(px(0.), px(120.))), cx)
    });
    ui.frame(|_, _| ());
    let read_at = scroll.offset().y;
    assert!(read_at > -scroll.max_offset().y + px(2.), "scrolled up");
    let before = scroll.max_offset().y;
    done.notify_one();
    ui.wait_for("the rest of the reply", |ui| {
        reply(ui).ends_with("The end.") && scroll.max_offset().y > before
    });
    ui.frame(|_, _| ());
    assert_eq!(
        scroll.offset().y,
        read_at,
        "still where the user scrolled to"
    );
    drop(daemon);
}

/// Offscreen screenshots of asking about code (#229): the ask popover, then
/// the pinned thread with the agent's reply, then the chat with the
/// question in short (#248), as `coqo-ask-{dark,light}.png`,
/// `coqo-thread-{dark,light}.png` and `coqo-asked-chat-{dark,light}.png` in
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn ask_screenshots() {
    use std::sync::Arc;

    use coqo_agents::{
        AgentUpdate,
        model::{ContentBlock as Block, CreateSessionParams, StopReason},
        testing::{ScriptedProvider, Step},
    };
    use coqo_protocol::methods::agents as agm;
    use coqo_ui::{
        CoqoAssets,
        gpui::{HeadlessAppContext, platform},
    };

    use crate::test_support::AgentDaemon;

    const SHOT: &str = "\
use crate::money::Cents;

/// A line of the cart.
pub struct Line {
    pub price: Cents,
    pub quantity: u32,
}

pub struct Cart {
    lines: Vec<Line>,
    loyalty: Option<Cents>,
}

/// What the customer pays: the lines, less the loyalty bonus, capped.
pub fn total(cart: &Cart) -> Cents {
    let sum: Cents = cart.lines.iter().map(|l| l.price * l.quantity).sum();
    let capped = sum.min(Cents(50_000));
    capped - cart.loyalty.unwrap_or_default()
}
";
    const REPLY: &str = "It caps the **sum of the lines** at 500.00 before the loyalty \
bonus comes off, so a customer at the cap still gets their whole bonus.\n\n\
If the cap should hold for what they pay, subtract the bonus first and cap \
`sum - loyalty` instead.";

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let say = |text: &str| {
        [
            Step::Update(AgentUpdate::AgentMessageChunk(Block::text(text))),
            Step::Stop(StopReason::EndTurn),
        ]
    };
    let provider = ScriptedProvider::new().turn(say(REPLY)).turn(say(REPLY));
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("demo-project");
    write(&project, "src/cart.rs", SHOT);
    let project = project.canonicalize().unwrap();
    let daemon = AgentDaemon::start(tmp.path(), &provider);
    let mut session: Option<agm::SessionId> = None;
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
        let shoot = |cx: &mut HeadlessAppContext, file: String| {
            for _ in 0..20 {
                cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                    .unwrap();
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(20));
            }
            let image = cx.capture_screenshot(handle).expect("screenshot");
            let path = dir.join(file);
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        };
        let place = |cx: &App| {
            let current = Workspaces::global(cx)?.read(cx).current()?.clone();
            Some((current.id.clone(), current.roots[0].id))
        };
        settle(&mut cx, &|cx| place(cx).is_some());
        let (ws, root) = cx.update(|cx| place(cx)).unwrap();
        let id = match &session {
            Some(id) => id.clone(),
            None => {
                let (cwd, ws_id) = (project.to_string_lossy().into_owned(), ws.0.clone());
                let id = daemon.with_client(async move |client| {
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
                session = Some(id.clone());
                id
            }
        };
        settle(&mut cx, &|cx| {
            crate::chat::list::ChatList::global(cx).is_some_and(|l| l.read(cx).info(&id).is_some())
        });
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        let tab = cx
            .update_window(handle, |_, window, cx| {
                navigate::open_file(&dock, &ws, root, "src/cart.rs", window, cx)
            })
            .unwrap()
            .unwrap();
        settle(&mut cx, &|cx| tab.read(cx).editor().is_some());
        let code = cx.update(|cx| tab.read(cx).editor().cloned()).unwrap();
        let (start, stop) = (
            SHOT.find("    let sum").unwrap(),
            SHOT.find("    capped -").unwrap(),
        );
        cx.update_window(handle, |_, window, cx| {
            code.update(cx, |code, cx| {
                code.set_selections(std::slice::from_ref(&(start..stop)), cx)
            });
            tab.update(cx, |tab, cx| tab.ask_about(15..17, window, cx));
        })
        .unwrap();
        settle(&mut cx, &|cx| tab.read(cx).ask_popover().is_some());
        let ask = cx.update(|cx| tab.read(cx).ask_popover().cloned()).unwrap();
        let input = cx.update(|cx| ask.read(cx).input().clone());
        cx.update_window(handle, |_, window, cx| {
            input.update(cx, |i, cx| {
                i.set_text(
                    "Does this cap apply before or after the loyalty bonus?",
                    window,
                    cx,
                )
            })
        })
        .unwrap();
        shoot(&mut cx, format!("coqo-ask-{name}.png"));

        if name == "dark" {
            cx.update_window(handle, |_, window, cx| {
                ask.update(cx, |ask, cx| ask.send(window, cx))
            })
            .unwrap();
        } else {
            // Asked already: show the thread.
            cx.update_window(handle, |_, window, cx| {
                tab.update(cx, |tab, cx| tab.close_ask(window, cx));
                let store = questions_for(&ws, cx).unwrap();
                let q = store.read(cx).open().first().map(|q| q.id).unwrap();
                tab.update(cx, |tab, cx| tab.show_question(q, true, false, window, cx));
            })
            .unwrap();
        }
        settle(&mut cx, &|cx| {
            tab.read(cx).popover().is_some_and(|p| {
                let p = p.read(cx);
                let Some(replies) = p.replies() else {
                    return false;
                };
                replies.read(cx).reply(1).is_some_and(|r| r.done)
            })
        });
        shoot(&mut cx, format!("coqo-thread-{name}.png"));

        // The chat: the question in short, next to the thread's card (#248).
        let q = cx.update(|cx| tab.read(cx).popover_question(cx)).unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_action(Box::new(popover::OpenQuestionChat { id: q.0 }), cx)
        })
        .unwrap();
        let chat = |cx: &App| {
            let dock = dock.read(cx);
            let active = dock.active_tab()?;
            dock.tab(active)?.downcast::<ChatTab>()
        };
        settle(&mut cx, &|cx| {
            chat(cx).is_some_and(|chat| {
                chat.read(cx).transcript().entries.iter().any(|e| {
                    matches!(e, crate::chat::transcript::Entry::Agent { text } if text == REPLY)
                })
            })
        });
        shoot(&mut cx, format!("coqo-asked-chat-{name}.png"));
    }
}

// ---- findings written as text (#345) ----------------------------------------------

/// What an agent says after a review: two findings in lists with severity
/// words, a nit, one inside a code block, one past the end, one in a file
/// that isn't there, and a port number.
const REVIEW: &str = "\
Review of the cart:

- src/cart.rs:6 critical: the sum can overflow
- `src/cart.rs:5` should check the lines first
- nit: src/cart.rs:2 stray blank line
- src/cart.rs:900 is past the end
- src/gone.rs:3 does not exist
- served on localhost:8080

```
src/cart.rs:1
```
";

/// A chat in the UI's workspace, on a scripted agent that says `turns` in
/// turn.
struct ReviewChat {
    ui: Ui,
    chat: Entity<ChatTab>,
    session: agm::SessionId,
    _daemon: crate::test_support::AgentDaemon,
    _tmp: tempfile::TempDir,
}

fn review_chat(turns: &[&str]) -> ReviewChat {
    use coqo_agents::{
        AgentUpdate,
        model::{ContentBlock as Block, StopReason},
        testing::{ScriptedProvider, Step},
    };

    use crate::test_support::AgentDaemon;

    let mut provider = ScriptedProvider::new();
    for text in turns {
        provider = provider.turn([
            Step::Update(AgentUpdate::AgentMessageChunk(Block::text(*text))),
            Step::Stop(StopReason::EndTurn),
        ]);
    }
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("demo-project");
    write(&project, "src/main.rs", "fn main() {}\n");
    let project = project.canonicalize().unwrap();
    let daemon = AgentDaemon::start(tmp.path(), &provider);
    let mut ui = open_ui_at(&project, &daemon.paths, &daemon.runtime);
    let (ws, _) = ui.place().unwrap();
    let cwd = project.to_string_lossy().into_owned();
    let session = daemon.with_client(async move |client| {
        client
            .request::<agm::Create>(agm::CreateSessionParams {
                provider: "fake".into(),
                cwd,
                workspace: Some(ws.0.clone()),
                title: Some("Cart review".into()),
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
    let dock = ui.dock();
    let id = session.clone();
    ui.cx
        .update_window(ui.window, |_, window, cx| {
            crate::chat::open_session(dock.downgrade(), id, window, cx)
        })
        .unwrap();
    let chat = {
        let dock = ui.dock();
        ui.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .find_map(|t| dock.tab(t.id)?.downcast::<ChatTab>())
                .unwrap()
        })
    };
    ReviewChat {
        ui,
        chat,
        session,
        _daemon: daemon,
        _tmp: tmp,
    }
}

impl ReviewChat {
    /// Send `text` and wait for the turn to end.
    fn say(&mut self, text: &str) {
        let chat = self.chat.clone();
        let composer = self.ui.cx.read(|cx| chat.read(cx).composer().clone());
        let text = text.to_owned();
        self.ui.frame(|window, cx| {
            composer.update(cx, |c, cx| c.set_text(text, window, cx));
            let focus = composer.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        let before = self.agent_messages();
        self.ui.press("enter");
        self.ui.wait_for("the reply", |ui| {
            ui.cx.read(|cx| {
                let t = chat.read(cx).transcript();
                !t.running
                    && t.entries
                        .iter()
                        .filter(|e| matches!(e, Entry::Agent { .. }))
                        .count()
                        > before
            })
        });
    }

    fn agent_messages(&mut self) -> usize {
        self.ui.cx.read(|cx| {
            self.chat
                .read(cx)
                .transcript()
                .entries
                .iter()
                .filter(|e| matches!(e, Entry::Agent { .. }))
                .count()
        })
    }

    /// The seq and entry of the agent message `n`.
    fn message(&mut self, n: usize) -> (u64, usize) {
        self.ui.cx.read(|cx| {
            let t = self.chat.read(cx).transcript();
            let ix = (0..t.entries.len())
                .filter(|ix| matches!(t.entries[*ix], Entry::Agent { .. }))
                .nth(n)
                .unwrap();
            (t.message_seq(ix).unwrap(), ix)
        })
    }

    /// The open notes made from messages, oldest first.
    fn notes(&mut self) -> Vec<Question> {
        let store = self.ui.store();
        self.ui.cx.read(|cx| {
            store
                .read(cx)
                .open()
                .into_iter()
                .filter(|q| q.origin.is_some())
                .cloned()
                .collect()
        })
    }
}

/// The 1-based lines of `notes`, sorted.
fn lines_of(notes: &[Question]) -> Vec<u32> {
    let mut lines: Vec<u32> = notes
        .iter()
        .map(|q| anchor_line(&q.anchors[0]) + 1)
        .collect();
    lines.sort_unstable();
    lines
}

#[test]
fn findings_in_a_message_become_notes_linked_to_it() {
    use crate::files::editor::layer;

    // The user's own prompt names a place too: no note comes from it.
    let mut r = review_chat(&[REVIEW]);
    r.say("Review src/cart.rs:3 please");
    let (seq, ix) = r.message(0);
    r.ui.wait_for("the notes", |ui| ui.exists(("chat-notes", ix)));

    // One note per valid finding, and none for the code block, the line past
    // the end, the missing file, the port or the user's message.
    let notes = r.notes();
    assert_eq!(lines_of(&notes), vec![2, 5, 6]);
    for q in &notes {
        assert_eq!(q.kind, QuestionKind::Annotation);
        assert_eq!(q.session, r.session);
        assert_eq!(q.origin.map(|o| o.message), Some(seq));
        assert!(q.author.is_agent());
        assert_eq!(kind_label(q), "FINDING");
    }

    // Coloured by severity words.
    let at = |line: u32| {
        notes
            .iter()
            .find(|q| anchor_line(&q.anchors[0]) + 1 == line)
            .cloned()
            .unwrap()
    };
    let (critical, warning, nit) = (at(6), at(5), at(2));
    let colors = r.ui.cx.read(|cx| {
        let p = cx.tokens().palette.clone();
        (
            marker_color(&critical, cx) == p.rose,
            marker_color(&warning, cx) == p.sand,
            marker_color(&nit, cx) == p.text_subtle,
        )
    });
    assert_eq!(colors, (true, true, true));

    // The message says so; its link shows the first note's popover.
    r.ui.click(("chat-notes-show", ix));
    r.ui.wait_for("the file", |ui| ui.file_tab("src/cart.rs").is_some());
    let tab = r.ui.file_tab("src/cart.rs").unwrap();
    r.ui.wait_for("a popover", |ui| ui.popover(&tab).is_some());
    let open =
        r.ui.cx
            .read(|cx| tab.read(cx).popover_question(cx))
            .unwrap();
    assert!(notes.iter().any(|q| q.id == open));

    // They are gutter markers in the file, below everything deliberate.
    r.ui.wait_for("the markers", |ui| ui.markers(&tab).len() == 3);
    assert!(
        r.ui.markers(&tab)
            .iter()
            .all(|m| m.priority == layer::FINDING)
    );
    const { assert!(layer::FINDING < layer::OWN_THREAD) };

    // F8 stays with what an agent pinned on purpose.
    let store = r.ui.store();
    assert!(r.ui.cx.read(|cx| store.read(cx).navigation().is_empty()));

    // "Open in chat" on the note scrolls the chat to the message and marks it.
    let chat = r.chat.clone();
    assert_eq!(r.ui.cx.read(|cx| chat.read(cx).marked_message()), None);
    r.ui.click("question-open-chat");
    r.ui.wait_for("the message marked", |ui| {
        ui.cx.read(|cx| chat.read(cx).marked_message()) == Some(seq)
    });

    // Dismissing one note keeps the others.
    let session = r.session.clone();
    let remaining = |ui: &mut Ui| {
        ui.cx
            .read(|cx| store.read(cx).message_notes(&session, seq).len())
    };
    r.ui.cx
        .update(|cx| dismiss(&store, notes[0].id, cx).detach());
    r.ui.wait_for("one dismissed", |ui| remaining(ui) == 2);
    assert!(r.ui.exists(("chat-notes", ix)));

    // "Dismiss all" closes the rest at once.
    r.ui.click(("chat-notes-dismiss", ix));
    r.ui.wait_for("all dismissed", |ui| remaining(ui) == 0);
    r.ui.wait_for("the footer gone", |ui| !ui.exists(("chat-notes", ix)));
    // The message is not scanned again.
    assert!(
        r.ui.cx
            .read(|cx| store.read(cx).has_message_notes(&session, seq))
    );
    assert!(r.notes().is_empty());
}

#[test]
fn findings_can_be_turned_off() {
    let mut r = review_chat(&[REVIEW]);
    r.ui.cx
        .update(|cx| crate::settings::set_local(cx, crate::settings::FINDINGS_IN_GUTTER, false));
    r.say("Review it");
    let (_, ix) = r.message(0);
    for _ in 0..30 {
        r.ui.wait_for("a frame", |_| true);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(r.notes().is_empty());
    assert!(!r.ui.exists(("chat-notes", ix)));
}

/// Offscreen screenshots of findings written as text (#345): the chat with
/// its message and "3 notes in code", then the file with the notes in its
/// gutter and one popover open, as `coqo-findings-chat-{dark,light}.png` and
/// `coqo-findings-code-{dark,light}.png` in `$COQO_SHOT_DIR` (default: the
/// system temp dir). Needs a GPU:
/// `cargo test -p coqo -- --ignored findings_screenshots`.
#[test]
#[ignore = "needs a GPU; run manually"]
fn findings_screenshots() {
    use std::sync::Arc;

    use coqo_agents::{
        AgentUpdate,
        model::{ContentBlock as Block, CreateSessionParams, StopReason},
        testing::{ScriptedProvider, Step},
    };
    use coqo_protocol::methods::agents as agm;
    use coqo_ui::{
        CoqoAssets,
        gpui::{Focusable as _, HeadlessAppContext, platform},
    };

    use crate::test_support::AgentDaemon;

    const SHOT: &str = "\
use crate::money::Cents;

/// A line of the cart.
pub struct Line {
    pub price: Cents,
    pub quantity: u32,
}

pub struct Cart {
    lines: Vec<Line>,
    loyalty: Option<Cents>,
}

/// What the customer pays: the lines, less the loyalty bonus, capped.
pub fn total(cart: &Cart) -> Cents {
    let sum: Cents = cart.lines.iter().map(|l| l.price * l.quantity).sum();
    let capped = sum.min(Cents(50_000));
    capped - cart.loyalty.unwrap_or_default()
}
";
    const REVIEW: &str = "\
I read the cart and found three things.\n\n\
### Critical\n\n\
- `src/cart.rs:15` the sum of the lines can overflow `Cents` before it is capped.\n\n\
### Should fix\n\n\
- `src/cart.rs:17` the loyalty bonus can take the total below zero, and the result is unsigned.\n\n\
### Nits\n\n\
- `src/cart.rs:4` the doc comment says \"line\" twice.\n\n\
`localhost:8080` is only the dev server, and `src/cart.rs:90` is past the end of the file, so neither is pinned.";

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let provider = ScriptedProvider::new().turn([
            Step::Update(AgentUpdate::AgentMessageChunk(Block::text(REVIEW))),
            Step::Stop(StopReason::EndTurn),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo-project");
        write(&project, "src/cart.rs", SHOT);
        let project = project.canonicalize().unwrap();
        let daemon = AgentDaemon::start(tmp.path(), &provider);
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
        let shoot = |cx: &mut HeadlessAppContext, file: String| {
            for _ in 0..20 {
                cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                    .unwrap();
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(20));
            }
            let image = cx.capture_screenshot(handle).expect("screenshot");
            let path = dir.join(file);
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        };
        let place = |cx: &App| {
            let current = Workspaces::global(cx)?.read(cx).current()?.clone();
            Some((current.id.clone(), current.roots[0].id))
        };
        settle(&mut cx, &|cx| place(cx).is_some());
        let (ws, _) = cx.update(|cx| place(cx)).unwrap();
        let (cwd, ws_id) = (project.to_string_lossy().into_owned(), ws.0.clone());
        let id = daemon.with_client(async move |client| {
            client
                .request::<agm::Create>(CreateSessionParams {
                    provider: "fake".into(),
                    cwd,
                    workspace: Some(ws_id),
                    title: Some("Review the cart".into()),
                    mode: None,
                    from: None,
                })
                .await
                .unwrap()
                .id
        });
        settle(&mut cx, &|cx| {
            crate::chat::list::ChatList::global(cx).is_some_and(|l| l.read(cx).info(&id).is_some())
        });
        let dock = cx.update(|cx| shell.read(cx).dock().clone());
        let session = id.clone();
        cx.update_window(handle, |_, window, cx| {
            crate::chat::open_session(dock.downgrade(), session, window, cx)
        })
        .unwrap();
        let chat = cx.update(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .find_map(|t| dock.tab(t.id)?.downcast::<ChatTab>())
                .unwrap()
        });
        // The user asks; the agent's review comes back.
        let composer = cx.update(|cx| chat.read(cx).composer().clone());
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("Review the cart", window, cx));
            let focus = composer.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            window.dispatch_keystroke(Keystroke::parse("enter").unwrap(), cx);
        })
        .unwrap();
        let store = cx.update(|cx| questions_for(&ws, cx)).unwrap();
        settle(&mut cx, &|cx| store.read(cx).open().len() == 3);
        shoot(&mut cx, format!("coqo-findings-chat-{name}.png"));

        // The critical one, open at its code.
        let critical = cx.update(|cx| {
            store
                .read(cx)
                .open()
                .into_iter()
                .find(|q| q.origin.is_some_and(|o| o.level == FindingLevel::Critical))
                .map(|q| q.id)
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_action(Box::new(OpenQuestion { id: critical.0 }), cx);
        })
        .unwrap();
        let tab = |cx: &App| {
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id)?.downcast::<FileEditor>())
        };
        settle(&mut cx, &|cx| {
            tab(cx).is_some_and(|t| t.read(cx).popover().is_some())
        });
        shoot(&mut cx, format!("coqo-findings-code-{name}.png"));
    }
}
