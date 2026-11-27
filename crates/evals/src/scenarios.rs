// SPDX-License-Identifier: Apache-2.0

//! The scenarios and their checks. Each runs one short prompt on a fresh
//! copy of a fixture project (`evals/fixtures/`), optionally with a change
//! applied (`evals/changes/`), and checks which Coqo tools the agent called.

use coqo_protocol::methods::agents::{ToolGroup, ToolUse};
use coqo_protocol::methods::walkthroughs::{StepAnchor, Walkthrough};

/// Lines of a file: path relative to the root, first and last line
/// (1-based, inclusive).
pub type Span = (String, u32, u32);

/// A file tab the simulated editor shows, with an optional selection.
#[derive(Debug, Clone, Copy)]
pub struct Tab {
    /// Path relative to the fixture root.
    pub path: &'static str,
    /// Selected lines, 1-based, inclusive.
    pub selection: Option<(u32, u32)>,
}

/// One scenario.
#[derive(Debug, Clone, Copy)]
pub struct Scenario {
    /// Short id (`--only`).
    pub name: &'static str,
    /// The fixture under `evals/fixtures/`.
    pub fixture: &'static str,
    /// A patch under `evals/changes/`: the fixture copy becomes a git
    /// repository with one commit, and the patch is applied on top,
    /// uncommitted (the change the agent is asked about).
    pub change: Option<&'static str>,
    /// The editor's tabs, the first active; empty for no editor context.
    pub tabs: &'static [Tab],
    /// What the user types.
    pub prompt: &'static str,
    /// The answer to any question the agent asks.
    pub answer: &'static str,
    /// What it expects, for the table.
    pub expects: &'static str,
    /// Whether the run passed.
    pub check: fn(&Outcome) -> bool,
}

/// What a scenario's agent did.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// The session's tool-use stats (`SessionInfo::tool_use`).
    pub tool_use: Vec<ToolUse>,
    /// Questions it asked: anchor path (relative) and first line (1-based).
    pub questions: Vec<(String, u32)>,
    /// Paths it revealed (relative).
    pub reveals: Vec<String>,
    /// Context tokens in use at the end (the last usage update).
    pub tokens: Option<u64>,
    /// Cost of the session so far, as the agent reports it.
    pub cost: Option<f64>,
    /// How the turn ended, or why it did not.
    pub ended: String,
    /// What the session's last published walkthrough covers (with a
    /// `change`).
    pub walkthrough: Option<Coverage>,
    /// The change's hunks (with a `change`): `git diff -U0` after the turn.
    pub hunks: Vec<Hunk>,
}

/// A changed hunk: its lines in the new file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Path relative to the repository root.
    pub path: String,
    /// First line, 1-based. For a pure deletion, the line before the gap.
    pub start: u32,
    /// Number of lines (0 for a pure deletion).
    pub lines: u32,
}

impl std::fmt::Display for Hunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.lines {
            0 => write!(f, "{}:{} (deletion)", self.path, self.start),
            1 => write!(f, "{}:{}", self.path, self.start),
            n => write!(f, "{}:{}-{}", self.path, self.start, self.start + n - 1),
        }
    }
}

/// The hunks of a `git diff -U0` (new-file side).
pub fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut path: Option<String> = None;
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ ") {
            path = p.strip_prefix("b/").map(str::to_owned);
        } else if let (Some(rest), Some(p)) = (line.strip_prefix("@@ "), &path) {
            let Some(new) = rest.split(' ').find_map(|w| w.strip_prefix('+')) else {
                continue;
            };
            let (start, lines) = match new.split_once(',') {
                Some((s, n)) => (s.parse().unwrap_or(0), n.parse().unwrap_or(1)),
                None => (new.parse().unwrap_or(0), 1),
            };
            hunks.push(Hunk {
                path: p.clone(),
                start,
                lines,
            });
        }
    }
    hunks
}

