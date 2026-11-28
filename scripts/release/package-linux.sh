#!/bin/sh
# Tar the built binaries as coqo/bin/{coqo,coqod}.
# usage: package-linux.sh <version> <target> <bin-dir> <out-dir>
set -eu
version=${1:?}; target=${2:?}; bindir=${3:?}; out=${4:?}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/coqo/bin" "$out"
cp "$bindir/coqo" "$bindir/coqod" "$work/coqo/bin/"
strip "$work/coqo/bin/coqo" "$work/coqo/bin/coqod"
tar -czf "$out/coqo-$version-$target.tar.gz" -C "$work" coqo
