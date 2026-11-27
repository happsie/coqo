// SPDX-License-Identifier: Apache-2.0

//! `editor_context` and `editor_reveal`: what the user sees in Coqo, and
//! showing them a place (`editor/*`).

use coqo_protocol::methods::editor::{
    self as edm, EditorTab, GetContextParams, Position, Range, RevealParams, Selection,
};
use coqo_protocol::methods::workspace::WorkspaceInfo;
use serde_json::{Map, Value, json};

use super::{absolute, opt_u64, path_properties, target, workspace};
use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

const NO_WINDOW: &str = "No Coqo window shows this workspace right now (the editor is closed \
                         or showing another workspace), so there is no editor context.";

/// `{line, column}`, 1-based.
fn position(p: Position) -> Value {
    json!({ "line": p.line + 1, "column": p.character + 1 })
}

fn selection_json(s: &Selection) -> Value {
    let mut v = json!({ "start": position(s.range.start), "end": position(s.range.end) });
    if let Some(text) = &s.text {
        v["text"] = json!(text);
        if s.truncated {
            v["truncated"] = json!(true);
        }
    }
    v
}

/// `12:5` or `12:5-14:1`, 1-based.
fn range_text(r: &Range) -> String {
    let at = |p: Position| format!("{}:{}", p.line + 1, p.character + 1);
    if r.is_empty() {
        at(r.start)
    } else {
        format!("{}-{}", at(r.start), at(r.end))
    }
}

fn tab_json(ws: &WorkspaceInfo, tab: &EditorTab) -> Value {
    let mut v = json!({
        "path": absolute(ws, tab.root, &tab.path),
        "root": tab.root,
        "relativePath": tab.path,
    });
    if tab.dirty {
        v["unsaved"] = json!(true);
    }
    if !tab.selections.is_empty() {
        v["selections"] = Value::Array(tab.selections.iter().map(selection_json).collect());
    }
    if let Some(visible) = tab.visible {
        v["visibleLines"] = json!({ "start": visible.start + 1, "end": visible.end + 1 });
    }
    v
}

/// The workspace and session, for `editor_context`'s result.
fn workspace_json(ws: &WorkspaceInfo, session: &str) -> Value {
    json!({
        "id": ws.id,
        "name": ws.name,
        "session": session,
        "roots": ws.roots.iter().map(|r| json!({
            "id": r.id,
            "name": r.name,
            "path": r.path,
        })).collect::<Vec<_>>(),
    })
}

/// The workspace and session, as the first lines of `editor_context`'s text.
fn workspace_text(ws: &WorkspaceInfo, session: &str) -> Vec<String> {
    let mut text = vec![format!("Workspace: {} (session {session})", ws.name)];
    match ws.roots.as_slice() {
        [] => text.push("No folder roots.".into()),
        [root] => text.push(format!("Root: {}", root.path)),
        roots => {
            text.push("Roots (id: path):".into());
            for r in roots {
                text.push(format!("- {}: {}", r.id.0, r.path));
            }
        }
    }
    text
}

/// `editor_context`.
#[derive(Debug)]
pub struct EditorContextTool;

impl Tool for EditorContextTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "editor_context",
            "What the user is looking at in Coqo right now: the active file with its cursors \
             and selections (and the selected text), the lines on screen, and the other open \
             tabs (unsaved ones marked), plus the workspace's folder roots and this session's \
             id. Call it first whenever the user refers to \"this\", \"here\", \"the \
             selection\", \"this function\" or \"the open file\", or asks about what they are \
             looking at, instead of guessing which code they mean. Lines and columns are \
             1-based. When no Coqo window shows the workspace it says so and gives only the \
             roots.",
        )
        .with_title("Editor context")
        .read_only()
    }

    fn call<'a>(
        &'a self,
        cx: &'a ToolContext,
        _args: Map<String, Value>,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let ws = workspace(cx).await?;
            let context = cx
                .daemon()
                .request::<edm::GetContext>(GetContextParams {
                    workspace: cx.workspace().clone(),
                })
                .await?;
            let about = workspace_json(&ws, cx.session());
            let mut text = workspace_text(&ws, cx.session());
            let Some(window) = context.windows.first() else {
                text.push(NO_WINDOW.into());
                let mut output = ToolOutput::json(json!({ "attached": false, "workspace": about }));
                output.text = text.join("\n");
                return Ok(output);
            };
            let tabs = &window.context.tabs;
            let active = tabs.iter().find(|t| t.active);

            match active {
                Some(tab) => {
                    text.push(format!(
                        "Active file: {}{}",
                        absolute(&ws, tab.root, &tab.path),
                        if tab.dirty { " (unsaved changes)" } else { "" }
                    ));
                    if let Some(v) = tab.visible {
                        text.push(format!("Visible lines: {}-{}", v.start + 1, v.end + 1));
                    }
                    for (i, s) in tab.selections.iter().enumerate() {
                        let what = if s.range.is_empty() {
                            "Cursor"
                        } else {
                            "Selection"
                        };
                        let primary = if i == 0 && tab.selections.len() > 1 {
                            " (primary)"
                        } else {
                            ""
                        };
                        text.push(format!("{what}{primary}: {}", range_text(&s.range)));
                        if let Some(selected) = &s.text {
                            let cut = if s.truncated { "\n[… truncated]" } else { "" };
                            text.push(format!("```\n{selected}{cut}\n```"));
                        }
                    }
                }
                None => text.push("No file tab is active.".into()),
            }
            let others: Vec<_> = tabs.iter().filter(|t| !t.active).collect();
            if !others.is_empty() {
                text.push("Other open tabs:".into());
                for tab in others {
                    let unsaved = if tab.dirty { " (unsaved)" } else { "" };
                    text.push(format!("- {}{unsaved}", absolute(&ws, tab.root, &tab.path)));
                }
            }
            if context.windows.len() > 1 {
                text.push(format!(
                    "({} more Coqo windows show this workspace.)",
                    context.windows.len() - 1
                ));
            }

            let mut output = ToolOutput::json(json!({
                "attached": true,
                "workspace": about,
                "focused": window.context.focused,
                "activeFile": active.map(|t| tab_json(&ws, t)),
                "openTabs": tabs.iter().map(|t| tab_json(&ws, t)).collect::<Vec<_>>(),
                "otherWindows": context.windows.iter().skip(1).map(|w| json!({
                    "focused": w.context.focused,
                    "openTabs": w.context.tabs.iter().map(|t| tab_json(&ws, t)).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }));
            output.text = text.join("\n");
            Ok(output)
        })
    }
}

