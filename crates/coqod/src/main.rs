// SPDX-License-Identifier: Apache-2.0

//! `coqod`: the Coqo background daemon (PLAN §4.3). The daemon itself is
//! the `coqod` library (`src/lib.rs`); this is only its command line.

use std::process::ExitCode;

use anyhow::{Context as _, Result};
use coqo_client::{
    ConnectOptions, DETACHED_FLAG, DaemonPaths, EXIT_ALREADY_RUNNING, LOG_DIR, LOG_FILE,
    STDERR_FILE, SpawnOptions, connect_or_spawn, stderr_prefix,
};
use coqo_kernel::logging::{self, LogConfig, LogTarget};
use coqo_protocol::ClientConfig;

use coqod::cli::{self, Args, Mode};
use coqod::{daemon, editor_mcp};

fn main() -> ExitCode {
    // A detached daemon's stderr is a file shared with other daemons (ADR
    // 0007): tag what it writes there, from the first line on, so the
    // launcher that started it can tell why it exited.
    let detached = std::env::args_os().any(|a| a == DETACHED_FLAG);
    if detached {
        tag_panics();
    }
    let args = match cli::parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(e) if detached => {
            report(detached, &e);
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("coqod: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let result = match args.mode {
        Mode::Version => {
            println!("coqod {}", daemon::VERSION);
            return ExitCode::SUCCESS;
        }
        Mode::Help => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Mode::Start => start(&args),
        Mode::Foreground | Mode::Detached => run(&args),
        Mode::Mcp => mcp(&args),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            let error = format!("{e:#}");
            tracing::error!(pid = std::process::id(), error, "coqod failed");
            report(detached, &error);
            ExitCode::FAILURE
        }
    }
}

/// Print a fatal error on stderr: tagged with the pid when detached.
fn report(detached: bool, error: &str) {
    if detached {
        eprintln!("{}{error}", stderr_prefix(std::process::id()));
    } else {
        eprintln!("coqod: {error}");
    }
}

/// Panics go to the log and, tagged, to stderr, instead of only to an
/// untagged stderr line.
fn tag_panics() {
    let prefix = stderr_prefix(std::process::id());
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "coqod panicked");
        eprintln!("{prefix}{info}");
        let backtrace = std::backtrace::Backtrace::capture();
        if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
            eprintln!("{backtrace}");
        }
    }));
}

/// `--foreground` / `--detached`: serve until idle or told to stop.
fn run(args: &Args) -> Result<ExitCode> {
    let detached = args.mode == Mode::Detached;
    if detached {
        // Leave the spawning UI's session and process group, so closing it
        // (or its terminal) does not signal us. EPERM means we already lead
        // a process group (started from a shell), which is detached enough.
        let _ = nix::unistd::setsid();
    }
    let paths = DaemonPaths::from_env()?;
    let data = daemon::data_dirs()?;
    let logs = data.root().join(LOG_DIR);
    let (target, redirected) = if detached {
        // Launchers already point our stderr at coqod.stderr; do it here
        // too, for a daemon started some other way (`2>/dev/null`).
        let redirected = daemon::redirect_stderr(&logs.join(STDERR_FILE));
        (LogTarget::File(logs.join(LOG_FILE)), Some(redirected))
    } else {
        (LogTarget::Stderr, None)
    };
    logging::init(&LogConfig {
        target,
        ..LogConfig::default()
    })
    .context("cannot set up logging")?;
    if let Some(Err(e)) = redirected {
        tracing::warn!(error = %e, "cannot send stderr to {STDERR_FILE}");
    }

    let lock = match daemon::acquire(&paths) {
        Ok(lock) => lock,
        Err(e @ daemon::StartError::AlreadyRunning { .. }) => {
            // An expected outcome for a spawned daemon (the launcher attaches
            // to the running one), so not on a detached daemon's stderr.
            tracing::warn!("{e}");
            if !detached {
                eprintln!("coqod: {e}");
            }
            return Ok(ExitCode::from(EXIT_ALREADY_RUNNING as u8));
        }
        Err(daemon::StartError::Other(e)) => return Err(e),
    };
    if detached && let Err(e) = daemon::rotate_stderr(&logs) {
        tracing::warn!(error = %e, "cannot rotate {STDERR_FILE}");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("coqod")
        .build()
        .context("cannot start the async runtime")?;
    runtime.block_on(async {
        let listener = daemon::bind(&paths)?;
        daemon::serve(paths, data, args.idle, lock, listener).await
    })?;
    Ok(ExitCode::SUCCESS)
}

/// `coqod mcp`: the coqo-editor MCP server on stdin and stdout. stdout is
/// the MCP channel, so logs go to stderr (agents keep it in their MCP logs).
fn mcp(args: &Args) -> Result<ExitCode> {
    let mcp = args.mcp.as_ref().context("coqod mcp needs its arguments")?;
    logging::init(&LogConfig {
        default_directive: "warn".to_owned(),
        target: LogTarget::Stderr,
        ..LogConfig::default()
    })
    .context("cannot set up logging")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let exit = runtime.block_on(editor_mcp::run(mcp));
    // Do not wait for the blocking stdin read: with the daemon gone the
    // agent may keep stdin open, and dropping the runtime would wait for it
    // forever.
    runtime.shutdown_background();
    let exit = exit?;
    tracing::debug!(?exit, "coqod mcp stopped");
    Ok(match exit {
        coqo_mcp::Exit::StdinClosed => ExitCode::SUCCESS,
        coqo_mcp::Exit::DaemonGone => {
            eprintln!(
                "coqod mcp: the daemon connection closed (coqod stopped, or the agent session ended)"
            );
            ExitCode::FAILURE
        }
    })
}

/// No mode: make sure a daemon is running, starting one in the background.
fn start(args: &Args) -> Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    runtime.block_on(async {
        let paths = DaemonPaths::from_env()?;
        let mut options = ConnectOptions::new(ClientConfig::new("coqod-start", daemon::BUILD));
        let mut spawn = SpawnOptions::new(std::env::current_exe().context("cannot find coqod")?);
        spawn.args = args.idle_args.clone();
        options.spawn = spawn;
        let conn = connect_or_spawn(&paths, &options).await?;
        let status = conn
            .client
            .request::<coqo_protocol::daemon::Status>(())
            .await?;
        println!(
            "coqod {} {} (pid {}, socket {})",
            status.version,
            if conn.spawned {
                "started"
            } else {
                "already running"
            },
            status.pid,
            paths.socket().display()
        );
        Ok(ExitCode::SUCCESS)
    })
}
