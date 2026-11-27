// SPDX-License-Identifier: Apache-2.0

//! A [`FileDiff`] as the diff body takes it (a unified patch), the hunks a
//! reviewer stages, line selections as `git/stage` takes them, and the
//! content hash that keys "viewed".

use coqo_protocol::methods::git::{DiffLine, FileDiff, LineKind, LineSelection};
use coqo_ui::diff::{LineRange, LineRef, Side};

/// Unchanged lines between two runs of changes that still make one hunk,
/// as `git diff` groups them with its default three lines of context.
pub const HUNK_GAP: usize = 6;

/// The unified patch of `diff`, with git's C-style path quoting and
/// `\ No newline at end of file` markers.
pub fn to_patch(diff: &FileDiff) -> String {
    let a = quote(&format!("a/{}", diff.path));
    let b = quote(&format!("b/{}", diff.path));
    let mut out = format!("diff --git {a} {b}\n");
    match (&diff.old, &diff.new) {
        (None, Some(new)) => out.push_str(&format!("new file mode {:o}\n", new.mode)),
        (Some(old), None) => out.push_str(&format!("deleted file mode {:o}\n", old.mode)),
        (Some(old), Some(new)) if old.mode != new.mode => {
            out.push_str(&format!(
                "old mode {:o}\nnew mode {:o}\n",
                old.mode, new.mode
            ));
        }
        _ => {}
    }
    let old = if diff.old.is_some() {
        a.as_str()
    } else {
        "/dev/null"
    };
    let new = if diff.new.is_some() {
        b.as_str()
    } else {
        "/dev/null"
    };
    if diff.binary {
        out.push_str(&format!("Binary files {old} and {new} differ\n"));
        return out;
    }
    if diff.hunks.is_empty() {
        return out;
    }
    out.push_str(&format!("--- {old}\n+++ {new}\n"));
    for hunk in &diff.hunks {
        // Counted from the lines, so a provider's header slip cannot make
        // the patch unreadable.
        let count = |skip: LineKind| hunk.lines.iter().filter(|l| l.kind != skip).count();
        let (old_lines, new_lines) = (count(LineKind::Added), count(LineKind::Removed));
        out.push_str(&format!(
            "@@ -{} +{} @@",
            range(hunk.old_start, u32::try_from(old_lines).unwrap_or(u32::MAX)),
            range(hunk.new_start, u32::try_from(new_lines).unwrap_or(u32::MAX))
        ));
        if let Some(section) = &hunk.section {
            out.push(' ');
            out.push_str(section);
        }
        out.push('\n');
        for line in &hunk.lines {
            out.push(match line.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            out.push_str(&line.text);
            out.push('\n');
            if !line.newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    out
}

fn range(start: u32, lines: u32) -> String {
    if lines == 1 {
        start.to_string()
    } else {
        format!("{start},{lines}")
    }
}

/// git's C-style quoting, for names git would quote.
fn quote(name: &str) -> String {
    let plain = name
        .chars()
        .all(|c| (c.is_ascii_graphic() && c != '"' && c != '\\') || c == ' ');
    if plain {
        return name.to_owned();
    }
    let mut q = String::from("\"");
    for b in name.bytes() {
        match b {
            b'"' => q.push_str("\\\""),
            b'\\' => q.push_str("\\\\"),
            b'\t' => q.push_str("\\t"),
            b'\n' => q.push_str("\\n"),
            0x20..=0x7e => q.push(char::from(b)),
            _ => q.push_str(&format!("\\{b:03o}")),
        }
    }
    q.push('"');
    q
}

/// A run of changes a reviewer stages or unstages as one, grouped as
/// `git diff` groups hunks (changes at most [`HUNK_GAP`] lines apart).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewHunk {
    /// The first changed line, where the hunk's controls go.
    pub first: LineRef,
    /// The changed lines, as `git/stage` takes them.
    pub lines: LineSelection,
}

/// Every line of `diff` in patch order.
fn lines(diff: &FileDiff) -> impl Iterator<Item = &DiffLine> {
    diff.hunks.iter().flat_map(|h| &h.lines)
}

/// Where a line of the patch is shown: removed and context lines by their
/// old number, added lines by their new one.
fn address(line: &DiffLine) -> Option<(Side, u32)> {
    match line.kind {
        LineKind::Added => line.new_line.map(|n| (Side::New, n)),
        _ => line.old_line.map(|n| (Side::Old, n)),
    }
}

/// The hunks of `diff`.
pub fn hunks(diff: &FileDiff) -> Vec<ReviewHunk> {
    let mut out: Vec<ReviewHunk> = Vec::new();
    for hunk in &diff.hunks {
        // Context since the last change, within this patch hunk.
        let mut gap: Option<usize> = None;
        for line in &hunk.lines {
            if line.kind == LineKind::Context {
                gap = gap.map(|g| g + 1);
                continue;
            }
            let joins = gap.is_some_and(|g| g <= HUNK_GAP);
            if !joins {
                let Some((side, n)) = address(line) else {
                    continue;
                };
                out.push(ReviewHunk {
                    first: LineRef::new(diff.path.clone(), side, n),
                    lines: LineSelection::default(),
                });
            }
            gap = Some(0);
            let current = out.last_mut().expect("a hunk was started");
            match line.kind {
                LineKind::Added => current.lines.added.extend(line.new_line),
                LineKind::Removed => current.lines.removed.extend(line.old_line),
                LineKind::Context => {}
            }
        }
    }
    out
}

/// The changed lines of `diff` that `range` covers, in display order from
/// its first line to its last (in unified mode a range may start on an old
/// line and end on a new one). Context lines count by either number.
/// Empty when the range is not in `diff`.
pub fn selection(diff: &FileDiff, range: &LineRange) -> LineSelection {
    let matches = |line: &DiffLine, (side, n): (Side, u32)| match side {
        Side::Old => line.old_line == Some(n) && line.kind != LineKind::Added,
        Side::New => line.new_line == Some(n) && line.kind != LineKind::Removed,
    };
    let all: Vec<&DiffLine> = lines(diff).collect();
    let start = all.iter().position(|l| matches(l, range.start));
    let end = all.iter().rposition(|l| matches(l, range.end));
    let mut out = LineSelection::default();
    let (Some(mut start), Some(mut end)) = (start, end) else {
        return out;
    };
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    // In split mode the ends are on one side: only that side's changes.
    let one_side = (range.start.0 == range.end.0).then_some(range.start.0);
    for line in &all[start..=end] {
        match line.kind {
            LineKind::Added if one_side != Some(Side::Old) => out.added.extend(line.new_line),
            LineKind::Removed if one_side != Some(Side::New) => out.removed.extend(line.old_line),
            _ => {}
        }
    }
    out
}

/// A stable hash of what `diff` changes: its sides' presence and modes and
/// every changed line with its numbers. Context does not count, so the same
/// change fetched with more context hashes the same; any other change to
/// the file (here or elsewhere in it) changes the hash.
pub fn content_hash(diff: &FileDiff) -> String {
    let mut h = Fnv::default();
    h.write(diff.path.as_bytes());
    for side in [&diff.old, &diff.new] {
        match side {
            Some(s) => {
                h.write(b"s");
                h.write(&s.mode.to_le_bytes());
                if let Some(oid) = &s.oid {
                    h.write(oid.as_bytes());
                }
            }
            None => h.write(b"-"),
        }
    }
    h.write(&[u8::from(diff.binary)]);
    for line in lines(diff).filter(|l| l.kind != LineKind::Context) {
        h.write(if line.kind == LineKind::Added {
            b"+"
        } else {
            b"-"
        });
        h.write(&line.old_line.unwrap_or(0).to_le_bytes());
        h.write(&line.new_line.unwrap_or(0).to_le_bytes());
        h.write(&[u8::from(line.newline)]);
        h.write(line.text.as_bytes());
        h.write(b"\n");
    }
    format!("{:016x}", h.0)
}

/// FNV-1a, 64-bit: stable across builds and platforms (unlike `std`'s
/// hasher), which persisted hashes need.
pub struct Fnv(pub u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    pub fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn hash(bytes: &[u8]) -> u64 {
        let mut h = Self::default();
        h.write(bytes);
        h.0
    }
}