/// A 1-based argument as 0-based, if given.
fn zero_based(args: &Map<String, Value>, key: &str) -> Option<u32> {
    opt_u64(args, key).map(|n| u32::try_from(n.saturating_sub(1)).unwrap_or(u32::MAX))
}

/// The range a reveal asks for, from 1-based `line`, `column`, `end_line`
/// and `end_column`. Without an end it is a cursor; an `end_line` without
/// `end_column` runs to the end of that line.
fn reveal_range(args: &Map<String, Value>) -> Result<Option<Range>, ToolError> {
    let Some(line) = zero_based(args, "line") else {
        if args.contains_key("end_line") || args.contains_key("column") {
            return Err(ToolError::InvalidArguments(
                "`column` and `end_line` need a `line`".into(),
            ));
        }
        return Ok(None);
    };
    let start = Position::new(line, zero_based(args, "column").unwrap_or(0));
    let end = match (zero_based(args, "end_line"), zero_based(args, "end_column")) {
        (None, None) => start,
        (None, Some(c)) => Position::new(line, c),
        (Some(l), Some(c)) => Position::new(l, c),
        (Some(l), None) => Position::new(l.saturating_add(1), 0),
    };
    if end < start {
        return Err(ToolError::InvalidArguments(
            "the range ends before it starts".into(),
        ));
    }
    Ok(Some(Range::new(start, end)))
}

/// `editor_reveal`.
#[derive(Debug)]
pub struct EditorRevealTool;

impl Tool for EditorRevealTool {
    fn definition(&self) -> ToolDefinition {
        let mut props = path_properties("File to open");
        let line = |what: &str| json!({ "type": "integer", "minimum": 1, "description": what });
        props.insert(
            "line".into(),
            line("1-based line to show (default: the top)."),
        );
        props.insert(
            "column".into(),
            line("1-based column on `line` (default 1)."),
        );
        props.insert(
            "end_line".into(),
            line("1-based last line to select; the selection runs to its end unless `end_column` is given."),
        );
        props.insert(
            "end_column".into(),
            line("1-based column where the selection ends (exclusive)."),
        );
        ToolDefinition::new(
            "editor_reveal",
            "Show the user code in Coqo: open the file in an editor tab, scroll to `line` and \
             select from `line`/`column` to `end_line`/`end_column` (1-based). Use it to show \
             the user the code you are talking about, or code you just changed that they \
             should look at. It moves the user's cursor, so call it once per place you want \
             them to see, not for files you only read yourself and not in a loop. Does not \
             edit anything.",
        )
        .with_title("Reveal in editor")
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
            let range = reveal_range(&args)?;
            let target = target(cx, &args, None).await?;
            let result = cx
                .daemon()
                .request::<edm::Reveal>(RevealParams {
                    workspace: cx.workspace().clone(),
                    root: target.root,
                    path: target.path.clone(),
                    range,
                })
                .await?;
            let path = target.absolute();
            let at = range.map_or(String::new(), |r| format!(" at {}", range_text(&r)));
            let text = if result.delivered {
                format!("Opened {path}{at} in Coqo.")
            } else {
                format!(
                    "No Coqo window shows this workspace, so nobody saw it; {path}{at} will not \
                     be opened."
                )
            };
            let mut output = ToolOutput::json(json!({
                "delivered": result.delivered,
                "path": path,
                "range": range.map(|r| json!({ "start": position(r.start), "end": position(r.end) })),
            }));
            output.text = text;
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn reveal_ranges_are_one_based() {
        assert_eq!(reveal_range(&args(json!({}))).unwrap(), None);
        let p = Position::new;
        assert_eq!(
            reveal_range(&args(json!({ "line": 3 }))).unwrap(),
            Some(Range::new(p(2, 0), p(2, 0)))
        );
        assert_eq!(
            reveal_range(&args(json!({ "line": 3, "column": 5, "end_column": 9 }))).unwrap(),
            Some(Range::new(p(2, 4), p(2, 8)))
        );
        assert_eq!(
            reveal_range(&args(json!({ "line": 3, "end_line": 4 }))).unwrap(),
            Some(Range::new(p(2, 0), p(4, 0)))
        );
        assert_eq!(
            reveal_range(&args(
                json!({ "line": 3, "column": 2, "end_line": 5, "end_column": 1 })
            ))
            .unwrap(),
            Some(Range::new(p(2, 1), p(4, 0)))
        );
        assert!(reveal_range(&args(json!({ "line": 5, "end_line": 2 }))).is_err());
        assert!(reveal_range(&args(json!({ "column": 5 }))).is_err());
    }
}
