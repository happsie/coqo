// SPDX-License-Identifier: Apache-2.0

//! `coqo`: the Coqo desktop UI and CLI.
//!
//! With no arguments it opens the workbench window. The daemon connection, workspaces
//! and CLI subcommands arrive in later M1 issues (see `docs/PLAN.md` §4 and §12).

mod shell;

use std::process::ExitCode;

use coqo_ui::{
    Appearance,
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

fn run() {
    coqo_ui::application().run(|cx| {
        coqo_ui::init(Appearance::System, cx);
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
        if let Err(err) = coqo_ui::open_window(options, cx, |_, cx| cx.new(|_| shell::Shell)) {
            eprintln!("coqo: failed to open window: {err:#}");
            cx.quit();
        }
        cx.activate(true);
    });
}
