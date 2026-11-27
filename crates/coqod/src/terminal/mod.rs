// SPDX-License-Identifier: Apache-2.0

//! Daemon-owned terminals (issue #46, PLAN §7.3): PTYs from `portable-pty`,
//! emulated with `alacritty_terminal`, so the grid and scrollback outlive the
//! UI. Clients attach through the `terminal/*` methods
//! ([`coqo_protocol::methods::terminal`]); [`register`] adds them to a router.
//!
//! Other daemon features use the Rust API directly. Claude terminal mode
//! (#57), for example, runs a specific command in a PTY:
//!
//! ```ignore
//! let term = terminals.spawn(
//!     SpawnSpec::command("claude")
//!         .arg("--mcp-config")
//!         .arg(&mcp_config)
//!         .cwd(&workspace_root)
//!         .env("COQO_SESSION", session_id),
//! )?;
//! // The UI opens a terminal tab on `term.id()`; the daemon can watch it:
//! let code = term.wait().await;
//! ```
//!
//! Processes are killed (their process group and the PTY's foreground job)
//! by [`Terminals::close`], and all of them by [`Terminals::shutdown`] or when
//! the last [`Terminals`] handle is dropped. If the daemon dies outright, the
//! kernel hangs up the PTYs.

mod osc;
mod render;
mod session;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use coqo_protocol::methods::terminal::{
    self as proto, NO_SUCH_TERMINAL, SPAWN_FAILED, SpawnParams, TermSize, TerminalId, TerminalInfo,
};
use coqo_protocol::{Access, ErrorObject, Router};

use crate::{Activity, DaemonContext};

pub use session::Terminal;

/// What to run in a new terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The program; `None` runs the user's login shell (`$SHELL -l`).
    pub program: Option<OsString>,
    pub args: Vec<OsString>,
    /// Working directory; `None` is the home directory.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables, on top of the daemon's environment and
    /// `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=coqo`.
    pub env: Vec<(OsString, OsString)>,
    pub size: TermSize,
    /// Title until the program sets one; defaults to the program name.
    pub title: Option<String>,
}

impl SpawnSpec {
    /// The user's login shell.
    pub fn shell() -> Self {
        Self::default()
    }

    /// A specific program, looked up on `PATH`.
    pub fn command(program: impl Into<OsString>) -> Self {
        Self {
            program: Some(program.into()),
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<OsString>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn size(mut self, size: TermSize) -> Self {
        self.size = size;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

impl From<SpawnParams> for SpawnSpec {
    fn from(p: SpawnParams) -> Self {
        Self {
            program: p.program.map(Into::into),
            args: p.args.into_iter().map(Into::into).collect(),
            cwd: p.cwd.map(Into::into),
            env: p
                .env
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            size: p.size,
            title: p.title,
        }
    }
}

/// Why a terminal operation failed.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("no terminal {0}")]
    NoSuchTerminal(TerminalId),
    #[error("{0}")]
    Spawn(String),
}

impl From<TerminalError> for ErrorObject {
    fn from(e: TerminalError) -> Self {
        let code = match e {
            TerminalError::NoSuchTerminal(_) => NO_SUCH_TERMINAL,
            TerminalError::Spawn(_) => SPAWN_FAILED,
        };
        ErrorObject::new(code, e.to_string())
    }
}

#[derive(Debug, Default)]
struct Registry {
    next: u64,
    terminals: BTreeMap<TerminalId, Terminal>,
    /// Running terminals keep the daemon from idling out.
    activity: Option<Activity>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        for terminal in self.terminals.values() {
            terminal.kill();
        }
    }
}

/// The daemon's terminals. Cheap to clone; clones share the terminals.
#[derive(Clone, Debug, Default)]
pub struct Terminals {
    registry: Arc<Mutex<Registry>>,
}

impl Terminals {
    /// Terminals on their own, not tied to a daemon (tests, tools).
    pub fn new() -> Self {
        Self::default()
    }

    /// The daemon's terminals: each running one holds a busy guard (so the
    /// daemon doesn't idle-exit under it), and all are killed when the
    /// daemon shuts down. Call inside the daemon's runtime.
    pub fn for_daemon(ctx: &DaemonContext) -> Self {
        let terminals = Self::default();
        terminals.lock().activity = Some(ctx.activity.clone());
        let on_shutdown = terminals.clone();
        let token = ctx.shutdown.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            on_shutdown.shutdown();
        });
        terminals
    }

    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start `spec` in a new PTY.
    pub fn spawn(&self, spec: SpawnSpec) -> Result<Terminal, TerminalError> {
        let (id, activity) = {
            let mut r = self.lock();
            r.next += 1;
            (TerminalId(r.next), r.activity.clone())
        };
        let busy = activity.map(|a| {
            let program = spec
                .program
                .as_ref()
                .map_or_else(|| "shell".into(), |p| p.to_string_lossy());
            a.busy(format!("terminal {id} ({program})"))
        });
        let terminal = Terminal::spawn(id, spec, busy)?;
        self.lock().terminals.insert(id, terminal.clone());
        Ok(terminal)
    }

