# 33. Mods from GitHub repositories

- **Status:** Accepted, revised 2026-11-28 (see below)
- **Date:** 2026-11-28
- **Spike:** S10, issue #355. Prototype in [`docs/spikes/s10-mods-from-github/`](../spikes/s10-mods-from-github/), with recorded runs in its [`results.md`](../spikes/s10-mods-from-github/results.md). Builds on PLAN §5 (the mod system), §7.3 (the Claude policy), [ADR 0005](0005-claude-integration.md) (Claude through ACP), and the release format of #348 and #349 (GitHub Releases, `manifest.json`, minisign; [ADR 0031](0031-in-app-updater.md)).

## Revision 2026-11-28

The maintainer decided (verbatim): "claude plugins are not related to coqo, thats just for the harness. The mods are for the editor and everything around it."

What this removes from the original decision:
- **Claude Code plugins are not mods.** Coqo does not install them, and does not read Claude marketplaces (`marketplace.json`, `plugin.json`). Sources are `owner/repo[@ref][#subdir]` and Coqo marketplaces (`coqo.marketplace.toml`) only.
- **No "agent plugins" part of the content tier.** The `[[contributes.agent_plugins]]` point is dropped, and so is `agent-claude` collecting mod plugins per session.
- **No skills or slash commands bundled by mods for agents.** Skills, slash commands and agent plugins belong to the agent harness, not to Coqo mods.
- **Trust level 2, "prompts for Claude", is removed.** It existed only for agent plugins. There are now three levels (content only, sandboxed code, runs programs as you), and the old level 4 is level 3.
- The Claude-marketplace go/fallback rows, the `claude.*` id rule, the `CLAUDE PLUGIN` badge, and open questions 1 (plugins that run code) and 6 (`userConfig`) go with them.

