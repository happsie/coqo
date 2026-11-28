// SPDX-License-Identifier: Apache-2.0

//! The keyboard shortcuts page, headless: recording, conflicts, resets, the
//! file keeping its comments, and recording not firing anything.

use std::{cell::Cell, path::PathBuf, rc::Rc};

use coqo_ui::{
    Appearance,
    gpui::{
        self, AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::KeybindingsEditor;
use crate::commands::{Command, keyfile, registry};

gpui::actions!(kb_test, [Alpha, Beta, Gamma, Delta]);

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    editor: Entity<KeybindingsEditor>,
    path: PathBuf,
    alpha: Rc<Cell<u32>>,
    gamma: Rc<Cell<u32>>,
    _dir: tempfile::TempDir,
}

fn open_ui(file: &str) -> Ui {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(keyfile::FILE_NAME);
    if !file.is_empty() {
        std::fs::write(&path, file).unwrap();
    }
    let cx = TestAppContext::single();
    let (alpha, gamma) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let (a, g) = (alpha.clone(), gamma.clone());
    let keys_path = path.clone();
    let (window, editor) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        cx.on_action(move |_: &Alpha, _| a.set(a.get() + 1));
        cx.on_action(move |_: &Gamma, _| g.set(g.get() + 1));
        registry::register(
            cx,
            [
                Command::new("test.alpha", "Alpha", "Test", Alpha).key("ctrl-alt-a"),
                Command::new("test.beta", "Beta", "Test", Beta).key("ctrl-alt-b"),
                Command::new("test.gamma", "Gamma", "Test", Gamma),
                // The same keys as Alpha, in another context: no conflict.
                Command::new("test.delta", "Delta", "Other", Delta)
                    .key("ctrl-alt-a")
                    .context("Special"),
            ],
        );
        super::register(cx);
        registry::load_user_keys(keys_path, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1000.), px(700.)),
            })),
            ..Default::default()
        };
        coqo_ui::open_window(options, cx, |window, cx| {
            cx.new(|cx| KeybindingsEditor::new(window, cx))
        })
        .expect("open window")
    });
    let mut ui = Ui {
        cx,
        window,
        editor,
        path,
        alpha,
        gamma,
        _dir: dir,
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

    fn click(&mut self, id: &str) {
        let id = ElementId::Name(id.to_owned().into());
        self.frame(|window, cx| window.click(id, cx));
        self.settle();
    }

    fn shows(&mut self, id: &str) -> bool {
        let id = ElementId::Name(id.to_owned().into());
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(self.window, keys);
        self.settle();
    }

    fn keys_of(&mut self, id: &str) -> Vec<String> {
        self.cx.read(|cx| {
            registry::CommandRegistry::try_get(cx)
                .map(|r| r.keys_of(id).iter().map(|k| k.to_string()).collect())
                .unwrap_or_default()
        })
    }

    fn strokes(&mut self) -> Option<Vec<String>> {
        self.cx.read(|cx| {
            self.editor
                .read(cx)
                .recording
                .as_ref()
                .map(|r| r.strokes.clone())
        })
    }

    fn file(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    fn file_keys(&self) -> keyfile::UserKeys {
        keyfile::parse(&self.file(), cfg!(target_os = "macos"))
    }

    fn record(&mut self, id: &str, keys: &str) {
        self.click(&format!("kb-change-{id}"));
        self.keys(keys);
    }
}

#[test]
fn every_command_is_listed_with_where_its_keys_come_from() {
    let mut ui = open_ui("\"test.beta\" = \"alt+shift+b\"\n");
    for id in ["test.alpha", "test.beta", "test.gamma", "test.delta"] {
        assert!(ui.shows(&format!("kb-row-{id}")), "{id}");
    }
    // Only a command the user's file sets has a Reset.
    assert!(ui.shows("kb-reset-test.beta"));
    assert!(!ui.shows("kb-reset-test.alpha"));
    assert_eq!(ui.keys_of("test.beta"), vec!["alt-shift-b"]);
}

#[test]
fn recording_saves_the_keys_and_applies_them_at_once() {
    let mut ui = open_ui("");
    ui.record("test.gamma", "alt-shift-g");
    assert_eq!(ui.strokes(), Some(vec!["alt-shift-g".to_owned()]));
    assert!(ui.shows("kb-recording-test.gamma"));
    ui.click("kb-save-test.gamma");
    assert_eq!(ui.strokes(), None);
    assert_eq!(ui.keys_of("test.gamma"), vec!["alt-shift-g"]);
    assert_eq!(ui.file_keys().overrides["test.gamma"], vec!["alt-shift-g"]);
    assert!(ui.file().contains("alt+shift+g"), "{}", ui.file());
    // Live: the new keys run the command.
    assert_eq!(ui.gamma.get(), 0);
    ui.keys("alt-shift-g");
    assert_eq!(ui.gamma.get(), 1);
}

#[test]
fn a_chord_is_recorded_whole() {
    let mut ui = open_ui("");
    ui.record("test.gamma", "ctrl-alt-k ctrl-alt-s");
    assert_eq!(
        ui.strokes(),
        Some(vec!["ctrl-alt-k".to_owned(), "ctrl-alt-s".to_owned()])
    );
    ui.click("kb-save-test.gamma");
    assert_eq!(ui.keys_of("test.gamma"), vec!["ctrl-alt-k ctrl-alt-s"]);
    ui.keys("ctrl-alt-k ctrl-alt-s");
    assert_eq!(ui.gamma.get(), 1);
}

#[test]
fn recording_fires_no_shortcut_and_escape_cancels() {
    let mut ui = open_ui("");
    ui.click("kb-change-test.gamma");
    // Alpha's own keys are only recorded.
    ui.keys("ctrl-alt-a");
    assert_eq!(ui.alpha.get(), 0);
    assert_eq!(ui.strokes(), Some(vec!["ctrl-alt-a".to_owned()]));
    ui.keys("escape");
    assert_eq!(ui.strokes(), None);
    assert!(ui.file().is_empty(), "nothing is written: {}", ui.file());
    // Not recording any more: the shortcut works again.
    ui.keys("ctrl-alt-a");
    assert_eq!(ui.alpha.get(), 1);
}

#[test]
fn keys_another_command_has_ask_before_replacing() {
    let mut ui = open_ui("");
    ui.record("test.gamma", "ctrl-alt-a");
    ui.click("kb-save-test.gamma");
    // Named, and nothing written yet.
    assert!(ui.shows("kb-conflict-test.gamma"));
    assert!(ui.file().is_empty());
    assert_eq!(ui.keys_of("test.alpha"), vec!["ctrl-alt-a"]);
    // Cancel keeps everything as it was.
    ui.click("kb-cancel-test.gamma");
    assert_eq!(ui.strokes(), None);
    assert!(ui.file().is_empty());
    assert!(!ui.shows("kb-conflict-test.gamma"));

    ui.record("test.gamma", "ctrl-alt-a");
    ui.click("kb-save-test.gamma");
    ui.click("kb-replace-test.gamma");
    assert_eq!(ui.keys_of("test.gamma"), vec!["ctrl-alt-a"]);
    assert!(ui.keys_of("test.alpha").is_empty());
    let file = ui.file_keys();
    assert_eq!(file.overrides["test.gamma"], vec!["ctrl-alt-a"]);
    assert!(file.overrides["test.alpha"].is_empty());
    ui.keys("ctrl-alt-a");
    assert_eq!((ui.gamma.get(), ui.alpha.get()), (1, 0));
    let problems = ui
        .cx
        .read(|cx| registry::CommandRegistry::try_get(cx).unwrap().problems());
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn the_same_keys_in_another_context_are_no_conflict() {
    let mut ui = open_ui("");
    // Delta is in the `Special` context and has Alpha's keys by default.
    ui.record("test.beta", "ctrl-alt-a");
    ui.click("kb-save-test.beta");
    // Beta is global, so Alpha conflicts, Delta does not.
    let conflicts = ui.cx.read(|cx| {
        ui.editor
            .read(cx)
            .recording
            .as_ref()
            .map(|r| r.conflicts.clone())
    });
    assert_eq!(conflicts, Some(vec!["test.alpha".into()]));
}

#[test]
fn reset_puts_one_or_every_command_back() {
    let mut ui = open_ui("");
    ui.record("test.alpha", "alt-shift-a");
    ui.click("kb-save-test.alpha");
    ui.record("test.beta", "alt-shift-b");
    ui.click("kb-save-test.beta");
    assert_eq!(ui.keys_of("test.alpha"), vec!["alt-shift-a"]);

    ui.click("kb-reset-test.alpha");
    assert_eq!(ui.keys_of("test.alpha"), vec!["ctrl-alt-a"]);
    assert!(!ui.file_keys().overrides.contains_key("test.alpha"));
    assert!(ui.file_keys().overrides.contains_key("test.beta"));

    // Reset all asks first.
    ui.click("kb-reset-all");
    assert!(ui.shows("kb-reset-all-confirm"));
    ui.click("kb-reset-all-cancel");
    assert_eq!(ui.keys_of("test.beta"), vec!["alt-shift-b"]);
    ui.click("kb-reset-all");
    ui.click("kb-reset-all-confirm");
    assert_eq!(ui.keys_of("test.beta"), vec!["ctrl-alt-b"]);
    assert!(ui.file_keys().overrides.is_empty());
    assert!(!ui.shows("kb-reset-all"));
}

#[test]
fn a_hand_edited_file_keeps_its_comments_and_other_entries() {
    let mut ui = open_ui(
        "# my shortcuts\n\"other.mod_command\" = \"alt+shift+m\"  # keep this\n\n# the diff\n\"test.beta\" = [\"alt+shift+b\", \"f5\"]\n",
    );
    ui.record("test.gamma", "alt-shift-g");
    ui.click("kb-save-test.gamma");
    ui.click("kb-reset-test.beta");
    let file = ui.file();
    for kept in ["# my shortcuts", "# keep this", "other.mod_command"] {
        assert!(file.contains(kept), "{kept} lost:\n{file}");
    }
    assert!(!file.contains("test.beta"), "{file}");
    assert!(file.contains("test.gamma"), "{file}");
    assert!(ui.file_keys().problems.is_empty());
}

#[test]
fn a_file_that_is_not_toml_is_left_alone_and_reported() {
    let mut ui = open_ui("this is = not [toml");
    ui.record("test.gamma", "alt-shift-g");
    ui.click("kb-save-test.gamma");
    assert_eq!(ui.file(), "this is = not [toml");
    assert!(ui.shows("kb-problems"));
}

#[test]
fn the_search_finds_commands_by_name_and_by_keys() {
    let mut ui = open_ui("");
    let search = |ui: &mut Ui, text: &str| {
        let text = text.to_owned();
        let query = ui.cx.read(|cx| ui.editor.read(cx).query.clone());
        ui.cx
            .update_window(ui.window, |_, window, cx| {
                query.update(cx, |q, cx| q.set_text(text, window, cx))
            })
            .unwrap();
        ui.settle();
    };
    search(&mut ui, "gam");
    assert!(ui.shows("kb-row-test.gamma"));
    assert!(!ui.shows("kb-row-test.alpha"));
    search(&mut ui, "ctrl alt b");
    assert!(ui.shows("kb-row-test.beta"));
    assert!(!ui.shows("kb-row-test.gamma"));
    search(&mut ui, "nothing like this");
    assert!(ui.shows("kb-empty"));
}

#[test]
fn the_palette_can_open_the_page() {
    let ui = open_ui("");
    let found = ui.cx.read(|cx| {
        registry::CommandRegistry::try_get(cx)
            .and_then(|r| r.get(super::OPEN_COMMAND).map(|c| c.title.to_string()))
    });
    assert_eq!(found.as_deref(), Some("Open keyboard shortcuts"));
}

/// Offscreen screenshots of the page, dark and light, into
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn keybindings_screenshots() {
    use coqo_ui::test::render_offscreen;

    gpui::actions!(kb_shot, [A1, A2, A3, A4, A5, A6, A7, A8]);

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let file_dir = tempfile::tempdir().unwrap();
    let path = file_dir.path().join(keyfile::FILE_NAME);
    std::fs::write(
        &path,
        "# my shortcuts\n\"terminal.new\" = \"alt+shift+t\"\n",
    )
    .unwrap();
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let keys_path = path.clone();
        let image = render_offscreen(appearance, size(1100., 820.), move |window, cx| {
            registry::register(
                cx,
                [
                    Command::new("command_palette.open", "Show all commands", "Palette", A1)
                        .key("ctrl-shift-p"),
                    Command::new("command_palette.search", "Search", "Palette", A2).key("ctrl-k"),
                    Command::new("settings.open", "Open settings", "Preferences", A3).key("ctrl-,"),
                    Command::new(
                        "settings.open_keyboard",
                        "Open keyboard shortcuts",
                        "Preferences",
                        A4,
                    ),
                    Command::new("terminal.new", "New terminal", "Terminal", A5).key("ctrl-`"),
                    Command::new("dock.split_right", "Split right", "View", A6)
                        .key("ctrl-\\")
                        .context("Dock"),
                    Command::new("dock.close_tab", "Close tab", "View", A7)
                        .key("ctrl-w")
                        .context("Dock"),
                    Command::new("diff.next_hunk", "Next hunk", "Diff", A8)
                        .key("j")
                        .key("down")
                        .context("DiffReview"),
                ],
            );
            registry::load_user_keys(keys_path, cx);
            let editor = cx.new(|cx| KeybindingsEditor::new(window, cx));
            editor.update(cx, |e, cx| {
                e.record("settings.open_keyboard".into(), window, cx);
                let os = cx.platform_info().os;
                let key = gpui::Keystroke::parse_with_os("ctrl-k", os).unwrap();
                e.take_key(&key, os, cx);
                e.save(cx);
            });
            editor
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-keyboard-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
