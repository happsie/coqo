// SPDX-License-Identifier: Apache-2.0

//! Real PTYs running small `sh` programs, directly and over the protocol.

use std::time::{Duration, Instant};

use coqo_protocol::methods::terminal::{
    self as proto, InputParams, ResizeParams, Screen, ScrollParams, ScrollTo, SpawnParams,
    TermSize, TerminalEvent, TerminalRef, TerminalStatus,
};
use coqo_protocol::{Client, ClientConfig, Server, ServerConfig};
use futures_util::StreamExt as _;

use super::*;

const TIMEOUT: Duration = Duration::from_secs(10);

/// `/bin/sh` with a fixed prompt and no rc files, so output is predictable.
fn sh() -> SpawnSpec {
    SpawnSpec::command("/bin/sh")
        .env("PS1", "$ ")
        .env("ENV", "")
        .size(TermSize::new(60, 10))
}

fn script(script: &str) -> SpawnSpec {
    sh().arg("-c").arg(script)
}

/// Poll `f` until it returns `Some`, or panic with `what` after [`TIMEOUT`].
fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < TIMEOUT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_exit(term: &Terminal) -> Option<i32> {
    eventually("the process to exit", || match term.status() {
        TerminalStatus::Exited { code } => Some(code),
        TerminalStatus::Running => None,
    })
}

#[test]
fn printf_output_lands_in_the_grid() {
    let terminals = Terminals::new();
    let term = terminals
        .spawn(script(
            r"printf 'hello \033[1;31mred\033[0m\nsecond line\n'",
        ))
        .unwrap();
    assert_eq!(wait_exit(&term), Some(0));
    let screen = term.screen();
    assert_eq!(screen.text(), "hello red\nsecond line");
    let red = &screen.rows[0].spans[1];
    assert_eq!(red.text, "red");
    assert_eq!(red.style.fg, proto::Color::Indexed(1));
    assert!(red.style.has(proto::flags::BOLD));
    assert_eq!(term.info().program, "/bin/sh");
}

#[test]
fn an_interactive_shell_echoes_input_and_reports_exit_codes() {
    let terminals = Terminals::new();
    let term = terminals.spawn(sh()).unwrap();
    term.write("echo hi-$((1+2))\n");
    eventually("the echo", || {
        term.screen()
            .text()
            .lines()
            .any(|l| l == "hi-3")
            .then_some(())
    });
    term.write("exit 7\n");
    assert_eq!(wait_exit(&term), Some(7));
    // An exited terminal keeps its output until it is closed.
    assert!(term.screen().text().contains("hi-3"));
    assert!(terminals.close(term.id()));
    assert!(!terminals.close(term.id()));
    assert!(terminals.list().is_empty());
}

#[test]
fn shell_integration_reports_cwd_and_command_results() {
    let terminals = Terminals::new();
    let term = terminals
        .spawn(script(
            r"printf '\033]7;file://host/tmp/My%%20Dir\007\033]133;D;4\007'; sleep 5",
        ))
        .unwrap();
    let info = eventually("the cwd report", || {
        let info = term.info();
        (info.cwd.as_deref() == Some("/tmp/My Dir")).then_some(info)
    });
    assert_eq!(info.last_command_exit, Some(4));
    assert_eq!(info.status, TerminalStatus::Running);
    terminals.close(term.id());
}

#[test]
fn scrollback_is_kept_in_the_daemon() {
    let terminals = Terminals::new();
    let term = terminals.spawn(script("seq 1 100")).unwrap();
    wait_exit(&term);
    let screen = term.screen();
    assert!(screen.text().ends_with("100"), "{}", screen.text());
    assert!(screen.history_size >= 90, "{}", screen.history_size);

    term.scroll(ScrollTo::Top);
    let top = term.screen();
    assert_eq!(top.text().lines().next(), Some("1"));
    assert_eq!(top.display_offset, top.history_size);
    term.scroll(ScrollTo::Lines(-3));
    assert_eq!(term.screen().display_offset, top.history_size - 3);
    term.scroll(ScrollTo::Bottom);
    assert_eq!(term.screen().display_offset, 0);
}

