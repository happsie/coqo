// SPDX-License-Identifier: Apache-2.0

//! A scripted ACP agent for tests; see `coqo_acp_host::testing`.

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), agent_client_protocol::Error> {
    coqo_acp_host::testing::run_fake_agent().await
}
