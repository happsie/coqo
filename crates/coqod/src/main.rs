// SPDX-License-Identifier: Apache-2.0

//! `coqod`: the Coqo background daemon. Placeholder until M1 (see `docs/PLAN.md` §4.3).

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("coqod {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("coqod: the daemon is not implemented yet (see docs/PLAN.md)");
            ExitCode::FAILURE
        }
    }
}
