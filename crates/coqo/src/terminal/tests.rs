// SPDX-License-Identifier: Apache-2.0

//! Terminal tabs against an in-memory backend, on GPUI's headless test platform.

use std::{cell::RefCell, rc::Rc};

use coqo_protocol::methods::terminal::{
    Color, Cursor, CursorShape, Frame, Modes, Row, RowUpdate, Screen, ScrollTo, Span, SpawnParams,
    Style, TermSize, TerminalEvent, TerminalId, TerminalInfo, TerminalSnapshot, TerminalStatus,
    flags,
};
use coqo_ui::{
    Appearance,
    dock::{Dock, Edge, TabItem as _, store::MemoryLayoutStore},
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, Context, Entity, Keystroke,
        Point, ScrollDelta, Window, WindowBounds, WindowOptions, point, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use futures::channel::{mpsc, oneshot};

use super::backend::{TerminalBackend, Update};
use super::view::ViewState;
use super::{COPY_PASTE, TerminalView};
use crate::shell::{DEFAULT_WORKSPACE, Shell};

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Spawn,
    Attach(TerminalId),
    Input(TerminalId, String),
    Resize(TerminalId, TermSize),
    Scroll(TerminalId, ScrollTo),
    Close(TerminalId),
}

/// Records calls; tests push updates to attached views.
#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<Call>>,
    spawned: RefCell<Vec<SpawnParams>>,
    feeds: RefCell<Vec<(TerminalId, mpsc::UnboundedSender<Update>)>>,
}

impl Fake {
    fn push(&self, id: TerminalId, update: Update) {
        for (_, tx) in self.feeds.borrow().iter().filter(|(t, _)| *t == id) {
            let _ = tx.unbounded_send(update.clone());
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn inputs(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Input(_, s) => Some(s),
                _ => None,
            })
            .collect()
    }
}

impl TerminalBackend for Fake {
    fn spawn(&self, params: SpawnParams) -> oneshot::Receiver<Result<TerminalInfo, String>> {
        self.calls.borrow_mut().push(Call::Spawn);
        self.spawned.borrow_mut().push(params.clone());
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(info(ID, params.size)));
        rx
    }
    fn attach(&self, id: TerminalId) -> mpsc::UnboundedReceiver<Update> {
        self.calls.borrow_mut().push(Call::Attach(id));
        let (tx, rx) = mpsc::unbounded();
        self.feeds.borrow_mut().push((id, tx));
        rx
    }
    fn input(&self, id: TerminalId, data: String) {
        self.calls.borrow_mut().push(Call::Input(id, data));
    }
    fn resize(&self, id: TerminalId, size: TermSize) {
        self.calls.borrow_mut().push(Call::Resize(id, size));
    }
    fn scroll(&self, id: TerminalId, scroll: ScrollTo) {
        self.calls.borrow_mut().push(Call::Scroll(id, scroll));
    }
    fn close(&self, id: TerminalId) {
        self.calls.borrow_mut().push(Call::Close(id));
    }
}

const ID: TerminalId = TerminalId(7);

fn info(id: TerminalId, size: TermSize) -> TerminalInfo {
    TerminalInfo {
        id,
        title: "zsh".into(),
        program: "/bin/zsh".into(),
        cwd: Some("/home/me/coqo".into()),
        size,
        pid: Some(4242),
        status: TerminalStatus::Running,
        last_command_exit: None,
    }
}

fn plain(col: u16, text: &str) -> Span {
    Span {
        col,
        text: text.into(),
        wide: false,
        style: Style::default(),
    }
}

fn styled(col: u16, text: &str, fg: u8, bits: u16) -> Span {
    Span {
        style: Style {
            fg: Color::Indexed(fg),
            bg: Color::Default,
            flags: bits,
        },
        ..plain(col, text)
    }
}

/// A screen with `rows` (each a list of spans) and the cursor at `cursor`.
fn screen(size: TermSize, rows: Vec<Vec<Span>>, cursor: (u16, u16)) -> Screen {
    let mut s = Screen::blank(size);
    for (i, spans) in rows.into_iter().enumerate() {
        s.rows[i] = Row { spans };
    }
    s.cursor = Some(Cursor {
        row: cursor.0,
        col: cursor.1,
        shape: CursorShape::Block,
    });
    s
}

fn snapshot(s: Screen) -> Update {
    Update::Snapshot(TerminalSnapshot {
        seq: 3,
        info: info(ID, s.size),
        screen: s,
    })
}

struct Harness {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    fake: Rc<Fake>,
}