What stays:
- Mods may still contribute **editor MCP tools**, served to agents through `coqod mcp` (PLAN §5's "MCP tools" contribution point), because those are editor capabilities. **Assumed, confirm with the maintainer.**
- Coqo's own built-in Claude Code plugin (`integrations/claude-code/coqo-plugin`, the walkthrough skill) is a separate integration and is not affected.
- Everything about resolving, pinning, the store, the lock, updates, WASM and process mods, and the dry-run plan.

**The first slice is now:** install a content mod (a settings section, keybindings, and themes once a loader exists) from GitHub, pinned to a commit.

The prototype's measurements of Claude marketplaces below are kept as a historical note only. Text marked "historical" or "revised" no longer describes the plan.

## Context

The maintainer wants users to add mods by pointing Coqo at a GitHub repository, the way Claude Code plugins are added (`/plugin marketplace add owner/repo`, then install by name). Claude Code is prior art for the flow only; see the revision above. PLAN §5 already says mods install "from a local folder, a git URL, or an OCI/HTTP tarball", but it never says how, or how safely.

**What the mod system can do today:**

- **Manifest and contribution points: done.** `coqo-mod-api` has the manifest (`coqo.mod.toml`, #31), 20 typed contribution points, and the `core | wasm | process` tiers.
- **Mod loading: compiled-in mods only.** The kernel's `ModManager` loads the `BuiltinMods` table in `crates/coqod/src/mods.rs` (hello, syntax, git, search, claude). Nothing loads a mod from disk.
- **Hosts and UI: not started.**
  - `crates/mod-host-wasm` and `crates/mod-host-proc` are placeholders (#33, #34, and spike S7 #14).
  - Permission grants (#36), the declarative view renderer (#35) and the mod manager (#37) are open.
  - #154, which exposes `mods/list`, `enable` and `disable` over the protocol, is open too.
- **Contribution points something reads:** `commands`, `keybindings`, `languages`, `agent_providers`, `mcp_tools` and `alert_sources`.
  - `settings` gets a reader with #26 (in progress).
  - `themes` has no reader: `crates/ui/src/theme.rs` is fixed.
  - The view points have no reader until #35 lands.
- **The mod audit (#353) found two gaps.** Most features were built straight into `coqod` and `coqo` rather than as mods, and `coqo-mod-api` has no UI contribution points.
- **The maintainer's direction on UI (2026-11-28, on #35 and #353):**
  - core mods contribute native GPUI views through a UI-side mod API;
  - third-party mods (WASM or process) contribute declarative view trees that Coqo renders natively;
  - both go through the same contribution points.
- **Coqo already ships a Claude Code plugin.** `crates/agent-claude/src/plugin.rs` writes `integrations/claude-code/coqo-plugin` to `<data>/claude-plugins/coqo` and passes it to every Claude session as `_meta.claudeCode.options.plugins: [{type: "local", path}]`. Claude Code loads it with `--plugin-dir`, and nothing is written to the user's Claude Code configuration.

The issue asks six questions: prior art, what a repository can hold per tier, addressing and pinning, trust, UX, and what in the missing mod system each part needs.

## What we tested

The prototype resolves `owner/repo[@ref][#subdir]` to a commit through the public GitHub API, with no authentication and the generic User-Agent `coqo-mod-installer`.

**Network use per plan:**
- two API calls: the ref to a SHA (`GET /repos/{o}/{r}/commits/{ref}` with `Accept: application/vnd.github.sha`), then the tree at that SHA;
- one raw fetch per text file it reads, by SHA.

**What it does with the files:**
- finds a `coqo.mod.toml` or a Coqo marketplace index (the prototype also read Claude `marketplace.json` and plugins; that part is dropped, see the revision);
- validates manifests with the real `coqo_mod_api::Manifest`;
- prints a dry-run plan: the files, the permissions, the contributions, a trust level, and everything that would run.

It never executes, builds or installs what it reads. 12 offline tests cover the fixtures under `fixtures/`.

**The runs (pinned to commits; 25 API calls in all). Historical: the Claude-marketplace rows are kept as measurements only, since Coqo no longer installs Claude plugins.**

| Target | Result |
|---|---|
| `anthropics/claude-plugins-official@0fbfda5` | 317 plugins. 55 live in the repository: 22 are skills, commands and agents only, and 33 have hooks or MCP or LSP servers. 262 live in other repositories (163 `url`, 98 `git-subdir`, 1 `command`), and 261 of those are pinned to a full SHA. |
| …`--pick frontend-design` | One skill, nothing runs: "prompts for Claude". 5 files, 31 KiB. |
| …`--pick hookify` | Four hooks (`python3 "${CLAUDE_PLUGIN_ROOT}/hooks/pretooluse.py"`, …), and four commands that pre-approve tools through `allowed-tools`, including `Write`: "runs programs as you". |
| …`--pick context7` | Only an HTTP MCP server, `https://mcp.context7.com/mcp`: Claude Code connects to it. |
| `anthropics/skills@e4bb5d9 --pick document-skills` | Every plugin is `"source": "./"` with its own `skills` list, so one install is the whole 11.8 MB repository. It ships 27 scripts, which the agent can run only through tools Claude Code asks about. |
| `happsie/coqo#integrations/claude-code/coqo-plugin` | Coqo's own plugin: one skill and one command, nothing runs. |
| `happsie/coqo#crates/mod-api/examples` | The complete example manifest, a process mod: nine permissions, a language server, and an `entry` that isn't in the repository (it would come from a per-platform release asset). |
| `happsie/coqo#mods/hello` | Refused: a core mod is compiled in. |

## Findings

### Prior art (question 1)

| | Claude Code plugins | Zed extensions | VS Code |
|---|---|---|---|
| **Index** | Any git repository with `.claude-plugin/marketplace.json` ([reference](https://code.claude.com/docs/en/plugins/marketplace-reference)) | One central repository, `zed-industries/extensions`: `extensions.toml` plus a git submodule per extension ([publishing](https://zed.dev/docs/extensions/developing-extensions)) | A central Marketplace (or Open VSX); a VSIX file by hand |
| **Addressing** | `owner/repo`, `owner/repo@ref` or `#ref`, a git URL, a local path; then `name@marketplace` | Install by id from the gallery | `publisher.name` |
| **Pinning** | An entry can pin `sha`: "a full 40-character lowercase commit SHA" | The submodule commit, and grammar `rev`s ("such as the SHA of a Git commit") | None: you get the Marketplace's current version |
| **Code** | Hooks, MCP and LSP servers run as the user. The docs warn that a plugin "can execute arbitrary code on your machine with your user privileges" ([security](https://code.claude.com/docs/en/plugins/security)) | WASM built **by Zed's CI** from the pinned source, run in Wasmtime. Process, download and npm access go through user-level `granted_extension_capabilities` ([capabilities](https://zed.dev/docs/extensions/capabilities)) | Extensions run in an extension host that "has the same permissions as VS Code itself" ([runtime security](https://code.visualstudio.com/docs/configure/extensions/extension-runtime-security)) |
| **Trust** | A fixed warning at install. Managed settings can allowlist marketplaces (`strictKnownMarketplaces`) | Review by the Zed team at PR time, plus a licence check | Publisher trust prompted on first install (1.97+), Marketplace signatures, malware scans |
| **Updates** | Auto-update is "off for every other marketplace" (third-party); a version is the manifest `version` or a 12-character SHA | A submodule bump and a version bump | Auto-update; off by default for a VSIX |
| **Team** | `extraKnownMarketplaces` and `enabledPlugins` in project settings, after the workspace trust dialog | – | `.vscode/extensions.json` recommendations, plus Workspace Trust |

**What Coqo copies:**
- **Claude Code's addressing and marketplace shape:** an index file in any repository, entries with relative or pinned external sources, and install by name.
- **Claude Code's update default:** no auto-update for third-party sources.
- **Zed's rule that what is installed is pinned to a commit.**
- **VS Code's recommendations file**, with the rule that recommending never installs.

**What Coqo does not copy:**
- **Zed's central build farm.** Coqo has no registry or CI to build third-party WASM.
- **VS Code's trust-the-publisher model.** There is no Coqo marketplace to verify publishers.

**Claude Code marketplaces: not read (revised 2026-11-28).**
- Historical finding: the prototype could read them (22 of the official marketplace's 55 in-repository plugins are prompts only, and 33 have hooks or MCP or LSP servers).
- The maintainer decided that Claude plugins are for the harness, not for Coqo. Coqo does not install them, read `marketplace.json` or `plugin.json`, or hand them to Claude sessions.

### What a repository can hold, per tier (question 2)

| Tier | What the repository holds | What runs | UI it can contribute |
|---|---|---|---|
| **Content** (new, `tier = "content"`) | A manifest and data files: themes, keybindings, a settings schema, languages (without servers), static views | Nothing | **A settings section** (the `settings` schema, rendered by #26); **static declarative views** (a view tree file: markdown, lists, links, and buttons that run already-registered commands) in the sidebar, panel or a tab, through #35's renderer; themes, once a theme loader exists; keybindings |
| **WASM** | A prebuilt component, either committed at the pinned SHA or attached to a GitHub Release with its sha256 in `coqo-mod.json` | The component in the WASM host, limited by WASI capabilities from the granted permissions (#33, #36) | **Declarative view trees** (#35) in every view location, status items, commands, settings, MCP tools, alert sources, and editor contributions as #353 defines them |
| **Process** | Per-platform executables from release assets (`<id>-<version>-<target>.tar.gz` in `coqo-mod.json`), or a script run by an interpreter on `PATH` | A supervised process with the user's rights (#34) | Same as WASM: declarative view trees over the Coqo Mod Protocol |
| **Core** | Not installable: core mods are Rust crates compiled into Coqo | – | Native GPUI views through the UI-side mod API (#353 designs it) |

**WASM: prebuilt only, never built on install.**
- Building a Rust or TS component runs the author's code before the user has approved anything: `build.rs`, proc macros, npm scripts.
- It also needs toolchains the user may not have. Zed avoids the problem only because its CI does the building.
- The WASM sandbox, not the source, is the security boundary, so a prebuilt component is acceptable.
- Building from source stays a developer action (`coqo mod dev`, #38) that the user starts.

**Process mods** are the trust problem the issue names.
- A binary per platform can't be reviewed.
- A script with an interpreter on `PATH` is pinned by the SHA, but runs as the user.
- An entry such as `npx` downloads more code that Coqo can't pin, and the plan says so.

**Content mods are the first win.** No code runs, so most of the security work goes away, and they need no mod host at all.

### Addressing, pinning and updates (question 3)

- **The source.** `owner/repo[@ref][#subdir]`, as the issue proposes, with an optional `github:` prefix, or a github.com URL (`…/tree/<ref>/<subdir>`).
  - `@` takes a tag, a branch or a commit.
  - `#` takes a folder.
  - Claude Code's `owner/repo#ref` therefore means something else in Coqo. When `#x` isn't a folder, the plan suggests `@x`; the prototype does this.
- **Marketplaces.** The source is the marketplace repository, and the user picks entries in the dialog, or with `--pick <id>` in the CLI.
- **Pinning.** On install the ref becomes a 40-character SHA, and the files come from that SHA only: `GET /repos/{o}/{r}/tarball/{sha}`, one more API call.
  - The extractor keeps only the mod's folder. It refuses absolute paths, `..`, symlinks, hard links and devices, and caps the unpacked size and the number of files.
  - A content mod's files lose their executable bit.
  - The repository's numeric id (from `GET /repos/{o}/{r}`) goes in the lock. If `owner/repo` later resolves to a different repository, Coqo refuses the update. This covers renames, transfers and repo-jacking.
- **The store.**
  - Mods go in `<data>/mods/<id>/<version>-<sha12>/`, read-only. `<data>` is the daemon data directory (PLAN §8).
  - A mod's private data goes in `<data>/mod-data/<id>/`, which is `$data` in its permissions and is kept across updates.
  - `<data>/mods/installed.toml` is the lock. Per mod it records the source as typed, the resolved SHA, the repository id, the version, a sha256 over the installed files, the trust level and permissions the user approved, and whether the mod is enabled.
  - One previous version is kept for rollback, as #349 does for Coqo itself.
- **Updates.** They follow what the user pinned:
  - **A commit:** never updated.
  - **A tag:** a newer semver tag, found through `releases/latest` or else the tag list.
  - **A branch, or no ref:** a new head commit, checked with `commits/{branch}`.
  - Checks reuse #349's client: no auth, a generic User-Agent, `ETag` and `If-None-Match`, at most once a day per mod, and a setting to turn checks off.
  - An update is **offered, never applied** without approval. The dialog shows the permission and trust diff.
  - A tag that now points at a different commit gets a loud warning and counts as a new version.
  - A downgrade is refused unless asked for explicitly.

### Trust and security (question 4)

**Trust levels.** The install dialog shows one of three, each computed from the plan. (Revised: the former level 2, "prompts for Claude", existed only for agent plugins and is removed; the former level 4 is now level 3.)

1. **Content only:** nothing runs.
2. **Sandboxed code:** WASM, limited to the listed permissions.
3. **Runs programs as you:** process mods and language servers.

**What a malicious repository could do, and what stops it:**

| Tier | Attack | Defence |
|---|---|---|
| All | A path-traversal or symlink archive, or a zip bomb | The extractor's rules and caps; files only from the pinned SHA |
| All | A later commit turns malicious, or the repository is renamed or transferred to someone else | SHA pinning, no auto-update, the permission and trust diff on update, the moved-tag warning, the repository-id check |
| All | Squatting an id such as `coqo.git` | Installed mods may not use `coqo.*` |
| Content | Keybindings that rebind keys to destructive commands | Bindings may only name registered commands. The plan lists them, and the dialog flags any that override a built-in binding. |
| Content | A settings schema that shadows another mod's keys | A content mod's setting keys must start with its id's last segment, which #26's validation enforces |
| Content | A static view whose buttons run commands | Buttons run only registered commands, and only when clicked; they never run on render |
| WASM | A sandbox escape, or exhausting resources | wasmtime kept current, plus fuel or epoch limits and memory caps (#33) |
| WASM | Misusing a granted permission | Least privilege, an enforced network allowlist, revocable grants (#36) |
| Process | Anything the user can do | Explicit consent at level 3, re-consent when anything changes, no auto-update, commands shown verbatim, a relative `entry` resolved only inside the mod's folder |

**Signatures are optional, and only where they add something.**
- **Files from a pinned commit:** a signature adds nothing. The commit SHA already identifies the content, TLS to github.com carries it, and a key kept in the same repository proves nothing on first install.
- **Release assets (WASM and process binaries):** the commit doesn't pin these, so signatures help here.
  - The release must have a `coqo-mod.json` shaped like #348's `manifest.json`: `{ "id", "version", "assets": { "<target>" | "wasm": { "file", "sha256", "size" } } }`. Coqo checks sha256 and size.
  - If the mod's manifest declares `[signing] minisign = "<public key>"`, the release must also carry `coqo-mod.json.minisig`. Coqo verifies it with #349's minisign code and **pins the key on first install** (trust on first use). Updates signed with a different key, or not signed at all, are refused.
  - GitHub artifact attestations (Sigstore), which tie an asset to the workflow and commit that built it, are a possible later addition.

**The Claude policy (PLAN §7.3) holds, and installed mods do not touch Claude Code at all (revised).**
- Installed mods never add anything to Claude sessions. Coqo's own plugin (`coqo-plugin`) is a separate integration and is unchanged.
- The one path from a mod to an agent is an editor MCP tool served through `coqod mcp` (assumed, confirm with the maintainer).

### UX (question 5)

**Entry points:**
- the palette command "Mods: Add from GitHub…", which takes the source;
- the board 8 "Install mod" button, with a "From GitHub" choice (#37);
- the CLI: `coqo mod add <source> [--pick <id>]`, `list`, `enable`, `disable`, `remove`, `outdated` and `update`.

**The install dialog** shows the plan, in the order the prototype prints it:
- what it is (the tier badge, with CONTENT next to board 8's CORE, WASM and PROCESS);
- the source and short SHA, linked to the commit;
- the trust level as one sentence;
- the permission chips, in board 8 style;
- what it contributes;
- **what would run**, with each command verbatim;
- the file count and size, the licence, and how it updates.

For a marketplace, the dialog lists the entries with their trust badges first.

**Install** is enabled only for a plan with no errors. A level-3 plan needs a second, explicit confirmation. Nothing is downloaded beyond the plan's text files until Install is clicked, and nothing activates until the files are verified.

**After install:**
- Enabling and disabling is live (`ModManager::enable` and `disable`) and is saved in the lock.
- Removing deletes the folder and the lock entry, and keeps `$data` unless the user asks for it to go too.

**Global and workspace:**
- Installs are global.
- A repository can recommend mods in `.coqo/mods.toml`: each entry has a source and a SHA.
  - Opening the workspace shows a quiet notice that opens the same dialog.
  - Recommending never installs and never enables anything.
- Whether a mod can be enabled per workspace is an open question.

### What each part needs from the missing mod system (question 6)

| Part | Needs |
|---|---|
| Resolve, fetch, verify, store, lock; CLI | Nothing from #353. Plus the new mod store, and #154 for the `mods/*` routes. |
| Content mods: keybindings, languages | The content tier in `coqo-mod-api`, and loading installed mods in the daemon. The readers already exist. |
| Content mods: settings section (UI) | #26's settings UI reading the `settings` point |
| Static views (UI) | #35's view tree schema and renderer (unblocked: #31 and #24 are closed) |
| Themes | A theme loader reading the `themes` point (`crates/ui/src/theme.rs` is fixed) |
| Install dialog, mod manager | A design for the dialog; #37, which needs #35, #36 and #26 |
| WASM from GitHub | #14 (S7, the WIT world), #33, #36 |
| Process mods from GitHub | #34, #36 |
| Anything that adds UI beyond static views (panes, gutter markers, popovers, live views) | #353's list of UI contribution points, and #35's event round trip. Core mods' native UI API is also #353's to design. |

S10 does not wait for #353. The audit moves built-in features into mods and designs the UI-side API. The install pipeline and content mods need neither, but **which view locations and editor contributions third-party mods get beyond static views** comes from #353's gap list.

## Decision

**Go.** Coqo installs mods from GitHub repositories, pinned to a commit, after a dry-run plan the user approves.

**Recommended design:**
- **Sources:** `owner/repo[@ref][#subdir]`, or a Coqo marketplace (`coqo.marketplace.toml`). No Claude plugins or marketplaces.
- **Install:** a single daemon-side mod store resolves the ref to a SHA (two to four unauthenticated GitHub API calls) and builds a plan without running anything.
- **Storage:** the mod is extracted read-only into `<data>/mods/<id>/<version>-<sha12>/` and recorded in `installed.toml`.
- **Updates:** offered, never applied silently.
- **The new content tier:** no code; it carries data only (settings, keybindings, themes, languages without servers, static views).
- **Agent-facing:** a mod may contribute editor MCP tools, served through `coqod mcp` (assumed, confirm with the maintainer). No skills, slash commands or agent plugins.

### Go / fallback per tier

| Tier | Decision | UI it can contribute | Fallback |
|---|---|---|---|
| **Content** | **Go, the first slice** | Settings sections (#26), static declarative views (#35), keybindings, themes (with a theme loader) | If #35 slips: settings sections and keybindings only |
| **WASM** | **Go after #14, #33 and #36**: prebuilt components only, committed or as release assets with sha256, optional minisign with the key pinned on first use. Never built on install. | Declarative view trees in every view location, status items, settings, plus what #353 adds | If S7 decides process mods are the primary third-party tier: WASM installs wait, and process mods take that place |
| **Process** | **Go after #34 and #36**, at level 3: per-platform release assets with sha256, optional minisign, no auto-update, re-consent on change | Declarative view trees, as WASM | Only scripts run by an interpreter on `PATH` (pinned by the commit), no prebuilt binaries |
| **Core** | **No-go**: compiled into Coqo | Native GPUI views through #353's UI-side API, in the repository only | – |
| **Claude plugins and marketplaces** | **No-go (2026-11-28)**: they are for the harness, not Coqo mods | – | – |

### The first slice

> Install a content mod from GitHub, pinned to a commit, from the CLI and the palette. Its keybindings work in the keymap, its themes apply once a theme loader exists, and **its settings appear as a section in Settings**. A static sidebar view follows as soon as #35's renderer exists.

**Why settings for the UI piece:**
- The smallest safe UI contribution is a settings section through #26's schema point. It is data on both sides: a manifest table in, a native form out.
- A static declarative sidebar view is the first one users will notice. It depends on #35, which is unblocked but not started, so it is the next task rather than part of the slice.

**In order:**
1. **#360, the content tier in `coqo-mod-api`.** The tier, its validation rules, and `docs/mods/manifest.md`.
2. **#361, the mod store.** Resolve, plan, fetch the tarball at the SHA, extract safely, the lock, and the repository-id check.
3. **#362, installed mods in the daemon.** Load them at start, `mods/plan`, `mods/install` and `mods/remove` next to #154's `list`, `enable` and `disable`, and the `coqo mod` CLI.
4. **#365, installed mods' settings in Settings.**
5. **#366, "Add from GitHub" and the install dialog** (needs a design).

**Then:**
- #367: static views from content mods
- #368: update checks
- #369: Coqo marketplaces
- #370: WASM and process mods from release assets
- #371: workspace recommendations

### Formats (proposed; the tasks finalise them)

```toml
# coqo.mod.toml of a content mod
id = "acme.team-kit"
version = "1.2.0"
engine = "^0.1"
tier = "content"                   # no entry, activation, provides/consumes or permissions

[[contributes.settings]]           # appears as a section in Settings (#26)
key = "team-kit.ticket-prefix"
type = "string"
default = "ACME"

[[contributes.views.sidebar]]      # a static view tree file (#35's schema), after #35
id = "team-kit.handbook"
title = "Handbook"
source = "views/handbook.toml"
```

```toml
# coqo.marketplace.toml at a repository's root
name = "acme-mods"
[[mods]]
id = "acme.night"                  # must equal the mod's own manifest id
source = "mods/night"              # a folder in this repository
[[mods]]
id = "acme.lint"
source = { github = "acme/lint-mod", ref = "v1.0.0", sha = "<40 hex>" }
```

```toml
# <data>/mods/installed.toml (written by Coqo)
[[mod]]
id = "acme.team-kit"
version = "1.2.0"
source = "acme/coqo-mods@v1.2.0#team-kit"
sha = "<40 hex>"
repository_id = 123456789
files_sha256 = "<hex>"
trust = "content"
granted = { }                      # the permissions approved (#36)
enabled = true
```

## Open questions for the maintainer

1. ~~Plugins that run code in the first slice.~~ **Answered 2026-11-28:** no Claude plugins at all.
2. **The install dialog's design.** Board 8 has only the "Install mod" button. Should it be a modal, a sheet in Settings, or a tab? Does CONTENT get its own badge?
3. **Per-workspace enablement.** Should an installed mod be enabled per workspace, as Claude Code's project scope does, or only globally? And should `.coqo/mods.toml` recommendations wait for a workspace-trust prompt, which Coqo doesn't have yet?
4. **Private repositories and the rate limit.** Should installs and update checks use the user's GitHub token from the keychain (the PR inbox's) when one exists? That raises the limit from 60 to 5,000 an hour and reaches private repositories, but it sends a credential with every install request.
5. **Auto-update.** Keep it off for everything, or allow it as an opt-in per mod for content mods whose update doesn't change the trust level or the permissions?
6. ~~Claude plugin `userConfig`.~~ **Dropped 2026-11-28** with Claude plugins.
7. **Mods contributing editor MCP tools (assumed, confirm with the maintainer).** The ADR assumes mods keep PLAN §5's "MCP tools" contribution point, served through `coqod mcp`.
8. **Size caps.** 64 MiB unpacked and 20,000 files per mod are the proposed defaults. anthropics/skills' plugins are the whole 11.8 MB repository.

## Consequences

- PLAN §5 gains the content tier and the install-from-GitHub flow, §8 gains the mod store paths, and §11 gains S10.
- `coqo-mod-api` gains a fourth tier. A static view's `source` file is added with #35's schema.
- `agent-claude` is unchanged: `coqo-plugin` stays its only plugin.
- The mod store and the updater (#349) share the GitHub Releases client (ETag polling, generic User-Agent) and minisign verification, so they should live in one place rather than be written twice.
- Third-party UI stays declarative. Which view locations and editor contributions are open to it follows #353.
- **Follow-up tasks under epic #30:**
  - #360: the content tier
  - #361: the mod store
  - #362: installed mods in the daemon, and the CLI
  - #364: agent plugins in Claude sessions (closed as not planned, 2026-11-28)
  - #365: settings sections
  - #366: "Add from GitHub" and the install dialog (`human-needed`)
  - #367: static views
  - #368: update checks
  - #369: Coqo marketplaces
  - #370: WASM and process mods from release assets
  - #371: workspace recommendations (`human-needed`)
