// SPDX-License-Identifier: Apache-2.0

//! The core tools every `coqo-editor` server offers.
//!
//! - `workspace_info`: the session's workspace: id, name and roots.
//! - `read_file`: a text file from one of its roots (`fs/read`).
//!
//! The editor, LSP and question tools of PLAN §7.4 build on the same
//! [`Tool`] interface (#70 and later).

use std::path::Path;

use coqo_protocol::methods::fs::{ReadFile, ReadParams};
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceInfo, WorkspaceParams};
use serde_json::{Map, Value, json};

use crate::registry::{
    BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput, ToolRegistry,
};

/// Register the core tools.
pub fn core(registry: &ToolRegistry) {
    for tool in [
        std::sync::Arc::new(WorkspaceInfoTool) as std::sync::Arc<dyn Tool>,
        std::sync::Arc::new(ReadFileTool),
    ] {
        if let Err(e) = registry.register_arc(tool) {
            tracing::warn!(error = %e, "cannot register a core tool");
        }
    }
}

async fn workspace(cx: &ToolContext) -> Result<WorkspaceInfo, ToolError> {
    Ok(cx
        .daemon()
        .request::<wsm::Get>(WorkspaceParams {
            id: cx.workspace().clone(),
        })
        .await?)
}

/// `workspace_info`.
#[derive(Debug)]
pub struct WorkspaceInfoTool;

impl Tool for WorkspaceInfoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "workspace_info",
            "The Coqo workspace this session works in: its id, name and folder roots (id, \
             name and absolute path of each). File paths in other coqo-editor tools are \
             relative to a root.",
        )
        .with_title("Workspace info")
        .read_only()
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        _args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let ws = workspace(cx).await?;
            Ok(ToolOutput::json(json!({
                "id": ws.id,
                "name": ws.name,
                "roots": ws.roots,
                "session": cx.session(),
            })))
        })
    }
}

/// `read_file`.
#[derive(Debug)]
pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read_file",
            "Read a UTF-8 text file from the Coqo workspace, as saved on disk. `path` is \
             relative to a root (the first unless `root` is given), or absolute inside one \
             of the workspace's roots. Paths outside the workspace are refused.",
        )
        .with_title("Read file")
        .read_only()
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path, relative to the root or absolute inside the workspace.",
                },
                "root": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Root id from workspace_info. Default: the first root, or the root an absolute path is in.",
                },
            },
            "required": ["path"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidArguments("`path` (a string) is required".into())
                })?;
            let root = match args.get("root") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .map(RootId)
                        .ok_or_else(|| {
                            ToolError::InvalidArguments("`root` must be a root id".into())
                        })?,
                ),
            };
            let (root, rel) = if Path::new(path).is_absolute() {
                let ws = workspace(cx).await?;
                locate(&ws, path, root)?
            } else if let Some(root) = root {
                (root, path.to_owned())
            } else {
                let ws = workspace(cx).await?;
                let first = ws
                    .roots
                    .first()
                    .ok_or_else(|| ToolError::Failed("the workspace has no folder roots".into()))?;
                (first.id, path.to_owned())
            };
            let read = cx
                .daemon()
                .request::<ReadFile>(ReadParams {
                    workspace: cx.workspace().clone(),
                    root,
                    path: rel.clone(),
                    max_bytes: None,
                })
                .await?;
            match read.text {
                Some(text) => Ok(ToolOutput::text(text)),
                None => Err(ToolError::Failed(format!(
                    "{rel} is not a UTF-8 text file ({} bytes)",
                    read.size
                ))),
            }
        })
    }
}

/// The root an absolute `path` is in, and the path relative to it.
fn locate(
    ws: &WorkspaceInfo,
    path: &str,
    root: Option<RootId>,
) -> Result<(RootId, String), ToolError> {
    let path = Path::new(path);
    for r in ws.roots.iter().filter(|r| root.is_none_or(|id| r.id == id)) {
        if let Ok(rel) = path.strip_prefix(&r.path) {
            let rel: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
            return Ok((r.id, rel.join("/")));
        }
    }
    Err(ToolError::InvalidArguments(format!(
        "{} is outside the workspace's roots",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::workspace::{WorkspaceId, WorkspaceRoot};

    use super::*;

    fn ws() -> WorkspaceInfo {
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
    }
}
