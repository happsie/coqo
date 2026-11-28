# Spike S3: Claude via ACP from a Rust daemon (issue #10)

Throwaway prototype behind [ADR 0005](../../adr/0005-claude-integration.md). It isn't part of the Coqo workspace and CI doesn't build it.

One binary, three subcommands:

- `acp`: an ACP client on `agent-client-protocol = "=5.6.1"`. It spawns the Claude Agent ACP adapter over stdio, runs `initialize`, then `session/new`, `session/load` or `session/resume` with a stub MCP server injected through `mcpServers`, and optionally one `session/prompt`. It streams and summarises the updates, answers permission requests with *allow once*, serves `fs/*`, and prints timings.
- `mcp-echo`: a tiny stdio MCP server with one `echo` tool. It stands in for `coqod mcp`, and it logs what it receives to `ECHO_LOG`.
- `pty`: terminal mode. It runs the official `claude` in a PTY with `--mcp-config` pointing at `mcp-echo`. It accepts the folder-trust dialog, optionally types one prompt, and reads the stub's log to prove that the server was connected and its tool called.

Nothing here reads anything under `~/.claude` or handles credentials. The adapter and the CLI own authentication.

## Run

```sh
# The adapter isn't vendored: install it anywhere outside the repo (needs Node >= 22).
npm install --prefix /tmp/acp @agentclientprotocol/claude-agent-acp@0.113.0
A=/tmp/acp/node_modules/.bin/claude-agent-acp
W=$(mktemp -d)

cargo build --release
B=target/release/spike-s3-acp

# Startup only, no prompt (no usage). --installed-claude points the adapter at your own `claude`.
$B acp --adapter $A --cwd $W --installed-claude

# One prompt in "Manual" mode, so permission requests appear.
$B acp --adapter $A --cwd $W --installed-claude --set-mode default \
  --prompt "Call the echo tool from the coqo-echo MCP server with the text 'hi'. Then reply with just the word pong."

# A new adapter process: replay the history, or resume it without a replay.
$B acp --adapter $A --cwd $W --installed-claude --mode load   --session <id>
$B acp --adapter $A --cwd $W --installed-claude --mode resume --session <id> --prompt "..."

# Terminal mode. Use a clean env when you run it from inside a Claude Code session.
env -i HOME=$HOME USER=$USER TERM=xterm-256color PATH=$HOME/.local/bin:/usr/bin:/bin \
  $B pty --cwd $W --session-id $(cat /proc/sys/kernel/random/uuid) --prompt "Call the echo tool from the coqo-echo MCP server with the text hi, then stop."
env -i ... $B pty --cwd $W --resume <id>
```
