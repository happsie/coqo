// SPDX-License-Identifier: Apache-2.0

//! Socket security against the real `coqod` binary (PLAN §9, ADR 0007):
//! the runtime directory checks, the session token in the handshake, and
//! token scopes. The peer-credential comparison is unit-tested in
//! `src/auth.rs`, since connecting as another uid needs root; every
//! connection here goes through the same accept-path check with our uid.

use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use coqo_client::{
    ConnectError, DATA_DIR_ENV, DaemonPaths, RUNTIME_DIR_ENV, RuntimeDir, RuntimeDirError, connect,
    current_uid,
};
use coqo_protocol::daemon::{Shutdown, ShutdownParams, Status};
use coqo_protocol::{Client, ClientConfig, ProtocolError, SessionToken, codes};
use tempfile::TempDir;
use tokio::net::UnixStream;

const COQOD: &str = env!("CARGO_BIN_EXE_coqod");
const TIMEOUT: Duration = Duration::from_secs(10);

struct Env {
    tmp: TempDir,
    run: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        Self { tmp, run }
    }

    fn paths(&self) -> DaemonPaths {
        DaemonPaths::at(&self.run)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(COQOD);
        cmd.arg("--foreground")
            .env(RUNTIME_DIR_ENV, &self.run)
            .env(DATA_DIR_ENV, self.tmp.path().join("data"))
            .env("COQO_LOG", "debug")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    }

    /// `coqod --foreground`, logging to `log`, once it accepts clients.
    async fn start(&self, log: &Path) -> Daemon {
        let child = self
            .command()
            .stderr(File::create(log).unwrap())
            .spawn()
            .unwrap();
        let daemon = Daemon { child };
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while connect(&self.paths(), config()).await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "coqod did not start"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        daemon
    }

    /// Run `coqod --foreground` that is expected to fail; its stderr.
    fn fails_to_start(&self) -> String {
        let output = self.command().stderr(Stdio::piped()).output().unwrap();
        assert!(!output.status.success(), "coqod started anyway");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn token(&self) -> SessionToken {
        RuntimeDir::open(&self.run)
            .unwrap()
            .unwrap()
            .read_token()
            .unwrap()
            .expect("a token file")
    }

    /// Handshake by hand with `token`, bypassing coqo-client.
    async fn raw_connect(&self, token: Option<SessionToken>) -> Result<Client, ProtocolError> {
        let stream = UnixStream::connect(self.paths().socket()).await.unwrap();
        let mut config = config();
        config.token = token;
        Client::connect(stream, config).await.map(|(c, _)| c)
    }
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config() -> ClientConfig {
    ClientConfig::new("coqod-security-tests", "test")
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

#[tokio::test(flavor = "multi_thread")]
async fn the_token_is_private_and_required() {
    let env = Env::new();
    let log = env.tmp.path().join("coqod.log");
    let daemon = env.start(&log).await;

    // User-only directory, socket and token file.
    assert_eq!(mode(&env.run), 0o700);
    assert_eq!(mode(&env.paths().socket()), 0o600);
    let token_file = env.paths().token_file();
    assert_eq!(mode(&token_file), 0o600);
    assert_eq!(fs::metadata(&token_file).unwrap().uid(), current_uid());
    let token = env.token();
    assert_eq!(token.expose().len(), 64);

    // coqo-client sends it by itself.
    let (client, _) = connect(&env.paths(), config()).await.unwrap();
    assert_eq!(
        client.request::<Status>(()).await.unwrap().pid,
        daemon.child.id()
    );

    // Without it, or with a wrong one: a clear error, not a hang.
    for bad in [None, Some(SessionToken::new("0".repeat(64)))] {
        let e = tokio::time::timeout(TIMEOUT, env.raw_connect(bad))
            .await
            .expect("no hang")
            .unwrap_err();
        assert!(matches!(e, ProtocolError::Unauthorized(_)), "{e:?}");
        assert!(e.to_string().contains("session token"), "{e}");
    }
    // A client that is not coqo-client but reads the file is fine.
    let client = env.raw_connect(Some(token.clone())).await.unwrap();
    client.request::<Status>(()).await.unwrap();

    // The token never reaches the log, even at debug level.
    drop(client);
    drop(daemon);
    let log = fs::read_to_string(&log).unwrap();
    assert!(log.contains("coqod listening"), "{log}");
    assert!(
        log.contains("session token"),
        "rejections are logged: {log}"
    );
    assert!(
        !log.contains(token.expose()),
        "the token leaked into the log"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_gets_a_new_token_and_removes_the_old_one() {
    let env = Env::new();
    let log = env.tmp.path().join("coqod.log");
    let daemon = env.start(&log).await;
    let first = env.token();
    let (client, _) = connect(&env.paths(), config()).await.unwrap();
    client
        .request::<Shutdown>(ShutdownParams { force: true })
        .await
        .unwrap();
    let mut daemon = daemon;
    assert!(daemon.child.wait().unwrap().success());
    assert!(!env.paths().token_file().exists(), "removed on exit");

    let _daemon = env.start(&log).await;
    let second = env.token();
    assert_ne!(first, second);
    let e = env.raw_connect(Some(first)).await.unwrap_err();
    assert!(matches!(e, ProtocolError::Unauthorized(_)), "{e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_too_open_runtime_dir_is_refused() {
    let env = Env::new();
    fs::create_dir(&env.run).unwrap();
    fs::set_permissions(&env.run, fs::Permissions::from_mode(0o755)).unwrap();
    let stderr = env.fails_to_start();
    assert!(stderr.contains("accessible to other users"), "{stderr}");
    assert_eq!(mode(&env.run), 0o755, "not silently changed");
    assert!(!env.paths().token_file().exists());
    assert!(!env.paths().socket().exists());

    // Clients refuse it too, rather than spawning or connecting.
    let e = connect(&env.paths(), config()).await.unwrap_err();
    assert!(
        matches!(e, ConnectError::RuntimeDir(RuntimeDirError::TooOpen { .. })),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_symlinked_runtime_dir_is_refused() {
    let env = Env::new();
    let target = env.tmp.path().join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&target, &env.run).unwrap();
    let stderr = env.fails_to_start();
    assert!(stderr.contains("is a symlink"), "{stderr}");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0, "nothing written");

    let e = connect(&env.paths(), config()).await.unwrap_err();
    assert!(
        matches!(e, ConnectError::RuntimeDir(RuntimeDirError::Symlink { .. })),
        "{e}"
    );
}

/// A token minted for one agent session (what `coqod mcp` gets) is held to
/// its scope. Runs the daemon in-process so the test can mint one.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_token_is_held_to_its_scope() {
    let env = Env::new();
    let paths = env.paths();
    let data = coqo_storage::DataDirs::at(env.tmp.path().join("data"));
    let lock = coqod::daemon::acquire(&paths).unwrap();
    let auth = lock.auth().clone();
    let listener = coqod::daemon::bind(&paths).unwrap();
    let daemon = tokio::spawn(coqod::daemon::serve(
        paths.clone(),
        data,
        coqod::IdlePolicy::default(),
        lock,
        listener,
    ));

    let scoped = auth.mint_session("ws-a", "session-1").unwrap();
    let mut config = config();
    config.token = Some(scoped.clone());
    let (client, _) = connect(&paths, config.clone()).await.unwrap();
    // Read-only daemon status is fine; stopping the daemon is not.
    client.request::<Status>(()).await.unwrap();
    let e = client
        .request::<Shutdown>(ShutdownParams { force: true })
        .await
        .unwrap_err();
    assert_eq!(e.rpc().map(|e| e.code), Some(codes::FORBIDDEN), "{e}");
    assert!(e.to_string().contains("ws-a"), "{e}");

    // Revoked: new connections with it are refused.
    assert!(auth.revoke(&scoped));
    let e = connect(&paths, config).await.unwrap_err();
    assert!(
        matches!(e, ConnectError::Protocol(ProtocolError::Unauthorized(_))),
        "{e}"
    );

    // The daemon token may stop it.
    let (client, _) = connect(&paths, self::config()).await.unwrap();
    let result = client
        .request::<Shutdown>(ShutdownParams { force: true })
        .await
        .unwrap();
    assert!(result.stopping);
    tokio::time::timeout(TIMEOUT, daemon)
        .await
        .expect("stops")
        .unwrap()
        .unwrap();
}
