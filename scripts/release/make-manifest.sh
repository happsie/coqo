#!/bin/sh
# Write manifest.json for every coqo-<version>-<target>.tar.gz in <dir>.
# usage: make-manifest.sh <version> <dir>   (run from the repo root)
# Format (shared with the updater, see docs/releasing.md). It is pretty
# printed with one key per line, which scripts/install.sh relies on.
set -eu
version=${1:?}; dir=${2:?}
proto=$(sed -n 's/.*PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: \([0-9]*\), minor: \([0-9]*\) }.*/\1.\2/p' crates/protocol/src/handshake.rs)
[ -n "$proto" ] || { echo "cannot read PROTOCOL_VERSION" >&2; exit 1; }
cd "$dir"
assets=$(for f in coqo-"$version"-*.tar.gz; do
  target=${f#coqo-"$version"-}; target=${target%.tar.gz}
  jq -n --arg t "$target" --arg f "$f" --arg s "$(sha256sum "$f" | cut -d' ' -f1)" \
    --argjson n "$(wc -c < "$f" | tr -d ' ')" '{($t): {file: $f, sha256: $s, size: $n}}'
done | jq -s 'add')
jq -n --arg v "$version" --arg p "$proto" --argjson a "$assets" \
  '{version: $v, protocol: $p, assets: $a}' > manifest.json
cat manifest.json
