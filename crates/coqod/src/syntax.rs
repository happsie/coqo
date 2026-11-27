// SPDX-License-Identifier: Apache-2.0

//! `syntax/*`: symbols, outline and folding of open buffers (PLAN §7.1.1,
//! issue #42), from `coqo-syntax`.
//!
//! The service keeps one parsed [`Document`] per buffer it was asked about,
//! with the revision it reflects. A request brings it up to date with the
//! buffer's changes since then (an incremental re-parse), or parses the whole
//! text if the buffer's log no longer has them. Parsing and queries run on a
//! blocking thread. Parses of buffers that were unloaded are dropped on the
//! next request.
//!
//! **Access:** a session token (an agent's `coqod mcp`) may ask about
//! buffers of its own workspace; the router checks it before the handler
//! runs, as for `buffer/changes`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use coqo_protocol::methods::buffer::BufferId;
use coqo_protocol::methods::syntax::{
    self as synm, ByteRange, FoldingResult, OutlineResult, SymbolsResult, SyntaxParams,
};
use coqo_protocol::{Access, ErrorObject, Router};
use coqo_syntax::{Document, Language, TextEdit};

use crate::buffers::{BufferService, SyntaxView};

/// A buffer's parse.
#[derive(Debug, Default)]
struct Parsed {
    /// The revision it reflects; `None` before the first parse.
    rev: Option<u64>,
    /// `None` for a file in no bundled language.
    doc: Option<Document>,
}

/// The daemon's syntax service.
#[derive(Debug)]
pub struct SyntaxService {
    buffers: Arc<BufferService>,
    parsed: Mutex<HashMap<BufferId, Arc<Mutex<Parsed>>>>,
}

impl SyntaxService {
    /// A service over `buffers`.
    pub fn new(buffers: Arc<BufferService>) -> Arc<Self> {
        Arc::new(Self {
            buffers,
            parsed: Mutex::new(HashMap::new()),
        })
    }

    /// How many buffers have a parse. For tests.
    #[cfg(test)]
    fn parsed_buffers(&self) -> usize {
        self.parsed.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Bring the parse of `buffer` up to date and run `f` on it, on a
    /// blocking thread. `f` gets the revision and the document (`None` for
    /// plain text).
    async fn with_doc<T: Send + 'static>(
        self: &Arc<Self>,
        buffer: BufferId,
        f: impl FnOnce(u64, Option<&Document>) -> T + Send + 'static,
    ) -> Result<T, ErrorObject> {
        let entry = {
            let mut parsed = self.parsed.lock().unwrap_or_else(|e| e.into_inner());
            parsed.retain(|id, _| *id == buffer || self.buffers.rev(*id).is_some());
            Arc::clone(parsed.entry(buffer).or_default())
        };
        let since = entry.lock().unwrap_or_else(|e| e.into_inner()).rev;
        let view = match self.buffers.syntax_view(buffer, since) {
            Ok(view) => view,
            Err(e) => {
                self.forget(buffer);
                return Err(e);
            }
        };
        let svc = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut parsed = entry.lock().unwrap_or_else(|e| e.into_inner());
            let mut view = view;
            if parsed.rev != since {
                // Another request moved the parse on meanwhile.
                view = svc.buffers.syntax_view(buffer, parsed.rev)?;
            }
            if !apply(&mut parsed, view) {
                apply(&mut parsed, svc.buffers.syntax_view(buffer, None)?);
            }
            Ok(f(parsed.rev.unwrap_or_default(), parsed.doc.as_ref()))
        })
        .await
        .map_err(|e| ErrorObject::internal(format!("syntax task failed: {e}")))?
    }

    fn forget(&self, buffer: BufferId) {
        self.parsed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&buffer);
    }

    /// `syntax/symbols`.
    pub async fn symbols(self: &Arc<Self>, p: SyntaxParams) -> Result<SymbolsResult, ErrorObject> {
        self.with_doc(p.buffer, |rev, doc| SymbolsResult {
            rev,
            language: language(doc),
            symbols: doc
                .map(|d| d.symbols().into_iter().map(symbol).collect())
                .unwrap_or_default(),
        })
        .await
    }

    /// `syntax/outline`.
    pub async fn outline(self: &Arc<Self>, p: SyntaxParams) -> Result<OutlineResult, ErrorObject> {
        self.with_doc(p.buffer, |rev, doc| OutlineResult {
            rev,
            language: language(doc),
            items: doc
                .map(|d| d.outline().into_iter().map(outline_item).collect())
                .unwrap_or_default(),
        })
        .await
    }

    /// `syntax/folding`.
    pub async fn folding(self: &Arc<Self>, p: SyntaxParams) -> Result<FoldingResult, ErrorObject> {
        self.with_doc(p.buffer, |rev, doc| FoldingResult {
            rev,
            language: language(doc),
            ranges: doc
                .map(|d| d.folding_ranges().into_iter().map(fold).collect())
                .unwrap_or_default(),
        })
        .await
    }
}

