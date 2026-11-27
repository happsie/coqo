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
crates/syntax/         coqo-syntax: languages, tree-sitter parsing, symbols, folding (no GPUI)
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

The UI uses GPUI, which needs a few system libraries on Linux:

```sh
# Fedora
sudo dnf install clang fontconfig-devel freetype-devel wayland-devel libxkbcommon-devel \
  libxkbcommon-x11-devel libxcb-devel libX11-devel openssl-devel libzstd-devel \
  alsa-lib-devel vulkan-loader-devel
# Debian / Ubuntu
sudo apt install clang libfontconfig-dev libfreetype-dev libwayland-dev libxkbcommon-dev \
  libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libssl-dev libzstd-dev libasound2-dev libvulkan-dev
```

On macOS, the Xcode command-line tools are enough. Run the app with `cargo run -p coqo --release`.

```sh
cargo build --workspace
cargo test --workspace
```

Before opening a PR, also run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Running CI locally

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs the commands above on Linux and macOS, plus two policy checks you can run yourself:

```sh
cargo install cargo-deny --locked     # once
cargo deny check                      # licences (GPL/AGPL/LGPL rejected), advisories, bans, sources; see deny.toml
scripts/check-mod-boundary.sh         # crates under mods/ may depend only on coqo-mod-api; needs jq
```

## Licence

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
