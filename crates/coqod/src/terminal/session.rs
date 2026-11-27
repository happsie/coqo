// SPDX-License-Identifier: Apache-2.0

//! One terminal: a PTY, the process in it, and the emulator state.
//!
//! Three threads per terminal, all blocking I/O:
//! - **reader**: PTY output → OSC scanner → `alacritty_terminal` parser, then
//!   wakes the flusher. On EOF it reaps the process and publishes `Exited`.
//! - **flusher**: turns damage into a [`Frame`] and publishes it, at most once
//!   per [`FRAME_INTERVAL`] (the first change after a quiet spell goes out at
//!   once, so typing echoes without delay).
//! - **writer**: input and the emulator's replies → PTY, in order.
//!
//! Every publish happens under the state lock, so a snapshot's `seq` is
//! exactly the frames it already reflects.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Scroll as GridScroll;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use coqo_protocol::methods::terminal::{
    Screen, ScrollTo, TermSize, TerminalEvent, TerminalId, TerminalInfo, TerminalSnapshot,
    TerminalStatus,
};
use coqo_protocol::{EventFeed, SeqLog};
#[cfg(not(unix))]
use portable_pty::ChildKiller;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use tokio::sync::watch;

use super::osc::{OscScanner, ShellReport};
use super::render::{self, Dims, Sent};
use super::{SpawnSpec, TerminalError};
use crate::BusyGuard;

/// Frames go out at most this often (about 60 Hz).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Events kept for resume. A client further behind takes a new snapshot.
const LOG_CAPACITY: usize = 512;
/// Lines of scrollback.
const SCROLLBACK: usize = 10_000;
/// How long a closed terminal's process gets to exit after SIGHUP before SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(1500);

/// A terminal. Cheap to clone; every clone is the same terminal.
#[derive(Clone)]
pub struct Terminal(Arc<Inner>);

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("id", &self.0.id)
            .field("program", &self.0.program)
            .field("pid", &self.0.pid)
            .finish_non_exhaustive()
    }
}

struct Inner {
    id: TerminalId,
    program: String,
    spawn_cwd: Option<PathBuf>,
    pid: Option<u32>,
    state: Mutex<State>,
    wake: Condvar,
    log: Arc<SeqLog<TerminalEvent>>,
    input: mpsc::Sender<Vec<u8>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    #[cfg(not(unix))]
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    status: watch::Sender<TerminalStatus>,
    /// Set by `close`, so subscriptions end.
    closed: watch::Sender<bool>,
    /// Keeps the daemon awake while the process runs.
    busy: Mutex<Option<BusyGuard>>,
}

struct State {
    term: Term<Proxy>,
    parser: Processor<StdSyncHandler>,
    osc: OscScanner,
    proxy: Arc<Mutex<Vec<Event>>>,
    title: String,
    default_title: String,
    reported_cwd: Option<String>,
    last_command_exit: Option<i32>,
    last: Sent,
    dirty: bool,
    /// The reader is done: no more output.
    stopped: bool,
}

/// Receives the emulator's events. Replies go straight to the writer; the
/// rest are queued and handled under the state lock.
#[derive(Clone)]
struct Proxy {
    input: mpsc::Sender<Vec<u8>>,
    queue: Arc<Mutex<Vec<Event>>>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                let _ = self.input.send(text.into_bytes());
            }
            Event::Title(_) | Event::ResetTitle | Event::Bell | Event::TextAreaSizeRequest(_) => {
                lock(&self.queue).push(event);
            }
            _ => {}
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn pty_size(size: TermSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn exit_code(status: &portable_pty::ExitStatus) -> Option<i32> {
    if status.signal().is_some() {
        None
    } else {
        i32::try_from(status.exit_code()).ok()
    }
}

/// The user's login shell.
fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

fn base_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map_or_else(|| program.to_owned(), |n| n.to_string_lossy().into_owned())
}

