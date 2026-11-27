// SPDX-License-Identifier: Apache-2.0

//! `coqo`: the Coqo desktop UI and CLI.
//!
//! `coqo` opens the workbench window and restores the last workspace;
//! `coqo <folder>` opens that folder. Either way the window attaches to
//! `coqod`, starting it if needed (PLAN §4.3). More windows share the
//! connection (`windows`). CLI subcommands arrive in later issues (PLAN §12).

mod activity;
mod agents;
mod alerts;
mod chat;
mod daemon;
mod diff;
mod files;
mod git;
mod layout;
mod questions;
mod resume;
mod shell;
mod terminal;
mod walkthrough;
mod welcome;
mod windows;
mod workspace;

#[cfg(test)]
mod attach_tests;
#[cfg(test)]
mod test_support;

use std::{path::PathBuf, process::ExitCode, rc::Rc};

use coqo_ui::{Appearance, dock::store::JsonFileLayoutStore, gpui::QuitMode};

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

/// The UI's own files: `$COQO_DATA_DIR/ui` if set, otherwise under the
/// per-user data directory (PLAN §8): `$XDG_DATA_HOME/coqo/ui` on Linux,
/// `~/Library/Application Support/Coqo/ui` on macOS.
fn ui_dir() -> PathBuf {
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
    base.join("ui")
}

/// Where layouts are kept while `coqod` is unreachable (and where they lived
/// before the daemon kept them).
fn layout_dir() -> PathBuf {
    ui_dir().join("layouts")
}

fn run(folder: Option<PathBuf>) {
    let app = coqo_ui::application().with_quit_mode(QuitMode::LastWindowClosed);
    app.run(move |cx| {
        coqo_ui::init(Appearance::System, cx);
        let daemon = match daemon::DaemonConfig::from_env() {
            Ok(config) => Some(daemon::init(config, cx)),
            Err(err) => {
                eprintln!("coqo: {err}");
                None
            }
        };
        // Before the first connection is up: it decides on "welcome back".
        let last_seen = agents::away::FileLastSeen::new(ui_dir().join("presence.json"));
        agents::away::init(Rc::new(last_seen), cx);
        workspace::init(folder, cx);
        shell::init(cx);
        if let Some(daemon) = daemon {
            let handle = daemon.read(cx).handle();
            terminal::init(Rc::new(terminal::DaemonBackend::new(handle)), cx);
        }
        let fallback = Rc::new(JsonFileLayoutStore::new(layout_dir()));
        let store = layout::DaemonLayoutStore::install(fallback, shell::DEFAULT_WORKSPACE, cx);
        windows::init(store, cx);
        if let Err(err) = windows::open(cx) {
            eprintln!("coqo: failed to open window: {err:#}");
            cx.quit();
        }
        cx.activate(true);
    });
}
