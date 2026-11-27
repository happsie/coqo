# `coqo.mod.toml`: the mod manifest

Every mod ships a `coqo.mod.toml` next to its code, whatever its tier (PLAN §5). It says what the mod is, which Coqo versions it supports, how to run it, when to activate it, which services it provides and consumes, what it may touch, and what it contributes before it runs.

The parser and validator are `coqo_mod_api::Manifest::from_toml_str`. A complete example that uses every field is [`crates/mod-api/examples/coqo.mod.toml`](../../crates/mod-api/examples/coqo.mod.toml); the tests parse it, so it always matches the code. The smallest useful manifest is [`mods/hello/coqo.mod.toml`](../../mods/hello/coqo.mod.toml).

## Errors

Validation reports every problem at once, each with its line, column and field path:

```text
6:9: entry: core mods are linked into Coqo, so they take no `entry`; remove it
19:6: contributes.commands[1].id: duplicate command `bad.run` (also at contributes.commands[0])
23:7: contributes.keybindings[0].key: unknown modifier `ctl` in `ctl+r`; expected one of ctrl, alt, shift, cmd, mod
```

TOML syntax and type errors (a wrong type, an unknown field, an invalid id) stop parsing, so they are reported alone, with their position.

## Ids

Mod ids, service ids and contribution ids are dot-separated segments of `[a-z0-9][a-z0-9_-]*`: `coqo.git`, `maven.run-goal`, `views.sidebar`. Service references add a version: `coqo.git@1` or `coqo.git@1.2`.

## Top-level fields

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | Mod id, also the provider id of everything it registers |
| `name` | | Display name (defaults to the id) |
| `version` | yes | The mod's version, semver |
| `description`, `authors`, `license`, `repository` | | Shown in the mod manager |
| `engine` | yes | Semver requirement on the mod API version (`coqo_mod_api::API_VERSION`), e.g. `^0.3` |
| `tier` | yes | `core`, `wasm` or `process` |
| `entry` | wasm, process | wasm: component path ending in `.wasm`. process: program to run (relative path or a name on `PATH`). core: not allowed |
| `args` | | Arguments for a process mod's `entry` |
| `activation` | | Activation events, below |
| `provides` | | Services provided, at exact versions: `["acme.maven@1.2"]` |
| `consumes` | | Services consumed, at minimum versions: `"coqo.git@1"` or `{ service = "coqo.git@1", optional = true }` |
| `[permissions]` | | Below |
| `[contributes]` | | Below |

Paths in the manifest (`entry`, grammars, themes) are relative to the mod's folder, use `/`, and may not use `..`.

## Activation events

| Event | Fires when |
|---|---|
| `startup` | Coqo starts (use sparingly) |
| `command:<id>` | the command runs |
| `language:<id>` | a file of that language opens |
| `view:<id>` | the view is shown |
| `workspace_contains:<glob>` | the workspace has a matching file, e.g. `**/pom.xml` |
| `service:<id>` | another mod consumes the service |
| `mcp_tool:<name>` | an agent calls the MCP tool |
| `agent_provider:<id>` | a chat session starts with the agent provider |

## Permissions

```toml
[permissions]
fs.read = ["$workspace"]            # $workspace, $mod, $data, $cache, $home, $tmp, or an absolute path
fs.write = ["$workspace/target"]
network = ["api.github.com", "*.example.com:443"]   # host, *.domain, host:port, or *
process = ["git", "mvn"]            # programs the mod may spawn, or *
secrets = ["github.token"]          # keychain secrets the mod may read
```

The install prompt lists these. The WASM host enforces them through WASI capabilities; process and core mods are trusted to keep to them (PLAN §9).

## Contribution points

Static contributions go under `[contributes]`. A point's id is its key path there, so `[[contributes.views.sidebar]]` goes to the point `views.sidebar`. Mods can also contribute at runtime through `ModContext`, with the same value types (`coqo_mod_api::contrib`) and point handles (`coqo_mod_api::points`).

