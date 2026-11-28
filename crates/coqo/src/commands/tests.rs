// SPDX-License-Identifier: Apache-2.0

//! The command palette in a workbench window: fuzzy search, running a
//! command where the focus was, commands of other contexts hidden, typing
//! that never fires a shortcut, the user's keybindings, and mod commands
//! from a real daemon.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_ui::{
    Appearance,
    dock::{AnyTab, TabItem, store::MemoryLayoutStore},
    gpui::{
        self, AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FocusHandle,
        Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Point, Render,
        SharedString, Styled as _, Window, WindowBounds, WindowOptions, div, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::{
    Command, CommandFiles, CommandRegistry, OPEN_COMMANDS_KEY, OPEN_SEARCH_KEY,
    mods::{ModCommands, RunModCommand},
    register,
};
use crate::{
    daemon,
    shell::{DEFAULT_WORKSPACE, Shell},
    test_support::{TIMEOUT, TestDaemon},
};

gpui::actions!(palette_test, [Say, Probe, Plain]);

/// The sample mod's command (`mods/hello`), built into the daemon.
const GREET: &str = "hello.greet";

/// A tab with its own key context, `Probe`, that counts [`Probe`]s.
struct ProbeTab {
    focus: FocusHandle,
    probes: Rc<RefCell<usize>>,
}

impl TabItem for ProbeTab {
    fn kind(&self) -> SharedString {
        "probe".into()
    }
    fn title(&self, _: &App) -> SharedString {
        "Probe".into()
    }
    fn focus_handle(&self, _: &App) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }
}

impl Focusable for ProbeTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ProbeTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Probe")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Probe, _, _| *this.probes.borrow_mut() += 1))
            .size_full()
            .child("probe")
    }
}

#[derive(Default, Clone)]
struct Counts {
    say: Rc<RefCell<usize>>,
    plain: Rc<RefCell<usize>>,
    probe: Rc<RefCell<usize>>,
}

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    counts: Counts,
}

fn test_commands() -> Vec<Command> {
    vec![
        Command::new("test.say", "Say something", "Test", Say).key("ctrl-alt-s"),
        Command::new("test.probe", "Poke the probe", "Test", Probe).context("Probe"),
        Command::new("test.plain", "Plain key", "Test", Plain).key("g"),
    ]
}