    pub fn get(&self, id: TerminalId) -> Option<Terminal> {
        self.lock().terminals.get(&id).cloned()
    }

    fn require(&self, id: TerminalId) -> Result<Terminal, TerminalError> {
        self.get(id).ok_or(TerminalError::NoSuchTerminal(id))
    }

    /// Every terminal, running or exited, oldest first.
    pub fn list(&self) -> Vec<TerminalInfo> {
        let terminals: Vec<Terminal> = self.lock().terminals.values().cloned().collect();
        terminals.iter().map(Terminal::info).collect()
    }

    /// Kill the terminal's processes and forget it. `false` if it didn't exist.
    pub fn close(&self, id: TerminalId) -> bool {
        let removed = self.lock().terminals.remove(&id);
        removed.inspect(Terminal::kill).is_some()
    }

    /// Kill and forget every terminal, for daemon shutdown.
    pub fn shutdown(&self) {
        let all = std::mem::take(&mut self.lock().terminals);
        for terminal in all.values() {
            terminal.kill();
        }
    }
}

/// Add the `terminal/*` methods and the `terminal/output` topic to `router`.
/// All of them need the daemon token: terminals are not tied to a
/// workspace, so a session token (`coqod mcp`) gets `FORBIDDEN`.
pub fn register(router: Router, terminals: Terminals) -> Router {
    let t = terminals;
    router
        .method_with::<proto::Spawn, _, _>(Access::daemon(), {
            let t = t.clone();
            move |_, params| {
                let result = t.spawn(params.into()).map(|term| term.info());
                async move { result.map_err(Into::into) }
            }
        })
        .method_with::<proto::List, _, _>(Access::daemon(), {
            let t = t.clone();
            move |_, ()| {
                let list = t.list();
                async move { Ok(list) }
            }
        })
        .method_with::<proto::Snapshot, _, _>(Access::daemon(), {
            let t = t.clone();
            move |_, p| {
                let result = t.require(p.id).map(|term| term.snapshot());
                async move { result.map_err(Into::into) }
            }
        })
        .method_with::<proto::Input, _, _>(Access::daemon(), {
            let t = t.clone();
            move |_, p| {
                let result = t.require(p.id).map(|term| term.write(p.data));
                async move { result.map_err(Into::into) }
            }
        })
        .method_with::<proto::Resize, _, _>(Access::daemon(), {
            let t = t.clone();
            move |_, p| {
                let result = t.require(p.id).map(|term| term.resize(p.size));
                async move { result.map_err(Into::into) }
            }
        })
        .method_with::<proto::Scroll, _, _>(Access::daemon(), {
            let t = t.clone();
            move |_, p| {
                let result = t.require(p.id).map(|term| term.scroll(p.scroll));
                async move { result.map_err(Into::into) }
            }
        })
        .method_with::<proto::Close, _, _>(Access::daemon(), {
            let t = t.clone();
            move |_, p| {
                let closed = t.close(p.id);
                async move { Ok(closed) }
            }
        })
        .topic_with::<proto::Output, _, _>(Access::daemon(), move |_, p, since| {
            let result = t
                .require(p.id)
                .map_err(ErrorObject::from)
                .and_then(|term| term.subscribe(since));
            async move { result }
        })
}
