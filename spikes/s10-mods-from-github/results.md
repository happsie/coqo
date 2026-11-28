# Spike S10: recorded runs

Output of `spikes/s10-mods-from-github` on 2026-11-28, pinned to the commits below so the runs can be repeated. Each GitHub run costs two api.github.com calls (resolve the ref, list the tree) plus one raw fetch per file read. Nothing was downloaded beyond the files read, and nothing was executed.

## The official Claude Code marketplace, listed

```
$ cargo run -- plan anthropics/claude-plugins-official@0fbfda5a91f4adc311daca25acc99132f40f0b6e
DRY RUN: nothing installed, nothing executed.
source      github.com/anthropics/claude-plugins-official @ 0fbfda5a91f4 (0fbfda5a91f4adc311daca25acc99132f40f0b6e)
found       Claude Code plugin marketplace `claude-plugins-official` (317 plugins)
lists
  - agent-sdk-dev              1 commands, 2 agents
  - asana                      1 commands
  - champion-kit               7 skills, 1 agents
  - clangd-lsp                 LSP
  - claude-code-setup          1 skills
  - claude-md-management       1 skills, 1 commands
  - claude-security            6 skills, 17 agents, HOOKS, 25 scripts
  - code-modernization         13 commands, 8 agents, HOOKS
  - code-review                1 commands
  - code-simplifier            1 agents
  - commit-commands            3 commands
  - context7                   MCP
  - csharp-lsp                 LSP
  - cwc-makers                 2 skills, 1 commands
  - discord                    2 skills, MCP
  - explanatory-output-style   HOOKS, 1 scripts
  - fakechat                   MCP
  - feature-dev                1 commands, 3 agents
  - firebase                   MCP
  - frontend-design            1 skills
  - github                     MCP
  - gitlab                     MCP
  - gopls-lsp                  LSP
  - hookify                    1 skills, 4 commands, 1 agents, HOOKS, 5 scripts
  - imessage                   2 skills, MCP
  - jdtls-lsp                  LSP
  - kotlin-lsp                 LSP
  - laravel-boost              MCP
  - learning-output-style      HOOKS, 1 scripts
  - linear                     MCP
  - lua-lsp                    LSP
  - math-olympiad              1 skills, 2 scripts
  - math-proof                 2 skills, 3 agents, 1 scripts
  - mcp-server-dev             3 skills
  - mcp-tunnels                1 commands
  - php-lsp                    LSP
  - playground                 1 skills
  - playwright                 MCP
  - plugin-dev                 7 skills, 1 commands, 3 agents, 10 scripts
  - pr-review-toolkit          1 commands, 6 agents
  … 277 more
note        elsewhere by source kind: command 1, git-subdir 98, url 163; 261 of them pinned to a full sha
note        read-compatible: 22 content only (skills/commands/agents), 0 with allowed-tools, 33 running code (hooks/MCP/LSP servers), 262 hosted in other repositories
note        updates: pinned to commit 0fbfda5a91f4adc311daca25acc99132f40f0b6e; never updated automatically
network     2 api.github.com calls, 1 raw file fetches
verdict     pick an entry
full sha    0fbfda5a91f4adc311daca25acc99132f40f0b6e
```

## A content-only plugin from it (one skill)

```
$ cargo run -- plan anthropics/claude-plugins-official@0fbfda5a91f4adc311daca25acc99132f40f0b6e --pick frontend-design
DRY RUN: nothing installed, nothing executed.
source      github.com/anthropics/claude-plugins-official @ 0fbfda5a91f4 (0fbfda5a91f4adc311daca25acc99132f40f0b6e)
folder      plugins/frontend-design
found       Claude Code plugin `frontend-design` (from Claude marketplace `claude-plugins-official`)
mod         claude.claude-plugins-official.frontend-design 1.1.1
install to  <data>/mods/claude.claude-plugins-official.frontend-design/1.1.1-0fbfda5a91f4/
files       5 files, 31.3 KiB
  .claude-plugin/plugin.json
  LICENSE
  README.md
  skills/frontend-design/LICENSE.txt
  skills/frontend-design/SKILL.md
trust       prompts for Claude: nothing runs by itself; Claude's permission prompts still apply
permissions none
contributes
  - 1 agent plugin for Coqo's Claude sessions: 1 skills
  - skills: frontend-design
would run   nothing
note        loaded only into Coqo's own Claude sessions (Agent SDK `plugins`, or `--plugin-dir` in terminal mode); nothing is written to your Claude Code configuration
note        updates: pinned to commit 0fbfda5a91f4adc311daca25acc99132f40f0b6e; never updated automatically
network     2 api.github.com calls, 3 raw file fetches
verdict     installable after the user approves
full sha    0fbfda5a91f4adc311daca25acc99132f40f0b6e
```

