// SPDX-License-Identifier: Apache-2.0

//! [`ClaudeProvider`]: the ACP host running the Claude adapter, plus setup
//! status and the install action.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use coqo_acp_host::{AcpOptions, AcpProvider, FsHandle};
use coqo_agents::model::{ProviderAction, ProviderActionRun, ProviderCapabilities, ProviderStatus};
use coqo_agents::{
    AgentProvider, ProviderError, ProviderSession, ResumeRequest, SessionContext, SessionEvents,
};
use coqo_mod_api::BoxFuture;
use tokio::io::AsyncReadExt as _;

use crate::detect::{Detection, Detector, adapter_bin};
use crate::launch::{adapter_command, npm_install_args, session_markers};
use crate::{ADAPTER_PACKAGE, ADAPTER_VERSION, INSTALL_ACTION, MIN_NODE_MAJOR};

/// How long `npm install` may take.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Settings of a [`ClaudeProvider`].
#[derive(Clone, Debug)]
pub struct ClaudeOptions {
    /// Coqo's data directory; the adapter is installed in
    /// `<data_dir>/claude-acp`.
    pub data_dir: PathBuf,
    /// "Use Claude plan logins through ACP": when off, the adapter runs with
    /// `--hide-claude-auth`.
    pub plan_logins: bool,
    /// "Append Coqo's operating guide" (`claude.guide`): when on, every new
    /// and resumed session gets [`crate::guide`] appended to Claude Code's
    /// system prompt, if the adapter takes one.
    pub guide: bool,
    /// "Load Coqo's Claude Code plugin" (`claude.plugin`): when on, the
    /// plugin ([`crate::plugin`]) is written to [`Self::plugin_dir`] and
    /// loaded into every session.
    pub plugin: bool,
    /// Where the agent's file traffic goes (the daemon's buffers).
    pub fs: FsHandle,
    /// The `PATH` to find `claude`, `node` and `npm` on; the daemon's when
    /// `None`.
    pub path: Option<OsString>,
    /// Settings of the ACP host underneath (timeouts, restarts).
    pub acp: AcpOptions,
}

impl ClaudeOptions {
    /// Defaults, with the adapter under `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            plan_logins: true,
            guide: true,
            plugin: true,
            fs: FsHandle::new(),
            path: None,
            acp: AcpOptions::new(crate::ADAPTER_BIN),
        }
    }

    /// The adapter's install directory.
    pub fn adapter_dir(&self) -> PathBuf {
        self.data_dir.join("claude-acp")
    }

    /// Where Coqo's Claude Code plugin is written.
    pub fn plugin_dir(&self) -> PathBuf {
        crate::plugin::plugin_dir(&self.data_dir)
    }
}

/// Claude Code as an agent provider: the user's own `claude`, through the
/// Claude Agent ACP adapter, one adapter process per workspace.
#[derive(Clone)]
pub struct ClaudeProvider {
    inner: Arc<Inner>,
}

struct Inner {
    detector: Arc<Detector>,
    acp: AcpProvider,
    /// One install at a time.
    install: tokio::sync::Mutex<()>,
    installing: AtomicBool,
}

impl fmt::Debug for ClaudeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeProvider")
            .field("adapter_dir", &self.inner.detector.adapter_dir())
            .finish_non_exhaustive()
    }
}