/// The lines a whole-line step anchor covers: 1-based, inclusive.
fn anchor_lines(a: &StepAnchor) -> (u32, u32) {
    let r = a.anchor.range;
    let end = if r.end.col == 0 && r.end.line > r.start.line {
        r.end.line - 1
    } else {
        r.end.line
    };
    (r.start.line + 1, end + 1)
}

/// Whether lines `first..=last` touch `hunk` (a deletion: the lines on
/// either side of its gap).
fn touches(hunk: &Hunk, first: u32, last: u32) -> bool {
    let (start, end) = match hunk.lines {
        0 => (hunk.start, hunk.start + 1),
        n => (hunk.start, hunk.start + n - 1),
    };
    first <= end && start <= last
}

/// What a walkthrough covers of a change.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Its steps' anchors.
    pub anchors: Vec<Span>,
    /// Its trivial hunks: a path, and lines unless it is the whole file.
    pub trivial: Vec<(String, Option<(u32, u32)>)>,
    /// Its step count.
    pub steps: usize,
}

impl Coverage {
    /// What `w` covers.
    pub fn of(w: &Walkthrough) -> Self {
        Self {
            anchors: w
                .steps
                .iter()
                .flat_map(|s| &s.anchors)
                .map(|a| {
                    let (first, last) = anchor_lines(a);
                    (a.anchor.path.clone(), first, last)
                })
                .collect(),
            trivial: w
                .trivial_hunks
                .iter()
                .map(|t| (t.path.clone(), t.lines.map(|l| (l.start + 1, l.end + 1))))
                .collect(),
            steps: w.steps.len(),
        }
    }

    /// The hunks of `hunks` that no anchor touches and that are not listed
    /// as trivial: what the coverage guard (#99) would flag.
    pub fn uncovered<'a>(&self, hunks: &'a [Hunk]) -> Vec<&'a Hunk> {
        hunks
            .iter()
            .filter(|h| {
                let anchored = self
                    .anchors
                    .iter()
                    .any(|(path, first, last)| *path == h.path && touches(h, *first, *last));
                let trivial = self.trivial.iter().any(|(path, lines)| {
                    *path == h.path && lines.is_none_or(|(first, last)| touches(h, first, last))
                });
                !anchored && !trivial
            })
            .collect()
    }
}

impl Outcome {
    /// Calls of the `coqo-editor` tool `name`.
    pub fn calls(&self, name: &str) -> u64 {
        self.tool_use
            .iter()
            .filter(|u| u.name == name && matches!(u.group, ToolGroup::Coqo | ToolGroup::Mod))
            .map(|u| u.count)
            .sum()
    }

    /// All `coqo-editor` tool calls.
    pub fn coqo_calls(&self) -> u64 {
        self.tool_use
            .iter()
            .filter(|u| matches!(u.group, ToolGroup::Coqo | ToolGroup::Mod))
            .map(|u| u.count)
            .sum()
    }

    /// Calls of the agent's own tools.
    pub fn agent_calls(&self) -> u64 {
        self.tool_use
            .iter()
            .filter(|u| !matches!(u.group, ToolGroup::Coqo | ToolGroup::Mod))
            .map(|u| u.count)
            .sum()
    }

    /// Whether its last walkthrough covers every hunk of the change.
    pub fn covers_the_change(&self) -> bool {
        !self.hunks.is_empty()
            && self
                .walkthrough
                .as_ref()
                .is_some_and(|w| w.uncovered(&self.hunks).is_empty())
    }

    /// The Coqo tools it called, for the table (`set_status×2`).
    pub fn coqo_summary(&self) -> String {
        let parts: Vec<String> = self
            .tool_use
            .iter()
            .filter(|u| matches!(u.group, ToolGroup::Coqo | ToolGroup::Mod))
            .map(|u| {
                if u.count == 1 {
                    u.name.clone()
                } else {
                    format!("{}×{}", u.name, u.count)
                }
            })
            .collect();
        if parts.is_empty() {
            "-".into()
        } else {
            parts.join(", ")
        }
    }
}

