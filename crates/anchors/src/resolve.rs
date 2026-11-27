// SPDX-License-Identifier: Apache-2.0

//! Resolving an anchor against a snapshot: exact mapping through the
//! buffer's changes when they are known, else relocation by fingerprint.
//!
//! **With history** (the changes from the anchor's revision to the
//! snapshot's), the range is mapped exactly ([`map_range`]):
//! - untouched: the same text, [`Exact`](Status::Exact) or
//!   [`Moved`](Status::Moved);
//! - edited in place: [`Fuzzy`](Status::Fuzzy) with the similarity of the
//!   old and new text, or [`Orphaned`](Status::Orphaned) below
//!   [`MIN_CONFIDENCE`];
//! - deleted: only an exact copy of its text elsewhere (cut and paste) is
//!   accepted, else orphaned;
//! - swallowed by one large replacement across its edges (a whole-file
//!   rewrite): relocated as if there were no history.
//!
//! **Without history** (file changed on disk, daemon restarted, log
//! trimmed), the anchor is relocated in tiers, best candidate wins:
//! 1. **Exact text**: occurrences of its lines, then of its text, scored by
//!    their context lines and enclosing symbol.
//! 2. **Whitespace-insensitive**: the text with all whitespace removed
//!    (reindented or reflowed code).
//! 3. **Fuzzy**: windows of lines found by voting (lines of the anchor, its
//!    context and its enclosing symbol by name), aligned line by line and
//!    scored by text similarity, context and symbol identity.
//!
//! A candidate below [`MIN_CONFIDENCE`], or one tied with a different
//! candidate, leaves the anchor orphaned: a wrong match is worse than none.
//! An anchor whose context lines now sit next to each other was deleted in
//! place, so only exact copies of it are looked for.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use coqo_syntax::{Symbol, TextEdit};

use crate::map::map_range;
use crate::model::{Anchor, CONTEXT_LINES, Resolution, Span, Status, SymbolPath};
use crate::snapshot::{Snapshot, line_hash};
use crate::text::{diff, map_through_diff, non_ws, normalize, ratio, ratio_bound};

/// Matches less sure than this are not used: the anchor is orphaned.
pub const MIN_CONFIDENCE: f32 = 0.6;

/// An exact-text match at least this sure is [`Moved`](Status::Moved) (or
/// [`Exact`](Status::Exact)) rather than [`Fuzzy`](Status::Fuzzy).
const SURE: f32 = 0.85;

/// Most candidates scored per tier.
const MAX_CANDIDATES: usize = 256;

/// Most fuzzy seeds tried.
const MAX_SEEDS: usize = 4;

/// The largest line-alignment table (anchor lines × window lines).
const MAX_ALIGN: usize = 250_000;

