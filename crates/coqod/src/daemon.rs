// SPDX-License-Identifier: Apache-2.0

//! The daemon skeleton: runtime directory, single-instance lock, socket,
//! accept loop, idle policy and shutdown (PLAN §4.3).

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use coqo_client::{
    DATA_DIR_ENV, DaemonPaths, LOCK_FILE, RuntimeDir, STDERR_FILE, STDERR_PREVIOUS_FILE,
    current_uid,
};
use coqo_protocol::{Server, ServerConfig};
use coqo_storage::DataDirs;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::auth::{self, Auth};
use crate::lifecycle::{Activity, IdlePhase, IdlePolicy, idle_monitor};
use crate::mods::{self, Mods};
use crate::routes;
use crate::workspace::Trash;

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

/// The first pause after an accept error that passes ([`is_transient`]);
/// it doubles with every one in a row, up to [`ACCEPT_BACKOFF_MAX`].
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);
/// The longest pause between accepts while they keep failing.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

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
    /// The session tokens the daemon accepts. Services mint scoped tokens
    /// here ([`Auth::mint_session`]) for `coqod mcp`.
    pub auth: Auth,
    /// Where ACP agents read and write files: set to the daemon's buffers
    /// once they exist ([`routes::router`]), since provider mods activate
    /// first.
    pub agent_fs: coqo_acp_host::FsHandle,
    /// Where ACP agents' `terminal/*` commands run: set to the daemon's
    /// terminals in [`routes::router`], like `agent_fs`.
    pub agent_terminals: coqo_acp_host::TerminalHandle,
    /// Where `fs/delete` puts deleted entries: the OS trash, or a folder
    /// (tests).
    pub trash: Trash,
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

/// Holds the single-instance lock for the daemon's lifetime (the kernel
/// releases it when the process exits, however it exits), the verified
/// runtime directory and the session tokens.
#[derive(Debug)]
pub struct InstanceLock {
    dir: RuntimeDir,
    auth: Auth,
    _file: Flock<File>,
}

impl InstanceLock {
    /// The tokens this daemon accepts.
    pub fn auth(&self) -> &Auth {
        &self.auth
    }
}

/// Create and verify the runtime directory (0700, ours, not a symlink),
/// take the single-instance lock, and write a fresh session token to
/// `coqod.token` (0600).
pub fn acquire(paths: &DaemonPaths) -> Result<InstanceLock, StartError> {
    let dir = RuntimeDir::create(paths.runtime_dir()).map_err(anyhow::Error::new)?;

    let lock_path = paths.lock_file();
    // Never truncate before locking: the file holds the running daemon's pid.
    let file = dir
        .open_file(LOCK_FILE, OFlag::O_RDWR | OFlag::O_CREAT, 0o600)
        .map_err(anyhow::Error::new)?;
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

    // Before the socket exists, so a client that can connect can also
    // read the token of the daemon it reached.
    let auth = Auth::generate().context("cannot generate a session token")?;
    auth::write_token_file(&dir, &auth.daemon_token())?;
    Ok(InstanceLock {
        dir,
        auth,
        _file: file,
    })
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

/// Point this process's stderr at `path`, appended to and created `0600`
/// (with its directory). A detached daemon does this before anything else
/// can fail, so that nothing it writes there is lost (ADR 0007).
pub fn redirect_stderr(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    nix::unistd::dup2_stderr(&file).map_err(io::Error::from)
}

/// With the lock held: keep a non-empty `coqod.stderr` (what the previous
/// daemon wrote, a panic say) as `coqod.stderr.1`, and send stderr to a
/// fresh `coqod.stderr`. Only the lock holder rotates, so a starting daemon
/// never wipes the reason another one just wrote.
pub fn rotate_stderr(logs: &Path) -> io::Result<()> {
    let current = logs.join(STDERR_FILE);
    match fs::metadata(&current) {
        Ok(meta) if meta.len() > 0 => fs::rename(&current, logs.join(STDERR_PREVIOUS_FILE))?,
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    redirect_stderr(&current)
}

/// Whether an `accept` error passes, so the daemon pauses and accepts
/// again instead of exiting: out of file descriptors or memory, a client
/// that gave up before it was accepted, or a signal.
pub fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted | io::ErrorKind::OutOfMemory
    ) || e.raw_os_error().is_some_and(|errno| {
        matches!(
            Errno::from_raw(errno),
            Errno::EMFILE
                | Errno::ENFILE
                | Errno::ECONNABORTED
                | Errno::EINTR
                | Errno::ENOBUFS
                | Errno::ENOMEM
        )
    })
}

