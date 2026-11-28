// SPDX-License-Identifier: Apache-2.0

//! The update item, its popover and the launcher's decisions. The daemon's
//! side (checking, verifying, the swap) is tested in `coqo-updater` and
//! `coqod`; here the status is set as the daemon would answer.

use std::{cell::RefCell, fs, path::Path, rc::Rc};

use coqo_protocol::methods::update::{
    OffReason, ReleaseInfo, RestartPending, RolledBack, UpdateState, UpdateStatus,
};
use coqo_ui::{
    Appearance,
    dock::store::MemoryLayoutStore,
    gpui::{
        AnyWindowHandle, AppContext as _, Bounds, ElementId, Entity, Point, WindowBounds,
        WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use coqo_updater::Install;
use coqo_updater::records::{Pending, UpdateDir};

use super::view::{Tone, item_label};
use super::{AfterFailure, Busy, ConnectFailure, Updates, decide};
use crate::shell::{DEFAULT_WORKSPACE, Shell};
use crate::workspace;

fn status(state: UpdateState) -> UpdateStatus {
    UpdateStatus {
        current: "0.1.0".into(),
        state,
        restart: None,
        rolled_back: None,
    }
}

fn ready() -> UpdateStatus {
    status(UpdateState::Ready {
        release: ReleaseInfo {
            version: "0.2.0".into(),
            notes: "## What's new\n- Click to update\n- Chats resume after a daemon restart\n\nThanks for testing.".into(),
            url: Some("https://github.com/happsie/coqo/releases/tag/v0.2.0".into()),
            published_at: Some("2026-11-28T12:00:00Z".into()),
        },
    })
}

fn waiting(running: u32) -> UpdateStatus {
    UpdateStatus {
        restart: Some(RestartPending { running }),
        ..status(UpdateState::Installed {
            version: "0.2.0".into(),
        })
    }
}

#[test]
fn the_item_says_what_there_is_to_do() {
    let label = |s: &UpdateStatus, open| item_label(Some(s), None, false, open);
    assert_eq!(
        label(&ready(), false),
        Some((Tone::Info, "Update to v0.2.0".into()))
    );
    assert_eq!(
        label(&waiting(2), false),
        Some((
            Tone::Waiting,
            "Restarting the daemon when agents are idle (2 running)".into()
        ))
    );
    assert_eq!(
        label(&waiting(0), false),
        Some((Tone::Waiting, "Restarting the daemon…".into()))
    );
    let downloading = status(UpdateState::Downloading {
        version: "0.2.0".into(),
    });
    assert_eq!(label(&downloading, false).unwrap().1, "Downloading v0.2.0…");
    let refused = status(UpdateState::Failed {
        message: "the signature of manifest.json does not verify".into(),
        version: Some("0.2.0".into()),
        checked_at_ms: 1,
    });
    assert_eq!(
        label(&refused, false),
        Some((Tone::Failed, "Update to v0.2.0 failed".into()))
    );

    // Nothing to do: hidden, unless its popover is open.
    let current = status(UpdateState::UpToDate {
        checked_at_ms: Some(1),
    });
    assert_eq!(label(&current, false), None);
    assert_eq!(
        label(&current, true).unwrap().1,
        "Coqo v0.1.0 is up to date"
    );
    let dev = status(UpdateState::Off {
        reason: OffReason::DevBuild,
        message: "Running a development build".into(),
    });
    assert_eq!(label(&dev, false), None);
    assert_eq!(label(&dev, true).unwrap().1, "Running a development build");
    assert_eq!(
        item_label(Some(&current), Some(Busy::Checking), false, true)
            .unwrap()
            .1,
        "Checking for updates…"
    );
    assert_eq!(item_label(None, None, false, false), None);

    // A rollback shows until it was seen.
    let rolled = UpdateStatus {
        rolled_back: Some(RolledBack {
            version: "0.2.0".into(),
            restored: "0.1.0".into(),
            reason: "boom".into(),
        }),
        ..current.clone()
    };
    assert_eq!(
        item_label(Some(&rolled), None, false, false),
        Some((Tone::Failed, "v0.2.0 rolled back".into()))
    );
    assert_eq!(item_label(Some(&rolled), None, true, false), None);
}

fn write_exe(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A Linux install of 0.2.0 swapped in over 0.1.0, as the daemon leaves it.
fn swapped(home: &Path) -> (Install, UpdateDir) {
    let bin = home.join(".local/share/coqo/bin");
    write_exe(&bin.join("coqo"), "coqo 0.2.0");
    write_exe(&bin.join("coqod"), "coqod 0.2.0");
    let old = home.join(".local/share/coqo/bin.old");
    write_exe(&old.join("coqo"), "coqo 0.1.0");
    write_exe(&old.join("coqod"), "coqod 0.1.0");
    let install = Install::BinDir { bin: bin.clone() };
    let dir = UpdateDir::new(&home.join("data"));
    dir.set_pending(&Pending {
        from: "0.1.0".into(),
        to: "0.2.0".into(),
        install: bin,
    })
    .unwrap();
    (install, dir)
}

/// The launcher's rollback: the new version's daemon exited during
/// start-up, so the previous version goes back and is relaunched, once.
#[test]
fn a_new_daemon_that_exits_during_start_up_rolls_back_and_relaunches_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (install, dir) = swapped(tmp.path());
    let exited = ConnectFailure {
        daemon_exited: Some("coqod exited during startup (exit status: 1): bad config".into()),
        incompatible: false,
    };
    let relaunched = RefCell::new(Vec::new());
    let relaunch = |i: &Install| {
        relaunched.borrow_mut().push(i.ui_binary().unwrap());
        Ok(())
    };
    // Another version's failure is not ours.
    assert_eq!(
        decide(&dir, &install, "0.1.0", &exited, relaunch),
        AfterFailure::Nothing
    );
    assert_eq!(
        decide(&dir, &install, "0.2.0", &exited, relaunch),
        AfterFailure::RolledBack
    );
    let ui = install.ui_binary().unwrap();
    assert_eq!(fs::read_to_string(&ui).unwrap(), "coqo 0.1.0");
    assert_eq!(*relaunched.borrow(), vec![ui]);
    let record = dir.rolled_back().unwrap();
    assert_eq!(
        record.reason,
        "coqod exited during startup (exit status: 1): bad config"
    );
    // Once.
    assert_eq!(
        decide(&dir, &install, "0.2.0", &exited, relaunch),
        AfterFailure::Nothing
    );
    assert_eq!(relaunched.borrow().len(), 1);
}

/// A new UI that cannot talk to the old daemon (another protocol) waits
/// for it to restart instead of giving up.
#[test]
fn an_incompatible_old_daemon_is_waited_for() {
    let tmp = tempfile::tempdir().unwrap();
    let (install, dir) = swapped(tmp.path());
    let incompatible = ConnectFailure {
        daemon_exited: None,
        incompatible: true,
    };
    let never = |_: &Install| -> Result<(), String> { panic!("no relaunch") };
    assert_eq!(
        decide(&dir, &install, "0.2.0", &incompatible, never),
        AfterFailure::RetrySoon
    );
    dir.clear_pending();
    assert_eq!(
        decide(&dir, &install, "0.2.0", &incompatible, never),
        AfterFailure::Nothing
    );
}

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    updates: Entity<Updates>,
}

fn name(id: &str) -> ElementId {
    ElementId::Name(id.to_owned().into())
}

impl Ui {
    fn open(install: Install, status: UpdateStatus) -> Self {
        let cx = TestAppContext::single();
        let window = cx.update(|cx| {
            coqo_ui::init(Appearance::Dark, cx);
            workspace::init(None, cx);
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
            .unwrap()
            .0
        });
        let updates = cx.read(|cx| Updates::global(cx).unwrap());
        cx.update(|cx| {
            updates.update(cx, |u, cx| u.set_for_tests(install, Some(status), cx));
        });
        let mut ui = Self {
            cx,
            window,
            updates,
        };
        ui.frame();
        ui
    }

    fn frame(&mut self) {
        self.cx.run_until_parked();
        self.cx
            .update_window(self.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn shows(&mut self, id: &str) -> bool {
        self.frame();
        self.cx
            .update_window(self.window, |_, window, _| {
                window.try_find(name(id)).is_some()
            })
            .unwrap()
    }

    fn click(&mut self, id: &str) {
        self.frame();
        self.cx
            .update_window(self.window, |_, window, cx| window.click(name(id), cx))
            .unwrap();
        self.frame();
    }
}

#[test]
fn clicking_the_item_shows_the_notes_and_update_and_restart() {
    let mut ui = Ui::open(
        Install::App {
            app: "/Users/you/Applications/Coqo.app".into(),
        },
        ready(),
    );
    assert!(ui.shows("status-update"));
    assert!(!ui.shows("update-popover"));
    ui.click("status-update");
    assert!(ui.shows("update-popover"));
    assert!(ui.shows("update-notes"));
    assert!(ui.shows("update-install"));
    ui.click("update-later");
    assert!(!ui.shows("update-popover"));
    assert!(ui.shows("status-update"), "still there for later");
}

#[test]
fn a_development_build_offers_no_install() {
    let mut ui = Ui::open(
        Install::Dev {
            exe: "/home/you/code/coqo/target/debug/coqo".into(),
        },
        ready(),
    );
    ui.click("status-update");
    ui.click("update-install");
    assert!(ui.shows("update-popover"));
    let busy = ui.cx.read(|cx| ui.updates.read(cx).busy());
    assert_eq!(busy, None, "nothing was installed");
    // The palette command says why.
    ui.cx
        .update(|cx| ui.updates.update(cx, |u, cx| u.install(cx)));
    let error = ui
        .cx
        .read(|cx| ui.updates.read(cx).error().map(str::to_owned));
    assert_eq!(
        error.as_deref(),
        Some("Running a development build: nothing to install into")
    );
}

#[test]
fn restart_now_warns_while_turns_run() {
    let mut ui = Ui::open(
        Install::App {
            app: "/Users/you/Applications/Coqo.app".into(),
        },
        waiting(2),
    );
    assert!(ui.shows("status-update"));
    ui.click("status-update");
    assert!(!ui.shows("update-restart-warning"));
    ui.click("update-restart-now");
    assert!(ui.shows("update-restart-warning"), "asks first");
    ui.click("update-restart-cancel");
    assert!(!ui.shows("update-restart-warning"));
}

/// Renders the update item and popover offscreen to PNGs for review against
/// board 1. Needs a GPU: `cargo test -p coqo -- --ignored update_screenshots`.
/// Writes to `$COQO_SHOT_DIR` (default: the system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn update_screenshots() {
    use coqo_ui::test::render_offscreen;

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let app = Install::App {
        app: "/Users/you/Applications/Coqo.app".into(),
    };
    for (kind, status) in [("ready", ready()), ("waiting", waiting(2))] {
        for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
            let status = status.clone();
            let app = app.clone();
            let image = render_offscreen(appearance, size(1440., 900.), move |window, cx| {
                workspace::init(None, cx);
                crate::shell::init(cx);
                let updates = Updates::global(cx).unwrap();
                let handle = window.window_handle();
                updates.update(cx, |u, cx| {
                    u.set_for_tests(app, Some(status), cx);
                    u.toggle(handle, cx);
                });
                let store = Rc::new(MemoryLayoutStore::default());
                cx.new(|cx| Shell::new(store, DEFAULT_WORKSPACE, window, cx))
            })
            .expect("offscreen render");
            let path = dir.join(format!("coqo-update-{kind}-{name}.png"));
            image.save(&path).expect("save png");
            println!("wrote {}", path.display());
        }
    }
}