impl Snapshot<'_> {
    /// Find `anchor` in this snapshot.
    ///
    /// `history` is every change from the anchor's revision to the
    /// snapshot's, oldest first, each a list of edits in the coordinates
    /// before it (an empty list when nothing changed). Pass `None` when they
    /// are not known: the anchor is then relocated by its fingerprint.
    pub fn resolve(&self, anchor: &Anchor, history: Option<&[Vec<TextEdit>]>) -> Resolution {
        let status_range = match history {
            Some(changes) => self.through_history(anchor, changes),
            None => self.relocate(anchor, Mode::Full),
        };
        match status_range {
            Some((status, range)) => Resolution {
                status,
                range: Some(Span::of(self, range)),
                rev: self.rev(),
            },
            None => Resolution {
                status: Status::Orphaned,
                range: None,
                rev: self.rev(),
            },
        }
    }

    /// [`resolve`](Self::resolve) several anchors that share one history
    /// (for example, every anchor of a buffer last resolved at the same
    /// revision). The snapshot's indexes are built once.
    pub fn resolve_all(
        &self,
        anchors: &[Anchor],
        history: Option<&[Vec<TextEdit>]>,
    ) -> Vec<Resolution> {
        anchors.iter().map(|a| self.resolve(a, history)).collect()
    }

    fn through_history(
        &self,
        anchor: &Anchor,
        changes: &[Vec<TextEdit>],
    ) -> Option<(Status, Range<usize>)> {
        let m = map_range(anchor.byte_range(), changes);
        let text = self.text();
        let Some(now) = text.get(m.range.clone()) else {
            // The changes do not fit this text.
            return self.relocate(anchor, Mode::Full);
        };
        if m.swallowed {
            return self.relocate(anchor, Mode::Full);
        }
        if m.collapsed {
            return self.relocate(anchor, Mode::ExactOnly);
        }
        if !m.touched && now != anchor.text {
            // The changes are not this text's: the history is wrong.
            return self.relocate(anchor, Mode::Full);
        }
        if now == anchor.text && (!m.touched || !now.is_empty()) {
            // Untouched, or edited back to what it was.
            let status = if m.range == anchor.byte_range() {
                Status::Exact
            } else {
                Status::Moved
            };
            return Some((status, m.range));
        }
        let confidence = if anchor.text.is_empty() {
            // A spot: how much its line still looks the same.
            let line = self.line_text(self.line_of(m.range.start));
            edited_confidence(&anchor.block(), line)
        } else {
            edited_confidence(&anchor.text, now)
        };
        (confidence >= MIN_CONFIDENCE).then_some((Status::Fuzzy { confidence }, m.range))
    }

    /// Relocate by fingerprint.
    fn relocate(&self, anchor: &Anchor, mode: Mode) -> Option<(Status, Range<usize>)> {
        let here = anchor.byte_range();
        if mode == Mode::Full
            && self.text().get(here.clone()) == Some(anchor.text.as_str())
            && self.keeps_words(anchor, &here)
            && self.evidence(anchor, &here) >= 0.75
        {
            // Same text, same surroundings, same place.
            return Some((Status::Exact, here));
        }
        let mut found = Vec::new();
        self.exact_candidates(anchor, &mut found);
        let sure = found.iter().any(|c| c.confidence >= SURE);
        if mode == Mode::Full && !sure {
            self.squashed_candidates(anchor, &mut found);
            if !self.deleted_in_place(anchor) {
                self.fuzzy_candidates(anchor, &mut found);
            }
        }
        let best = pick(found, anchor.range.start.offset)?;
        let text = self.text();
        let exact_text = text.get(best.range.clone()) == Some(anchor.text.as_str());
        if mode == Mode::ExactOnly && !(exact_text && best.confidence >= SURE) {
            return None;
        }
        if exact_text && best.confidence >= SURE {
            let status = if best.range == anchor.byte_range() {
                Status::Exact
            } else {
                Status::Moved
            };
            return Some((status, best.range));
        }
        if best.confidence >= MIN_CONFIDENCE {
            let confidence = best.confidence.min(0.99);
            return Some((Status::Fuzzy { confidence }, best.range));
        }
        None
    }

    // ---- tier 1: exact text -------------------------------------------------

    fn exact_candidates(&self, anchor: &Anchor, out: &mut Vec<Candidate>) {
        let text = self.text();
        let block = anchor.block();
        let origin = anchor.range.start.offset;
        let mut block_hits = Vec::new();
        if non_ws(&block) > 0 {
            for (pos, _) in text.match_indices(block.as_str()) {
                let end = pos + block.len();
                let at_line_start = pos == 0 || text.as_bytes()[pos - 1] == b'\n';
                let at_line_end = block.ends_with('\n')
                    || end == text.len()
                    || matches!(text.as_bytes()[end], b'\n' | b'\r');
                if at_line_start && at_line_end {
                    let start = pos + anchor.prefix.len();
                    block_hits.push(start..start + anchor.text.len());
                }
            }
        }
        let mut hits = block_hits.clone();
        let partial = !anchor.prefix.is_empty() || !anchor.suffix.is_empty();
        if partial && non_ws(&anchor.text) > 0 {
            for (pos, _) in text.match_indices(anchor.text.as_str()) {
                let range = pos..pos + anchor.text.len();
                if self.keeps_words(anchor, &range) {
                    hits.push(range);
                }
            }
        }
        hits.sort_by_key(|r| (r.start, r.end));
        hits.dedup();
        nearest(&mut hits, origin);

        let unique = block_hits.len() == 1;
        let specific = non_ws(&block) >= 24;
        for range in hits {
            let evidence = self.evidence(anchor, &range);
            let mut confidence = 0.5 + 0.5 * evidence;
            if evidence >= 0.75 {
                confidence = 1.0;
            } else if unique && specific && block_hits[0] == range {
                confidence = confidence.max(0.9);
            }
            out.push(Candidate {
                range,
                confidence,
                evidence,
            });
        }
    }

    // ---- tier 2: whitespace-insensitive -------------------------------------

    fn squashed_candidates(&self, anchor: &Anchor, out: &mut Vec<Candidate>) {
        let needle: String = anchor.text.chars().filter(|c| !c.is_whitespace()).collect();
        if needle.chars().count() < 8 {
            return;
        }
        let q = self.squashed();
        let text = self.text();
        let mut hits: Vec<Range<usize>> = q
            .text
            .match_indices(needle.as_str())
            .take(4 * MAX_CANDIDATES)
            .map(|(pos, _)| {
                let mut start = q.origin[pos];
                let mut end = q.origin[pos + needle.len() - 1] + 1;
                // Whole-line anchors keep their indentation and line break.
                if anchor.prefix.is_empty() {
                    let line = self.line_range(self.line_of(start));
                    if text[line.start..start].trim().is_empty() {
                        start = line.start;
                    }
                }
                if anchor.suffix.is_empty() && anchor.text.ends_with('\n') {
                    let line = self.line_range(self.line_of(end.saturating_sub(1)));
                    if text[end.min(line.end)..line.end].trim().is_empty() {
                        end = self.line_start(self.line_of(line.start) + 1);
                    }
                }
                start..end
            })
            .collect();
        nearest(&mut hits, anchor.range.start.offset);
        for range in hits {
            let evidence = self.evidence(anchor, &range);
            out.push(Candidate {
                confidence: 0.95 * (0.75 + 0.25 * evidence),
                range,
                evidence,
            });
        }
    }

    // ---- tier 3: fuzzy ------------------------------------------------------

    fn fuzzy_candidates(&self, anchor: &Anchor, out: &mut Vec<Candidate>) {
        let block = anchor.block();
        let block_lines = split_lines(&block);
        let k = block_lines.len();
        let n = self.line_count();
        if k == 0 || n == 0 {
            return;
        }
        let ends_with_break = block.ends_with('\n');
        let anchor_hashes: Vec<u64> = block_lines.iter().map(|l| line_hash(l)).collect();
        let lines = self.line_hashes();
        // Lines that look like the anchor's context, not like the anchor.
        let anchor_set: HashSet<u64> = anchor_hashes.iter().copied().collect();
        let context_set: HashSet<u64> = anchor
            .context_before
            .iter()
            .chain(&anchor.context_after)
            .map(|l| line_hash(l))
            .filter(|h| *h != 0 && !anchor_set.contains(h))
            .collect();

        // Vote for where the anchor's first line is now.
        let mut votes: HashMap<isize, f32> = HashMap::new();
        let mut vote = |hash: u64, at: isize, weight: f32| {
            if hash == 0 {
                return;
            }
            if let Some(found) = lines.by_hash.get(&hash)
                && found.len() <= 64
            {
                let w = weight / found.len() as f32;
                for &l in found {
                    *votes.entry(l as isize - at).or_default() += w;
                }
            }
        };
        for (j, &h) in anchor_hashes.iter().enumerate() {
            vote(h, j as isize, 1.0);
        }
        let before = anchor.context_before.len() as isize;
        for (i, l) in anchor.context_before.iter().enumerate() {
            vote(line_hash(l), i as isize - before, 0.5);
        }
        for (i, l) in anchor.context_after.iter().enumerate() {
            vote(line_hash(l), (k + i) as isize, 0.5);
        }
        if let Some(sym) = &anchor.symbol {
            for s in self.symbols() {
                if s.kind.as_str() == sym.kind && s.name == sym.name() {
                    let same_container = s.container[..] == sym.path[..sym.path.len() - 1];
                    let at = (s.start_line + sym.line_offset) as isize;
                    *votes.entry(at).or_default() += if same_container { 2.0 } else { 1.0 };
                }
            }
        }
        let original = anchor.range.start.line as isize;
        *votes.entry(original).or_default() += 0.3;
        let mut seeds: Vec<(isize, f32)> = votes.into_iter().collect();
        seeds.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then((a.0 - original).abs().cmp(&(b.0 - original).abs()))
                .then(a.0.cmp(&b.0))
        });
        seeds.truncate(MAX_SEEDS);

        let norm_block = normalize(&block);
        let specificity = (non_ws(&block) as f32 / 40.0).min(1.0);
        let floor = MIN_CONFIDENCE + 0.3 * (1.0 - specificity);
        let is_context = |l: usize| context_set.contains(&lines.hashes[l]);
        let text = self.text();
        for (seed, _) in seeds {
            let seed = seed.clamp(0, n as isize - 1) as usize;
            let slack = 2 + k / 4;
            let lo = seed.saturating_sub(slack);
            let hi = (seed + k + slack).min(n);
            let pairs = if k * (hi - lo) <= MAX_ALIGN {
                align(&anchor_hashes, &lines.hashes[lo..hi])
            } else {
                Vec::new()
            };
            let (first, last) = match (pairs.first(), pairs.last()) {
                (Some(&(j1, l1)), Some(&(j2, l2))) => {
                    let (l1, l2) = (lo + l1, lo + l2);
                    // Unmatched anchor lines map one to one, but never onto
                    // a context line.
                    let mut first = l1;
                    for _ in 0..j1 {
                        if first == 0 || is_context(first - 1) {
                            break;
                        }
                        first -= 1;
                    }
                    let mut last = l2;
                    for _ in 0..(k - 1 - j2) {
                        if last + 1 >= n || is_context(last + 1) {
                            break;
                        }
                        last += 1;
                    }
                    (first, last)
                }
                _ => (seed, (seed + k - 1).min(n - 1)),
            };
            let (mut first, mut last) = (first, last);
            let window_lines = last - first + 1;
            let foreign = (first..=last).filter(|&l| is_context(l)).count();
            if 2 * foreign >= window_lines.max(1) && foreign > 0 {
                continue;
            }
            let bytes = |first: usize, last: usize| {
                let end = if ends_with_break {
                    self.line_start(last + 1)
                } else {
                    self.line_range(last).end
                };
                self.line_start(first)..end
            };
            let similarity_of = |first: usize, last: usize| {
                let norm_window = normalize(&text[bytes(first, last)]);
                if ratio_bound(norm_block.len(), norm_window.len()) < floor / 2.0 {
                    0.0
                } else {
                    ratio(&norm_block, &norm_window)
                }
            };
            let mut similarity = similarity_of(first, last);
            // Grow the window while it gets more like the anchor (a line
            // split into several), then to close brackets it opened.
            let most = k + 3;
            for _ in 0..most {
                if last + 1 >= n || is_context(last + 1) {
                    break;
                }
                let s = similarity_of(first, last + 1);
                if s <= similarity {
                    break;
                }
                (similarity, last) = (s, last + 1);
            }
            for _ in 0..most {
                if first == 0 || is_context(first - 1) {
                    break;
                }
                let s = similarity_of(first - 1, last);
                if s <= similarity {
                    break;
                }
                (similarity, first) = (s, first - 1);
            }
            let want = brackets(&block);
            for _ in 0..most {
                if brackets(&text[bytes(first, last)]) <= want
                    || last + 1 >= n
                    || brackets(self.line_text(last + 1)) >= 0
                {
                    break;
                }
                last += 1;
                similarity = similarity_of(first, last);
            }
            if similarity < floor {
                continue;
            }
            let Range { start, end } = bytes(first, last);
            let window = &text[start..end];
            let range = if anchor.prefix.is_empty() && anchor.suffix.is_empty() {
                start..end
            } else {
                let ops = diff(block.as_bytes(), window.as_bytes());
                let s = map_through_diff(&ops, anchor.prefix.len(), false).min(window.len());
                let e = map_through_diff(&ops, anchor.prefix.len() + anchor.text.len(), true)
                    .clamp(s, window.len());
                let s = floor_char(window, s);
                let e = ceil_char(window, e);
                if !anchor.text.is_empty() && s >= e {
                    continue;
                }
                start + s..start + e
            };
            let evidence = self.evidence(anchor, &range);
            out.push(Candidate {
                confidence: similarity * (0.75 + 0.25 * evidence),
                range,
                evidence,
            });
        }
    }

    /// Whether the anchor's context lines before and after now follow each
    /// other: the anchored lines were deleted there.
    fn deleted_in_place(&self, anchor: &Anchor) -> bool {
        let (before, after) = (&anchor.context_before, &anchor.context_after);
        if before.is_empty() || after.is_empty() {
            return false;
        }
        let want: Vec<u64> = before.iter().chain(after).map(|l| line_hash(l)).collect();
        let have = &self.line_hashes().hashes;
        if want.len() > have.len() {
            return false;
        }
        have.windows(want.len()).any(|w| w == want.as_slice())
    }

    // ---- scoring ------------------------------------------------------------

    /// Whether `range` starts and ends at word edges where the anchor did:
    /// `total` is not inside `subtotal`.
    fn keeps_words(&self, anchor: &Anchor, range: &Range<usize>) -> bool {
        let text = self.text();
        let edge =
            |before: &str, after: &str| word_edge(before.chars().next_back(), after.chars().next());
        let (start, end) = (range.start, range.end);
        (!edge(&anchor.prefix, &anchor.text) || edge(&text[..start], &text[start..]))
            && (!edge(&anchor.text, &anchor.suffix) || edge(&text[..end], &text[end..]))
    }

    /// How well the surroundings of `range` match the anchor's: the rest of
    /// its lines, the context lines and the enclosing symbol, in `[0, 1]`.
    fn evidence(&self, anchor: &Anchor, range: &Range<usize>) -> f32 {
        let text = self.text();
        let (first, last) = self.lines_of(range);
        let mut sum = 0.0;
        let mut weight = 0.0;
        if !anchor.prefix.is_empty() || !anchor.suffix.is_empty() {
            let first_line = self.line_range(first);
            let last_line = self.line_range(last);
            let prefix = &text[first_line.start.min(range.start)..range.start];
            let suffix = if range.end <= last_line.end {
                &text[range.end..last_line.end]
            } else {
                ""
            };
            let want = format!(
                "{}\n{}",
                normalize(&anchor.prefix),
                normalize(&anchor.suffix)
            );
            let have = format!("{}\n{}", normalize(prefix), normalize(suffix));
            sum += ratio(&want, &have);
            weight += 1.0;
        }
        let before: Vec<&str> = (first.saturating_sub(CONTEXT_LINES)..first)
            .map(|l| self.line_text(l))
            .collect();
        let after: Vec<&str> = (last + 1..(last + 1 + CONTEXT_LINES).min(self.line_count()))
            .map(|l| self.line_text(l))
            .collect();
        sum += context_similarity(&anchor.context_before, &before);
        sum += context_similarity(&anchor.context_after, &after);
        weight += 2.0;
        if let Some(sym) = &anchor.symbol
            && self.document().is_some()
        {
            sum += symbol_similarity(sym, self.enclosing_symbol(range));
            weight += 1.0;
        }
        sum / weight
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Every tier.
    Full,
    /// Only exact copies of the text (its text was deleted where it was).
    ExactOnly,
}

