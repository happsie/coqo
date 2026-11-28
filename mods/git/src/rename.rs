// SPDX-License-Identifier: Apache-2.0

//! Rename detection, as git's `diffcore-rename` does it with the default
//! settings (`git status`, `git diff -M`): a deleted file and an added file
//! pair up as a rename when they are at least 50% similar.
//!
//! The steps are git's:
//!
//! 1. **Exact renames:** the same contents (and, for anything but regular
//!    files, the same mode), preferring a source with the same file name.
//! 2. **Same file name:** a source and a destination whose file name is
//!    unique on both sides pair up when they are at least 75% similar.
//! 3. **Everything else:** every remaining pair is scored, the best four
//!    sources per destination are kept, and the pairs are taken best first
//!    (a same-named source wins a tie). Skipped when there are more than
//!    1000 × 1000 pairs (`diff.renameLimit`'s default).
//!
//! Similarity is git's `estimate_similarity`: both files are cut into
//! spans (a line, or 64 bytes at most), each span is hashed, and the score
//! is the bytes of the source found again in the destination, over the
//! larger file's size. Only regular files are scored; symlinks and
//! submodules rename only exactly. Copies are not detected.

use std::collections::HashMap;

/// git's `MAX_SCORE`: a score of 100%.
pub(crate) const MAX_SCORE: u64 = 60_000;
/// git's `DEFAULT_RENAME_SCORE`: 50%.
const MIN_SCORE: u64 = 30_000;
/// Same-name pairs need halfway between the minimum and 100%: 75%.
const MIN_BASENAME_SCORE: u64 = MIN_SCORE + (MAX_SCORE - MIN_SCORE) / 2;
/// `diff.renameLimit`'s default.
const RENAME_LIMIT: u64 = 1000;
/// git's `NUM_CANDIDATE_PER_DST`.
const CANDIDATES_PER_DEST: usize = 4;
/// git's `HASHBASE`.
const HASHBASE: u32 = 107_927;

/// A deleted (source) or added (destination) file.
#[derive(Debug)]
pub(crate) struct Candidate<'a> {
    pub path: &'a str,
    /// The contents compared (a work-tree file cleaned as `git add` would).
    pub data: &'a [u8],
    pub mode: u32,
    /// Not binary: a CR before a LF is not counted.
    pub text: bool,
}

/// A source and destination paired as a rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pair {
    pub source: usize,
    pub dest: usize,
    /// Of [`MAX_SCORE`].
    pub score: u64,
}

