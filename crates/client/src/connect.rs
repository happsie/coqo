// SPDX-License-Identifier: Apache-2.0

//! Connect to the daemon, or spawn it and connect.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use coqo_protocol::{Client, ClientConfig, Notifications, ProtocolError, Welcome};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::paths::{DAEMON_BIN_ENV, DaemonPaths, NoRuntimeDir, RUNTIME_DIR_ENV};
use crate::runtime_dir::{RuntimeDir, RuntimeDirError, current_uid};

/// The flag [`connect_or_spawn`] passes to `coqod`: start a new session
/// (`setsid`), log to a file, and serve until idle.
pub const DETACHED_FLAG: &str = "--detached";

/// `coqod`'s exit code when another daemon already holds the
/// single-instance lock.
pub const EXIT_ALREADY_RUNNING: i32 = 3;

/// Why a client could not reach the daemon.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// No runtime directory could be determined.
    #[error(transparent)]
    NoRuntimeDir(#[from] NoRuntimeDir),
    /// The runtime directory or the token file cannot be trusted: a
    /// symlink, another user's, or open to other users.
    #[error(transparent)]
    RuntimeDir(#[from] RuntimeDirError),
    /// The process listening on the socket runs as another user. Nothing
    /// was sent to it.
    #[error("{} is served by uid {uid}, not by you; refusing to talk to it", path.display())]
    ForeignDaemon {
        /// The socket.
        path: PathBuf,
        /// The listener's uid.
        uid: u32,
    },
    /// Nothing is listening on the socket (no daemon, or a stale socket
    /// left by a crashed one).
    #[error("coqod is not running (nothing listens on {})", .0.display())]
    NotRunning(PathBuf),
    /// Connecting failed for another reason (permissions, path too long, …).
    #[error("cannot connect to {}: {source}", path.display())]
    Io {
        /// The socket.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The `coqod` binary could not be started.
    #[error("cannot start {}: {source}", program.display())]
    Spawn {
        /// The binary.
        program: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The spawned daemon exited before its socket came up.
    #[error("coqod exited during startup ({status}); see its log in the data directory")]
    DaemonExited {
        /// How it exited.
        status: ExitStatus,
    },
    /// The spawned daemon did not open its socket in time.
    #[error("coqod did not open {} within {timeout:?}", path.display())]
    Timeout {
        /// The socket.
        path: PathBuf,
        /// [`ConnectOptions::ready_timeout`].
        timeout: Duration,
    },
    /// The handshake or a request failed. An incompatible daemon is
    /// [`ProtocolError::IncompatibleVersion`]: restart it (PLAN §4.3).
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// How to start `coqod` when it is not running.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// The `coqod` binary.
    pub program: PathBuf,
    /// Extra arguments after [`DETACHED_FLAG`] (for example idle settings).
    pub args: Vec<OsString>,
    /// Extra environment variables for the daemon.
    pub env: Vec<(OsString, OsString)>,
}

impl SpawnOptions {
    /// Spawn `program` with no extra arguments.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Find `coqod`: `$COQO_DAEMON` if set, else a `coqod` next to the
    /// running executable (the installed layout), else `coqod` on `PATH`.
    pub fn locate() -> Self {
        let from_env = std::env::var_os(DAEMON_BIN_ENV)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        let sibling = || {
            let exe = std::env::current_exe().ok()?;
            let candidate = exe.parent()?.join("coqod");
            candidate.is_file().then_some(candidate)
        };
        Self::new(
            from_env
                .or_else(sibling)
                .unwrap_or_else(|| PathBuf::from("coqod")),
        )
    }
}

/// Settings for [`connect_or_spawn`].
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// The handshake identity and protocol settings.
    pub client: ClientConfig,
    /// How to start the daemon.
    pub spawn: SpawnOptions,
    /// How long to wait for a spawned daemon's socket.
    pub ready_timeout: Duration,
}

impl ConnectOptions {
    /// Defaults: [`SpawnOptions::locate`] and a 10 s ready timeout.
    pub fn new(client: ClientConfig) -> Self {
        Self {
            client,
            spawn: SpawnOptions::locate(),
            ready_timeout: Duration::from_secs(10),
        }
    }
}

/// An attached client.
#[derive(Debug)]
pub struct Connection {
    /// Requests and subscriptions.
    pub client: Client,
    /// Notifications from the daemon that are not subscription events.
    pub notifications: Notifications,
    /// Whether this call started a daemon. It may still be attached to
    /// another client's daemon if both spawned at once.
    pub spawned: bool,
}

impl Connection {
    /// The daemon's handshake answer.
    pub fn welcome(&self) -> &Welcome {
        self.client.welcome()
    }

    /// Whether the daemon runs the same build as this client. Different
    /// builds work together when the protocol is compatible (the handshake
    /// already checked that); a UI may still offer to restart an older
    /// daemon once it is idle (PLAN §4.3).
    pub fn same_build(&self, build: &str) -> bool {
        self.welcome().build == build
    }
}

/// Attach to a running daemon.
///
/// Before sending anything this checks that the runtime directory is the
/// user's own and private ([`RuntimeDir`]) and that the process on the
/// socket runs as the same user. Unless `config.token` is already set (a
/// scoped token, say), it then sends the daemon's session token from
/// `coqod.token`, read after connecting so it belongs to the daemon that
/// accepted the connection.
///
/// Returns [`ConnectError::NotRunning`] when no daemon listens on the
/// socket, and [`ProtocolError::Unauthorized`] (inside
/// [`ConnectError::Protocol`]) when the daemon rejects the token.
pub async fn connect(
    paths: &DaemonPaths,
    mut config: ClientConfig,
) -> Result<(Client, Notifications), ConnectError> {
    let path = paths.socket();
    let Some(dir) = RuntimeDir::open(paths.runtime_dir())? else {
        return Err(ConnectError::NotRunning(path));
    };
    let stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(e) if is_not_running(&e) => return Err(ConnectError::NotRunning(path)),
        Err(source) => return Err(ConnectError::Io { path, source }),
    };
    let uid = stream
        .peer_cred()
        .map_err(|source| ConnectError::Io {
            path: path.clone(),
            source,
        })?
        .uid();
    if uid != current_uid() {
        return Err(ConnectError::ForeignDaemon { path, uid });
    }
    if config.token.is_none() {
        // `None`: an older daemon without tokens. A daemon that needs one
        // answers with a clear `Unauthorized`.
        config.token = dir.read_token()?;
    }
    Ok(Client::connect(stream, config).await?)
}

fn is_not_running(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Attach to the daemon, starting it first if it is not running.
///
/// A spawned daemon gets [`DETACHED_FLAG`] and `COQO_RUNTIME_DIR` set to
/// `paths`, so it listens where this client looks. It calls `setsid` itself
/// and so leaves this process's session and process group: closing the UI
/// or its terminal does not stop it.
///
/// Concurrent callers are safe: the daemon's single-instance lock lets one
/// win, and the others attach to it.
pub async fn connect_or_spawn(
    paths: &DaemonPaths,
    options: &ConnectOptions,
) -> Result<Connection, ConnectError> {
    match connect(paths, options.client.clone()).await {
        Ok((client, notifications)) => {
            return Ok(Connection {
                client,
                notifications,
                spawned: false,
            });
        }
        Err(ConnectError::NotRunning(_)) => {}
        Err(e) => return Err(e),
    }

    let deadline = Instant::now() + options.ready_timeout;
    let mut child = spawn(paths, &options.spawn)?;
    let mut delay = Duration::from_millis(5);
    loop {
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(100));

        match connect(paths, options.client.clone()).await {
            // Dropping `child` does not kill it; tokio reaps it when it exits.
            Ok((client, notifications)) => {
                return Ok(Connection {
                    client,
                    notifications,
                    spawned: true,
                });
            }
            Err(ConnectError::NotRunning(_)) => {}
            Err(e) => return Err(e),
        }

        if let Some(status) = child.try_wait().map_err(|source| ConnectError::Spawn {
            program: options.spawn.program.clone(),
            source,
        })? {
            if status.code() != Some(EXIT_ALREADY_RUNNING) {
                return Err(ConnectError::DaemonExited { status });
            }
            // Another daemon holds the lock: it is starting (we attach to it
            // on the next try) or shutting down (it is gone soon, so try to
            // start ours again).
            tracing::debug!("another coqod holds the lock; retrying");
            child = spawn(paths, &options.spawn)?;
        }

        if Instant::now() >= deadline {
            return Err(ConnectError::Timeout {
                path: paths.socket(),
                timeout: options.ready_timeout,
            });
        }
    }
}

fn spawn(paths: &DaemonPaths, options: &SpawnOptions) -> Result<Child, ConnectError> {
    tracing::info!(program = %options.program.display(), dir = %paths.runtime_dir().display(), "starting coqod");
    Command::new(&options.program)
        .arg(DETACHED_FLAG)
        .args(&options.args)
        .env(RUNTIME_DIR_ENV, paths.runtime_dir())
        .envs(options.env.iter().map(|(k, v)| (k, v)))
        // Don't pin the client's working directory (it may be unmounted).
        .current_dir(Path::new("/"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .map_err(|source| ConnectError::Spawn {
            program: options.program.clone(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private, not yet created runtime directory in a fresh temp dir.
    fn run_dir() -> (tempfile::TempDir, DaemonPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::at(tmp.path().join("run"));
        (tmp, paths)
    }

    fn options(program: &str) -> ConnectOptions {
        let mut options = ConnectOptions::new(ClientConfig::new("test", "test"));
        options.spawn = SpawnOptions::new(program);
        options.ready_timeout = Duration::from_secs(5);
        options
    }

    #[tokio::test]
    async fn no_socket_is_not_running() {
        let (_tmp, paths) = run_dir();
        let err = connect(&paths, ClientConfig::new("test", "test"))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::NotRunning(_)), "{err}");
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        let err = connect(&paths, ClientConfig::new("test", "test"))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::NotRunning(_)), "{err}");
    }

    #[tokio::test]
    async fn stale_socket_is_not_running() {
        let (_tmp, paths) = run_dir();
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(paths.socket()).unwrap());
        assert!(paths.socket().exists());
        let err = connect(&paths, ClientConfig::new("test", "test"))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::NotRunning(_)), "{err}");
    }

    #[tokio::test]
    async fn missing_binary_is_a_spawn_error() {
        let (_tmp, paths) = run_dir();
        let err = connect_or_spawn(&paths, &options("/nonexistent/coqod"))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn daemon_that_exits_is_reported() {
        let (_tmp, paths) = run_dir();
        let err = connect_or_spawn(&paths, &options("false"))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::DaemonExited { .. }), "{err}");
    }

    #[tokio::test]
    async fn an_insecure_runtime_dir_is_refused_before_connecting() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_tmp, paths) = run_dir();
        std::fs::create_dir(paths.runtime_dir()).unwrap();
        std::fs::set_permissions(paths.runtime_dir(), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        // Something listens, but the client must not talk to it.
        let _listener = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
        let err = connect(&paths, ClientConfig::new("test", "test"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ConnectError::RuntimeDir(RuntimeDirError::TooOpen { .. })
            ),
            "{err}"
        );
        // connect_or_spawn does not spawn over it either.
        let err = connect_or_spawn(&paths, &options("/nonexistent/coqod"))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::RuntimeDir(_)), "{err}");
    }

    #[tokio::test]
    async fn the_token_file_is_sent_in_the_hello() {
        use coqo_protocol::{Authenticator, Router, Scope, Server, ServerConfig, Unauthorized};
        use std::os::unix::fs::PermissionsExt as _;

        #[derive(Debug)]
        struct Expect(coqo_protocol::SessionToken);
        impl Authenticator for Expect {
            fn authenticate(
                &self,
                token: Option<&coqo_protocol::SessionToken>,
            ) -> Result<Scope, Unauthorized> {
                match token {
                    Some(t) if *t == self.0 => Ok(Scope::Daemon),
                    _ => Err(Unauthorized("bad token".into())),
                }
            }
        }

        let (_tmp, paths) = run_dir();
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        let mut config = ServerConfig::new("test-daemon", "test");
        config.authenticator = Some(std::sync::Arc::new(Expect(
            coqo_protocol::SessionToken::new("right"),
        )));
        let server = Server::new(Router::new(), config);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move { server.serve(stream).await });
            }
        });

        // No token file: no token, so the server says why.
        let err = connect(&paths, ClientConfig::new("test", "test"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ConnectError::Protocol(ProtocolError::Unauthorized(_))),
            "{err}"
        );

        std::fs::write(paths.token_file(), "wrong\n").unwrap();
        std::fs::set_permissions(paths.token_file(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let err = connect(&paths, ClientConfig::new("test", "test"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ConnectError::Protocol(ProtocolError::Unauthorized(_))),
            "{err}"
        );

        std::fs::write(paths.token_file(), "right\n").unwrap();
        connect(&paths, ClientConfig::new("test", "test"))
            .await
            .expect("the right token is accepted");

        // An explicit token wins over the file.
        let mut config = ClientConfig::new("test", "test");
        config.token = Some(coqo_protocol::SessionToken::new("explicit"));
        assert!(connect(&paths, config).await.is_err());
    }
}
