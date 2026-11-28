//! Terminal mode: the official, unmodified `claude` in a PTY with `--mcp-config`
//! pointing at the stub MCP server (PLAN §7.3, issue #57).
//!
//! The spike watches the stub's `ECHO_LOG` to prove that Claude Code connected
//! to the injected server (and, with `--prompt`, called its tool). It does not
//! parse the TUI. In coqod the PTY belongs to the terminal mod (#46).

use crate::{Args, which};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

pub fn run(raw: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::new(raw);
    let cwd = PathBuf::from(args.value("--cwd").ok_or("--cwd is required")?);
    let prompt = args.value("--prompt");
    let resume = args.value("--resume");
    let session_id = args.value("--session-id");

    let claude = which("claude").ok_or("`claude` not found on PATH")?;
    let me = std::env::current_exe()?;
    let echo_log = cwd.join("echo-pty.log");
    let _ = std::fs::remove_file(&echo_log);

    // The generated MCP config. In coqod this lives in the user-only runtime
    // directory (0600) and points at `coqod mcp --session <id>`.
    let mcp_config = cwd.join("coqo-mcp.json");
    let config = serde_json::json!({
        "mcpServers": {
            "coqo-echo": {
                "type": "stdio",
                "command": me,
                "args": ["mcp-echo"],
                "env": { "ECHO_LOG": echo_log }
            }
        }
    });
    std::fs::write(&mcp_config, serde_json::to_vec_pretty(&config)?)?;

    let mut cmd = CommandBuilder::new(&claude);
    cmd.cwd(&cwd);
    cmd.arg("--mcp-config");
    cmd.arg(&mcp_config);
    // Pre-allow only the stub's tool so the scripted prompt doesn't stop on a
    // permission dialog. Real terminal mode leaves permissions to the user.
    cmd.arg("--allowedTools");
    cmd.arg("mcp__coqo-echo__echo");
    if let Some(id) = resume {
        cmd.arg("--resume");
        cmd.arg(id);
    }
    if let Some(id) = session_id {
        cmd.arg("--session-id");
        cmd.arg(id);
    }

    let t0 = Instant::now();
    let t0_wall = now_ms();
    let log = |m: &str| println!("[{:>6} ms] {m}", t0.elapsed().as_millis());
    let pair = native_pty_system().openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    log(&format!("spawned {} in a PTY", claude.display()));

    // Collect the TUI output so we can detect the trust dialog and the prompt box.
    let screen = Arc::new(Mutex::new(String::new()));
    let mut reader = pair.master.try_clone_reader()?;
    let screen_w = screen.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            screen_w
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    let mut writer = pair.master.take_writer()?;

    let echo_events = || -> Vec<(u128, String)> {
        std::fs::read_to_string(&echo_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (ms, rest) = l.split_once(' ')?;
                Some((
                    ms.parse::<u128>().ok()?.saturating_sub(t0_wall),
                    rest.to_owned(),
                ))
            })
            .collect()
    };
    let wait_for = |pred: &dyn Fn() -> bool, secs: u64| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    };

    // First output from the TUI.
    wait_for(&|| !screen.lock().unwrap().is_empty(), 20);
    log("first PTY output");

    // A folder the user hasn't trusted yet shows a trust dialog first, and the
    // CLI starts MCP servers only after it. In Coqo the user answers it in the
    // terminal tab. The default option is "No, exit", so the spike moves down to
    // "Yes, I trust this folder" and confirms. (The TUI positions words with
    // cursor moves, so spaces are stripped before matching.)
    let trust_shown = || {
        let s: String = strip_ansi(&screen.lock().unwrap())
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        s.to_lowercase().contains("itrustthisfolder")
    };
    wait_for(&|| trust_shown() || !echo_events().is_empty(), 30);
    if trust_shown() {
        std::thread::sleep(Duration::from_millis(500));
        log("trust dialog shown: choosing \"Yes, I trust this folder\"");
        writer.write_all(b"\x1b[B")?;
        writer.flush()?;
        std::thread::sleep(Duration::from_millis(300));
        writer.write_all(b"\r")?;
        writer.flush()?;
    }

    if wait_for(
        &|| echo_events().iter().any(|(_, e)| e == "recv tools/list"),
        30,
    ) {
        for (ms, e) in echo_events() {
            println!("[{ms:>6} ms] echo-server: {e}");
        }
    } else {
        log("echo server never saw tools/list");
    }

    if let Some(prompt) = prompt {
        // Give the TUI a moment to draw its prompt box.
        std::thread::sleep(Duration::from_millis(1500));
        log(&format!("typing prompt {prompt:?}"));
        writer.write_all(prompt.as_bytes())?;
        writer.flush()?;
        std::thread::sleep(Duration::from_millis(300));
        writer.write_all(b"\r")?;
        writer.flush()?;
        if wait_for(
            &|| {
                echo_events()
                    .iter()
                    .any(|(_, e)| e.starts_with("call echo"))
            },
            90,
        ) {
            let (ms, e) = echo_events()
                .into_iter()
                .find(|(_, e)| e.starts_with("call echo"))
                .unwrap_or_default();
            println!("[{ms:>6} ms] echo-server: {e}");
        } else {
            log("echo tool was not called within 90 s");
        }
        std::thread::sleep(Duration::from_secs(5));
    }

    log("sending /exit");
    writer.write_all(b"/exit")?;
    writer.flush()?;
    std::thread::sleep(Duration::from_millis(300));
    writer.write_all(b"\r")?;
    writer.flush()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        log("claude still running: killing");
        child.kill()?;
    }
    log("claude exited");

    // The CLI prints a resume hint on exit; show the tail of the screen without
    // escape sequences so the session id can be picked up.
    let text = strip_ansi(&screen.lock().unwrap());
    if let Some(dump) = args.value("--dump") {
        std::fs::write(dump, &text)?;
    }
    if let Some(pos) = text.rfind("claude --resume") {
        println!(
            "resume hint: {}",
            text[pos..].lines().next().unwrap_or("").trim()
        );
    }
    Ok(())
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