/// Bring `parsed` to `view`'s revision: apply its changes (an incremental
/// re-parse), or parse its text. Returns `false` if the changes did not
/// apply, so the text is needed.
fn apply(parsed: &mut Parsed, view: SyntaxView) -> bool {
    match (view.changes, view.text) {
        (_, Some(text)) => {
            let path = view.path.to_string_lossy();
            parsed.doc = Language::detect(&path, &text).map(|lang| Document::new(lang, text));
        }
        (Some(changes), None) => {
            // A file in no bundled language stays plain text.
            if let Some(doc) = parsed.doc.as_mut() {
                for change in changes {
                    let edits: Vec<TextEdit> = change
                        .into_iter()
                        .map(|e| TextEdit::new(e.start..e.end, e.text))
                        .collect();
                    if let Err(e) = doc.edit(&edits) {
                        tracing::warn!(error = %e, "a buffer change did not apply to its parse");
                        return false;
                    }
                }
            }
        }
        (None, None) => return false,
    }
    parsed.rev = Some(view.rev);
    true
}

fn language(doc: Option<&Document>) -> Option<String> {
    doc.map(|d| d.language().id().to_owned())
}

fn kind(k: coqo_syntax::SymbolKind) -> synm::SymbolKind {
    use coqo_syntax::SymbolKind as S;
    use synm::SymbolKind as W;
    match k {
        S::Module => W::Module,
        S::Class => W::Class,
        S::Interface => W::Interface,
        S::Trait => W::Trait,
        S::Struct => W::Struct,
        S::Enum => W::Enum,
        S::EnumMember => W::EnumMember,
        S::TypeAlias => W::TypeAlias,
        S::Impl => W::Impl,
        S::Function => W::Function,
        S::Method => W::Method,
        S::Constructor => W::Constructor,
        S::Field => W::Field,
        S::Constant => W::Constant,
        S::Variable => W::Variable,
        S::Macro => W::Macro,
        S::Heading => W::Heading,
        S::Key => W::Key,
        S::Table => W::Table,
        S::Element => W::Element,
        S::Rule => W::Rule,
    }
}

fn symbol(s: coqo_syntax::Symbol) -> synm::Symbol {
    synm::Symbol {
        name: s.name,
        kind: kind(s.kind),
        range: ByteRange::from(s.range),
        name_range: ByteRange::from(s.name_range),
        start_line: s.start_line,
        end_line: s.end_line,
        container: s.container,
    }
}

fn outline_item(item: coqo_syntax::OutlineItem) -> synm::OutlineItem {
    let s = item.symbol;
    synm::OutlineItem {
        name: s.name,
        kind: kind(s.kind),
        range: ByteRange::from(s.range),
        name_range: ByteRange::from(s.name_range),
        start_line: s.start_line,
        end_line: s.end_line,
        children: item.children.into_iter().map(outline_item).collect(),
    }
}

fn fold(f: coqo_syntax::FoldingRange) -> synm::FoldingRange {
    synm::FoldingRange {
        start_line: f.start_line,
        end_line: f.end_line,
        kind: match f.kind {
            coqo_syntax::FoldKind::Region => synm::FoldKind::Region,
            coqo_syntax::FoldKind::Comment => synm::FoldKind::Comment,
            coqo_syntax::FoldKind::Imports => synm::FoldKind::Imports,
        },
    }
}

/// Register every `syntax/*` method on `router`. Session tokens may call
/// them for buffers of their own workspace.
pub fn register(router: Router, svc: Arc<SyntaxService>) -> Router {
    let access = || {
        let buffers = Arc::clone(&svc.buffers);
        Access::check(move |scope, p: &SyntaxParams| buffers.require_buffer_access(scope, p.buffer))
    };
    let symbols = Arc::clone(&svc);
    let outline = Arc::clone(&svc);
    let folding = Arc::clone(&svc);
    router
        .method_with::<synm::Symbols, _, _>(access(), move |_ctx, p| {
            let s = Arc::clone(&symbols);
            async move { s.symbols(p).await }
        })
        .method_with::<synm::Outline, _, _>(access(), move |_ctx, p| {
            let s = Arc::clone(&outline);
            async move { s.outline(p).await }
        })
        .method_with::<synm::Folding, _, _>(access(), move |_ctx, p| {
            let s = Arc::clone(&folding);
            async move { s.folding(p).await }
        })
}

#[cfg(test)]
mod tests;
