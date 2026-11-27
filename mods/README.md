# Built-in mods

Coqo's built-in core mods (PLAN §6) live here, one directory per mod: `mods/<name>/`.

**Boundary rule:** a crate under `mods/` may depend only on `coqo-mod-api` (plus allowed third-party crates), never on kernel internals. CI enforces this with [`scripts/check-mod-boundary.sh`](../scripts/check-mod-boundary.sh), which also fails if a crate under `mods/` is not a workspace member.

`mods/*` is a workspace member. A mod implements `coqo_mod_api::Mod` and ships a `coqo.mod.toml` next to its `Cargo.toml` (format: [`docs/mods/manifest.md`](../docs/mods/manifest.md)). [`hello`](hello/) is a minimal example.
