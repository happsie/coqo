// SPDX-License-Identifier: Apache-2.0

//! A compiled [`Query`]: matches in one line, in one text, and their
//! replacements.

use std::collections::HashSet;
use std::io;
use std::ops::Range;
use std::path::Path;

use grep_matcher::{Captures as _, LineTerminator, Matcher as _};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};

use crate::{MAX_PREVIEW, Query, Result, SearchError};

/// One match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// 0-based line.
    pub line: u32,
    /// 0-based character where it starts on the line.
    pub start: u32,
    /// 0-based character where it ends (exclusive), on the same line.
    pub end: u32,
    /// Its bytes in the text searched.
    pub bytes: Range<usize>,
    /// The line, without its line ending and leading whitespace; a window
    /// around the match (with `…` where cut) if the line is long.
    pub preview: String,
    /// The match's bytes in [`preview`](Self::preview).
    pub preview_range: Range<usize>,
}

impl Match {
    /// Where it is, as [`Matcher::replace`] takes it.
    pub fn at(&self) -> MatchAt {
        MatchAt {
            line: self.line,
            start: self.start,
            end: self.end,
        }
    }
}

/// Where a match is: its line and character columns. A client names the
/// matches to replace this way, so a text that changed since the search
/// (an edit above, say) replaces only matches still at those places.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MatchAt {
    /// 0-based line.
    pub line: u32,
    /// 0-based start character on the line.
    pub start: u32,
    /// 0-based end character (exclusive).
    pub end: u32,
}

/// One replacement: `bytes` of the text become `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    /// The match it replaces.
    pub at: MatchAt,
    /// Its bytes in the text.
    pub bytes: Range<usize>,
    /// The new text (capture groups expanded, for a regular expression).
    pub text: String,
}

/// A compiled [`Query`]. Cheap to share between threads.
#[derive(Clone, Debug)]
pub struct Matcher {
    query: Query,
    regex: RegexMatcher,
}

impl Matcher {
    pub(crate) fn new(query: Query) -> Result<Self> {
        if query.pattern.is_empty() {
            return Err(SearchError::invalid("the search is empty"));
        }
        let mut b = RegexMatcherBuilder::new();
        b.case_insensitive(!query.case_sensitive)
            .word(query.whole_word)
            .fixed_strings(!query.regex)
            .multi_line(true)
            .line_terminator(Some(b'\n'))
            .crlf(true);
        let regex = b
            .build(&query.pattern)
            .map_err(|e| SearchError::invalid(e.to_string()))?;
        // Check the globs once, here, rather than per folder searched.
        crate::walk::overrides(Path::new("/"), &query)?;
        Ok(Self { query, regex })
    }

    /// The query it was built from.
    pub fn query(&self) -> &Query {
        &self.query
    }

    pub(crate) fn regex(&self) -> &RegexMatcher {
        &self.regex
    }

    // Both searchers end lines at `\n` and strip a `\r` before it, as the
    // matcher (`crlf`) expects: a mismatch would fail every search.

    /// A searcher for text in memory (an open buffer): never binary.
    pub(crate) fn text_searcher() -> Searcher {
        SearcherBuilder::new()
            .line_number(true)
            .line_terminator(LineTerminator::crlf())
            .binary_detection(BinaryDetection::none())
            .build()
    }

    /// A searcher for files: a NUL byte makes the file binary, and it is
    /// skipped.
    pub(crate) fn file_searcher() -> Searcher {
        SearcherBuilder::new()
            .line_number(true)
            .line_terminator(LineTerminator::crlf())
            .binary_detection(BinaryDetection::quit(0))
            .build()
    }

    /// Every match in `text`.
    pub fn find(&self, text: &str) -> Vec<Match> {
        let mut out = Vec::new();
        let mut sink = LineSink::new(self, None, |m, _| {
            out.push(m);
            true
        });
        // A sink that never fails, on a slice: the search cannot fail.
        let _ = Self::text_searcher().search_slice(&self.regex, text.as_bytes(), &mut sink);
        out
    }

    /// The replacements of the matches in `text` by `replacement`: every
    /// match, or only those at `only`. For a regular expression,
    /// `replacement` may name capture groups (`$1`, `${name}`; `$$` is a
    /// `$`); otherwise it is literal. In order, not overlapping.
    pub fn replace(
        &self,
        text: &str,
        replacement: &str,
        only: Option<&[MatchAt]>,
    ) -> Vec<Replacement> {
        let only: Option<HashSet<MatchAt>> = only.map(|o| o.iter().copied().collect());
        let mut out = Vec::new();
        let mut sink = LineSink::new(self, Some(replacement), |m, text| {
            if only.as_ref().is_none_or(|o| o.contains(&m.at())) {
                out.push(Replacement {
                    at: m.at(),
                    bytes: m.bytes.clone(),
                    text: text.unwrap_or_default(),
                });
            }
            true
        });
        let _ = Self::text_searcher().search_slice(&self.regex, text.as_bytes(), &mut sink);
        out
    }

