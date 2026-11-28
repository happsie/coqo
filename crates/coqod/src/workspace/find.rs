// SPDX-License-Identifier: Apache-2.0

//! Fuzzy matching of file paths for `fs/find`.
//!
//! A query matches a path when its characters appear in the path in order
//! (whitespace in the query is ignored). Smart case: a query with an
//! upper-case letter matches case-sensitively, any other ignores case.
//!
//! Among the ways a query can match one path, the best scoring one is found
//! by dynamic programming over (query character, path character), like fzf's
//! v2 algorithm, with the same kind of bonuses:
//!
//! - every matched character scores, more at the start of a path segment
//!   (after `/`), after `_ - . ` and at camelCase humps;
//! - runs of consecutive characters score more, gaps cost a little;
//! - characters matched in the file name (after the last `/`) score more,
//!   and a query that is the file name (or its stem) scores most;
//! - longer paths lose a little, so `main.rs` beats `deep/down/main.rs`.
//!
//! [`find`] scores every indexed path on a few threads, keeps the best `n`,
//! and works out the matched positions only for those.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use coqo_protocol::methods::fs::FindMatch;
use coqo_protocol::methods::workspace::RootId;

const SCORE_MATCH: i32 = 16;
/// At the start of the path or right after a `/`.
const BONUS_SEGMENT: i32 = 10;
/// After `_`, `-`, `.`, a space, or another non-alphanumeric character.
const BONUS_BOUNDARY: i32 = 8;
/// An upper-case letter after a lower-case one, or a digit after a letter.
const BONUS_CAMEL: i32 = 7;
/// Each character right after the previous matched one.
const BONUS_CONSECUTIVE: i32 = 6;
/// Each character matched in the file name.
const BONUS_NAME: i32 = 4;
/// The query is the file name, or the file name without its extension.
const BONUS_EXACT_NAME: i32 = 40;
/// The first matched character's bonus counts this many times.
const FIRST_MULTIPLIER: i32 = 2;
const GAP_START: i32 = 3;
const GAP_EXTENSION: i32 = 1;
/// One point off per this many characters of path.
const LENGTH_PENALTY_PER: i32 = 6;

/// Paths longer than this (in characters) are matched without the dynamic
/// programme: only the greedy alignment is scored.
const MAX_DP_CHARS: usize = 512;

/// Paths per unit of work handed to a thread.
const CHUNK: usize = 8_192;
/// Below this many paths, one thread does it all.
const PARALLEL_FROM: usize = 16_384;
/// At most this many threads match one query.
const MAX_THREADS: usize = 8;

const NEG: i32 = i32::MIN / 4;

/// A parsed query.
#[derive(Debug, Clone)]
pub(crate) struct Pattern {
    /// The query's characters (folded to lower case unless case-sensitive).
    chars: Vec<char>,
    case_sensitive: bool,
    /// The same as bytes, when the query is ASCII: the fast path.
    ascii: Option<Vec<u8>>,
}

