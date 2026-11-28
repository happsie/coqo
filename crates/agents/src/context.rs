// SPDX-License-Identifier: Apache-2.0

//! The context a session that starts from another one's transcript ("Start
//! a new chat with this context", forks) gives the agent with its first
//! prompt: that transcript as plain text, the prompts and replies in full
//! and tool calls by their titles, cut from the start when it is long.

use std::fmt::Write as _;

use coqo_protocol::methods::agents::{ContentBlock, SessionUpdate};

/// At most this many characters of transcript, the newest kept.
pub(crate) const MAX: usize = 60_000;

/// The text that goes before the first prompt of a session started from
/// `updates` (another session's transcript, oldest first), titled `title`;
/// `None` if it has no messages (a fork before the first prompt).
pub(crate) fn render(title: Option<&str>, updates: &[SessionUpdate]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut reply = String::new();
    let flush = |reply: &mut String, parts: &mut Vec<String>| {
        let text = reply.trim();
        if !text.is_empty() {
            parts.push(format!("Assistant: {text}"));
        }
        reply.clear();
    };
    for update in updates {
        match update {
            SessionUpdate::UserMessage { content } => {
                flush(&mut reply, &mut parts);
                parts.push(format!("User: {}", blocks(content)));
            }
            SessionUpdate::AgentMessageChunk { content } => {
                reply.push_str(&blocks(std::slice::from_ref(content)))
            }
            SessionUpdate::ToolCall(call) => {
                flush(&mut reply, &mut parts);
                parts.push(format!("[Tool call: {}]", call.title.trim()));
            }
            _ => {}
        }
    }
    flush(&mut reply, &mut parts);
    if parts.is_empty() {
        return None;
    }

    // Keep the newest parts that fit.
    let mut kept = Vec::new();
    let mut size = 0;
    for part in parts.iter().rev() {
        if size + part.len() > MAX {
            break;
        }
        size += part.len() + 2;
        kept.push(part.as_str());
    }
    kept.reverse();
    let omitted = parts.len() - kept.len();

    let mut out = String::from("This chat continues an earlier one");
    if let Some(title) = title {
        let _ = write!(out, " (\"{title}\")");
    }
    out.push_str(
        ". Its transcript follows, for context only: carry on from where it ends, and answer \
         the message after it.\n\n<earlier-chat>\n",
    );
    if omitted > 0 {
        let _ = writeln!(out, "[{omitted} earlier messages left out]\n");
    }
    out.push_str(&kept.join("\n\n"));
    out.push_str("\n</earlier-chat>");
    Some(out)
}

/// Content blocks as text: mentions and attachments by name.
fn blocks(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::ResourceLink { uri, name, .. } => {
                let _ = write!(out, "@{name} ({uri})");
            }
            ContentBlock::Resource { uri, text, .. } => {
                let _ = write!(out, "\n[{uri}]\n{text}\n");
            }
            ContentBlock::Image { .. } => out.push_str("[image]"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_protocol::methods::agents::{ToolCall, ToolCallId, ToolCallStatus, ToolKind};

    fn user(text: &str) -> SessionUpdate {
        SessionUpdate::UserMessage {
            content: vec![ContentBlock::text(text)],
        }
    }

    fn said(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text(text),
        }
    }

    #[test]
    fn the_transcript_becomes_text() {
        let call = ToolCall {
            id: ToolCallId("t1".into()),
            title: "Run cargo test".into(),
            name: None,
            kind: ToolKind::Execute,
            status: ToolCallStatus::Completed,
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: None,
        };
        let text = render(
            Some("Flaky test"),
            &[
                user("Fix the flaky test"),
                said("Looking"),
                said(" at it."),
                SessionUpdate::ToolCall(call),
                said("Fixed."),
                SessionUpdate::TurnStarted { turn: 1 },
            ],
        )
        .unwrap();
        assert!(text.starts_with("This chat continues an earlier one (\"Flaky test\")."));
        assert!(text.contains(
            "<earlier-chat>\nUser: Fix the flaky test\n\nAssistant: Looking at it.\n\n\
             [Tool call: Run cargo test]\n\nAssistant: Fixed.\n</earlier-chat>"
        ));
    }

    #[test]
    fn a_long_transcript_keeps_its_end() {
        let long = "x".repeat(MAX / 3);
        let updates: Vec<_> = (0..6).map(|i| user(&format!("{i}{long}"))).collect();
        let text = render(None, &updates).unwrap();
        assert!(text.len() < MAX + 500);
        assert!(text.contains("[4 earlier messages left out]"));
        assert!(text.contains("User: 5x"));
        assert!(!text.contains("User: 3x"));
    }

    #[test]
    fn no_messages_no_context() {
        assert_eq!(
            render(Some("x"), &[SessionUpdate::TurnStarted { turn: 1 }]),
            None
        );
    }
}
