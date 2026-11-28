# 31. The in-app updater: GitHub Releases, minisign, a two-rename swap and an idle handoff

- **Status:** Accepted
- **Date:** 2026-11-28
- **Issue:** #349 (epic #123; refs #125; sibling #348, the release workflow; PLAN §4.3, §12 M10)

## Context

The maintainer builds Coqo at home, tags a release, and wants the Mac at work (Apple Silicon, `aarch64-apple-darwin`) to pick it up with one click. The release format is set in #349 and produced by #348: per target a `coqo-X.Y.Z-<target>.tar.gz` (macOS: `Coqo.app/`; Linux: `coqo/bin/{coqo,coqod}`) and its `.minisig`, plus a `manifest.json` (`version`, `protocol`, and per target `file`, `sha256`, `size`) and its `.minisig`, all on a GitHub Release of `happsie/coqo`. Signatures are minisign Ed25519; the secret key lives only in a GitHub Actions secret.

The update replaces running binaries (the UI and the daemon), and the daemon owns agent sessions that must never be cut off mid-turn. PLAN §4.3 says a new UI keeps working with an older daemon whose protocol is compatible, and otherwise waits for agents to be idle before the daemon restarts.

## Decision

**One crate, `coqo-updater`, used by both sides.** The daemon checks, downloads, verifies, unpacks and swaps (`update/*`); the UI shows the result, relaunches itself and does the rollback. The daemon is the only checker, so several windows never each poll GitHub.

**What is trusted, and against what.**

