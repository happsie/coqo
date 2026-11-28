#!/bin/sh
# Install a pinned minisign into the directory given as $1, checking the
# download against a sha256 pinned here. Used by the release workflow.
set -eu
dest=${1:?usage: install-minisign.sh <dir>}
ver=0.12
mkdir -p "$dest"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/jedisct1/minisign/releases/download/$ver"
case "$(uname -s)" in
  Linux)
    file="minisign-$ver-linux.tar.gz"
    sha=9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73
    ;;
  Darwin)
    file="minisign-$ver-macos.zip"
    sha=89000b19535765f9cffc65a65d64a820f433ef6db8020667f7570e06bf6aac63
    ;;
  *) echo "unsupported OS" >&2; exit 1 ;;
esac
curl -fsSL -o "$tmp/$file" "$base/$file"
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$file" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/$file" | cut -d' ' -f1)
fi
[ "$got" = "$sha" ] || { echo "minisign checksum mismatch: $got" >&2; exit 1; }
if [ "$(uname -s)" = Linux ]; then
  case "$(uname -m)" in aarch64|arm64) arch=aarch64 ;; *) arch=x86_64 ;; esac
  tar -xzf "$tmp/$file" -C "$tmp" 2>/dev/null
  cp "$tmp/minisign-linux/$arch/minisign" "$dest/minisign"
else
  unzip -q -o "$tmp/$file" -d "$tmp" -x '._*'
  cp "$tmp/minisign" "$dest/minisign"
fi
chmod +x "$dest/minisign"
"$dest/minisign" -v | head -1
