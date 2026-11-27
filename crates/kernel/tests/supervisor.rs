// SPDX-License-Identifier: Apache-2.0

//! Supervisor tests against real processes (`sh`, `sleep`, `cat`).

use std::time::Duration;

use coqo_kernel::supervisor::resource_usage;
use coqo_kernel::{
    Backoff, ChildEvent, ChildHandle, ChildSpec, ChildState, ChildStdio, OutputStream, RestartMode,
    RestartPolicy, Supervisor,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::{Instant, sleep, timeout};

/// Upper bound for any single test, so a bug hangs nothing.
const LIMIT: Duration = Duration::from_secs(5);

fn sh(name: &str, script: &str) -> ChildSpec {
    ChildSpec::new(name, "sh").args(["-c", script])
}

/// Reads events until `stop` returns true for one; returns all of them.
async fn events_until(
    handle: &mut ChildHandle,
    mut stop: impl FnMut(&ChildEvent) -> bool,
) -> Vec<ChildEvent> {
    let mut events = Vec::new();
    loop {
        let event = handle.next_event().await.expect("event stream ended early");
        let done = stop(&event);
        events.push(event);
        if done {
            return events;
        }
    }
}

#[tokio::test]
async fn restarts_a_crashing_child_with_backoff_then_gives_up() {
    timeout(LIMIT, async {
        let supervisor = Supervisor::new();
        let policy = RestartPolicy {
            mode: RestartMode::OnFailure,
            backoff: Backoff {
                initial: Duration::from_millis(20),
                max: Duration::from_secs(1),
                multiplier: 2,
            },
            max_restarts: Some(3),
            reset_after: Duration::from_secs(60),
        };
        let started = Instant::now();
        let mut child = supervisor
            .spawn(sh("crasher", "exit 1").restart(policy))
            .unwrap();

        let events = events_until(&mut child, |e| matches!(e, ChildEvent::GaveUp { .. })).await;
        let elapsed = started.elapsed();

        let starts = events
            .iter()
            .filter(|e| matches!(e, ChildEvent::Started { .. }))
            .count();
        assert_eq!(starts, 4, "first run plus three restarts: {events:#?}");
        let exits: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ChildEvent::Exited { exit, .. } => Some(exit.code),
                _ => None,
            })
            .collect();
        assert_eq!(exits, [Some(1); 4]);
        let delays: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ChildEvent::RestartScheduled { attempt, delay } => Some((*attempt, *delay)),
                _ => None,
            })
            .collect();
        let ms = Duration::from_millis;
        assert_eq!(delays, [(1, ms(20)), (2, ms(40)), (3, ms(80))]);
        assert!(
            elapsed >= ms(140),
            "backoff was not waited out: {elapsed:?}"
        );
        assert_eq!(events.last(), Some(&ChildEvent::GaveUp { attempts: 3 }));

        let status = child.wait().await;
        assert_eq!(status.state, ChildState::Failed);
        assert_eq!(status.restarts, 3);
        assert_eq!(status.last_exit.and_then(|e| e.code), Some(1));
        // The stream ends once the child is done.
        assert_eq!(child.next_event().await, None);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn restart_always_restarts_clean_exits() {
    timeout(LIMIT, async {
        let supervisor = Supervisor::new();
        let policy = RestartPolicy {
            mode: RestartMode::Always,
            backoff: Backoff {
                initial: Duration::from_millis(5),
                max: Duration::from_millis(5),
                multiplier: 1,
            },
            max_restarts: Some(1),
            ..RestartPolicy::default()
        };
        let mut child = supervisor
            .spawn(sh("ok", "exit 0").restart(policy))
            .unwrap();
        let events = events_until(&mut child, |e| matches!(e, ChildEvent::GaveUp { .. })).await;
        let starts = events
            .iter()
            .filter(|e| matches!(e, ChildEvent::Started { .. }))
            .count();
        assert_eq!(starts, 2);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reports_rss_for_a_live_child_and_stops_it() {
    timeout(LIMIT, async {
        let supervisor = Supervisor::new();
        let mut child = supervisor
            .spawn(
                ChildSpec::new("sleeper", "sleep")
                    .arg("5")
                    .restart(RestartPolicy::never()),
            )
            .unwrap();
        let events = events_until(&mut child, |e| matches!(e, ChildEvent::Started { .. })).await;
        let Some(ChildEvent::Started { pid, instance: 0 }) = events.last().cloned() else {
            panic!("expected Started, got {events:?}");
        };
        assert_eq!(child.pid(), Some(pid));

        let usage = child.resource_usage().await.expect("live child has usage");
        assert!(usage.rss_bytes > 0, "{usage:?}");

        let snapshot = supervisor.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].name, "sleeper");
        assert!(snapshot[0].usage.is_some_and(|u| u.rss_bytes > 0));

        let stopping = Instant::now();
        let status = child.stop().await;
        assert_eq!(status.state, ChildState::Stopped);
        assert!(
            stopping.elapsed() < Duration::from_secs(2),
            "SIGTERM should stop sleep"
        );
        assert_eq!(status.last_exit.and_then(|e| e.signal), Some(15));
        assert!(resource_usage(pid).await.unwrap().is_none());

        // The runner removes the child once it's down.
        for _ in 0..100 {
            if supervisor.snapshot().await.is_empty() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("stopped child still listed");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn captures_stdout_and_stderr_lines() {
    timeout(LIMIT, async {
        let supervisor = Supervisor::new();
        let spec = sh("talker", "echo out; echo err >&2; printf 'no newline'")
            .restart(RestartPolicy::default());
        let mut child = supervisor.spawn(spec).unwrap();
        let events = events_until(&mut child, |e| matches!(e, ChildEvent::Exited { .. })).await;

        let mut lines: Vec<(OutputStream, String)> = events
            .iter()
            .filter_map(|e| match e {
                ChildEvent::Output(line) => Some((line.stream, line.line.clone())),
                _ => None,
            })
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            [
                (OutputStream::Stdout, "no newline".to_owned()),
                (OutputStream::Stdout, "out".to_owned()),
                (OutputStream::Stderr, "err".to_owned()),
            ]
        );

        // Exit 0 under OnFailure: no restart.
        let status = child.wait().await;
        assert!(matches!(status.state, ChildState::Exited(exit) if exit.success()));
        assert_eq!(status.restarts, 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn protocol_children_hand_over_stdin_and_stdout() {
    timeout(LIMIT, async {
        let supervisor = Supervisor::new();
        let mut child = supervisor
            .spawn(ChildSpec::new("echo", "cat").stdio(ChildStdio::Protocol))
            .unwrap();
        let mut io = child.next_io().await.expect("protocol child has io");
        assert_eq!(io.instance, 0);
        io.stdin.write_all(b"ping\n").await.unwrap();
        io.stdin.flush().await.unwrap();
        let mut reply = String::new();
        BufReader::new(&mut io.stdout)
            .read_line(&mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "ping\n");
        assert_eq!(child.stop().await.state, ChildState::Stopped);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn stop_kills_the_whole_process_group() {
    timeout(LIMIT, async {
        let supervisor = Supervisor::new();
        let mut child = supervisor
            .spawn(sh("parent", "sleep 30 & echo $!; wait").restart(RestartPolicy::never()))
            .unwrap();
        let events = events_until(&mut child, |e| matches!(e, ChildEvent::Output(_))).await;
        let Some(ChildEvent::Output(line)) = events.last() else {
            unreachable!()
        };
        let grandchild: u32 = line.line.parse().expect("pid of the background sleep");
        assert!(resource_usage(grandchild).await.unwrap().is_some());

        assert_eq!(child.stop().await.state, ChildState::Stopped);
        for _ in 0..200 {
            if !alive(grandchild).await {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("grandchild {grandchild} survived the stop");
    })
    .await
    .unwrap();
}

/// Whether `pid` exists and isn't a zombie.
async fn alive(pid: u32) -> bool {
    let out = tokio::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .await
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[tokio::test]
async fn shutdown_stops_everything_and_drop_stops_a_child() {
    timeout(LIMIT, async {
        let supervisor = Supervisor::new();
        let a = supervisor
            .spawn(ChildSpec::new("a", "sleep").arg("5"))
            .unwrap();
        let b = supervisor
            .spawn(ChildSpec::new("b", "sleep").arg("5"))
            .unwrap();
        assert_eq!(supervisor.snapshot().await.len(), 2);

        let watch_b = b.watch_status();
        drop(b);
        let mut watch_b = watch_b;
        let _ = watch_b.wait_for(|s| s.state.is_terminal()).await;
        assert_eq!(watch_b.borrow().state, ChildState::Stopped);

        supervisor.shutdown().await;
        assert_eq!(a.status().state, ChildState::Stopped);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn spawning_a_missing_program_fails() {
    let supervisor = Supervisor::new();
    let err = supervisor
        .spawn(ChildSpec::new("ghost", "/nonexistent/coqo-test-program"))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(supervisor.snapshot().await.is_empty());
}