fn is_regular(mode: u32) -> bool {
    mode & 0o170000 == 0o100000
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Span sizes by span hash: git's `hash_chars`.
fn spans(c: &Candidate<'_>) -> HashMap<u32, u64> {
    let mut out: HashMap<u32, u64> = HashMap::new();
    let (mut a1, mut a2, mut n) = (0u32, 0u32, 0u64);
    let data = c.data;
    let mut i = 0;
    while i < data.len() {
        let ch = data[i];
        i += 1;
        // A CR in a CRLF of a text file is not counted.
        if c.text && ch == b'\r' && data.get(i) == Some(&b'\n') {
            continue;
        }
        let old1 = a1;
        a1 = (a1 << 7) ^ (a2 >> 25);
        a2 = (a2 << 7) ^ (old1 >> 25);
        a1 = a1.wrapping_add(u32::from(ch));
        n += 1;
        if n < 64 && ch != b'\n' {
            continue;
        }
        let hash = a1.wrapping_add(a2.wrapping_mul(0x61)) % HASHBASE;
        *out.entry(hash).or_default() += n;
        n = 0;
        a1 = 0;
        a2 = 0;
    }
    if n > 0 {
        let hash = a1.wrapping_add(a2.wrapping_mul(0x61)) % HASHBASE;
        *out.entry(hash).or_default() += n;
    }
    out
}

/// git's `estimate_similarity`, 0 when below `min`.
fn similarity(
    src: &Candidate<'_>,
    dst: &Candidate<'_>,
    min: u64,
    cache: &mut [Option<HashMap<u32, u64>>],
    src_ix: usize,
    dst_spans: &HashMap<u32, u64>,
) -> u64 {
    if !is_regular(src.mode) || !is_regular(dst.mode) {
        return 0;
    }
    let (a, b) = (src.data.len() as u64, dst.data.len() as u64);
    let (max, base) = (a.max(b), a.min(b));
    // Too different in size to reach `min`.
    if max * (MAX_SCORE - min) < (max - base) * MAX_SCORE {
        return 0;
    }
    if b == 0 {
        return 0;
    }
    let src_spans = cache[src_ix].get_or_insert_with(|| spans(src));
    let copied: u64 = src_spans
        .iter()
        .map(|(h, n)| (*n).min(dst_spans.get(h).copied().unwrap_or(0)))
        .sum();
    copied * MAX_SCORE / max
}

/// The renames among `sources` (deleted files) and `dests` (added files).
pub(crate) fn find(sources: &[Candidate<'_>], dests: &[Candidate<'_>]) -> Vec<Pair> {
    let mut pairs = Vec::new();
    if sources.is_empty() || dests.is_empty() {
        return pairs;
    }
    let mut used = vec![false; sources.len()];
    let mut done = vec![false; dests.len()];

    // 1. Exact renames.
    for (d, dst) in dests.iter().enumerate() {
        let mut best: Option<(usize, u8)> = None;
        for (s, src) in sources.iter().enumerate() {
            if used[s] || src.data != dst.data {
                continue;
            }
            if (!is_regular(src.mode) || !is_regular(dst.mode)) && src.mode != dst.mode {
                continue;
            }
            let score = 1 + u8::from(basename(src.path) == basename(dst.path));
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((s, score));
                if score == 2 {
                    break;
                }
            }
        }
        if let Some((s, _)) = best {
            used[s] = true;
            done[d] = true;
            pairs.push(Pair {
                source: s,
                dest: d,
                score: MAX_SCORE,
            });
        }
    }

    let mut cache: Vec<Option<HashMap<u32, u64>>> = vec![None; sources.len()];

    // 2. Same file name, unique on both sides.
    let mut src_names: HashMap<&str, Option<usize>> = HashMap::new();
    for (s, src) in sources.iter().enumerate().filter(|(s, _)| !used[*s]) {
        src_names
            .entry(basename(src.path))
            .and_modify(|e| *e = None)
            .or_insert(Some(s));
    }
    let mut dst_names: HashMap<&str, Option<usize>> = HashMap::new();
    for (d, dst) in dests.iter().enumerate().filter(|(d, _)| !done[*d]) {
        dst_names
            .entry(basename(dst.path))
            .and_modify(|e| *e = None)
            .or_insert(Some(d));
    }
    for s in 0..sources.len() {
        if used[s] {
            continue;
        }
        let name = basename(sources[s].path);
        let (Some(Some(src_ix)), Some(Some(d))) = (src_names.get(name), dst_names.get(name)) else {
            continue;
        };
        if *src_ix != s || done[*d] {
            continue;
        }
        let dst_spans = spans(&dests[*d]);
        let score = similarity(
            &sources[s],
            &dests[*d],
            MIN_SCORE,
            &mut cache,
            s,
            &dst_spans,
        );
        if score < MIN_BASENAME_SCORE {
            continue;
        }
        used[s] = true;
        done[*d] = true;
        pairs.push(Pair {
            source: s,
            dest: *d,
            score,
        });
    }

    // 3. The best of every remaining pair.
    let srcs: Vec<usize> = (0..sources.len()).filter(|s| !used[*s]).collect();
    let dsts: Vec<usize> = (0..dests.len()).filter(|d| !done[*d]).collect();
    if srcs.is_empty()
        || dsts.is_empty()
        || (srcs.len() as u64) * (dsts.len() as u64) > RENAME_LIMIT * RENAME_LIMIT
    {
        return pairs;
    }
    // (score, same name, source, destination)
    let mut scored: Vec<(u64, bool, usize, usize)> = Vec::new();
    for &d in &dsts {
        let dst_spans = spans(&dests[d]);
        let mut best: Vec<(u64, bool, usize, usize)> = Vec::new();
        for &s in &srcs {
            let score = similarity(&sources[s], &dests[d], MIN_SCORE, &mut cache, s, &dst_spans);
            if score < MIN_SCORE {
                continue;
            }
            let same = basename(sources[s].path) == basename(dests[d].path);
            best.push((score, same, s, d));
        }
        best.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        best.truncate(CANDIDATES_PER_DEST);
        scored.extend(best);
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    for (score, _, s, d) in scored {
        if done[d] || used[s] {
            continue;
        }
        used[s] = true;
        done[d] = true;
        pairs.push(Pair {
            source: s,
            dest: d,
            score,
        });
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c<'a>(path: &'a str, data: &'a str) -> Candidate<'a> {
        Candidate {
            path,
            data: data.as_bytes(),
            mode: 0o100644,
            text: true,
        }
    }

    #[test]
    fn exact_then_similar_then_nothing() {
        let body: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let edited = body.replace("line 3\n", "three\n");
        let sources = [
            c("a/same.txt", &body),
            c("old.txt", &edited),
            c("gone.txt", "x\n"),
        ];
        let dests = [
            c("b/same.txt", &body),
            c("new.txt", &body),
            c("other.txt", "y\n"),
        ];
        let pairs = find(&sources, &dests);
        assert_eq!(pairs.len(), 2, "{pairs:?}");
        assert_eq!((pairs[0].source, pairs[0].dest), (0, 0));
        assert_eq!(pairs[0].score, MAX_SCORE);
        assert_eq!((pairs[1].source, pairs[1].dest), (1, 1));
        assert!(pairs[1].score >= MIN_SCORE && pairs[1].score < MAX_SCORE);
    }

    #[test]
    fn crlf_counts_as_lf_in_text() {
        let lf: String = (0..10).map(|i| format!("line {i}\n")).collect();
        let crlf = lf.replace('\n', "\r\n");
        let pairs = find(&[c("a.txt", &lf)], &[c("b.txt", &crlf)]);
        assert_eq!(pairs.len(), 1);
    }
}