impl Terminal {
    pub(super) fn spawn(
        id: TerminalId,
        spec: SpawnSpec,
        busy: Option<BusyGuard>,
    ) -> Result<Self, TerminalError> {
        let size = TermSize::new(spec.size.cols, spec.size.rows);
        let pair = portable_pty::native_pty_system()
            .openpty(pty_size(size))
            .map_err(|e| TerminalError::Spawn(format!("cannot open a PTY: {e}")))?;

        let (mut cmd, program) = match &spec.program {
            Some(program) => (
                CommandBuilder::new(program),
                program.to_string_lossy().into_owned(),
            ),
            None => (CommandBuilder::new_default_prog(), login_shell()),
        };
        cmd.args(&spec.args);
        let cwd = spec.cwd.clone().or_else(std::env::home_dir);
        if let Some(cwd) = &cwd {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "coqo");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TerminalError::Spawn(format!("cannot start {program}: {e}")))?;
        // The child has its own copy; ours would keep the PTY open forever.
        drop(pair.slave);
        let pid = child.process_id();
        #[cfg(not(unix))]
        let killer = child.clone_killer();
        let master = pair.master;
        let reader = master
            .try_clone_reader()
            .map_err(|e| TerminalError::Spawn(format!("cannot read the PTY: {e}")))?;
        let writer = master
            .take_writer()
            .map_err(|e| TerminalError::Spawn(format!("cannot write the PTY: {e}")))?;

        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let proxy = Proxy {
            input: input.clone(),
            queue: Arc::clone(&queue),
        };
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let default_title = spec.title.clone().unwrap_or_else(|| base_name(&program));
        let inner = Arc::new(Inner {
            id,
            program,
            spawn_cwd: cwd,
            pid,
            state: Mutex::new(State {
                term: Term::new(config, &Dims(size), proxy),
                parser: Processor::new(),
                osc: OscScanner::default(),
                proxy: queue,
                title: default_title.clone(),
                default_title,
                reported_cwd: None,
                last_command_exit: None,
                last: Sent::default(),
                dirty: true,
                stopped: false,
            }),
            wake: Condvar::new(),
            log: Arc::new(SeqLog::new(LOG_CAPACITY)),
            input,
            master: Mutex::new(Some(master)),
            #[cfg(not(unix))]
            killer: Mutex::new(killer),
            status: watch::channel(TerminalStatus::Running).0,
            closed: watch::channel(false).0,
            busy: Mutex::new(busy),
        });

