// SPDX-License-Identifier: Apache-2.0

//! `search/*`: project search and replace (PLAN §7.1, issue #45), served
//! from the `coqo.search` mod's service.
//!
//! Every call resolves `coqo.search@1` from the mod registry, so a disabled
//! search mod turns these methods off (`SEARCH_UNAVAILABLE`) and a mod
//! providing the service replaces the built-in one.
//!
//! - **`search/run`** is a topic whose subscription *is* the search. The
//!   walk runs on a blocking thread; files with matches are batched (the
//!   first at once, then every [`BATCH_WINDOW`]) into `files` events, and a
//!   `done` event with the summary ends the stream. Dropping the
//!   subscription (`coqo/unsubscribe`, a new query, a closed connection)
//!   sets the walk's cancel flag. Open buffers of the workspace are
//!   searched in their current text, unsaved edits included.
//! - **`search/replace`** recomputes the matches in each file's current
//!   text with the same matcher, so it replaces exactly what a search would
//!   show now. An open buffer gets one `replace` change (unsaved, revertible
//!   with `buffer/revertChange`); a file that is not open is written
//!   atomically ([`BufferService::replace_in_file`]). Buffers the caller
//!   holds come back as edits for its own editor, so they land in its undo
//!   history.
//!
//! **Access:** both need the daemon token. Agents search with their own
//! tools (PLAN §7.3: no MCP tool for what harnesses already have).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use coqo_mod_api::Service as _;
use coqo_mod_search::{self as ms, Matcher, MatchAt, Root, Search, SearchError};
use coqo_protocol::methods::buffer::TextEdit;
use coqo_protocol::methods::codes;
use coqo_protocol::methods::search::{
    self as sm, FileMatches, FileReplaced, ReplaceOutcome, ReplaceParams, ReplaceResult,
    RunParams, SearchEvent, SearchMatch, SearchQuery, SearchSummary,
};
use coqo_protocol::methods::workspace::RootId;
use coqo_protocol::{Access, ErrorObject, EventFeed, Router, Sequenced};
use futures_util::StreamExt as _;
use tokio::sync::mpsc;

use crate::buffers::{BufferService, FileReplace};
use crate::mods::Mods;
use crate::workspace::WorkspaceService;

/// After the first file of a search, results are sent in batches this far
/// apart (or of [`BATCH_FILES`] files), so a big search is a few hundred
/// events rather than one per file.
pub const BATCH_WINDOW: Duration = Duration::from_millis(30);
/// Most files in one `files` event.
pub const BATCH_FILES: usize = 256;

/// The daemon's search service.
#[derive(Debug)]
pub struct SearchService {
    workspaces: Arc<WorkspaceService>,
    buffers: Arc<BufferService>,
    mods: Mods,
    /// How each walk ended, for tests.
    #[cfg(test)]
    walks: Arc<std::sync::Mutex<Vec<ms::Summary>>>,
}

fn error(e: SearchError) -> ErrorObject {
    match e.kind {
        ms::ErrorKind::InvalidQuery => ErrorObject::new(codes::INVALID_QUERY, e.message),
        ms::ErrorKind::Io => ErrorObject::new(codes::IO_ERROR, e.message),
    }
}

