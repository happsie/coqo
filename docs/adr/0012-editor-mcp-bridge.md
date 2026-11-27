# 12. Editor MCP bridge: `coqod mcp`, one-time session tokens and the tool registry

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #69 (refines the token delivery in ADR 0007; builds on ADR 0005 §3)

## Context

PLAN §7.4 gives every agent a `coqo-editor` MCP server: the agent launches `coqod mcp`, a stdio MCP server that talks to the daemon over its socket, so editor tools work whether or not a UI is open. The daemon's route policy (ADR 0007, *Route policy*) already limits a **session-scoped** token to one workspace and agent session. Three things were open:

1. **How the bridge gets its token.** ADR 0007 said "in the environment (`COQO_SESSION_TOKEN`), never in `argv`". But ADR 0005 found that the Claude ACP adapter, when it runs the user's own `claude` (`CLAUDE_CODE_EXECUTABLE`), passes the MCP servers to Claude Code **on its command line** as `--mcp-config <json>`, `env` included. Any local user can read a command line through `/proc` or `ps`. So an MCP server's environment is not private either.
2. **Which MCP implementation**, and where the tools live.
3. **Lifecycle:** when the bridge exits, and what happens to its token when the agent session ends.

## Decision

**Same binary.** The bridge is `coqod mcp --workspace <id> --session <id>`, not a separate binary. The daemon launches its own executable (`current_exe`), so the bridge always matches the daemon's build and protocol, and there is nothing extra to install or find. The MCP code lives in `crates/mcp` (`coqo-mcp`), which depends on `coqo-protocol` and `coqo-client` but not on the daemon; `coqod` only adds the subcommand. The workspace and session ids on `argv` are routing information, not secrets.

**MCP SDK.** `rmcp` 3.12 (the official Rust SDK, Apache-2.0), with only the `server` feature: no HTTP, OAuth or chrono. It negotiates the client's protocol version (2024-11-05 to 2026-07-28), so current and older Claude Code builds and the MCP inspector work. `rmcp` types stay inside `coqo-mcp`.

**One-time tokens.** When a session's provider starts or resumes, the agents service calls `EditorMcp::servers`, which issues a **one-time token** (`Auth::issue_ticket(workspace, session, ttl)`) and describes the server:

| | Value |
|---|---|
| `command` | the running `coqod` |
| `args` | `mcp --workspace <id> --session <id>` |
| `env` | `COQO_SESSION_TOKEN=<one-time token>`, `COQO_RUNTIME_DIR=<this daemon's runtime directory>` |

The token is 256 random bits, like every other token. It grants `Scope::Session { workspace, session }` to the **first** `coqo/hello` that presents it, within `TICKET_TTL` (60 s; ADR 0005 measured MCP servers connecting 1–2 s after `session/new`), and is then gone. The connection keeps the scope until it closes. No protocol change was needed: the bridge sends the token as the hello's `token`, like any other client.

So what ends up on Claude Code's command line is spent within a second or two of being written, and was only ever usable by a same-uid process, because the socket's directory and peer-credential checks (ADR 0007) turn everyone else away before the handshake. Nothing reusable is ever on a command line or in a file. `Auth::mint_session` (reusable until revoked) stays for clients that keep their token private.