#[derive(Clone, Debug)]
struct Candidate {
    range: Range<usize>,
    confidence: f32,
    evidence: f32,
}

/// The best candidate, unless another one elsewhere is about as good.
fn pick(mut found: Vec<Candidate>, origin: usize) -> Option<Candidate> {
    let distance = |c: &Candidate| c.range.start.abs_diff(origin);
    found.sort_by(|a, b| {
        b.confidence
            .total_cmp(&a.confidence)
            .then(b.evidence.total_cmp(&a.evidence))
            .then(distance(a).cmp(&distance(b)))
    });
    let mut found = found.into_iter();
    let mut best = found.next()?;
    let rival = found.find(|c| c.range.end <= best.range.start || best.range.end <= c.range.start);
    if let Some(r) = rival
        && r.confidence >= best.confidence - 0.03
        && r.evidence >= best.evidence - 0.05
    {
        // Two places match about as well: don't guess.
        best.confidence = best.confidence.min(MIN_CONFIDENCE - 0.05);
    }
    Some(best)
}

/// Keep the [`MAX_CANDIDATES`] ranges nearest to `origin`.
fn nearest(hits: &mut Vec<Range<usize>>, origin: usize) {
    if hits.len() > MAX_CANDIDATES {
        hits.sort_by_key(|r| r.start.abs_diff(origin));
        hits.truncate(MAX_CANDIDATES);
    }
}

