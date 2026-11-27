// SPDX-License-Identifier: Apache-2.0

//! `coqo`: the Coqo desktop UI and CLI.
//!
//! With no arguments it opens the workbench window. The daemon connection, workspaces
//! and CLI subcommands arrive in later M1 issues (see `docs/PLAN.md` §4 and §12).

mod shell;
mod welcome;

use std::{path::PathBuf, process::ExitCode, rc::Rc};

use coqo_ui::{
    Appearance,
    dock::store::JsonFileLayoutStore,
    gpui::{AppContext as _, Bounds, Size, TitlebarOptions, WindowBounds, WindowOptions, px, size},
};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("coqo {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        None => {
            run();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("coqo: unknown argument `{other}` (try --version)");
            ExitCode::FAILURE
        }
    }
}

/// Where saved workbench layouts live: `$COQO_DATA_DIR/ui/layouts` if set,
/// otherwise under the per-user data directory (PLAN §8):
/// `$XDG_DATA_HOME/coqo` on Linux, `~/Library/Application Support/Coqo` on macOS.
fn layout_dir() -> PathBuf {
    let base = std::env::var_os("COQO_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let name = if cfg!(target_os = "macos") {
                "Coqo"
            } else {
                "coqo"
            };
            dirs::data_dir().map(|d| d.join(name))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("coqo"));
    base.join("ui").join("layouts")
}

fn run() {
    coqo_ui::application().run(|cx| {
        coqo_ui::init(Appearance::System, cx);
        shell::init(cx);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Coqo".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1440.), px(900.)),
                cx,
            ))),
            window_min_size: Some(Size {
                width: px(900.),
                height: px(560.),
            }),
            app_id: Some("dev.coqo.Coqo".into()),
            ..Default::default()
        };
        let store = Rc::new(JsonFileLayoutStore::new(layout_dir()));
        let opened = coqo_ui::open_window(options, cx, |window, cx| {
            cx.new(|cx| shell::Shell::new(store, shell::DEFAULT_WORKSPACE, window, cx))
        });
        if let Err(err) = opened {
            eprintln!("coqo: failed to open window: {err:#}");
            cx.quit();
        }
        cx.activate(true);
    });
}