## A plugin with hooks and pre-approved tools

```
$ cargo run -- plan anthropics/claude-plugins-official@0fbfda5a91f4adc311daca25acc99132f40f0b6e --pick hookify
DRY RUN: nothing installed, nothing executed.
source      github.com/anthropics/claude-plugins-official @ 0fbfda5a91f4 (0fbfda5a91f4adc311daca25acc99132f40f0b6e)
folder      plugins/hookify
found       Claude Code plugin `hookify` (from Claude marketplace `claude-plugins-official`)
mod         claude.claude-plugins-official.hookify 0.1.1
install to  <data>/mods/claude.claude-plugins-official.hookify/0.1.1-0fbfda5a91f4/
files       25 files, 78.2 KiB
  .claude-plugin/plugin.json
  .gitignore
  LICENSE
  README.md
  agents/conversation-analyzer.md
  commands/configure.md
  commands/help.md
  commands/hookify.md
  commands/list.md
  core/__init__.py
  core/config_loader.py
  core/rule_engine.py
  … 13 more
trust       RUNS PROGRAMS AS YOU: not sandboxed; review before enabling
permissions none
contributes
  - 1 agent plugin for Coqo's Claude sessions: 1 skills, 4 commands, 1 agents, HOOKS, 5 scripts
  - skills: writing-rules
  - slash commands: /hookify:configure, /hookify:help, /hookify:hookify, /hookify:list
  - subagents: conversation-analyzer
would run   
  ! hook (in Claude Code, as you): PostToolUse: runs `python3 "${CLAUDE_PLUGIN_ROOT}/hooks/posttooluse.py"`
  ! hook (in Claude Code, as you): PreToolUse: runs `python3 "${CLAUDE_PLUGIN_ROOT}/hooks/pretooluse.py"`
  ! hook (in Claude Code, as you): Stop: runs `python3 "${CLAUDE_PLUGIN_ROOT}/hooks/stop.py"`
  ! hook (in Claude Code, as you): UserPromptSubmit: runs `python3 "${CLAUDE_PLUGIN_ROOT}/hooks/userpromptsubmit.py"`
  ! pre-approves tools while active (plugins/hookify/commands/configure.md): ["Glob", "Read", "Edit", "AskUserQuestion", "Skill"]
  ! pre-approves tools while active (plugins/hookify/commands/help.md): ["Read"]
  ! pre-approves tools while active (plugins/hookify/commands/hookify.md): ["Read", "Write", "AskUserQuestion", "Task", "Grep", "TodoWrite", "Skill"]
  ! pre-approves tools while active (plugins/hookify/commands/list.md): ["Glob", "Read", "Skill"]
note        ships 5 executable scripts; nothing starts them by itself, the agent runs them only through tools Claude Code asks you about
note        loaded only into Coqo's own Claude sessions (Agent SDK `plugins`, or `--plugin-dir` in terminal mode); nothing is written to your Claude Code configuration
note        updates: pinned to commit 0fbfda5a91f4adc311daca25acc99132f40f0b6e; never updated automatically
network     2 api.github.com calls, 8 raw file fetches
verdict     installable after the user approves
full sha    0fbfda5a91f4adc311daca25acc99132f40f0b6e
```

## A plugin that is only an MCP server

