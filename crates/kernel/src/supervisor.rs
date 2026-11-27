// SPDX-License-Identifier: Apache-2.0

//! Process supervisor for the daemon's children: language servers, agents,
//! debug adapters and process mods (PLAN §4.3, §5).
//!
//! Each child:
//!
//! - runs in **its own process group**, so stopping it also stops whatever it
//!   spawned, and a terminal signal aimed at the daemon doesn't reach it;
//! - is restarted according to its [`RestartPolicy`], with exponential
//!   [`Backoff`] between consecutive failures;
//! - has its stderr (and, in [`ChildStdio::Capture`] mode, stdout) captured
//!   as a stream of [`OutputLine`]s, delivered as [`ChildEvent::Output`];
//! - reports its resident set size through
//!   [`ChildHandle::resource_usage`] and [`Supervisor::snapshot`].
//!
//! Children that speak a protocol over stdio (LSP, ACP, DAP, the Coqo Mod
//! Protocol) use [`ChildStdio::Protocol`]: each new instance hands its stdin
//! and stdout to the owner as a [`ChildIo`] via [`ChildHandle::next_io`].
//!
//! When the group leader exits, the rest of its process group is killed too,
//! so a restart never runs next to leftovers from the previous instance.
//!
//! Linux and macOS only. Everything here needs a Tokio runtime.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;

/// Capacity of each child's event channel. Slow subscribers skip events
/// rather than block the child.
const EVENT_CAPACITY: usize = 1024;

/// How long to wait for output readers to drain after the leader exits.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Specification
// ---------------------------------------------------------------------------

/// How a child's standard streams are wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChildStdio {
    /// stdin is `/dev/null`; stdout and stderr are captured as line events.
    #[default]
    Capture,
    /// stdin and stdout are handed to the owner as a [`ChildIo`] for each
    /// instance; stderr is captured as line events.
    Protocol,
}

/// When to restart a child after it exits on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartMode {
    /// Never restart.
    Never,
    /// Restart after a non-zero exit or a signal.
    #[default]
    OnFailure,
    /// Restart after any exit.
    Always,
}

/// Exponential backoff between consecutive restarts: `initial`, then
/// `initial * multiplier`, … capped at `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay before the first restart.
    pub initial: Duration,
    /// Upper bound for the delay.
    pub max: Duration,
    /// Growth factor per consecutive failure (at least 1).
    pub multiplier: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(30),
            multiplier: 2,
        }
    }
}

impl Backoff {
    /// Delay before restart number `attempt` (0-based count of consecutive
    /// failures so far).
    pub fn delay(&self, attempt: u32) -> Duration {
        let factor = self.multiplier.max(1).saturating_pow(attempt);
        self.initial.saturating_mul(factor).min(self.max)
    }
}

/// Restart behaviour of a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// When to restart.
    pub mode: RestartMode,
    /// Delay between consecutive restarts.
    pub backoff: Backoff,
    /// Give up after this many consecutive restarts; `None` retries forever.
    pub max_restarts: Option<u32>,
    /// An instance that ran at least this long resets the consecutive-failure
    /// count, so backoff starts again from [`Backoff::initial`].
    pub reset_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            mode: RestartMode::OnFailure,
            backoff: Backoff::default(),
            max_restarts: Some(5),
            reset_after: Duration::from_secs(30),
        }
    }
}

impl RestartPolicy {
    /// A policy that never restarts.
    pub fn never() -> Self {
        Self {
            mode: RestartMode::Never,
            ..Self::default()
        }
    }
}

/// What to run and how to supervise it.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Human-readable name used in logs and snapshots, e.g. `jdtls`.
    pub name: String,
    /// Program to execute; looked up on `PATH` if it has no slash.
    pub program: OsString,
    /// Arguments.
    pub args: Vec<OsString>,
    /// Extra environment variables (on top of the daemon's).
    pub env: Vec<(OsString, OsString)>,
    /// Variables of the daemon's environment not to pass on (applied before
    /// [`env`](Self::env), so a variable both removed and set is set).
    pub env_remove: Vec<OsString>,
    /// Working directory; the daemon's if `None`.
    pub cwd: Option<PathBuf>,
    /// How the standard streams are wired.
    pub stdio: ChildStdio,
    /// Restart behaviour.
    pub restart: RestartPolicy,
    /// How long a stop waits after `SIGTERM` before sending `SIGKILL`.
    pub stop_timeout: Duration,
}

