// SPDX-License-Identifier: Apache-2.0

//! Attributing disk writes to agent sessions.
//!
//! Claude Code edits files by writing them itself (ADR 0005); the daemon
//! only sees the result through the file watcher. Before the write, though,
//! the agent's ACP `tool_call` (or `tool_call_update`) carries a `diff`
//! content block: the path, the old text and the new text. The ACP host
//! registers it with [`BufferService::expect_agent_write`]; when the file
//! then changes on disk and the change matches, the buffer change gets
//! `origin: {kind: "agent", id: <session>}` instead of `disk`.
//!
//! A change matches an expected write when the new file content equals the
//! diff's new text (a whole-file write), or when the changed region of the
//! file lies inside an occurrence of the new text and the region it
//! replaced lies inside an occurrence of the old text (Claude Code's `Edit`
//! tool reports just the replaced snippet). Expected writes are matched
//! once and expire after a while.
//!
//! [`BufferService::expect_agent_write`]: super::BufferService::expect_agent_write

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use coqo_buffers::{Rope, diff};

#[derive(Debug)]
struct Expected {
    session: String,
    path: PathBuf,
    old_text: Option<String>,
    new_text: String,
    at: Instant,
}

/// Agent writes announced but not yet seen on disk.
#[derive(Debug, Default)]
pub(crate) struct AgentWrites {
    pending: Vec<Expected>,
}

/// Most writes kept waiting at once; the oldest are dropped first.
const MAX_PENDING: usize = 256;

impl AgentWrites {
    pub fn expect(
        &mut self,
        session: String,
        path: PathBuf,
        old_text: Option<String>,
        new_text: String,
    ) {
        if self.pending.len() >= MAX_PENDING {
            self.pending.remove(0);
        }
        self.pending.push(Expected {
            session,
            path,
            old_text,
            new_text,
            at: Instant::now(),
        });
    }

    /// The session whose announced write explains `before` → `after` of the
    /// file at `path` (canonical), consuming that announcement.
    pub fn take_match(
        &mut self,
        path: &Path,
        before: &Rope,
        after: &Rope,
        ttl: Duration,
    ) -> Option<String> {
        self.pending.retain(|e| e.at.elapsed() < ttl);
        if !self.pending.iter().any(|e| e.path == path) {
            return None;
        }
        let op = diff(before, after);
        let edit = op.to_edits().into_iter().next()?;
        let (before, after) = (before.to_string(), after.to_string());
        let removed = edit.start..edit.end;
        let added = edit.start..edit.start + edit.text.len();
        let idx = self
            .pending
            .iter()
            .position(|e| e.path == path && matches(e, &before, &after, &removed, &added))?;
        Some(self.pending.remove(idx).session)
    }
}

fn covered(text: &str, needle: &str, range: &std::ops::Range<usize>) -> bool {
    !needle.is_empty()
        && text
            .match_indices(needle)
            .any(|(i, m)| i <= range.start && range.end <= i + m.len())
}

fn matches(
    e: &Expected,
    before: &str,
    after: &str,
    removed: &std::ops::Range<usize>,
    added: &std::ops::Range<usize>,
) -> bool {
    if after == e.new_text {
        return true;
    }
    let old = e.old_text.as_deref().unwrap_or("");
    let added_ok = if added.is_empty() {
        true
    } else {
        covered(after, &e.new_text, added)
    };
    let removed_ok = if removed.is_empty() {
        true
    } else {
        covered(before, old, removed)
    };
    // A pure deletion needs the old text to say what was removed.
    let says_something = !e.new_text.is_empty() || !old.is_empty();
    added_ok && removed_ok && says_something
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn whole_file_and_snippet_writes_match() {
        let p = PathBuf::from("/w/src/lib.rs");
        let mut w = AgentWrites::default();
        w.expect("s1".into(), p.clone(), None, "fn a() {}\n".into());
        assert_eq!(
            w.take_match(&p, &rope(""), &rope("fn a() {}\n"), TTL),
            Some("s1".into())
        );
        assert_eq!(
            w.take_match(&p, &rope(""), &rope("fn a() {}\n"), TTL),
            None,
            "consumed"
        );

        // Claude Code's Edit tool: old_string / new_string snippets.
        let before = "fn main() {\n    old();\n}\n";
        let after = "fn main() {\n    new_call();\n}\n";
        w.expect(
            "s2".into(),
            p.clone(),
            Some("    old();".into()),
            "    new_call();".into(),
        );
        assert_eq!(
            w.take_match(&p, &rope(before), &rope(after), TTL),
            Some("s2".into())
        );
    }

    #[test]
    fn unrelated_changes_do_not_match() {
        let p = PathBuf::from("/w/a.rs");
        let mut w = AgentWrites::default();
        w.expect("s1".into(), p.clone(), Some("x".into()), "y".into());
        // Another file.
        assert_eq!(
            w.take_match(Path::new("/w/b.rs"), &rope("x"), &rope("y"), TTL),
            None
        );
        // Another change to this file.
        assert_eq!(w.take_match(&p, &rope("x\n"), &rope("x\nz\n"), TTL), None);
        // Expired.
        assert_eq!(
            w.take_match(&p, &rope("x"), &rope("y"), Duration::ZERO),
            None
        );
    }
}