fn query(q: &SearchQuery) -> ms::Query {
    ms::Query {
        pattern: q.pattern.clone(),
        regex: q.regex,
        case_sensitive: q.case_sensitive,
        whole_word: q.whole_word,
        include: q.include.clone(),
        exclude: q.exclude.clone(),
    }
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn search_match(m: ms::Match) -> SearchMatch {
    SearchMatch {
        line: m.line,
        start: m.start,
        end: m.end,
        preview_start: to_u32(m.preview_range.start),
        preview_end: to_u32(m.preview_range.end),
        preview: m.preview,
    }
}

/// Sets the walk's cancel flag when the subscription's stream is dropped.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// What the walk sends the batcher.
enum Found {
    File(ms::FileMatches),
    Done(Result<ms::Summary, SearchError>),
}

impl SearchService {
    /// A service over `workspaces` and `buffers`, resolving `coqo.search@1`
    /// from `mods`.
    pub fn new(workspaces: Arc<WorkspaceService>, buffers: Arc<BufferService>, mods: Mods) -> Arc<Self> {
        Arc::new(Self {
            workspaces,
            buffers,
            mods,
            #[cfg(test)]
            walks: Arc::default(),
        })
    }

    fn compile(&self, q: &SearchQuery) -> Result<Matcher, ErrorObject> {
        let search = self
            .mods
            .services
            .consume::<Search>(&Search::service_ref())
            .map_err(|e| {
                ErrorObject::new(
                    codes::SEARCH_UNAVAILABLE,
                    format!("search is not available: the coqo.search mod is not active ({e})"),
                )
            })?;
        search.compile(&query(q)).map_err(error)
    }

    /// `search/run`: start the search; its events are the feed.
    pub async fn run(self: &Arc<Self>, p: RunParams) -> Result<EventFeed<SearchEvent>, ErrorObject> {
        let started = Instant::now();
        let matcher = self.compile(&p.query)?;
        let mut roots = self.workspaces.roots(p.workspace.clone()).await?;
        if let Some(only) = &p.roots {
            if let Some(missing) = only.iter().find(|r| !roots.iter().any(|(id, _)| id == *r)) {
                return Err(ErrorObject::new(
                    codes::UNKNOWN_ROOT,
                    format!("workspace {} has no root {missing}", p.workspace),
                ));
            }
            roots.retain(|(id, _)| only.contains(id));
        }
        let ids: Vec<RootId> = roots.iter().map(|(id, _)| *id).collect();
        let folders: Vec<Root> = roots.into_iter().map(|(_, path)| Root::new(path)).collect();
        let overlay = self.buffers.open_texts(&p.workspace);
        let max = p
            .max_matches
            .unwrap_or(sm::DEFAULT_MAX_MATCHES)
            .clamp(1, sm::MAX_MATCHES) as usize;

        let cancel = Arc::new(AtomicBool::new(false));
        let (found_tx, mut found_rx) = mpsc::unbounded_channel::<Found>();
        let walk_cancel = Arc::clone(&cancel);
        #[cfg(test)]
        let walks = Arc::clone(&self.walks);
        tokio::task::spawn_blocking(move || {
            let tx = found_tx.clone();
            let summary = matcher.search(&folders, &overlay, max, &walk_cancel, &|file| {
                tx.send(Found::File(file)).is_ok()
            });
            #[cfg(test)]
            if let Ok(s) = &summary {
                walks.lock().unwrap().push(*s);
            }
            let _ = found_tx.send(Found::Done(summary));
        });

        // Batch what the walk finds into events.
        let (event_tx, event_rx) = mpsc::channel::<Result<SearchEvent, ErrorObject>>(16);
        tokio::spawn(async move {
            let to_file = |f: ms::FileMatches| FileMatches {
                root: ids.get(f.root).copied().unwrap_or(RootId(0)),
                path: f.rel,
                matches: f.matches.into_iter().map(search_match).collect(),
            };
            let mut batch = Vec::new();
            let mut deadline: Option<tokio::time::Instant> = None;
            loop {
                let next = match deadline {
                    // The first file goes out at once.
                    None => found_rx.recv().await,
                    Some(at) => tokio::select! {
                        item = found_rx.recv() => item,
                        () = tokio::time::sleep_until(at) => {
                            if !batch.is_empty() {
                                let files = std::mem::take(&mut batch);
                                if event_tx.send(Ok(SearchEvent::Files { files })).await.is_err() {
                                    return;
                                }
                            }
                            deadline = Some(tokio::time::Instant::now() + BATCH_WINDOW);
                            continue;
                        }
                    },
                };
                match next {
                    Some(Found::File(f)) => {
                        batch.push(to_file(f));
                        if deadline.is_none() || batch.len() >= BATCH_FILES {
                            let files = std::mem::take(&mut batch);
                            if event_tx.send(Ok(SearchEvent::Files { files })).await.is_err() {
                                return;
                            }
                            deadline = Some(tokio::time::Instant::now() + BATCH_WINDOW);
                        }
                    }
                    Some(Found::Done(summary)) => {
                        if !batch.is_empty() {
                            let files = std::mem::take(&mut batch);
                            if event_tx.send(Ok(SearchEvent::Files { files })).await.is_err() {
                                return;
                            }
                        }
                        let last = summary.map_err(error).map(|s| {
                            SearchEvent::Done(SearchSummary {
                                files: to_u32(s.files),
                                matches: to_u32(s.matches),
                                searched: to_u32(s.searched),
                                limit_hit: s.limit_hit,
                                elapsed_ms: u64::try_from(started.elapsed().as_millis())
                                    .unwrap_or(u64::MAX),
                            })
                        });
                        let _ = event_tx.send(last).await;
                        return;
                    }
                    None => return,
                }
            }
        });

        let guard = CancelOnDrop(cancel);
        let mut seq = 0;
        let events = tokio_stream_from(event_rx)
            .map(move |item| {
                let _keep = &guard;
                seq += 1;
                item.map(|event| Sequenced { seq, event })
            })
            .boxed();
        Ok(EventFeed { head: 0, events })
    }

    /// `search/replace`.
    pub async fn replace(&self, p: ReplaceParams) -> Result<ReplaceResult, ErrorObject> {
        let matcher = Arc::new(self.compile(&p.query)?);
        let replacement: Arc<str> = p.replacement.into();
        let mut result = ReplaceResult::default();
        for file in p.files {
            let mut out = FileReplaced {
                root: file.root,
                path: file.path.clone(),
                outcome: ReplaceOutcome::Unchanged,
                replaced: 0,
                buffer: None,
                rev: None,
                edits: Vec::new(),
                error: None,
            };
            let only: Option<Vec<MatchAt>> = file.matches.map(|m| {
                m.into_iter()
                    .map(|a| MatchAt {
                        line: a.line,
                        start: a.start,
                        end: a.end,
                    })
                    .collect()
            });
            let compute = {
                let (matcher, replacement) = (Arc::clone(&matcher), Arc::clone(&replacement));
                move |text: &str| -> Vec<TextEdit> {
                    matcher
                        .replace(text, &replacement, only.as_deref())
                        .into_iter()
                        .map(|r| TextEdit::new(r.bytes, r.text))
                        .collect()
                }
            };
            let done = async {
                let resolved = self
                    .workspaces
                    .resolve_file(p.workspace.clone(), file.root, file.path.clone())
                    .await?;
                let held = self
                    .buffers
                    .buffer_at(&p.workspace, &resolved.real)
                    .filter(|id| p.hold.contains(id));
                if let Some(id) = held {
                    let (rev, text) = self.buffers.head(id)?;
                    let edits = compute(&text);
                    out.buffer = Some(id);
                    out.rev = Some(rev);
                    out.replaced = to_u32(edits.len());
                    if !edits.is_empty() {
                        out.outcome = ReplaceOutcome::Held;
                        out.edits = edits;
                    }
                    return Ok(());
                }
                match self
                    .buffers
                    .replace_in_file(&p.workspace, resolved.real, compute)
                    .await?
                {
                    FileReplace::Buffer {
                        buffer,
                        rev,
                        replaced,
                    } => {
                        out.outcome = ReplaceOutcome::Buffer;
                        out.buffer = Some(buffer);
                        out.rev = Some(rev);
                        out.replaced = to_u32(replaced);
                    }
                    FileReplace::Written { replaced } => {
                        out.outcome = ReplaceOutcome::Written;
                        out.replaced = to_u32(replaced);
                    }
                    FileReplace::Unchanged => {}
                }
                Ok::<_, ErrorObject>(())
            };
            if let Err(e) = done.await {
                out.outcome = ReplaceOutcome::Failed;
                out.error = Some(e.message);
            }
            result.replaced = result.replaced.saturating_add(out.replaced);
            result.files.push(out);
        }
        Ok(result)
    }
}

/// `rx` as a stream.
fn tokio_stream_from<T: Send + 'static>(
    rx: mpsc::Receiver<T>,
) -> futures_util::stream::BoxStream<'static, T> {
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|t| (t, rx)) })
        .boxed()
}

/// Register `search/run` and `search/replace` on `router`: the daemon token
/// only.
pub fn register(router: Router, svc: Arc<SearchService>) -> Router {
    let run = Arc::clone(&svc);
    router
        .topic_with::<sm::Run, _, _>(Access::daemon(), move |_ctx, p, _since| {
            let s = Arc::clone(&run);
            async move { s.run(p).await }
        })
        .method_with::<sm::Replace, _, _>(Access::daemon(), move |_ctx, p| {
            let s = Arc::clone(&svc);
            async move { s.replace(p).await }
        })
}

#[cfg(test)]
mod tests;