impl Pattern {
    pub fn new(query: &str) -> Self {
        let raw: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
        let case_sensitive = raw.iter().any(|c| c.is_uppercase());
        let chars: Vec<char> = if case_sensitive {
            raw
        } else {
            raw.into_iter().map(fold).collect()
        };
        let ascii = chars
            .iter()
            .all(char::is_ascii)
            .then(|| chars.iter().map(|&c| c as u8).collect());
        Self {
            chars,
            case_sensitive,
            ascii,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Reusable buffers for scoring paths.
#[derive(Debug, Default)]
pub(crate) struct Matcher {
    chars: Vec<char>,
    offsets: Vec<u32>,
    bonus: Vec<i32>,
    scores: Vec<i32>,
    from: Vec<u32>,
}

fn bonus_at(prev: Option<char>, c: char) -> i32 {
    let Some(prev) = prev else {
        return BONUS_SEGMENT;
    };
    if prev == '/' {
        BONUS_SEGMENT
    } else if !prev.is_alphanumeric() {
        if c.is_alphanumeric() {
            BONUS_BOUNDARY
        } else {
            0
        }
    } else if (prev.is_lowercase() && c.is_uppercase()) || (prev.is_alphabetic() && c.is_numeric())
    {
        BONUS_CAMEL
    } else {
        0
    }
}

impl Matcher {
    /// Whether `pattern` can match `path` at all: a quick subsequence check.
    fn prefilter(pattern: &Pattern, path: &str) -> bool {
        match &pattern.ascii {
            Some(q) => {
                let mut qi = 0;
                for &b in path.as_bytes() {
                    let b = if pattern.case_sensitive {
                        b
                    } else {
                        b.to_ascii_lowercase()
                    };
                    if b == q[qi] {
                        qi += 1;
                        if qi == q.len() {
                            return true;
                        }
                    }
                }
                false
            }
            None => {
                let mut qi = 0;
                for c in path.chars() {
                    let c = if pattern.case_sensitive { c } else { fold(c) };
                    if c == pattern.chars[qi] {
                        qi += 1;
                        if qi == pattern.chars.len() {
                            return true;
                        }
                    }
                }
                false
            }
        }
    }

    /// The best score of `pattern` in `path`, and with `positions` the byte
    /// offsets of the matched characters. `None` if it doesn't match.
    pub fn score(
        &mut self,
        pattern: &Pattern,
        path: &str,
        positions: Option<&mut Vec<u32>>,
    ) -> Option<i32> {
        if pattern.is_empty() {
            return Some(0);
        }
        if !Self::prefilter(pattern, path) {
            return None;
        }
        self.chars.clear();
        self.offsets.clear();
        for (off, c) in path.char_indices() {
            self.chars
                .push(if pattern.case_sensitive { c } else { fold(c) });
            self.offsets.push(off as u32);
        }
        let n = self.chars.len();
        let name_start = path.rfind('/').map_or(0, |i| path[..=i].chars().count());
        self.bonus.clear();
        let mut prev = None;
        for (j, c) in path.chars().enumerate() {
            let name = if j >= name_start { BONUS_NAME } else { 0 };
            self.bonus.push(bonus_at(prev, c) + name);
            prev = Some(c);
        }
        let q = &pattern.chars;
        let score = if n <= MAX_DP_CHARS {
            self.align(q, positions)
        } else {
            self.greedy(q, positions)
        }?;
        let name = &self.chars[name_start..];
        let stem = name
            .iter()
            .rposition(|&c| c == '.')
            .filter(|&dot| dot > 0)
            .map_or(name, |dot| &name[..dot]);
        let exact = if name == q.as_slice() || stem == q.as_slice() {
            BONUS_EXACT_NAME
        } else {
            0
        };
        Some(score + exact - n as i32 / LENGTH_PENALTY_PER)
    }

    /// The dynamic programme: `scores[i * n + j]` is the best score of the
    /// first `i + 1` query characters with the last at path character `j`.
    fn align(&mut self, q: &[char], positions: Option<&mut Vec<u32>>) -> Option<i32> {
        let (m, n) = (q.len(), self.chars.len());
        self.scores.clear();
        self.scores.resize(m * n, NEG);
        let want = positions.is_some();
        if want {
            self.from.clear();
            self.from.resize(m * n, u32::MAX);
        }
        for j in 0..n {
            if self.chars[j] == q[0] {
                self.scores[j] = SCORE_MATCH + self.bonus[j] * FIRST_MULTIPLIER;
            }
        }
        for i in 1..m {
            let (prev_row, row) = self.scores.split_at_mut(i * n);
            let prev_row = &prev_row[(i - 1) * n..];
            let row = &mut row[..n];
            // The best `prev_row[k] - gap(j - k - 1)` for k <= j - 2.
            let (mut run, mut run_k) = (NEG, u32::MAX);
            for j in i..n {
                if j >= 2 {
                    let k = j - 2;
                    let open = prev_row[k] - GAP_START;
                    let extended = run - GAP_EXTENSION;
                    if open >= extended {
                        (run, run_k) = (open, k as u32);
                    } else {
                        run = extended;
                    }
                }
                if self.chars[j] != q[i] {
                    continue;
                }
                let diagonal = prev_row[j - 1];
                let diagonal = if diagonal > NEG {
                    diagonal + BONUS_CONSECUTIVE
                } else {
                    NEG
                };
                let (best, k) = if diagonal >= run {
                    (diagonal, (j - 1) as u32)
                } else {
                    (run, run_k)
                };
                if best <= NEG {
                    continue;
                }
                row[j] = best + SCORE_MATCH + self.bonus[j];
                if want {
                    self.from[i * n + j] = k;
                }
            }
        }
        let last = &self.scores[(m - 1) * n..];
        let (end, &best) = last
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(&a.0)))?;
        if best <= NEG {
            return None;
        }
        if let Some(positions) = positions {
            positions.clear();
            positions.resize(m, 0);
            let mut j = end;
            for i in (0..m).rev() {
                positions[i] = self.offsets[j];
                if i > 0 {
                    j = self.from[i * n + j] as usize;
                }
            }
        }
        Some(best)
    }

    /// For very long paths: the first match of each character, scored the
    /// same way.
    fn greedy(&mut self, q: &[char], positions: Option<&mut Vec<u32>>) -> Option<i32> {
        let mut score = 0;
        let mut prev: Option<usize> = None;
        let mut at = Vec::with_capacity(q.len());
        let mut j = 0;
        for (i, &c) in q.iter().enumerate() {
            while j < self.chars.len() && self.chars[j] != c {
                j += 1;
            }
            if j == self.chars.len() {
                return None;
            }
            let bonus = self.bonus[j] * if i == 0 { FIRST_MULTIPLIER } else { 1 };
            score += SCORE_MATCH + bonus;
            match prev {
                Some(p) if p + 1 == j => score += BONUS_CONSECUTIVE,
                Some(p) => score -= GAP_START + GAP_EXTENSION * (j - p - 2) as i32,
                None => {}
            }
            at.push(self.offsets[j]);
            prev = Some(j);
            j += 1;
        }
        if let Some(positions) = positions {
            *positions = at;
        }
        Some(score)
    }
}

/// The files of one root, as the index has them now.
pub(crate) type RootSnapshot = (RootId, Arc<[Arc<str>]>);

/// A ranked candidate: score, then shorter, then by path. Greater is better.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ranked {
    score: i32,
    root: usize,
    path: Arc<str>,
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.path.len().cmp(&self.path.len()))
            .then_with(|| other.path.cmp(&self.path))
            .then_with(|| other.root.cmp(&self.root))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The best `limit` matches of `pattern` among `roots`' files, best first,