#[test]
fn resize_changes_the_grid_and_the_pty() {
    let terminals = Terminals::new();
    let term = terminals.spawn(sh()).unwrap();
    term.resize(TermSize::new(33, 7));
    assert_eq!(term.info().size, TermSize::new(33, 7));
    term.write("stty size\n");
    eventually("stty to see the new size", || {
        term.screen()
            .text()
            .lines()
            .any(|l| l == "7 33")
            .then_some(())
    });
    terminals.close(term.id());
}

#[cfg(unix)]
#[test]
fn closing_kills_the_process_group() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let terminals = Terminals::new();
    // A background child in the same process group, and one that ignores SIGHUP.
    let term = terminals
        .spawn(script(
            "trap '' HUP; sleep 1000 & echo $! > /dev/null; sleep 1000",
        ))
        .unwrap();
    let pid = Pid::from_raw(term.pid().unwrap() as i32);
    std::thread::sleep(Duration::from_millis(100));
    assert!(kill(pid, None).is_ok());
    assert!(terminals.close(term.id()));
    eventually("the process to die", || {
        matches!(term.status(), TerminalStatus::Exited { .. }).then_some(())
    });
    // Nothing is left in the group (killpg with no signal: ESRCH).
    eventually("the process group to be gone", || {
        nix::sys::signal::killpg(pid, None).is_err().then_some(())
    });
}

#[test]
fn shutdown_kills_everything() {
    let terminals = Terminals::new();
    let a = terminals.spawn(script("sleep 1000")).unwrap();
    let b = terminals.spawn(sh()).unwrap();
    terminals.shutdown();
    assert!(terminals.list().is_empty());
    wait_exit(&a);
    wait_exit(&b);
}

// ---- over the protocol -----------------------------------------------------

fn daemon() -> (Server, Terminals) {
    let terminals = Terminals::new();
    let router = register(coqo_protocol::Router::new(), terminals.clone());
    (
        Server::new(router, ServerConfig::new("coqod", "test")),
        terminals,
    )
}

async fn connect(server: &Server) -> Client {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let server = server.clone();
    tokio::spawn(async move { server.serve(server_io).await });
    let (client, _notifications) = Client::connect(client_io, ClientConfig::new("coqo-ui", "test"))
        .await
        .unwrap();
    client
}

