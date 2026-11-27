// SPDX-License-Identifier: Apache-2.0

//! `coqod mcp` as a process against a real `coqod` (ADR 0012): MCP on
//! stdio, the token only in the environment, and exits on stdin EOF or when
//! the daemon goes away. Scoped one-time tokens and the route policy are
//! tested in-process (`src/editor_mcp/tests.rs`); a real daemon mints those
//! only for agent sessions, so here the bridge gets the daemon token.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use coqo_client::{
    DATA_DIR_ENV, DaemonPaths, RUNTIME_DIR_ENV, RuntimeDir, SESSION_TOKEN_ENV, connect,
};
use coqo_protocol::methods::workspace::{self as wsm, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig, SessionToken};
use serde_json::{Value, json};
use tempfile::TempDir;

const COQOD: &str = env!("CARGO_BIN_EXE_coqod");
const TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    tmp: TempDir,
    run: PathBuf,
    child: Child,
}

impl Daemon {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        let child = Command::new(COQOD)
            .arg("--foreground")
            .env(RUNTIME_DIR_ENV, &run)
            .env(DATA_DIR_ENV, tmp.path().join("data"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self { tmp, run, child };
        let deadline = Instant::now() + TIMEOUT;
        while daemon.client().await.is_none() {
            assert!(Instant::now() < deadline, "coqod did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        daemon
    }

    async fn client(&self) -> Option<Client> {
        let config = ClientConfig::new("coqod-mcp-tests", "test");
        connect(&DaemonPaths::at(&self.run), config)
            .await
            .ok()
            .map(|(c, _)| c)
    }

    fn token(&self) -> SessionToken {
        RuntimeDir::open(&self.run)
            .unwrap()
            .unwrap()
            .read_token()
            .unwrap()
            .expect("a token file")
    }

    async fn workspace(&self) -> WorkspaceInfo {
        let dir = self.tmp.path().join("project");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "text\n").unwrap();
        self.client()
            .await
            .unwrap()
            .request::<wsm::Open>(wsm::OpenParams {
                path: Some(dir.to_str().unwrap().to_owned()),
                ..wsm::OpenParams::default()
            })
            .await
            .unwrap()
    }

    fn bridge(&self, workspace: &str, token: Option<&SessionToken>) -> Command {
        let mut cmd = Command::new(COQOD);
        cmd.args(["mcp", "--workspace", workspace, "--session", "s-1"])
            .env(RUNTIME_DIR_ENV, &self.run)
            .env_remove(SESSION_TOKEN_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(token) = token {
            cmd.env(SESSION_TOKEN_ENV, token.expose());
        }
        cmd
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The agent's end of a `coqod mcp` process.
struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Agent {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "stdout closed"
            );
            let message: Value = serde_json::from_str(&line).expect("only MCP on stdout");
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    fn initialize(&mut self) {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-agent", "version": "0" },
            }),
        );
        assert_eq!(
            init["result"]["serverInfo"]["name"], "coqo-editor",
            "{init}"
        );
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }

    /// Wait for the process to exit; its status and stderr.
    fn wait(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("coqod mcp did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut self.child.stderr.take().unwrap(), &mut stderr).unwrap();
        (status, stderr)
    }
}

/// The command line of process `pid`, as other users see it.
fn command_line(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn coqod_mcp_serves_stdio_with_the_token_off_argv() {
    let daemon = Daemon::start().await;
    let ws = daemon.workspace().await;
    let token = daemon.token();
    let mut agent = Agent::spawn(daemon.bridge(&ws.id.0, Some(&token)));

    let argv = command_line(agent.child.id());
    assert!(argv.contains("mcp --workspace"), "{argv}");
    assert!(!argv.contains(token.expose()), "token on argv: {argv}");

    agent.initialize();
    let r = agent.request(
        "tools/call",
        json!({ "name": "editor_context", "arguments": {} }),
    );
    let workspace = &r["result"]["structuredContent"]["workspace"];
    assert_eq!(workspace["id"], ws.id.0, "{r}");
    let r = agent.request(
        "tools/call",
        json!({ "name": "editor_reveal", "arguments": { "path": "a.txt" } }),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["delivered"], false, "{r}");

    // stdin EOF: a clean exit.
    let (status, stderr) = agent.wait();
    assert!(status.success(), "{status}: {stderr}");
    assert!(!stderr.contains(token.expose()), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn coqod_mcp_exits_when_the_daemon_stops() {
    let daemon = Daemon::start().await;
    let ws = daemon.workspace().await;
    let mut agent = Agent::spawn(daemon.bridge(&ws.id.0, Some(&daemon.token())));
    agent.initialize();
    drop(daemon);
    let (status, stderr) = agent.wait();
    assert!(!status.success(), "{status}");
    assert!(stderr.contains("daemon connection closed"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn coqod_mcp_needs_a_token_in_its_environment() {
    let daemon = Daemon::start().await;
    let (status, stderr) = Agent::spawn(daemon.bridge("ws", None)).wait();
    assert!(!status.success(), "{status}");
    assert!(stderr.contains(SESSION_TOKEN_ENV), "{stderr}");

    // A wrong one is refused by the daemon.
    let wrong = SessionToken::new("0".repeat(64));
    let (status, stderr) = Agent::spawn(daemon.bridge("ws", Some(&wrong))).wait();
    assert!(!status.success(), "{status}");
    assert!(stderr.contains("invalid session token"), "{stderr}");
}