impl ClaudeProvider {
    /// A provider with `options`.
    pub fn new(options: ClaudeOptions) -> Self {
        let detector = Arc::new(Detector::new(options.path.clone(), options.adapter_dir()));
        let mut acp = options.acp.clone();
        acp.fs = options.fs.clone();
        acp.system_prompt = options.guide.then(crate::guide);
        if options.plugin {
            let dir = options.plugin_dir();
            match crate::plugin::install(&dir) {
                Ok(()) => acp.session_meta = Some(crate::plugin::session_meta(&dir)),
                Err(e) => tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "cannot write Coqo's Claude Code plugin; sessions start without it"
                ),
            }
        }
        let launch = Arc::clone(&detector);
        let plan_logins = options.plan_logins;
        let acp = AcpProvider::new(
            move |_: &Path| {
                let found = launch.detect();
                match (&found.claude, &found.node) {
                    (Some(claude), Some(node)) if found.ready() => Ok(adapter_command(
                        &found.adapter_dir,
                        node,
                        claude,
                        plan_logins,
                        std::env::vars_os(),
                    )),
                    _ => Err(ProviderError::Failed(not_ready_message(&found, false))),
                }
            },
            acp,
        );
        Self {
            inner: Arc::new(Inner {
                detector,
                acp,
                install: tokio::sync::Mutex::new(()),
                installing: AtomicBool::new(false),
            }),
        }
    }

    /// What is installed now.
    pub fn detect(&self) -> Detection {
        self.inner.detector.detect()
    }

    async fn install(&self) -> Result<(), ProviderError> {
        let _one = self.inner.install.lock().await;
        let found = self.detect();
        if found.adapter_ok() {
            return Ok(());
        }
        if !found.node.as_ref().is_some_and(|n| n.new_enough()) {
            return Err(ProviderError::Failed(not_ready_message(&found, false)));
        }
        let Some(npm) = found.npm.clone() else {
            return Err(ProviderError::Failed(
                "npm wasn't found on PATH; it comes with Node.js.".into(),
            ));
        };
        self.inner.installing.store(true, Ordering::SeqCst);
        let _done = Reset(&self.inner.installing);
        let dir = found.adapter_dir.clone();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ProviderError::Failed(format!("cannot create {}: {e}", dir.display())))?;
        tracing::info!(dir = %dir.display(), npm = %npm.display(), "installing {ADAPTER_PACKAGE}@{ADAPTER_VERSION}");
        let mut command = tokio::process::Command::new(&npm);
        command
            .args(npm_install_args(&dir))
            .current_dir(&dir)
            .env("PATH", self.inner.detector.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in session_markers(std::env::vars_os()) {
            command.env_remove(key);
        }
        let mut child = command
            .spawn()
            .map_err(|e| ProviderError::Failed(format!("cannot run npm: {e}")))?;
        let mut stderr = child.stderr.take();
        let run = async {
            let mut err = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut err).await;
            }
            (child.wait().await, err)
        };
        let (status, err) = tokio::time::timeout(INSTALL_TIMEOUT, run)
            .await
            .map_err(|_| ProviderError::Failed("npm install timed out".into()))?;
        let status = status.map_err(|e| ProviderError::Failed(format!("npm: {e}")))?;
        if !status.success() {
            let err = String::from_utf8_lossy(&err);
            let tail: Vec<&str> = err.lines().rev().take(6).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            tracing::warn!(%status, "npm install failed: {}", tail.join(" | "));
            return Err(ProviderError::Failed(format!(
                "npm install failed ({status}): {}",
                tail.join("\n")
            )));
        }
        let found = self.detect();
        if !found.adapter_ok() {
            return Err(ProviderError::Failed(format!(
                "npm finished, but {} is missing",
                adapter_bin(&found.adapter_dir).display()
            )));
        }
        tracing::info!("installed {ADAPTER_PACKAGE}@{ADAPTER_VERSION}");
        Ok(())
    }
}

struct Reset<'a>(&'a AtomicBool);

impl Drop for Reset<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Why sessions can't start, for the user.
fn not_ready_message(found: &Detection, installing: bool) -> String {
    if found.claude.is_none() {
        return "Claude Code isn't installed: `claude` wasn't found on PATH. Install Claude Code, \
                run `claude` once to sign in, then start a new chat."
            .into();
    }
    match &found.node {
        None => {
            return format!(
                "The Claude ACP adapter needs Node.js {MIN_NODE_MAJOR} or newer, and `node` \
                 wasn't found on PATH."
            );
        }
        Some(node) if !node.new_enough() => {
            let found = node.version.map_or_else(
                || "an unknown version".into(),
                |(a, b, c)| format!("v{a}.{b}.{c}"),
            );
            return format!(
                "The Claude ACP adapter needs Node.js {MIN_NODE_MAJOR} or newer; found {found}."
            );
        }
        Some(_) => {}
    }
    if installing {
        return "Installing the Claude ACP adapter…".into();
    }
    let what = match &found.adapter_version {
        Some(v) if v != ADAPTER_VERSION => {
            format!("Claude Code chats need the Claude ACP adapter {ADAPTER_VERSION} (found {v})")
        }
        _ => "Claude Code chats need the Claude ACP adapter".into(),
    };
    let npm = if found.npm.is_some() {
        ""
    } else {
        " npm wasn't found on PATH; it comes with Node.js."
    };
    format!(
        "{what}: Coqo installs {ADAPTER_PACKAGE} with npm into its data directory when you ask \
         it to. It runs your own `claude` and your own login.{npm}"
    )
}

