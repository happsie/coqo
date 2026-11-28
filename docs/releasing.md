# Releasing Coqo

Pushing a tag `vX.Y.Z` (`-rc.N` pre-releases allowed) runs
`.github/workflows/release.yml` and publishes a signed GitHub Release.
Notarization, dmg and AppImage are out of scope here (issue #124).

## Targets

- `aarch64-apple-darwin`, built natively on the arm64 `macos-latest` runner.
  This is the only target that is built.
- `x86_64-apple-darwin` (Intel) is left out on purpose: no one needs it, and it
  would mean cross-compiling GPUI's Metal build.
- Linux x64 is not built yet. `scripts/install.sh` already understands the
  `x86_64-unknown-linux-gnu` layout (`coqo/bin/{coqo,coqod}`), so adding it
  later is a matrix entry plus a packaging step.

## One-time key setup (maintainer)

The workflow never creates keys or secrets. Run this once, on a trusted
machine with [minisign](https://jedisct1.github.io/minisign/) installed
(`brew install minisign`):

```sh
umask 077
mkdir -p ~/.coqo-release-key && cd ~/.coqo-release-key
minisign -G -W -p release.pub -s release.key      # -W: no password
gh secret set COQO_RELEASE_MINISIGN_KEY -R happsie/coqo < release.key
cp release.pub /path/to/coqo/crates/updater/release.pub
shred -u release.key 2>/dev/null || rm -P release.key || rm -f release.key
```

Then commit `crates/updater/release.pub` (two lines: `untrusted comment: ...`
and the base64 key) in a PR, and delete `~/.coqo-release-key`. Never commit
or paste `release.key`. The committed file is a placeholder until you do
this, and the workflow refuses to publish while it contains `PLACEHOLDER`.

## Releasing

1. Bump `version` in `[workspace.package]` in `Cargo.toml` and run
   `cargo check` so `Cargo.lock` follows. Merge that to `main`.
2. Optionally rehearse: Actions, "Release", "Run workflow", `dry_run` on (or
   `gh workflow run release.yml -R happsie/coqo --ref main -f dry_run=true`).
   It builds, signs and runs `install.sh` against the result, then uploads
   the files as workflow artifacts without publishing.
3. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`. The tag must
   equal the workspace version or the build fails.

## What the workflow does

1. **build** (macOS arm64): `cargo build --release --locked` for `coqo` and
   `coqod`, assembles `Coqo.app` (`Info.plist`, bundle id `dev.coqo.Coqo`,
   both binaries in `Contents/MacOS/`), signs it ad hoc (`codesign -s -`),
   checks `lipo -info` says arm64 and that `--version` prints the workspace
   version, then tars it as `coqo-X.Y.Z-<target>.tar.gz`.
2. **sign**: writes `manifest.json`, signs every archive and the manifest
   with minisign (pinned 0.12, download checked against a sha256 in
   `scripts/release/install-minisign.sh`), and verifies each signature with
   `crates/updater/release.pub`.
3. **install-test** (macOS): runs `scripts/install.sh` against the signed
   files, runs `coqod --version`, and checks that a tampered manifest is
   refused.
4. **release** (tags only): `gh release create` with the archives, the
   manifest and all `.minisig` files.

Without the secret, a manual dry run generates a throwaway keypair inside the
job so the signing steps can be tested. Its artifacts are named
`...-unsigned-test-build`, carry `UNSIGNED-TEST-BUILD.txt`, and `release`
refuses to publish them. A tag build with no secret, or with the placeholder
public key, fails with a clear error.

## Release format

Shared with the updater (issue #349). Per release:

- `coqo-X.Y.Z-<target>.tar.gz` and `.minisig`. On macOS it contains
  `Coqo.app/`; on Linux it contains `coqo/bin/{coqo,coqod}`.
- `manifest.json` and `manifest.json.minisig`:
  `{"version": "X.Y.Z", "protocol": "<major>.<minor>", "assets": {"<target>": {"file", "sha256", "size"}}}`.
  `protocol` is `PROTOCOL_VERSION` from `crates/protocol`, as a string such as
  `"0.1"`.
- minisign Ed25519 signatures; the public key is `crates/updater/release.pub`.

## Installing: `scripts/install.sh`

```sh
curl -fsSL https://raw.githubusercontent.com/happsie/coqo/main/scripts/install.sh | sh
```

It installs to `~/Applications/Coqo.app` on macOS (curl does not set the
Gatekeeper quarantine flag, so the ad-hoc signed app opens), and to
`~/.local/share/coqo/bin` with symlinks in `~/.local/bin` on Linux.
`COQO_VERSION=X.Y.Z` picks a version.

### Trust model

- Everything is fetched over HTTPS from github.com (the latest release's
  assets) and raw.githubusercontent.com (the public key, from `main`).
- The script verifies the `manifest.json` signature with the public key, then
  the archive's sha256 and size against the manifest, then the archive's
  signature. A bad signature or hash aborts the install.
- The public key and the script itself come from the same GitHub repository.
  Someone who can push to `main` can change both, so the signature protects
  against a tampered release or CDN, not against a compromised repository.
- If `minisign` is not installed the script stops and says how to install it.
  `COQO_SKIP_SIGNATURE=1` continues with sha256 only. The hash comes from the
  same place as the archive, so that only catches corruption, not tampering.
- While the public key is still the placeholder the script refuses to install.
- The app is ad-hoc signed, not notarized (issue #124).

## Updating

An installed Coqo updates itself from this repository's GitHub Releases ([ADR 0031](adr/0031-in-app-updater.md), issue #349). A development build (run from a checkout, `target/…`) never updates; the popover says "Running a development build".

- **Where:** macOS `~/Applications/Coqo.app` (or `/Applications/Coqo.app` when you can write there); Linux `~/.local/share/coqo/bin` (also when started through the `~/.local/bin` symlinks or the .desktop entry, which keep working after an update because the whole folder is swapped). Install there once by hand (`scripts/install.sh`, #348); after that, updates arrive in place.
- **When:** the daemon checks shortly after it starts, every 6 hours, and on **Check for updates** in the palette. It asks `api.github.com/repos/happsie/coqo/releases/latest` with only `User-Agent: coqo-updater/<version>` (and `If-None-Match`), and downloads the release's `manifest.json`, `.minisig` files and the archive for this machine's target (`aarch64-apple-darwin` on Apple Silicon).
- **Trust:** `manifest.json.minisig` must verify against the public key compiled in from `crates/updater/release.pub`; the archive's sha256 and size must match the manifest, and its own `.minisig` must verify too. Downgrades and the same version are refused; pre-releases only with `channel = "pre"`. While `release.pub` is the placeholder, no build updates.
- **Install:** the status bar shows **Update to vX.Y.Z**; its popover has the release notes and **Update and restart**. The daemon swaps the verified app in (the previous one is kept as `Coqo.app.old`, or `bin.old`), Coqo relaunches, and the daemon restarts once no agent turn is running ("Restarting the daemon when agents are idle (N running)"; **Restart now** asks first). Chats resume on the new daemon.
- **Rollback:** if the new daemon exits during start-up, Coqo moves the previous version back once, relaunches it, and shows why. That version is not offered again; the next release is.
- **Settings** (`settings.toml` in the config folder):

  ```toml
  [updates]
  check = true        # false: never contact GitHub
  channel = "stable"  # "pre": offer pre-releases too
  ```

### Before the first release

1. Do the one-time key setup above. Builds made before the real `release.pub` is committed can't verify anything, so the first updatable version is the first one built with the real key.
2. Install that version once with `scripts/install.sh`, so Coqo runs from an install location rather than a checkout.
3. Archives must hold only regular files and folders under `Coqo.app/` (or `coqo/bin/`): no symlinks, and `COPYFILE_DISABLE=1` so macOS tar adds no `._*` files (`scripts/release/package-macos.sh` does both).