/// Test hook: errors the accept loop gets before the listener's own, as if
/// `accept` had returned them.
#[derive(Clone, Debug, Default)]
pub(crate) struct AcceptFaults(Arc<Mutex<VecDeque<io::Error>>>);

impl AcceptFaults {
    #[cfg(test)]
    pub(crate) fn push(&self, e: io::Error) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push_back(e);
    }

    async fn accept(&self, listener: &UnixListener) -> io::Result<UnixStream> {
        let injected = self.0.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
        match injected {
            Some(e) => Err(e),
            None => listener.accept().await.map(|(stream, _)| stream),
        }
    }
}

fn remove_socket(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(socket = %path.display(), error = %e, "cannot remove the socket"),
    }
}

/// Empties the agent handles when serving ends, however it ends (an
/// in-process daemon's serving task can be aborted mid-await).
///
/// [`routes::router`] sets them to the buffers and the terminals, which
/// reach back to the provider mods holding the handles (the terminals
/// through the session manager): an `Arc` cycle. Left in place, it keeps
/// every service alive, and with them the workspaces' file watchers, each
/// holding an inotify instance (at most 128 per user by default).
struct UnlinkAgentHandles(coqo_acp_host::FsHandle, coqo_acp_host::TerminalHandle);

impl Drop for UnlinkAgentHandles {
    fn drop(&mut self) {
        self.0.clear();
        self.1.clear();
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
    serve_with(paths, data, idle_policy, lock, listener, Trash::System).await
}

/// [`serve`], with `fs/delete` putting deleted entries in `trash` (tests
/// pass a folder, so they never touch the user's trash).
pub async fn serve_with(
    paths: DaemonPaths,
    data: DataDirs,
    idle_policy: IdlePolicy,
    lock: InstanceLock,
    listener: UnixListener,
    trash: Trash,
) -> Result<()> {
    serve_inner(
        paths,
        data,
        idle_policy,
        lock,
        listener,
        trash,
        AcceptFaults::default(),
    )
    .await
}

async fn serve_inner(
    paths: DaemonPaths,
    data: DataDirs,
    idle_policy: IdlePolicy,
    lock: InstanceLock,
    listener: UnixListener,
    trash: Trash,
    faults: AcceptFaults,
) -> Result<()> {
    // Nobody can have swapped the directory (it is ours and 0700, and so
    // is its entry in a sticky /tmp), but check before trusting the
    // path-bound socket.
    lock.dir.check_still_at_path()?;
    let auth = lock.auth.clone();
    let activity = Activity::new();
    let shutdown = CancellationToken::new();
    let agent_fs = coqo_acp_host::FsHandle::new();
    let agent_terminals = coqo_acp_host::TerminalHandle::new();
    let _unlink = UnlinkAgentHandles(agent_fs.clone(), agent_terminals.clone());
    let mods = Mods::load(mods::builtins(&data, &agent_fs, &agent_terminals));
    mods.start().await;
    let ctx = DaemonContext {
        paths: paths.clone(),
        data,
        activity: activity.clone(),
        idle: activity.idle_phase(),
        idle_policy,
        shutdown: shutdown.clone(),
        started_at: SystemTime::now(),
        started: Instant::now(),
        mods,
        auth: auth.clone(),
        agent_fs,
        agent_terminals,
        trash,
    };
    let router = routes::router(&ctx)?;
    tracing::debug!(?router, "routes registered");
    let mut config = ServerConfig::new("coqod", BUILD);
    config.authenticator = Some(Arc::new(auth));
    let server = Server::new(router, config);
    let uid = current_uid();

    let mut sigterm = signal(SignalKind::terminate()).context("cannot handle SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("cannot handle SIGINT")?;
    let idle = idle_monitor(activity.clone(), idle_policy);
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
    // Set while accepting pauses after an error that passes.
    let mut paused_until: Option<Instant> = None;
    let mut backoff = ACCEPT_BACKOFF_MIN;
    let result = loop {
        while connections.try_join_next().is_some() {}
        tokio::select! {
            accepted = faults.accept(&listener), if paused_until.is_none() => match accepted {
                Ok(stream) => {
                    backoff = ACCEPT_BACKOFF_MIN;
                    // Before reading a byte: only our own user gets in.
                    if let Err(e) = auth::admit_peer(&stream, uid) {
                        tracing::warn!("{e}");
                        continue;
                    }
                    let guard = activity.connection();
                    let server = server.clone();
                    connections.spawn(async move {
                        let _guard = guard;
                        if let Err(e) = server.serve(stream).await {
                            tracing::warn!(error = %e, "connection ended with an error");
                        }
                    });
                }
                Err(e) if is_transient(&e) => {
                    // Accepting again at once would fail the same way, and
                    // spin: pause, longer each time in a row.
                    tracing::warn!(error = %e, retry_in = ?backoff, "accept failed; retrying");
                    paused_until = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                }
                Err(e) => {
                    tracing::error!(error = %e, "accept failed; stopping");
                    break Err(anyhow::Error::new(e).context("accept failed"));
                }
            },
            () = tokio::time::sleep_until(paused_until.unwrap_or_else(Instant::now)),
                if paused_until.is_some() => paused_until = None,
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
    auth::remove_token_file(&lock.dir);
    shutdown.cancel();
    ctx.mods.shutdown().await;
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    connections.shutdown().await;
    drop(lock);
    tracing::info!("coqod stopped");
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_client::connect;
    use coqo_protocol::ClientConfig;
    use coqo_protocol::daemon::Status;

    const PASSING: [Errno; 6] = [
        Errno::EMFILE,
        Errno::ENFILE,
        Errno::ECONNABORTED,
        Errno::EINTR,
        Errno::ENOBUFS,
        Errno::ENOMEM,
    ];

    /// Launchers find a spawned daemon's stderr file through `coqo-client`,
    /// so it must agree with the daemon on the data directory.
    #[test]
    fn launchers_agree_on_the_data_dir() {
        assert_eq!(
            coqo_client::default_data_dir(),
            DataDirs::platform().ok().map(|d| d.root().to_owned())
        );
    }

    #[test]
    fn transient_accept_errors() {
        for errno in PASSING {
            assert!(is_transient(&io::Error::from(errno)), "{errno}");
        }
        for errno in [Errno::EBADF, Errno::EINVAL, Errno::ENOTSOCK] {
            assert!(!is_transient(&io::Error::from(errno)), "{errno}");
        }
    }

    /// Accept errors that pass leave the daemon serving; one that doesn't
    /// stops it, with the error.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_accept_loop_survives_errors_that_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::at(tmp.path().join("run"));
        let data = DataDirs::at(tmp.path().join("data"));
        let lock = acquire(&paths).unwrap();
        let listener = bind(&paths).unwrap();
        let faults = AcceptFaults::default();
        for errno in PASSING {
            faults.push(io::Error::from(errno));
        }
        let policy = IdlePolicy {
            stop_services_after: None,
            exit_after: None,
        };
        let serving = tokio::spawn(serve_inner(
            paths.clone(),
            data,
            policy,
            lock,
            listener,
            Trash::Folder(tmp.path().join("trash")),
            faults.clone(),
        ));

        let config = || ClientConfig::new("coqod-tests", "test");
        let (client, _) = tokio::time::timeout(Duration::from_secs(10), connect(&paths, config()))
            .await
            .expect("accepted after the errors")
            .unwrap();
        assert!(faults.0.lock().unwrap().is_empty(), "all were returned");
        let status = client.request::<Status>(()).await.unwrap();
        assert_eq!(status.pid, std::process::id());
        assert!(!serving.is_finished());

        // Taken after the next connection is accepted.
        faults.push(io::Error::from(Errno::EBADF));
        let _ = connect(&paths, config()).await;
        let result = tokio::time::timeout(Duration::from_secs(10), serving)
            .await
            .expect("the daemon stops")
            .unwrap();
        let e = result.unwrap_err();
        assert!(format!("{e:#}").starts_with("accept failed"), "{e:#}");
        assert!(!paths.socket().exists());
    }
}
