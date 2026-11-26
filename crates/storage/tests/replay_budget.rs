// SPDX-License-Identifier: Apache-2.0

//! Acceptance check for #17: replaying 10 000 events must take under 100 ms.
//!
//! This is a test with a time budget rather than a criterion benchmark, so CI
//! enforces it on every run. The 100 ms budget applies to optimised builds
//! (`cargo test --release`). Unoptimised test builds also compile the bundled
//! SQLite at `-O0`, so there the budget is multiplied by [`DEBUG_MULTIPLIER`];
//! that still catches algorithmic regressions (a missing index, per-row
//! queries) without flaking on slow CI runners.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use coqo_storage::{DataDirs, EventFilter, GlobalDb, NewEvent, Seq};

const EVENTS: usize = 10_000;
const BUDGET: Duration = Duration::from_millis(100);
const DEBUG_MULTIPLIER: u32 = 5;

fn budget() -> Duration {
    if cfg!(debug_assertions) {
        BUDGET * DEBUG_MULTIPLIER
    } else {
        BUDGET
    }
}

/// A payload about the size of a typical protocol event (a chat delta or a
/// small buffer edit).
fn payload(i: usize) -> Vec<u8> {
    format!(
        r#"{{"session":"s-{}","delta":"{}","index":{i}}}"#,
        i % 7,
        "lorem ipsum dolor sit amet ".repeat(4)
    )
    .into_bytes()
}

#[test]
fn ten_thousand_events_replay_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = DataDirs::at(tmp.path());
    let db = GlobalDb::open(&dirs).unwrap();
    let log = db.events();

    let payloads: Vec<_> = (0..EVENTS).map(payload).collect();
    let workspaces = ["ws-a", "ws-b", "ws-c"];
    let batch: Vec<_> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| NewEvent {
            workspace: Some(workspaces[i % workspaces.len()]),
            kind: "session.delta",
            payload: p,
        })
        .collect();
    let append_start = Instant::now();
    for chunk in batch.chunks(1_000) {
        log.append_batch(chunk).unwrap();
    }
    let append_time = append_start.elapsed();

    // Replay from a fresh connection, as a reattaching UI's daemon handler would.
    let reader = GlobalDb::open(&dirs).unwrap();
    let start = Instant::now();
    let mut count = 0usize;
    let mut bytes = 0usize;
    let last = reader
        .events()
        .replay_since(Seq::ZERO, EventFilter::All, |e| {
            count += 1;
            bytes += e.payload.len();
            ControlFlow::Continue(())
        })
        .unwrap();
    let replay_time = start.elapsed();

    assert_eq!(count, EVENTS);
    assert_eq!(last, Seq(EVENTS as u64));
    assert!(bytes > 0);
    eprintln!(
        "appended {EVENTS} events in {append_time:?}, replayed in {replay_time:?} \
         (budget {:?})",
        budget()
    );
    assert!(
        replay_time < budget(),
        "replaying {EVENTS} events took {replay_time:?}, budget is {:?}",
        budget()
    );
}
