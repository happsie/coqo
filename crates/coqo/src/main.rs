// SPDX-License-Identifier: Apache-2.0

//! `coqo`: the Coqo desktop UI and CLI. Placeholder until M1 (see `docs/PLAN.md` §4).

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("coqo {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("coqo: the UI is not implemented yet (see docs/PLAN.md)");
            ExitCode::FAILURE
        }
    }
}
