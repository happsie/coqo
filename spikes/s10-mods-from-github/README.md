# Spike S10: mods from GitHub repositories

Throwaway prototype for issue #355. The findings and the decision are in [`docs/adr/0033-mods-from-github.md`](../../docs/adr/0033-mods-from-github.md).

This directory is its own Cargo workspace, so the Coqo workspace and CI don't build it. It depends on `crates/mod-api` by path, so manifests are checked by the real `coqo_mod_api::Manifest`.

It resolves `owner/repo[@ref][#subdir]` to a pinned commit through the public GitHub API, finds what the folder holds, and prints a dry-run install plan: the files, the permissions, the contributions, and what would run. **It never executes, builds or installs anything it downloads.** It reads only text files: manifests, indexes, hook and MCP definitions, and skill frontmatter.

| Path | What it is |
|---|---|
| `src/source.rs` | Parses `owner/repo[@ref][#subdir]`, `github:` and github.com URLs; refuses `..` and odd refs. |
| `src/repo.rs` | A read-only view of a repository at one commit. `GitHub` makes two API calls (the ref to a SHA, then the tree at that SHA) and reads files from raw.githubusercontent.com by SHA. `Local` reads a folder, for the offline tests. The User-Agent is the generic `coqo-mod-installer`, with no auth and nothing personal. |
| `src/coqo.rs` | Checks a `coqo.mod.toml`, including the proposed `tier = "content"` and `[[contributes.agent_plugins]]`, and reads the proposed `coqo.marketplace.toml`. |
| `src/claude.rs` | Reads Claude Code plugins and `marketplace.json`: skills, commands, agents, hooks, MCP and LSP servers, `allowed-tools`, executables. |
| `src/plan.rs` | Works out what a folder is, the trust level, and renders the plan. |
| `src/tests.rs` | Offline tests on `fixtures/`. |
| `results.md` | Recorded runs against public repositories, pinned to commits. |

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo test
cargo run -- plan anthropics/claude-plugins-official --pick frontend-design
cargo run -- plan happsie/coqo#crates/mod-api/examples --releases
cargo run -- local fixtures '#content-mod'
```

Unauthenticated GitHub API calls are limited to 60 an hour per IP. A plan costs two, or three with `--releases`, plus one raw fetch per file read (up to 25 skill and command files).

**Known shortcuts.** It counts a plugin's skills from the default `skills/` folder, so a marketplace entry that lists its own `skills` paths (as anthropics/skills does) shows every skill in the folder. It doesn't follow plugins hosted in other repositories, and it doesn't fetch release assets. Symlinks are refused outright, without checking where they point.