/// How sure an anchor edited in place still is: the similarity of the old
/// and new text, whitespace aside.
fn edited_confidence(old: &str, new: &str) -> f32 {
    let (old, new) = (normalize(old), normalize(new));
    if old == new {
        return 0.95;
    }
    ratio(&old, &new).min(0.95)
}

fn context_similarity(want: &[String], have: &[&str]) -> f32 {
    // Blank lines count only as line breaks: `[]` and `[""]` agree.
    let join =
        |lines: &mut dyn Iterator<Item = &str>| lines.map(normalize).collect::<Vec<_>>().join("\n");
    let want = join(&mut want.iter().map(String::as_str));
    let have = join(&mut have.iter().copied());
    ratio(want.trim(), have.trim())
}

fn symbol_similarity(want: &SymbolPath, have: Option<&Symbol>) -> f32 {
    let Some(s) = have else { return 0.0 };
    let name = s.name == want.name();
    let container = s.container[..] == want.path[..want.path.len().saturating_sub(1)];
    let kind = s.kind.as_str() == want.kind;
    match (name, container, kind) {
        (true, true, true) => 1.0,
        (true, true, false) | (true, false, true) => 0.7,
        // Renamed in place.
        (false, true, true) => 0.5,
        (true, false, false) => 0.3,
        _ => 0.0,
    }
}

