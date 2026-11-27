// SPDX-License-Identifier: Apache-2.0

//! The scripted ACP agent from `coqo_acp_host::testing`, standing in for a
//! real agent in this crate's integration tests.

fn main() -> std::process::ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    match runtime.block_on(coqo_acp_host::testing::run_fake_agent()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("coqod-fake-acp-agent: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
