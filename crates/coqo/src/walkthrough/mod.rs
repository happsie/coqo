// SPDX-License-Identifier: Apache-2.0

//! The walkthrough player (PLAN §7.6, issue #100, ADR 0020, design board 3).
//!
//! An agent publishes a walkthrough after a change (`publish_walkthrough`,
//! #97): the change in reading order, each step anchored to code with what
//! it does, why, and its risks. The player replays it:
//!
//! - **The `walkthrough` tab** ([`player::WalkthroughPlayer`]): the header
//!   (title, scope, who wrote it, where it is kept, "3 of 8 understood"),
//!   the summary, the step list (number, title, kind, file, state), the
//!   acceptance criteria with the steps that meet them, and the current
//!   step's code: the file in an editor, or the walkthrough's diff
//!   ([`diff::WalkthroughDiffSource`]) in the review surface.
//! - **The step popover** ([`popover::StepPopover`]) at the step's lines:
//!   title and kind, what, why, the risks by severity, the criteria and the
//!   steps to read first, the anchor ("anchor 1 of 3", "code moved" with
//!   the quoted code when it is gone), and Previous / Next, **Understood**
//!   (`walkthroughs/setStepState`) and **Ask about this step**: the question
//!   goes to the agent that wrote the walkthrough as a thread pinned to the
//!   step's lines (`questions/ask`, #229), and the step is marked
//!   `question`.
//! - **Step markers:** a numbered badge on each step's lines in the editor
//!   ([`crate::files::editor::StepMark`]) and in the diff
//!   ([`diff::StepOverlay`]); a click goes to the step.
//!
//! [`OpenWalkthrough`] opens the player at the first unread step with its
//! popover shown: the chat's "Walkthrough published" card and the session
//! panel's "Walkthroughs" rows dispatch it. Progress persists in the
//! daemon and follows `walkthroughs/changes`, so another window's marks
//! show at once.
//!
//! Keys in the player: `n` / `→` next step, `p` / `←` previous, `u`
//! understood, `d` editor or diff. A text field (the code, the question)
//! gets its characters first.
//!
//! **In ordinary file tabs** (#260, [`marks`]): the steps of the
//! workspace's active walkthroughs (each session's newest one not fully
//! understood, and any open in a player) are numbered badges in the gutter
//! of every file tab of their files. A click opens the step popover there,
//! with **Open in player** and **Mark understood**. "Toggle walkthrough
//! markers" in the command palette hides or shows them.

pub mod coverage;
pub mod diff;
pub mod marks;
pub mod player;
pub mod popover;

use coqo_protocol::methods::{
    agents::CriterionStatus,
    questions::{AnchorSpan, AnchorStatus},
    walkthroughs::{
        RiskSeverity, ScopeKind, Step, StepAnchor, StepKind, StepState, Walkthrough, WalkthroughId,
    },
    workspace::WorkspaceId,
};
use coqo_ui::{
    ActiveTokens as _, Rgb,
    dock::{AnyTab, Dock, register_tab_kind},
    gpui::{Action, App, AppContext as _, Entity, Window},
};

use crate::workspace::Workspaces;

#[allow(unused_imports)]
use coqo_ui::gpui; // The action derives spell their paths `gpui::…`.

coqo_ui::gpui::actions!(
    walkthrough,
    [
        /// Go to the next step (n, →).
        NextStep,
        /// Go to the previous step (p, ←).
        PreviousStep,
        /// Mark the step understood, or unread again (u).
        ToggleUnderstood,
        /// Show the step in the editor or in the diff (d).
        ToggleDiff,
        /// Show or hide walkthrough step markers in file tabs.
        ToggleWalkthroughMarkers,
    ]
);

/// Open walkthrough `id` in its player, at `step` (by id; the first unread
/// step without one) with its popover shown.
#[derive(Clone, Debug, PartialEq, Eq, Action)]
#[action(namespace = walkthrough, no_json)]
pub struct OpenWalkthrough {
    pub workspace: String,
    pub id: String,
    pub step: Option<String>,
}

/// Where the player's keys apply.
pub const KEY_CONTEXT: &str = "WalkthroughPlayer";