impl ChildSpec {
    /// A spec with default settings: captured output, restart on failure,
    /// five-second stop timeout.
    pub fn new(name: impl Into<String>, program: impl Into<OsString>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd: None,
            stdio: ChildStdio::default(),
            restart: RestartPolicy::default(),
            stop_timeout: Duration::from_secs(5),
        }
    }

    /// Appends one argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets an environment variable.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Does not pass the daemon's variable `key` on to the child.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    /// Sets the working directory.
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Sets the stdio wiring.
    pub fn stdio(mut self, stdio: ChildStdio) -> Self {
        self.stdio = stdio;
        self
    }

    /// Sets the restart policy.
    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Sets the stop timeout.
    pub fn stop_timeout(mut self, stop_timeout: Duration) -> Self {
        self.stop_timeout = stop_timeout;
        self
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        cmd.args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .process_group(0)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.stdin(match self.stdio {
            ChildStdio::Capture => Stdio::null(),
            ChildStdio::Protocol => Stdio::piped(),
        });
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd
    }
}

// ---------------------------------------------------------------------------
// Status and events
// ---------------------------------------------------------------------------

/// Identifies a child within its supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChildId(pub u64);

impl fmt::Display for ChildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// How an instance ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildExit {
    /// Exit code, if it exited normally.
    pub code: Option<i32>,
    /// Signal number, if a signal terminated it.
    pub signal: Option<i32>,
}

impl ChildExit {
    /// Whether it exited with code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl From<ExitStatus> for ChildExit {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            signal: status.signal(),
        }
    }
}

impl fmt::Display for ChildExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "exit code {code}"),
            (None, Some(signal)) => write!(f, "signal {signal}"),
            (None, None) => f.write_str("unknown exit"),
        }
    }
}

/// Lifecycle state of a supervised child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChildState {
    /// An instance is running.
    Running {
        /// Its process id (also its process group id).
        pid: u32,
    },
    /// Waiting to restart after an exit.
    Backoff {
        /// How long the current wait is.
        delay: Duration,
    },
    /// A stop was requested and the instance is being terminated.
    Stopping,
    /// Stopped on request. Terminal.
    Stopped,
    /// Exited and the policy says not to restart. Terminal.
    Exited(ChildExit),
    /// Gave up after too many consecutive failures. Terminal.
    Failed,
}

impl ChildState {
    /// Whether the child is done and will not run again.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Stopped | Self::Exited(_) | Self::Failed)
    }
}

/// Snapshot of a child's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildStatus {
    /// Current state.
    pub state: ChildState,
    /// Restarts so far (not counting the first start).
    pub restarts: u32,
    /// How the most recent instance ended, if one has.
    pub last_exit: Option<ChildExit>,
}

impl ChildStatus {
    /// The running instance's pid, if any.
    pub fn pid(&self) -> Option<u32> {
        match self.state {
            ChildState::Running { pid } => Some(pid),
            _ => None,
        }
    }
}

/// Which stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One line of captured output, without its line terminator. Invalid UTF-8
/// is replaced with U+FFFD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputLine {
    /// Instance that wrote it (0 for the first run, then 1, 2, … per restart).
    pub instance: u32,
    /// Which stream.
    pub stream: OutputStream,
    /// The text.
    pub line: String,
}

/// Something that happened to a supervised child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChildEvent {
    /// An instance started.
    Started {
        /// Instance number (0 for the first run).
        instance: u32,
        /// Its pid.
        pid: u32,
    },
    /// A line of output.
    Output(OutputLine),
    /// An instance exited on its own (not because of a stop request).
    Exited {
        /// Instance number.
        instance: u32,
        /// How it ended.
        exit: ChildExit,
        /// How long it ran.
        uptime: Duration,
    },
    /// Respawning an instance failed (the program vanished, …).
    SpawnFailed {
        /// The OS error.
        error: String,
    },
    /// A restart will happen after `delay`.
    RestartScheduled {
        /// Consecutive restart number, starting at 1.
        attempt: u32,
        /// Backoff delay before it.
        delay: Duration,
    },
    /// Too many consecutive failures; the child stays down.
    GaveUp {
        /// Consecutive restarts attempted.
        attempts: u32,
    },
    /// Stopped on request.
    Stopped,
}

