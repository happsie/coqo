// SPDX-License-Identifier: Apache-2.0

//! The tools every `coqo-editor` server offers (PLAN §7.4).
//!
//! A core tool exists only for what the agent's own tools cannot do: the
//! editor's live state, the user, and Coqo's own features (ADR 0014,
//! *Editor-only tools*). Reading, listing and searching files, outlines and
//! code navigation are left to the harness (Claude Code's Read, Glob, Grep).
//!
//! Core tools run in the bridge and reach editor state through daemon
//! methods on the session-scoped connection:
//!
//! | Tool | What | Daemon methods |
//! |---|---|---|
//! | `editor_context` | the workspace's roots, open tabs, active file, selections, visible lines | `workspace/get`, `editor/getContext` |
//! | `editor_reveal` | open a file in the editor and select a range | `editor/reveal` |
//! | `set_status` | tell the user what the agent is doing (status line, progress, state) | `sessions/setStatus` |
//! | `ask_question` | a question pinned to code; waits for the answer unless `wait: false` | `questions/create`, `questions/watch` |
//! | `await_answer` | wait for the answer to an earlier question | `questions/watch` |
//! | `annotate` | a note pinned to code, non-blocking | `questions/create` |
//! | `set_acceptance_criteria` | the task's acceptance criteria and whether each holds | `sessions/setCriteria` |
//! | `publish_walkthrough` | a code walkthrough of the finished change, checked in full | `walkthroughs/create` |
//! | `reply_to_review` | answer a thread of the user's code review, optionally resolving it | `review/reply` |
//!
//! [`daemon`] adds the tools mods contribute in the daemon (`mcp/*`).
//!
//! Lines and columns in tool arguments and results are **1-based**, as
//! editors and `cat -n` show them; the protocol's are 0-based.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceInfo, WorkspaceParams};
use serde_json::{Map, Value, json};

use crate::registry::{Tool, ToolContext, ToolError, ToolRegistry};

mod criteria;
pub mod daemon;
mod editor;
pub mod questions;
mod review;
mod status;
mod walkthrough;

pub use criteria::SetAcceptanceCriteriaTool;
pub use editor::{EditorContextTool, EditorRevealTool};
pub use questions::{AnnotateTool, AskQuestionTool, AwaitAnswerTool};
pub use review::ReplyToReviewTool;
pub use status::SetStatusTool;
pub use walkthrough::PublishWalkthroughTool;

/// Register the core tools.
pub fn core(registry: &ToolRegistry) {
    let tools: [Arc<dyn Tool>; 9] = [
        Arc::new(EditorContextTool),
        Arc::new(EditorRevealTool),
        Arc::new(SetStatusTool),
        Arc::new(AskQuestionTool),
        Arc::new(AwaitAnswerTool),
        Arc::new(AnnotateTool),
        Arc::new(SetAcceptanceCriteriaTool),
        Arc::new(PublishWalkthroughTool),
        Arc::new(ReplyToReviewTool),
    ];
    for error in registry.update(&[], tools.into()) {
        tracing::warn!(error = %error, "cannot register a core tool");
    }
}

/// The session's workspace.
pub(crate) async fn workspace(cx: &ToolContext) -> Result<WorkspaceInfo, ToolError> {
    Ok(cx
        .daemon()
        .request::<wsm::Get>(WorkspaceParams {
            id: cx.workspace().clone(),
        })
        .await?)
}

/// The `path` and `root` schema properties every tool that names a file
/// takes.
pub(crate) fn path_properties(what: &str) -> Map<String, Value> {
    let Value::Object(props) = json!({
        "path": {
            "type": "string",
            "description": format!("{what}, relative to the root or absolute inside the workspace."),
        },
        "root": {
            "type": "integer",
            "minimum": 0,
            "description": "Root id from editor_context. Default: the first root, or the root an absolute path is in.",
        },
    }) else {
        unreachable!("an object literal")
    };
    props
}

/// An optional string argument.
pub(crate) fn opt_str<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// An optional non-negative integer argument (the schema checked its type).
pub(crate) fn opt_u64(args: &Map<String, Value>, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
    })
}