fn open_with(store: MemoryLayoutStore) -> Harness {
    let cx = TestAppContext::single();
    let fake = Rc::new(Fake::default());
    let backend: Rc<dyn TerminalBackend> = fake.clone();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        crate::shell::init(cx);
        super::init(backend, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        coqo_ui::open_window(options, cx, |window, cx| {
            cx.new(|cx| Shell::new(Rc::new(store), DEFAULT_WORKSPACE, window, cx))
        })
        .expect("open shell window")
    });
    Harness {
        cx,
        window,
        shell,
        fake,
    }
}

impl Harness {
    fn frame<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx.run_until_parked();
        self.cx
            .update_window(self.window, |_, window, cx| {
                window.render_frame(cx);
                f(window, cx)
            })
            .unwrap()
    }

    fn dock<R>(&mut self, f: impl FnOnce(&mut Dock, &mut Window, &mut Context<Dock>) -> R) -> R {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx
            .update_window(self.window, |_, window, cx| {
                dock.update(cx, |dock, cx| f(dock, window, cx))
            })
            .unwrap()
    }

    /// Open a tab on terminal [`ID`] and show `s` in it.
    fn open_terminal(&mut self, s: Option<Screen>) {
        self.dock(|dock, window, cx| {
            let tab = super::tab(ID, cx).expect("terminals are enabled");
            dock.open_tab(tab, window, cx);
        });
        if let Some(s) = s {
            self.fake.push(ID, snapshot(s));
        }
        self.frame(|_, _| {});
    }

    fn view(&mut self) -> Entity<TerminalView> {
        self.dock(|dock, _, _| {
            let id = dock.active_tab().expect("a tab");
            dock.tab(id).unwrap().downcast::<TerminalView>().unwrap()
        })
    }

    fn press(&mut self, keys: &'static str) {
        self.frame(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
        });
    }

    fn terminal_bounds(&mut self) -> Bounds<coqo_ui::gpui::Pixels> {
        self.frame(|window, _| window.find(("terminal", ID.0 as usize)).bounds())
    }
}

fn prompt_screen() -> Screen {
    screen(
        TermSize::new(80, 24),
        vec![
            vec![plain(0, "$ ls")],
            vec![
                styled(0, "Cargo.toml", 7, 0),
                styled(12, "crates", 4, flags::BOLD),
            ],
            vec![plain(0, "$ ")],
        ],
        (2, 2),
    )
}

#[test]
fn shows_the_mirrored_screen_and_fits_the_pty_to_the_panel() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.open_terminal(Some(prompt_screen()));
    let view = h.view();
    h.cx.read(|cx| {
        let v = view.read(cx);
        assert_eq!(*v.state(), ViewState::Live);
        assert_eq!(v.screen().text(), "$ ls\nCargo.toml  crates\n$");
    });

    let calls = h.fake.calls();
    assert_eq!(calls[0], Call::Attach(ID));
    let Some(Call::Resize(_, size)) = calls.iter().find(|c| matches!(c, Call::Resize(..))) else {
        panic!("no resize: {calls:?}");
    };
    // A 1440×900 window leaves a large panel: well over 80×24 cells.
    assert!(size.cols > 100 && size.rows > 30, "{size:?}");

    // The cursor sits two cells into the third row.
    let term = h.terminal_bounds();
    let cursor = h.frame(|window, _| window.find("terminal-cursor").bounds());
    assert!(cursor.origin.x > term.origin.x && cursor.origin.y > term.origin.y);
    let cell_w = cursor.size.width;
    assert!(cell_w > px(5.) && cell_w < px(12.), "{cell_w:?}");
    let col = ((cursor.origin.x - term.origin.x - px(10.)) / cell_w).round();
    assert_eq!(col, 2.);

    // The tab title comes from the daemon, and the layout saves the id.
    let title = h.cx.read(|cx| view.read(cx).title(cx));
    assert_eq!(title.as_ref(), "zsh");
    let saved = h.dock(|dock, _, cx| dock.snapshot(cx));
    let json = coqo_ui::serde_json::to_string(&saved).unwrap();
    assert!(
        json.contains("\"terminal\"") && json.contains("\"id\":7"),
        "{json}"
    );
}

#[test]
fn keys_go_to_the_pty_in_order() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.open_terminal(Some(prompt_screen()));
    for key in [
        "l", "shift-s", "enter", "ctrl-c", "up", "ctrl-w", "tab", "escape",
    ] {
        h.press(key);
    }
    assert_eq!(
        h.fake.inputs(),
        ["l", "S", "\r", "\x03", "\x1b[A", "\x17", "\t", "\x1b"]
    );
    // Ctrl W went to the shell; the tab is still open.
    assert_eq!(h.dock(|d, _, _| d.tree().tabs().len()), 1);

    // Application cursor mode changes the arrows.
    let mut s = prompt_screen();
    s.modes = Modes {
        app_cursor: true,
        ..Modes::default()
    };
    h.fake.push(ID, snapshot(s));
    h.press("down");
    assert_eq!(h.fake.inputs().last().unwrap(), "\x1bOB");
}

