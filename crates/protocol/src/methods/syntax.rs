// SPDX-License-Identifier: Apache-2.0

//! `syntax/*`: the always-on syntax layer of an open buffer (PLAN §7.1.1,
//! issue #42).
//!
//! The daemon parses open buffers with tree-sitter (`coqo-syntax`) and
//! answers with the symbols, outline and folding ranges of the buffer's
//! current text, unsaved edits included. No language server is involved, so
//! these work for every bundled language whether or not its ⚡ bolt is on.
//! Agents use them to find and name code ("the method `Circle::area`"), and
//! anchors use them to re-attach to a symbol after edits.
//!
//! Each result carries the `rev` it describes; the offsets are UTF-8 byte
//! offsets into the buffer text at that revision and lines are 0-based. A
//! file in no bundled language has `language: null` and empty lists.

use serde::{Deserialize, Serialize};

use super::buffer::BufferId;
use crate::method::Method;

/// Params of every `syntax/*` method.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SyntaxParams {
    /// An open buffer ([`buffer/open`](super::buffer::Open)).
    pub buffer: BufferId,
}

/// A byte range of the buffer text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ByteRange {
    /// First byte.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
}

impl From<std::ops::Range<usize>> for ByteRange {
    fn from(r: std::ops::Range<usize>) -> Self {
        Self {
            start: r.start,
            end: r.end,
        }
    }
}

/// What a symbol is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum SymbolKind {
    /// A module, namespace or package.
    Module,
    /// A class (also a Kotlin `object` or a Java record).
    Class,
    /// An interface or a Java annotation type.
    Interface,
    /// A trait.
    Trait,
    /// A struct or union.
    Struct,
    /// An enum.
    Enum,
    /// An enum member or variant.
    EnumMember,
    /// A type alias.
    TypeAlias,
    /// An `impl` block; its name is the implementing type.
    Impl,
    /// A free function.
    Function,
    /// A function inside a class, struct, trait, interface, impl or object.
    Method,
    /// A constructor.
    Constructor,
    /// A field or property.
    Field,
    /// A constant or static.
    Constant,
    /// A top-level variable.
    Variable,
    /// A macro.
    Macro,
    /// A Markdown heading (its section).
    Heading,
    /// A key in a data file (JSON, YAML, TOML).
    Key,
    /// A TOML table.
    Table,
    /// An HTML or XML element.
    Element,
    /// A CSS rule, at-rule or keyframe.
    Rule,
}

/// A named definition: a function, method, type, module, heading, key, …
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Symbol {
    /// The name, with runs of whitespace collapsed to one space.
    pub name: String,
    /// What it is.
    pub kind: SymbolKind,
    /// The whole definition.
    pub range: ByteRange,
    /// The name.
    pub name_range: ByteRange,
    /// First line of the definition (0-based).
    pub start_line: usize,
    /// Last line of the definition (0-based, inclusive).
    pub end_line: usize,
    /// Names of the symbols that contain this one, outermost first: a method
    /// `area` in `impl Circle` inside `mod shapes` has `["shapes", "Circle"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub container: Vec<String>,
}

/// Result of [`Symbols`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SymbolsResult {
    /// The revision the symbols describe.
    pub rev: u64,
    /// The language id (`rust`, `tsx`, …), or `null` for plain text.
    pub language: Option<String>,
    /// Every symbol, in document order (a container before what it holds).
    pub symbols: Vec<Symbol>,
}

/// A symbol and the symbols inside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OutlineItem {
    /// The name.
    pub name: String,
    /// What it is.
    pub kind: SymbolKind,
    /// The whole definition.
    pub range: ByteRange,
    /// The name.
    pub name_range: ByteRange,
    /// First line (0-based).
    pub start_line: usize,
    /// Last line (0-based, inclusive).
    pub end_line: usize,
    /// The symbols directly inside it, in document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<OutlineItem>,
}

/// Result of [`Outline`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct OutlineResult {
    /// The revision the outline describes.
    pub rev: u64,
    /// The language id, or `null` for plain text.
    pub language: Option<String>,
    /// The top-level symbols.
    pub items: Vec<OutlineItem>,
}

/// What a fold holds (LSP's `FoldingRangeKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum FoldKind {
    /// A block, body or literal.
    Region,
    /// A comment, or a run of line comments.
    Comment,
    /// A run of imports.
    Imports,
}

/// Lines that can be folded: `start_line` stays visible, the lines after it
/// through `end_line` are hidden.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct FoldingRange {
    /// First line (0-based).
    pub start_line: usize,
    /// Last line folded (0-based, inclusive).
    pub end_line: usize,
    /// What the fold holds.
    pub kind: FoldKind,
}

/// Result of [`Folding`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FoldingResult {
    /// The revision the ranges describe.
    pub rev: u64,
    /// The language id, or `null` for plain text.
    pub language: Option<String>,
    /// The ranges, sorted by start line, at most one per line.
    pub ranges: Vec<FoldingRange>,
}

/// `syntax/symbols`: every symbol of an open buffer, flat, with container
/// paths.
#[derive(Debug)]
pub enum Symbols {}
impl Method for Symbols {
    const NAME: &'static str = "syntax/symbols";
    type Params = SyntaxParams;
    type Result = SymbolsResult;
}

/// `syntax/outline`: the symbols of an open buffer as a tree.
#[derive(Debug)]
pub enum Outline {}
impl Method for Outline {
    const NAME: &'static str = "syntax/outline";
    type Params = SyntaxParams;
    type Result = OutlineResult;
}

/// `syntax/folding`: the foldable line ranges of an open buffer.
#[derive(Debug)]
pub enum Folding {}
impl Method for Folding {
    const NAME: &'static str = "syntax/folding";
    type Params = SyntaxParams;
    type Result = FoldingResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn symbol_wire_shape() {
        let s = Symbol {
            name: "area".into(),
            kind: SymbolKind::EnumMember,
            range: (4..20).into(),
            name_range: (7..11).into(),
            start_line: 1,
            end_line: 3,
            container: vec!["shapes".into()],
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({
                "name": "area",
                "kind": "enumMember",
                "range": {"start": 4, "end": 20},
                "nameRange": {"start": 7, "end": 11},
                "startLine": 1,
                "endLine": 3,
                "container": ["shapes"],
            })
        );
        let top: Symbol = serde_json::from_value(json!({
            "name": "f", "kind": "function", "range": {"start": 0, "end": 1},
            "nameRange": {"start": 0, "end": 1}, "startLine": 0, "endLine": 0
        }))
        .unwrap();
        assert!(top.container.is_empty());
    }
}
