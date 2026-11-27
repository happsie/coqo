# 5. Claude integration: the Claude Agent ACP adapter on the user's own `claude`, with terminal mode beside it

- **Status:** Accepted
- **Date:** 2026-11-27
- **Spike:** S3, issue #10. Prototype in [`spikes/s3-acp/`](../../spikes/s3-acp/).

## Context

PLAN §7.3 makes Claude the first-class agent and offers two ways to run it on the user's own login: an **ACP agent**, where the daemon drives the Claude Agent ACP adapter, and a **terminal agent**, where the official `claude` TUI runs in a daemon PTY. Both must stay inside Anthropic's policy and Coqo's hard rules: no claude.ai login or OAuth flow in Coqo, no reading of `~/.claude` credential files, no modifying or wrapping of the `claude` binary, and no proxying of subscription traffic.

Spike S3 asks whether a Rust daemon can do this end to end, and what it costs:

- the ACP lifecycle;
- MCP server injection;
- permissions, plans, tool-call diffs and `available_commands`;
- resume after the client is killed;
- terminal mode with `--mcp-config`;
- detecting the billing mode without touching credentials.

The planned fallback (PLAN §11) is terminal mode only for subscription users.

## What we tested

`spikes/s3-acp` is a standalone Rust binary (not in the workspace) with three subcommands:

- **`acp`:** a client on `agent-client-protocol` 5.6.1 that spawns the adapter over stdio. It runs `initialize`, then `session/new`, `session/load` or `session/resume`, each with a stub stdio MCP server injected in `mcpServers`. It can also run `session/set_mode` and one `session/prompt`. It answers permission requests with *allow once*, serves `fs/*`, and logs every update with a timestamp.
- **`mcp-echo`:** that stub MCP server. It has one `echo` tool and logs what it receives.
- **`pty`:** the official `claude` in a PTY (`portable-pty`), with `--mcp-config` pointing at the stub.

**Versions:**
- Adapter: `@agentclientprotocol/claude-agent-acp` **0.113.0**, which depends on `@anthropic-ai/claude-agent-sdk` 0.3.331 and `@agentclientprotocol/sdk` 2.12.0.
- Claude Code: the user's installed **2.1.332**.
- Node 24.18.
- Machine: Linux (Fedora 44).

**Usage budget:** four tiny prompts in total against a signed-in Claude subscription (plan login). Two went through ACP: a tool call plus a file write, then one resumed follow-up. Two went through terminal mode, each a single echo call. Every other run was `initialize` plus `session/new` or `session/load` with no prompt.

## Findings

### 1. What the adapter is and how it launches Claude

- **Package:** the adapter was renamed twice. `@zed-industries/claude-code-acp` became `@zed-industries/claude-agent-acp`, which became **`@agentclientprotocol/claude-agent-acp`** (repo `agentclientprotocol/claude-agent-acp`, Apache-2.0). Both old names are deprecated on npm.
- **Releases:** they ship almost daily (0.113.0 on 2026-11-26).
- **Runtime:** it needs **Node ≥ 22**. The binary is `claude-agent-acp`. Standalone Bun-compiled executables also exist on its GitHub releases, but they ship their own copy of `claude`.
- **How Claude runs:** the adapter is built on the **Claude Agent SDK**, and the SDK spawns a **native `claude` binary** in SDK mode (stream-JSON over stdio). The adapter picks the binary as follows (`dist/claude-cli.js`):
  1. `CLAUDE_CODE_EXECUTABLE`, if set;
  2. otherwise the `claude` that ships next to a standalone executable;
  3. otherwise the SDK's platform package `@anthropic-ai/claude-agent-sdk-linux-x64/claude`. That copy is Claude Code 2.1.331 and takes 381 MB of a 455 MB install.
