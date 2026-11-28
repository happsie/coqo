# Coqo: Product & Technical Plan

> Status: **DRAFT v2 for approval** · 2026-11-26
> v2 changes: Rust-native architecture, background daemon (`coqod`), open source (Apache-2.0), GitHub auth and name confirmed.
> After approval: design mocks (§14), then GitHub milestones and issues (§12).

---

## 1. Vision

**Agents write the code. Humans keep the knowledge.**

Teams that work 100% agentically ship faster, but over time they lose their grip on *what* got built and *why*. Coqo is an open-source desktop editor for macOS and Linux that treats the agent as the main author. Every feature is designed so the person who owns the code still understands it:

| Problem in agentic teams | Coqo answer |
|---|---|
| The plan is prose in chat, and the structure gets decided without you | **Skeleton** (Coqo's plan mode): the agent proposes the change's shape and flow in the code itself, you edit and approve it, and the result is checked against it |
| Agent questions arrive as plain text with no code context | **Anchored questions**: the question is pinned to the exact lines it is about |
| "It's done": a 40-file diff nobody really reads | **Code walkthroughs**: changes in a logical order, with reasoning and risks |
| Review feedback gets typed into chat by hand | **Native diff with comment threads** that go straight back to the agent |
| PR review is a separate context switch | **PR Inbox** using the same diff and walkthrough tools, plus an AI review skill |
| Several agents in several repos | **Multiple workspaces** in one window, plus an **alert center** for agents waiting on you |
| Closing the editor kills the agent | **Background daemon**: agents keep working while the UI is closed, and the UI reattaches when reopened |
| "Tests pass", but what actually ran? | **Live trace**: DAP probes record real values while the agent tests acceptance criteria, and you can replay them later |

### Principles
1. **Everything is a mod.** The core is a small kernel. The editor, file tree, chat, git, and language support are all mods registered through the same contribution API.
2. **Chat sits next to code as an equal.** A chat session is a tab like a file. It can be split, docked, and kept around.
3. **Any provider, within the rules.** Claude comes first, then OpenAI and any OpenAI-compatible or ACP agent. Coqo never handles Claude subscription credentials (§7.3).
4. **Anchor everything to code.** Questions, skeleton steps, walkthrough steps, review comments, and trace values all use one anchor model.
5. **The UI can go away; the work goes on.** Agents, language servers, and traces live in the daemon, not in the window.
6. **Local-first and private.** No telemetry by default. Secrets go in the OS keychain. Trace data stays on the machine.
7. **Calm by default ("zen").** Floating rounded panels with breathing room, raised (tactile) buttons, and a relaxed, warm, low-contrast-but-accessible palette. Attention colours appear only when something actually needs you.
8. **Lean by default.** Full language intelligence is opt-in per language with the ⚡ **bolt** (§7.1.1). Syntax highlighting is always on.
9. **Native speed.** GPU-rendered UI. Budgets: keystroke-to-paint under 8 ms (120 fps), cold UI start under 500 ms with the daemon running, a 100k-line file opens in under 200 ms, under 150 MB RAM for the UI.

---

## 2. Scope

**Platforms:** macOS (arm64 + x64) and Linux (x64 + arm64; Wayland + X11). Windows is not in v1, but nothing in the design should rule it out (GPUI supports Windows).
**Licence:** **Apache-2.0**, open source from day one.
**Native language support (built-in mods):** Java, Kotlin, mixed Java+Kotlin, TypeScript, JavaScript, React (JSX/TSX), HTML, CSS, JSON, YAML, Markdown.
**Build systems:** Maven, first-class (multi-module, `mvnw`, surefire/failsafe). Gradle can come later as a mod.

**Not in v1:** Windows, real-time collaboration, remote/SSH development (the daemon design makes it possible later, §4.4), VS Code extension compatibility, a public mod marketplace.

---

## 3. Technology decision

### 3.1 Recommendation: Rust end-to-end, with a GPUI native UI and a Rust daemon

| Layer | Choice | Notes |
|---|---|---|
| UI framework | **GPUI** (Zed's GPU-accelerated UI framework, Apache-2.0) | Metal on macOS, Vulkan/Blade on Linux. Built and used for exactly this kind of editor. |
| Component library | **gpui-component** (Longbridge, Apache-2.0), consumed through the `gpui-kit` umbrella crate | 60+ widgets: dock layout, virtualized list/table, markdown, **code editor (rope + tree-sitter, LSP diagnostics/completion/hover, stable at 200k lines)**, side-by-side diff. Spike S4 chose its editor, extended by Coqo ([ADR 0002](adr/0002-ui-editor-component.md)). |
| Async runtime | tokio (daemon), GPUI executor (UI) | |
| Text | rope (from gpui-component or `ropey`) + **tree-sitter** (highlighting, structure, re-anchoring, probe planning) | |
| Buffer sync UI↔daemon | Replicated buffers: operation log or CRDT (`yrs` is the candidate) | Spike S6 |
| LSP / DAP | own clients on `lsp-types` / DAP schema types | |
| MCP | **`rmcp`** (official Rust MCP SDK) | |
| ACP | **`agent-client-protocol`** crate (official) | |
| Mod runtime | **wasmtime** + WASM component model (WIT) | §5 |
| Storage | SQLite (`rusqlite`) + JSON files | |
| Git | `gix` (gitoxide) for fast reads (status, blame, diff); `git` CLI for writes (commit, rebase, worktree) | |
| Terminal | `alacritty_terminal` + `portable-pty` | Same approach as Zed |
| Search / watch | ripgrep crates (`grep`, `ignore`), `notify` | |
| GitHub | `octocrab` + GraphQL | |
| Secrets / notifications | `keyring` (Keychain / Secret Service), `notify-rust` / macOS UserNotifications | |
| Packaging | `cargo-bundle` / `cargo-packager`: notarized `.app`/`.dmg`, AppImage + `.deb` + `.rpm` + Flatpak | |

**Licence hygiene:** GPUI and gpui-component are Apache-2.0. Most of Zed's *other* crates (`editor`, `project`, `language`, …) are **GPL-3.0 and must not be used**. CI runs `cargo-deny` to enforce this.

### 3.2 Why Rust over Electron

| | Full Rust (GPUI + Rust daemon) **(recommended)** | Rust daemon + Electron UI | All TypeScript (Electron) |
|---|---|---|---|
| Typing latency / scroll | Native GPU, Zed-class | DOM, VS Code-class | DOM, VS Code-class |
| UI memory | ~100–150 MB | ~400–600 MB | ~400–600 MB |
| Daemon footprint (runs all day) | ~20–50 MB, single static binary | ~20–50 MB | Node ~80–150 MB |
| Time to first dogfood | Slowest: more UI to build (diff view, popovers) | Medium | Fastest |
| Mod authoring | WASM (Rust, TS/JS via `jco`), process mods in any language, declarative UI | Full TS/React UI mods | Full TS/React UI mods |
| UI mod freedom | Declarative views + editor decorations (no arbitrary UI code) | Anything | Anything |
| Team skills | Rust required for core | Rust + TS | TS only |

Honest notes:
- Electron is not *slow* for editing (VS Code proves that), but it will never feel like Zed and it costs memory. JDT LS and kotlin-lsp use 1–3 GB on their own, so every saved MB counts on dev laptops running several workspaces.
- Not having the Claude Agent SDK in Rust is **not** a problem: Claude is integrated through ACP (the Claude Agent ACP adapter is built on that SDK) or the terminal mode. Both are separate processes anyway (§7.3).
- The **daemon protocol is the real boundary**. If the GPUI UI turns out too slow to build, an Electron or web UI can be plugged onto the same daemon later without touching agents, LSP, or trace. That makes the Rust bet reversible on the UI side.
- GPUI is pre-1.0. We get it as the `gpui-pre` crates.io snapshots of Zed's GPUI that gpui-component builds against, pinned exactly through `gpui-kit` ([ADR 0002](adr/0002-ui-editor-component.md)). We upgrade deliberately and keep our widgets behind our own `coqo-ui` facade. One crate of that stack, `gpui-base` (the editing engine), is vendored with a small additive patch for remote edits ([ADR 0008](adr/0008-editor-remote-edits-patch.md)), so each upgrade re-vendors it.

### 3.3 Alternatives rejected
- **Fork Zed:** GPL editor crates, a WASM extension API too narrow for "everything is a mod", and permanent rebasing.
- **Fork Code-OSS:** a huge workbench that is hard to reshape around chat, plus permanent rebasing.
- **Tauri:** WebKitGTK on Linux is inconsistent for heavy editor UIs, so it gets neither native speed nor Electron's consistency.

---

## 4. Architecture

### 4.1 Overview

```
 ┌──────────────── coqo (UI client, GPUI) ────────────────┐   ┌─ coqo (2nd window / CLI) ─┐
 │ workbench · editor · diff · chat · trace explorer       │   │  attaches to same daemon   │
 │ renders declarative mod views · local buffer replicas   │   └─────────────┬──────────────┘
 └───────────────────────────┬─────────────────────────────┘                 │
                             │  Coqo Protocol (unix socket, 0600 + peer-cred check)
 ┌───────────────────────────┴──────────── coqod (daemon) ─────────────────────────────────┐
 │ Kernel: mod registry · wasmtime host · process supervisor · event log · storage · secrets│
 │ Per workspace: buffers (authoritative) · fs watch · git · LSP clients · DAP/trace ·      │
 │                agent sessions · GitHub poller · alert engine · MCP endpoint              │
 └──┬──────────┬───────────┬────────────┬──────────────┬─────────────────┬─────────────────┘
    │ stdio    │           │            │              │                 │
  jdtls   kotlin-lsp     vtsls    java-debug/      claude-agent-acp,   process mods
                                  js-debug         codex, gemini …      (any language)
                                                        │ stdio MCP
                                                   coqod mcp ──► daemon socket
```

### 4.2 What runs where

| Daemon `coqod` (survives UI close) | UI `coqo` (can come and go) |
|---|---|
| Workspaces and their state | Windows, layout, focus, themes |
| **Agent sessions** (agent processes are daemon children) | Chat rendering, input |
| Authoritative buffers, file watching, saving | Local buffer replicas (typing is instant locally), editor rendering |
| **LSP servers**: stay warm, so JDT LS indexing survives UI restarts | Completion, hover, and diagnostics *display* |
| Git, worktrees, GitHub polling | Diff and PR views |
| MCP endpoint for agents (`coqod mcp`) | Popovers for questions and walkthroughs |
| DAP sessions, trace capture, `traces.db` | Trace explorer, inline values |
| Alert engine + OS notifications | Alert center, badges |
| Mod logic (WASM, process mods, core mods' host parts) | Rendering mods' declarative views and decorations |

### 4.3 Daemon lifecycle
- **Start:** the UI connects to `coqod.sock` (`$XDG_RUNTIME_DIR/coqo/` on Linux, `~/Library/Application Support/Coqo/run/` on macOS). If no daemon is running, it spawns one detached (`setsid`). Both sides handshake on protocol version and build. You can also enable *start at login* (systemd `--user` unit or launchd agent).
- **UI closed:** agents keep running. Blocking questions, permission prompts, and "agent finished" become **OS notifications sent by the daemon**. Clicking one launches the UI straight to that question. On macOS the daemon ships as a helper inside the `.app` bundle so notifications work.
- **Reattach:** the UI asks for a snapshot (workspaces, sessions, open questions, alerts, buffer state), then subscribes to the **event log** from the last sequence number it saw. Unsaved edits and chat history are all there.
- **Several clients:** more than one window, plus the `coqo` CLI (`coqo status`, `coqo agents`, `coqo open <ws>`), can attach at the same time.
- **Idle policy:** with no UI attached and no busy agent or trace, language servers shut down after N minutes (default 30) and the daemon exits after M minutes (default 60). There is an "always on" setting. Socket, lock, spawn and idle details are in [ADR 0007](adr/0007-daemon-lifecycle.md).
- **Updates:** a new UI with an older daemon keeps working if the protocol is compatible. Otherwise it offers to restart the daemon once agents are idle. Sessions resume after a restart through ACP `session/load` or Claude Code session resume, where supported. The daemon checks GitHub Releases, verifies them with minisign against a compiled-in key and swaps the install; the UI relaunches, and the daemon restarts once no agent turn runs, rolling back once if the new daemon does not start ([ADR 0031](adr/0031-in-app-updater.md), #349).
- **Crash safety:** agents run in their own process groups, and the event log and transcripts are persisted to SQLite. After a daemon crash the supervisor restarts it and resumes sessions where the agent supports that.
- **Sessions the agent can't resume** are marked ended ("Can't resume"), never silently restarted behind their transcript. "Start a new chat with this context" carries the old transcript into a new session, and forks work the same way, from a chosen prompt. Retry sends the last prompt again, and closed chats reopen from the CHATS history ([ADR 0026](adr/0026-session-resume-and-forks.md)).

### 4.4 Coqo Protocol
- JSON-RPC 2.0 with length-prefixed frames over the Unix socket. MessagePack encoding is negotiable for hot paths (buffer ops, trace streams). Supports request/response, notifications, and **subscriptions** with sequence numbers.
- Versioned, documented schema (`protocol/`). The Rust types in `crates/protocol` are the source of truth. A JSON Schema and a TypeScript client stub are generated from them for tooling and possible future UIs ([ADR 0003](adr/0003-coqo-protocol-v0.md)).
- The handshake (`coqo/hello`: protocol version + build) is frozen across versions. Equal major versions are compatible, and an incompatible one is reported as a clear error, never a hang. Subscriptions resume from the last seen sequence number, or fail with a resume-gap error that tells the client to re-snapshot.
- **Buffers** replicate as a daemon-authoritative op-log: UI replicas apply their own edits immediately and send ranged edits (`buffer/edit`, UTF-8 byte offsets) against a buffer revision; the daemon publishes every accepted change on the `buffer/changes` topic, with the buffer revision as its sequence number, and agents edit at head. See [ADR 0006](adr/0006-buffer-replication.md) for the messages, undo and anchors, and [ADR 0010](adr/0010-replicated-buffers.md) for replica ids, disk merges, saving and the journal.
- Because the protocol is transport-agnostic, running `coqod` on a remote machine over SSH later is straightforward. That gives remote development without changing the UI.

---

## 5. Mod system ("everything is a mod")

Everything registers through one **contribution registry** with typed, versioned **services** (`coqo.editor@1`, `coqo.git@1`, `coqo.annotations@1`, `coqo.agents@1`, …). Mods `provide` and `consume` services, so any of them can be disabled or replaced.

There are four tiers, all using the same manifest and contribution model:

| Tier | What | Runs in | Use for |
|---|---|---|---|
| **Core mods** | Rust crates in the repo, implementing `coqo_mod_api::Mod` | daemon and/or UI | Built-ins that need native speed: editor, diff, LSP client, git, trace |
| **WASM mods** | WASM components (wasmtime, WIT world `coqo:mod@1`), written in Rust or TS/JS (`jco`/componentize-js) | daemon, sandboxed (WASI p2 capabilities) | Third-party mods: languages, commands, MCP tools, alert sources, views, themes |
| **Process mods** | Any executable speaking the **Coqo Mod Protocol** (JSON-RPC over stdio) | separate process, supervised by daemon | Heavy or ecosystem-bound mods, e.g. one written in Kotlin or TS/Node |
| **Content mods** | A manifest and data files, no code (`tier = "content"`, [ADR 0033](adr/0033-mods-from-github.md)) | nothing runs | Themes, keybindings, settings sections, static views, languages without servers |

Rules:
- Core mods may only depend on `coqo-mod-api`, never on kernel internals. `cargo-deny` plus a CI dependency-graph check enforce this, so the public API is dogfooded.
- **UI contributions are declarative**: a mod returns a view tree (list, tree, table, form, markdown, code snippet, button, badge, split, tabs, progress) that the UI renders natively, plus *editor contributions*: decorations, gutter markers, inline hints, popovers, code lenses. That keeps mods fast and sandboxable. A webview escape hatch is not in v1.
- **Manifest** `coqo.mod.toml`: id, version, engine range, tier and entry, activation events, provides/consumes, **permissions** (fs scope, network hosts, process spawn, secrets), contributes.
- **Contribution points:** commands, keybindings, menus, settings schema (sections and typed fields, [ADR 0032](adr/0032-settings.md)), views (sidebar / panel / editor tab / status bar / rail), languages (tree-sitter grammar + LSP server definition), debug adapters, trace probe planners, agent providers, **MCP tools** (exposed to agents through `coqod mcp`), chat slash commands, context providers (@-mentions), annotation renderers, walkthrough sources, alert sources, git hosting providers, themes, icon packs.
- **Agent-facing contributions:** a mod can contribute editor MCP tools, served to agents through `coqod mcp`. Skills, slash commands and agent plugins belong to the agent harness, not to Coqo mods.
- **Installing** ([ADR 0033](adr/0033-mods-from-github.md)): from a local folder, or from GitHub as `owner/repo[@ref][#subdir]`. The source can be a mod or a Coqo marketplace (`coqo.marketplace.toml`). Claude Code plugins and marketplaces are not read.
  - Coqo pins the ref to a commit SHA and shows a dry-run plan before anything is downloaded beyond text files. The plan lists the files, the permissions, the contributions, a trust level and every command that would run.
  - Nothing runs before the user approves, and nothing is built on install: WASM and process binaries come prebuilt, with sha256 checks and an optional minisign key pinned on first use.
  - Updates are offered, never applied silently.
  - Core mods can't be installed. They are compiled in.
  - A repository can recommend mods in `.coqo/mods.toml`, which never installs anything by itself.
  - Hot reload in dev mode. `coqo mod new --tier wasm|process` scaffolds one.
- **UI by tier:**
  - core mods contribute native GPUI views (the UI-side API is #353's);
  - WASM and process mods contribute declarative view trees;
  - content mods contribute settings sections and static view trees.
  - All of them go through the same contribution points.

---

## 6. Built-in mods catalog

| Mod | Responsibility |
|---|---|
| `workbench` | Layout/docking, tab groups, command palette, keybindings, settings UI, themes |
| `editor` | Code editor: multi-cursor, splits, folding, sticky scroll, minimap (optional), vim mode (optional) |
| `files` | File tree, fuzzy finder, file operations |
| `search` | Project search/replace ([ADR 0025](adr/0025-search-mod.md)), symbol search |
| `terminal` | Integrated terminal tabs (daemon-owned PTYs; in `coqod` and `coqo` until the mod API can host it, see [ADR 0009](adr/0009-daemon-terminals.md)) |
| `lsp` | Generic LSP client (diagnostics, completion, hover, nav, rename, code actions, inlay hints, semantic tokens, formatting) + ⚡ bolt lifecycle (`coqo.intellisense@1`) |
| `syntax` | tree-sitter highlighting, folding, outline, symbol tags, which are always on (§7.1.1) |
| `lang-web` | TS/JS/React (vtsls), ESLint, Prettier, HTML/CSS/JSON servers, Emmet |
| `lang-java` | Eclipse JDT LS (Maven import via m2e) |
| `lang-kotlin` | JetBrains `kotlin-lsp` |
| `jvm-mixed` | Mixed Java+Kotlin: per-language routing, compile-on-save to keep diagnostics clean (§7.2) |
| `maven` | Module tree, goals, run configs, test explorer, dependency tree, agent tools |
| `git` | Status, hunk staging, commit, branches, **worktrees**, stash, blame, log |
| `diff` | Shared diff component + comment threads |
| `agents` | Chat UI, AgentProvider API, sessions, @-mentions, permission prompts |
| `agent-claude` | Claude: ACP (your own Claude Code login), terminal mode, API key, Bedrock/Vertex |
| `agent-openai` | OpenAI API + Codex CLI (ACP) |
| `agent-acp` | Any ACP agent (Gemini CLI, …) |
| `agent-native` | Coqo's own agent loop for API-key providers (Anthropic, OpenAI, OpenAI-compatible, Ollama, OpenRouter) |
| `mcp` | MCP client + server manager UI (native loop) |
| `editor-mcp` | `coqod mcp` endpoint + MCP tool registry (§7.4) |
| `annotations` | Anchor engine + anchored questions (§7.5) |
| `walkthrough` | Walkthrough schema, skill, coverage guard, player (§7.6) |
| `skeleton` | Skeleton proposals, the skeleton review, the contract and the drift guard (§7.11) |
| `workspaces` | Workspace rail, worktree workspaces (§7.9) |
| `alerts` | Alert engine, center, badges, OS notifications (§7.9) |
| `github` | PR inbox and review (§7.8) |
| `review` | User-chosen code-review skill → draft comments |
| `trace` / `dap` / `dap-jvm` / `dap-js` | Live trace + interactive debugging. `dap-jvm` wraps kotlin-lsp DAP / java-debug / `coqo-jvm-probe` (§7.10) |

---

## 7. Feature design

### 7.1 Editor core (Zed-level editing)
Multi-cursor and column selection, split panes, tab groups, fuzzy file and symbol finder, project search, outline and breadcrumbs, go to definition / references / implementation, rename, code actions, format on save, inlay hints, diagnostics panel, semantic highlighting, indent and bracket guides, folding, sticky scroll, optional vim mode, terminal, tasks/run configurations, and a large-file mode. The starting point is gpui-component's code editor, extended by Coqo where it falls short (anchored popovers, inline ghost values; see [ADR 0002](adr/0002-ui-editor-component.md)).

**Live agent edits:** agent edits arrive in open buffers live, as tracked changes, even in files with unsaved user edits. ACP agents that write through the client (`fs/write_text_file`, served by the daemon) go straight into the replicated buffer. Claude Code writes files itself, so for Claude the daemon takes the diff from the ACP tool call (it arrives before the write) and reconciles open buffers through a workspace file watcher ([ADR 0005](adr/0005-claude-integration.md)).

### 7.1.1 ⚡ Bolt: on-demand language intelligence
JDT LS and kotlin-lsp alone can use 1–3 GB, and often you only want to read code or watch an agent work. Coqo therefore splits language support into two layers:

| Layer | Always on? | Provided by | Features |
|---|---|---|---|
| **Syntax** | ✅ always | tree-sitter via `coqo-syntax`, in the UI and the daemon (`syntax/*`), no server process ([ADR 0011](adr/0011-syntax-layer.md)) | Highlighting, folding, bracket matching, indentation, outline/breadcrumbs, tree-sitter symbol search, word completion, and anchors/walkthrough/trace structure |
| **Intellisense** | ⚡ opt-in per language | LSP servers in the daemon | Semantic completion, go to definition/references, rename, diagnostics, code actions, inlay hints, formatting |

- **The bolt button** in the top bar shows how many languages have intellisense on ("1 of 3"). Its popover lists each language in the workspace with its server, its state (`sleeping · starting · indexing · ready`), live memory use, and a switch. The status bar shows the same state in short form, and a tab shows a small ⚡ badge when its language has intellisense on.
- **Per-workspace memory:** bolt state is saved per workspace and language. There is a global default (off) and a per-repo default in `.coqo/settings.toml`.
- **Agents and the bolt:** Coqo gives agents no `lsp_*` MCP tools (#204, not planned; [ADR 0014](adr/0014-editor-tools-and-mod-tools.md)): code navigation is the harness's job (Claude Code's own LSP, Grep and Read). The bolt governs the editor's language servers.
- **Sleep, not kill:** turning the bolt off shuts the server down cleanly. Where the server supports it, its index cache is kept on disk, so turning it back on later is fast.
- Implemented by the `lsp` mod as a `coqo.intellisense@1` service, so language mods only declare their server and don't manage its lifecycle themselves.

### 7.2 Languages, JVM, and Maven
- **TS/JS/React/HTML/CSS:** vtsls, ESLint LSP, Prettier, html/css/json servers, Emmet. Tree-sitter grammars handle highlighting.
- **Java:** JDT LS with Maven import (m2e), plus the java-debug plugin bundle that is also used for trace (§7.10).
- **Kotlin:** JetBrains `kotlin-lsp`. According to the Kotlin docs it supports JVM Gradle and **Maven** projects.
- **Mixed Java+Kotlin.** The goal is **coexistence, not cross-language navigation**. Both languages live in the same Maven project, each gets full editing support, and the project compiles and runs. Jumping from Java code to its Kotlin definition (or the reverse) is **not required**.
  - Routing: `.java` files go to JDT LS and `.kt` files go to kotlin-lsp. Both servers attach to the same Maven project and must not conflict, for example over output dirs or the project model.
  - Build: the standard mixed setup, where `kotlin-maven-plugin` compiles first (both `src/main/kotlin` and `src/main/java` as source dirs, execution id `kotlin-compile`, which kotlin-lsp's Maven import recognises), followed by `maven-compiler-plugin`. The `maven` mod includes a **mixed-project doctor** that checks the plugin ordering and source dirs and offers a fix.
  - No false errors: JDT LS only sees Kotlin types through compiled classes. `jvm-mixed` therefore triggers an incremental Kotlin compile when `.kt` files are saved, so Java files that use Kotlin types don't show bogus "cannot resolve" diagnostics.
  - **Spike S1** checks that both servers run side by side on a mixed multi-module Maven sample, that diagnostics stay clean after cross-language edits, and that `mvn verify` passes.
- Language servers live in the **daemon**, so JDT LS and kotlin-lsp **stay indexed across UI restarts**.
- **Maven mod:** detects `pom.xml`/`mvnw`, shows a module tree with goals and plugins, handles run configs, parses surefire/failsafe reports into a test explorer, and shows the dependency tree. Agent tools: `maven_run`, `maven_test`, `maven_modules`.

### 7.3 Agent chat (first-class)

**Placement:** a chat session is a **full tab in the main tab area, exactly like a file**. It is not a side panel. It opens full width by default, with the conversation in a centred reading column and a **session panel** inside the tab showing the plan, acceptance criteria, changed files (with a "Review diff" link) and open questions. Like any tab it can be split, moved or dragged next to a file. The sidebar lists the workspace's chats so you can switch between them quickly. Each workspace can have several sessions, and they live in the daemon, so they keep running when the UI closes.

**AgentProvider kinds.** All of them render in the same chat UI:

| Kind | How | Examples |
|---|---|---|
| **ACP agent** | The daemon runs the agent as an ACP client. It renders messages, plans, tool calls, and diffs, answers permission requests, serves `fs/*` and `terminal/*`, and **injects the `coqo-editor` MCP server** in `session/new.mcpServers`. | **Claude** (`@agentclientprotocol/claude-agent-acp`, built on the Claude Agent SDK; Coqo installs it from npm and points it at your installed `claude` with `CLAUDE_CODE_EXECUTABLE`, [ADR 0005](adr/0005-claude-integration.md)), Codex CLI, Gemini CLI, any ACP agent |
| **Terminal agent** | The official, unmodified `claude` CLI runs interactively in a Coqo terminal tab (the PTY is owned by the daemon, so it survives UI close), launched with `--mcp-config <coqo>`. You get the CLI's own UI, and **all Coqo editor features still work through MCP**. | Claude Code TUI |
| **Native loop** | Coqo's own Rust agent loop over provider HTTP APIs, with an API key, Coqo tools, MCP servers (`rmcp`), and skills | Anthropic API, OpenAI API, OpenRouter, Ollama, Bedrock, Vertex |

**Claude authentication, within Anthropic policy.** Per [Anthropic's legal & compliance page](https://code.claude.com/docs/en/legal-and-compliance): third-party developers may not offer Claude.ai login in their own apps, may not route requests through Free/Pro/Max credentials on behalf of users, and may not collect, store, or intermediate Claude.ai credentials or tokens. An end user is not prevented from "signing in to the unmodified Claude Code binary with their own Claude subscription".

| Mode | Auth | Billing |
|---|---|---|
| **Claude Code (your login)** via ACP | You run `claude` → `/login` yourself in the official binary. Coqo only spawns it and **never** sees, reads, or stores tokens. | Claude plan. ⚠️ Anthropic announced (May 2026) that ACP, `claude -p`, and SDK usage would move to a separate monthly **"Agent SDK credit"**. Anthropic paused that on June 15, 2026: Agent SDK, `claude -p` and third-party app usage still draw from subscription limits. Since Oct 7, 2026, Max/Ultra/Team plans also include monthly API credits, used only by an API key from the linked Console org. The billing source comes from ACP `auth/status`, and the UI shows which pool applies. **Re-check before release (#129).** A setting can stop plan logins over ACP (the adapter's `--hide-claude-auth`) and send those users to terminal mode, in case Anthropic rules that out. |
| **Claude Code terminal** | Same: the unmodified CLI, interactive | Normal first-party Claude Code usage of your plan |
| **Claude API key** | Console key in the OS keychain | API pay-as-you-go |
| **Bedrock / Vertex / Foundry** | Cloud credentials | Cloud billing |

Hard rules, documented in `agent-claude` and in CONTRIBUTING: no claude.ai OAuth flow in Coqo, no reading of `~/.claude` credential files, no patching or wrapping of the `claude` binary, and no proxying of subscription traffic. Because Coqo is open source and public, the project should confirm this integration model with Anthropic before the 1.0 release.

**Skills, MCP, plugins:**
- ACP and terminal modes: Claude Code loads the user's own skills, plugins, and MCP servers (`~/.claude`, project `.claude/`, `.mcp.json`) natively. Coqo surfaces them: a slash-command and skill palette (ACP `available_commands`), MCP status, and plugin list.
- Native loop: an Agent Skills loader (`SKILL.md` + frontmatter, progressive disclosure), an MCP client (stdio + streamable HTTP) that reads `.mcp.json`, and slash commands.
- Mods can contribute editor MCP tools for every provider. Skills and plugins belong to the harness, not to mods.
- The agent's own question tools work in the chat: Claude Code's `AskUserQuestion` and MCP servers' forms (ACP form elicitation, which Coqo advertises), and plan approval (`ExitPlanMode`, a permission request). They show as a question card or a permission card, and the answer goes back through the request that asked, with the question text and labels unchanged ([ADR 0029](adr/0029-agent-question-tools.md)).

**Chat UX:** streaming markdown, collapsible tool calls, inline diffs with accept/reject, the agent's plan/todo view, @-mentions (files, symbols, selection, diagnostics, git diff, terminal output, PR), drag code into chat, provider and model picker per session, a usage/billing-source indicator, stop/retry/fork, and permission prompts that also raise alerts (§7.9).

### 7.4 Editor MCP
- An agent launches `coqod mcp --workspace <id> --session <id>` (the same binary in a stdio MCP server mode). It connects to the daemon socket with a one-time session token from its environment, and that token's scope routes calls to the right workspace and agent session ([ADR 0012](adr/0012-editor-mcp-bridge.md)). **This works whether or not a UI is open.**
- **Tools are contributed by mods** through the registry, so a new mod automatically gives agents new editor powers. Mods register them in the daemon (`ModContext::register_mcp_tool`); the bridge lists them over `mcp/*`, names an installed mod's tools `<mod id>_<tool>`, and checks every call's arguments against the tool's schema. The UI publishes each window's editor context to the daemon, and reveals reach it over a daemon topic ([ADR 0014](adr/0014-editor-tools-and-mod-tools.md)).

- **Editor-only tools.** A tool exists only for what no harness already offers: the editor's live state, the user (questions, notes, status), and Coqo's own features (criteria, walkthroughs, trace). Reading, listing and searching files, outlines and LSP navigation are left to the agent's own tools (Claude Code's Read, Glob, Grep, LSP); each tool definition costs context on every turn. Every description says when to use the tool, and the server's MCP instructions say when to reach for each ([ADR 0014](adr/0014-editor-tools-and-mod-tools.md)). Mod tools follow the same rule.
- **Agent guidance (#216).** Claude sessions also get a short, versioned Coqo operating guide appended to Claude Code's system prompt (ACP `systemPrompt`, append mode; setting `claude.guide`). The daemon counts each session's tool calls by tool and group (Coqo core, mod, other MCP, the agent's own) on `SessionInfo.toolUse`, shown in the session panel, and `cargo run -p coqo-evals` measures tool use against the real Claude Code ([docs/evals.md](evals.md)).

| Tool | Purpose |
|---|---|
| `editor_context` | What the user is looking at: active file, selections, cursors, visible lines, open tabs (when the UI is attached), plus the workspace's roots and the session id |
| `editor_reveal` | Show the user code: open a file and select a range |
| `ask_question` | **Anchored question** (§7.5). Blocks until answered (with progress), or returns an id when `wait: false` |
| `await_answer` | Waits for an async question |
| `annotate` | Non-blocking anchored info or warning note ([ADR 0016](adr/0016-anchored-questions.md)) |
| `set_status` | A short status line, optional progress, and `working \| blocked \| done`, which feeds alerts (`needs_input` comes from questions; [ADR 0015](adr/0015-agent-state-and-status.md)) |
| `set_acceptance_criteria` | Registers the task's acceptance criteria (`AC1`, …) and, as the agent verifies them, each one's status (`pending \| met \| failed \| unknown`) and evidence; shown in the chat's session panel, where the user can mark them too ([ADR 0017](adr/0017-acceptance-criteria.md)) |
| `propose_skeleton` | Proposes the change's skeleton before it is written and blocks until the user approves it or sends it back (§7.11); not offered when `skeleton.mode` is `never` |
| `publish_walkthrough` | Publishes a walkthrough (§7.6) |
| `reply_to_review` | Replies in a review thread the user sent (§7.7), optionally resolving it ([ADR 0022](adr/0022-review-threads.md)) |
| `request_review` | Asks the user to review the diff |
| `trace_launch` / `trace_plan` / `scenario_begin` / `scenario_end` / `trace_query` | Live trace (§7.10) |
| `maven_run` / `maven_test` | Build and test |

Blocking calls send MCP progress notifications so they don't time out. Agents with hard tool timeouts can use async mode instead (`wait: false`, then `await_answer`). Questions asked while the UI is closed are queued and trigger an OS notification.

### 7.5 Anchored questions
- **Anchor model** (`coqo-anchors` crate, shared by questions, walkthroughs, comments, and trace):
  `{ path, range: {start:{line,col,offset}, end:{line,col,offset}}, text_hash, text, prefix, suffix, context_before[3], context_after[3], symbol, rev }` (`symbol`: the enclosing tree-sitter symbol's path and kind).
  **Re-anchoring** after edits first maps the range exactly through the buffer's changes when they are known. After a gap (disk change, restart) it tries, in order: the exact range, a search for the snippet, a whitespace-insensitive match, a fuzzy match (line diff + tree-sitter symbol identity), and finally an `orphaned` state shown with its original snippet. The status is `exact`, `moved`, `fuzzy` (with a confidence) or `orphaned`, and a weak or ambiguous match is orphaned rather than guessed. See [ADR 0013](adr/0013-anchors.md).
- **UI:** a gutter marker plus range highlight and a **popover** anchored to the code, with the question (markdown), quick-answer option buttons, a free-text reply, an "open in chat" link, and the asking agent and session. One question can have several anchors ("these two places disagree"). A questions panel lists everything open in the workspace. `F8` / `Shift+F8` jump to the next/previous question.
- **Kinds:** `decision` (options), `clarification` (free text), `confirmation` (yes/no), `info` (acknowledge).
- **Answer flow:** the answer returns as the MCP tool result *and* appears in the chat transcript as a structured message linked to the anchor. Unanswered blocking questions raise alerts.
- **Skills' own questions** (`AskUserQuestion`, MCP elicitations, permission options) are pinned to code the same way when they name a place. Their answer goes back through the original request, unchanged. References like `file:line` in an agent's message become dismissible gutter notes ([ADR 0030](adr/0030-skill-questions-in-code.md)).
- **Store and anchors service:** questions and notes live in `coqo.db` with their anchors (as asked, and as last rebased); the daemon resolves them against the open buffer through its changes, or by fingerprint after a gap, and publishes moves on `questions/changes`. While a session has an open question its state is `needs_input` ([ADR 0016](adr/0016-anchored-questions.md)).

### 7.6 Code walkthrough
- **Schema** (`protocol/schema/walkthrough.schema.json`, generated from the Rust types and exposed to agents as the MCP resource `coqo://schemas/walkthrough.json`; the agent leaves out `id`, `created_by` and `created_at`, which Coqo fills in, and gets every problem of a bad walkthrough at once with its JSON path, [ADR 0020](adr/0020-walkthroughs.md)):
  ```
  Walkthrough { id, title, summary, scope: {kind: worktree|commits|pr, base, head},
                acceptance_criteria[], steps[], trivial_hunks[], created_by, created_at }
  Step { id, title, kind: entry_point|flow|data_model|logic|edge_case|test|config|migration,
         anchors[], what (markdown), why (reasoning), risks[{severity: low|med|high, text}],
         criteria_refs[], depends_on[] }
  ```
- **Generation:** a `coqo-walkthrough` skill. It tells the agent to order the steps the way a reader would follow the code (entry point → flow → data → edge cases → tests), to cover every hunk or list it as trivial, to refer to the acceptance criteria, and to finish with `publish_walkthrough`. For Claude it ships in Coqo's own Claude Code plugin (`integrations/claude-code/coqo-plugin`, with the command `/coqo:walkthrough`), which Coqo writes to its data directory and loads into each of its Claude sessions through the ACP adapter; nothing is installed into the user's Claude Code configuration ([ADR 0005](adr/0005-claude-integration.md), *As built (#98)*). The native-loop form follows the skills loader (#62).
- **Coverage guard:** Coqo computes the diff itself and checks that every changed hunk is covered by a step or explicitly listed as trivial. Uncovered hunks are flagged, so nothing slips past the reader: the walkthrough is still published, but the agent's tool result leads with each uncovered hunk so it can explain them and publish again (which replaces the walkthrough in place, keeping the reader's progress on unchanged steps), and the reader sees a banner listing them (a draft with `require_coverage` is refused instead; overlap rules in [ADR 0020](adr/0020-walkthroughs.md)).
- **Player:** a step list with progress. Each step opens its anchor in the editor or diff view with a popover showing what, why, and risks. Navigate with next/prev. Mark a step "understood" or "question". A question on a step goes to the agent as an anchored message.
- **Where walkthroughs are stored** (open decision §13.1):
  - *Local (default):* in the daemon's data (`coqo.db`, keyed by workspace, [ADR 0020](adr/0020-walkthroughs.md)). Private to you; you can replay a walkthrough for as long as it exists.
  - *Committed (opt-in per repo):* written to `.coqo/walkthroughs/<id>.json` in the repo for the user to commit with the feature (Coqo never commits; opted in with the workspace state `walkthroughs.storage = "committed"` until `settings.toml` exists). Teammates, PR reviewers, and future readers can replay the agent's explanation in Coqo, so it becomes living documentation of why the code looks the way it does.
  - *Attached to the PR (opt-in):* published as a collapsible PR comment (markdown rendering plus embedded JSON) that Coqo's PR walkthrough player picks up.

### 7.7 Git and native diff
- **Git mod:** status (with git's rename detection and `.gitattributes`), hunk/line staging, discarding files, hunks or lines (it always asks first, and untracked files go to the trash so they can be undone), commit (with an agent-drafted message), branches, worktrees, stash, blame, log/graph ([ADR 0018](adr/0018-git-mod-and-protocol.md)).
- **Diff view** (shared with PR review): side-by-side and unified, word-level highlights, collapsed unchanged regions, a file tree with a "viewed" checkbox per file, keyboard navigation, and walkthrough overlay mode. Any diff source can feed it, and other views add gutter markers, line slots and tints to it ([ADR 0021](adr/0021-diff-view.md)).
- **Comment threads → agent:** select lines and comment. Comments build up as a *pending review*, and "Send to agent" sends the batch as one structured message (comments plus anchors plus diff context) to the session that made the change (by default; the user can pick another chat or a new one, and a chat mid-turn is refused). Agent replies land in the threads through the `reply_to_review` tool, and threads can be resolved. The thread model is shared with GitHub (§7.8); only the backend differs. Threads are pinned to a file version by anchor and re-resolved as the files change ([ADR 0022](adr/0022-review-threads.md)).

### 7.8 PR Inbox (GitHub)
- **Auth:** reuse `gh auth token` when the GitHub CLI is installed and logged in. Otherwise use the GitHub OAuth device flow with the Coqo OAuth app. The token is kept in the keychain. GitHub Enterprise hosts are supported.
- **Inbox:** review requested (you or your team), assigned, authored, and mentioned. Shows CI status, unread/updated tracking, filters, and saved searches. Polling is done by the **daemon** (GraphQL with ETags), so "review requested" alerts arrive even with the UI closed.
- **PR view:** description and conversation, files changed in the shared diff, existing threads mapped onto lines, a pending review, submitting **Approve / Request changes / Comment**, reply and resolve, and suggestion blocks.
- **Local worktree per PR**, created automatically, so language servers and agents get the full code without touching your branch.
- **AI assist:** a PR walkthrough (same player as §7.6), and a **user-chosen review skill** per repo (`.coqo/settings.toml` → `review.skill`) or globally. It produces **draft comments marked "AI draft"** that you accept, edit, or discard. **Nothing is posted to GitHub automatically.**
- Sits behind a `coqo.githost@1` service, so GitLab or Bitbucket can be added later as mods.

### 7.9 Multiple workspaces and alerts
- **Workspace rail:** a vertical strip at the far left with one icon per **project**: a repository, with every one of its worktrees, or a folder in no repository. Switch projects with `Cmd/Ctrl+1..9`. A project's icon sums the alerts and running agents of all its worktrees. All workspaces live in the daemon, and background workspaces keep their agents running ([ADR 0028](adr/0028-worktrees-and-projects.md)).
- **Worktrees:** each worktree of a repository is its own workspace (files, chats, terminals, git state), so agents can work on several tasks at once without touching each other's files. A **worktree switcher** at the top of the sidebar, under the project's name, shows the current worktree and branch and lists every worktree (made in Coqo or not) with its branch, change count and agent state; it adds one (**New worktree…**: a branch and a base, by default the current branch, in `<repo>-worktrees/<branch>` beside the repository) and removes one (it warns about uncommitted or unpushed work and never forces, and never removes the main worktree). Palette: "Switch worktree", "New worktree". The daemon runs `git worktree` through the git mod (`git/worktrees`, `git/addWorktree`, `git/removeWorktree`). Worktrees made outside Coqo (an agent's `git worktree add`, Claude Code's subagent worktrees) appear in the list live, without opening anything, and a row shows "Claude working · from <chat>" while a chat's tool calls work in that worktree (#359).
- **Agent states:** `idle · working · needs_input · needs_permission · blocked · error · done`, derived from provider events by one state machine, with the agent's own status line (`set_status`) on top and `state_since` for how long it has waited ([ADR 0015](adr/0015-agent-state-and-status.md)).
- **Alert sources:** permission requests, `ask_question`, the agent's own questions (ADR 0029), `set_status`, a finished turn, agent errors, review requested, CI failed, trace run finished.
- **Alert engine** (in the daemon, [ADR 0019](adr/0019-alert-engine.md)): one alert per source and key (an agent session has at most one standing alert), resolved by itself when the condition clears, kept in `coqo.db`; `alerts/*` methods and the resumable `alerts/changes` topic for the UI, and the `coqo.alerts@1` service for mods' alert sources.
- **UI:** badges on the rail and tabs, an **Agents overview** panel showing all sessions across workspaces with their state and how long they have waited (plus a "welcome back" summary after the UI was away, [design](design/agents.md)), a notification center, and OS notifications (the daemon sends them, and clicking one opens or focuses the UI at the exact question). Includes do-not-disturb and per-source settings. The CLI shows the same data: `coqo agents`.

### 7.10 Live trace (DAP)

**Idea:** while the agent tests the feature for real against its acceptance criteria, Coqo places **non-blocking snapshot probes** on the newly written code and records the actual values flowing through it. You replay each scenario afterwards as a walkthrough with real data. The whole thing runs in the daemon, so the agent can run trace sessions while the UI is closed.

**Flow:**
1. **Criteria:** the agent registers acceptance criteria (`set_acceptance_criteria`).
2. **Probe planning** (`trace_plan`, which you can also edit): diff `base..HEAD`, map the changed hunks to their enclosing functions with tree-sitter and LSP symbols, then place probes at **function entry** (arguments), **each return/throw site** (locals plus the return expression), and **branch lines inside changed hunks**. Caps apply: maximum probes, a per-probe hit limit, and sampling. The probe planner is a contribution point.
3. **Launch/attach** (`trace_launch`): Coqo starts the app with debugging enabled (for example `mvn spring-boot:run` with JDWP, `node --inspect`, or Chrome for React) and attaches the DAP adapter, or it attaches to a dev process it launched earlier.
4. **Scenarios:** the agent calls `scenario_begin({criterion, name})`, runs the scenario (HTTP calls, Playwright, a test class), then calls `scenario_end({result, evidence})`.
5. **Capture**, with the strategy chosen per probe:
   - **Logpoints** (DAP `logMessage`, supported by java-debug and js-debug): *never suspend*. The template encodes the chosen expressions as a tagged payload, parsed from `output` events. Cheap, but limited to `toString`/expression values.
   - **Snapshot breakpoints:** thread-only suspend → `stackTrace`/`scopes`/`variables` to depth N → immediate `continue`. Full locals and object graphs, at a cost of a few milliseconds per hit.
   - **Adaptive:** a probe that starts as a snapshot drops to a logpoint, or stops capturing, if its hit rate goes above a threshold.
6. **Correlation:** v1 tags hits to scenarios by time window and thread. v2 propagates a scenario id (`X-Coqo-Scenario` header) and reads it in probe expressions from the MDC (JVM) or AsyncLocalStorage (Node).
7. **Storage:** SQLite `traces.db` per workspace, with the tables `runs`, `scenarios`, `probes`, and `hits` (ts, thread, probe, stack summary, values with depth and size limits). **Redaction rules** (password/token/secret/authorization/PII patterns, user-extensible) run *before* anything is written. A retention policy cleans up old data.
8. **Trace explorer UI:** runs → scenarios (criterion, pass/fail) → hit timeline. **Trace walkthrough:** step through hits in execution order while the editor jumps to each line, shows **inline ghost values**, and opens a popover with the full variable tree. You can compare two scenarios and filter by probe.
9. **Agent self-verification:** `trace_query` lets the agent check that a branch actually ran with the expected values and attach that as evidence for a criterion.

**Adapters:**
- **JVM (Java + Kotlin).** Key constraint: **a JVM accepts only one JDWP debugger connection**. In a mixed project, *one* adapter must therefore handle probes in both `.java` and `.kt` files. The candidates, in order:
  1. **kotlin-lsp's built-in DAP server.** JetBrains' kotlin-lsp now ships a DAP server, started through the LSP command `workspace/executeCommand` → `start_debug_server`, which returns a TCP port for the DAP client to connect to. Recent releases cover attach to a running JVM, `launch.json`-style JVM launch, line breakpoints, stepping, threads, stack frames, variables, expression evaluation, coroutine async stack traces, and thread dumps. It is built on the IntelliJ debugger, so it is the best candidate for handling Kotlin *and* Java sources. **Open questions for spike S2:** are DAP logpoints (`logMessage`) and conditions supported (they are documented for JetBrains' sibling "Java and Kotlin by IntelliJ IDEA" extension, not for kotlin-lsp itself); is the suspend policy thread-only or all-threads; do breakpoints in `.java` files resolve; is it stable for Maven-launched apps. It was introduced as experimental and kotlin-lsp is still Alpha and partly closed-source, so Coqo has to tolerate API changes. A known quirk: it returns `source.path` as a `file://` URI even when the client asks for `pathFormat: "path"`.
  2. **microsoft/java-debug** (the JDT LS plugin): mature logpoints, conditions, and hit counts for Java. Whether it resolves breakpoints in `.kt` files is unproven. Good choice for Java-only projects.
  3. **`coqo-jvm-probe`** (fallback, and possibly the long-term answer for tracing): a small DAP-speaking adapter built on JDI/JDWP and designed for non-blocking capture. Probes use `SUSPEND_EVENT_THREAD`, so only the hitting thread pauses. Classes are resolved through `ClassPrepareRequest` with source-name filters plus `locationsOfLine`, which works for both Java and Kotlin because both emit standard line tables. Method-exit events give exact return values. It works without either language server.
  - **Strategy:** the `dap-jvm` mod hides all three behind one `TraceAdapter` interface and picks one per project (pure Kotlin → kotlin-lsp DAP, pure Java → java-debug, mixed → whichever S2 proves handles both, otherwise `coqo-jvm-probe`). The same adapter also powers normal interactive debugging in Coqo.
  - If an adapter has no logpoints, every probe uses the snapshot strategy (thread-only suspend → read → continue). That requires a thread-only suspend policy. If S2 shows an adapter suspends all threads, it is used for interactive debugging only, and tracing goes through `coqo-jvm-probe`.
- **Node / TS / React:** vscode-js-debug (DAP; Node plus Chrome via CDP, source maps, logpoints).

**Safety:** dev runs only, a clear "tracing active" indicator, and no attaching to processes Coqo didn't launch unless the user explicitly allows it.

### 7.11 Skeleton (Coqo's plan mode)

**Idea:** before the agent writes a change, it proposes the change's **skeleton**: where the pieces go and in what order things happen, without the details. The user reads it in the code where it will live, edits it, and approves it. The agent then fills it in, and Coqo checks the result against what was approved. This is Coqo's plan mode. A plan in chat is prose that gets skimmed; a skeleton is the structure itself, and a user who chose the names, the placement and the order of operations still knows them afterwards. It also catches a wrong direction before forty files exist.

**Two kinds of element** in one proposal:

| Kind | For | Form | Checked afterwards by |
|---|---|---|---|
| **Shape** | New or changed files, types and signatures | Stub code: the declarations, a one-line doc comment each, and bodies left unimplemented (`todo!()`, `throw new UnsupportedOperationException()`), plus empty tests named after the acceptance criteria | Comparing files, public symbols and signatures |
| **Flow** | Logic added inside existing code | Ordered **intent steps**, one line each, anchored between the existing statements where the code will go | Every step has a hunk, and every hunk belongs to an element |

```
fn process_refund(order_id, amount) {
    let order = load(order_id)?;
  ┆ 1. Reject if amount > remaining refundable (paid − already refunded)
  ┆ 2. Partial refunds allowed, but the currency must match the order's
    let tx = gateway.refund(&order, amount)?;
  ┆ 3. Record the refund in the same DB transaction as the status change
    notify(order.customer);
}
```

- A flow step names a branch, an ordering constraint, a side effect, an error path or a boundary: something the user could disagree with. "Loop over the line items" is a detail and does not belong.
- Flow steps are anchors (§7.5), drawn as ghost lines. They are never written into the file as comments: there is nothing to clean up, and where a step sits ("before the gateway call", "inside the transaction") is part of what is approved.
- Only the public surface is shape. Private helpers are the agent's business, or the skeleton becomes the implementation written twice.

**Nothing is written before approval.** The proposal carries the stubs, and Coqo shows them as a diff against the worktree (the diff view takes any source, [ADR 0021](adr/0021-diff-view.md)). On approval Coqo writes the approved stubs, with the user's edits, into the worktree, and the agent continues from them. A skeleton that is sent back or abandoned leaves the worktree as it was. Where the provider has a read-only mode (Claude Code's `plan`), a session that starts with a skeleton runs in it until approval, so this holds by the harness's own rule and not by the agent's good behaviour (spike S8).

**Schema** (generated from the Rust types and served as an MCP resource, like the walkthrough's):
```
Skeleton { id, revision, title, summary, criteria_refs[], elements[], decisions[],
           state: proposed|changes_requested|approved|superseded, created_by, created_at }
Element  { id, role (one line: what this piece is for), kind: shape|flow,
           shape: { path, change: add|modify|remove|move, moved_from?, stub },
           flow:  { anchor (the insertion point), steps[{ id, text }] } }
Decision { id, question, options[], element_refs[] }
```

**Flow:**
1. **Propose:** the agent explores, then calls `propose_skeleton`. It blocks with progress like `ask_question` (or `wait: false`), the session is `needs_input`, and an alert is raised (§7.9).
2. **Skeleton review** (board 9): an outline of the elements (files → symbols, marked new, changed, removed or moved, with each function's flow steps under it) beside the stub diff, with the flow steps as ghost lines and each decision as an anchored question (§7.5) on its element. The user edits stubs and steps directly (rename, move, change a signature, reword, reorder, add or delete a step), comments on lines through review threads (§7.7), and then **approves** or **sends it back**. Sending back returns the comments and edits as the tool result, and the agent's next revision is shown as a change against the last one.
3. **Contract:** on approval Coqo records what was agreed: the files, the public symbols and their signatures (from the approved stubs, with `coqo-syntax`) and the flow steps.
4. **Build:** the agent fills the skeleton in. If the shape turns out wrong, it proposes a new revision instead of drifting.
5. **Drift guard:** at `publish_walkthrough` and `request_review`, Coqo compares the change with the contract and lists public symbols and files that were added, are missing or were renamed, changed signatures, flow steps with no hunk, and hunks outside every element. Drift is **flagged, not blocked**, like the coverage guard (§7.6): the tool result leads with it so the agent explains it or proposes a revision, and the reader sees a banner. Coqo checks that the mapping is complete, not that the code does what a step says; that stays with the reader and the walkthrough.
6. **Walkthrough:** steps name the skeleton elements they implement (`skeleton_refs`), and the player groups them that way: "you approved this; here is how it was filled in".

**When** (`skeleton.mode`: a global default, a per-repo default in `.coqo/settings.toml`, and a per-session choice beside the composer's mode chip):
- `always`: every task that changes code starts with a skeleton, and the session starts in the provider's read-only mode where it has one.
- `auto` (default): the agent decides, following Coqo's guide (§7.4). It proposes one when the change adds files or public symbols, changes signatures, or adds logic with more than one decision point, and skips it for a bug fix, a one-branch edit, a rename or formatting. A skeleton of one step is an anchored question, so it asks that instead.
- `never`: `propose_skeleton` is not offered, so it costs no context.

**Session panel:** a stage line, Skeleton → Build → Walkthrough, with the skeleton's state, a link to its review, and the drift count once there is a change to compare.

**Providers:** ACP and terminal agents get the tool through MCP and a `coqo-skeleton` skill in Coqo's Claude Code plugin (`/coqo:skeleton`, beside `/coqo:walkthrough`). The native loop enforces the read-only phase itself.

**Storage:** in `coqo.db` with the session, every revision kept. Committing a skeleton next to its walkthrough (§7.6) can follow.

---

## 8. Data and storage

| Location | Contents |
|---|---|
| Daemon data (`~/Library/Application Support/Coqo`, `$XDG_DATA_HOME/coqo`) | `coqo.db` (workspaces and their projects, sessions, transcripts, **event log**, alerts, anchored questions and notes, skeletons, walkthroughs, review threads), per-workspace: `traces.db`, PR worktrees; installed mods in `mods/<id>/<version>-<sha12>/` (read-only) with the lock `mods/installed.toml`, and each mod's private data in `mod-data/<id>/` ([ADR 0033](adr/0033-mods-from-github.md)) |
| UI data (`<data dir>/ui/`) | Workbench layouts saved while `coqod` was unreachable, and a window's no-workspace layout; the daemon keeps each workspace's layout in its state (`ui.layout`, [ADR 0004](adr/0004-workbench-layout.md)) |
| Config (`$COQO_CONFIG_DIR`, else `~/.config/coqo`, macOS equivalent) | `settings.toml` (the user's settings, checked against the settings schema mods contribute; the daemon owns it, reloads it live and keeps its comments on writes, [ADR 0032](adr/0032-settings.md)), `keybindings.toml` (key overrides by command id, [ADR 0023](adr/0023-commands-and-keybindings.md)), themes |
| OS keychain | API keys, GitHub token |
| Repo `.coqo/` | `settings.toml` (workspace overrides of the user's settings, in the workspace's first root, incl. review skill; user-only settings such as the theme are ignored there), `run.toml` (launch/trace configs), `mods.toml` (recommended mods, pinned), `walkthroughs/` (only if opted in) |

## 9. Security and privacy
- No telemetry by default. Crash reports are opt-in.
- Secrets live only in the keychain and never in files or logs.
- **Daemon socket:** a user-only directory (verified, never adopted: no symlinks, no other owner, no group or other access), 0600 permissions, a peer credential check (`SO_PEERCRED` / `LOCAL_PEERCRED`), a per-start daemon token in `coqod.token` (0600) that every `coqo/hello` must present, and per-session MCP tokens scoped to a workspace (one-time, since agent adapters may put an MCP server's environment on a command line; [ADR 0012](adr/0012-editor-mcp-bridge.md)). The daemon token is an ephemeral capability in a user-only runtime file, not a long-lived secret, so it does not go in the keychain. See [ADR 0007](adr/0007-daemon-lifecycle.md).
- WASM mods are sandboxed through WASI capabilities. Process mods and core mods are trusted and declare their permissions.
- Every mod install shows its permission list and one of four trust levels: content only, prompts for Claude, sandboxed code, or runs programs as you.
- Installs from GitHub are pinned to a commit, never auto-updated, and re-approved when their permissions or trust level grow ([ADR 0033](adr/0033-mods-from-github.md)).
- Trace redaction runs before writing, and tracing is never used against production.
- Agent permission prompts are always shown, and auto-approve rules can be set per workspace. Rules are an allowlist kept by the daemon: they only match simple commands (word-wise prefix) and files inside the workspace's roots (glob), never shell operators, paths outside the workspace or programs such as `rm`, `sudo` or `curl`; everything else is asked ([chat design](design/chat.md#permissions)).
- Claude credential rules as in §7.3.

## 10. Repository structure (Cargo workspace)
```
crates/coqod/            daemon binary (incl. `coqod mcp` mode)
crates/coqo/             UI binary (GPUI) + `coqo` CLI subcommands
crates/client/           coqo-client: daemon socket paths + connect-or-spawn (UI, CLI, `coqod mcp`)
crates/kernel/           registry, services, supervisor, event log
crates/protocol/         Coqo Protocol types + schema (generates TS client)
crates/mod-api/          public mod API (coqo-mod-api) — only dependency allowed for mods
crates/mod-host-wasm/    wasmtime host + WIT world (wit/coqo-mod.wit)
crates/mod-host-proc/    Coqo Mod Protocol host
crates/ui/               coqo-ui facade over GPUI + gpui-component, declarative view renderer
crates/anchors/          anchor model + re-anchoring
crates/syntax/           coqo-syntax: languages, tree-sitter parsing, symbols, outline, folding (UI + daemon)
crates/storage/          SQLite + migrations
crates/lsp-client/ crates/dap-client/ crates/acp-host/ crates/mcp/
mods/<name>/             built-in core mods (§6)
skills/                  coqo-review-default, … (native-loop skills, #62)
integrations/claude-code/coqo-plugin/  Coqo's Claude Code plugin: coqo-walkthrough skill, /coqo:walkthrough
sdk/                     mod SDKs: Rust (wasm), TS (jco), process-mod templates
docs/                    PLAN.md, ADRs, protocol docs
```

---

## 11. Risks and spikes (first, in parallel with M1)

| # | Risk | Spike (time-boxed) | Fallback |
|---|---|---|---|
| **S1** | Java + Kotlin coexisting in one Maven project | JDT LS + kotlin-lsp side by side on a mixed multi-module Maven sample: no server conflicts, clean diagnostics after cross-language edits (compile-on-save), `mvn verify` passes, mixed-project doctor rules | Run kotlin-lsp only for `.kt` with JDT LS reading compiled classes; JetBrains "Java and Kotlin" LSP if it can run standalone |
| **S2** | Non-blocking JVM capture in mixed projects (one JDWP connection) | Run each of kotlin-lsp DAP (`start_debug_server`), java-debug, and a minimal JDI prototype against a Maven Spring Boot app with `.java` + `.kt` probes. Check: logpoint support, thread-only suspend, `.java` resolution in kotlin-lsp DAP, `.kt` resolution in java-debug, p50/p99 overhead per hit under load | `coqo-jvm-probe` (JDI/JDWP) |
| **S3** | Claude via ACP, policy-safe auth, daemon-owned sessions | Claude Agent ACP adapter: lifecycle, MCP injection, permissions, `available_commands`, `session/load` after daemon restart, billing-source detection; terminal mode + `--mcp-config` in a daemon PTY | Terminal mode only for subscription users. **Go** ([ADR 0005](adr/0005-claude-integration.md)) |
| **S4** | GPUI editor capability | gpui-component editor: 100k-line file, 500 decorations, anchored popovers, inline ghost values, side-by-side diff of a 5k-line file, multi-cursor feel | Own editor element on rope + tree-sitter |
| **S5** | Packaging and notifications | Notarized `.app` with daemon helper + notifications; AppImage/deb/rpm/Flatpak; Wayland + X11 | Per-platform workarounds |
| **S6** | UI↔daemon buffer replication | Op-log vs `yrs` CRDT: typing latency under 1 ms locally, concurrent agent + user edits, reattach with unsaved changes. **Decided ([ADR 0006](adr/0006-buffer-replication.md)): daemon-authoritative op-log with optimistic local apply** | `yrs` CRDT replicas behind the same `buffer/*` messages |
| **S7** | WASM mod API ergonomics | WIT world for commands + MCP tool + declarative view; author one mod in Rust and one in TS via `jco` | Process mods as the primary third-party tier |
| **S8** | The skeleton as plan mode on a provider's read-only mode (§7.11; runs before the skeleton epic, not with M1) | Claude Code over ACP in `plan` mode: can it call `propose_skeleton`, can the skeleton's approval and Claude Code's own plan exit be one approval, does the turn continue after `session/set_mode`. In `auto`: does Claude stop at the proposal instead of writing code (measured with `coqo-evals`). Signatures from tree-sitter queries for Rust, TypeScript, Java and Kotlin. Proposed stubs as a diff source or as unsaved buffers | The agent writes the stubs into the worktree in a normal mode and stops (guide and skill), and Coqo restores the files when the skeleton is sent back; where signatures can't be extracted the drift guard compares files and symbol names only |
| **S9** | Third-party skills' questions and findings shown the Coqo way, without changing the skills (#340) | Pin `AskUserQuestion`, elicitations and permission options with an explicit `path:line`, an option preview or the selection (100 % precise on the samples), and answer through the original request with the labels unchanged; detect `file:line` findings in a message and check them against the workspace (34/35 on samples, 0 false positives; a syntax-only detector shows 18 % bogus references on 156 real reviews); never pin plain-text endings automatically; no hook for ACP. **Go** ([ADR 0030](adr/0030-skill-questions-in-code.md)) | Everything else stays a chat card with "Pin to code". A session-only plugin `PreToolUse` hook is policy-safe, and is kept for terminal mode only |
| **S10** | Installing mods from GitHub repositories, like Claude Code plugin marketplaces, without running code before the user approves (#355) | Resolve `owner/repo[@ref][#subdir]` to a pinned SHA through the public API (2 calls), validate `coqo.mod.toml` with the real parser, and print a dry-run plan with the files, the permissions and what would run. Revised 2026-11-28: Claude plugins and marketplaces are not Coqo mods. **Go** ([ADR 0033](adr/0033-mods-from-github.md)): content mods first, WASM and process mods after their hosts, core mods never | Content mods only; process mods only as scripts pinned by the commit |

Other risks:
- **GPUI churn:** pin the exact `gpui-kit` release (which pins its `gpui-pre` snapshot), keep the `coqo-ui` facade, and keep the vendored `gpui-base` patch additive (ADR 0008).
- **The UI takes longer to build in Rust:** the protocol boundary allows an interim web UI if needed.
- **Anthropic billing/policy changes:** provider modes stay decoupled, and the billing source is shown in the UI.
- **ACP spec churn:** pin versions behind the `acp-host` adapter.
- **LSP memory across many workspaces:** idle suspension in the daemon.
- **Scope:** strict milestone gates, and dogfood early.

---

## 12. Roadmap

Each milestone becomes a GitHub milestone. Each bullet becomes an epic issue with sub-issues. The order is chosen so the team can **use Coqo to build Coqo** as early as possible.

**M0 · Spikes and foundations:** S1–S7. Cargo workspace, CI (fmt, clippy, test, `cargo-deny` licence/deps, API-boundary check, e2e harness), CONTRIBUTING (incl. Claude policy rules), CODE_OF_CONDUCT, ADR folder, Apache-2.0 headers.

**M1 · Daemon, protocol, UI shell:** `coqod` lifecycle (spawn/attach/idle/start-at-login), Coqo Protocol + event log + subscriptions, kernel registry/services/supervisor, workspace model, UI shell (window, workbench docking/tabs, command palette, keybindings, settings, themes), `coqo` CLI basics.

**M2 · Mod system:** `coqo-mod-api`, manifest, contribution points, WASM host (WIT), process-mod host, declarative view renderer, editor contribution API, permissions, mod manager UI, `coqo mod` scaffolding + hot reload, SDKs.

**M3 · Editing core:** editor mod, replicated buffers, file tree, fuzzy finder, project search, terminal (daemon PTY), `syntax` mod (tree-sitter, always on), LSP client framework + ⚡ bolt (per-language on-demand servers, memory display, agent wake policy), `lang-web`, formatting, diagnostics.

**M4 · Agent chat (→ dogfood alpha):** chat UI as tab, AgentProvider API, daemon-owned sessions + reattach, ACP host, `agent-claude` (ACP login mode, terminal mode, API key, Bedrock/Vertex), `agent-openai`, `agent-acp`, `agent-native` + MCP client + skills loader, @-mentions, permission prompts, billing-source indicator.

**M5 · Editor MCP, questions, workspaces, alerts:** `coqod mcp` + tool registry, `coqo-anchors`, anchored questions + panel, `set_status`, workspace rail, alert engine + daemon OS notifications + center, agents overview.

**M6 · JVM:** `lang-java`, `lang-kotlin`, `jvm-mixed` (routing + compile-on-save), `maven` mod + mixed-project doctor, `dap-jvm` interactive debugging (adapter chosen by S2).

**M7 · Git, diff, walkthrough:** git mod, worktrees, diff view, comment threads → agent, walkthrough schema + skill + coverage guard + player + storage modes; skeleton (§7.11, after spike S8): schema + `propose_skeleton` + skill, skeleton review, contract + drift guard, `skeleton.mode`.

**M8 · PR Inbox:** GitHub auth (`gh` reuse + device flow), daemon poller, inbox, PR view, threads, review submit, PR worktrees, PR walkthrough, review-skill drafts.

**M9 · Live trace:** trace service + storage + redaction, DAP client, probe planner (JVM + JS), `dap-jvm` (or `coqo-jvm-probe`), `dap-js`, scenario MCP tools, trace explorer, trace walkthrough with inline values, `trace_query`.

**M10 · Ship v1:** packaging (notarized universal dmg, AppImage/deb/rpm/Flatpak), auto-update (UI + daemon handoff; the GitHub Releases updater landed early with #349, ADR 0031), onboarding (detect `claude`/`gh`/JDK/Node/Maven), docs site, mod SDK docs + example mods, confirming the Claude integration with Anthropic.

Dependencies: M1 → M2 → M3 → M4 → M5 → (M6 ∥ M7) → M8. M9 depends on M5 and M6, and its JS part can start after M3. Spike S2 runs early because trace is the riskiest feature.

---

## 13. Decisions

| # | Decision | Status |
|---|---|---|
| 1 | Walkthrough storage: local by default; per-repo opt-in to commit to `.coqo/walkthroughs/` and/or attach to the PR | ✅ decided |
| 2 | Stack: full Rust (GPUI UI + Rust daemon); fallback for the UI is a web client on the same protocol | ✅ decided |
| 3 | Licence: Apache-2.0, open source | ✅ decided |
| 4 | GitHub auth: reuse `gh`, else device flow | ✅ decided |
| 5 | Name: Coqo | ✅ decided |
| 6 | Agents run in a background daemon that survives the UI and supports reattach | ✅ decided |
| 7 | Mixed Java+Kotlin: coexist and compile together; no cross-language code navigation required | ✅ decided |
| 8 | JVM debug/trace adapter: kotlin-lsp DAP vs java-debug vs `coqo-jvm-probe`, chosen per project behind one interface | decided by spike S2 |
| 9 | Claude: the Claude Agent ACP adapter on the user's installed `claude`, with terminal mode beside it ([ADR 0005](adr/0005-claude-integration.md)) | ✅ decided by spike S3 |

## 14. Design mocks

**Visual language, "zen":** a soft warm-charcoal ground (`#16171A`) with floating panels (`#1C1D21`/`#1F2024`, 16 px radius, 10 px gaps, soft shadows), raised buttons (subtle top highlight + drop shadow), and relaxed accents: sage `#9DBF9E` (primary/OK), sand `#E2B97F` (needs you), soft blue `#8FB3D9` (info/links), lavender and peach for syntax, and dusty rose `#D98E8E` (risk/removed). All text meets 4.5:1 contrast. Inter for UI, JetBrains Mono for code.

Screens:
1. Main workbench: workspace rail · file tree · code editor · chat tab in a split
2. Anchored question popover (single and multi-anchor) + questions panel
3. Walkthrough player over the diff (step list, what/why/risks popover, coverage warnings)
4. Native diff review with comment threads → "Send to agent"
5. PR Inbox list + PR review view (AI-draft comments, approve/request changes)
6. Agents overview + alert center + OS notification (including the "UI was closed" reattach state)
7. Live trace explorer + trace walkthrough with inline values
8. Mod manager + settings (providers/auth modes, review skill, daemon settings)
9. Skeleton review: element outline · stub diff with flow steps as ghost lines · decisions · approve / send back (to be drawn)