#[test]
fn frames_titles_and_exit_update_the_view() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.open_terminal(Some(prompt_screen()));
    h.fake.push(
        ID,
        Update::Event(TerminalEvent::Frame(Frame {
            size: TermSize::new(80, 24),
            full: false,
            rows: vec![RowUpdate {
                row: 2,
                spans: vec![plain(0, "$ cargo test")],
            }],
            cursor: None,
            modes: Modes::default(),
            display_offset: 0,
            history_size: 0,
        })),
    );
    h.fake.push(
        ID,
        Update::Event(TerminalEvent::Title {
            title: "cargo test".into(),
        }),
    );
    h.frame(|_, _| {});
    let view = h.view();
    h.cx.read(|cx| {
        let v = view.read(cx);
        assert!(v.screen().text().ends_with("$ cargo test"));
        assert_eq!(v.title(cx).as_ref(), "cargo test");
    });
    assert!(h.frame(|w, _| w.try_find("terminal-cursor").is_none()));

    h.fake
        .push(ID, Update::Event(TerminalEvent::Exited { code: Some(1) }));
    h.frame(|_, _| {});
    assert_eq!(
        h.cx.read(|cx| view.read(cx).state().clone()),
        ViewState::Exited(Some(1))
    );
    assert!(h.frame(|w, _| w.try_find("terminal-banner").is_some()));
    // An exited terminal takes no input.
    let before = h.fake.inputs().len();
    h.press("a");
    assert_eq!(h.fake.inputs().len(), before);
}

#[test]
fn closing_the_last_tab_of_a_terminal_closes_it() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.open_terminal(Some(prompt_screen()));
    // A split shows the same terminal twice.
    h.dock(|dock, window, cx| dock.split(Edge::Right, window, cx));
    h.frame(|_, _| {});
    let attaches = |h: &Harness| {
        h.fake
            .calls()
            .iter()
            .filter(|c| **c == Call::Attach(ID))
            .count()
    };
    assert_eq!(attaches(&h), 2);

    let tabs: Vec<_> = h.dock(|d, _, _| d.tree().tabs().iter().map(|t| t.id).collect());
    h.dock(|d, _, cx| d.close_tab(tabs[1], cx));
    h.frame(|_, _| {});
    assert!(!h.fake.calls().contains(&Call::Close(ID)));
    h.dock(|d, _, cx| d.close_tab(tabs[0], cx));
    h.frame(|_, _| {});
    assert!(h.fake.calls().contains(&Call::Close(ID)));
}

#[test]
fn saved_terminal_tabs_reattach_after_a_restart() {
    let store = MemoryLayoutStore::default();
    let mut h = open_with(store.clone());
    h.open_terminal(Some(prompt_screen()));
    h.cx.update(|cx| h.shell.update(cx, |s, cx| s.save_now(cx)));
    drop(h);

    // A new UI process: the layout brings the tab back, attached to the same
    // daemon terminal; nothing was closed.
    let mut h = open_with(store);
    h.frame(|_, _| {});
    assert_eq!(h.fake.calls().first(), Some(&Call::Attach(ID)));
    h.fake.push(ID, snapshot(prompt_screen()));
    h.frame(|_, _| {});
    let view = h.view();
    assert!(h.cx.read(|cx| view.read(cx).screen().text().starts_with("$ ls")));
}

#[test]
fn selection_copies_and_paste_types() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.open_terminal(Some(prompt_screen()));
    let term = h.terminal_bounds();
    let cursor = h.frame(|window, _| window.find("terminal-cursor").bounds());
    let (cw, lh) = (cursor.size.width, cursor.size.height);
    // Drag across "Cargo" on the second row.
    let at = |col: f32, row: f32| {
        point(
            term.origin.x + px(10.) + cw * col + cw / 2.,
            term.origin.y + px(10.) + lh * row + lh / 2.,
        )
    };
    h.frame(|window, cx| window.drag(at(0., 1.), at(4., 1.), cx));
    let view = h.view();
    assert_eq!(
        h.cx.read(|cx| view.read(cx).selected_text()).as_deref(),
        Some("Cargo")
    );
    h.press(COPY_PASTE[0]);
    let copied =
        h.cx.read(|cx| cx.read_from_clipboard().and_then(|i| i.text()));
    assert_eq!(copied.as_deref(), Some("Cargo"));

    h.cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("echo hi\n".into())));
    h.press(COPY_PASTE[1]);
    assert_eq!(h.fake.inputs().last().unwrap(), "echo hi\r");
}