- **With the user's own binary:** with `CLAUDE_CODE_EXECUTABLE=<the user's claude>`, we observed the adapter start the user's binary unmodified, as `claude --output-format stream-json --input-format stream-json --permission-prompt-tool=stdio --mcp-config {…} --setting-sources=user,project,local --permission-mode=<user default> --session-id=<id> …`. These are the SDK's own documented flags. Nothing is patched or replaced.
- **Slim install:** `npm install --omit=optional` leaves out the bundled CLI (75 MB install). It works with `CLAUDE_CODE_EXECUTABLE` set, and fails cleanly without it.
- **Licence of the parts:**
  - The adapter is **Apache-2.0**.
  - **`@anthropic-ai/claude-agent-sdk` is proprietary** ("© Anthropic PBC. All rights reserved"; use is subject to Anthropic's legal agreements).
  - Coqo therefore must **not vendor or redistribute** the adapter's `node_modules` (SDK plus bundled CLI). It installs the adapter on the user's machine from npm, as Zed does.
- **Its own reads:** the adapter reads `~/.claude/settings.json` and `~/.claude/projects/` (transcripts, for `session/list` and load). It does not read credential files: login, logout and status all go through the CLI (`claude auth status --json`, `claude auth logout`). Coqo itself reads nothing under `~/.claude`.

### 2. Rust ACP crate

- **Crate:** **`agent-client-protocol` 5.6.1**, Apache-2.0, repo `agentclientprotocol/rust-sdk`, 178 reverse dependencies. Its schema crate is `agent-client-protocol-schema` 1.22.0.
- **Runtime:** it doesn't depend on a particular async runtime (`futures` plus `async-process` behind the `process` feature). It worked under tokio without glue.
- **What it gives us:**
  - `AcpAgent::new(AcpAgentConfig::new(cmd).args(..).env(..))` spawns the agent in its own process group and kills that group on drop.
  - `Client.v1().on_receive_notification(..).on_receive_request(..).connect_with(agent, …)` gives typed handlers, and `UntypedMessage` catches extension notifications.
- **Protocol versions:** ACP v1 is stable. v2 is in preview behind `unstable_protocol_v2`, and the adapter serves both.
- **Licences:** cargo-deny over the spike's full tree (with `portable-pty` and `tokio`) passes Coqo's `deny.toml` with no new exceptions.
- **Churn is high:** majors 1 → 5 shipped between June and November 2026, and 5.1 → 5.6.1 shipped in November alone. PLAN §11 already plans to pin the crate exactly behind an `acp-host` adapter layer, and the spike confirms that's necessary.

### 3. MCP injection

| | ACP mode | Terminal mode |
|---|---|---|
| How | `session/new`, `session/load` and `session/resume` take `mcpServers: [{name, command (absolute), args, env}]`. HTTP, SSE and *MCP-over-ACP* (`"type":"acp"`, served by the client on the ACP connection, `unstable_mcp_over_acp` in the Rust crate) are also advertised. | `claude --mcp-config <file>` (`--strict-mcp-config` would hide the user's own servers, which we don't want) |
| Verified | ✅ The stub got `initialize` → `tools/list` → `tools/call {text:"hi"}`. The tool shows as `coqo-echo: echo` (`mcp__coqo-echo__echo`). Claude reached it through its deferred `ToolSearch` first, which costs one extra step per new tool. | ✅ The stub was connected 1.1–1.8 s after spawn, and the typed prompt called `echo` |
| Delivery to the CLI | With `CLAUDE_CODE_EXECUTABLE`, the adapter passes the servers **on the CLI's argv** as `--mcp-config <json>`, so their `env` is readable by other local users through `/proc`. (Its bundled CLI receives them over stdin instead.) | Our generated file |
| Re-injection | `session/load` and `session/resume` re-inject the servers in the new process (verified) | Same `--mcp-config` on `--resume` (verified) |

The user's own MCP servers, skills and plugins load as usual in both modes. We saw this because `available_commands` listed the user's skills.

### 4. Turn content: permissions, tool calls, diffs, plans, commands

- **Permissions:** the adapter sends `session/request_permission` for MCP tools and for `Write` in *Manual* mode (`default`). The options are `allow-once` (AllowOnce), `allow-with-updates` (AllowAlways, which persists a rule) and `reject` (RejectOnce). Answering *allow once* resumed the turn at once.
- **Modes:** `session/new` returns the modes: `default` ("Manual"), `acceptEdits`, `plan`, `auto` and others. The current mode comes from the user's Claude settings, and here it was `auto`. So **whether a permission prompt appears depends on the mode**, and Coqo must show the mode and let the user change it (`session/set_mode` works).
- **Tool calls:** a tool call arrives as `tool_call` (pending), then `tool_call_update`s, ending in `completed`. Each has a kind (`edit`, `other`, …), a title ("Write pong.txt"), and `rawInput` on replay.
- **File edits:** an edit carries a **`diff` content block** (path, old text, new text) *before* the permission request, which is what inline accept/reject needs.
- **`available_commands_update`:** arrives right after `session/new` (72 commands, which the CLI then trimmed to 67): built-in slash commands plus the user's skills.
- **Usage:** `usage_update` reports context use, `_claude/model` and `_claude/rateLimit` (`five_hour`, `resetsAt`, `overageStatus`). At the end of a turn it reports a **`cost` in USD even on a subscription**. That cost is SDK-computed and notional, so the UI must not present it as a charge on a plan.
- **Plans:** we saw no `plan` update. The prompts were too small to make Claude write a todo list. The client has to advertise `plan` capabilities, and our client did. Verify with a real task in #54.

### 5. `fs/*` is not how Claude edits files

We advertised `fs.readTextFile` and `fs.writeTextFile`, but **the adapter never called them**. Claude Code's own `Write` tool wrote `pong.txt` straight to disk. In 0.113.0 the client `fs` methods are only pass-throughs for other ACP traffic; earlier adapter versions routed edits through an ACP MCP server. **PLAN §7.1 ("ACP agents write files through `fs/write_text_file`") is therefore wrong for Claude.** Live edits must come from:

1. the `tool_call` diff, which arrives before the write and is enough for review UI;
2. a daemon file watcher on the workspace, which reconciles open buffers when the file changes on disk.

Agents that do use `fs/*` (other ACP agents) keep the buffer path.

### 6. Resume after the client is killed

- **`session/load` (v1, `loadSession: true`):** in a fresh adapter process it **replays the whole history** as `session/update`s (user chunk, tool calls with their diffs, agent text) before it responds. That took about 30 ms after `initialize` for this short session, and spawn to loaded was 986 ms. It needs the same `cwd`.
- **`session/resume` (`sessionCapabilities.resume`):** reattaches **without replaying** anything, with MCP re-injected. A follow-up prompt ("what word did you write?") answered correctly, so the context was kept.
- **Also advertised:** `session/list`, `close`, `delete`, `fork` and `additionalDirectories`.
- **Fit for Coqo:** Coqo keeps its own transcript in the event log (#17) and reattaches with `session/resume`. It uses `session/load` only when it has lost its transcript. In that case it rebuilds the transcript from the replay; it never reads `~/.claude/projects` itself.
- **Persistence:** sessions live in Claude Code's own store, so they survive a daemon restart. We didn't need anything Coqo-specific.
- **Shutdown noise:** closing the connection right after `session/load` or `session/new` sometimes made the adapter exit with status 1. That's harmless, but the supervisor should treat an exit after close as normal.
- **Terminal mode:** `claude --session-id <uuid>` at launch, then `claude --resume <uuid>` after a kill, restored the conversation and the MCP server. Coqo generates the UUID, so it knows the id without scraping the TUI.

### 7. Auth and billing detection without touching credentials

- **No auth methods:** with no `auth.terminal` client capability, the adapter advertises **no auth methods** (`authMethods: []`). Coqo therefore never sees a login flow. A signed-out CLI makes `session/new` fail with `auth_required`.
- **Identity reports:** the adapter pushes the identity the CLI itself reports, read with `claude auth status --json` in the unmodified binary, about 330 ms after spawn. It does this twice:
  - the RFD notification `auth/status_update`: `{state: "authenticated", accounts: [{label: "Claude <plan>", _meta.claudeCode: {kind: "account", plan: …}}]}` (`unstable_auth_status` in the Rust crate);
  - the legacy extension `_auth/status_update`: `{authStatus: {kind, label, account: {plan}}}`.
- **Kinds:** `kind` is one of `account` (a claude.ai plan), `api_key` (with a detail such as `ANTHROPIC_API_KEY` or `apiKeyHelper`), `gateway`, `external` (Bedrock, Vertex, Foundry) or `none`.
- **What Coqo uses:** this is all the billing indicator needs. Coqo shows only kind and plan, and drops the email and organization fields the payload also carries.
- **Terminal mode:** the TUI shows the plan in its own header. Coqo doesn't need it there; at most it runs `claude auth status --json` itself with the same redaction.
- **`--hide-claude-auth`:** the adapter has this flag for integrations that "must never bill a claude.ai subscription, except a Claude Enterprise plan". Under it, it refuses Free, Pro, Max and Team turns with `auth_required` (`reason: claude_subscription_not_supported`). That's a ready-made **policy kill switch** for Coqo (see Decision).

**"Agent SDK credit" status (checked 2026-11-27):**
- Anthropic's help article *Use the Claude Agent SDK with your Claude plan* says the change announced in May 2026 was **paused on 2026-06-15**: "Claude Agent SDK, `claude -p`, and third-party app usage still draw from your subscription limits."
- An **update of 2026-10-07** adds monthly API credits to Max, Ultra and Team plans. They are used by the API, Managed Agents, and the Agent SDK or `claude -p` *when run with an API key from the linked Console org*; signed-in plan usage is unchanged.
- So today, ACP on a plan login draws from the plan's normal limits. The UI shows "Claude plan (subscription limits)" versus "API key (Console billing / plan API credits)" from `auth/status`.

### 8. Measurements

Release build. Times are from spawning the adapter (or `claude`), unless noted.

| Step | ACP (adapter + installed `claude`) | ACP (bundled CLI) | Terminal (`claude` in PTY) |
|---|---|---|---|
| `initialize` answered | 221–226 ms warm (5 runs); 312–496 ms on a fresh install dir; one outlier 1.25 s | 221–229 ms | first PTY output 150–400 ms |
| Identity known (`auth/status_update`) | 324–333 ms | 329–340 ms | n/a |
| Session ready (`session/new` OK; MCP `tools/list` in terminal) | **0.70–1.10 s** (one 1.68 s) | 0.91–1.31 s | **1.1–1.8 s** including the scripted trust dialog; 0.48 s on `--resume` |
| `session/load` with replay | 0.99 s | | |
| `session/resume` | 2.24 s (slow `initialize` that run) | | |
| Prompt to first output | **3.4 s** to the first text token (resumed session, text-only answer; spawn to first token 5.6 s). A new session's first `tool_call` took 3.6 s. | | 5.6–6.1 s from typing to the MCP `tools/call` |

The model was the user's default (Opus 5.5, high effort), so prompt latency is the model's, not the protocol's. The ACP overhead before the first prompt is about 1 s, and adapter `initialize` is a ~225 ms Node start. Keeping one adapter process per workspace and many sessions on it amortises both. The adapter advertises `promptQueueing`, `session/list` and multiple sessions per connection.

### 9. Process and environment hygiene

- **Inherited environment:** a child started from inside a Claude Code session inherits `CLAUDECODE`, `CLAUDE_CODE_*` and `CLAUDE_CODE_CHILD_SESSION`. The terminal-mode CLI then **turned transcript saving off** ("inherited CLAUDE_CODE_CHILD_SESSION marker"), so `--resume` found nothing. Running from a clean environment fixed it. coqod must start agents with a **curated environment**: the user's login environment, without the `CLAUDE*` session markers it may have inherited (for example when a developer launches coqod from a Claude Code terminal).
- **Folder trust:** a new folder shows Claude Code's own trust dialog, whose default is "No, exit", before any MCP server starts. In terminal mode the user answers it in the tab. In ACP mode the SDK path didn't prompt. Coqo must never answer this dialog for the user.

## Policy compliance checklist (Coqo code paths)

- [x] **No claude.ai login or OAuth in Coqo.** Coqo doesn't advertise `auth.terminal`, so it gets no auth methods. The sign-in UX is "Open a terminal tab running `claude`, then `/login`" (or `claude auth login`), which is Anthropic's own flow in the unmodified binary. Coqo never captures or parses it.
- [x] **No reading of `~/.claude` credentials (or anything else there).** The prototype and the planned `acp-host` read nothing under `~/.claude` or `~/.claude.json`. The billing source comes from ACP `auth/status`. Session lists come from ACP `session/list`, not transcript files. (The adapter, a separate process, reads settings and transcripts but not credentials.)
- [x] **Unmodified binary.** `CLAUDE_CODE_EXECUTABLE` points at the user's own installed `claude`, which the Agent SDK drives with its standard flags. Terminal mode runs it interactively. Nothing is patched, replaced or shimmed. Coqo doesn't ship a `claude` either: the adapter is installed with `--omit=optional`.
- [x] **No proxying.** The CLI talks to Anthropic directly. Coqo sees only ACP content. Coqo never sets `ANTHROPIC_BASE_URL`, and it doesn't use the adapter's `providers/set` or gateway auth to route traffic through itself.
- [x] **Secrets.**
  - ACP-login mode handles no secrets.
  - API-key mode passes the keychain key only in the adapter's **environment**, never on argv, in files or in logs.
  - Because the adapter puts MCP server `env` on the CLI's argv, the `coqo-editor` MCP config must carry **no secret**. `coqod mcp` authenticates through the user-only socket's peer-credential check, and the session id only routes; it isn't a secret (PLAN §9).
- [x] **Billing shown.** The indicator comes from `auth/status` kind and plan, and never displays the SDK's notional `cost` as a charge on a plan.
- [x] **No redistribution of Anthropic's proprietary SDK.** The adapter is fetched from npm onto the user's machine at setup, never vendored into the repo or installers.

**Needs an Anthropic decision (→ #129):** the legal page says developers using the Agent SDK "should use API key authentication", and that third parties may not "route requests through Free, Pro, Max, or Ultra plan credentials on behalf of their users". It also says end users may sign in to the unmodified binary with their own subscription, and the help centre says third-party app usage draws from subscription limits. Whether **the ACP adapter (Agent SDK) on a user's own plan login inside an open-source editor** counts as permitted end-user use (like Zed and JetBrains) or as routing on the user's behalf is a policy question we don't answer here. Terminal mode is unambiguously the "end user runs the unmodified binary" case.

## Decision

**Go: integrate Claude through the Claude Agent ACP adapter (`@agentclientprotocol/claude-agent-acp`, pinned exactly) on the user's own installed `claude` (`CLAUDE_CODE_EXECUTABLE`), driven by `agent-client-protocol` (pinned exactly) in `crates/acp-host`. Build terminal mode alongside it.** Every S3 question came back positive: lifecycle, MCP injection, permissions, diffs, `available_commands`, load/resume after a client restart, terminal mode with `--mcp-config` and `--resume`, and billing detection without credentials.

- ACP is the default for API keys, Console logins, Bedrock, Vertex and Foundry, Enterprise plans, and (for M4 dogfooding) the user's own plan login.
- A setting **"Use Claude plan logins through ACP"** starts the adapter with `--hide-claude-auth` when it's off. Non-Enterprise subscription sessions then fail with `auth_required` / `claude_subscription_not_supported`, and Coqo offers terminal mode instead. #129 decides that setting's default before 1.0.

**Fallback (unchanged from PLAN §11): terminal mode only for subscription users.** We switch to it by flipping the setting above if Anthropic says ACP on a plan login is not permitted, or if the adapter stops supporting `CLAUDE_CODE_EXECUTABLE`. Editor features keep working there through `--mcp-config`.

## Consequences

- **PLAN §7.1:** live Claude edits come from tool-call diffs plus a workspace file watcher, not from `fs/write_text_file`. **PLAN §7.3** names the adapter package, says Coqo points it at the installed `claude`, adds the plan-login setting, and updates the billing note. PLAN §11 and §13 link this ADR.
- **#54 (ACP host)**, guidance:
  - Pin `agent-client-protocol = "=5.6.1"` with features `process` and `unstable_auth_status`. `session/load` and `session/resume` are ungated v1 requests. Speak v1, since v2 is still in preview.
  - Handle `UntypedMessage` for `_auth/status_update` and future extensions.
  - Advertise `fs`, `plan` and (optionally) `terminal`, but don't rely on `fs/*` for Claude: add a workspace file watcher.
  - Run one adapter process per workspace with many sessions. Reattach with `session/resume` and fall back to `session/load`.
  - **As built (#185): client terminals.** When the daemon's terminals are wired in (`TerminalHandle`, late-bound like `FsHandle`), `initialize` advertises `clientCapabilities.terminal`, and `terminal/create`, `output`, `wait_for_exit`, `kill` and `release` run in daemon PTYs (ADR 0009). These PTYs are listed in `terminal/list`, titled `<session title>: <command>`, and the user can open them as terminal tabs.
    - The host refuses a `cwd` outside the session's workspace roots or one that isn't a directory. It applies `outputByteLimit` from the start of the output, at a character boundary.
    - A command with no `args` runs as `/bin/sh -c <command>`; with args, it runs as program plus args. `PAGER` and `GIT_PAGER` are set to `cat` unless the agent sets them.
    - The output is the emulator's grid as plain text: scrollback plus screen, wrapped rows joined, no escape sequences. It is reported `truncated` once the 10 000-line scrollback is full.
    - Tool-call `terminal` content is rewritten from the ACP id (`term-<n>`) to the daemon terminal id, so the chat card shows an **Open terminal** link. Content naming a terminal the session didn't create (or one from replayed history) is dropped.
    - `terminal/release` frees a terminal at once, unless a tool call shows it. Then the terminal stays, exited, so the link keeps working, until the session closes. Closing a session, or losing its agent connection, kills and frees its terminals.
    - Permission prompts are unchanged: the agent asks before it runs a command, and Coqo never answers for the user.
    - **Claude:** adapter 0.113.0 never sends `terminal/*`. Its Bash tool runs inside Claude Code, and the only terminal-shaped output it knows is the Zed-style `_meta.terminal_info` / `terminal_output` display extension. It sends that only when the client advertises `_meta.terminal_output`, which Coqo doesn't. Advertising `terminal: true` therefore changes nothing for Claude: Bash output stays text in the tool-call card. Coqo advertises it to every ACP agent (Claude included), because agents that do use client terminals (other ACP agents) get live, killable terminal tabs from it.
  - Start the adapter with a curated environment and supervise its exits. Treat an exit after close as normal.
- **#56 (agent-claude via ACP)**, guidance:
  - Detect `claude` on `PATH`, plus Node ≥ 22.
  - Install the adapter into Coqo's data directory with `npm install --omit=optional @agentclientprotocol/claude-agent-acp@<pinned>`, and run `<data>/node_modules/.bin/claude-agent-acp` with `CLAUDE_CODE_EXECUTABLE=<realpath of claude>`.
  - Signed-out flow: show `auth_required` with a button that opens a terminal tab running `claude` (the user runs `/login`).
  - Billing badge from `auth/status`; `--hide-claude-auth` setting as above.
  - Surface modes and `available_commands`.
  - Never ship the SDK or a `claude` binary.
  - **As built (#56):** the `coqo.claude` built-in mod (`crates/agent-claude`, provider `claude-acp`). It lives in `crates/` rather than `mods/` because it needs `coqo-acp-host` and `coqo-agents`.
    - The adapter goes into `<data>/claude-acp` (`npm install --prefix <dir> --omit=optional --save-exact`), and only when the user presses "Install Claude ACP adapter (npm)". That button is a daemon action in the provider's status (`agents/providers`), run through `agents/runAction`.
    - Until the adapter is installed, the provider reports `available: false` together with a message.
    - Coqo runs the adapter as `<node on PATH> <dir>/node_modules/.bin/claude-agent-acp`, so the Node.js it checked (≥ 22) is the one that runs it, whatever the script's `#!/usr/bin/env node` would find.
    - **Curated environment:** the daemon's environment minus Claude Code session markers. Those are `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_AGENT_SDK_VERSION`, `AI_AGENT`, and every `CLAUDE_CODE_*`. A few `CLAUDE_CODE_*` variables are kept because they are user configuration: `CLAUDE_CODE_USE_*`, `_SKIP_*`, `_DISABLE_*`, `_CLIENT_*`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `_API_KEY_HELPER_TTL_MS`, `_SUBAGENT_MODEL` and `_EFFORT_LEVEL`. `CLAUDE_CODE_EXECUTABLE` is then set to the realpath of `claude`.
    - **Sign-in action:** a terminal action that runs the `claude` found on `PATH` in a terminal tab in the workspace.
    - **Plan-login setting:** the mod declares it as `claude.acp.plan-logins` (default on). Until the settings store (#26) exists, coqod reads it from `COQO_CLAUDE_PLAN_LOGINS=0|false|off`.
    - **As built (#216): the Coqo operating guide.** A short, versioned text (`crates/agent-claude/guide.md`, about 250 words) is sent as ACP `systemPrompt: { mode: "append", content: [text] }` on `session/new`, `session/resume` and `session/load`, only when the agent advertises `sessionCapabilities.systemPrompt` (adapter 0.113.0 does). Never `override`: the adapter keeps Claude Code's `claude_code` preset, the user's CLAUDE.md and settings, and appends. A prompt that changes on resume applies at the next compaction. `agent-client-protocol` 5.6.1 has the field typed and stable (`NewSessionRequest::system_prompt` and friends), so no `_meta` is needed. The generic part is `AcpOptions::system_prompt` in `coqo-acp-host`; the Claude mod fills it. Setting `claude.guide` (default on); until #26, `COQO_CLAUDE_GUIDE=0|false|off`. See ADR 0014, *Agent guidance*.
    - **As built (#98): Coqo's Claude Code plugin.** `integrations/claude-code/coqo-plugin` is a Claude Code plugin named `coqo`: `.claude-plugin/plugin.json`, the skill `skills/coqo-walkthrough/SKILL.md` (how to write a walkthrough and publish it with `publish_walkthrough`, PLAN §7.6) and the command `commands/walkthrough.md` (`/coqo:walkthrough`). The mod compiles it in (`coqo_agent_claude::plugin`), writes it at startup to `<data>/claude-plugins/coqo` (the directory replaced whole, a symlink there removed rather than followed, so it holds only these files), and sends `_meta: { claudeCode: { options: { plugins: [{ type: "local", path }] } } }` on `session/new`, `session/resume` and `session/load` (`AcpOptions::session_meta`, generic in `coqo-acp-host`). Adapter 0.113.0 passes `_meta.claudeCode.options` to the Agent SDK, whose `plugins` option starts Claude Code with `--plugin-dir <path>`: the session's `available_commands` then list `coqo:walkthrough` and `coqo:coqo-walkthrough` (verified against Claude Code 2.1.332; without the option, neither). The plugin is loaded for Coqo's sessions only. Coqo installs nothing into the user's Claude Code configuration (no `/plugin install`, nothing under `~/.claude`), and the path is not a secret, so its place on the CLI's argv is fine. Setting `claude.plugin` (default on); until #26, `COQO_CLAUDE_PLUGIN=0|false|off`. **Outside Coqo** (a `claude` the user runs in a terminal): the user can load the same plugin themselves with `claude --plugin-dir <repo>/integrations/claude-code/coqo-plugin` (or Coqo's copy in its data directory); `publish_walkthrough` exists only where the `coqo-editor` MCP server is connected, and without it the skill says to give the walkthrough in the chat. *Alternative:* an MCP prompt on `coqo-editor` (Claude Code lists MCP prompts as `/mcp__coqo-editor__…` commands). It would also reach terminal mode and other MCP clients, but a prompt is only user-invoked, so Claude could not pick the skill up by itself at the end of a change; kept as a follow-up for terminal mode (#57).
- **#57 (terminal mode):** generate `--session-id <uuid>` and resume with `--resume <uuid>`. Put the generated `--mcp-config` file in the user-only runtime directory, with no secrets. Leave the trust dialog to the user. Start from a curated environment without inherited `CLAUDE*` markers.
- **Follow-up:** evaluate MCP-over-ACP (`"type":"acp"`) for `coqo-editor` in ACP mode. It needs no extra process, and nothing would land on argv. The stdio `coqod mcp` stays the baseline because terminal mode needs it anyway.
- **Re-check before 1.0 (#129):** the adapter's launch flags and `CLAUDE_CODE_EXECUTABLE` behaviour (they changed in SDK 0.3.326), the legal page, and the Agent SDK credit status.
