# 7. Daemon lifecycle: socket, single-instance lock, spawn-on-demand and idle policy

- **Status:** Accepted
- **Date:** 2026-11-27
- **Issue:** #20, #21 (socket security), #153 (default-deny route policy)

## Context

PLAN §4.3 describes a per-user daemon (`coqod`) that the UI and CLI attach to, that is started on demand, and that outlives the UI while agents work. Issue #20 implements it on top of the Coqo Protocol (ADR 0003). The lifecycle sets contracts that other work builds on: where the socket lives, how clients start the daemon, how services register their methods, and what keeps the daemon alive. This ADR records them.

## Decision

**Paths.** The runtime directory is `$XDG_RUNTIME_DIR/coqo` on Linux (falling back to `<tmp>/coqo-<uid>` when `XDG_RUNTIME_DIR` is unset) and `~/Library/Application Support/Coqo/run` on macOS. It holds `coqod.sock`, `coqod.lock` and `coqod.token`. `COQO_RUNTIME_DIR` overrides it and `COQO_DATA_DIR` overrides the data directory (PLAN §8), so tests and side-by-side daemons are hermetic. The directory is created `0700` and the socket is `chmod 0600`. Both sides verify the directory before using it (see *Socket security*).

**Single instance.** The daemon takes an exclusive, non-blocking `flock` on `coqod.lock` before touching the socket, and writes its pid into it. The kernel drops the lock however the process dies, so a lock holder is always a live daemon. With the lock held, any existing socket file is stale (left by a crashed daemon) and is replaced. A second daemon exits with code `3` (`EXIT_ALREADY_RUNNING`) and names the running pid. On a clean exit the daemon removes its socket; the lock file stays, because deleting a lock file reintroduces races.

**Client crate.** Socket path resolution and connect-or-spawn live in a new crate, `crates/client` (`coqo-client`). It depends only on `coqo-protocol`, so the UI, the CLI and `coqod mcp` never link the daemon. `coqod` uses it too, so both sides resolve the same paths. Method types stay in `coqo-protocol` (`coqo_protocol::daemon::{Status, Shutdown}`), next to the generated schema.

