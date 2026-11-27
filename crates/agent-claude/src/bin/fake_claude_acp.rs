// SPDX-License-Identifier: Apache-2.0

//! The scripted ACP agent, standing in for `claude-agent-acp` in tests.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = coqo_acp_host::testing::run_fake_agent().await {
        eprintln!("fake agent: {e}");
        std::process::exit(1);
    }
}
