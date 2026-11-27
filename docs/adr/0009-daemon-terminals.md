# 9. Daemon terminals: PTY and emulator in `coqod`, the UI mirrors rows

- **Status:** Accepted
- **Date:** 2026-11-27

## Context

Issue #46 needs terminals that survive the UI closing, because Claude terminal mode (#57, ADR 0005) runs the user's own `claude` in one. PLAN §3 picks `alacritty_terminal` and `portable-pty`, and PLAN §6 lists `terminal` as a built-in mod. The daemon now hosts built-in mods (#147), but a mod can't yet register protocol routes or contribute a dock tab kind through `coqo-mod-api`. The workbench (ADR 0004) was built outside a mod for the same reason.

## Decision

- **The daemon owns everything stateful.** `coqod::terminal` opens the PTY with `portable-pty` (the child gets its own session and process group), feeds the output into an `alacritty_terminal::Term` (10 000 lines of scrollback), and picks OSC 7 (cwd) and OSC 133;D (exit code) out of the byte stream. Each terminal runs three blocking threads: a reader, a writer and a flusher.
- **The wire carries row diffs, not grids.** The flusher wakes on damage. It renders the damaged rows as sparse style runs, compares them with what it last sent, and publishes only the rows that changed, together with the cursor and modes, as a `terminal/output` frame. It does this at most every 16 ms, and the first change after a quiet spell goes out immediately. Every publish happens under the terminal's lock, so a `terminal/snapshot` and its `seq` agree exactly. Clients reattach with snapshot-then-subscribe-since-seq (ADR 0003).
- **Scrollback stays in the daemon.** `terminal/scroll` moves the terminal's one viewport. A client never holds history, which keeps both the client mirror and the wire simple. The cost is that every attached view shares one viewport.
- **Lifecycle.** A running terminal holds an `Activity` busy guard, so the daemon doesn't idle-exit under it. Closing a terminal sends SIGHUP to its process group and to the PTY's foreground job (a shell starts each job in a separate group), then SIGKILL after 1.5 s. Daemon shutdown closes every terminal. If the daemon dies outright, the kernel hangs up the PTYs. The UI closes a terminal when its last tab is closed (the new `TabItem::on_close` hook), but never when the app quits.
- **Rust API for other daemon features.** `Terminals::spawn(SpawnSpec::command(..).args(..).cwd(..).env(..))` returns a `Terminal` with `write`, `resize`, `snapshot`, `subscribe`, `wait` and `kill`. Claude terminal mode (#57) uses it.
- **ACP agents' commands (#185).** `AgentTerminalService` implements the ACP host's `AgentTerminals` on the same `Terminals`. `routes::router` builds that set once and shares it between the `terminal/*` routes and agents, so an agent's command is an ordinary listed terminal that the user can open, watch and close. For agents, `Terminal::text` reads the grid (scrollback first) as plain text, and `Terminal::terminate` kills the process like `kill` but keeps subscriptions open, so an open tab shows the exit.
- **Ids are unique across daemon runs.** `Terminals::for_daemon` starts its ids at the start time in milliseconds × 1000. Without that, a saved layout or a chat transcript that names a terminal of an earlier run would reach whatever new terminal got the same id.
- **Not a mod yet.** The daemon side is `crates/coqod/src/terminal/` and the UI side is `crates/coqo/src/terminal/`, with wire types in `coqo_protocol::methods::terminal`. The code moves to `mods/terminal` once the mod API can contribute a daemon service and a tab kind.

## Consequences

- Terminals survive UI restarts. The acceptance check (start `top`, detach, reattach, still live) is a daemon test over the protocol.
- A frame costs roughly one changed row (the cursor row) when typing, and the visible screen when output scrolls: about 10 KB of JSON per frame at 120×40, at most 60 frames a second.
- Shared viewports and the absence of mouse reporting, IME and injected shell-integration scripts are known gaps, tracked as follow-ups.
- PLAN §6 still lists `terminal` as a mod. This ADR records why it isn't one yet.