/// stdin and stdout of one instance of a [`ChildStdio::Protocol`] child.
#[derive(Debug)]
pub struct ChildIo {
    /// Instance number.
    pub instance: u32,
    /// Write requests here.
    pub stdin: ChildStdin,
    /// Read responses here.
    pub stdout: ChildStdout,
}

/// Resource usage of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResourceUsage {
    /// Resident set size in bytes.
    pub rss_bytes: u64,
}

/// One child as reported by [`Supervisor::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSnapshot {
    /// Its id.
    pub id: ChildId,
    /// Its name.
    pub name: String,
    /// Its status.
    pub status: ChildStatus,
    /// Resource usage of the running instance, if any.
    pub usage: Option<ResourceUsage>,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

struct Registered {
    name: String,
    status: watch::Receiver<ChildStatus>,
    cancel: CancellationToken,
}

struct Shared {
    children: Mutex<BTreeMap<ChildId, Registered>>,
    next_id: AtomicU64,
    shutdown: CancellationToken,
}

/// Spawns and supervises child processes. Cheap to clone; clones share the
/// same set of children.
#[derive(Clone)]
pub struct Supervisor {
    shared: Arc<Shared>,
}

impl fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let children = self
            .shared
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut map = f.debug_map();
        for (id, child) in children.iter() {
            map.entry(id, &(&child.name, &child.status.borrow().state));
        }
        map.finish()
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// A supervisor with no children.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                children: Mutex::default(),
                next_id: AtomicU64::new(1),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Starts the first instance of `spec` and supervises it from then on.
    ///
    /// Fails if that first spawn fails (for example, the program doesn't
    /// exist) or if called outside a Tokio runtime. Later respawn failures
    /// are reported as [`ChildEvent::SpawnFailed`] and retried with backoff.
    ///
    /// Dropping the returned handle stops the child.
    pub fn spawn(&self, spec: ChildSpec) -> io::Result<ChildHandle> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| io::Error::other(format!("the supervisor needs a Tokio runtime: {e}")))?;
        let _guard = runtime.enter();
        let first = spec.command().spawn()?;

        let id = ChildId(self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        let cancel = self.shared.shutdown.child_token();
        let (status_tx, status_rx) = watch::channel(ChildStatus {
            state: ChildState::Running {
                pid: first.id().unwrap_or_default(),
            },
            restarts: 0,
            last_exit: None,
        });
        let (events_tx, events_rx) = broadcast::channel(EVENT_CAPACITY);
        let (io_tx, io_rx) = mpsc::unbounded_channel();

        self.shared
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Registered {
                    name: spec.name.clone(),
                    status: status_rx.clone(),
                    cancel: cancel.clone(),
                },
            );

        let io = (spec.stdio == ChildStdio::Protocol).then_some(io_rx);
        let runner = Runner {
            id,
            spec,
            status: status_tx,
            events: events_tx,
            io: io_tx,
            cancel: cancel.clone(),
            shared: Arc::clone(&self.shared),
        };
        runtime.spawn(runner.run(first));

        Ok(ChildHandle {
            id,
            status: status_rx,
            events: events_rx,
            io,
            cancel,
        })
    }

    /// Every live child with its status and resource usage.
    pub async fn snapshot(&self) -> Vec<ChildSnapshot> {
        let children: Vec<(ChildId, String, ChildStatus)> = {
            let children = self
                .shared
                .children
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            children
                .iter()
                .map(|(id, c)| (*id, c.name.clone(), c.status.borrow().clone()))
                .collect()
        };
        let mut out = Vec::with_capacity(children.len());
        for (id, name, status) in children {
            let usage = match status.pid() {
                Some(pid) => resource_usage(pid).await.ok().flatten(),
                None => None,
            };
            out.push(ChildSnapshot {
                id,
                name,
                status,
                usage,
            });
        }
        out
    }

    /// Stops every child and waits until all are down. Children spawned
    /// afterwards are stopped immediately.
    pub async fn shutdown(&self) {
        self.shared.shutdown.cancel();
        let statuses: Vec<watch::Receiver<ChildStatus>> = {
            let children = self
                .shared
                .children
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            children.values().map(|c| c.status.clone()).collect()
        };
        for mut status in statuses {
            let _ = status.wait_for(|s| s.state.is_terminal()).await;
        }
    }

    /// Stops the child `id`, if it is live, without waiting.
    pub fn stop(&self, id: ChildId) -> bool {
        let children = self
            .shared
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match children.get(&id) {
            Some(child) => {
                child.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Owner's handle to a supervised child. Dropping it stops the child.
#[derive(Debug)]
pub struct ChildHandle {
    id: ChildId,
    status: watch::Receiver<ChildStatus>,
    events: broadcast::Receiver<ChildEvent>,
    io: Option<mpsc::UnboundedReceiver<ChildIo>>,
    cancel: CancellationToken,
}

impl ChildHandle {
    /// The child's id within its supervisor.
    pub fn id(&self) -> ChildId {
        self.id
    }

    /// Current status.
    pub fn status(&self) -> ChildStatus {
        self.status.borrow().clone()
    }

    /// The running instance's pid, if any.
    pub fn pid(&self) -> Option<u32> {
        self.status.borrow().pid()
    }

    /// Watches the status.
    pub fn watch_status(&self) -> watch::Receiver<ChildStatus> {
        self.status.clone()
    }

    /// The next event since spawn, including the first `Started`. Returns
    /// `None` once the child is done and every event has been read. If this
    /// handle falls more than 1024 events behind, the oldest are skipped.
    pub async fn next_event(&mut self) -> Option<ChildEvent> {
        loop {
            match self.events.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(child = %self.id, skipped, "event reader lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// An extra event stream, starting from now.
    pub fn subscribe(&self) -> broadcast::Receiver<ChildEvent> {
        self.events.resubscribe()
    }

    /// stdin and stdout of the next instance, for [`ChildStdio::Protocol`]
    /// children. Returns `None` for captured children and once the child is
    /// done.
    pub async fn next_io(&mut self) -> Option<ChildIo> {
        self.io.as_mut()?.recv().await
    }

    /// Resident set size of the running instance, if any.
    pub async fn resource_usage(&self) -> Option<ResourceUsage> {
        resource_usage(self.pid()?).await.ok().flatten()
    }

    /// Asks the child to stop without waiting.
    pub fn request_stop(&self) {
        self.cancel.cancel();
    }

    /// Stops the child (`SIGTERM` to its process group, then `SIGKILL` after
    /// the stop timeout) and waits until it is down.
    pub async fn stop(&self) -> ChildStatus {
        self.cancel.cancel();
        self.wait().await
    }

    /// Waits until the child is done (stopped, exited for good or given up),
    /// without stopping it.
    pub async fn wait(&self) -> ChildStatus {
        let mut status = self.status.clone();
        let done = status
            .wait_for(|s| s.state.is_terminal())
            .await
            .map(|s| s.clone());
        // The sender is gone only if the runner task died; report what it last set.
        done.unwrap_or_else(|_| status.borrow().clone())
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// ---------------------------------------------------------------------------
// Runner: one task per child
// ---------------------------------------------------------------------------

enum Outcome {
    Exited { exit: ChildExit, uptime: Duration },
    SpawnFailed,
    Stopped,
}

struct Runner {
    id: ChildId,
    spec: ChildSpec,
    status: watch::Sender<ChildStatus>,
    events: broadcast::Sender<ChildEvent>,
    io: mpsc::UnboundedSender<ChildIo>,
    cancel: CancellationToken,
    shared: Arc<Shared>,
}

impl Runner {
    fn emit(&self, event: ChildEvent) {
        // No receivers is fine: nobody is listening right now.
        let _ = self.events.send(event);
    }

    fn set_state(&self, state: ChildState) {
        self.status.send_modify(|s| s.state = state);
    }

    async fn run(self, first: Child) {
        let span = tracing::info_span!("child", id = %self.id, name = %self.spec.name);
        let final_state = tracing::Instrument::instrument(self.supervise(first), span).await;
        self.set_state(final_state);
        self.shared
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }

    async fn supervise(&self, first: Child) -> ChildState {
        let policy = self.spec.restart;
        let mut next = Some(first);
        let mut instance: u32 = 0;
        let mut consecutive: u32 = 0;

        loop {
            let child = match next.take() {
                Some(child) => Ok(child),
                None => self.spec.command().spawn(),
            };
            let outcome = match child {
                Ok(child) => self.run_instance(child, instance).await,
                Err(error) => {
                    tracing::warn!(%error, "respawn failed");
                    self.emit(ChildEvent::SpawnFailed {
                        error: error.to_string(),
                    });
                    Outcome::SpawnFailed
                }
            };

            let (restart, uptime) = match outcome {
                Outcome::Stopped => {
                    tracing::info!("stopped");
                    self.emit(ChildEvent::Stopped);
                    return ChildState::Stopped;
                }
                Outcome::SpawnFailed => (true, Duration::ZERO),
                Outcome::Exited { exit, uptime } => {
                    let restart = match policy.mode {
                        RestartMode::Never => false,
                        RestartMode::OnFailure => !exit.success(),
                        RestartMode::Always => true,
                    };
                    if !restart {
                        return ChildState::Exited(exit);
                    }
                    (restart, uptime)
                }
            };
            debug_assert!(restart);

            if uptime >= policy.reset_after {
                consecutive = 0;
            }
            if policy.max_restarts.is_some_and(|max| consecutive >= max) {
                tracing::error!(attempts = consecutive, "giving up after repeated failures");
                self.emit(ChildEvent::GaveUp {
                    attempts: consecutive,
                });
                return ChildState::Failed;
            }
            let delay = policy.backoff.delay(consecutive);
            consecutive += 1;
            tracing::warn!(attempt = consecutive, ?delay, "restart scheduled");
            self.emit(ChildEvent::RestartScheduled {
                attempt: consecutive,
                delay,
            });
            self.set_state(ChildState::Backoff { delay });
            tokio::select! {
                () = sleep(delay) => {}
                () = self.cancel.cancelled() => {
                    tracing::info!("stopped during backoff");
                    self.emit(ChildEvent::Stopped);
                    return ChildState::Stopped;
                }
            }
            instance += 1;
            self.status.send_modify(|s| s.restarts += 1);
        }
    }

    async fn run_instance(&self, mut child: Child, instance: u32) -> Outcome {
        let Some(pid) = child.id() else {
            // Already reaped: can't happen for a freshly spawned child.
            return Outcome::SpawnFailed;
        };
        let started = Instant::now();
        tracing::info!(pid, instance, "started");
        self.set_state(ChildState::Running { pid });
        self.emit(ChildEvent::Started { instance, pid });

        let mut readers: Vec<JoinHandle<()>> = Vec::with_capacity(2);
        if let Some(stderr) = child.stderr.take() {
            readers.push(self.read_lines(stderr, instance, OutputStream::Stderr));
        }
        match self.spec.stdio {
            ChildStdio::Capture => {
                if let Some(stdout) = child.stdout.take() {
                    readers.push(self.read_lines(stdout, instance, OutputStream::Stdout));
                }
            }
            ChildStdio::Protocol => {
                if let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) {
                    let _ = self.io.send(ChildIo {
                        instance,
                        stdin,
                        stdout,
                    });
                }
            }
        }

        let status = tokio::select! {
            status = child.wait() => status,
            () = self.cancel.cancelled() => {
                self.set_state(ChildState::Stopping);
                let exit = self.terminate(&mut child, pid).await;
                tracing::debug!(?exit, "terminated");
                self.status.send_modify(|s| s.last_exit = exit);
                self.drain(readers).await;
                return Outcome::Stopped;
            }
        };
        let uptime = started.elapsed();
        // Clean up anything the leader left behind in its group.
        signal_group(pid, Signal::SIGKILL);
        self.drain(readers).await;

        let exit = match status {
            Ok(status) => ChildExit::from(status),
            Err(error) => {
                tracing::warn!(%error, "waiting for child failed");
                ChildExit {
                    code: None,
                    signal: None,
                }
            }
        };
        if exit.success() {
            tracing::info!(%exit, ?uptime, "exited");
        } else {
            tracing::warn!(%exit, ?uptime, "exited");
        }
        self.status.send_modify(|s| s.last_exit = Some(exit));
        self.emit(ChildEvent::Exited {
            instance,
            exit,
            uptime,
        });
        Outcome::Exited { exit, uptime }
    }

    /// `SIGTERM` to the group, then `SIGKILL` after the stop timeout.
    async fn terminate(&self, child: &mut Child, pid: u32) -> Option<ChildExit> {
        signal_group(pid, Signal::SIGTERM);
        let status = match timeout(self.spec.stop_timeout, child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                tracing::warn!(timeout = ?self.spec.stop_timeout, "no exit after SIGTERM; sending SIGKILL");
                signal_group(pid, Signal::SIGKILL);
                child.wait().await
            }
        };
        // Leftovers that ignored SIGTERM.
        signal_group(pid, Signal::SIGKILL);
        status.ok().map(ChildExit::from)
    }

    async fn drain(&self, readers: Vec<JoinHandle<()>>) {
        for mut reader in readers {
            if timeout(DRAIN_TIMEOUT, &mut reader).await.is_err() {
                // Something outside the group still holds the pipe open.
                reader.abort();
            }
        }
    }

    fn read_lines<R>(&self, stream: R, instance: u32, which: OutputStream) -> JoinHandle<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let events = self.events.clone();
        let child = self.id;
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if buf.last() == Some(&b'\n') {
                            buf.pop();
                            if buf.last() == Some(&b'\r') {
                                buf.pop();
                            }
                        }
                        let line = String::from_utf8_lossy(&buf).into_owned();
                        // Child output can contain anything, so keep it out of normal logs.
                        tracing::trace!(target: "coqo_kernel::child_output", %child, stream = ?which, "{line}");
                        let _ = events.send(ChildEvent::Output(OutputLine {
                            instance,
                            stream: which,
                            line,
                        }));
                    }
                    Err(error) => {
                        tracing::debug!(%child, %error, "output reader failed");
                        break;
                    }
                }
            }
        })
    }
}