**Socket security (#21).** Only the user's own processes may attach (PLAN §9). There are three layers, all on the accept and connect paths.

1. *Runtime directory.* The daemon creates the last component of the path with `mkdir 0700`, never recursively, so parents get default permissions. It then opens the directory with `O_NOFOLLOW | O_DIRECTORY` and `fstat`s that descriptor: it must be a directory, owned by the effective uid, with no group or other bits. Anything else is refused with an error that names the fix: a symlink, another user's directory (for example a `/tmp/coqo-<uid>` someone else pre-created), or a directory open to other users. The daemon never `chmod`s or adopts an existing directory, because a directory that was open may already hold planted files. The lock and token files are opened with `openat` relative to the checked descriptor, also with `O_NOFOLLOW`. Only the socket is bound by path, because `bind` and `connect` need one. That is safe because nobody else can create or rename entries in a `0700` directory we own, and before serving the daemon checks that the path still leads to the checked directory (same `dev`/`ino`). Clients (`coqo-client`) run the same check before they connect, and treat a missing directory as "not running". So they never talk to a socket someone else planted, and never send it a token.
2. *Peer credentials.* The daemon reads every accepted connection's credentials (`UnixStream::peer_cred`, which is `SO_PEERCRED` on Linux and `getpeereid` on macOS). It drops the connection before reading a byte if the uid is not its own, and logs the peer's uid and pid at `warn`. The client checks the daemon's uid the same way.
3. *Session token.* At each start, after taking the lock and before binding the socket, the daemon generates 256 random bits from the OS (`getrandom`). It writes them as hex to `coqod.token` (`0600`), through a temporary file and `renameat`, and removes the file on a clean exit. Protocol 0.1 adds an optional `token` to `coqo/hello` (ADR 0003's minor-version rules: a new optional field). `coqod` answers a missing or wrong token with `-32006 UNAUTHORIZED` and closes the connection. `coqo-client` reads the token itself, after connecting, so it is the token of the daemon that accepted the connection. It uses `ClientConfig::token` instead if that is set. A missing token file means an older daemon without tokens, so the client sends none.

The token is handled as a secret. `SessionToken` compares in constant time (`subtle`) and its `Debug` prints `<redacted>`. The server takes it out of the `Hello` before handlers see it, and it is never logged (a test checks the debug log). It is never passed in `argv`: child processes such as `coqod mcp` get a token in the environment (`COQO_SESSION_TOKEN`).

*Scopes.* The daemon token grants `Scope::Daemon`. `Auth::mint_session(workspace, session)` mints a token for one agent session in one workspace (`Scope::Session`), for `coqod mcp` (PLAN §7.4, #69), and `revoke` / `revoke_session` retire it. Every request carries its connection's scope (`RequestContext::scope`).

*Route policy (#153).* Session scopes are **denied by default, in the router**. Each method, notification and topic declares at registration who may call it, with an `Access` (`Router::method_with`, `notification_with`, `topic_with`), and the router checks it before the handler runs:

| `Access` | Session tokens may call it… |
|---|---|
| `Access::daemon()` | never: `-32007 FORBIDDEN` before the params are parsed |
| `Access::workspace(\|p\| &p.workspace.0)` | for their own workspace, read from the params (`Scope::require_workspace`) |
| `Access::session(\|p\| &p.session.0)` | for their own agent session (`Scope::require_session`) |
| `Access::check(\|scope, p\| …)` | when a custom check passes, for bindings the params do not spell out (a buffer id → its workspace) |

`Router::method` / `notification` / `topic` without an `Access` register a daemon-only route, so a handler that forgets cannot let a session token through. `Scope::Daemon` connections skip the check and reach every route. A refused notification is dropped and logged, since it has no response. `Router::routes()` lists every route with its access. A `coqod` test builds the real router and fails if any route has no explicit `Access`, or if the set of session-callable routes differs from its allowlist, and an end-to-end test sends a session token to every daemon-only route and to every session route aimed at another workspace and expects `FORBIDDEN`. Opening a route to session tokens is therefore a deliberate, reviewed change to that allowlist.

The allowlist is what the agent bridge (`coqod mcp`, #69) needs in its own workspace:

- `workspace/get`, `workspace/getState`, and `workspace/events` filtered to it.
- `fs/list`, `fs/stat`, `fs/read`, `fs/changes`.
- `buffer/open` in it, and `buffer/edit`, `buffer/changes`, `buffer/close` on buffers open in it (a check looks up the buffer's workspace).
- `syntax/symbols`, `syntax/outline`, `syntax/folding` on buffers open in it (read-only, the same check; added by #42 so agents can name and anchor to code).

Everything else needs the daemon token: `daemon/status` (it lists every busy session) and `daemon/shutdown`; opening, listing and managing workspaces, because adding a root would widen a session's file access; `workspace/setState` (the UI's state); `buffer/save` and `buffer/revertChange` (the user's review actions); all of `terminal/*`, which is not tied to a workspace; and all of `agents/*` and `sessions/*`, which reach other sessions, permission rules and provider install actions. New editor tools (#70, #73, #75) open the methods they add with `Access::workspace` or `Access::session`, and add them to the allowlist test.

*Why a file and not the keychain.* PLAN §9 keeps secrets in the OS keychain. The daemon token is not that kind of secret. It is an ephemeral capability: random per daemon start, worthless once that process exits, and stored where only the user can read it. Whoever can read it already runs as the user, and could read the user's files or start their own daemon anyway. Its value is defence in depth on top of the directory and peer checks (for example, a same-uid process in a container that can see a bind-mounted socket but not the token), and it is the hook scoped tokens need. A keychain entry would need an unlocked keychain or a prompt for a headless daemon and its `coqod mcp` children. It would not work over SSH or in CI, and it would outlive the process it belongs to. This is the same pattern as ssh-agent sockets, X11 cookies and Jupyter's runtime files.

**Spawn on demand.** `connect_or_spawn` connects to the socket. On `ENOENT` or `ECONNREFUSED` it starts `coqod --detached` with `COQO_RUNTIME_DIR` set to the directory it looks in, stdio on `/dev/null` and `/` as working directory, then polls the socket with backoff until it answers (default timeout 10 s). If the child exits with code 3, another client won the race (or an old daemon is still shutting down), so it keeps polling and respawns if needed. Any other early exit is reported. The daemon binary is `$COQO_DAEMON`, else `coqod` next to the running executable, else `coqod` on `PATH`.

**Detaching.** `unsafe_code` is denied, so the client cannot call `setsid` in a `pre_exec` hook. Instead `coqod --detached` calls `setsid()` itself as its first action. That gives it its own session and process group, so closing the UI or its terminal does not signal it. The client does not use `process_group(0)`, because a process-group leader cannot call `setsid`. Detached daemons log to `<data dir>/logs/coqod.log`; `--foreground` logs to stderr. A bare `coqod` starts a detached daemon through the same client code and returns once it answers.

**Compatibility.** The handshake already rejects an incompatible protocol major (ADR 0003); `connect_or_spawn` passes that through as `ProtocolError::IncompatibleVersion`. Different builds with compatible protocols keep working. `Connection::same_build` lets the UI notice an older daemon and offer a restart once it is idle (PLAN §4.3).

**Services.** `coqod` is a library plus a thin binary. A service is a module with `register(router, …) -> Router`, built from the shared `DaemonContext` (paths, data dirs, activity, idle phase, shutdown token). Each service adds one line to `routes::router`, and declares an `Access` for every route it registers (see *Route policy*).

**Idle policy.** `Activity` counts attached connections and *busy guards*. A service holds `ctx.activity.busy("agent session 42")` while it does work that must outlive the UI. With no connections and no busy guards the daemon is idle: after `--idle-stop-services` (default 30 min) the phase becomes `ServicesStopped`, and services watching `ctx.idle` release resources such as language servers. After `--idle-exit` (default 60 min) the daemon exits; `--always-on` disables that. Any connection or busy guard resets the timers. The timers use `tokio::time`, so tests drive them with a paused clock.

**Shutdown.** Idle exit, `daemon/shutdown`, SIGTERM and SIGINT all take the same path: stop accepting, remove the socket, cancel `ctx.shutdown`, give connections 200 ms to flush their last responses, then close them. `daemon/shutdown` refuses while busy guards are held unless `force` is set, and lists them.

## Consequences

- The UI (#27), the CLI and `coqod mcp` attach with one call, and concurrent starts are safe.
- Agent sessions, traces and other long work keep the daemon alive by holding a busy guard; nothing else needs to know about the idle policy.
- The idle settings are command-line flags for now. Reading them (and "always on") from `settings.toml` comes with the settings service.
- Start at login (systemd `--user` unit, launchd agent) and the macOS helper inside the `.app` bundle are not part of this decision. A login-started daemon uses the same lock and socket, so clients need no changes.
- Scoped tokens give least privilege and route calls to the right session. They do not isolate the user's own code from itself: an agent running as the user can read `coqod.token`. Isolating agents would need a sandbox that hides the runtime directory, or a separate uid. That is not part of this decision.
- Scope checks are central and default-deny (#153). A route is reachable by a session token only if its registration says so, and the allowlist test makes every such change visible. The binding is still only as good as the extractor a route declares: a route opened with `Access::check` must look up the workspace itself, and a session route whose params name no workspace or session cannot use `Access::workspace`.
- The other-uid rejection is unit-tested by having the check expect a different uid, because connecting as another user needs root. Every end-to-end test still goes through the real accept-path check.
- Between `spawn` and the daemon's `setsid` there is a window of a few milliseconds in which a signal to the UI's process group would reach the daemon. That is acceptable.