    /// Call `f` for each match in `line` (one line with its terminator, if
    /// any), number `line_no` (0-based), starting at byte `offset` of the
    /// text, with its replacement if `replacement` is given. Stops and
    /// returns `false` when `f` does.
    fn each_in_line(
        &self,
        line: &[u8],
        line_no: u64,
        offset: usize,
        replacement: Option<&str>,
        f: &mut impl FnMut(Match, Option<String>) -> bool,
    ) -> bool {
        let content = content_len(line);
        let Ok(mut caps) = self.regex.new_captures() else {
            return true;
        };
        let mut go = true;
        let _ = self.regex.captures_iter(line, &mut caps, |caps| {
            let Some(m) = caps.get(0) else {
                return true;
            };
            let range = m.start()..m.end().min(content);
            if range.is_empty() {
                return true;
            }
            let found = build_match(line, content, range, line_no, offset);
            let text = replacement.map(|rep| {
                if self.query.regex {
                    let mut dst = Vec::new();
                    caps.interpolate(
                        |name| self.regex.capture_index(name),
                        line,
                        rep.as_bytes(),
                        &mut dst,
                    );
                    String::from_utf8_lossy(&dst).into_owned()
                } else {
                    rep.to_owned()
                }
            });
            go = f(found, text);
            go
        });
        go
    }
}

/// The length of `line` without its line ending.
fn content_len(line: &[u8]) -> usize {
    let mut n = line.len();
    if n > 0 && line[n - 1] == b'\n' {
        n -= 1;
        if n > 0 && line[n - 1] == b'\r' {
            n -= 1;
        }
    }
    n
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The largest char boundary of `s` at or below `at`.
fn floor_boundary(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest char boundary of `s` at or above `at`.
fn ceil_boundary(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// How many bytes of context a long line keeps before its match.
const PREVIEW_BEFORE: usize = 48;

fn build_match(
    line: &[u8],
    content: usize,
    range: Range<usize>,
    line_no: u64,
    offset: usize,
) -> Match {
    let before = lossy(&line[..range.start]);
    let hit = lossy(&line[range.clone()]);
    let after = lossy(&line[range.end..content]);
    let start = before.chars().count();
    let end = start + hit.chars().count();

    let mut before = before.trim_start();
    let mut after = after.as_str();
    let mut hit = hit.as_str();
    let (mut cut_front, mut cut_back) = (false, false);
    if before.len() + hit.len() + after.len() > MAX_PREVIEW {
        if before.len() > PREVIEW_BEFORE {
            let at = ceil_boundary(before, before.len() - PREVIEW_BEFORE);
            before = &before[at..];
            cut_front = true;
        }
        if before.len() + hit.len() > MAX_PREVIEW {
            hit = &hit[..floor_boundary(hit, MAX_PREVIEW.saturating_sub(before.len()))];
            after = "";
            cut_back = true;
        } else {
            let room = MAX_PREVIEW - before.len() - hit.len();
            if after.len() > room {
                after = &after[..floor_boundary(after, room)];
                cut_back = true;
            }
        }
    }
    let mut preview = String::with_capacity(before.len() + hit.len() + after.len() + 6);
    if cut_front {
        preview.push('…');
    }
    preview.push_str(before);
    let hit_start = preview.len();
    preview.push_str(hit);
    let hit_end = preview.len();
    preview.push_str(after);
    if cut_back {
        preview.push('…');
    }
    Match {
        line: u32::try_from(line_no).unwrap_or(u32::MAX),
        start: u32::try_from(start).unwrap_or(u32::MAX),
        end: u32::try_from(end).unwrap_or(u32::MAX),
        bytes: offset + range.start..offset + range.end,
        preview,
        preview_range: hit_start..hit_end,
    }
}

/// Hands each match of the lines a searcher finds to `f`.
pub(crate) struct LineSink<'a, F> {
    matcher: &'a Matcher,
    replacement: Option<&'a str>,
    f: F,
    /// The searcher found a NUL byte (a binary file) and stopped.
    pub binary: bool,
    /// `f` asked to stop.
    pub stopped: bool,
}

impl<'a, F: FnMut(Match, Option<String>) -> bool> LineSink<'a, F> {
    pub(crate) fn new(matcher: &'a Matcher, replacement: Option<&'a str>, f: F) -> Self {
        Self {
            matcher,
            replacement,
            f,
            binary: false,
            stopped: false,
        }
    }
}

impl<F: FnMut(Match, Option<String>) -> bool> Sink for LineSink<'_, F> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let mut line_no = mat.line_number().unwrap_or(1).saturating_sub(1);
        let mut offset = usize::try_from(mat.absolute_byte_offset()).unwrap_or(usize::MAX);
        for line in mat.lines() {
            if !self
                .matcher
                .each_in_line(line, line_no, offset, self.replacement, &mut self.f)
            {
                self.stopped = true;
                return Ok(false);
            }
            line_no += 1;
            offset = offset.saturating_add(line.len());
        }
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}