        let name = |role: &str| format!("term-{}-{role}", id.0);
        let spawn_thread = |role: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(name(role))
                .spawn(f)
                .map(drop)
                .map_err(|e| TerminalError::Spawn(format!("cannot start a thread: {e}")))
        };
        spawn_thread("writer", Box::new(move || write_loop(writer, input_rx)))?;
        let t = Arc::clone(&inner);
        spawn_thread("flusher", Box::new(move || flush_loop(&t)))?;
        let t = Arc::clone(&inner);
        spawn_thread(
            "reader",
            Box::new(move || {
                read_loop(&t, reader);
                let status = child.wait().ok();
                t.exited(status.as_ref().and_then(exit_code));
            }),
        )?;
        tracing::debug!(terminal = %id, pid, "terminal started");
        Ok(Self(inner))
    }

    pub fn id(&self) -> TerminalId {
        self.0.id
    }

    /// Process id of the program, which is also its process group.
    pub fn pid(&self) -> Option<u32> {
        self.0.pid
    }

    pub fn info(&self) -> TerminalInfo {
        let st = self.0.lock();
        self.0.info(&st)
    }

    /// Whether the process is still running.
    pub fn status(&self) -> TerminalStatus {
        *self.0.status.borrow()
    }

    /// Watch the status, e.g. to react when the process exits.
    pub fn watch_status(&self) -> watch::Receiver<TerminalStatus> {
        self.0.status.subscribe()
    }

    /// Wait until the process exits and return its code.
    pub async fn wait(&self) -> Option<i32> {
        let mut rx = self.watch_status();
        let status = rx
            .wait_for(|s| matches!(s, TerminalStatus::Exited { .. }))
            .await
            .map(|s| *s);
        match status {
            Ok(TerminalStatus::Exited { code }) => code,
            _ => None,
        }
    }

    /// Write bytes to the PTY (typed input, pasted text, escape sequences).
    /// Scrolls the viewport back to the bottom, like any terminal.
    pub fn write(&self, data: impl Into<Vec<u8>>) {
        let data = data.into();
        if data.is_empty() {
            return;
        }
        {
            let mut st = self.0.lock();
            if st.term.grid().display_offset() != 0 {
                st.term.scroll_display(GridScroll::Bottom);
                self.0.mark_dirty(&mut st);
            }
        }
        let _ = self.0.input.send(data);
    }

    /// Resize the PTY and the grid.
    pub fn resize(&self, size: TermSize) {
        let size = TermSize::new(size.cols, size.rows);
        let mut st = self.0.lock();
        if render::size(&st.term) == size {
            return;
        }
        if let Some(master) = lock(&self.0.master).as_ref()
            && let Err(error) = master.resize(pty_size(size))
        {
            tracing::debug!(terminal = %self.0.id, %error, "PTY resize failed");
        }
        st.term.resize(Dims(size));
        self.0.mark_dirty(&mut st);
    }

    /// Move the viewport through the scrollback.
    pub fn scroll(&self, scroll: ScrollTo) {
        let mut st = self.0.lock();
        st.term.scroll_display(match scroll {
            ScrollTo::Lines(n) => GridScroll::Delta(n),
            ScrollTo::PageUp => GridScroll::PageUp,
            ScrollTo::PageDown => GridScroll::PageDown,
            ScrollTo::Top => GridScroll::Top,
            ScrollTo::Bottom => GridScroll::Bottom,
        });
        self.0.mark_dirty(&mut st);
    }

    /// The screen in view and the output seq it reflects.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let st = self.0.lock();
        TerminalSnapshot {
            seq: self.0.log.head(),
            info: self.0.info(&st),
            screen: render::screen(&st.term),
        }
    }

    /// The screen in view.
    pub fn screen(&self) -> Screen {
        render::screen(&self.0.lock().term)
    }

    /// Output events after `since` (see `coqo_protocol::subscription`). The
    /// feed ends when the terminal is closed.
    pub fn subscribe(
        &self,
        since: Option<u64>,
    ) -> Result<EventFeed<TerminalEvent>, coqo_protocol::ErrorObject> {
        use futures_util::StreamExt as _;

        let feed = self.0.log.subscribe(since)?;
        let mut closed = self.0.closed.subscribe();
        let until = async move {
            let _ = closed.wait_for(|c| *c).await;
        };
        Ok(EventFeed {
            head: feed.head,
            events: feed.events.take_until(until).boxed(),
        })
    }

    /// End the process: SIGHUP to its process group and the PTY's foreground
    /// group (a job the shell started), then SIGKILL if it is still running
    /// after a grace period. Subscriptions end.
    pub fn kill(&self) {
        self.0.closed.send_replace(true);
        if matches!(self.status(), TerminalStatus::Exited { .. }) {
            return;
        }
        #[cfg(unix)]
        {
            let groups = self.process_groups();
            signal_groups(&groups, nix::sys::signal::Signal::SIGHUP);
            let mut status = self.watch_status();
            let _ = std::thread::Builder::new()
                .name(format!("term-{}-kill", self.0.id.0))
                .spawn(move || {
                    let deadline = Instant::now() + KILL_GRACE;
                    while Instant::now() < deadline {
                        if matches!(*status.borrow_and_update(), TerminalStatus::Exited { .. }) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    signal_groups(&groups, nix::sys::signal::Signal::SIGKILL);
                });
        }
        #[cfg(not(unix))]
        {
            let _ = lock(&self.0.killer).kill();
        }
    }

    #[cfg(unix)]
    fn process_groups(&self) -> Vec<i32> {
        let mut groups: Vec<i32> = self
            .0
            .pid
            .and_then(|p| i32::try_from(p).ok())
            .into_iter()
            .collect();
        if let Some(fg) = lock(&self.0.master)
            .as_ref()
            .and_then(|m| m.process_group_leader())
            && fg > 0
            && !groups.contains(&fg)
        {
            groups.push(fg);
        }
        groups
    }
}

