// SPDX-License-Identifier: Apache-2.0

//! A walkthrough's coverage as the reader sees it (#99, board 3's coverage
//! guard), shared by the chat's card, the session panel's row and the
//! player's banner (#264): the counts, the banner's sentence, where an
//! uncovered hunk opens, and what "Ask <agent> to explain it" sends.

use coqo_protocol::methods::walkthroughs::{
    UncoveredHunk, WalkthroughCoverage, WalkthroughId, WalkthroughLocation, WholeFile,
};
use coqo_ui::gpui::{App, Window};

use crate::chat::OpenLocation;
use crate::workspace::Workspaces;

/// "Coverage 12/14 hunks", for the card and the panel row.
pub(crate) fn coverage_label(c: &WalkthroughCoverage) -> String {
    if c.unavailable.is_some() {
        "Coverage not checked".into()
    } else if c.total == 0 {
        "No changed hunks".into()
    } else {
        format!(
            "Coverage {}/{} hunk{}",
            c.covered + c.trivial,
            c.total,
            if c.total == 1 { "" } else { "s" }
        )
    }
}

/// Whether to flag `c`: some hunks are uncovered.
pub(crate) fn incomplete(c: &WalkthroughCoverage) -> bool {
    c.unavailable.is_none() && c.uncovered_count() > 0
}

/// The banner's sentence, as board 3 has it: the counts and what is not
/// explained.
pub(crate) fn banner_label(c: &WalkthroughCoverage) -> (String, String) {
    let head = format!(
        "{} of {} changed hunks covered.",
        c.covered + c.trivial,
        c.total
    );
    let n = c.uncovered_count();
    let body = match (n, c.uncovered.first()) {
        (1, Some(h)) => format!(
            "{} ({}) is not explained by any step and wasn't marked trivial.",
            h.location(),
            h.change()
        ),
        _ => format!("{n} hunks are not explained by any step and weren't marked trivial."),
    };
    (head, body)
}

/// What "Ask the agent to explain it" sends about walkthrough `id`.
pub(crate) fn explain_prompt(title: &str, id: &WalkthroughId, c: &WalkthroughCoverage) -> String {
    let n = c.uncovered_count();
    let mut text = format!(
        "Your walkthrough \"{title}\" ({id}) doesn't cover {n} changed hunk{}:",
        if n == 1 { "" } else { "s" }
    );
    for h in &c.uncovered {
        text.push_str(&format!("\n- {} ({})", h.location(), h.change()));
    }
    if (n as usize) > c.uncovered.len() {
        text.push_str(&format!(
            "\n- … and {} more",
            n as usize - c.uncovered.len()
        ));
    }
    text.push_str(
        "\nExplain each: add it to a step's anchors, or to trivial_hunks with a reason if it needs no explanation, and publish the walkthrough again.",
    );
    text
}

/// Where to open an uncovered hunk in a file; a deleted file has nowhere
/// (only the diff shows it).
pub(crate) fn hunk_location(h: &UncoveredHunk) -> Option<WalkthroughLocation> {
    (h.whole_file != Some(WholeFile::Deleted)).then(|| WalkthroughLocation {
        root: h.root,
        path: h.path.clone(),
        lines: h.new_range,
    })
}

/// Open `at` (root-relative) in a file tab of the window's workspace.
pub(crate) fn open_location(at: &WalkthroughLocation, window: &mut Window, cx: &mut App) {
    let workspaces = Workspaces::for_window(window, cx).or_else(|| Workspaces::global(cx));
    let Some(root) = workspaces.as_ref().and_then(|w| {
        w.read(cx)
            .current()?
            .roots
            .iter()
            .find(|r| r.id == at.root)
            .map(|r| r.path.clone())
    }) else {
        return;
    };
    let action = OpenLocation {
        path: format!("{}/{}", root.trim_end_matches('/'), at.path),
        line: Some(at.lines.start),
        end: Some(at.lines.end),
    };
    window.dispatch_action(Box::new(action), cx);
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::editor::LineRange;
    use coqo_protocol::methods::workspace::RootId;

    use super::*;

    fn hunk(path: &str, start: u32, end: u32, added: u32, removed: u32) -> UncoveredHunk {
        UncoveredHunk {
            root: RootId(0),
            path: path.into(),
            new_range: LineRange { start, end },
            old_range: None,
            added,
            removed,
            whole_file: None,
            renamed_from: None,
            excerpt: String::new(),
        }
    }

    #[test]
    fn labels() {
        let mut c = WalkthroughCoverage {
            total: 15,
            covered: 12,
            trivial: 2,
            uncovered: vec![hunk("pom.xml", 87, 93, 7, 1)],
            unavailable: None,
        };
        assert_eq!(coverage_label(&c), "Coverage 14/15 hunks");
        assert!(incomplete(&c));
        assert_eq!(
            banner_label(&c),
            (
                "14 of 15 changed hunks covered.".to_owned(),
                "pom.xml:88-94 (+7 \u{2212}1) is not explained by any step and wasn't marked trivial."
                    .to_owned()
            )
        );
        c.covered = 11;
        c.uncovered.push(hunk("src/a.rs", 3, 3, 0, 2));
        assert_eq!(
            banner_label(&c).1,
            "2 hunks are not explained by any step and weren't marked trivial."
        );
        let prompt = explain_prompt("Volume discount", &WalkthroughId("vd-1".into()), &c);
        assert!(
            prompt.starts_with(
                "Your walkthrough \"Volume discount\" (vd-1) doesn't cover 2 changed hunks:\n- pom.xml:88-94"
            ),
            "{prompt}"
        );
        assert_eq!(
            coverage_label(&WalkthroughCoverage::default()),
            "No changed hunks"
        );
        let unchecked = WalkthroughCoverage::unavailable("no git");
        assert_eq!(coverage_label(&unchecked), "Coverage not checked");
        assert!(!incomplete(&unchecked));
        let mut gone = hunk("old.rs", 0, 0, 0, 4);
        assert!(hunk_location(&gone).is_some());
        gone.whole_file = Some(WholeFile::Deleted);
        assert!(hunk_location(&gone).is_none());
    }
}