/// Apply frames from `sub` to `screen` until `done` holds.
async fn follow(
    screen: &mut Screen,
    sub: &mut coqo_protocol::Subscription<proto::Output>,
    done: impl Fn(&Screen) -> bool,
) {
    tokio::time::timeout(TIMEOUT, async {
        while !done(screen) {
            let event = sub.next().await.expect("subscription ended").unwrap().event;
            if let TerminalEvent::Frame(frame) = event {
                screen.apply(&frame);
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; screen is:\n{}", screen.text()));
}

/// `top`'s header is on screen (procps and macOS spell it differently).
fn is_top(s: &Screen) -> bool {
    let text = s.text();
    text.contains("load average") || text.contains("Load Avg")
}

fn has_line(s: &Screen, line: &str) -> bool {
    s.text().lines().any(|l| l == line)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_round_trip_streams_frames_to_a_mirror() {
    let (server, terminals) = daemon();
    let client = connect(&server).await;

    let info = client
        .request::<proto::Spawn>(SpawnParams {
            program: Some("/bin/sh".into()),
            env: [("PS1".to_owned(), "$ ".to_owned())].into(),
            size: TermSize::new(40, 8),
            ..SpawnParams::default()
        })
        .await
        .unwrap();
    assert_eq!(info.size, TermSize::new(40, 8));
    assert_eq!(info.title, "sh");
    let id = info.id;

    let snap = client
        .request::<proto::Snapshot>(TerminalRef { id })
        .await
        .unwrap();
    let mut screen = snap.screen;
    let mut sub = client
        .subscribe::<proto::Output>(TerminalRef { id }, Some(snap.seq))
        .await
        .unwrap();

    client
        .request::<proto::Input>(InputParams {
            id,
            data: "echo round-$((20+1))\n".into(),
        })
        .await
        .unwrap();
    follow(&mut screen, &mut sub, |s| has_line(s, "round-21")).await;
    // The mirror matches the daemon's own screen.
    eventually("the mirror to settle", || {
        (terminals.get(id).unwrap().screen().text() == screen.text()).then_some(())
    });

    client
        .request::<proto::Resize>(ResizeParams {
            id,
            size: TermSize::new(50, 12),
        })
        .await
        .unwrap();
    follow(&mut screen, &mut sub, |s| s.size == TermSize::new(50, 12)).await;
    assert_eq!(screen.rows.len(), 12);

    client
        .request::<proto::Scroll>(ScrollParams {
            id,
            scroll: ScrollTo::Top,
        })
        .await
        .unwrap();

    assert!(
        client
            .request::<proto::Close>(TerminalRef { id })
            .await
            .unwrap()
    );
    // Closing ends the subscription.
    tokio::time::timeout(TIMEOUT, async { while sub.next().await.is_some() {} })
        .await
        .unwrap();
    assert!(client.request::<proto::List>(()).await.unwrap().is_empty());

    let err = client
        .request::<proto::Snapshot>(TerminalRef { id })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no terminal"), "{err}");
}

/// The acceptance check of #46, minus the UI: a client starts `top`, goes
/// away, and a new client finds it still live, scrollback and all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_outlives_its_client_and_reattaches() {
    let (server, terminals) = daemon();

    let first = connect(&server).await;
    let id = first
        .request::<proto::Spawn>(SpawnParams {
            program: Some("/bin/sh".into()),
            env: [("PS1".to_owned(), "$ ".to_owned())].into(),
            size: TermSize::new(80, 24),
            ..SpawnParams::default()
        })
        .await
        .unwrap()
        .id;
    first
        .request::<proto::Input>(InputParams {
            id,
            data: "top -d 1 || top\n".into(),
        })
        .await
        .unwrap();
    let started = Instant::now();
    loop {
        let s = terminals.get(id).unwrap().screen();
        if is_top(&s) {
            break;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "top didn't start:\n{}",
            s.text()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let second = connect(&server).await;
    let list = second.request::<proto::List>(()).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, TerminalStatus::Running);

    let snap = second
        .request::<proto::Snapshot>(TerminalRef { id })
        .await
        .unwrap();
    assert!(is_top(&snap.screen), "top is still showing");
    let mut screen = snap.screen;
    let mut sub = second
        .subscribe::<proto::Output>(TerminalRef { id }, Some(snap.seq))
        .await
        .unwrap();
    // Still live: top keeps redrawing (its clock ticks) and takes input.
    let before = screen.text();
    follow(&mut screen, &mut sub, |s| s.text() != before).await;
    for data in ["q", "echo after-$((1+1))\n"] {
        second
            .request::<proto::Input>(InputParams {
                id,
                data: data.into(),
            })
            .await
            .unwrap();
    }
    follow(&mut screen, &mut sub, |s| has_line(s, "after-2")).await;
    terminals.shutdown();
}

#[test]
fn running_terminals_keep_the_daemon_busy() {
    let activity = crate::Activity::default();
    let terminals = Terminals::new();
    terminals.lock().activity = Some(activity.clone());
    let term = terminals.spawn(script("read line")).unwrap();
    assert_eq!(
        activity.snapshot().busy,
        vec!["terminal 1 (/bin/sh)".to_owned()]
    );
    term.write("\n");
    wait_exit(&term);
    eventually("the busy guard to drop", || {
        activity.snapshot().busy.is_empty().then_some(())
    });
}