#[test]
fn the_wheel_scrolls_the_scrollback_or_the_program() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.open_terminal(Some(prompt_screen()));
    h.frame(|window, cx| {
        window.scroll(
            ("terminal", ID.0 as usize),
            ScrollDelta::Lines(point(0., 3.)),
            cx,
        );
    });
    let scrolls: Vec<_> = h
        .fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Scroll(_, ScrollTo::Lines(n)) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(scrolls, [3]);

    // Full-screen programs get arrow keys instead.
    let mut s = prompt_screen();
    s.modes.alt_screen = true;
    h.fake.push(ID, snapshot(s));
    h.frame(|window, cx| {
        window.scroll(
            ("terminal", ID.0 as usize),
            ScrollDelta::Lines(point(0., -2.)),
            cx,
        );
    });
    assert_eq!(h.fake.inputs().last().unwrap(), "\x1b[B\x1b[B");
}

#[test]
fn new_terminal_spawns_a_shell_and_opens_it() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.press("ctrl-`");
    h.frame(|_, _| {});
    let calls = h.fake.calls();
    assert_eq!(calls[0], Call::Spawn);
    assert!(calls.contains(&Call::Attach(ID)), "{calls:?}");
    let kinds: Vec<String> =
        h.dock(|d, _, _| d.tree().tabs().iter().map(|t| t.kind.clone()).collect());
    assert_eq!(kinds, ["terminal"]);
}

#[test]
fn a_sign_in_program_runs_in_a_new_terminal_tab() {
    let mut h = open_with(MemoryLayoutStore::default());
    h.frame(|window, cx| {
        window.dispatch_action(
            Box::new(crate::chat::RunInTerminal {
                program: "/home/u/.local/bin/claude".into(),
                args: vec![],
            }),
            cx,
        );
    });
    h.frame(|_, _| {});
    let spawned = h.fake.spawned.borrow().clone();
    assert_eq!(spawned.len(), 1);
    assert_eq!(
        spawned[0].program.as_deref(),
        Some("/home/u/.local/bin/claude")
    );
    assert_eq!(spawned[0].title.as_deref(), Some("claude"));
    let kinds: Vec<String> =
        h.dock(|d, _, _| d.tree().tabs().iter().map(|t| t.kind.clone()).collect());
    assert_eq!(kinds, ["terminal"]);
}

