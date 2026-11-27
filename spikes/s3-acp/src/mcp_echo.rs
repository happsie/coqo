//! A tiny stdio MCP server with a single `echo` tool. Stands in for
//! `coqod mcp --session <id>` (PLAN §7.4). Newline-delimited JSON-RPC.
//!
//! If `ECHO_LOG` is set, every received method is appended to that file with a
//! wall-clock timestamp (ms since the Unix epoch), so the spike can prove that
//! the agent connected and called the tool, and measure when.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let log_path = std::env::var_os("ECHO_LOG");
    let log = |line: &str| {
        if let Some(path) = &log_path {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or_default();
                let _ = writeln!(f, "{ms} {line}");
            }
        }
    };
    log("started");
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(&line)?;
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg.get("id").cloned();
        log(&format!("recv {method}"));
        let result = match method {
            "initialize" => Some(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "coqo-echo", "version": "0.0.0" }
            })),
            "tools/list" => Some(json!({
                "tools": [{
                    "name": "echo",
                    "description": "Echo the given text back. Used by the Coqo S3 spike.",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }]
            })),
            "tools/call" => {
                let text = msg["params"]["arguments"]["text"].as_str().unwrap_or("");
                log(&format!("call echo text={text:?}"));
                Some(json!({ "content": [{ "type": "text", "text": format!("echo: {text}") }] }))
            }
            "ping" => Some(json!({})),
            _ => None,
        };
        // Notifications (no id) get no reply.
        if let Some(id) = id {
            let reply = match result {
                Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                None => json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                }),
            };
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    log("eof");
    Ok(())
}
