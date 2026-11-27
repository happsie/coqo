// SPDX-License-Identifier: Apache-2.0

//! The daemon skeleton: runtime directory, single-instance lock, socket,
//! accept loop, idle policy and shutdown (PLAN §4.3).

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use coqo_client::{DATA_DIR_ENV, DaemonPaths};
use coqo_protocol::{Server, ServerConfig};
use coqo_storage::DataDirs;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::lifecycle::{Activity, IdlePhase, IdlePolicy, idle_monitor};
use crate::mods::{self, Mods};
use crate::routes;

/// The daemon's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The build id sent in the handshake: `$COQO_BUILD` at compile time (set
/// by release builds, e.g. `0.1.0+abc1234`), else the version.
pub const BUILD: &str = match option_env!("COQO_BUILD") {
    Some(build) => build,
    None => VERSION,
};

/// How long open connections get to flush their last responses (such as
/// the answer to `daemon/shutdown`) before the daemon closes them.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);

/// Shared daemon state handed to every service when it registers its
/// routes ([`routes::router`]). Cheap to clone.
#[derive(Clone, Debug)]
pub struct DaemonContext {
    /// Socket and lock location.
    pub paths: DaemonPaths,
    /// Where data lives (`coqo.db`, per-workspace directories).
    pub data: DataDirs,
    /// Attached clients and busy work. Hold [`Activity::busy`] guards for
    /// work that must keep the daemon alive.
    pub activity: Activity,
    /// The idle phase. Services that hold resources (language servers)
    /// release them on [`IdlePhase::ServicesStopped`].
    pub idle: watch::Receiver<IdlePhase>,
    /// The idle policy in effect.
    pub idle_policy: IdlePolicy,
    /// Cancelled when the daemon shuts down.
    pub shutdown: CancellationToken,
    /// When the daemon started.
    pub started_at: SystemTime,
    /// When the daemon started, for uptime.
    pub started: Instant,
    /// The kernel registries and the mods running on them. Startup mods
    /// are active before services register their routes.
    pub mods: Mods,
}

/// Why the daemon could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// Another daemon holds the single-instance lock.
    #[error("another coqod is already running{} (socket {})", pid.map(|p| format!(" (pid {p})")).unwrap_or_default(), socket.display())]
    AlreadyRunning {
        /// Its recorded pid.
        pid: Option<u32>,
        /// The socket it serves.
        socket: PathBuf,
    },
    /// Anything else.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// The data directory: `$COQO_DATA_DIR` if set, else the platform default.
pub fn data_dirs() -> Result<DataDirs> {
    match std::env::var_os(DATA_DIR_ENV) {
        Some(dir) if !dir.is_empty() => Ok(DataDirs::at(dir)),
        _ => DataDirs::platform().context("cannot determine the data directory"),
    }
}

/// Holds the single-instance lock for the daemon's lifetime. The kernel
/// releases it when the process exits, however it exits.
#[derive(Debug)]
pub struct InstanceLock {
    _file: Flock<File>,
}

/// Create the runtime directory (0700) and take the single-instance lock.
pub fn acquire(paths: &DaemonPaths) -> Result<InstanceLock, StartError> {
    let dir = paths.runtime_dir();
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    // `recursive` leaves an existing directory's mode alone. Ownership and
    // peer-credential checks come with socket hardening (#21).
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", dir.display()))?;

    let lock_path = paths.lock_file();
    // Never truncate before locking: the file holds the running daemon's pid.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    let mut file = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => lock,
        Err((_, Errno::EWOULDBLOCK)) => {
            return Err(StartError::AlreadyRunning {
                pid: paths.recorded_pid().ok().flatten(),
                socket: paths.socket(),
            });
        }
        Err((_, errno)) => {
            return Err(anyhow::Error::new(io::Error::from(errno))
                .context(format!("cannot lock {}", lock_path.display()))
                .into());
        }
    };
    file.set_len(0)
        .and_then(|()| writeln!(&mut *file, "{}", std::process::id()))
        .with_context(|| format!("cannot write {}", lock_path.display()))?;
    Ok(InstanceLock { _file: file })
}

/// Bind the socket, replacing a stale one. Only call with the lock held:
/// a live daemon would hold it, so whatever socket file is there is stale.
pub fn bind(paths: &DaemonPaths) -> Result<UnixListener> {
    let socket = paths.socket();
    match fs::remove_file(&socket) {
        Ok(()) => tracing::info!(socket = %socket.display(), "removed a stale socket"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("cannot remove stale {}", socket.display()));
        }
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict {}", socket.display()))?;
    Ok(listener)
}

fn remove_socket(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(socket = %path.display(), error = %e, "cannot remove the socket"),
    }
}

/// Serve until idle exit, `daemon/shutdown`, SIGTERM or SIGINT. Takes the
/// lock and the bound listener from [`acquire`] and [`bind`].
pub async fn serve(
    paths: DaemonPaths,
    data: DataDirs,
    idle_policy: IdlePolicy,
    lock: InstanceLock,
    listener: UnixListener,
) -> Result<()> {
    let activity = Activity::new();
    let (idle_tx, idle_rx) = watch::channel(IdlePhase::Active);
    let shutdown = CancellationToken::new();
    let mods = Mods::load(mods::builtins());
    mods.start().await;
    let ctx = DaemonContext {
        paths: paths.clone(),
        data,
        activity: activity.clone(),
        idle: idle_rx,
        idle_policy,
        shutdown: shutdown.clone(),
        started_at: SystemTime::now(),
        started: Instant::now(),
        mods,
    };
    let router = routes::router(&ctx)?;
    tracing::debug!(?router, "routes registered");
    let server = Server::new(router, ServerConfig::new("coqod", BUILD));

    let mut sigterm = signal(SignalKind::terminate()).context("cannot handle SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot handle SIGINT")?;
    let idle = idle_monitor(activity.clone(), idle_policy, idle_tx);
    tokio::pin!(idle);

    tracing::info!(
        version = VERSION,
        build = BUILD,
        pid = std::process::id(),
        socket = %paths.socket().display(),
        data = %ctx.data.root().display(),
        ?idle_policy,
        "coqod listening"
    );

    let mut connections = JoinSet::new();
    let result = loop {
        while connections.try_join_next().is_some() {}
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let guard = activity.connection();
                    let server = server.clone();
                    connections.spawn(async move {
                        let _guard = guard;
                        if let Err(e) = server.serve(stream).await {
                            tracing::warn!(error = %e, "connection ended with an error");
                        }
                    });
                }
                Err(e) => break Err(anyhow::Error::new(e).context("accept failed")),
            },
            () = &mut idle => break Ok(()),
            () = shutdown.cancelled() => {
                tracing::info!("shutdown requested");
                break Ok(());
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM; shutting down");
                break Ok(());
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT; shutting down");
                break Ok(());
            }
        }
    };

    // No new clients from here on; then let services and open connections
    // wind down.
    drop(listener);
    remove_socket(&paths.socket());
    shutdown.cancel();
    ctx.mods.shutdown().await;
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    connections.shutdown().await;
    drop(lock);
    tracing::info!("coqod stopped");
    result
}