```
$ cargo run -- plan anthropics/claude-plugins-official@0fbfda5a91f4adc311daca25acc99132f40f0b6e --pick context7
DRY RUN: nothing installed, nothing executed.
source      github.com/anthropics/claude-plugins-official @ 0fbfda5a91f4 (0fbfda5a91f4adc311daca25acc99132f40f0b6e)
folder      external_plugins/context7
found       Claude Code plugin `context7` (from Claude marketplace `claude-plugins-official`)
mod         claude.claude-plugins-official.context7 0.0.0
install to  <data>/mods/claude.claude-plugins-official.context7/0.0.0-0fbfda5a91f4/
files       3 files, 2.4 KiB
  .claude-plugin/plugin.json
  .mcp.json
  README.md
trust       RUNS PROGRAMS AS YOU: not sandboxed; review before enabling
permissions none
contributes
  - 1 agent plugin for Coqo's Claude sessions: MCP
would run   
  ! MCP server (started by Claude Code, as you): context7: connects to https://mcp.context7.com/mcp?client=claude-code-plugin
note        loaded only into Coqo's own Claude sessions (Agent SDK `plugins`, or `--plugin-dir` in terminal mode); nothing is written to your Claude Code configuration
note        updates: pinned to commit 0fbfda5a91f4adc311daca25acc99132f40f0b6e; never updated automatically
network     2 api.github.com calls, 3 raw file fetches
verdict     installable after the user approves
full sha    0fbfda5a91f4adc311daca25acc99132f40f0b6e
```

## anthropics/skills: a marketplace whose plugins are skill lists over one folder

```
$ cargo run -- plan anthropics/skills@e4bb5d960d87126ff3b3a8af1a104b7db67560ba --pick document-skills
DRY RUN: nothing installed, nothing executed.
source      github.com/anthropics/skills @ e4bb5d960d87 (e4bb5d960d87126ff3b3a8af1a104b7db67560ba)
found       Claude Code plugin `document-skills` (from Claude marketplace `anthropic-agent-skills`)
mod         claude.anthropic-agent-skills.document-skills 0.0.0
install to  <data>/mods/claude.anthropic-agent-skills.document-skills/0.0.0-e4bb5d960d87/
files       448 files, 11815.9 KiB
  .claude-plugin/marketplace.json
  .gitignore
  README.md
  THIRD_PARTY_NOTICES.md
  skills/academy-guide/LICENSE.txt
  skills/academy-guide/SKILL.md
  skills/algorithmic-art/LICENSE.txt
  skills/algorithmic-art/SKILL.md
  skills/algorithmic-art/templates/generator_template.js
  skills/algorithmic-art/templates/viewer.html
  skills/brand-guidelines/LICENSE.txt
  skills/brand-guidelines/SKILL.md
  … 436 more
trust       prompts for Claude: nothing runs by itself; Claude's permission prompts still apply
permissions none
contributes
  - 1 agent plugin for Coqo's Claude sessions: 19 skills, 27 scripts
  - skills: academy-guide, algorithmic-art, brand-guidelines, canvas-design, claude-api, discernment-nudge, doc-coauthoring, docx … (+11)
would run   nothing
note        ships 27 executable scripts; nothing starts them by itself, the agent runs them only through tools Claude Code asks you about
note        loaded only into Coqo's own Claude sessions (Agent SDK `plugins`, or `--plugin-dir` in terminal mode); nothing is written to your Claude Code configuration
note        updates: pinned to commit e4bb5d960d87126ff3b3a8af1a104b7db67560ba; never updated automatically
network     2 api.github.com calls, 20 raw file fetches
verdict     installable after the user approves
full sha    e4bb5d960d87126ff3b3a8af1a104b7db67560ba
```

## Coqo's own Claude Code plugin, as a folder of this repository