/// An optional boolean argument.
pub(crate) fn opt_bool(args: &Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A file the call names: the workspace, the root and the path relative to
/// it (`/`-separated; empty for the root itself).
pub(crate) struct Target {
    pub(crate) ws: WorkspaceInfo,
    pub(crate) root: RootId,
    pub(crate) path: String,
}

impl Target {
    /// The absolute path, for the model.
    pub(crate) fn absolute(&self) -> String {
        absolute(&self.ws, self.root, &self.path)
    }
}

/// `root`'s absolute path joined with `rel`.
pub(crate) fn absolute(ws: &WorkspaceInfo, root: RootId, rel: &str) -> String {
    match ws.roots.iter().find(|r| r.id == root) {
        Some(r) if rel.is_empty() => r.path.clone(),
        Some(r) => Path::new(&r.path).join(rel).to_string_lossy().into_owned(),
        None => rel.to_owned(),
    }
}

/// Resolve the call's `path` (`default` if absent) and `root` arguments.
pub(crate) async fn target(
    cx: &ToolContext,
    args: &Map<String, Value>,
    default: Option<&str>,
) -> Result<Target, ToolError> {
    let path = match (opt_str(args, "path"), default) {
        (Some(p), _) => p,
        (None, Some(d)) => d,
        (None, None) => {
            return Err(ToolError::InvalidArguments(
                "`path` (a string) is required".into(),
            ));
        }
    };
    let root = opt_u64(args, "root")
        .map(|n| {
            u32::try_from(n)
                .map(RootId)
                .map_err(|_| ToolError::InvalidArguments("`root` must be a root id".into()))
        })
        .transpose()?;
    let ws = workspace(cx).await?;
    let (root, rel) = if Path::new(path).is_absolute() {
        locate(&ws, path, root)?
    } else {
        let root = match root {
            Some(root) => root,
            None => {
                ws.roots
                    .first()
                    .ok_or_else(|| ToolError::Failed("the workspace has no folder roots".into()))?
                    .id
            }
        };
        let rel = path.trim_start_matches("./").trim_end_matches('/');
        (
            root,
            if rel == "." {
                String::new()
            } else {
                rel.to_owned()
            },
        )
    };
    Ok(Target {
        ws,
        root,
        path: rel,
    })
}

/// The root an absolute `path` is in, and the path relative to it.
///
/// The path is matched as given first. Failing that, it is matched with
/// symlinks resolved on both sides, so a file named through a symlinked
/// folder still maps to its root: an agent whose working directory is a
/// symlink to the project, or a macOS temp folder (`/var` is a symlink to
/// `/private/var`, and roots are stored canonical). A file that does not
/// exist yet resolves through its nearest existing ancestor. A path that
/// resolves outside every root is refused. This only maps the path to a
/// root; the daemon's path checks decide what the call may touch.
pub(crate) fn locate(
    ws: &WorkspaceInfo,
    path: &str,
    root: Option<RootId>,
) -> Result<(RootId, String), ToolError> {
    let path = Path::new(path);
    let roots = || ws.roots.iter().filter(|r| root.is_none_or(|id| r.id == id));
    for r in roots() {
        if let Some(rel) = relative(path, Path::new(&r.path)) {
            return Ok((r.id, rel));
        }
    }
    if let Some(resolved) = resolve(path) {
        for r in roots() {
            let Ok(root_path) = std::fs::canonicalize(&r.path) else {
                continue;
            };
            if let Some(rel) = relative(&resolved, &root_path) {
                return Ok((r.id, rel));
            }
        }
    }
    Err(ToolError::InvalidArguments(format!(
        "{} is outside the workspace's roots",
        path.display()
    )))
}

/// `path` relative to `root`, `/`-separated, if it is inside.
fn relative(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let rel: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
    Some(rel.join("/"))
}

/// `path` with every symlink resolved: its nearest existing ancestor made
/// canonical, and the rest (which does not exist yet) appended. `None` if
/// nothing of it exists, or the missing rest steps up with `..`.
fn resolve(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(existing) {
            resolved.extend(missing.iter().rev());
            return Some(resolved);
        }
        // `file_name` is `None` for a path ending in `..`.
        missing.push(existing.file_name()?);
        existing = existing.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::workspace::{WorkspaceId, WorkspaceRoot};

    use super::*;

    pub(crate) fn ws() -> WorkspaceInfo {
        WorkspaceInfo {
            id: WorkspaceId("ws".into()),
            name: "ws".into(),
            roots: vec![
                WorkspaceRoot {
                    id: RootId(0),
                    path: "/src/app".into(),
                    name: "app".into(),
                },
                WorkspaceRoot {
                    id: RootId(1),
                    path: "/src/lib".into(),
                    name: "lib".into(),
                },
            ],
            created_at: 0,
            last_opened_at: 0,
            open: true,
            project: None,
        }
    }

    #[test]
    fn absolute_paths_map_to_their_root() {
        let ws = ws();
        assert_eq!(
            locate(&ws, "/src/lib/a/b.rs", None).unwrap(),
            (RootId(1), "a/b.rs".to_owned())
        );
        assert_eq!(
            locate(&ws, "/src/app/x", Some(RootId(0))).unwrap(),
            (RootId(0), "x".to_owned())
        );
        assert!(locate(&ws, "/src/app/x", Some(RootId(1))).is_err());
        assert!(locate(&ws, "/etc/passwd", None).is_err());
        // A sibling with the same prefix is not inside.
        assert!(locate(&ws, "/src/application/x", None).is_err());
        assert_eq!(absolute(&ws, RootId(1), "a/b.rs"), "/src/lib/a/b.rs");
        assert_eq!(absolute(&ws, RootId(0), ""), "/src/app");
    }

    /// A workspace with one root at `path`, stored canonical as the daemon
    /// stores it.
    fn ws_at(path: &Path) -> WorkspaceInfo {
        let mut ws = ws();
        ws.roots.truncate(1);
        ws.roots[0].path = path.canonicalize().unwrap().to_string_lossy().into_owned();
        ws
    }

    fn at(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn absolute_paths_through_symlinks_map_to_their_root() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().unwrap();
        let project = base.path().join("project");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(project.join("src/a.rs"), "").unwrap();
        // The agent's view of the project: through a symlink to it.
        let link = base.path().join("link");
        symlink(&project, &link).unwrap();
        // A link inside the root to a folder outside it.
        symlink(&outside, project.join("escape")).unwrap();
        let ws = ws_at(&project);
        let ok = |path: &Path| locate(&ws, &at(path), None).unwrap();

        assert_eq!(ok(&link.join("src/a.rs")), (RootId(0), "src/a.rs".into()));
        assert_eq!(ok(&link), (RootId(0), String::new()));
        // Files (and folders) that do not exist yet.
        assert_eq!(
            ok(&link.join("src/new.rs")),
            (RootId(0), "src/new.rs".into())
        );
        assert_eq!(ok(&link.join("new/b.rs")), (RootId(0), "new/b.rs".into()));
        // A symlinked parent of the root, like macOS's `/var` → `/private/var`.
        let var = tempfile::tempdir().unwrap();
        let private_var = var.path().join("var");
        symlink(base.path(), &private_var).unwrap();
        assert_eq!(
            ok(&private_var.join("project/src/a.rs")),
            (RootId(0), "src/a.rs".into())
        );
        // A temp folder as given (not canonical on macOS).
        assert_eq!(
            ok(&project.join("src/a.rs")),
            (RootId(0), "src/a.rs".into())
        );

        // Still refused: whatever resolves outside the root.
        for path in [
            outside.join("x.rs"),
            link.join("escape/x.rs"),
            link.join("../outside/x.rs"),
            link.join("missing/../../outside/x.rs"),
            base.path().join("nothing/here.rs"),
        ] {
            assert!(locate(&ws, &at(&path), None).is_err(), "{}", path.display());
        }
        assert!(locate(&ws, &at(&link.join("src/a.rs")), Some(RootId(1))).is_err());
    }
}
