// SPDX-License-Identifier: Apache-2.0

//! Coqo's always-on syntax layer (PLAN §7.1.1): what every file gets without
//! starting a language server.
//!
//! - **Languages** ([`Language`], [`languages`]): the bundled tree-sitter
//!   grammars and how files map to them, by file name, extension or `#!`
//!   line.
//! - **Documents** ([`Document`]): a parsed text that re-parses incrementally
//!   after [`edit`](Document::edit)s.
//! - **Symbols** ([`Symbol`], [`OutlineItem`]): named definitions found by
//!   tree-sitter tags queries, each with its kind, ranges and container path,
//!   flat or as an outline tree. Anchors re-attach to code by them, and
//!   agents ask about them.
//! - **Folding** ([`FoldingRange`]): foldable line ranges.
//!
//! This crate has no GPUI dependency: the daemon serves it to agents
//! (`syntax/*` methods) and the UI uses its registry. Highlighting is drawn
//! by gpui-component's highlighter on the same grammars (ADR 0011).
//!
//! The built-in mod `coqo.syntax` ([`builtin`]) contributes the bundled
//! languages to the `languages` contribution point.
//!
//! Offsets are UTF-8 byte offsets and lines are 0-based, as in the buffer
//! protocol.

pub mod builtin;
mod document;
mod folding;
mod language;
mod symbols;

pub use document::{Document, EditError, TextEdit};
pub use folding::{FoldKind, FoldingRange};
pub use language::{Language, languages};
pub use symbols::{OutlineItem, Symbol, SymbolKind};
pub use tree_sitter;
