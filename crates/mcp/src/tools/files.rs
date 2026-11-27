// SPDX-License-Identifier: Apache-2.0

//! `workspace_info`, `read_file`, `list_files` and `find_files`.

use std::collections::VecDeque;

use coqo_protocol::methods::fs::{EntryKind, ListDir, PathParams, ReadFile, ReadParams};
use serde_json::{Map, Value, json};

use super::{Target, opt_bool, opt_u64, path_properties, target, workspace};
use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

/// Most entries `list_files` returns.
const MAX_LIST: u64 = 2000;
/// Default for `list_files`' `limit`.
const DEFAULT_LIST: u64 = 500;
/// Deepest `list_files` recursion.
const MAX_DEPTH: u64 = 8;
/// Most entries `find_files` looks at.
const MAX_WALK: usize = 50_000;
/// Default and most matches of `find_files`.
const DEFAULT_MATCHES: u64 = 50;
const MAX_MATCHES: u64 = 500;

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
        let mut props = path_properties("File path");
        if let Some(p) = props.get_mut("path").and_then(Value::as_object_mut) {
            p.insert("minLength".into(), json!(1));
        }
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
            "properties": props,
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
            let Target { root, path, .. } = target(cx, &args, None).await?;
            let read = cx
                .daemon()
                .request::<ReadFile>(ReadParams {
                    workspace: cx.workspace().clone(),
                    root,
                    path: path.clone(),
                    max_bytes: None,
                })
                .await?;
            match read.text {
                Some(text) => Ok(ToolOutput::text(text)),
                None => Err(ToolError::Failed(format!(
                    "{path} is not a UTF-8 text file ({} bytes)",
                    read.size
                ))),
            }
        })
    }
}

/// One entry found by walking a root.
#[derive(Debug, Clone, PartialEq)]
struct Found {
    /// Relative to the root, `/`-separated.
    path: String,
    dir: bool,
    size: u64,
}

impl Found {
    fn json(&self) -> Value {
        if self.dir {
            json!({ "path": self.path, "kind": "dir" })
        } else {
            json!({ "path": self.path, "kind": "file", "size": self.size })
        }
    }

    fn line(&self) -> String {
        if self.dir {
            format!("{}/", self.path)
        } else {
            self.path.clone()
        }
    }
}