/// Register the `walkthrough` tab kind and bind the player's keys.
pub fn init(cx: &mut App) {
    use crate::commands::Command;
    let command = |id: &'static str, title: &'static str, action: Box<dyn Action>| {
        Command::new_boxed(id, title, "Walkthrough", action).context(KEY_CONTEXT)
    };
    crate::commands::register(
        cx,
        [
            command("walkthrough.next_step", "Next step", Box::new(NextStep))
                .key("n")
                .key("right"),
            command(
                "walkthrough.previous_step",
                "Previous step",
                Box::new(PreviousStep),
            )
            .key("p")
            .key("left"),
            command(
                "walkthrough.toggle_understood",
                "Mark step understood",
                Box::new(ToggleUnderstood),
            )
            .key("u"),
            command(
                "walkthrough.toggle_diff",
                "Show the step in the editor or the diff",
                Box::new(ToggleDiff),
            )
            .key("d"),
            Command::new(
                "walkthrough.toggle_markers",
                "Toggle walkthrough markers",
                "Walkthrough",
                ToggleWalkthroughMarkers,
            ),
        ],
    );
    cx.on_action(|_: &ToggleWalkthroughMarkers, cx| marks::toggle_current(cx));
    popover::init(cx);
    register_tab_kind(cx, player::KIND, |state, window, cx| {
        player::restore(state, window, cx)
    });
}

/// Open (or show) walkthrough `id`'s player in `dock` and go to `step` (by
/// id; the first unread step without one), with its popover shown and the
/// keyboard in it.
pub fn open(
    dock: &Entity<Dock>,
    workspace: WorkspaceId,
    id: WalkthroughId,
    step: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<player::WalkthroughPlayer>> {
    let key = player::key(&id);
    let open = dock.read(cx).find_tab(player::KIND, &key, cx);
    let tab = match open {
        Some(tab) => {
            dock.update(cx, |dock, cx| dock.activate_tab(tab, window, cx));
            dock.read(cx)
                .tab(tab)
                .and_then(AnyTab::downcast::<player::WalkthroughPlayer>)
        }
        None => {
            let tab = cx.new(|cx| player::WalkthroughPlayer::new(workspace, id, None, window, cx));
            dock.update(cx, |dock, cx| {
                dock.open_tab(AnyTab::new(tab.clone()), window, cx)
            });
            Some(tab)
        }
    };
    if let Some(tab) = &tab {
        tab.update(cx, |tab, cx| tab.start_at(step, window, cx));
    }
    tab
}

/// Handle [`OpenWalkthrough`] in the window's dock.
pub fn open_action(
    dock: &Entity<Dock>,
    action: &OpenWalkthrough,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = if action.workspace.is_empty() {
        Workspaces::for_window(window, cx)
            .or_else(|| Workspaces::global(cx))
            .and_then(|w| w.read(cx).current().map(|w| w.id.clone()))
    } else {
        Some(WorkspaceId(action.workspace.clone()))
    };
    if let Some(workspace) = workspace {
        open(
            dock,
            workspace,
            WalkthroughId(action.id.clone()),
            action.step.clone(),
            window,
            cx,
        );
    }
}

// ---- helpers ----------------------------------------------------------------

/// Who wrote `w`: the provider's name ("Claude"), from the workspace's
/// `questions` store when it knows the session.
pub fn agent_name(
    w: &Walkthrough,
    questions: Option<&Entity<crate::questions::WorkspaceQuestions>>,
    cx: &App,
) -> String {
    // "Claude · Volume discount": the provider's name.
    let asker = questions
        .map(|store| store.read(cx).asker(&w.created_by.session, cx))
        .and_then(|a| a.split(" · ").next().map(str::to_owned))
        .filter(|a| a != "Agent");
    asker.unwrap_or_else(|| match w.created_by.agent.as_str() {
        "claude-code" | "claude" => "Claude".to_owned(),
        _ => "the agent".to_owned(),
    })
}

/// The reader's state of `step`.
pub fn step_state(w: &Walkthrough, step: &Step) -> StepState {
    w.step_states.get(&step.id).copied().unwrap_or_default()
}

/// How many steps are understood.
pub fn understood_count(w: &Walkthrough) -> usize {
    w.steps
        .iter()
        .filter(|s| step_state(w, s) == StepState::Understood)
        .count()
}

/// Where a reader starts: the first step not understood (or with a
/// question), else the first.
pub fn first_unread(w: &Walkthrough) -> usize {
    w.steps
        .iter()
        .position(|s| step_state(w, s) == StepState::Unread)
        .unwrap_or(0)
}

/// "Entry point", "Data model", ….
pub fn kind_label(kind: StepKind) -> &'static str {
    match kind {
        StepKind::EntryPoint => "Entry point",
        StepKind::Flow => "Flow",
        StepKind::DataModel => "Data model",
        StepKind::Logic => "Logic",
        StepKind::EdgeCase => "Edge case",
        StepKind::Test => "Test",
        StepKind::Config => "Config",
        StepKind::Migration => "Migration",
    }
}

