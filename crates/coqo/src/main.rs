// SPDX-License-Identifier: Apache-2.0

//! `coqo`: the Coqo desktop UI and CLI.
//!
//! `coqo` opens the workbench window and restores the last workspace;
//! `coqo <folder>` opens that folder. Either way the window attaches to
//! `coqod`, starting it if needed (PLAN §4.3). CLI subcommands arrive in
//! later issues (PLAN §12).

mod chat;
mod daemon;
mod files;
mod shell;
mod welcome;
mod workspace;

use std::{path::PathBuf, process::ExitCode, rc::Rc};

use coqo_ui::{
    Appearance,
    dock::store::JsonFileLayoutStore,
    gpui::{AppContext as _, Bounds, Size, TitlebarOptions, WindowBounds, WindowOptions, px, size},
};

const USAGE: &str = "usage: coqo [<folder>] | --version";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version" | "-V"] => {
            println!("coqo {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["--help" | "-h"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        [] => {
            run(None);
            ExitCode::SUCCESS
        }
        [path] if !path.starts_with('-') => match folder(path) {
            Ok(folder) => {
                run(Some(folder));
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("coqo: {err}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("coqo: unexpected arguments `{}`\n{USAGE}", args.join(" "));
            ExitCode::FAILURE
        }
    }
}

/// The folder to open: absolute, since the daemon has its own working
/// directory.
fn folder(path: &str) -> Result<PathBuf, String> {
    let canonical =
        std::fs::canonicalize(path).map_err(|e| format!("cannot open `{path}`: {e}"))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(format!("`{path}` is not a folder"))
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

fn run(folder: Option<PathBuf>) {
    coqo_ui::application().run(move |cx| {
        coqo_ui::init(Appearance::System, cx);
        match daemon::DaemonConfig::from_env() {
            Ok(config) => {
                daemon::init(config, cx);
            }
            Err(err) => eprintln!("coqo: {err}"),
        }
        workspace::init(folder, cx);
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
