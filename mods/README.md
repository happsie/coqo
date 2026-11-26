# Built-in mods

Coqo's built-in core mods (PLAN §6) live here, one directory per mod: `mods/<name>/`.

**Boundary rule:** a crate under `mods/` may depend only on `coqo-mod-api` (plus allowed third-party crates), never on kernel internals. CI enforces this with [`scripts/check-mod-boundary.sh`](../scripts/check-mod-boundary.sh), which also fails if a crate under `mods/` is not a workspace member.

When the first mod is added, add `"mods/*"` to `members` in the root `Cargo.toml`.