/// The provider status for `found`.
pub fn status(found: &Detection, installing: bool) -> ProviderStatus {
    let sign_in = found.claude.as_ref().map(|claude| ProviderAction {
        id: "sign-in".into(),
        label: "Open `claude` in a terminal".into(),
        run: ProviderActionRun::Terminal {
            program: claude.on_path.display().to_string(),
            args: Vec::new(),
        },
    });
    if found.ready() {
        return ProviderStatus {
            sign_in,
            ..ProviderStatus::ready()
        };
    }
    let can_install = !installing
        && found.claude.is_some()
        && found.node.as_ref().is_some_and(|n| n.new_enough())
        && found.npm.is_some()
        && !found.adapter_ok();
    let actions = if can_install {
        vec![ProviderAction {
            id: INSTALL_ACTION.into(),
            label: "Install Claude ACP adapter (npm)".into(),
            run: ProviderActionRun::Daemon,
        }]
    } else {
        Vec::new()
    };
    ProviderStatus {
        ready: false,
        message: Some(not_ready_message(found, installing)),
        actions,
        sign_in,
    }
}

impl AgentProvider for ClaudeProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.acp.capabilities()
    }

    fn status(&self) -> Option<ProviderStatus> {
        let installing = self.inner.installing.load(Ordering::SeqCst);
        Some(status(&self.detect(), installing))
    }

    fn run_action(&self, action: String) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(async move {
            match action.as_str() {
                INSTALL_ACTION => self.install().await,
                _ => Err(ProviderError::Unsupported(format!("action `{action}`"))),
            }
        })
    }

    fn start_session(
        &self,
        cx: SessionContext,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>> {
        self.inner.acp.start_session(cx, events)
    }

    fn resume_session(
        &self,
        request: ResumeRequest,
        events: SessionEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn ProviderSession>, ProviderError>> {
        self.inner.acp.resume_session(request, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Claude, Node};

    fn found() -> Detection {
        Detection {
            claude: Some(Claude {
                on_path: "/home/u/.local/bin/claude".into(),
                real: "/home/u/.local/share/claude/versions/2.1.0".into(),
            }),
            node: Some(Node {
                path: "/usr/bin/node".into(),
                version: Some((24, 1, 0)),
            }),
            npm: Some("/usr/bin/npm".into()),
            adapter_dir: "/nonexistent/claude-acp".into(),
            adapter_version: None,
        }
    }

    #[test]
    fn a_missing_adapter_offers_the_install_and_the_sign_in_terminal() {
        let s = status(&found(), false);
        assert!(!s.ready);
        assert_eq!(s.actions.len(), 1);
        assert_eq!(s.actions[0].id, INSTALL_ACTION);
        assert_eq!(s.actions[0].label, "Install Claude ACP adapter (npm)");
        assert_eq!(s.actions[0].run, ProviderActionRun::Daemon);
        assert!(s.message.unwrap().contains("npm"));
        assert_eq!(
            s.sign_in.unwrap().run,
            ProviderActionRun::Terminal {
                program: "/home/u/.local/bin/claude".into(),
                args: vec![]
            }
        );

        // While installing, no second install.
        let s = status(&found(), true);
        assert!(s.actions.is_empty());
        assert_eq!(
            s.message.as_deref(),
            Some("Installing the Claude ACP adapter…")
        );
    }

    #[test]
    fn missing_claude_or_old_node_cannot_be_installed_by_coqo() {
        let mut no_claude = found();
        no_claude.claude = None;
        let s = status(&no_claude, false);
        assert!(s.actions.is_empty() && s.sign_in.is_none());
        assert!(s.message.unwrap().contains("`claude` wasn't found"));

        let mut old = found();
        old.node.as_mut().unwrap().version = Some((20, 0, 0));
        let s = status(&old, false);
        assert!(s.actions.is_empty());
        assert!(s.message.unwrap().contains("found v20.0.0"));

        let mut no_npm = found();
        no_npm.npm = None;
        let s = status(&no_npm, false);
        assert!(s.actions.is_empty());
        assert!(s.message.unwrap().contains("npm wasn't found"));
    }
}
