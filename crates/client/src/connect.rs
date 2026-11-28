// SPDX-License-Identifier: Apache-2.0

//! Connect to the daemon, or spawn it and connect.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use coqo_protocol::{Client, ClientConfig, Notifications, ProtocolError, Welcome};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::paths::{
    DAEMON_BIN_ENV, DATA_DIR_ENV, DaemonPaths, LOG_DIR, NoRuntimeDir, RUNTIME_DIR_ENV, STDERR_FILE,
    STDERR_PREVIOUS_FILE, data_dir, stderr_prefix,
};
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
    #[error("coqod exited during startup ({status}){}", exit_detail(reason.as_deref(), log.as_deref()))]
    DaemonExited {
        /// How it exited.
        status: ExitStatus,
        /// Why: what it wrote to stderr before exiting (its last lines,
        /// without the [`stderr_prefix`](crate::stderr_prefix)), or why its
        /// stderr could not be captured.
        reason: Option<String>,
        /// Where its stderr went ([`STDERR_FILE`]), for more.
        log: Option<PathBuf>,
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

fn exit_detail(reason: Option<&str>, log: Option<&Path>) -> String {
    match (reason, log) {
        (Some(reason), _) => format!(": {reason}"),
        (None, Some(log)) => format!("; see {}", log.display()),
        (None, None) => "; see its log in the data directory".to_owned(),
    }
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
    let uid = match stream.peer_cred() {
        Ok(cred) => cred.uid(),
        Err(e) if is_not_running(&e) => return Err(ConnectError::NotRunning(path)),
        Err(source) => return Err(ConnectError::Io { path, source }),
    };
    if uid != current_uid() {
        return Err(ConnectError::ForeignDaemon { path, uid });
    }
    if config.token.is_none() {
        // `None`: an older daemon without tokens. A daemon that needs one
        // answers with a clear `Unauthorized`.
        config.token = dir.read_token()?;
    }
    match Client::connect(stream, config).await {
        Ok(client) => Ok(client),
        // A non-blocking connect to a socket nobody listens on can report
        // success; the refusal then shows up as a reset, a broken pipe or EOF
        // on the first write or read. That is a stale socket, not a daemon.
        Err(ProtocolError::Io(e)) if is_not_running(&e) => Err(ConnectError::NotRunning(path)),
        Err(e) => Err(e.into()),
    }
}

fn is_not_running(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
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

        let exited = child
            .process
            .try_wait()
            .map_err(|source| ConnectError::Spawn {
                program: options.spawn.program.clone(),
                source,
            })?;
        if let Some(status) = exited {
            if status.code() != Some(EXIT_ALREADY_RUNNING) {
                let reason = child.stderr.reason(child.pid);
                tracing::warn!(
                    %status,
                    reason = reason.as_deref().unwrap_or("unknown"),
                    "coqod exited during startup"
                );
                return Err(ConnectError::DaemonExited {
                    status,
                    reason,
                    log: child.stderr.path,
                });
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

/// A daemon this client started.
struct Spawned {
    process: Child,
    /// Taken right away: the pid is gone once the child is reaped.
    pid: Option<u32>,
    stderr: StderrCapture,
}

fn spawn(paths: &DaemonPaths, options: &SpawnOptions) -> Result<Spawned, ConnectError> {
    tracing::info!(program = %options.program.display(), dir = %paths.runtime_dir().display(), "starting coqod");
    let (stderr, capture) = StderrCapture::open(options);
    let process = Command::new(&options.program)
        .arg(DETACHED_FLAG)
        .args(&options.args)
        .env(RUNTIME_DIR_ENV, paths.runtime_dir())
        .envs(options.env.iter().map(|(k, v)| (k, v)))
        // Don't pin the client's working directory (it may be unmounted).
        .current_dir(Path::new("/"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .kill_on_drop(false)
        .spawn()
        .map_err(|source| ConnectError::Spawn {
            program: options.program.clone(),
            source,
        })?;
    Ok(Spawned {
        pid: process.id(),
        process,
        stderr: capture,
    })
}

/// Where a spawned daemon's stderr goes: `<data dir>/logs/coqod.stderr`,
/// appended to (other daemons may be writing to it too). So whatever it
/// writes before it can log, or outside the log, is kept (ADR 0007).
#[derive(Debug, Default)]
struct StderrCapture {
    /// [`STDERR_FILE`], if it could be opened.
    path: Option<PathBuf>,
    /// Why it could not be opened (the daemon then gets `/dev/null`).
    error: Option<String>,
}

/// At most this much of the end of a stderr file is read.
const STDERR_TAIL_BYTES: u64 = 64 * 1024;
/// At most this many of a daemon's last stderr lines make the reason.
const REASON_LINES: usize = 20;

impl StderrCapture {
    fn open(options: &SpawnOptions) -> (Stdio, Self) {
        // The daemon's `COQO_DATA_DIR`: from the spawn options, else ours.
        let dir = options
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == DATA_DIR_ENV)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var_os(DATA_DIR_ENV));
        let Some(logs) = data_dir(dir.as_deref()).map(|d| d.join(LOG_DIR)) else {
            return (Stdio::null(), Self::default());
        };
        let path = logs.join(STDERR_FILE);
        let opened = fs::create_dir_all(&logs).and_then(|()| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&path)
        });
        match opened {
            Ok(file) => (
                file.into(),
                Self {
                    path: Some(path),
                    error: None,
                },
            ),
            Err(e) => {
                let error = format!("cannot open {}: {e}", path.display());
                tracing::warn!(%error, "coqod's stderr is not captured");
                (
                    Stdio::null(),
                    Self {
                        path: None,
                        error: Some(error),
                    },
                )
            }
        }
    }

    /// Why the daemon with `pid` exited: its lines in the stderr file (and
    /// in the previous one, in case a starting daemon rotated it meanwhile),
    /// else why there is no file.
    fn reason(&self, pid: Option<u32>) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(format!("its output was not captured ({error})"));
        }
        let (path, pid) = (self.path.as_ref()?, pid?);
        let mut lines = Vec::new();
        for file in [path.with_file_name(STDERR_PREVIOUS_FILE), path.clone()] {
            if let Ok(text) = read_tail(&file) {
                lines.extend(lines_of(&text, pid).map(str::to_owned));
            }
        }
        let skip = lines.len().saturating_sub(REASON_LINES);
        let reason = lines[skip..].join("\n");
        (!reason.trim().is_empty()).then_some(reason)
    }
}