/// The lines of a block; a final line break ends the last line rather than
/// starting another.
fn split_lines(block: &str) -> Vec<&str> {
    let block = block.strip_suffix('\n').unwrap_or(block);
    block.split('\n').collect()
}

/// A longest common subsequence of two line-hash lists: the matched index
/// pairs, in order.
fn align(a: &[u64], b: &[u64]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] = if a[i] == b[j] {
                t[(i + 1) * w + j + 1] + 1
            } else {
                t[(i + 1) * w + j].max(t[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            // Blank lines align but don't count as evidence.
            if a[i] != 0 {
                out.push((i, j));
            }
            i += 1;
            j += 1;
        } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Opening minus closing brackets.
fn brackets(s: &str) -> isize {
    s.bytes()
        .map(|b| match b {
            b'{' | b'(' | b'[' => 1,
            b'}' | b')' | b']' => -1,
            _ => 0,
        })
        .sum()
}

/// Whether a word ends or starts between `before` and `after`.
fn word_edge(before: Option<char>, after: Option<char>) -> bool {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    !(word(before) && word(after))
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_finds_common_lines() {
        assert_eq!(align(&[1, 2, 3], &[9, 1, 3, 8]), vec![(0, 1), (2, 2)]);
        assert_eq!(align(&[1, 0, 2], &[1, 0, 2]), vec![(0, 0), (2, 2)]);
        assert!(align(&[1], &[2]).is_empty());
    }

    #[test]
    fn split_lines_drops_the_final_break() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a"), vec!["a"]);
    }
}
