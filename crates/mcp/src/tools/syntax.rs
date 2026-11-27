// SPDX-License-Identifier: Apache-2.0

//! `file_outline`: a file's symbols from the always-on syntax layer
//! (tree-sitter, PLAN §7.1.1). No language server needed.

use coqo_protocol::methods::buffer::{self as bufm, CloseParams, OpenParams};
use coqo_protocol::methods::syntax::{self as synm, OutlineItem, SymbolKind, SyntaxParams};
use serde_json::{Map, Value, json};

use super::{path_properties, target};
use crate::registry::{BoxFuture, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};

fn kind(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "module",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Trait => "trait",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::EnumMember => "enum member",
        SymbolKind::TypeAlias => "type",
        SymbolKind::Impl => "impl",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Constructor => "constructor",
        SymbolKind::Field => "field",
        SymbolKind::Constant => "constant",
        SymbolKind::Variable => "variable",
        SymbolKind::Macro => "macro",
        SymbolKind::Heading => "heading",
        SymbolKind::Key => "key",
        SymbolKind::Table => "table",
        SymbolKind::Element => "element",
        SymbolKind::Rule => "rule",
    }
}

/// The outline as indented lines, with 1-based line ranges.
fn render(items: &[OutlineItem], depth: usize, out: &mut Vec<String>) {
    for item in items {
        let lines = if item.start_line == item.end_line {
            format!("line {}", item.start_line + 1)
        } else {
            format!("lines {}-{}", item.start_line + 1, item.end_line + 1)
        };
        out.push(format!(
            "{}{} {} ({lines})",
            "  ".repeat(depth),
            kind(item.kind),
            item.name
        ));
        render(&item.children, depth + 1, out);
    }
}

fn to_json(items: &[OutlineItem]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|i| {
                let mut v = json!({
                    "name": i.name,
                    "kind": kind(i.kind),
                    "startLine": i.start_line + 1,
                    "endLine": i.end_line + 1,
                });
                if !i.children.is_empty() {
                    v["children"] = to_json(&i.children);
                }
                v
            })
            .collect(),
    )
}

/// `file_outline`.
#[derive(Debug)]
pub struct FileOutlineTool;

impl Tool for FileOutlineTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "file_outline",
            "The outline of a file in the Coqo workspace: its classes, functions, methods, \
             types, headings, … as a tree, each with its kind and 1-based line range. From \
             tree-sitter, so it works for every supported language without a language server, \
             and includes unsaved edits. Use it to find where something is defined before \
             reading a long file.",
        )
        .with_title("File outline")
        .read_only()
        .with_schema(json!({
            "type": "object",
            "properties": path_properties("File path"),
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
            let target = target(cx, &args, None).await?;
            let open = cx
                .daemon()
                .request::<bufm::Open>(OpenParams {
                    workspace: cx.workspace().clone(),
                    root: target.root,
                    path: target.path.clone(),
                    replica: None,
                })
                .await?;
            let outline = cx
                .daemon()
                .request::<synm::Outline>(SyntaxParams {
                    buffer: open.buffer,
                })
                .await;
            // Done with the buffer either way.
            if let Err(e) = cx
                .daemon()
                .request::<bufm::Close>(CloseParams {
                    buffer: open.buffer,
                    replica: open.replica,
                })
                .await
            {
                tracing::debug!(error = %e, "cannot close the outline's buffer");
            }
            let outline = outline?;
            let path = target.absolute();
            let mut lines = Vec::new();
            render(&outline.items, 0, &mut lines);
            let text = match (&outline.language, lines.is_empty()) {
                (None, _) => format!("{path}: no outline (not a language Coqo parses)"),
                (Some(lang), true) => format!("{path} ({lang}): no symbols"),
                (Some(lang), false) => format!("{path} ({lang}):\n{}", lines.join("\n")),
            };
            let mut output = ToolOutput::json(json!({
                "path": path,
                "language": outline.language,
                "items": to_json(&outline.items),
            }));
            output.text = text;
            Ok(output)
        })
    }
}
