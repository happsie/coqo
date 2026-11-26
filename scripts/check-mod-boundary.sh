#!/usr/bin/env bash
# Enforce the mod boundary (PLAN §6): a crate under `mods/` may depend only on
# `coqo-mod-api` plus third-party crates, never on another workspace crate such
# as `coqo-kernel`. Every crate under `mods/` must also be a workspace member,
# so no mod can slip past this check.
#
# Usage: scripts/check-mod-boundary.sh [path/to/workspace/Cargo.toml]
# Needs: cargo, jq.
set -euo pipefail

manifest="${1:-Cargo.toml}"
allowed="coqo-mod-api"

metadata="$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest")"
root="$(jq -r '.workspace_root' <<<"$metadata")"

status=0

# 1. Every crate under mods/ must be a workspace member.
while IFS= read -r toml; do
  if ! jq -e --arg m "$toml" '.packages | any(.manifest_path == $m)' <<<"$metadata" >/dev/null; then
    echo "error: ${toml#"$root"/} is not a workspace member, so the mod boundary can't be checked." >&2
    echo "       Add \"mods/*\" to [workspace] members in the root Cargo.toml." >&2
    status=1
  fi
done < <(find "$root/mods" -mindepth 2 -maxdepth 2 -name Cargo.toml 2>/dev/null | sort)

# 2. Mods may not depend on any workspace crate except coqo-mod-api.
#    Matched by package name, so renames and git/registry sources are caught too.
violations="$(jq -r --arg mods "$root/mods/" --arg allowed "$allowed" '
  [.packages[].name] as $ws
  | .packages[]
  | select(.manifest_path | startswith($mods))
  | . as $mod
  | .dependencies[]
  | select(.name != $allowed and (.name as $n | $ws | index($n)))
  | "\($mod.name) -> \(.name) (\(.kind // "normal") dependency)"
' <<<"$metadata")"

if [[ -n "$violations" ]]; then
  echo "error: mod boundary violated. Crates under mods/ may depend only on $allowed" >&2
  echo "       (plus third-party crates), never on kernel internals or other workspace crates:" >&2
  while IFS= read -r line; do echo "         $line" >&2; done <<<"$violations"
  echo "       Expose what the mod needs through $allowed instead (see mods/README.md)." >&2
  status=1
fi

if [[ $status -eq 0 ]]; then
  echo "mod boundary OK"
fi
exit $status