/// and how many matched in all.
pub(crate) fn find(
    roots: &[RootSnapshot],
    pattern: &Pattern,
    limit: usize,
) -> (Vec<FindMatch>, u64) {
    let total_files: usize = roots.iter().map(|(_, f)| f.len()).sum();
    // Units of work: (root index, range of its files).
    let mut units = Vec::new();
    for (r, (_, files)) in roots.iter().enumerate() {
        let mut start = 0;
        while start < files.len() {
            let end = (start + CHUNK).min(files.len());
            units.push((r, start..end));
            start = end;
        }
    }
    let threads = if total_files < PARALLEL_FROM {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_THREADS)
            .min(units.len())
    };
    let next = AtomicUsize::new(0);
    let work = |_: usize| {
        let mut matcher = Matcher::default();
        let mut heap: BinaryHeap<Reverse<Ranked>> = BinaryHeap::with_capacity(limit + 1);
        let mut matched = 0u64;
        loop {
            let u = next.fetch_add(1, AtomicOrdering::Relaxed);
            let Some((r, range)) = units.get(u) else {
                break;
            };
            for path in &roots[*r].1[range.clone()] {
                let score = if pattern.is_empty() {
                    // Nearest the root first.
                    Some(-(path.matches('/').count() as i32))
                } else {
                    matcher.score(pattern, path, None)
                };
                let Some(score) = score else {
                    continue;
                };
                matched += 1;
                if limit == 0 {
                    continue;
                }
                let ranked = Ranked {
                    score,
                    root: *r,
                    path: Arc::clone(path),
                };
                if heap.len() < limit {
                    heap.push(Reverse(ranked));
                } else if heap.peek().is_some_and(|w| ranked > w.0) {
                    heap.pop();
                    heap.push(Reverse(ranked));
                }
            }
        }
        (heap, matched)
    };
    let parts: Vec<(BinaryHeap<Reverse<Ranked>>, u64)> = if threads <= 1 {
        vec![work(0)]
    } else {
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads).map(|t| s.spawn(move || work(t))).collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        })
    };
    let total = parts.iter().map(|(_, n)| n).sum();
    let mut best: Vec<Ranked> = parts
        .into_iter()
        .flat_map(|(heap, _)| heap.into_iter().map(|r| r.0))
        .collect();
    best.sort_by(|a, b| b.cmp(a));
    best.truncate(limit);
    let mut matcher = Matcher::default();
    let matches = best
        .into_iter()
        .map(|r| {
            let mut positions = Vec::new();
            if !pattern.is_empty() {
                matcher.score(pattern, &r.path, Some(&mut positions));
            }
            FindMatch {
                root: roots[r.root].0,
                path: r.path.to_string(),
                score: i64::from(r.score),
                positions,
            }
        })
        .collect();
    (matches, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, path: &str) -> Option<i32> {
        Matcher::default().score(&Pattern::new(query), path, None)
    }

    fn positions(query: &str, path: &str) -> Vec<u32> {
        let mut out = Vec::new();
        Matcher::default()
            .score(&Pattern::new(query), path, Some(&mut out))
            .expect("matches");
        out
    }

    fn ranked(query: &str, paths: &[&str]) -> Vec<String> {
        let files: Arc<[Arc<str>]> = paths.iter().map(|p| Arc::from(*p)).collect();
        let (matches, _) = find(&[(RootId(1), files)], &Pattern::new(query), 10);
        matches.into_iter().map(|m| m.path).collect()
    }

    #[test]
    fn matches_subsequences_with_smart_case() {
        assert!(score("main", "src/main.rs").is_some());
        assert!(score("smr", "src/main.rs").is_some());
        assert!(score("xyz", "src/main.rs").is_none());
        assert!(score("mian", "src/main.rs").is_none(), "order matters");
        assert!(score("MAIN", "src/main.rs").is_none(), "smart case");
        assert!(score("main", "src/MAIN.rs").is_some());
        assert!(score("Main", "src/Main.kt").is_some());
        assert!(score("src main", "src/main.rs").is_some(), "spaces ignored");
        assert!(score("ü", "docs/Über.md").is_some(), "non-ASCII folds");
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn positions_are_the_best_alignment() {
        // `main` in the file name, not the `m`…`a`…`i`…`n` spread through
        // the folders.
        assert_eq!(positions("main", "my/app/in/now/main.rs"), [14, 15, 16, 17]);
        // Segment starts beat the first occurrence.
        assert_eq!(positions("fb", "foo/bar.rs"), [0, 4]);
        assert_eq!(positions("ür", "Über/uber.md"), [0, 4]);
        // Byte offsets, past a multi-byte character.
        assert_eq!(positions("x", "é/x"), [3]);
    }

    #[test]
    fn ranks_file_names_segments_and_short_paths_first() {
        assert_eq!(
            ranked(
                "main",
                &["src/domain/lib.rs", "src/main.rs", "a/b/c/d/main.rs"]
            ),
            ["src/main.rs", "a/b/c/d/main.rs", "src/domain/lib.rs"]
        );
        // The file name itself wins over a longer name containing it.
        assert_eq!(
            ranked("tree.rs", &["src/files/subtree.rs", "src/files/tree.rs"]),
            ["src/files/tree.rs", "src/files/subtree.rs"]
        );
        // Segment starts beat letters inside a word.
        assert_eq!(
            ranked("fe", &["src/safety.rs", "files/editor.rs"])[0],
            "files/editor.rs"
        );
        // So do camelCase humps.
        assert_eq!(
            ranked("ft", &["src/after.rs", "src/FileTree.tsx"])[0],
            "src/FileTree.tsx"
        );
        // Consecutive runs beat scattered letters.
        assert_eq!(
            ranked("palette", &["p/a/l/e/t/t/e.rs", "commands/palette.rs"])[0],
            "commands/palette.rs"
        );
        // An empty query lists the files nearest the root first.
        assert_eq!(
            ranked("", &["a/b/c.rs", "z.rs", "a/b.rs", "Cargo.toml"]),
            ["z.rs", "Cargo.toml", "a/b.rs", "a/b/c.rs"]
        );
    }

    #[test]
    fn counts_every_match_and_keeps_the_best_n() {
        let files: Arc<[Arc<str>]> = (0..50_000)
            .map(|i| Arc::from(format!("pkg{}/mod{}/f{i}.rs", i % 7, i % 13)))
            .collect();
        let pattern = Pattern::new("f4999");
        let (matches, total) = find(&[(RootId(1), Arc::clone(&files))], &pattern, 5);
        assert_eq!(matches[0].path, "pkg1/mod7/f4999.rs");
        assert_eq!(matches.len(), 5);
        let mut matcher = Matcher::default();
        let expected = files
            .iter()
            .filter(|f| matcher.score(&pattern, f, None).is_some())
            .count();
        assert_eq!(total, expected as u64, "counted on every thread");
        assert!(total > 5);
        assert!(matches.windows(2).all(|w| w[0].score >= w[1].score));
    }

    #[test]
    fn very_long_paths_still_match() {
        let long = format!("{}/needle.rs", "d/".repeat(400));
        assert!(score("needle", &long).is_some());
        assert!(positions("needle", &long).len() == 6);
    }
}
