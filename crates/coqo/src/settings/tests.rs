// SPDX-License-Identifier: Apache-2.0

//! The Settings tab against a real daemon with a temporary config
//! directory: opening it, switching the theme live, toggling auto-reveal,
//! and edits by hand showing up.

use std::{
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use coqo_ui::{
    ActiveTokens as _, Appearance,
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Point, Window,
        WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};

use super::page::{self, OpenSettings, SettingsPage};
use super::{AUTO_REVEAL, THEME};
use crate::daemon::{self, DaemonConfig};
use crate::shell::{DEFAULT_WORKSPACE, Shell};
use crate::test_support::{Fixture, TIMEOUT};
use crate::workspace::{self, Workspaces};

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
    config: PathBuf,
    _fixture: Fixture,
}

fn name(id: &str) -> ElementId {
    ElementId::Name(id.to_owned().into())
}

fn open_ui(user_settings: &str) -> Ui {
    let fixture = Fixture::new();
    let config = fixture.daemon.config_dir();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join(super::FILE_NAME), user_settings).unwrap();
    let mut cx = TestAppContext::single();
    cx.executor().allow_parking();
    let paths = fixture.daemon.paths.clone();
    let folder = fixture.project.clone();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(Some(folder), cx);
        crate::shell::init(cx);
        super::init(cx);
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
    cx.update_window(window, |_, window, _| window.activate_window())
        .unwrap();
    let mut ui = Ui {
        cx,
        window,
        shell,
        config: config.join(super::FILE_NAME),
        _fixture: fixture,
    };
    ui.wait_for("the workspace", |ui| {
        ui.cx
            .read(|cx| Workspaces::global(cx).and_then(|w| w.read(cx).current().cloned()))
            .is_some()
    });
    ui.wait_for("the settings", |ui| {
        ui.cx.read(|cx| super::get(cx).snapshot().is_some())
    });
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

    fn click(&mut self, id: &str) {
        let id = name(id);
        self.frame(|window, cx| window.click(id, cx));
    }

    fn shows(&mut self, id: &str) -> bool {
        let id = name(id);
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn page(&mut self) -> Option<Entity<SettingsPage>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            let id = dock.find_tab(page::KIND, page::KIND, cx)?;
            dock.tab(id)?.downcast::<SettingsPage>()
        })
    }

    fn theme(&mut self) -> Appearance {
        self.cx.read(|cx| cx.tokens().choice)
    }

    fn file(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap_or_default()
    }
}

#[test]
fn the_page_switches_the_theme_live_and_saves_settings() {
    let mut ui = open_ui("# My settings\n[appearance]\ntheme = \"dark\"\n");
    ui.wait_for("the dark theme", |ui| ui.theme() == Appearance::Dark);

    // The palette command (and its key) opens the tab; again, it comes
    // forward instead of a second one.
    assert!(ui.cx.update(|cx| {
        crate::commands::registry::registry(cx)
            .get("settings.open")
            .is_some()
    }));
    ui.frame(|window, cx| window.dispatch_action(Box::new(OpenSettings), cx));
    ui.wait_for("the settings tab", |ui| ui.page().is_some());
    ui.frame(|window, cx| window.dispatch_action(Box::new(OpenSettings), cx));
    let tabs = ui.cx.read(|cx| {
        let dock = ui.shell.read(cx).dock().clone();
        let dock = dock.read(cx);
        dock.tree()
            .tabs()
            .into_iter()
            .filter(|t| t.kind == page::KIND)
            .count()
    });
    assert_eq!(tabs, 1);

    // The sections nav: the schema's sections and the keyboard page.
    ui.wait_for("the nav", |ui| ui.shows("settings-nav-appearance"));
    assert!(ui.shows("settings-nav-files"));
    assert!(ui.shows("settings-nav-claude"));
    assert!(ui.shows("settings-nav-keyboard"));

    // Appearance is first: Light applies at once and is saved.
    ui.click("setting-appearance.theme-light");
    ui.wait_for("the light theme", |ui| ui.theme() == Appearance::Light);
    ui.wait_for("the file to say light", |ui| {
        ui.file().contains("theme = \"light\"")
    });
    assert!(ui.file().starts_with("# My settings\n"), "{}", ui.file());

    // Files: auto-reveal, saved.
    ui.click("settings-nav-files");
    ui.wait_for("the files section", |ui| {
        ui.shows("setting-files.auto_reveal")
    });
    ui.click("setting-files.auto_reveal");
    assert!(ui.cx.read(|cx| super::get(cx).bool(AUTO_REVEAL, None)));
    ui.wait_for("auto_reveal in the file", |ui| {
        ui.file().contains("[files]\nauto_reveal = true")
    });
    // The palette toggle flips the same saved setting.
    ui.frame(|window, cx| window.dispatch_action(Box::new(crate::files::ToggleAutoReveal), cx));
    ui.wait_for("auto_reveal off in the file", |ui| {
        ui.file().contains("auto_reveal = false")
    });

    // An edit by hand applies live.
    let text = ui.file().replace("theme = \"light\"", "theme = \"dark\"");
    std::fs::write(&ui.config, text).unwrap();
    ui.wait_for("the dark theme again", |ui| ui.theme() == Appearance::Dark);

    // A broken file is reported on the page; the last good values stay.
    std::fs::write(&ui.config, "[appearance]\ntheme = \"light\n").unwrap();
    ui.click("settings-nav-appearance");
    ui.wait_for("the problem banner", |ui| ui.shows("settings-problems"));
    assert_eq!(ui.theme(), Appearance::Dark);
    assert_eq!(
        ui.cx.read(|cx| super::get(cx).value(THEME, None)),
        Some("dark".into())
    );

    // The keyboard shortcuts page is a placeholder for its editor.
    ui.click("settings-nav-keyboard");
    ui.wait_for("the keyboard page", |ui| ui.shows("settings-keyboard"));
}