- The **public key is compiled in** (`crates/updater/release.pub`, `include_str!`). There is no environment variable, setting or file that changes it. Tests make an Ed25519 key per run (ring) and hand it to the updater through a `cfg(test)` constructor; release builds have no such door. While the file is #348's placeholder (it contains `PLACEHOLDER`) or does not parse, the build has **no release key**: the updater is off (`OffReason::NoReleaseKey`) and never contacts GitHub.
- GitHub's release JSON is **not** trusted for integrity: it only says which tag is newest and where the assets are. Asset URLs must start with `https://github.com/happsie/coqo/releases/download/` (redirects to GitHub's storage are followed, `https` only, at most 5).
- `manifest.json` is trusted only after **`manifest.json.minisig` verifies** against the compiled-in key. Its `version` must equal the tag's, and it must be newer than the running version (**no downgrades**, no re-install of the same version; pre-releases only with `updates.channel = "pre"`). Its asset entry for this build's target must be named exactly `coqo-<version>-<target>.tar.gz`, with a 64-hex-digit sha256 and a size of at most 512 MiB.
- The archive is downloaded with that size as a hard cap; its **sha256 and size must match the signed manifest**, and its own **`.minisig` must verify** against the key too. Both minisign modes verify (prehashed, minisign's default, and legacy); both are Ed25519 by the same key.
- Unpacking allows only **regular files and folders under `Coqo.app/`** (or `coqo/bin/`): no symlinks, hard links or devices, no `..`, no absolute paths, nothing twice, at most 10 000 entries and 2 GiB. Modes are normalised to 0755/0644 (no set-id bits); extended attributes are never restored (`tar` without `xattr`).
- Anything that fails is refused with the reason (`Update to vX failed: the signature of manifest.json does not verify against Coqo's release key; not installing it`), and nothing is staged or swapped.

The threat model is a compromised or spoofed download path (CDN, network, a GitHub release edited without the key). Someone who can push to `main` can change the committed key, and anyone running as the user can already replace the app; neither is defended against here.

**What is sent.** Every request is a `GET` with `Host` and `User-Agent: coqo-updater/<version>`, plus `If-None-Match` when polling the release. Nothing else: no token, cookie, `Accept-*`, user or machine detail. A test asserts the exact header set the fake server receives.

**When.** Shortly after the daemon starts (5 s), then every 6 hours, and on the palette's **Check for updates** (`update/check`). The release poll sends the last `ETag`, so an unchanged release is a `304`. `updates.check = false` in `settings.toml` turns it all off; the daemon reads `[updates]` before every check. Stable reads `releases/latest`; `pre` reads the 20 newest releases and takes the highest version.

**Where it is installed.** `Install::detect` looks at `current_exe()` (with symlinks resolved): macOS `…/Coqo.app/Contents/MacOS/` in `~/Applications`, or in `/Applications` when the user can write there; Linux (x86_64, a first-class target alongside the M2; its release build is #374) `~/.local/share/coqo/bin`, whether started from there, through the `~/.local/bin/{coqo,coqod}` symlinks `install.sh` makes, or from the .desktop entry. Because the swap renames the whole bin folder, those symlinks and the .desktop entry reach the new version without being touched. Anything else, including a checkout's `target/debug` or `target/release` (also through a symlink), is a **development build**: the updater is off, the popover says "Running a development build", and nothing is ever unpacked or swapped (every install operation on `Install::Dev` is an error). In-process daemons (tests) are always off; `serve_with` passes `UpdatesConfig::Off`.

**The swap.** The verified archive is unpacked into `<install>.staging` beside the install (same folder, so the same file system), checked to hold both binaries (and `Info.plist`), and moved to `<install>.new`. **Update and restart** calls `update/install`: the daemon renames the install to `<install>.old` (removing an older `.old`) and `.new` into its place, and undoes the first rename if the second fails. **No running binary is ever written to**: a running process keeps its file under `.old` until it exits. One previous version is kept. A `pending.json` record in `<data>/updates/` says which version was swapped in over which. On macOS the quarantine attribute is cleared only if the unpacked app has one, which a download by Coqo itself does not set.

**The handoff.** `update/install` also arms **`daemon/restartWhenIdle`**: the daemon shuts down cleanly once no agent turn runs (busy guards whose reason starts with `agent session `, `coqo_agents::TURN_BUSY_PREFIX`; terminals don't count). The UI then starts the new `coqo` detached (`open -n Coqo.app` on macOS; the bin folder's `coqo` in its own process group on Linux) and quits. The new UI attaches to the old daemon while its agents finish; the status bar says **Restarting the daemon when agents are idle (N running)**, and the popover's **Restart now** asks first ("Restarting now stops N running agent turns…") before `daemon/restartWhenIdle { now: true }`. When the old daemon exits, the UI reconnects once by itself, which spawns the new `coqod` next to the new `coqo` through the normal spawn and lock path (ADR 0007, #262, #328), and sessions resume there (ADR 0026, #67). The new daemon clears `pending.json` once it is up. If the old daemon speaks an incompatible protocol, the new UI cannot attach; while `pending.json` names its version it retries every 5 s until the old daemon has restarted.

**Rollback.** If the daemon the UI spawned exits during start-up (`ConnectError::DaemonExited`, with #328's reason) and `pending.json` says this UI's version was just swapped into this install, the UI removes the record first (so this happens **once**), renames the install to `.failed` and `.old` back into place, writes `rolled-back.json` with the version, the restored version and the reason, relaunches the restored UI and quits. The restored daemon reports the record in `update/status` (the status bar shows "vX rolled back", and the popover the reason), and does not offer that version again; a newer release clears it.

**Protocol.** `update/status`, `update/check`, `update/install` and `daemon/restartWhenIdle`, all `Access::daemon()` (agents never update or restart Coqo). Errors use `UPDATE_FAILED` (2101). Additive, so the protocol version stays 0.1.

## Consequences

- The first release needs the real key committed (`docs/releasing.md`); until then every build refuses updates, by design.
- Only published targets update (`aarch64-apple-darwin` from #348, `x86_64-unknown-linux-gnu` from #374); others see "There are no Coqo releases for this platform" or "v… has no build for <target>".
- A restart closes the daemon's terminals; agent sessions resume, terminals do not.
- The app is ad-hoc signed and not notarized (#124); the updater does not change that.
- Releases larger than 512 MiB, or archives with symlinks, are refused; #348's packaging produces neither.

## Alternatives considered

- **Shelling out to `curl` and `minisign`:** fewer crates, but no control over headers, no testable fake server and a runtime dependency. The workspace already builds hyper and rustls (through gpui-kit), so the updater uses them directly with the OS's roots; `minisign-verify` (MIT, no dependencies) verifies.
- **The UI swaps and the daemon only downloads:** two processes would then know about the staged release; letting the daemon do the swap keeps one owner, and the UI needs only the install path to relaunch and to roll back.
- **An atomic exchange (`renameat2(RENAME_EXCHANGE)`, `renamex_np(RENAME_SWAP)`):** not portable across both systems without `unsafe`; the two renames leave the install missing for microseconds, during which nothing launches it.
