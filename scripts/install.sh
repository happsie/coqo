#!/bin/sh
# Install Coqo from a signed GitHub Release. See docs/releasing.md.
#
#   curl -fsSL https://raw.githubusercontent.com/happsie/coqo/main/scripts/install.sh | sh
#
# Environment:
#   COQO_VERSION          install this version (e.g. 0.1.0) instead of the latest
#   COQO_SKIP_SIGNATURE=1 install without minisign (sha256 only); see the trust model
#   COQO_BASE_URL         release asset base URL (testing; file:// works)
#   COQO_PUBKEY_FILE      use this public key file instead of fetching release.pub
#   COQO_PUBKEY_URL       fetch the public key from here instead
set -eu

REPO=happsie/coqo
say() { printf 'coqo-install: %s\n' "$*"; }
die() { printf 'coqo-install: error: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  *) die "no Coqo release for $(uname -s) $(uname -m)" ;;
esac

if [ -n "${COQO_BASE_URL:-}" ]; then
  base=$COQO_BASE_URL
elif [ -n "${COQO_VERSION:-}" ]; then
  base="https://github.com/$REPO/releases/download/v$COQO_VERSION"
else
  base="https://github.com/$REPO/releases/latest/download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fetch() { curl -fsSL --proto '=https,file' -o "$2" "$1" || die "download failed: $1"; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

fetch "$base/manifest.json" "$tmp/manifest.json"

# Signature check (manifest first, then the archive, whose hash the manifest pins).
verify=1
if ! command -v minisign >/dev/null 2>&1; then
  if [ "${COQO_SKIP_SIGNATURE:-}" = 1 ]; then
    verify=0
    say "WARNING: minisign not found; skipping signature check (sha256 only)"
  else
    die "minisign is not installed, so the release signature cannot be checked.
Install it (macOS: brew install minisign; Linux: your package manager) and re-run,
or re-run with COQO_SKIP_SIGNATURE=1 to accept sha256-only verification."
  fi
fi
if [ "$verify" = 1 ]; then
  if [ -n "${COQO_PUBKEY_FILE:-}" ]; then
    cp "$COQO_PUBKEY_FILE" "$tmp/release.pub"
  else
    ref=main
    fetch "${COQO_PUBKEY_URL:-https://raw.githubusercontent.com/$REPO/$ref/crates/updater/release.pub}" "$tmp/release.pub"
  fi
  grep -q PLACEHOLDER "$tmp/release.pub" && die "the release public key is still a placeholder; refusing to install"
  fetch "$base/manifest.json.minisig" "$tmp/manifest.json.minisig"
  minisign -Vm "$tmp/manifest.json" -p "$tmp/release.pub" -x "$tmp/manifest.json.minisig" >/dev/null \
    || die "manifest signature is invalid"
fi

# One key per line in manifest.json, so awk can pick this target's block.
field() {
  awk -v t="\"$target\"" -v k="\"$1\"" '
    index($0, t) && /\{/ { on = 1; next }
    on && /\}/ { exit }
    on && index($0, k) { v = $0; sub(/^[^:]*:[ ]*/, "", v); gsub(/[",]/, "", v); print v; exit }
  ' "$tmp/manifest.json"
}
file=$(field file); sha=$(field sha256); size=$(field size)
[ -n "$file" ] && [ -n "$sha" ] || die "this release has no build for $target"
version=$(sed -n 's/^ *"version": *"\(.*\)",*$/\1/p' "$tmp/manifest.json")
say "installing Coqo $version ($target)"

fetch "$base/$file" "$tmp/$file"
[ "$(sha256_of "$tmp/$file")" = "$sha" ] || die "sha256 mismatch for $file"
[ "$(wc -c < "$tmp/$file" | tr -d ' ')" = "$size" ] || die "size mismatch for $file"
if [ "$verify" = 1 ]; then
  fetch "$base/$file.minisig" "$tmp/$file.minisig"
  minisign -Vm "$tmp/$file" -p "$tmp/release.pub" -x "$tmp/$file.minisig" >/dev/null \
    || die "archive signature is invalid"
fi

mkdir "$tmp/x"
tar -xzf "$tmp/$file" -C "$tmp/x"

case "$target" in
  *-apple-darwin)
    dest="$HOME/Applications"
    mkdir -p "$dest"
    [ -d "$tmp/x/Coqo.app" ] || die "archive has no Coqo.app"
    rm -rf "$dest/Coqo.app.new"
    mv "$tmp/x/Coqo.app" "$dest/Coqo.app.new"
    rm -rf "$dest/Coqo.app"
    mv "$dest/Coqo.app.new" "$dest/Coqo.app"
    say "installed $dest/Coqo.app"
    say "run it with: $dest/Coqo.app/Contents/MacOS/coqo"
    ;;
  *)
    share="$HOME/.local/share/coqo"
    mkdir -p "$share" "$HOME/.local/bin"
    [ -d "$tmp/x/coqo/bin" ] || die "archive has no coqo/bin"
    rm -rf "$share/bin.new"
    mv "$tmp/x/coqo/bin" "$share/bin.new"
    rm -rf "$share/bin"
    mv "$share/bin.new" "$share/bin"
    ln -sf "$share/bin/coqo" "$HOME/.local/bin/coqo"
    ln -sf "$share/bin/coqod" "$HOME/.local/bin/coqod"
    say "installed to $share/bin (symlinked from ~/.local/bin)"
    ;;
esac
