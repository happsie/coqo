# mod-boundary fixture

A tiny standalone workspace that `scripts/test-check-mod-boundary.sh` runs `scripts/check-mod-boundary.sh` against. `mods/good-mod` depends only on `coqo-mod-api` and must pass. `mods/bad-mod` also depends on `coqo-kernel` (under a renamed key) and must be reported. The fixture is not a member of the root workspace and is never compiled.
