// SPDX-License-Identifier: Apache-2.0

//! Symbol tags and the document outline, from each language's
//! `queries/<lang>/symbols.scm`.
//!
//! A symbols query uses the tree-sitter tags conventions: the whole
//! definition is captured as `@definition.<kind>` and its name as `@name`,
//! for example `(function_item name: (identifier) @name) @definition.function`.
//! `<kind>` is a [`SymbolKind`] in snake case.

use std::fmt;
use std::ops::Range;

use tree_sitter::{QueryCursor, StreamingIterator as _};

use crate::document::{Document, LineIndex};

/// What a symbol is.
///
/// Adding a kind is a protocol change: `coqod` maps every kind to its wire
/// form exhaustively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

impl SymbolKind {
    /// Every kind.
    pub const ALL: [SymbolKind; 21] = [
        Self::Module,
        Self::Class,
        Self::Interface,
        Self::Trait,
        Self::Struct,
        Self::Enum,
        Self::EnumMember,
        Self::TypeAlias,
        Self::Impl,
        Self::Function,
        Self::Method,
        Self::Constructor,
        Self::Field,
        Self::Constant,
        Self::Variable,
        Self::Macro,
        Self::Heading,
        Self::Key,
        Self::Table,
        Self::Element,
        Self::Rule,
    ];

    /// The snake-case name, as in `@definition.<name>` captures.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::EnumMember => "enum_member",
            Self::TypeAlias => "type_alias",
            Self::Impl => "impl",
            Self::Function => "function",
            Self::Method => "method",
            Self::Constructor => "constructor",
            Self::Field => "field",
            Self::Constant => "constant",
            Self::Variable => "variable",
            Self::Macro => "macro",
            Self::Heading => "heading",
            Self::Key => "key",
            Self::Table => "table",
            Self::Element => "element",
            Self::Rule => "rule",
        }
    }

    /// The kind named `name` (see [`as_str`](Self::as_str)).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }

    /// Whether functions directly inside this kind are methods.
    fn holds_methods(self) -> bool {
        matches!(
            self,
            Self::Class | Self::Interface | Self::Trait | Self::Struct | Self::Enum | Self::Impl
        )
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A named definition in a document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Symbol {
    /// The name, with runs of whitespace collapsed to one space.
    pub name: String,
    /// What it is.
    pub kind: SymbolKind,
    /// The whole definition, in UTF-8 bytes.
    pub range: Range<usize>,
    /// The name, in UTF-8 bytes.
    pub name_range: Range<usize>,
    /// The first line of the definition (0-based).
    pub start_line: usize,
    /// The last line of the definition (0-based, inclusive).
    pub end_line: usize,
    /// The names of the symbols that contain this one, outermost first: a
    /// method `area` in `impl Shape for Circle` inside `mod shapes` has
    /// `["shapes", "Circle"]`.
    pub container: Vec<String>,
}

impl Symbol {
    /// The container path and the name, joined with `separator`:
    /// `shapes::Circle::area` for `"::"`.
    pub fn qualified_name(&self, separator: &str) -> String {
        let mut path = self.container.join(separator);
        if !path.is_empty() {
            path.push_str(separator);
        }
        path.push_str(&self.name);
        path
    }
}

/// A symbol and the symbols inside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutlineItem {
    /// The symbol.
    pub symbol: Symbol,
    /// The symbols directly inside it, in document order.
    pub children: Vec<OutlineItem>,
}

/// Run the symbols query over the whole document.
pub(crate) fn symbols(doc: &Document) -> Vec<Symbol> {
    let query = &doc.language().queries().symbols;
    let names = query.capture_names();
    let text = doc.text();
    // (definition, name, kind, pattern): when two patterns match the same
    // definition, the one earlier in the query wins.
    let mut found: Vec<(Range<usize>, Range<usize>, SymbolKind, usize)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, doc.tree().root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        let mut def = None;
        let mut name = None;
        for c in m.captures {
            let capture = names[c.index as usize];
            if capture == "name" {
                name = Some(c.node.byte_range());
            } else if let Some(kind) = capture.strip_prefix("definition.") {
                let kind = SymbolKind::from_name(kind)
                    .unwrap_or_else(|| panic!("unknown symbol kind @{capture}"));
                def = Some((c.node, kind));
            }
        }
        if let (Some((node, kind)), Some(name)) = (def, name) {
            let range = trim_end(text, node.byte_range());
            found.push((range, name, kind, m.pattern_index));
        }
    }
    // Outer definitions first, so containment is a stack walk.
    found.sort_by_key(|(range, name, _, pattern)| {
        (
            range.start,
            std::cmp::Reverse(range.end),
            name.start,
            *pattern,
        )
    });
    found.dedup_by(|b, a| a.0 == b.0 && a.1 == b.1);

    let lines = LineIndex::new(text);
    let mut out: Vec<Symbol> = Vec::with_capacity(found.len());
    // Indexes into `out` of the symbols containing the current one.
    let mut stack: Vec<usize> = Vec::new();
    for (range, name_range, mut kind, _) in found {
        while let Some(&top) = stack.last() {
            if out[top].range.end >= range.end && out[top].range.start <= range.start {
                break;
            }
            stack.pop();
        }
        let parent = stack.last().map(|&i| &out[i]);
        if kind == SymbolKind::Function && parent.is_some_and(|p| p.kind.holds_methods()) {
            kind = SymbolKind::Method;
        }
        let container = parent
            .map(|p| {
                let mut c = p.container.clone();
                c.push(p.name.clone());
                c
            })
            .unwrap_or_default();
        let start_line = lines.line(range.start);
        let end_line = lines
            .line(range.end.max(range.start + 1) - 1)
            .max(start_line);
        out.push(Symbol {
            name: clean_name(&text[name_range.clone()]),
            kind,
            range,
            name_range,
            start_line,
            end_line,
            container,
        });
        stack.push(out.len() - 1);
    }
    out
}

/// Nest `symbols` (in [`symbols`] order) by containment.
pub(crate) fn outline(symbols: Vec<Symbol>) -> Vec<OutlineItem> {
    type Symbols = std::iter::Peekable<std::vec::IntoIter<Symbol>>;
    fn take(iter: &mut Symbols, depth: usize) -> Vec<OutlineItem> {
        let mut items = Vec::new();
        while let Some(next) = iter.peek() {
            if next.container.len() < depth {
                break;
            }
            let symbol = iter.next().expect("peeked");
            let children = take(iter, depth + 1);
            items.push(OutlineItem { symbol, children });
        }
        items
    }
    take(&mut symbols.into_iter().peekable(), 0)
}

/// `range` without trailing whitespace. Some grammars end a node after the
/// line break and indentation that follow it (Markdown sections, YAML pairs,
/// HTML void elements); a definition ends at its last character.
pub(crate) fn trim_end(text: &str, range: Range<usize>) -> Range<usize> {
    let trimmed = text[range.clone()].trim_end().len();
    range.start..range.start + trimmed
}

fn clean_name(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}
