#!/bin/sh
# Assemble Coqo.app from built binaries, ad-hoc sign it and tar it.
# usage: package-macos.sh <version> <target> <bin-dir> <out-dir>
set -eu
version=${1:?}; target=${2:?}; bindir=${3:?}; out=${4:?}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app="$work/Coqo.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out"
cp "$bindir/coqo" "$bindir/coqod" "$app/Contents/MacOS/"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.coqo.Coqo</string>
  <key>CFBundleName</key><string>Coqo</string>
  <key>CFBundleDisplayName</key><string>Coqo</string>
  <key>CFBundleExecutable</key><string>coqo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force --deep -s - "$app"
codesign --verify --deep --strict "$app"
COPYFILE_DISABLE=1 tar -czf "$out/coqo-$version-$target.tar.gz" -C "$work" Coqo.app