/// Walk `start` breadth first, `depth` levels deep, skipping ignored
/// entries unless `ignored`, until `visit` returns false.
async fn walk(
    cx: &ToolContext,
    target: &Target,
    depth: u64,
    ignored: bool,
    mut visit: impl FnMut(Found) -> bool,
) -> Result<(), ToolError> {
    let mut queue = VecDeque::from([(target.path.clone(), 1_u64)]);
    let mut first = true;
    while let Some((dir, level)) = queue.pop_front() {
        let listing = cx
            .daemon()
            .request::<ListDir>(PathParams {
                workspace: cx.workspace().clone(),
                root: target.root,
                path: dir.clone(),
            })
            .await;
        let listing = match listing {
            Ok(listing) => listing,
            // The folder asked for must exist; one that vanished mid-walk
            // is skipped.
            Err(e) if first => return Err(e.into()),
            Err(e) => {
                tracing::debug!(dir, error = %e, "skipping a folder");
                continue;
            }
        };
        first = false;
        for entry in listing.entries {
            if entry.ignored && !ignored {
                continue;
            }
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{dir}/{}", entry.name)
            };
            let is_dir = entry.kind == EntryKind::Dir;
            if is_dir && !entry.symlink && level < depth {
                queue.push_back((path.clone(), level + 1));
            }
            if !visit(Found {
                path,
                dir: is_dir,
                size: entry.size,
            }) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// `list_files`.
#[derive(Debug)]
pub struct ListFilesTool;

impl Tool for ListFilesTool {
    fn definition(&self) -> ToolDefinition {
        let mut props = path_properties("Folder to list (default: the root)");
        props.insert(
            "depth".into(),
            json!({
                "type": "integer", "minimum": 1, "maximum": MAX_DEPTH,
                "description": "How many levels deep to list: 1 (the default) lists only the folder itself.",
            }),
        );
        props.insert(
            "include_ignored".into(),
            json!({
                "type": "boolean",
                "description": "Also list what .gitignore and friends ignore (build output, dependencies). Default false.",
            }),
        );
        props.insert(
            "limit".into(),
            json!({
                "type": "integer", "minimum": 1, "maximum": MAX_LIST,
                "description": format!("Most entries to return (default {DEFAULT_LIST})."),
            }),
        );
        ToolDefinition::new(
            "list_files",
            "List a folder of the Coqo workspace: files and subfolders (folders end in `/`), \
             paths relative to the root, ignored files left out unless `include_ignored`. Use \
             `depth` to list subfolders too.",
        )
        .with_title("List files")
        .read_only()
        .with_schema(json!({
            "type": "object",
            "properties": props,
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let target = target(cx, &args, Some("")).await?;
            let depth = opt_u64(&args, "depth").unwrap_or(1).clamp(1, MAX_DEPTH);
            let limit = opt_u64(&args, "limit")
                .unwrap_or(DEFAULT_LIST)
                .clamp(1, MAX_LIST) as usize;
            let mut found = Vec::new();
            let mut truncated = false;
            walk(
                cx,
                &target,
                depth,
                opt_bool(&args, "include_ignored"),
                |f| {
                    if found.len() == limit {
                        truncated = true;
                        return false;
                    }
                    found.push(f);
                    true
                },
            )
            .await?;
            found.sort_by(|a, b| a.path.cmp(&b.path));
            let mut text: Vec<String> = found.iter().map(Found::line).collect();
            if found.is_empty() {
                text.push("(empty)".into());
            }
            if truncated {
                text.push(format!(
                    "… more entries; raise `limit` (at most {MAX_LIST}) or list a subfolder"
                ));
            }
            let mut output = ToolOutput::json(json!({
                "root": target.root,
                "path": target.path,
                "absolute": target.absolute(),
                "entries": found.iter().map(Found::json).collect::<Vec<_>>(),
                "truncated": truncated,
            }));
            output.text = format!("{}:\n{}", target.absolute(), text.join("\n"));
            Ok(output)
        })
    }
}

/// How well `path` matches `query` (lowercase): higher is better, `None`
/// for no match.
fn score(path: &str, query: &str) -> Option<u32> {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name == query {
        return Some(400);
    }
    if name.starts_with(query) {
        return Some(300);
    }
    if name.contains(query) {
        return Some(250);
    }
    if lower.contains(query) {
        return Some(200);
    }
    // In order, not necessarily together (`flt` finds `files/tree.rs`).
    let mut chars = lower.chars();
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|q| chars.any(|c| c == q))
        .then_some(100)
}

/// `find_files`.
#[derive(Debug)]
pub struct FindFilesTool;

impl Tool for FindFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "find_files",
            "Find files in the Coqo workspace by name or path, like an editor's quick-open: \
             `query` matches file names first, then paths, then letters in order (`flt` finds \
             `files/tree.rs`). Case-insensitive. Ignored files are left out unless \
             `include_ignored`. Returns absolute paths, best matches first.",
        )
        .with_title("Find files")
        .read_only()
        .with_schema(json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1, "description": "Part of a file name or path." },
                "root": { "type": "integer", "minimum": 0, "description": "Only this root (id from workspace_info). Default: every root." },
                "include_ignored": { "type": "boolean", "description": "Also search ignored files. Default false." },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": MAX_MATCHES,
                    "description": format!("Most matches to return (default {DEFAULT_MATCHES})."),
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }))
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            let limit = opt_u64(&args, "limit")
                .unwrap_or(DEFAULT_MATCHES)
                .clamp(1, MAX_MATCHES) as usize;
            let only = opt_u64(&args, "root");
            let ignored = opt_bool(&args, "include_ignored");
            let ws = workspace(cx).await?;
            let mut matches: Vec<(u32, String)> = Vec::new();
            let mut seen = 0;
            for root in ws
                .roots
                .iter()
                .filter(|r| only.is_none_or(|id| u64::from(r.id.0) == id))
            {
                let target = Target {
                    ws: ws.clone(),
                    root: root.id,
                    path: String::new(),
                };
                walk(cx, &target, u64::MAX, ignored, |f| {
                    seen += 1;
                    if !f.dir
                        && let Some(s) = score(&f.path, &query)
                    {
                        matches.push((s, super::absolute(&target.ws, target.root, &f.path)));
                    }
                    seen < MAX_WALK
                })
                .await?;
            }
            // Best first; shorter paths first among equals.
            matches.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then(a.1.len().cmp(&b.1.len()))
                    .then(a.1.cmp(&b.1))
            });
            let total = matches.len();
            matches.truncate(limit);
            let paths: Vec<_> = matches.into_iter().map(|(_, p)| p).collect();
            let mut text = if paths.is_empty() {
                format!("No file matches `{query}`.")
            } else {
                paths.join("\n")
            };
            if total > paths.len() {
                text.push_str(&format!(
                    "\n… {} more; refine the query",
                    total - paths.len()
                ));
            }
            if seen >= MAX_WALK {
                text.push_str(&format!(
                    "\n(stopped after {MAX_WALK} entries; narrow it with `root`)"
                ));
            }
            let mut output = ToolOutput::json(json!({ "matches": paths, "total": total }));
            output.text = text;
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores() {
        assert_eq!(score("src/main.rs", "main.rs"), Some(400));
        assert_eq!(score("src/main.rs", "mai"), Some(300));
        assert_eq!(score("src/domain.rs", "main"), Some(250));
        assert_eq!(score("src/files/tree.rs", "files/t"), Some(200));
        assert_eq!(score("src/files/tree.rs", "flt"), Some(100));
        assert_eq!(score("src/files/tree.rs", "zzz"), None);
        assert_eq!(score("README.md", "readme"), Some(300));
    }
}
