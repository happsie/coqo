// SPDX-License-Identifier: Apache-2.0

//! End-to-end lifecycle tests against the real `coqod` binary. Every test
//! gets its own runtime and data directories, so they are hermetic and run
//! in parallel.

use std::ffi::OsString;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use coqo_client::{
    ConnectError, ConnectOptions, Connection, DATA_DIR_ENV, DaemonPaths, EXIT_ALREADY_RUNNING,
    RUNTIME_DIR_ENV, SpawnOptions, connect, connect_or_spawn,
};
use coqo_protocol::daemon::{IdlePhase, Shutdown, ShutdownParams, Status};
use coqo_protocol::{Client, ClientConfig};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getsid};
use tempfile::TempDir;

const COQOD: &str = env!("CARGO_BIN_EXE_coqod");
const TIMEOUT: Duration = Duration::from_secs(10);

struct Env {
    _tmp: TempDir,
    run: PathBuf,
    data: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        let data = tmp.path().join("data");
        Self {
            _tmp: tmp,
            run,
            data,
        }
    }

    fn paths(&self) -> DaemonPaths {
        DaemonPaths::at(&self.run)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(COQOD);
        cmd.args(args)
            .env(RUNTIME_DIR_ENV, &self.run)
            .env(DATA_DIR_ENV, &self.data)
            .env("COQO_LOG", "debug")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        cmd
    }

    /// `coqod --foreground <args>`, once its socket accepts clients.
    async fn foreground(&self, args: &[&str]) -> Daemon {
        let mut all = vec!["--foreground"];
        all.extend_from_slice(args);
        let child = self.command(&all).spawn().unwrap();
        let daemon = Daemon { child };
        let paths = self.paths();
        eventually("the socket accepts clients", || async {
            connect(&paths, config()).await.is_ok()
        })
        .await;
        daemon
    }

    async fn client(&self) -> Client {
        connect(&self.paths(), config()).await.unwrap().0
    }

    fn connect_options(&self, idle_exit: &str) -> ConnectOptions {
        let mut options = ConnectOptions::new(config());
        let mut spawn = SpawnOptions::new(COQOD);
        spawn.args = vec!["--idle-exit".into(), idle_exit.into()];
        spawn.env = vec![(OsString::from(DATA_DIR_ENV), self.data.clone().into())];
        options.spawn = spawn;
        options.ready_timeout = TIMEOUT;
        options
    }
}

/// A daemon this test started; killed when dropped.
struct Daemon {
    child: Child,
}

impl Daemon {
    fn pid(&self) -> Pid {
        Pid::from_raw(i32::try_from(self.child.id()).unwrap())
    }

