# Coqo

**Agents write the code. Humans keep the knowledge.**

Coqo is an open-source, agent-first desktop editor for macOS and Linux, written in Rust. The agent is the main author, and every feature is designed so that the person who owns the code still understands what was built and why: questions anchored to the lines they're about, code walkthroughs of agent changes, a native diff with comment threads that go back to the agent, a PR inbox, live traces of what actually ran, and a background daemon (`coqod`) that keeps agents working when the window is closed.

> **Status:** early development (milestone M0). There's nothing to run yet: the binaries are placeholders.

## Where things are

- **Plan:** [`docs/PLAN.md`](docs/PLAN.md) covers the vision, architecture, mod system, features and roadmap.
- **Architecture decisions:** [`docs/adr/`](docs/adr/) records why the design is the way it is.
- **Work tracking:** [GitHub issues](https://github.com/happsie/coqo/issues), organised by milestone. [`CLAUDE.md`](CLAUDE.md) describes how to pick up and finish an issue.

## Repository layout

```
crates/coqod/          daemon binary (incl. `coqod mcp` mode)
crates/coqo/           UI binary (GPUI) + `coqo` CLI subcommands
crates/kernel/         registry, services, supervisor, event log
crates/protocol/       Coqo Protocol types + schema
crates/mod-api/        public mod API (coqo-mod-api), the only dependency allowed for mods
crates/mod-host-wasm/  wasmtime host + WIT world
crates/mod-host-proc/  Coqo Mod Protocol host
crates/ui/             coqo-ui facade over GPUI + gpui-component
crates/anchors/        anchor model + re-anchoring
crates/storage/        SQLite + migrations
crates/lsp-client/  crates/dap-client/  crates/acp-host/  crates/mcp/
mods/                  built-in core mods
skills/                agent skills shipped with Coqo
sdk/                   mod SDKs and templates
fixtures/              sample projects for tests and spikes
docs/                  plan, ADRs, design and protocol docs
```

## Building

You need [rustup](https://rustup.rs). The toolchain version is pinned in [`rust-toolchain.toml`](rust-toolchain.toml), and rustup installs it automatically the first time you build.

```sh
cargo build --workspace
cargo test --workspace
```

Before opening a PR, also run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Licence

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