fn open_ui(daemon: Option<&TestDaemon>) -> Ui {
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let counts = Counts::default();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        if let Some(d) = daemon {
            let mut config = daemon::DaemonConfig::at(d.paths.clone());
            config.spawn = false;
            daemon::init(config, cx);
        }
        crate::workspace::init(None, cx);
        crate::shell::init(cx);
        register(cx, test_commands());
        let (say, plain) = (counts.say.clone(), counts.plain.clone());
        cx.on_action(move |_: &Say, _| *say.borrow_mut() += 1);
        cx.on_action(move |_: &Plain, _| *plain.borrow_mut() += 1);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        coqo_ui::open_window(options, cx, |window, cx| {
            cx.new(|cx| {
                Shell::new(
                    Rc::new(MemoryLayoutStore::default()),
                    DEFAULT_WORKSPACE,
                    window,
                    cx,
                )
            })
        })
        .expect("open shell window")
    });
    let mut ui = Ui {
        cx,
        window,
        shell,
        counts,
    };
    ui.settle();
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

    fn settle(&mut self) {
        for _ in 0..3 {
            self.cx.run_until_parked();
            self.frame(|_, _| ());
        }
    }

    fn press(&mut self, keys: &str) {
        for k in keys.split_whitespace() {
            self.frame(|window, cx| window.press(k, cx));
            self.cx.run_until_parked();
        }
        self.settle();
    }

    fn type_text(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(self.window, keys);
        self.settle();
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&mut Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.settle();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn palette_open(&mut self) -> bool {
        self.cx.read(|cx| self.shell.read(cx).palette().is_some())
    }

    /// The ids the palette lists, in order.
    fn listed(&mut self) -> Vec<String> {
        self.cx.read(|cx| {
            self.shell
                .read(cx)
                .palette()
                .map(|p| {
                    p.read(cx)
                        .items()
                        .iter()
                        .map(|i| i.id.to_string())
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    fn item(&mut self, id: &str) -> Option<super::PaletteItem> {
        self.cx.read(|cx| {
            let palette = self.shell.read(cx).palette()?.read(cx);
            palette.items().iter().find(|i| i.id == id).cloned()
        })
    }

    fn open_probe(&mut self) {
        let probes = self.counts.probe.clone();
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.frame(move |window, cx| {
            let focus = cx.focus_handle();
            let tab = AnyTab::new(cx.new(|_| ProbeTab { focus, probes }));
            dock.update(cx, |dock, cx| dock.open_tab(tab, window, cx));
        });
        self.settle();
    }
}

#[test]
fn the_palette_runs_a_fuzzy_match_where_the_focus_was() {
    let mut ui = open_ui(None);
    ui.press(OPEN_COMMANDS_KEY);
    assert!(ui.palette_open());
    let text = ui.cx.read(|cx| {
        let palette = ui.shell.read(cx).palette().unwrap().read(cx);
        (palette.text(cx), palette.mode())
    });
    assert_eq!(text, (">".to_owned(), Some(">")));
    let all = ui.listed();
    assert!(all.contains(&"test.say".to_owned()), "{all:?}");
    // Only works in the probe tab, and the focus is elsewhere.
    assert!(!all.contains(&"test.probe".to_owned()), "{all:?}");
    // The palette's own openers aren't listed.
    assert!(!all.contains(&super::OPEN_COMMANDS.to_owned()), "{all:?}");

    ui.type_text("s a y s m");
    assert_eq!(ui.listed().first().map(String::as_str), Some("test.say"));
    let item = ui.item("test.say").unwrap();
    assert_eq!(item.title, "Say something");
    assert_eq!(item.prefix.as_deref(), Some("Test"));
    assert!(!item.highlights.is_empty());
    let keys = if cfg!(target_os = "macos") {
        "⌃⌥S"
    } else {
        "Ctrl Alt S"
    };
    assert_eq!(item.keys.as_deref(), Some(keys));

    ui.press("enter");
    assert!(!ui.palette_open());
    assert_eq!(*ui.counts.say.borrow(), 1);
    // The focus went back to the dock.
    let dock_focused = ui.frame(|window, cx| {
        let dock = Shell::of_window(window.window_handle(), cx).unwrap();
        let focus = dock.read(cx).dock().read(cx).focus_handle(cx);
        focus.contains_focused(window, cx)
    });
    assert!(dock_focused);

    // Recently used comes first next time.
    ui.press(OPEN_COMMANDS_KEY);
    assert_eq!(ui.listed().first().map(String::as_str), Some("test.say"));
    assert!(ui.item("test.say").unwrap().recent);
    ui.press("escape");
    assert!(!ui.palette_open());
    assert_eq!(*ui.counts.say.borrow(), 1);

    // In the probe tab, its command is there and runs in the tab.
    ui.open_probe();
    ui.press(OPEN_COMMANDS_KEY);
    assert!(ui.listed().contains(&"test.probe".to_owned()));
    ui.type_text("p o k e");
    assert_eq!(ui.listed().first().map(String::as_str), Some("test.probe"));
    ui.press("enter");
    assert_eq!(*ui.counts.probe.borrow(), 1);
    assert_eq!(
        ui.cx
            .read(|cx| cx.global::<CommandRegistry>().recent().to_vec()),
        vec![SharedString::from("test.probe"), "test.say".into()]
    );
}

#[test]
fn the_header_search_opens_the_default_mode_and_arrows_move() {
    let mut ui = open_ui(None);
    ui.frame(|window, cx| window.click("command-search", cx));
    ui.settle();
    assert!(ui.palette_open());
    // The default mode is the file finder (#162); `>` switches to the
    // commands.
    let mode = |ui: &mut Ui| {
        ui.cx.read(|cx| {
            let palette = ui.shell.read(cx).palette().unwrap().read(cx);
            (palette.text(cx), palette.mode())
        })
    };
    assert_eq!(mode(&mut ui), (String::new(), Some("")));
    ui.type_text(">");
    assert_eq!(mode(&mut ui), (">".to_owned(), Some(">")));
    assert!(ui.listed().len() > 3);
    let first = ui.listed()[0].clone();
    let selected = |ui: &mut Ui| {
        ui.cx.read(|cx| {
            ui.shell
                .read(cx)
                .palette()
                .unwrap()
                .read(cx)
                .selected()
                .map(|i| i.id.to_string())
        })
    };
    assert_eq!(selected(&mut ui), Some(first.clone()));
    ui.press("down");
    assert_eq!(selected(&mut ui), Some(ui.listed()[1].clone()));
    ui.press("up up");
    assert_eq!(selected(&mut ui), ui.listed().last().cloned());
    // The search key opens it too, and asked again closes it.
    ui.press("escape");
    ui.press(OPEN_SEARCH_KEY);
    assert!(ui.palette_open());
    ui.press(OPEN_SEARCH_KEY);
    assert!(!ui.palette_open());
}

#[test]
fn typing_in_the_palette_fires_no_shortcut() {
    let mut ui = open_ui(None);
    // Outside a text field the plain key runs its command.
    ui.press("g");
    assert_eq!(*ui.counts.plain.borrow(), 1);
    ui.press(OPEN_COMMANDS_KEY);
    ui.type_text("g g p l a i n");
    assert_eq!(*ui.counts.plain.borrow(), 1);
    let text = ui
        .cx
        .read(|cx| ui.shell.read(cx).palette().unwrap().read(cx).text(cx));
    assert_eq!(text, ">ggplain");
    assert!(ui.cx.read(coqo_ui::keys::unguarded_bindings).is_empty());
}

#[test]
fn user_keybindings_override_defaults_and_report_problems() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(super::keyfile::FILE_NAME);
    std::fs::write(
        &path,
        r#"
"test.say" = "mod+alt+y"
"test.plain" = []
"test.probe" = "ctrl+alt+p"
"nobody.nothing" = "mod+u"
"test.bad" = "hyper+q"
"agents.show" = "mod+shift+n"
"#,
    )
    .unwrap();
    let mut ui = open_ui(None);
    ui.cx.update(|cx| {
        super::use_files(
            CommandFiles {
                keybindings: Some(path.clone()),
                recent: None,
            },
            cx,
        )
    });
    ui.settle();
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    // The default is gone, the user's key runs it.
    ui.press("ctrl-alt-s");
    assert_eq!(*ui.counts.say.borrow(), 0);
    ui.press(&format!("{modifier}-alt-y"));
    assert_eq!(*ui.counts.say.borrow(), 1);
    // Unbound.
    ui.press("g");
    assert_eq!(*ui.counts.plain.borrow(), 0);
    // The palette shows the user's key.
    ui.press(OPEN_COMMANDS_KEY);
    let keys = ui.item("test.say").unwrap().keys.unwrap();
    assert!(keys.contains('Y'), "{keys}");
    ui.press("escape");

    let problems = ui
        .cx
        .read(|cx| cx.global::<CommandRegistry>().last_problems().to_vec());
    let has = |needle: &str| problems.iter().any(|p| p.contains(needle));
    assert!(has("`nobody.nothing`: no command"), "{problems:?}");
    assert!(has("`test.bad`: "), "{problems:?}");
    assert!(has("`chat.new` and `agents.show`"), "{problems:?}");

    // Reloading reads the file again.
    std::fs::write(&path, "\"test.say\" = \"mod+alt+z\"\n").unwrap();
    ui.frame(|window, cx| window.dispatch_action(Box::new(super::ReloadKeybindings), cx));
    ui.settle();
    ui.press(&format!("{modifier}-alt-z"));
    assert_eq!(*ui.counts.say.borrow(), 2);
    ui.press("g");
    assert_eq!(*ui.counts.plain.borrow(), 1);
    let problems = ui
        .cx
        .read(|cx| cx.global::<CommandRegistry>().last_problems().to_vec());
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn commands_from_a_mod_appear_and_can_be_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
    let mut ui = open_ui(Some(&daemon));
    ui.wait_for("the mods' commands", |ui| {
        ui.cx
            .read(|cx| cx.global::<CommandRegistry>().get(GREET).is_some())
    });
    // In the palette, under the mod's category.
    ui.press(OPEN_COMMANDS_KEY);
    ui.type_text("h e l l o");
    let item = ui.item(GREET).expect("hello.greet is listed");
    assert_eq!(item.title, "Say hello");
    assert_eq!(item.prefix.as_deref(), Some("Hello"));
    ui.press("enter");
    let last = |ui: &mut Ui| {
        ui.cx.read(|cx| {
            ModCommands::global(cx)
                .and_then(|m| m.read(cx).last().cloned())
                .map(|(id, outcome)| (id.to_string(), outcome))
        })
    };
    ui.wait_for("the command to run", |ui| last(ui).is_some());
    assert_eq!(
        last(&mut ui),
        Some((GREET.to_owned(), Ok(serde_json::json!("Hello, world!"))))
    );

    // Bound by the user, it runs from the keyboard.
    let path = tmp.path().join(super::keyfile::FILE_NAME);
    std::fs::write(&path, "\"hello.greet\" = \"ctrl+alt+h\"\n").unwrap();
    ui.cx.update(|cx| {
        super::use_files(
            CommandFiles {
                keybindings: Some(path),
                recent: None,
            },
            cx,
        )
    });
    ui.cx.update(|cx| {
        ModCommands::global(cx)
            .unwrap()
            .update(cx, |m, _| m.clear_last_for_tests())
    });
    ui.press("ctrl-alt-h");
    ui.wait_for("the bound command to run", |ui| last(ui).is_some());
    assert_eq!(last(&mut ui).unwrap().0, GREET);
    let action = ui.cx.read(|cx| {
        cx.global::<CommandRegistry>()
            .get(GREET)
            .map(|c| c.runs(&RunModCommand { id: GREET.into() }))
    });
    assert_eq!(action, Some(true));
}

/// Renders the palette offscreen to PNGs for docs/design/palette.md. Needs
/// a GPU: `cargo test -p coqo -- --ignored palette_screenshots`. Writes to
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn palette_screenshots() {
    use coqo_ui::{gpui::size, test::render_offscreen};

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    for (query, suffix) in [(">", ""), (">new", "-query")] {
        for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
            let image = render_offscreen(appearance, size(1440., 900.), |window, cx| {
                crate::workspace::init(None, cx);
                crate::shell::init(cx);
                let registry = super::registry::registry(cx);
                for id in ["chat.new", "dock.split_right"] {
                    registry.touch(&id.into());
                }
                cx.new(|cx| {
                    let shell = Shell::new(
                        Rc::new(MemoryLayoutStore::default()),
                        DEFAULT_WORKSPACE,
                        window,
                        cx,
                    );
                    // After the first frame, so the focus path is known.
                    cx.spawn_in(window, async move |this, cx| {
                        this.update_in(cx, |shell, window, cx| {
                            shell.toggle_palette(query, window, cx)
                        })
                        .ok();
                    })
                    .detach();
                    shell
                })
            })
            .expect("offscreen render");
            let path = dir.join(format!("coqo-palette{suffix}-{name}.png"));
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        }
    }
}