    async fn exit(&mut self) -> ExitStatus {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(tokio::time::Instant::now() < deadline, "coqod did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config() -> ClientConfig {
    ClientConfig::new("coqod-tests", "test")
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !check().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting until {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn connections(client: &Client) -> u32 {
    client.request::<Status>(()).await.unwrap().connections
}

async fn stop(conn: &Connection) {
    let result = conn
        .client
        .request::<Shutdown>(ShutdownParams { force: true })
        .await
        .unwrap();
    assert!(result.stopping);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_the_daemon() {
    let env = Env::new();
    let daemon = env.foreground(&["--idle-exit", "5m"]).await;
    let client = env.client().await;
    assert_eq!(client.welcome().server, "coqod");

    let status = client.request::<Status>(()).await.unwrap();
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(status.pid, daemon.child.id());
    assert_eq!(status.connections, 1);
    assert!(status.busy.is_empty());
    assert_eq!(status.idle.phase, IdlePhase::Active);
    assert_eq!(status.idle.exit_after_secs, Some(300));
    assert_eq!(status.idle.stop_services_after_secs, Some(1800));

    // A user-only directory and socket.
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(env.run.clone()), 0o700);
    assert_eq!(mode(env.paths().socket()), 0o600);
    assert_eq!(env.paths().recorded_pid().unwrap(), Some(daemon.child.id()));
}

/// The real daemon serves the workspace model (#22), and workspaces live in
/// its data directory, so they survive a daemon restart.
#[tokio::test(flavor = "multi_thread")]
async fn workspaces_survive_a_daemon_restart() {
    use coqo_protocol::methods::{fs, workspace};
    let env = Env::new();
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("README.md"), "# hi\n").unwrap();
    let opened = {
        let mut daemon = env.foreground(&["--idle-exit", "5m"]).await;
        let client = env.client().await;
        let ws = client
            .request::<workspace::Open>(workspace::OpenParams {
                path: Some(folder.path().to_str().unwrap().into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let listed = client
            .request::<fs::ListDir>(fs::PathParams {
                workspace: ws.id.clone(),
                root: ws.roots[0].id,
                path: String::new(),
            })
            .await
            .unwrap();
        assert_eq!(listed.entries[0].name, "README.md");
        client
            .request::<Shutdown>(ShutdownParams { force: true })
            .await
            .unwrap();
        assert!(daemon.exit().await.success());
        ws
    };
    let _daemon = env.foreground(&["--idle-exit", "5m"]).await;
    let client = env.client().await;
    let list = client.request::<workspace::List>(()).await.unwrap();
    assert_eq!(list.workspaces, vec![opened]);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_attach_at_once() {
    let env = Env::new();
    let _daemon = env.foreground(&[]).await;
    let first = env.client().await;
    let second = env.client().await;
    assert_ne!(first.welcome().connection, second.welcome().connection);
    eventually("both are counted", || async {
        connections(&first).await == 2
    })
    .await;
    assert_eq!(connections(&second).await, 2);

    drop(second);
    eventually("the second detaches", || async {
        connections(&first).await == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exits_when_idle_and_removes_its_socket() {
    let env = Env::new();
    let mut daemon = env.foreground(&["--idle-exit", "300ms"]).await;
    let client = env.client().await;
    // Attached clients keep it alive past the idle timeout.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(connections(&client).await, 1);

    drop(client);
    assert!(daemon.exit().await.success());
    assert!(!env.paths().socket().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_instance_refuses_to_start() {
    let env = Env::new();
    let daemon = env.foreground(&[]).await;
    let output = env
        .command(&["--foreground"])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(EXIT_ALREADY_RUNNING));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "{stderr}");
    assert!(
        stderr.contains(&daemon.child.id().to_string()),
        "names the running pid: {stderr}"
    );

    // The first one is unaffected.
    let client = env.client().await;
    assert_eq!(
        client.request::<Status>(()).await.unwrap().pid,
        daemon.child.id()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_socket_from_a_crashed_daemon_is_replaced() {
    let env = Env::new();
    let mut crashed = env.foreground(&[]).await;
    kill(crashed.pid(), Signal::SIGKILL).unwrap();
    crashed.exit().await;
    assert!(env.paths().socket().exists(), "SIGKILL leaves the socket");
    assert!(matches!(
        connect(&env.paths(), config()).await,
        Err(ConnectError::NotRunning(_))
    ));

    let daemon = env.foreground(&[]).await;
    let client = env.client().await;
    assert_eq!(
        client.request::<Status>(()).await.unwrap().pid,
        daemon.child.id()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_and_sigint_shut_down_cleanly() {
    for signal in [Signal::SIGTERM, Signal::SIGINT] {
        let env = Env::new();
        let mut daemon = env.foreground(&[]).await;
        let client = env.client().await;
        kill(daemon.pid(), signal).unwrap();
        assert!(daemon.exit().await.success(), "{signal}");
        assert!(!env.paths().socket().exists(), "{signal}");
        client.closed().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_method_stops_the_daemon() {
    let env = Env::new();
    let mut daemon = env.foreground(&[]).await;
    let client = env.client().await;
    let result = client
        .request::<Shutdown>(ShutdownParams::default())
        .await
        .unwrap();
    assert!(result.stopping);
    assert!(daemon.exit().await.success());
    assert!(!env.paths().socket().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_or_spawn_starts_a_detached_daemon_once() {
    let env = Env::new();
    let options = env.connect_options("30s");
    let first = connect_or_spawn(&env.paths(), &options).await.unwrap();
    assert!(first.spawned);
    assert!(first.same_build(coqod::daemon::BUILD));
    let status = first.client.request::<Status>(()).await.unwrap();

    // It runs in its own session, not the test's.
    let pid = Pid::from_raw(i32::try_from(status.pid).unwrap());
    assert_eq!(getsid(Some(pid)).unwrap(), pid, "coqod leads its session");
    assert_ne!(getsid(Some(pid)).unwrap(), getsid(None).unwrap());

    let second = connect_or_spawn(&env.paths(), &options).await.unwrap();
    assert!(!second.spawned);
    assert_eq!(
        second.client.request::<Status>(()).await.unwrap().pid,
        status.pid
    );
    assert_eq!(connections(&second.client).await, 2);

    stop(&first).await;
    first.client.closed().await;
    eventually("the socket is gone", || async {
        !env.paths().socket().exists()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_spawns_share_one_daemon() {
    let env = Env::new();
    let options = env.connect_options("30s");
    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let (paths, options) = (env.paths(), options.clone());
            tokio::spawn(async move { connect_or_spawn(&paths, &options).await })
        })
        .collect();
    let mut conns = Vec::new();
    for task in tasks {
        conns.push(task.await.unwrap().unwrap());
    }
    let mut pids = Vec::new();
    for conn in &conns {
        pids.push(conn.client.request::<Status>(()).await.unwrap().pid);
    }
    pids.dedup();
    assert_eq!(pids.len(), 1, "one daemon: {pids:?}");
    stop(&conns[0]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bare_coqod_starts_it_in_the_background() {
    let env = Env::new();
    let output = env
        .command(&["--idle-exit", "30s"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("started"), "{stdout}");

    let output = env.command(&[]).stdout(Stdio::piped()).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("already running"), "{stdout}");

    let client = env.client().await;
    client
        .request::<Shutdown>(ShutdownParams { force: true })
        .await
        .unwrap();
    client.closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_sessions_are_served_from_the_global_database() {
    use coqo_protocol::methods::agents::{List, ListSessionsParams, Providers};

    let env = Env::new();
    let _daemon = env.foreground(&["--idle-exit", "5m"]).await;
    let client = env.client().await;
    // No built-in provider mod yet (#54, #56).
    assert!(client.request::<Providers>(()).await.unwrap().is_empty());
    let list = client
        .request::<List>(ListSessionsParams::default())
        .await
        .unwrap();
    assert!(list.sessions.is_empty());
    assert!(env.data.join("coqo.db").exists());
}