/// Sends `signal` to the process group led by `pid`. A group that's already
/// gone is not an error.
fn signal_group(pid: u32, signal: Signal) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    match killpg(Pid::from_raw(raw), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        // macOS reports EPERM for a group whose only members are zombies.
        Err(nix::errno::Errno::EPERM) => {}
        Err(error) => tracing::debug!(pid, ?signal, %error, "killpg failed"),
    }
}

// ---------------------------------------------------------------------------
// Resource usage
// ---------------------------------------------------------------------------

/// Resource usage of process `pid`; `Ok(None)` if there is no such process.
///
/// On Linux this reads `/proc/<pid>/statm`; on macOS it asks `ps`.
pub async fn resource_usage(pid: u32) -> io::Result<Option<ResourceUsage>> {
    Ok(rss_bytes(pid)
        .await?
        .map(|rss_bytes| ResourceUsage { rss_bytes }))
}

#[cfg(target_os = "linux")]
async fn rss_bytes(pid: u32) -> io::Result<Option<u64>> {
    use std::sync::OnceLock;
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();

    let statm = match tokio::fs::read_to_string(format!("/proc/{pid}/statm")).await {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // Fields: size resident shared text lib data dt, in pages.
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed statm"))?;
    let page_size = *PAGE_SIZE.get_or_init(|| {
        nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
            .ok()
            .flatten()
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(4096)
    });
    Ok(Some(pages * page_size))
}

#[cfg(target_os = "macos")]
async fn rss_bytes(pid: u32) -> io::Result<Option<u64>> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p"])
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await?;
    // `ps` exits non-zero when the pid doesn't exist.
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let kib: u64 = text.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected ps output {text:?}"),
        )
    })?;
    Ok(Some(kib * 1024))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn rss_bytes(_pid: u32) -> io::Result<Option<u64>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let backoff = Backoff {
            initial: Duration::from_millis(100),
            max: Duration::from_millis(700),
            multiplier: 2,
        };
        let delays: Vec<u128> = (0..5).map(|n| backoff.delay(n).as_millis()).collect();
        assert_eq!(delays, [100, 200, 400, 700, 700]);
        assert_eq!(backoff.delay(u32::MAX), Duration::from_millis(700));
    }

    #[test]
    fn exit_display() {
        let code = ChildExit {
            code: Some(1),
            signal: None,
        };
        let signal = ChildExit {
            code: None,
            signal: Some(15),
        };
        assert_eq!(code.to_string(), "exit code 1");
        assert_eq!(signal.to_string(), "signal 15");
        assert!(!code.success());
    }
}