| Key | Value type | Fields (required in bold) |
|---|---|---|
| `commands` | `Command` | **`id`**, **`title`**, `category`, `icon`, `when` |
| `keybindings` | `Keybinding` | **`command`**, **`key`**, `mac`, `when`, `args` |
| `menus` | `MenuItem` | **`menu`**, **`command`**, `group`, `order`, `when` |
| `settings` | `Setting` | **`key`**, **`description`**, **`type`**, `title`, `default`, `values`, `minimum`, `maximum` |
| `views.sidebar`, `views.panel`, `views.tab`, `views.rail` | `View` | **`id`**, **`title`**, `icon`, `when`, `order` |
| `views.status` | `StatusItem` | **`id`**, **`text`**, `tooltip`, `command`, `alignment` (`left`/`right`), `priority`, `when` |
| `languages` | `Language` | **`id`**, **`name`**, `extensions` (`.java`), `filenames` (`pom.xml`), `grammar`, `line_comment` |
| `lsp_servers` | `LspServer` | **`id`**, **`languages`**, **`command`**, `name`, `args`, `root_markers`, `initialization_options` |
| `debug_adapters` | `DebugAdapter` | **`id`**, **`name`**, `languages`, `command`, `args` |
| `mcp_tools` | `McpTool` | **`name`** (`[a-z0-9_-]`, at most 64, no dots), **`description`**, **`input_schema`** (JSON Schema object) |
| `agent_providers` | `AgentProviderInfo` | **`id`**, **`name`**, **`kind`** (`acp`/`terminal`/`native`), `description` |
| `slash_commands` | `SlashCommand` | **`name`** (without `/`), **`description`**, `argument_hint` |
| `context_providers` | `ContextProvider` | **`id`** (the `@`-mention keyword), **`name`**, `description` |
| `alert_sources` | `AlertSource` | **`id`**, **`name`**, `description` |
| `themes` | `Theme` | **`id`**, **`name`**, **`appearance`** (`light`/`dark`), **`path`** |

Some details:

- **Keys** are chords separated by spaces, each chord being modifiers and a key joined by `+`: `ctrl+shift+p`, `mod+k mod+s`. Modifiers are `ctrl`, `alt`, `shift`, `cmd` and `mod` (`cmd` on macOS, `ctrl` elsewhere). Keys are lowercase: one character, `f1`…`f24`, or `enter`, `escape`, `tab`, `space`, `backspace`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`, `right`.
- **Menus** are `command_palette`, `editor.context`, `editor.title`, `explorer.context`, `tab.context` and `view.title`.
- **Setting types** are `boolean`, `string`, `integer`, `number`, `enum` (needs `values`) and `string_list`. `default` must match the type, and `minimum` / `maximum` apply only to numbers.
- **View ids** share one namespace across all locations, so they can be used in `view:<id>` activation events.
- **`when`** clauses are context expressions such as `editor.focused`, evaluated by the workbench. v1 stores them as text.
- Keybindings and menu items have no id of their own. When the host registers them from the manifest, it names them `<mod id>.keybinding-<n>` and `<mod id>.menu-<n>`.

Two points have no manifest key, because they hold runtime handles for in-process mods: `command_handlers` (`Arc<dyn CommandHandler>`, filled by `ModContext::register_command`) `agent_provider_impls` (`Arc<dyn AgentProvider>`, owned by `coqo-agents` and filled together with `agent_providers` by `coqo_agents::register_provider`) and any other point a crate defines with its own `ContributionPoint` marker type.

## Lifecycle

1. The host parses and validates the manifest, checks `engine`, and registers `[contributes]` (`Contributes::register`), so commands and views appear before the mod runs.
2. When an activation event fires, the host calls `Mod::activate` with a `ModContext`. The mod provides services and registers handlers and runtime contributions.
3. On disable, unload or reload, the host calls `Mod::deactivate` and then withdraws everything the mod registered.

In the daemon, `coqo_kernel::mods::ModManager` does this over the kernel's service and contribution registries (`KernelHost`):

- Built-in core mods are listed in `crates/coqod/src/mods.rs` (`BuiltinMods::new().with(MANIFEST, MyMod::new())`) and provide services as built-ins, so an installed mod providing the same service replaces them. A built-in must have `tier = "core"`.
- `startup` mods activate when the daemon starts; `command:<id>` mods activate the first time the command runs. The other events are not fired yet.
- A mod whose manifest is rejected, or whose `activate` fails, panics or takes longer than 10 seconds, is marked failed and everything it registered is withdrawn. Other mods keep running.
- After a mod is disabled, its `ModContext` is refused, so it can't register anything again until it is enabled.
- A crate that owns a contribution point declares it before mods load, with `BuiltinMods::with_point::<P>()`.
