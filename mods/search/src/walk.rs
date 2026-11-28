// SPDX-License-Identifier: Apache-2.0

//! Searching folders: a parallel, ignore-aware walk, each file searched on
//! disk or in the text the caller has for it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ignore::overrides::{Override, OverrideBuilder};
use ignore::{WalkBuilder, WalkState};

use crate::matcher::{LineSink, Matcher};
use crate::{MAX_FILE_BYTES, Match, Query, Result, SearchError};

/// Texts to search instead of the files on disk, by the file's canonical
/// path: open buffers, unsaved edits included.
pub type Overlay = HashMap<PathBuf, String>;

/// A folder to search: canonical, so its files' paths match the
/// [`Overlay`]'s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// The folder.
    pub path: PathBuf,
}

impl Root {
    /// `path`, which should be canonical.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

/// The matches of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatches {
    /// Index of its folder in the roots searched.
    pub root: usize,
    /// The file.
    pub path: PathBuf,
    /// Its path relative to the folder, `/`-separated.
    pub rel: String,
    /// In order.
    pub matches: Vec<Match>,
}

/// How a search went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Files with matches.
    pub files: usize,
    /// Matches reported.
    pub matches: usize,
    /// Files searched.
    pub searched: usize,
    /// It stopped at the match limit: there are more.
    pub limit_hit: bool,
    /// It was cancelled, or the sink asked it to stop.
    pub cancelled: bool,
}

/// The include and exclude globs of `query`, for folder `root`. An include
/// glob without wildcards (`src`) also covers what is under it (`src/**`).
pub(crate) fn overrides(root: &Path, query: &Query) -> Result<Override> {
    let mut b = OverrideBuilder::new(root);
    let bad = |g: &str, e: ignore::Error| SearchError::invalid(format!("glob {g:?}: {e}"));
    for g in query.include.iter().map(|g| g.trim()).filter(|g| !g.is_empty()) {
        b.add(g).map_err(|e| bad(g, e))?;
        if !g.contains(['*', '?', '[', '{']) {
            let under = format!("{}/**", g.trim_end_matches('/'));
            b.add(&under).map_err(|e| bad(g, e))?;
        }
    }
    for g in query.exclude.iter().map(|g| g.trim()).filter(|g| !g.is_empty()) {
        let g = g.trim_start_matches('!');
        b.add(&format!("!{g}")).map_err(|e| bad(g, e))?;
    }
    b.build().map_err(|e| SearchError::invalid(e.to_string()))
}

/// `path` relative to `root`, `/`-separated.
fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

impl Matcher {
    /// Search the files under `roots` in parallel and hand each file with
    /// matches to `sink` as soon as it is searched (so the order is not
    /// fixed). Files in `overlay` are searched in its text instead of on
    /// disk. Stops after `max_matches` matches ([`Summary::limit_hit`]),
    /// when `cancel` is set, or when `sink` returns `false`.
    ///
    /// Respects `.gitignore`, `.ignore`, `.git/info/exclude` and the user's
    /// global excludes, in and out of a git repository; skips `.git`,
    /// symlinks, binary files and files over [`MAX_FILE_BYTES`]. Hidden
    /// files are searched. Unreadable files are skipped.
    pub fn search(
        &self,
        roots: &[Root],
        overlay: &Overlay,
        max_matches: usize,
        cancel: &AtomicBool,
        sink: &(dyn Fn(FileMatches) -> bool + Sync),
    ) -> Result<Summary> {
        let found = AtomicUsize::new(0);
        let files = AtomicUsize::new(0);
        let searched = AtomicUsize::new(0);
        let limit_hit = AtomicBool::new(false);
        let stopped = AtomicBool::new(false);
        for (ix, root) in roots.iter().enumerate() {
            if stopped.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
                break;
            }
            if !root.path.is_dir() {
                return Err(SearchError::new(
                    crate::ErrorKind::Io,
                    format!("{} is not a folder", root.path.display()),
                ));
            }
            let overrides = overrides(&root.path, self.query())?;
            let walker = WalkBuilder::new(&root.path)
                .hidden(false)
                .parents(true)
                .ignore(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .require_git(false)
                .follow_links(false)
                .max_filesize(Some(MAX_FILE_BYTES))
                .overrides(overrides)
                .filter_entry(|e| e.file_name() != ".git")
                .build_parallel();
            walker.run(|| {
                let mut on_disk = Matcher::file_searcher();
                let mut in_memory = Matcher::text_searcher();
                let (found, files, searched) = (&found, &files, &searched);
                let (limit_hit, stopped) = (&limit_hit, &stopped);
                Box::new(move |entry| {
                    if cancel.load(Ordering::Acquire) || stopped.load(Ordering::Acquire) {
                        return WalkState::Quit;
                    }
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let path = entry.path();
                    searched.fetch_add(1, Ordering::Relaxed);
                    let mut matches = Vec::new();
                    let mut sink_lines = LineSink::new(self, None, |m, _| {
                        matches.push(m);
                        true
                    });
                    let read = match overlay.get(path) {
                        Some(text) => in_memory.search_slice(
                            self.regex(),
                            text.as_bytes(),
                            &mut sink_lines,
                        ),
                        None => on_disk.search_path(self.regex(), path, &mut sink_lines),
                    };
                    let binary = sink_lines.binary;
                    if read.is_err() || binary || matches.is_empty() {
                        return WalkState::Continue;
                    }
                    let before = found.fetch_add(matches.len(), Ordering::AcqRel);
                    if before >= max_matches {
                        limit_hit.store(true, Ordering::Release);
                        stopped.store(true, Ordering::Release);
                        return WalkState::Quit;
                    }
                    if before + matches.len() > max_matches {
                        matches.truncate(max_matches - before);
                        limit_hit.store(true, Ordering::Release);
                        stopped.store(true, Ordering::Release);
                    }
                    files.fetch_add(1, Ordering::Relaxed);
                    let file = FileMatches {
                        root: ix,
                        path: path.to_path_buf(),
                        rel: relative(&root.path, path),
                        matches,
                    };
                    if !sink(file) {
                        stopped.store(true, Ordering::Release);
                    }
                    if stopped.load(Ordering::Acquire) {
                        WalkState::Quit
                    } else {
                        WalkState::Continue
                    }
                })
            });
        }
        let limit_hit = limit_hit.load(Ordering::Acquire);
        Ok(Summary {
            files: files.load(Ordering::Acquire),
            matches: found.load(Ordering::Acquire).min(max_matches),
            searched: searched.load(Ordering::Acquire),
            limit_hit,
            cancelled: cancel.load(Ordering::Acquire)
                || (stopped.load(Ordering::Acquire) && !limit_hit),
        })
    }
}