```
$ cargo run -- plan happsie/coqo@29ea89696b7c6c4db80b13422ffab1f86c358a80#integrations/claude-code/coqo-plugin
DRY RUN: nothing installed, nothing executed.
source      github.com/happsie/coqo @ 29ea89696b7c (29ea89696b7c6c4db80b13422ffab1f86c358a80)
folder      integrations/claude-code/coqo-plugin
found       Claude Code plugin `coqo`
mod         claude.coqo.coqo 0.1.0
install to  <data>/mods/claude.coqo.coqo/0.1.0-29ea89696b7c/
files       3 files, 5.5 KiB
  .claude-plugin/plugin.json
  commands/walkthrough.md
  skills/coqo-walkthrough/SKILL.md
trust       prompts for Claude: nothing runs by itself; Claude's permission prompts still apply
permissions none
contributes
  - 1 agent plugin for Coqo's Claude sessions: 1 skills, 1 commands
  - skills: coqo-walkthrough
  - slash commands: /coqo:walkthrough
would run   nothing
note        loaded only into Coqo's own Claude sessions (Agent SDK `plugins`, or `--plugin-dir` in terminal mode); nothing is written to your Claude Code configuration
note        updates: pinned to commit 29ea89696b7c6c4db80b13422ffab1f86c358a80; never updated automatically
network     2 api.github.com calls, 3 raw file fetches
verdict     installable after the user approves
full sha    29ea89696b7c6c4db80b13422ffab1f86c358a80
```

## The complete example manifest (a process mod)

```
$ cargo run -- plan happsie/coqo@29ea89696b7c6c4db80b13422ffab1f86c358a80#crates/mod-api/examples --releases
DRY RUN: nothing installed, nothing executed.
source      github.com/happsie/coqo @ 29ea89696b7c (29ea89696b7c6c4db80b13422ffab1f86c358a80)
folder      crates/mod-api/examples
found       Coqo mod `Maven tools` (process tier)
mod         acme.maven-tools 1.4.0
install to  <data>/mods/acme.maven-tools/1.4.0-29ea89696b7c/
files       1 files, 5.0 KiB
  coqo.mod.toml
trust       RUNS PROGRAMS AS YOU: not sandboxed; review before enabling
permissions read files: $workspace; read files: $home/.m2; write files: $workspace/target; write files: $cache; network: repo.maven.apache.org; network: *.example.com:443; start programs: mvn; start programs: java; keychain secret: acme.repo-token
contributes
  - 2 commands
  - 1 keybindings
  - 1 menu items
  - 5 settings (a section in Settings)
  - 2 languages
  - 1 language servers
  - 1 MCP tools for agents
  - 1 slash commands
  - 1 themes
  - 1 alert sources
  - 1 panel views
  - 1 rail views
  - 1 sidebar views
  - 1 status views
  - 1 tab views
would run   
  ! `bin/maven-tools --stdio` as a supervised process with your user rights, on workspace_contains:**/pom.xml, command:maven.run-goal, view:maven.modules, mcp_tool:maven_test
  ! language server `bin/lemminx` (started by Coqo, as you)
note        bin/maven-tools is not in the repository: it would come from a per-platform release asset (acme.maven-tools-1.4.0-<target>.tar.gz)
note        updates: pinned to commit 29ea89696b7c6c4db80b13422ffab1f86c358a80; never updated automatically
note        no releases
network     3 api.github.com calls, 1 raw file fetches
verdict     installable after the user approves
full sha    29ea89696b7c6c4db80b13422ffab1f86c358a80
```

## A core mod, which is refused

```
$ cargo run -- plan happsie/coqo@29ea89696b7c6c4db80b13422ffab1f86c358a80#mods/hello
DRY RUN: nothing installed, nothing executed.
source      github.com/happsie/coqo @ 29ea89696b7c (29ea89696b7c6c4db80b13422ffab1f86c358a80)
folder      mods/hello
found       Coqo mod `Hello` (core tier)
mod         coqo.hello 0.1.0
install to  <data>/mods/coqo.hello/0.1.0-29ea89696b7c/
files       3 files, 4.9 KiB
  Cargo.toml
  coqo.mod.toml
  src/lib.rs
trust       content only: nothing runs
permissions none
would run   nothing
note        updates: pinned to commit 29ea89696b7c6c4db80b13422ffab1f86c358a80; never updated automatically
ERROR       core mods are Rust crates compiled into Coqo; they can't be installed from a repository
network     2 api.github.com calls, 1 raw file fetches
verdict     REFUSED
full sha    29ea89696b7c6c4db80b13422ffab1f86c358a80
```

## Offline fixture: a content mod with a theme, settings, a static sidebar view and an agent plugin