/// The real path end to end: a shell in an in-process `coqod`, reached over
/// the app's daemon connection, typed into through the tab, and closed with
/// it.
#[cfg(unix)]
#[test]
fn a_tab_drives_a_shell_in_a_real_daemon() {
    use std::time::{Duration, Instant};

    use coqo_protocol::methods::terminal::List;

    use super::DaemonBackend;
    use crate::daemon::{self, DaemonConfig};
    use crate::test_support::TestDaemon;

    let tmp = tempfile::tempdir().unwrap();
    let coqod = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let cx = TestAppContext::single();
    // The daemon connection wakes GPUI tasks from its own threads.
    cx.executor().allow_parking();
    let paths = coqod.paths.clone();
    let (window, shell, backend) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths.clone());
        config.spawn = false;
        let daemon = daemon::init(config, cx);
        crate::shell::init(cx);
        let backend = Rc::new(DaemonBackend::new(daemon.read(cx).handle()));
        super::init(backend.clone(), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        let (window, shell) = coqo_ui::open_window(options, cx, |window, cx| {
            let store = Rc::new(MemoryLayoutStore::default());
            cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
        })
        .expect("open shell window");
        (window, shell, backend)
    });
    let mut h = Harness {
        cx,
        window,
        shell,
        fake: Rc::default(),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let wait = |h: &mut Harness, what: &str, done: &mut dyn FnMut(&mut Harness) -> bool| {
        loop {
            h.frame(|_, _| {});
            if done(h) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    // Spawned before the connection is up: the call waits for it.
    let mut started = backend.spawn(SpawnParams {
        program: Some("/bin/sh".into()),
        env: [("PS1", "$ "), ("ENV", "")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        ..SpawnParams::default()
    });
    let mut info = None;
    wait(&mut h, "the terminal to start", &mut |_| {
        if let Ok(Some(result)) = started.try_recv() {
            info = Some(result.expect("spawned"));
        }
        info.is_some()
    });
    let id = info.unwrap().id;
    h.dock(|dock, window, cx| {
        let tab = super::tab(id, cx).expect("terminals are enabled");
        dock.open_tab(tab, window, cx);
    });
    let view = h.view();
    let has_line = |h: &mut Harness, line: &str| {
        h.cx.read(|cx| view.read(cx).screen().text().lines().any(|l| l == line))
    };
    wait(&mut h, "the prompt", &mut |h| has_line(h, "$"));
    for key in ["e", "c", "h", "o", "space", "e", "2", "e", "enter"] {
        h.press(key);
    }
    wait(&mut h, "the command's output", &mut |h| has_line(h, "e2e"));
    // The tab fitted the PTY to its panel, well past the default 80x24.
    let fitted = h.cx.read(|cx| view.read(cx).screen().size);
    assert!(fitted.cols > 80 && fitted.rows > 24, "{fitted:?}");

    // Closing the only tab ends the terminal in the daemon.
    h.dock(|dock, _, cx| {
        let tab = dock.active_tab().expect("a tab");
        dock.close_tab(tab, cx);
    });
    let list = |coqod: &TestDaemon| {
        coqod.runtime.block_on(async {
            let (client, _) = coqo_client::connect(
                &coqod.paths,
                coqo_protocol::ClientConfig::new("coqo-tests", "test"),
            )
            .await
            .unwrap();
            client.request::<List>(()).await.unwrap()
        })
    };
    wait(&mut h, "the terminal to close", &mut |_| {
        list(&coqod).is_empty()
    });
}

/// Renders a terminal tab offscreen for visual review. Needs a GPU:
/// `COQO_SHOT_DIR=<dir> cargo test -p coqo -- --ignored terminal_screenshots`.
#[test]
#[ignore = "needs a GPU; run manually"]
fn terminal_screenshots() {
    use coqo_ui::test::render_offscreen;

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let fake = Rc::new(Fake::default());
        let backend: Rc<dyn TerminalBackend> = fake.clone();
        let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
            crate::shell::init(cx);
            super::init(backend, cx);
            let store = Rc::new(MemoryLayoutStore::default());
            cx.new(|cx| {
                let shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                shell.dock().update(cx, |dock, cx| {
                    dock.open_tab(super::tab(ID, cx).unwrap(), window, cx);
                });
                fake.push(ID, snapshot(sample_screen()));
                shell
            })
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-terminal-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}

fn sample_screen() -> Screen {
    let size = TermSize::new(120, 40);
    let prompt = |col: u16| {
        vec![
            styled(col, "~/code/coqo", 2, flags::BOLD),
            styled(col + 12, "main", 5, 0),
            plain(col + 17, "$"),
        ]
    };
    let mut rows = Vec::new();
    let mut p = prompt(0);
    p.push(plain(19, "ls"));
    rows.push(p);
    rows.push(vec![
        plain(0, "Cargo.lock  Cargo.toml  CLAUDE.md  deny.toml  "),
        styled(46, "crates", 4, flags::BOLD),
        plain(52, "  "),
        styled(54, "docs", 4, flags::BOLD),
        plain(58, "  "),
        styled(60, "protocol", 4, flags::BOLD),
    ]);
    let mut p = prompt(0);
    p.push(plain(19, "cargo test -p coqod terminal"));
    rows.push(p);
    rows.push(vec![
        styled(0, "   Compiling", 2, flags::BOLD),
        plain(13, "coqod v0.0.0 (crates/coqod)"),
    ]);
    rows.push(vec![
        styled(0, "    Finished", 2, flags::BOLD),
        plain(
            13,
            "`test` profile [unoptimized + debuginfo] target(s) in 4.2s",
        ),
    ]);
    for name in [
        "printf_output_lands_in_the_grid",
        "an_interactive_shell_echoes_input_and_reports_exit_codes",
        "a_terminal_outlives_its_client_and_reattaches",
    ] {
        let line = format!("test terminal::tests::{name} ... ");
        let col = u16::try_from(line.len()).unwrap();
        rows.push(vec![plain(0, &line), styled(col, "ok", 2, 0)]);
    }
    rows.push(vec![
        plain(0, "test result: "),
        styled(13, "ok", 2, 0),
        plain(15, ". 15 passed; 0 failed; finished in 1.62s"),
    ]);
    rows.push(vec![
        styled(0, "warning", 3, flags::BOLD),
        plain(7, ": 1 flaky test retried"),
    ]);
    rows.push(vec![styled(0, "error", 1, flags::BOLD), plain(5, ": none")]);
    let mut p = prompt(0);
    p.push(plain(19, ""));
    rows.push(p);
    let cursor_row = u16::try_from(rows.len() - 1).unwrap();
    screen(size, rows, (cursor_row, 20))
}