*Alternatives.* A reusable session token in `env` is what ADR 0007 planned, and it lands on `argv` (ADR 0005). A nonce on `argv` redeemed over the socket for a reusable token would let the bridge reconnect, but reconnecting buys nothing (see *Lifecycle*), and every reusable credential is one more secret to protect. MCP-over-ACP (`"type":"acp"`) puts nothing on `argv` and needs no process, but terminal mode (#57) and other agents need the stdio server anyway; it stays a follow-up in ADR 0005.

**Revocation.** `Auth::revoke_session` (called when an agent session closes: `revoke_on_close` follows `sessions/changes`) drops the session's tokens and unused one-time tokens **and closes connections attached with its scope**. The last part is new in `coqo-protocol`: `Authenticator::revoked(scope)` returns a future, and the server closes the connection when it resolves (default: never). For `Scope::Daemon` it never resolves.

**Tool registry.** Tools run in the bridge and reach editor state only through daemon methods on the session-scoped connection, so the route policy covers everything they do:

```rust
pub trait Tool: Send + Sync + 'static {
    fn definition(&self) -> ToolDefinition; // name, title, description, input JSON Schema, read_only
    fn call<'a>(&'a self, cx: &'a ToolContext, args: Map<String, Value>)
        -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}
// ToolContext: daemon() (the scoped Client), workspace(), session(), progress(…)
```

`ToolRegistry` is shared and may change while the server runs; each register or remove sends `notifications/tools/list_changed`. `ToolContext::progress` sends `notifications/progress` when the agent asked for it, for blocking tools such as `ask_question`. A cancelled call is dropped. Tool failures (`ToolError`, including a daemon `FORBIDDEN`) are MCP results with `isError`, so the model can read and correct them; an unknown tool is a JSON-RPC `-32602`. Names are 1–64 of `A-Z a-z 0-9 _ -`. The first two tools prove the path: `workspace_info` (`workspace/get`) and `read_file` (`fs/read`; relative to a root, or absolute inside one).

Tools that mods contribute (#70) run in the daemon, where the mods are. The bridge will list and call them through daemon routes opened with `Access::session`, and register them in the same registry. That is #70's decision ([ADR 0014](0014-editor-tools-and-mod-tools.md)); the registry already handles tools that come and go.

**Lifecycle.** The bridge exits with status 0 when stdin closes, and with status 1 when the daemon connection closes (the daemon stopped, or the session was closed and its scope revoked). It does not reconnect. A restarted daemon would not know the token (tokens are per daemon start, and this one is spent), and the agent process that launched the bridge is a child of the daemon's ACP host, so it goes away with the daemon. When the next daemon resumes the session, the provider starts a fresh bridge with a fresh token. The bridge never starts a daemon either. It logs to stderr (stdout is MCP), at `warn` unless `COQO_LOG` says otherwise, and never logs the token.

**Injection.** `ManagerOptions::mcp_servers` takes `EditorMcp::source()`. The session manager calls it for each start and resume, and again for a start after a failed resume, so each process gets its own token. Terminal mode (#57) and the native loop (#55) inject the same spec when they land.

*As built (#71).* Every session with a workspace gets the server; `COQO_EDITOR_MCP=0` (or `false`, `off`, `no`) leaves it out, for debugging an agent without it. Sessions without a workspace get none, since a token is scoped to one.

- **Binary.** `command` is `current_exe`, canonicalized (it can be a symlink on macOS), so it is absolute wherever the daemon runs from, `target/release` included. If the binary was replaced while the daemon runs (a rebuild), Linux reports `<path> (deleted)`; the daemon then names `<path>`, the new binary, and logs a warning.
- **The user's own MCP servers stay.** The Claude adapter (`@agentclientprotocol/claude-agent-acp` 0.113) loads Claude Code's `settingSources` `user`, `project` and `local` and adds the ACP `mcpServers` to them; it sets `strictMcpConfig` only for its internal options probe. In the manual run below, Claude had `coqo-editor` alongside the user's own `claude.ai Claude Docs` server. The adapter reports client servers that **fail** to start (a notice with `_meta.claudeCode.kind: "mcpServerStatus"`), but not ones that connect, so there is no "connected" signal to show yet.
- **Ticket TTL, measured.** `coqod::auth` logs each redeemed ticket's age at `debug` (`one-time token redeemed age=…`): issue (just before the provider starts the agent) to the bridge's `coqo/hello`. With the real adapter and Claude Code 2.1.332, release `coqod`, a folder Claude Code had never seen: **519 ms** for a new session (adapter spawn included) and **893 ms** for a resume after a daemon restart. No trust dialog runs in SDK mode. 60 s leaves ample room, so `TICKET_TTL` stays.

## Consequences

- Every agent session gets the editor tools by default; nothing to switch on.
- Agents get editor tools through the route policy, scoped to their own workspace and session, with no lasting credential on any command line or in any file.
- If an agent restarts its MCP server within a session (Claude Code's `/mcp` reconnect, say), the new bridge's token is already spent and it fails to attach. The session's next start or resume gets a working one. A refresh path is a follow-up (#199).
- A same-uid process that reads the token off the command line first can take the bridge's place for one connection. The real bridge then fails visibly. Same-uid processes can read `coqod.token` anyway (ADR 0007), so this adds no new access.
- Session closes now cut live connections. Daemon-token connections are never cut.
- `coqod` grows a subcommand and `rmcp` joins the dependency tree of `coqod` (and only `coqod`).
- `buffer/close` still lets a session close any replica in its workspace (from #153). The bridge does not use buffers yet; tying replicas to the connection that opened them is a follow-up (#198).