```
$ cargo run -- local fixtures #content-mod
DRY RUN: nothing installed, nothing executed.
source      local folder fixtures
folder      content-mod
found       Coqo mod `Acme team kit` (content tier)
mod         acme.team-kit 1.2.0
install to  <data>/mods/acme.team-kit/1.2.0-local/
files       5 files, 1.0 KiB
  agent/.claude-plugin/plugin.json
  agent/commands/check.md
  agent/skills/team-review/SKILL.md
  coqo.mod.toml
  themes/night.toml
trust       prompts for Claude: nothing runs by itself; Claude's permission prompts still apply
permissions none
contributes
  - 1 keybindings
  - 1 settings (a section in Settings)
  - 1 themes
  - 1 sidebar views
  - agent plugin agent: 1 skills, 1 commands
would run   nothing
network     0 api.github.com calls, 0 raw file fetches
verdict     installable after the user approves
```

## Offline fixture: a content mod that tries to carry code

```
$ cargo run -- local fixtures #bad-content
DRY RUN: nothing installed, nothing executed.
source      local folder fixtures
folder      bad-content
found       Coqo mod `acme.bad` (content tier)
mod         acme.bad 0.1.0
install to  <data>/mods/acme.bad/0.1.0-local/
files       1 files, 0.2 KiB
  coqo.mod.toml
trust       content only: nothing runs
permissions network: evil.example
contributes
  - 1 commands
would run   nothing
ERROR       content mods have no code to activate; remove `activation`
ERROR       content mods run nothing, so they ask for no permissions
ERROR       `commands` needs code behind it; a content mod can't contribute it (slash commands go in an agent plugin)
network     0 api.github.com calls, 0 raw file fetches
verdict     REFUSED
```

## Offline fixture: a WASM mod whose component is a release asset

```
$ cargo run -- local fixtures #wasm-mod
DRY RUN: nothing installed, nothing executed.
source      local folder fixtures
folder      wasm-mod
found       Coqo mod `acme.tailwind` (wasm tier)
mod         acme.tailwind 0.3.0
install to  <data>/mods/acme.tailwind/0.3.0-local/
files       1 files, 0.3 KiB
  coqo.mod.toml
trust       sandboxed code: runs in the WASM host with only the permissions below
permissions read files: $workspace; network: registry.npmjs.org
contributes
  - 1 sidebar views
would run   
  ! tailwind.wasm in the WASM host, on workspace_contains:**/tailwind.config.*
note        tailwind.wasm is not in the repository: it would come from a release asset (acme.tailwind-0.3.0.wasm, with its sha256 in the release). Coqo never builds it on install
network     0 api.github.com calls, 0 raw file fetches
verdict     installable after the user approves
```

## Offline fixture: a Claude plugin with hooks, MCP servers and allowed-tools

```
$ cargo run -- local fixtures #claude-plugin
DRY RUN: nothing installed, nothing executed.
source      local folder fixtures
folder      claude-plugin
found       Claude Code plugin `deployer`
mod         claude.local.deployer 0.4.0
install to  <data>/mods/claude.local.deployer/0.4.0-local/
files       5 files, 0.4 KiB
  .claude-plugin/plugin.json
  .mcp.json
  bin/fmt.sh
  hooks/hooks.json
  skills/deploy/SKILL.md
trust       RUNS PROGRAMS AS YOU: not sandboxed; review before enabling
permissions none
contributes
  - 1 agent plugin for Coqo's Claude sessions: 1 skills, HOOKS, MCP, 1 scripts
  - skills: deploy
would run   
  ! hook (in Claude Code, as you): PostToolUse: runs `${CLAUDE_PLUGIN_ROOT}/bin/fmt.sh`
  ! MCP server (started by Claude Code, as you): deploy-db: runs `npx -y deploy-db-mcp`
  ! MCP server (started by Claude Code, as you): docs: connects to https://docs.example.com/mcp
  ! pre-approves tools while active (claude-plugin/skills/deploy/SKILL.md): Bash(kubectl:*)
note        ships 1 executable scripts; nothing starts them by itself, the agent runs them only through tools Claude Code asks you about
note        loaded only into Coqo's own Claude sessions (Agent SDK `plugins`, or `--plugin-dir` in terminal mode); nothing is written to your Claude Code configuration
network     0 api.github.com calls, 0 raw file fetches
verdict     installable after the user approves
```
