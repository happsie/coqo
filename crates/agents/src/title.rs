// SPDX-License-Identifier: Apache-2.0

//! Session titles (#172): a short title derived from a session's first
//! prompt, and the cleanup every title gets (a provider's, a user's).
//!
//! The derived title is the prompt's first line of text with markdown marks
//! dropped and whitespace collapsed, cut at a word near [`DERIVED_MAX`]
//! characters. A prompt without text (only `@`-mentions) is titled after its
//! first mention. A provider's own title (ACP `session_info_update`) later
//! replaces a derived one; a user's title is never replaced.

use coqo_protocol::methods::agents::ContentBlock;

/// Longest derived title, in characters, before the `…`.
pub(crate) const DERIVED_MAX: usize = 50;
/// Longest title a provider or the user may set, in characters.
pub(crate) const MAX: usize = 120;

/// `text` on one line: whitespace collapsed, trimmed, and cut at
/// `max` characters (with `…`). Empty if it has nothing but whitespace.
pub(crate) fn clean(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&line, max, false)
}

/// `line` cut to at most `max` characters plus `…`; at a word boundary if
/// `at_word` and one falls in the second half.
fn cut(line: &str, max: usize, at_word: bool) -> String {
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let head: String = line.chars().take(max).collect();
    let end = if at_word {
        head.rfind(' ')
            .filter(|&i| head[..i].chars().count() >= max / 2)
            .unwrap_or(head.len())
    } else {
        head.len()
    };
    let mut out = head[..end]
        .trim_end_matches([' ', ',', ';', ':', '-', '.'])
        .to_owned();
    out.push('…');
    out
}

/// The first line of `text` with its markdown marks dropped: heading `#`s,
/// quote `>`s, a list bullet or number, emphasis and code marks.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut line = line.trim_start_matches(['#', '>']).trim_start();
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            line = rest;
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
    {
        line = rest;
    }
    let line = line.replace(['`', '*'], "").replace("__", "");
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then_some(line)
}

/// A title for a session whose first prompt is `content`, or `None` if it
/// has nothing to title it by.
pub(crate) fn derive(content: &[ContentBlock]) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .find_map(first_line);
    if let Some(line) = text {
        return Some(cut(&line, DERIVED_MAX, true));
    }
    content.iter().find_map(|b| match b {
        ContentBlock::ResourceLink { name, .. } if !name.trim().is_empty() => {
            Some(cut(&clean(name, MAX), DERIVED_MAX, true))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::text(t)]
    }

    #[test]
    fn derived_from_the_first_line_without_markdown() {
        assert_eq!(
            derive(&text("Fix flaky OrderControllerIT\n\nIt fails on CI.")).as_deref(),
            Some("Fix flaky OrderControllerIT")
        );
        assert_eq!(
            derive(&text("\n\n## Bump **Spring Boot** to `3.5`  \n")).as_deref(),
            Some("Bump Spring Boot to 3.5")
        );
        assert_eq!(
            derive(&text("- add a   volume discount")).as_deref(),
            Some("add a volume discount")
        );
        assert_eq!(
            derive(&text("1. Read the ticket")).as_deref(),
            Some("Read the ticket")
        );
        assert_eq!(derive(&text("/review")).as_deref(), Some("/review"));
    }

    #[test]
    fn long_prompts_are_cut_at_a_word() {
        let title = derive(&text(
            "Implement the volume discount per ACME-1423, using the existing PricingConfig",
        ))
        .unwrap();
        assert_eq!(title, "Implement the volume discount per ACME-1423…");
        assert!(title.chars().count() <= DERIVED_MAX + 1);
        // No space to cut at: cut mid-word.
        let long = "x".repeat(80);
        assert_eq!(
            derive(&text(&long)).unwrap(),
            format!("{}…", "x".repeat(DERIVED_MAX))
        );
    }

    #[test]
    fn a_prompt_without_text_is_titled_by_its_first_mention() {
        let content = vec![ContentBlock::ResourceLink {
            uri: "file:///w/src/lib.rs".into(),
            name: "lib.rs".into(),
            mime_type: None,
            size: None,
        }];
        assert_eq!(derive(&content).as_deref(), Some("lib.rs"));
        assert_eq!(derive(&text("  \n ")), None);
        assert_eq!(derive(&[]), None);
    }

    #[test]
    fn titles_are_cleaned_to_one_line() {
        assert_eq!(clean("  Volume\n discount ", MAX), "Volume discount");
        assert_eq!(clean(" \n", MAX), "");
        assert_eq!(clean(&"a".repeat(130), MAX).chars().count(), MAX + 1);
    }
}
