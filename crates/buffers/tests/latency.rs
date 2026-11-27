// SPDX-License-Identifier: Apache-2.0

//! Issue #40 acceptance: a local keystroke applies in under 1 ms on a
//! 100k-line file. ADR 0006 measured 0.4 µs in release; this also runs in
//! debug builds, so it checks the requirement, not the measurement.

use std::time::{Duration, Instant};

use coqo_buffers::{ClientReplica, ServerBuffer, UndoGrouping, text};
use coqo_protocol::methods::buffer::{BufferId, OpenResult, ReplicaId, TextEdit};

#[test]
fn keystroke_under_one_millisecond_on_100k_lines() {
    let file: String = (0..100_000)
        .map(|i| format!("    let value_{i} = compute({i}, \"é\");\n"))
        .collect();
    let daemon = ServerBuffer::new(text::decode(file.into_bytes()).unwrap());
    let open = OpenResult {
        buffer: BufferId(1),
        replica: ReplicaId("r-1".into()),
        rev: daemon.rev(),
        text: daemon.text().to_string(),
        line_ending: daemon.line_ending(),
        encoding: daemon.encoding(),
        saved_rev: 0,
        dirty: false,
        last_seq: 0,
    };
    let mut replica = ClientReplica::new(&open);
    let mut at = replica.text().len() / 2;
    while !replica.text().is_char_boundary(at) {
        at += 1;
    }
    let mut times = Vec::with_capacity(2000);
    for i in 0..2000 {
        let edit = TextEdit::insert(at, "x");
        let grouping = if i % 5 == 0 {
            UndoGrouping::NewGroup
        } else {
            UndoGrouping::Extend
        };
        let start = Instant::now();
        // The first keystroke is sent; the rest are composed behind it.
        replica.local_edit(&[edit], grouping).unwrap();
        times.push(start.elapsed());
        at += 1;
    }
    times.sort();
    let p50 = times[times.len() / 2];
    let p99 = times[times.len() * 99 / 100];
    eprintln!("local apply on 100k lines: p50 {p50:?}, p99 {p99:?}");
    assert!(p50 < Duration::from_millis(1), "p50 {p50:?}");
}
