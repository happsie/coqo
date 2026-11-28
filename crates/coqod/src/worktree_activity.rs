// SPDX-License-Identifier: Apache-2.0

//! Which other worktrees of its project a session is working in (#359,
//! ADR 0028): `SessionInfo.worktrees`.
//!
//! An agent can work in a worktree that has no session of its own. Claude
//! Code's in-process subagents (`isolation: worktree`) edit
//! `.claude/worktrees/agent-…`; an agent may `git worktree add` a folder
//! and work there. The work belongs to the chat that does it, so no session
//! is made for it. Instead, when a session's tool calls touch a path in
//! another worktree of its workspace's project, that worktree is added to
//! the session's info, and the worktree switcher shows the row as "Claude
//! working · from <chat>".
//!
//! - **Touches** are the paths of the session's tool calls (their locations
//!   and `diff` content, as reported, whatever the tool: reading there is
//!   working there too) and its ACP `fs/write_text_file`s, handed on by
//!   [`crate::provenance`], which already follows both.
//! - **Which worktree:** the deepest of the project's worktrees
//!   ([`crate::workspace::project_worktrees`], read from git's admin
//!   folders) that holds the path, resolved through symlinks. A touch in the
//!   session's own worktree (the one holding its workspace's first root)
//!   is not activity elsewhere. `.claude/worktrees/agent-1` is inside the
//!   main worktree's folder, and is still another worktree.
//! - **Fading:** a worktree goes from the list when the session's turn
//!   ends, or after [`QUIET`] with no touch there.
//! - Only changes of the set go out on `sessions/changes`; touching the
//!   same worktree again only extends its time.
//!
//! Belongs with the `workspaces` mod once the kernel lets mods see
//! sessions' tool calls (mod audit, #353).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use coqo_agents::SessionManager;
use coqo_protocol::methods::agents::{SessionId, WorktreeActivity};
use coqo_protocol::methods::workspace::WorkspaceId;
use coqo_storage::Timestamp;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::workspace::{WorkspaceService, project_of, project_worktrees};

/// How long a worktree stays in a session's list after its last touch.
pub const QUIET: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum Msg {
    Touched {
        session: SessionId,
        workspace: WorkspaceId,
        paths: Vec<PathBuf>,
    },
    TurnEnded(SessionId),
    Settled(oneshot::Sender<()>),
}

/// Follows sessions' touches of other worktrees. See the module docs. Cheap
/// to clone; the task ends when the last clone goes.
#[derive(Clone, Debug)]
pub struct WorktreeActivityTracker {
    tx: mpsc::UnboundedSender<Msg>,
}

/// One worktree a session works in.
#[derive(Debug)]
struct Entry {
    path: String,
    since: i64,
    last: Instant,
}

