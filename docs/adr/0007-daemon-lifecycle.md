# 7. Daemon lifecycle: socket, single-instance lock, spawn-on-demand and idle policy

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #20

## Context

PLAN §4.3 describes a per-user daemon (`coqod`) that the UI and CLI attach to, that is started on demand, and that outlives the UI while agents work. Issue #20 implements it on top of the Coqo Protocol (ADR 0003). The lifecycle sets contracts that other work builds on: where the socket lives, how clients start the daemon, how services register their methods, and what keeps the daemon alive. This ADR records them.

## Decision

**Paths.** The runtime directory is `$XDG_RUNTIME_DIR/coqo` on Linux (falling back to `<tmp>/coqo-<uid>` when `XDG_RUNTIME_DIR` is unset) and `~/Library/Application Support/Coqo/run` on macOS. It holds `coqod.sock` and `coqod.lock`. `COQO_RUNTIME_DIR` overrides it and `COQO_DATA_DIR` overrides the data directory (PLAN §8), so tests and side-by-side daemons are hermetic. The directory is created `0700` and the socket is `chmod 0600`. Ownership checks, peer credentials and tokens are #21.

**Single instance.** The daemon takes an exclusive, non-blocking `flock` on `coqod.lock` before touching the socket, and writes its pid into it. The kernel drops the lock however the process dies, so a lock holder is always a live daemon. With the lock held, any existing socket file is stale (left by a crashed daemon) and is replaced. A second daemon exits with code `3` (`EXIT_ALREADY_RUNNING`) and names the running pid. On a clean exit the daemon removes its socket; the lock file stays, because deleting a lock file reintroduces races.

**Client crate.** Socket path resolution and connect-or-spawn live in a new crate, `crates/client` (`coqo-client`). It depends only on `coqo-protocol`, so the UI, the CLI and `coqod mcp` never link the daemon. `coqod` uses it too, so both sides resolve the same paths. Method types stay in `coqo-protocol` (`coqo_protocol::daemon::{Status, Shutdown}`), next to the generated schema.

**Spawn on demand.** `connect_or_spawn` connects to the socket. On `ENOENT` or `ECONNREFUSED` it starts `coqod --detached` with `COQO_RUNTIME_DIR` set to the directory it looks in, stdio on `/dev/null` and `/` as working directory, then polls the socket with backoff until it answers (default timeout 10 s). If the child exits with code 3, another client won the race (or an old daemon is still shutting down), so it keeps polling and respawns if needed. Any other early exit is reported. The daemon binary is `$COQO_DAEMON`, else `coqod` next to the running executable, else `coqod` on `PATH`.

**Detaching.** `unsafe_code` is denied, so the client cannot call `setsid` in a `pre_exec` hook. Instead `coqod --detached` calls `setsid()` itself as its first action. That gives it its own session and process group, so closing the UI or its terminal does not signal it. The client does not use `process_group(0)`, because a process-group leader cannot call `setsid`. Detached daemons log to `<data dir>/logs/coqod.log`; `--foreground` logs to stderr. A bare `coqod` starts a detached daemon through the same client code and returns once it answers.

**Compatibility.** The handshake already rejects an incompatible protocol major (ADR 0003); `connect_or_spawn` passes that through as `ProtocolError::IncompatibleVersion`. Different builds with compatible protocols keep working. `Connection::same_build` lets the UI notice an older daemon and offer a restart once it is idle (PLAN §4.3).

**Services.** `coqod` is a library plus a thin binary. A service is a module with `register(router, …) -> Router`, built from the shared `DaemonContext` (paths, data dirs, activity, idle phase, shutdown token). Each service adds one line to `routes::router`.

**Idle policy.** `Activity` counts attached connections and *busy guards*. A service holds `ctx.activity.busy("agent session 42")` while it does work that must outlive the UI. With no connections and no busy guards the daemon is idle: after `--idle-stop-services` (default 30 min) the phase becomes `ServicesStopped`, and services watching `ctx.idle` release resources such as language servers. After `--idle-exit` (default 60 min) the daemon exits; `--always-on` disables that. Any connection or busy guard resets the timers. The timers use `tokio::time`, so tests drive them with a paused clock.

**Shutdown.** Idle exit, `daemon/shutdown`, SIGTERM and SIGINT all take the same path: stop accepting, remove the socket, cancel `ctx.shutdown`, give connections 200 ms to flush their last responses, then close them. `daemon/shutdown` refuses while busy guards are held unless `force` is set, and lists them.

## Consequences

- The UI (#27), the CLI and `coqod mcp` attach with one call, and concurrent starts are safe.
- Agent sessions, traces and other long work keep the daemon alive by holding a busy guard; nothing else needs to know about the idle policy.
- The idle settings are command-line flags for now. Reading them (and "always on") from `settings.toml` comes with the settings service.
- Start at login (systemd `--user` unit, launchd agent) and the macOS helper inside the `.app` bundle are not part of this decision. A login-started daemon uses the same lock and socket, so clients need no changes.
- Between `spawn` and the daemon's `setsid` there is a window of a few milliseconds in which a signal to the UI's process group would reach the daemon. That is acceptable.
