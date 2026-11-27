# 14. Editor MCP tools: core tools, editor context, and tools from mods

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #70 (builds on ADR 0012)

## Context

ADR 0012 gave every agent session a `coqo-editor` MCP server (`coqod mcp`) with a tool registry and two tools. PLAN §7.4 wants the registry filled: tools that show the agent what the user sees (`editor_context`), that show the user a place (`editor_reveal`), real IDE intelligence (`lsp_*`), and tools that mods contribute, so that "a new mod automatically gives agents new editor powers". Open points:

1. Where the user's editor state (open tabs, selections, visible lines) lives, since it exists only in the UI, and the bridge must work with no UI attached.
2. How a mod's tool reaches the bridge, what agents call it, and who checks its arguments.
3. Which tools are core (in the bridge) and which come from mods, so later work (#73 questions, #75 status, #204 LSP) knows where to put its tools.

## Decision

**Core tools run in the bridge**, over daemon methods on the session-scoped connection (ADR 0012), so the route policy covers them:

| Tool | Daemon methods | Read-only |
|---|---|---|
| `workspace_info` | `workspace/get` | yes |
| `read_file` | `fs/read` | yes |
| `list_files` (one folder, `depth` levels, ignored entries left out by default) | `fs/list` | yes |
| `find_files` (quick-open style: file name, then path, then letters in order) | `fs/list` | yes |
| `file_outline` (tree-sitter symbols as a tree, unsaved edits included) | `buffer/open`, `syntax/outline`, `buffer/close` | yes |
| `editor_context` | `editor/getContext` | yes |
| `editor_reveal` | `editor/reveal` | no |

Lines and columns in tool arguments and results are **1-based**, as editors and `cat -n` show them; the protocol stays 0-based. File results give absolute paths, which Claude Code's own tools use. There is no write tool: agents edit files with their own tools, and Coqo sees those edits through the ACP file system and the watcher.

**Editor context lives in the daemon, published by the UI.** Each UI window sends `editor/context` (a notification, daemon token only) for the workspace it shows: its file tabs in order, the active one, each tab's selections (0-based line and character, the selected text up to 16 KiB) and visible lines, and whether it has focus. It publishes on tab, selection, scroll, focus and workspace changes, at most every 120 ms, only when something changed, and again after a reconnect; it sends `context: null` when it closes or switches workspace. The daemon keeps the latest per connection and window and drops a window when its connection closes. `editor/getContext` (session-callable for its own workspace) returns the windows, focused first. With no UI, the list is empty and `editor_context` says so.

**Reveals go through a daemon topic.** `editor/reveal` (session-callable for its own workspace) checks that the path is a file in the workspace and publishes a `RevealEvent` (with the asking session) on `editor/reveals`, a live topic only the daemon token may follow. The UI follows it once per connection and gives each request to the active window if it shows the workspace, else the first window that does; that window opens or activates the file's tab and selects the range (clamped to the text), which scrolls it into view. The result's `delivered` says whether any window shows the workspace, so the agent knows whether anyone saw it.

**Mods contribute tools in the daemon.** `coqo-mod-api` has the point `mcp_tools` (the `McpTool` definition: name, title, description, input schema, read-only) and a new point `mcp_tool_handlers` (`Arc<dyn McpToolHandler>`, keyed by tool name), registered together by `ModContext::register_mcp_tool`. A handler gets an `McpToolCall { workspace, session, arguments }` and returns an `McpToolOutput { text, structured }` or a `ModError`, which the agent sees as a failed result. A tool declared only in the manifest activates its mod on its first call (`mcp_tool:<name>`, like commands). WASM and process hosts (#33, #34) register a forwarding handler, as for commands.

The daemon serves them as `mcp/tools` (session-callable for its own workspace), `mcp/call` (for its own workspace **and** session, which the handler is told) and `mcp/toolsChanged` (a live topic; the daemon compares the tool list on every contribution change and publishes only real changes). The bridge lists them before it answers the agent's first `tools/list`, registers a proxy per tool, and re-lists on every change; each change is one registry update and one `notifications/tools/list_changed`.

**Names.** A built-in mod's tool keeps its own name, so PLAN §7.4's names (`maven_test`, `ask_question`) hold. An installed mod's tool is prefixed with its mod id, dots as `_` (`acme.maven`'s `maven_test` is `acme_maven_maven_test`), so it cannot shadow a built-in, a core tool or another mod's tool. Names over 64 characters are cut and end in a hash. A mod tool never replaces a core tool of the same name; it is skipped with a warning.

**Arguments are checked against the schema before any tool runs**, in the bridge for every tool and again in the daemon for mod tools. The validator is a small one in `coqo-mcp` (`schema::validate`), no new dependency: `type`, `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`, length, size and numeric bounds, `allOf`/`anyOf`/`oneOf`/`not`. Keywords it does not know (`$ref`, `pattern`, `format`) are ignored, so a schema is at worst checked less strictly, never wrongly refused. A mismatch is a tool error (`isError`, "invalid arguments: `line` must be at least 1"), which MCP clients show the model so it can correct the call.

**Where new tools go.** A tool that only needs session-callable daemon state is a bridge core tool in `crates/mcp/src/tools` (add its daemon route to `SESSION_ROUTES` and `probes()` in `crates/coqod/src/routes/tests.rs`). A tool owned by a feature that is a mod in the daemon (questions and annotations #73, status #75, `lsp_*` #204, maven, trace) registers through `register_mcp_tool` from that mod, which needs no new route. A tool that blocks (`ask_question`) can be either; as a bridge tool it gets MCP progress through `ToolContext::progress`, which `mcp/call` does not carry yet.

## Consequences

- Agents see seven core tools plus every active mod's tools, all scoped by the route policy. A UI is optional: only `editor_context` and `editor_reveal` need one, and both say when there is none.
- The session-token allowlist grows by five routes: `editor/getContext`, `editor/reveal`, `mcp/tools`, `mcp/call`, `mcp/toolsChanged`.
- The UI publishes a little traffic on every selection change, throttled to 120 ms; nothing when idle.
- `lsp_*` tools wait for ⚡ Bolt (#48) and are #204. A workspace-wide `find_symbol` from the syntax layer would open a buffer per file through the bridge, so it needs a daemon-side search or index first (#205).
- WASM mods (#33) can contribute tools once their host forwards `mcp_tool_handlers`; nothing else changes.
- Mod tool calls get no MCP progress or cancellation yet; a long mod tool should be a bridge tool or wait for a follow-up that streams progress over `mcp/call`.
- `editor_context` reports visible rows, which equal buffer lines while file tabs do not soft-wrap.