impl WorktreeActivityTracker {
    /// Start following, reporting to `sessions`. A worktree goes after
    /// `quiet` with no touch. Must be called inside a Tokio runtime.
    pub fn spawn(
        workspaces: Arc<WorkspaceService>,
        sessions: SessionManager,
        quiet: Duration,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(workspaces, sessions, quiet, rx));
        Self { tx }
    }

    /// `session` (of `workspace`) touched `paths` (absolute; others are
    /// skipped).
    pub fn touched(&self, session: &SessionId, workspace: &WorkspaceId, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_absolute()).collect();
        if paths.is_empty() {
            return;
        }
        let _ = self.tx.send(Msg::Touched {
            session: session.clone(),
            workspace: workspace.clone(),
            paths,
        });
    }

    /// `session`'s turn ended: it works nowhere else any more.
    pub fn turn_ended(&self, session: &SessionId) {
        let _ = self.tx.send(Msg::TurnEnded(session.clone()));
    }

    /// Waits until everything reported so far is handled (for tests).
    pub async fn settled(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Settled(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

/// `path` resolved through symlinks as far as it exists.
fn real(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => real(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// A project's worktrees: each one's folder as git names it, and resolved.
type Worktrees = Vec<(String, PathBuf)>;

/// The worktrees of the project `root` is in, or none outside a
/// repository.
fn worktrees_of(root: &Path) -> Worktrees {
    let Some(common) = project_of(root) else {
        return Vec::new();
    };
    project_worktrees(&common)
        .into_iter()
        .map(|p| (p.to_string_lossy().into_owned(), real(&p)))
        .collect()
}

/// The deepest of `worktrees` holding `path` (resolved).
fn holding<'a>(worktrees: &'a Worktrees, path: &Path) -> Option<&'a (String, PathBuf)> {
    worktrees
        .iter()
        .filter(|(_, folder)| path.starts_with(folder))
        .max_by_key(|(_, folder)| folder.components().count())
}

/// The other worktrees of the project that `paths` touch, given the
/// workspace's first `root`.
pub(crate) fn elsewhere(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let worktrees = worktrees_of(root);
    let own = holding(&worktrees, &real(root)).map(|(name, _)| name.clone());
    let mut found: Vec<String> = Vec::new();
    for path in paths {
        if let Some((name, _)) = holding(&worktrees, &real(path))
            && own.as_ref() != Some(name)
            && !found.contains(name)
        {
            found.push(name.clone());
        }
    }
    found
}

async fn run(
    workspaces: Arc<WorkspaceService>,
    sessions: SessionManager,
    quiet: Duration,
    mut rx: mpsc::UnboundedReceiver<Msg>,
) {
    let mut live: HashMap<SessionId, Vec<Entry>> = HashMap::new();
    let publish = |session: &SessionId, entries: &[Entry]| {
        let mut worktrees: Vec<WorktreeActivity> = entries
            .iter()
            .map(|e| WorktreeActivity {
                path: e.path.clone(),
                since: e.since,
            })
            .collect();
        worktrees.sort_by(|a, b| b.since.cmp(&a.since).then_with(|| a.path.cmp(&b.path)));
        if let Err(e) = sessions.set_worktree_activity(session, worktrees) {
            tracing::debug!(session = %session.0, error = %e, "cannot report worktree activity");
        }
    };
    loop {
        let next = live.values().flatten().map(|e| e.last + quiet).min();
        let msg = tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => Some(msg),
                None => return,
            },
            () = async {
                match next {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => None,
        };
        match msg {
            None => {
                // Quiet ones fade.
                let now = Instant::now();
                for (session, entries) in &mut live {
                    let before = entries.len();
                    entries.retain(|e| now < e.last + quiet);
                    if entries.len() != before {
                        publish(session, entries);
                    }
                }
                live.retain(|_, entries| !entries.is_empty());
            }
            Some(Msg::Touched {
                session,
                workspace,
                paths,
            }) => {
                let Some(root) = workspaces
                    .roots(workspace)
                    .await
                    .ok()
                    .and_then(|roots| roots.into_iter().next())
                    .map(|(_, path)| path)
                else {
                    continue;
                };
                let found = tokio::task::spawn_blocking(move || elsewhere(&root, &paths))
                    .await
                    .unwrap_or_default();
                if found.is_empty() {
                    continue;
                }
                let entries = live.entry(session.clone()).or_default();
                let now = Instant::now();
                let mut added = false;
                for path in found {
                    if let Some(at) = entries.iter().position(|e| e.path == path) {
                        let mut entry = entries.remove(at);
                        entry.last = now;
                        entries.insert(0, entry);
                    } else {
                        entries.insert(
                            0,
                            Entry {
                                path,
                                since: Timestamp::now().0,
                                last: now,
                            },
                        );
                        added = true;
                    }
                }
                if added {
                    publish(&session, entries);
                }
            }
            Some(Msg::TurnEnded(session)) => {
                if live.remove(&session).is_some() {
                    publish(&session, &[]);
                }
            }
            Some(Msg::Settled(done)) => {
                let _ = done.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