fn read_tail(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The lines daemon `pid` wrote to a shared stderr file: those that start
/// with its [`stderr_prefix`] (without it), and the untagged lines that
/// follow them, such as the rest of a multi-line panic message.
fn lines_of(text: &str, pid: u32) -> impl Iterator<Item = &str> {
    let prefix = stderr_prefix(pid);
    let mut ours = false;
    text.lines().filter_map(move |line| {
        if let Some(rest) = line.strip_prefix(prefix.as_str()) {
            ours = true;
            Some(rest)
        } else if is_tagged(line) {
            ours = false;
            None
        } else {
            ours.then_some(line)
        }
    })
}

/// Whether `line` starts with some daemon's [`stderr_prefix`].
fn is_tagged(line: &str) -> bool {
    line.strip_prefix("coqod[")
        .and_then(|rest| rest.split_once("]: "))
        .is_some_and(|(pid, _)| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
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

    /// Spawn `program`, with the daemon's data (and so its stderr file)
    /// under `tmp`, never the user's.
    fn options(program: impl Into<PathBuf>, tmp: &tempfile::TempDir) -> ConnectOptions {
        let mut options = ConnectOptions::new(ClientConfig::new("test", "test"));
        options.spawn = SpawnOptions::new(program);
        options.spawn.env = vec![(DATA_DIR_ENV.into(), tmp.path().join("data").into())];
        options.ready_timeout = Duration::from_secs(5);
        options
    }

    /// An executable `sh` script in `tmp`, standing in for `coqod`.
    fn script(tmp: &tempfile::TempDir, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = tmp.path().join("fake-coqod");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
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

    #[test]
    fn refusals_that_surface_after_connect_mean_not_running() {
        // Seen on CI (#149): the stale-socket refusal arrives as EPIPE or
        // ECONNRESET during the handshake instead of ECONNREFUSED on connect.
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::NotConnected,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::NotFound,
        ] {
            assert!(is_not_running(&io::Error::from(kind)), "{kind:?}");
        }
        assert!(!is_not_running(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[tokio::test]
    async fn missing_binary_is_a_spawn_error() {
        let (tmp, paths) = run_dir();
        let err = connect_or_spawn(&paths, &options("/nonexistent/coqod", &tmp))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::Spawn { .. }), "{err}");
    }

    #[tokio::test]
    async fn daemon_that_exits_is_reported() {
        let (tmp, paths) = run_dir();
        let err = connect_or_spawn(&paths, &options("false", &tmp))
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::DaemonExited { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_daemon_that_exits_says_why() {
        let (tmp, paths) = run_dir();
        // Another daemon's lines in the same file are not ours; ours are,
        // with the untagged lines after them.
        let program = script(
            &tmp,
            "echo 'coqod[1]: someone else' >&2\n\
             echo \"coqod[$$]: cannot open the database\" >&2\n\
             echo 'caused by: disk full' >&2\n\
             echo 'coqod[1]: someone else again' >&2\n\
             exit 1",
        );
        let err = connect_or_spawn(&paths, &options(program, &tmp))
            .await
            .unwrap_err();
        let ConnectError::DaemonExited { reason, log, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(
            reason.as_deref(),
            Some("cannot open the database\ncaused by: disk full")
        );
        let log = log.as_ref().expect("the stderr file");
        assert_eq!(log, &tmp.path().join("data/logs/coqod.stderr"));
        assert!(
            err.to_string()
                .ends_with("): cannot open the database\ncaused by: disk full"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_uncapturable_stderr_is_the_reason() {
        let (tmp, paths) = run_dir();
        // The data directory is a file: neither the launcher nor the daemon
        // can write there.
        std::fs::write(tmp.path().join("data"), "").unwrap();
        let err = connect_or_spawn(&paths, &options("false", &tmp))
            .await
            .unwrap_err();
        let ConnectError::DaemonExited { reason, .. } = &err else {
            panic!("{err}");
        };
        let reason = reason.as_deref().unwrap_or_default();
        assert!(reason.contains("not captured"), "{reason}");
        assert!(reason.contains("coqod.stderr"), "{reason}");
    }

    #[test]
    fn stderr_lines_are_attributed_by_pid() {
        let text = "early untagged\n\
                    coqod[7]: one\n\
                    more of one\n\
                    coqod[8]: other\n\
                    more of other\n\
                    coqod[7]: two\n\
                    coqod[x]: not a tag\n";
        assert_eq!(
            lines_of(text, 7).collect::<Vec<_>>(),
            ["one", "more of one", "two", "coqod[x]: not a tag"]
        );
        assert_eq!(
            lines_of(text, 8).collect::<Vec<_>>(),
            ["other", "more of other"]
        );
        assert_eq!(lines_of(text, 9).count(), 0);
        assert!(is_tagged("coqod[12]: x"));
        assert!(!is_tagged("coqod[]: x"));
        assert!(!is_tagged("coqod: x"));
    }

    #[tokio::test]
    async fn an_insecure_runtime_dir_is_refused_before_connecting() {
        use std::os::unix::fs::PermissionsExt as _;
        let (tmp, paths) = run_dir();
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
        let err = connect_or_spawn(&paths, &options("/nonexistent/coqod", &tmp))
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