const PRICING: &str = "shop/pricing.py";
const CONFIG: &str = "shop/config.py";

/// The scenarios, in the order they run.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "explain-selection",
        fixture: "shop",
        change: None,
        tabs: &[Tab {
            path: PRICING,
            selection: Some((6, 12)),
        }],
        prompt: "Explain the selected function in two or three sentences.",
        answer: "Just explain it.",
        expects: "editor_context",
        check: |o| o.calls("editor_context") >= 1,
    },
    Scenario {
        name: "ambiguous-default",
        fixture: "shop",
        change: None,
        tabs: &[
            Tab {
                path: CONFIG,
                selection: None,
            },
            Tab {
                path: PRICING,
                selection: None,
            },
        ],
        prompt: "I'm not sure what the free-shipping threshold should default to. \
                 Give it a sensible default in the config.",
        answer: "Use 50.0 EUR.",
        expects: "ask_question on config.py:6-10",
        check: |o| {
            o.calls("ask_question") >= 1
                && o.questions
                    .iter()
                    .any(|(path, line)| path.ends_with("config.py") && (6..=10).contains(line))
        },
    },
    Scenario {
        name: "three-steps",
        fixture: "shop",
        change: None,
        tabs: &[],
        prompt: "Add an optional `tax_rate` parameter (default 0) to `total()` in \
                 shop/pricing.py, add a test for it to test_pricing.py, and mention it \
                 in the README's usage section. Don't run anything.",
        answer: "Apply the tax after shipping.",
        expects: "set_status ≥2, set_acceptance_criteria",
        check: |o| o.calls("set_status") >= 2 && o.calls("set_acceptance_criteria") >= 1,
    },
    Scenario {
        name: "show-where",
        fixture: "shop",
        change: None,
        tabs: &[Tab {
            path: "README.md",
            selection: None,
        }],
        prompt: "Show me where refunds are handled.",
        answer: "Yes.",
        expects: "editor_reveal orders.py",
        check: |o| {
            o.calls("editor_reveal") >= 1 && o.reveals.iter().any(|p| p.ends_with("orders.py"))
        },
    },
    Scenario {
        name: "plain-question",
        fixture: "shop",
        change: None,
        tabs: &[],
        prompt: "In general, what is the difference between a list and a tuple in Python? \
                 Two sentences, no code.",
        answer: "Just answer.",
        expects: "no Coqo tools",
        check: |o| o.coqo_calls() == 0,
    },
    Scenario {
        name: "walkthrough",
        fixture: "shop",
        change: Some("shop-tax-rate.patch"),
        tabs: &[],
        prompt: "Walk me through this change.",
        answer: "Go ahead.",
        expects: "publish_walkthrough covering every hunk",
        check: |o| o.calls("publish_walkthrough") >= 1 && o.covers_the_change(),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, group: ToolGroup, count: u64) -> ToolUse {
        ToolUse {
            name: name.into(),
            group,
            count,
        }
    }

    fn scenario(name: &str) -> &'static Scenario {
        SCENARIOS.iter().find(|s| s.name == name).unwrap()
    }

    #[test]
    fn names_are_unique_and_fixtures_exist() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures");
        for (i, s) in SCENARIOS.iter().enumerate() {
            assert!(
                SCENARIOS[..i].iter().all(|o| o.name != s.name),
                "{}",
                s.name
            );
            for tab in s.tabs {
                let path = root.join(s.fixture).join(tab.path);
                assert!(path.is_file(), "{}", path.display());
            }
            if let Some(change) = s.change {
                let path = root.join("../changes").join(change);
                assert!(path.is_file(), "{}", path.display());
            }
        }
        // The anchored line the ambiguous scenario expects is the flag's.
        let config = std::fs::read_to_string(root.join("shop").join(CONFIG)).unwrap();
        let line = config
            .lines()
            .position(|l| l.starts_with("FREE_SHIPPING_THRESHOLD"))
            .unwrap();
        assert!((6..=10).contains(&(line as u32 + 1)));
    }

    #[test]
    fn checks_look_at_the_right_tools() {
        let mut o = Outcome {
            tool_use: vec![
                tool("editor_context", ToolGroup::Coqo, 1),
                tool("Read", ToolGroup::Agent, 3),
            ],
            ..Outcome::default()
        };
        assert!((scenario("explain-selection").check)(&o));
        assert!(!(scenario("plain-question").check)(&o));
        assert!(!(scenario("ambiguous-default").check)(&o));
        o.tool_use.push(tool("ask_question", ToolGroup::Coqo, 1));
        o.questions.push(("shop/pricing.py".into(), 16));
        assert!(!(scenario("ambiguous-default").check)(&o), "wrong place");
        o.questions.push(("shop/config.py".into(), 8));
        assert!((scenario("ambiguous-default").check)(&o));
        assert_eq!(o.coqo_summary(), "editor_context, ask_question");
        assert_eq!((o.coqo_calls(), o.agent_calls()), (2, 3));

        let plain = Outcome {
            tool_use: vec![tool("Read", ToolGroup::Agent, 1)],
            ..Outcome::default()
        };
        assert!((scenario("plain-question").check)(&plain));
        assert_eq!(plain.coqo_summary(), "-");
    }

    #[test]
    fn hunks_come_from_the_new_side_of_a_zero_context_diff() {
        let diff = "diff --git a/a.py b/a.py\n\
                    --- a/a.py\n\
                    +++ b/a.py\n\
                    @@ -3 +3 @@\n\
                    -x\n\
                    +y\n\
                    @@ -10,0 +11,3 @@ def f():\n\
                    @@ -20,2 +23,0 @@\n\
                    diff --git a/gone.py b/gone.py\n\
                    --- a/gone.py\n\
                    +++ /dev/null\n\
                    @@ -1,4 +0,0 @@\n";
        let hunk = |path: &str, start, lines| Hunk {
            path: path.into(),
            start,
            lines,
        };
        assert_eq!(
            parse_hunks(diff),
            [hunk("a.py", 3, 1), hunk("a.py", 11, 3), hunk("a.py", 23, 0)]
        );
        assert_eq!(hunk("a.py", 11, 3).to_string(), "a.py:11-13");
    }

    #[test]
    fn coverage_needs_an_anchor_or_a_trivial_entry_on_every_hunk() {
        let hunks = parse_hunks(
            "+++ b/p.py\n@@ -3 +3 @@\n@@ -22,2 +22,5 @@\n@@ -40,2 +45,0 @@\n\
             +++ b/t.py\n@@ -0,0 +1,2 @@\n",
        );
        let mut w = Coverage {
            anchors: vec![("p.py".into(), 20, 23), ("t.py".into(), 2, 2)],
            ..Coverage::default()
        };
        let missing: Vec<String> = w.uncovered(&hunks).iter().map(|h| h.to_string()).collect();
        assert_eq!(missing, ["p.py:3", "p.py:45 (deletion)"]);
        // An anchor on the line after a deletion's gap covers it.
        w.anchors.push(("p.py".into(), 46, 50));
        w.trivial.push(("p.py".into(), Some((3, 3))));
        assert!(w.uncovered(&hunks).is_empty());
        // A whole-file trivial entry covers all of that file.
        let whole = Coverage {
            trivial: vec![("p.py".into(), None), ("t.py".into(), None)],
            ..Coverage::default()
        };
        assert!(whole.uncovered(&hunks).is_empty());

        let mut o = Outcome {
            tool_use: vec![tool("publish_walkthrough", ToolGroup::Coqo, 1)],
            hunks: hunks.clone(),
            walkthrough: Some(whole),
            ..Outcome::default()
        };
        assert!((scenario("walkthrough").check)(&o));
        o.walkthrough = Some(Coverage::default());
        assert!(!(scenario("walkthrough").check)(&o));
        o.walkthrough = None;
        assert!(!(scenario("walkthrough").check)(&o));
    }
}