/// "HIGH", "MEDIUM", "LOW" (board 3's badges).
pub fn severity_label(s: RiskSeverity) -> &'static str {
    match s {
        RiskSeverity::High => "HIGH",
        RiskSeverity::Med => "MEDIUM",
        RiskSeverity::Low => "LOW",
    }
}

/// A risk's colour: rose for high, sand for medium, sage for low.
pub fn severity_color(s: RiskSeverity, cx: &App) -> Rgb {
    let p = &cx.tokens().palette;
    match s {
        RiskSeverity::High => p.rose,
        RiskSeverity::Med => p.sand,
        RiskSeverity::Low => p.sage,
    }
}

/// A criterion's status as text.
pub fn criterion_label(status: Option<CriterionStatus>) -> &'static str {
    match status {
        Some(CriterionStatus::Met) => "Met",
        Some(CriterionStatus::Failed) => "Failed",
        Some(CriterionStatus::Pending) => "Pending",
        Some(CriterionStatus::Unknown) | None => "Not checked",
    }
}

/// The range of an anchor now (as published, if orphaned).
pub fn anchor_span(a: &StepAnchor) -> AnchorSpan {
    a.resolution.and_then(|r| r.range).unwrap_or(a.anchor.range)
}

/// How well an anchor was found again (`Exact` before `walkthroughs/get`
/// resolved it).
pub fn anchor_status(a: &StepAnchor) -> AnchorStatus {
    a.resolution.map(|r| r.status).unwrap_or_default()
}

/// The lines of an anchor now (0-based, `end` exclusive).
pub fn anchor_lines(a: &StepAnchor) -> std::ops::Range<u32> {
    crate::questions::span_lines(&anchor_span(a))
}

/// "src/cart.rs:42", "…:42–48" (1-based) for an anchor as it is now.
pub fn anchor_location(a: &StepAnchor) -> String {
    let lines = anchor_lines(a);
    if lines.len() > 1 {
        format!("{}:{}–{}", a.anchor.path, lines.start + 1, lines.end)
    } else {
        format!("{}:{}", a.anchor.path, lines.start + 1)
    }
}

/// "42–48" or "42".
pub fn anchor_range_label(a: &StepAnchor) -> String {
    let lines = anchor_lines(a);
    if lines.len() > 1 {
        format!("{}–{}", lines.start + 1, lines.end)
    } else {
        format!("{}", lines.start + 1)
    }
}

/// The file name of a path.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A commit id shortened for the header.
fn short(rev: &str) -> &str {
    if rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        &rev[..7]
    } else {
        rev
    }
}

/// What the walkthrough compares: "working tree vs HEAD", "feature vs main".
pub fn scope_label(w: &Walkthrough) -> String {
    let s = &w.scope;
    let base = s
        .base
        .as_deref()
        .or(s.base_commit.as_deref())
        .map(short)
        .unwrap_or("HEAD");
    match s.kind {
        ScopeKind::Worktree => format!("working tree vs {base}"),
        ScopeKind::Commits | ScopeKind::Pr => {
            let head = s
                .head
                .as_deref()
                .or(s.head_commit.as_deref())
                .map(short)
                .unwrap_or("HEAD");
            format!("{head} vs {base}")
        }
    }
}

/// The files the steps are anchored in, in reading order.
pub fn files(w: &Walkthrough) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for a in w.steps.iter().flat_map(|s| &s.anchors) {
        if !files.contains(&a.anchor.path) {
            files.push(a.anchor.path.clone());
        }
    }
    files
}

/// "1 file", "5 files".
pub fn files_label(n: usize) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

/// The steps (1-based numbers) that meet criterion `id`.
pub fn criterion_steps(w: &Walkthrough, id: &str) -> Vec<usize> {
    w.steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.criteria_refs.iter().any(|r| r == id))
        .map(|(ix, _)| ix + 1)
        .collect()
}

/// "step 1", "steps 1, 3".
pub fn steps_list(steps: &[usize]) -> String {
    let list: Vec<String> = steps.iter().map(usize::to_string).collect();
    match steps.len() {
        0 => "no step".to_owned(),
        1 => format!("step {}", list[0]),
        _ => format!("steps {}", list.join(", ")),
    }
}

/// The prompt a question about a step sends to the agent that wrote the
/// walkthrough (the code is attached as the thread's anchor).
pub fn question_body(w: &Walkthrough, ix: usize, question: &str) -> String {
    let step = &w.steps[ix];
    format!(
        "{}\n\n(About step {} of {} of your walkthrough \"{}\": {}.)",
        question.trim(),
        ix + 1,
        w.steps.len(),
        w.title,
        step.title
    )
}

#[cfg(test)]
mod tests;
