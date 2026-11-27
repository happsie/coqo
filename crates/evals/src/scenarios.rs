// SPDX-License-Identifier: Apache-2.0

//! The scenarios and their checks. Each runs one short prompt on a fresh
//! copy of a fixture project (`evals/fixtures/`) and checks which Coqo tools
//! the agent called.

use coqo_protocol::methods::agents::{ToolGroup, ToolUse};

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
        tabs: &[],
        prompt: "In general, what is the difference between a list and a tuple in Python? \
                 Two sentences, no code.",
        answer: "Just answer.",
        expects: "no Coqo tools",
        check: |o| o.coqo_calls() == 0,
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
}
