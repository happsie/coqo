// SPDX-License-Identifier: Apache-2.0

//! Command-line arguments.

use std::ffi::OsString;
use std::time::Duration;

use crate::lifecycle::IdlePolicy;

pub const USAGE: &str = "\
Usage: coqod [MODE] [OPTIONS]
       coqod mcp --workspace ID --session ID

Modes:
  (none)          Start the daemon in the background if it is not running,
                  then print its pid and socket.
  --foreground    Serve in this process, logging to stderr.
  --detached      Serve in a new session (setsid), logging to
                  <data dir>/logs/coqod.log, with stderr (start-up errors,
                  panics) in coqod.stderr beside it. This is what the UI and
                  CLI use when they spawn coqod.
  --version, -V   Print the version.
  --help, -h      Print this help.
  mcp             The coqo-editor MCP server for one agent session, on stdin
                  and stdout (PLAN 7.4). The daemon launches it for agents;
                  it reads a one-time token from COQO_SESSION_TOKEN.

Options:
  --idle-stop-services DUR  Idle time before services such as language
                            servers stop (default 30m).
  --idle-exit DUR           Idle time before the daemon exits (default 60m).
  --always-on               Never exit when idle.

DUR is a number with an optional unit: ms, s (default), m or h.
Idle means no client attached and no busy work (agents, traces).

Environment:
  COQO_RUNTIME_DIR  Socket, lock and token directory (default:
                    $XDG_RUNTIME_DIR/coqo on Linux, ~/Library/Application
                    Support/Coqo/run on macOS). It must be yours, not a
                    symlink, and closed to other users (0700).
  COQO_DATA_DIR     Data directory (default: $XDG_DATA_HOME/coqo,
                    ~/Library/Application Support/Coqo).
  COQO_LOG          Log filter, e.g. `debug` or `coqod=debug,info`.
  COQO_EDITOR_MCP   `0` to leave the coqo-editor MCP server (`coqod mcp`)
                    out of agent sessions, for debugging. On by default.
  COQO_CLAUDE_GUIDE `0` to leave Coqo's operating guide out of Claude
                    Code's system prompt (setting `claude.guide`).
  COQO_CLAUDE_PLUGIN `0` to start Claude sessions without Coqo's Claude
                    Code plugin, the walkthrough skill (setting
                    `claude.plugin`).
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Start,
    Foreground,
    Detached,
    Version,
    Help,
    /// `coqod mcp`: the coqo-editor MCP server.
    Mcp,
}

