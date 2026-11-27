//! Spike S3 (issue #10): drive the Claude Agent ACP adapter from Rust.
//!
//! Subcommands:
//! - `acp …`      ACP client: spawn the adapter, initialize, session/new|load|resume
//!   with a stub MCP server injected, one prompt, permission handling, timings.
//! - `mcp-echo`   A tiny stdio MCP server with one `echo` tool (the injected server).
//! - `pty …`      Terminal mode: the official `claude` in a PTY with `--mcp-config`.
//!
//! Throwaway code. Nothing here reads anything under `~/.claude`, and nothing
//! handles credentials: the adapter and the CLI own authentication.

mod acp;
mod mcp_echo;
mod pty;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = args
        .split_first()
        .map_or(("", &[][..]), |(c, r)| (c.as_str(), r));
    let result = match cmd {
        "mcp-echo" => mcp_echo::run(),
        "acp" => tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(acp::run(rest)),
        "pty" => pty::run(rest),
        _ => {
            eprintln!(
                "usage:\n  spike-s3-acp acp --adapter <claude-agent-acp> --cwd <dir> \
                 [--mode new|load|resume] [--session <id>] [--prompt <text>] [--installed-claude]\n  \
                 spike-s3-acp pty --cwd <dir> [--prompt <text>] [--resume <id>]\n  \
                 spike-s3-acp mcp-echo"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Minimal `--flag value` / `--switch` parser.
pub(crate) struct Args<'a>(&'a [String]);

impl<'a> Args<'a> {
    pub(crate) fn new(a: &'a [String]) -> Self {
        Self(a)
    }
    pub(crate) fn value(&self, name: &str) -> Option<&'a str> {
        self.0
            .iter()
            .position(|a| a == name)
            .and_then(|i| self.0.get(i + 1))
            .map(String::as_str)
    }
    pub(crate) fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
}

/// Resolve a program on `PATH` (no shell), like `which`.
pub(crate) fn which(program: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())
    })
}