#[test]
fn workspace_scope_writes_the_workspaces_file() {
    let mut ui = open_ui("[appearance]\ntheme = \"dark\"\n");
    ui.frame(|window, cx| window.dispatch_action(Box::new(OpenSettings), cx));
    ui.wait_for("the settings tab", |ui| ui.shows("settings-nav-files"));
    ui.click("settings-nav-files");
    ui.click("settings-scope-workspace");
    ui.wait_for("the workspace scope", |ui| {
        let page = ui.page().unwrap();
        ui.cx.read(|cx| page.read(cx).scope()) == page::Scope::Workspace
    });
    ui.click("setting-files.auto_reveal");
    let project = std::fs::canonicalize(&ui._fixture.project).unwrap();
    let ws_file = project.join(".coqo/settings.toml");
    ui.wait_for("the workspace file", |_| {
        std::fs::read_to_string(&ws_file).is_ok_and(|t| t.contains("auto_reveal = true"))
    });
    let id = ui
        .cx
        .read(|cx| Workspaces::global(cx).and_then(|w| w.read(cx).current().map(|w| w.id.clone())))
        .unwrap();
    ui.wait_for("the override", |ui| {
        ui.cx.read(|cx| super::get(cx).bool(AUTO_REVEAL, Some(&id)))
    });
    assert!(!ui.cx.read(|cx| super::get(cx).bool(AUTO_REVEAL, None)));
    assert!(!ui.file().contains("auto_reveal"));
}

#[test]
fn text_values_parse_by_kind() {
    use coqo_protocol::methods::settings::FieldKind;
    use serde_json::json;
    assert_eq!(page::parse_text(FieldKind::Integer, " 4 "), Ok(json!(4)));
    assert!(page::parse_text(FieldKind::Integer, "4.5").is_err());
    assert_eq!(page::parse_text(FieldKind::Number, "4.5"), Ok(json!(4.5)));
    assert_eq!(
        page::parse_text(FieldKind::StringList, "a, b,,c"),
        Ok(json!(["a", "b", "c"]))
    );
    assert_eq!(page::value_text(&json!(["a", "b"])), "a, b");
    assert_eq!(page::choice_label("on-save"), "On save");
}

/// Offscreen screenshots of the Settings tab, dark and light, into
/// `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn settings_screenshots() {
    use coqo_protocol::methods::settings::{
        FieldInfo, FieldKind, FieldScope, FileSettings, GetResult, SectionInfo, SettingsProblem,
    };
    use coqo_ui::test::render_offscreen;
    use serde_json::json;

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let field = |key: &str, title: &str, desc: &str, kind, default, values: &[&str]| FieldInfo {
        key: key.into(),
        title: title.into(),
        description: desc.into(),
        kind,
        default,
        values: values.iter().map(|v| (*v).to_owned()).collect(),
        minimum: None,
        maximum: None,
        scope: FieldScope::Workspace,
    };
    let section = |id: &str, title: &str, desc: &str, order, fields| SectionInfo {
        id: id.into(),
        title: title.into(),
        description: Some(desc.into()),
        order: Some(order),
        provider: "coqo.workbench".into(),
        fields,
    };
    let snapshot = GetResult {
        sections: vec![
            section(
                "appearance",
                "Appearance",
                "How Coqo looks.",
                10,
                vec![FieldInfo {
                    scope: FieldScope::User,
                    ..field(
                        THEME,
                        "Theme",
                        "System follows your operating system's light or dark mode.",
                        FieldKind::Enum,
                        json!("system"),
                        &["system", "dark", "light"],
                    )
                }],
            ),
            section(
                "files",
                "Files",
                "The file tree.",
                20,
                vec![field(
                    AUTO_REVEAL,
                    "Reveal the active file in the tree",
                    "Switching to a file tab expands its folders in the tree and selects it, without moving the focus.",
                    FieldKind::Boolean,
                    json!(false),
                    &[],
                )],
            ),
            section(
                "claude",
                "Claude Code",
                "Coqo starts your own, unmodified Claude Code and you sign in there.",
                30,
                vec![
                    field(
                        "claude.guide",
                        "Append Coqo's operating guide to Claude's system prompt",
                        "Add a short guide on when to use Coqo's editor tools to Claude Code's own system prompt.",
                        FieldKind::Boolean,
                        json!(true),
                        &[],
                    ),
                    field(
                        "claude.model",
                        "Default model",
                        "Used by new chats.",
                        FieldKind::String,
                        json!("sonnet"),
                        &[],
                    ),
                ],
            ),
        ],
        user: FileSettings {
            path: "/home/you/.config/coqo/settings.toml".into(),
            exists: true,
            values: [(THEME.to_owned(), json!("dark"))].into(),
            problems: vec![SettingsProblem {
                key: Some("files.auto_reveel".into()),
                line: Some(7),
                message: "`files.auto_reveel` is not a known setting".into(),
            }],
        },
        workspaces: vec![],
        generation: 1,
    };
    let shots = [
        ("dark", Appearance::Dark, "appearance"),
        ("light", Appearance::Light, "appearance"),
        ("claude-dark", Appearance::Dark, "claude"),
        ("claude-light", Appearance::Light, "claude"),
    ];
    for (name, appearance, section) in shots {
        let shot = snapshot.clone();
        let image = render_offscreen(appearance, size(1200., 720.), move |window, cx| {
            page::init(cx);
            cx.set_global(super::Settings {
                snapshot: Some(shot),
                ..super::Settings::default()
            });
            cx.new(|cx| {
                let mut page = SettingsPage::new_for_tests(window, cx);
                page.select(section, cx);
                page
            })
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-settings-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
