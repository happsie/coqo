// SPDX-License-Identifier: Apache-2.0

//! Findings written as text (`file:line — issue`) in an assistant message,
//! turned into gutter notes linked back to the message.

use crate::refs::{self, Verdict};
use crate::workspace::Workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    pub start: u32,
    pub end: u32,
    /// The message line it is on (a list item, a heading's paragraph).
    pub text: String,
    /// From the line or the nearest heading above: critical, important,
    /// minor, or none.
    pub severity: Option<&'static str>,
}

fn severity(s: &str) -> Option<&'static str> {
    let s = s.to_ascii_lowercase();
    for (k, v) in [
        ("critical", "critical"),
        ("blocker", "critical"),
        ("high", "critical"),
        ("important", "important"),
        ("major", "important"),
        ("warning", "important"),
        ("medium", "important"),
        ("minor", "minor"),
        ("nit", "minor"),
        ("suggestion", "minor"),
        ("low", "minor"),
    ] {
        if s.contains(k) {
            return Some(v);
        }
    }
    None
}

/// The findings of a message: references to real lines outside code
/// blocks, one per location.
pub fn findings(message: &str, ws: &Workspace, hints: &[String]) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for r in refs::find(message) {
        if r.in_code {
            continue;
        }
        let Verdict::Location { path, start, end } = refs::check(&r, ws, hints) else {
            continue;
        };
        if out.iter().any(|f| f.path == path && f.start == start) {
            continue;
        }
        let line_start = message[..r.span.0].rfind('\n').map_or(0, |i| i + 1);
        let line_end = message[r.span.1..]
            .find('\n')
            .map_or(message.len(), |i| r.span.1 + i);
        let text = message[line_start..line_end].trim().to_owned();
        let heading = message[..line_start]
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with('#'))
            .unwrap_or("");
        out.push(Finding {
            severity: severity(&text).or_else(|| severity(heading)),
            path,
            start,
            end,
            text,
        });
    }
    out
}