/// Arguments of `coqod mcp`. The token is not one of them: it comes from
/// `COQO_SESSION_TOKEN`, never `argv` (ADR 0012).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpArgs {
    /// The agent's workspace id.
    pub workspace: String,
    /// The agent session id.
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    pub idle: IdlePolicy,
    /// The idle options as given, passed on when `Start` spawns the daemon.
    pub idle_args: Vec<OsString>,
    /// For [`Mode::Mcp`].
    pub mcp: Option<McpArgs>,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
    let mut mode = None;
    let mut idle = IdlePolicy::default();
    let mut idle_args = Vec::new();
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|a| a == "mcp") {
        args.next();
        return Ok(Args {
            mode: Mode::Mcp,
            idle: IdlePolicy::default(),
            idle_args: Vec::new(),
            mcp: Some(parse_mcp(args)?),
        });
    }
    let mut set_mode = |m: Mode| match mode.replace(m) {
        Some(prev) if prev != m => Err("only one mode can be given".to_owned()),
        _ => Ok(()),
    };
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|a| format!("invalid argument: {}", a.to_string_lossy()))?;
        match arg.as_str() {
            "--foreground" => set_mode(Mode::Foreground)?,
            "--detached" => set_mode(Mode::Detached)?,
            "--version" | "-V" => set_mode(Mode::Version)?,
            "--help" | "-h" => set_mode(Mode::Help)?,
            "--always-on" => {
                idle.exit_after = None;
                idle_args.push(arg.into());
            }
            "--idle-exit" | "--idle-stop-services" => {
                let value = args
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .ok_or_else(|| format!("{arg} needs a duration"))?;
                let duration = parse_duration(&value).map_err(|e| format!("{arg}: {e}"))?;
                if arg == "--idle-exit" {
                    idle.exit_after = Some(duration);
                } else {
                    idle.stop_services_after = Some(duration);
                }
                idle_args.push(arg.into());
                idle_args.push(value.into());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if idle_args.iter().any(|a| a == "--always-on") {
        idle.exit_after = None;
    }
    Ok(Args {
        mode: mode.unwrap_or(Mode::Start),
        idle,
        idle_args,
        mcp: None,
    })
}

fn parse_mcp(args: impl Iterator<Item = OsString>) -> Result<McpArgs, String> {
    let (mut workspace, mut session) = (None, None);
    let mut args = args.map(|a| {
        a.into_string()
            .map_err(|a| format!("invalid argument: {}", a.to_string_lossy()))
    });
    while let Some(arg) = args.next() {
        let arg = arg?;
        let slot = match arg.as_str() {
            "--workspace" => &mut workspace,
            "--session" => &mut session,
            other => return Err(format!("coqod mcp: unknown argument: {other}")),
        };
        let value = args
            .next()
            .transpose()?
            .filter(|v| !v.is_empty() && !v.starts_with("--"))
            .ok_or_else(|| format!("coqod mcp: {arg} needs a value"))?;
        *slot = Some(value);
    }
    Ok(McpArgs {
        workspace: workspace.ok_or("coqod mcp: --workspace is required")?,
        session: session.ok_or("coqod mcp: --session is required")?,
    })
}

/// `90`, `90s`, `500ms`, `30m`, `2h`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration `{s}`"))?;
    let millis = match unit {
        "ms" => Some(n),
        "" | "s" => n.checked_mul(1000),
        "m" => n.checked_mul(60_000),
        "h" => n.checked_mul(3_600_000),
        _ => {
            return Err(format!(
                "invalid duration unit in `{s}` (use ms, s, m or h)"
            ));
        }
    };
    millis
        .map(Duration::from_millis)
        .ok_or_else(|| format!("duration `{s}` is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        for bad in ["", "m", "1d", "-1", "1.5s"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn modes_and_options() {
        let args = parse_strs(&[]).unwrap();
        assert_eq!(args.mode, Mode::Start);
        assert_eq!(args.idle, IdlePolicy::default());

        let args = parse_strs(&["--foreground", "--idle-exit", "2s"]).unwrap();
        assert_eq!(args.mode, Mode::Foreground);
        assert_eq!(args.idle.exit_after, Some(Duration::from_secs(2)));
        assert_eq!(args.idle_args, ["--idle-exit", "2s"]);

        let args = parse_strs(&["--always-on", "--idle-exit", "5s", "--detached"]).unwrap();
        assert_eq!(args.mode, Mode::Detached);
        assert_eq!(args.idle.exit_after, None);

        assert!(parse_strs(&["--foreground", "--detached"]).is_err());
        assert!(parse_strs(&["--idle-exit"]).is_err());
        assert!(parse_strs(&["--bogus"]).is_err());
    }

    #[test]
    fn mcp_mode() {
        let args = parse_strs(&["mcp", "--workspace", "ws-1", "--session", "s-1"]).unwrap();
        assert_eq!(args.mode, Mode::Mcp);
        assert_eq!(
            args.mcp,
            Some(McpArgs {
                workspace: "ws-1".into(),
                session: "s-1".into(),
            })
        );
        assert!(parse_strs(&["mcp", "--workspace", "ws-1"]).is_err());
        assert!(parse_strs(&["mcp", "--workspace", "--session", "s"]).is_err());
        // There is no way to pass a token on the command line.
        let e =
            parse_strs(&["mcp", "--workspace", "w", "--session", "s", "--token", "t"]).unwrap_err();
        assert!(e.contains("--token"), "{e}");
        // `mcp` is a subcommand only in first position.
        assert!(parse_strs(&["--foreground", "mcp"]).is_err());
    }
}