#[cfg(unix)]
fn signal_groups(groups: &[i32], signal: nix::sys::signal::Signal) {
    for &group in groups {
        match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(error) => tracing::debug!(group, ?signal, %error, "killpg failed"),
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn mark_dirty(&self, st: &mut State) {
        st.dirty = true;
        self.wake.notify_one();
    }

    fn info(&self, st: &State) -> TerminalInfo {
        TerminalInfo {
            id: self.id,
            title: st.title.clone(),
            program: self.program.clone(),
            cwd: st
                .reported_cwd
                .clone()
                .or_else(|| self.process_cwd())
                .or_else(|| {
                    self.spawn_cwd
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                }),
            size: render::size(&st.term),
            pid: self.pid,
            status: *self.status.borrow(),
            last_command_exit: st.last_command_exit,
        }
    }

    /// The working directory of the PTY's foreground process, where the OS
    /// exposes it without unsafe code (Linux `/proc`).
    fn process_cwd(&self) -> Option<String> {
        if !cfg!(target_os = "linux") || !matches!(*self.status.borrow(), TerminalStatus::Running) {
            return None;
        }
        let pid = lock(&self.master)
            .as_ref()
            .and_then(|m| m.process_group_leader())
            .filter(|p| *p > 0)
            .map(|p| p as u32)
            .or(self.pid)?;
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Handle what the emulator queued and the shell reported.
    fn drain(&self, st: &mut State, reports: Vec<ShellReport>) {
        let events = std::mem::take(&mut *lock(&st.proxy));
        for event in events {
            match event {
                Event::Title(title) => {
                    st.title.clone_from(&title);
                    self.log.publish(TerminalEvent::Title { title });
                }
                Event::ResetTitle => {
                    st.title = st.default_title.clone();
                    self.log.publish(TerminalEvent::Title {
                        title: st.title.clone(),
                    });
                }
                Event::Bell => {
                    self.log.publish(TerminalEvent::Bell);
                }
                Event::TextAreaSizeRequest(format) => {
                    let size = render::size(&st.term);
                    let reply = format(WindowSize {
                        num_lines: size.rows,
                        num_cols: size.cols,
                        cell_width: 8,
                        cell_height: 16,
                    });
                    let _ = self.input.send(reply.into_bytes());
                }
                _ => {}
            }
        }
        for report in reports {
            match report {
                ShellReport::Cwd(cwd) => {
                    st.reported_cwd = Some(cwd.clone());
                    self.log.publish(TerminalEvent::Cwd { cwd });
                }
                ShellReport::CommandFinished(code) => {
                    st.last_command_exit = code;
                    self.log.publish(TerminalEvent::CommandFinished { code });
                }
            }
        }
    }

    fn flush(&self, st: &mut State) {
        st.dirty = false;
        self.drain(st, Vec::new());
        let State { term, last, .. } = st;
        if let Some(frame) = render::take_frame(term, last) {
            self.log.publish(TerminalEvent::Frame(frame));
        }
    }

    fn exited(&self, code: Option<i32>) {
        {
            let mut st = self.lock();
            let st = &mut *st;
            if st.parser.sync_timeout().sync_timeout().is_some() {
                st.parser.stop_sync(&mut st.term);
            }
            self.flush(st);
            st.stopped = true;
            self.log.publish(TerminalEvent::Exited { code });
            self.status.send_replace(TerminalStatus::Exited { code });
            self.wake.notify_all();
        }
        // Nothing reads the PTY any more.
        lock(&self.master).take();
        lock(&self.busy).take();
        tracing::debug!(terminal = %self.id, ?code, "terminal process exited");
    }
}

fn read_loop(inner: &Inner, mut reader: Box<dyn Read + Send>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // EIO once every process holding the PTY's other end is gone.
            Err(_) => break,
        };
        let bytes = &buf[..n];
        let mut guard = inner.lock();
        let st = &mut *guard;
        let reports = st.osc.scan(bytes);
        st.parser.advance(&mut st.term, bytes);
        inner.drain(st, reports);
        inner.mark_dirty(st);
    }
}

fn flush_loop(inner: &Inner) {
    loop {
        let mut st = inner
            .wake
            .wait_while(inner.lock(), |s| !s.dirty && !s.stopped)
            .unwrap_or_else(|e| e.into_inner());
        if st.stopped {
            return;
        }
        // Hold frames back during a synchronized update (DEC 2026), up to
        // the parser's timeout.
        if let Some(deadline) = st.parser.sync_timeout().sync_timeout() {
            let now = Instant::now();
            if now < deadline {
                drop(
                    inner
                        .wake
                        .wait_timeout(st, deadline - now)
                        .unwrap_or_else(|e| e.into_inner()),
                );
                continue;
            }
            let s = &mut *st;
            s.parser.stop_sync(&mut s.term);
        }
        inner.flush(&mut st);
        drop(st);
        std::thread::sleep(FRAME_INTERVAL);
    }
}

fn write_loop(mut writer: Box<dyn Write + Send>, input: mpsc::Receiver<Vec<u8>>) {
    while let Ok(data) = input.recv() {
        if writer
            .write_all(&data)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
}
