// SPDX-License-Identifier: Apache-2.0

//! `walkthroughs/*`: code walkthroughs agents publish after a change (PLAN
//! §7.6, issue #97, ADR 0020).
//!
//! A walkthrough is the change in reading order: steps (entry point, flow,
//! data model, edge cases, tests, …), each anchored to code, with what it
//! does, why, and its risks, referring to the session's acceptance criteria
//! by id (ADR 0017). An agent writes a [`WalkthroughDraft`] (the format
//! `walkthrough.schema.json` describes, [`SCHEMA_JSON`]) and publishes it
//! with the `publish_walkthrough` MCP tool, which sends it as [`Create`].
//! The daemon checks it, pins each anchor to the code as it is now, resolves
//! the scope's git revisions, fills in the id, author and time, and stores
//! the [`Walkthrough`]. A draft with problems is refused with
//! [`INVALID_WALKTHROUGH`](super::codes::INVALID_WALKTHROUGH), whose `data`
//! ([`WalkthroughProblems`]) lists every problem with its JSON path.
//!
//! # Coverage
//!
//! Coqo computes the scope's diff itself (the git mod's, in the daemon) and
//! checks every changed hunk: covered by a step's anchor, listed in
//! `trivial_hunks`, or uncovered. The result is the walkthrough's
//! [`WalkthroughCoverage`], also in its summary, and the agent reads the
//! uncovered hunks in `publish_walkthrough`'s result. A published
//! walkthrough may leave hunks uncovered (they are flagged to the reader);
//! a draft with `require_coverage` is refused instead.
//!
//! # Storage
//!
//! Every walkthrough is kept in the daemon (`coqo.db`), private to the
//! user. A workspace can opt in to **committed** walkthroughs (workspace
//! state [`STORAGE_STATE_KEY`] = `"committed"`): each one is then also
//! written to `.coqo/walkthroughs/<id>.json` in its scope's repository for
//! the user to commit. Coqo never commits it.
//!
//! # For the UI
//!
//! [`List`] a workspace's walkthroughs, [`Get`] one with its anchors
//! resolved against the files as they are now (open buffers included), mark
//! steps read with [`SetStepState`] (the player's progress), [`Delete`] one,
//! and follow [`Changes`]. Each publication is also a
//! `SessionUpdate::Walkthrough` in the publishing session's transcript.
//!
//! **Access:** everything but [`Create`] takes the daemon token. A session
//! token may create walkthroughs for its own workspace and agent session
//! only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::agents::{CriterionStatus, SessionId};
use super::editor::LineRange;
use super::questions::{AnchorResolution, CodeAnchor};
use super::workspace::{RootId, WorkspaceId};
use crate::method::{Method, Topic};

/// The JSON Schema of a [`WalkthroughDraft`]: what `publish_walkthrough`
/// takes, and the MCP resource [`SCHEMA_URI`]. Generated from these types
/// (`COQO_BLESS=1 cargo test -p coqo-protocol schema`).
pub const SCHEMA_JSON: &str = include_str!("../../../../protocol/schema/walkthrough.schema.json");
/// Where agents read [`SCHEMA_JSON`] (an MCP resource of `coqo-editor`).
pub const SCHEMA_URI: &str = "coqo://schemas/walkthrough.json";
/// The workspace state key of the storage mode: `"local"` (the default) or
/// `"committed"`.
pub const STORAGE_STATE_KEY: &str = "walkthroughs.storage";
/// Where committed walkthroughs go, relative to the repository root.
pub const COMMITTED_DIR: &str = ".coqo/walkthroughs";

/// Most steps in one walkthrough.
pub const MAX_STEPS: usize = 100;
/// Most anchors one step can have.
pub const MAX_STEP_ANCHORS: usize = 16;
/// Most risks one step can name.
pub const MAX_RISKS: usize = 16;
/// Most trivial hunks one walkthrough can list.
pub const MAX_TRIVIAL_HUNKS: usize = 500;
/// Longest title, in characters.
pub const MAX_TITLE: usize = 200;
/// Longest markdown field (`summary`, `what`, `why`), in bytes.
pub const MAX_TEXT: usize = 16 * 1024;
/// Longest risk or reason, in characters.
pub const MAX_NOTE: usize = 2000;
/// Longest step id, in characters.
pub const MAX_STEP_ID: usize = 32;
/// Most uncovered hunks a walkthrough lists (the counts include them all).
pub const MAX_UNCOVERED: usize = 100;
/// Most uncovered hunks a [`WalkthroughSummary`] lists.
pub const SUMMARY_UNCOVERED: usize = 10;

/// Identifies a walkthrough: a slug of its title and a random suffix, e.g.
/// `volume-discount-3f2a9c1b`. Assigned by the daemon; also the committed
/// file's name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct WalkthroughId(pub String);

impl std::fmt::Display for WalkthroughId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a walkthrough covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// The uncommitted changes in the working tree, against `base` (default
    /// `HEAD`). No `head`: the working tree is the head.
    #[default]
    Worktree,
    /// The commits from `base` (exclusive) to `head`.
    Commits,
    /// A pull request: its base branch `base` and head branch `head`.
    Pr,
}

/// What a step is about, so the reader knows where they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Where the change is entered: a route, a command, a public function.
    EntryPoint,
    /// How control or data moves from there.
    Flow,
    /// Types, schemas, structs the change adds or alters.
    DataModel,
    /// The core logic: rules, calculations, decisions.
    Logic,
    /// Limits, errors, empty and concurrent cases.
    EdgeCase,
    /// Tests that show the change works.
    Test,
    /// Configuration, flags, build or dependency changes.
    Config,
    /// Data or schema migrations.
    Migration,
}

impl StepKind {
    /// Every kind, in reading order.
    pub const ALL: [Self; 8] = [
        Self::EntryPoint,
        Self::Flow,
        Self::DataModel,
        Self::Logic,
        Self::EdgeCase,
        Self::Test,
        Self::Config,
        Self::Migration,
    ];

    /// Its name on the wire: `entry_point`, `flow`, ….
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EntryPoint => "entry_point",
            Self::Flow => "flow",
            Self::DataModel => "data_model",
            Self::Logic => "logic",
            Self::EdgeCase => "edge_case",
            Self::Test => "test",
            Self::Config => "config",
            Self::Migration => "migration",
        }
    }
}

/// How bad it would be if a risk came true.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RiskSeverity {
    /// Unlikely, or cheap to fix.
    Low,
    /// Worth a reviewer's attention.
    Med,
    /// Could break users or data; review closely.
    High,
}

impl RiskSeverity {
    /// Every severity, lowest first.
    pub const ALL: [Self; 3] = [Self::Low, Self::Med, Self::High];

    /// Its name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Med => "med",
            Self::High => "high",
        }
    }
}

/// Something that could go wrong with a step's code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Risk {
    /// How bad.
    pub severity: RiskSeverity,
    /// What could go wrong, and what would show it.
    #[cfg_attr(test, schemars(length(min = 1, max = 2000)))]
    pub text: String,
}

/// Where the reader is with one step (the player's progress).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Not read yet.
    #[default]
    Unread,
    /// Read and understood.
    Understood,
    /// The reader has a question about it.
    Question,
}

/// Where a walkthrough is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    /// In the daemon only, private to the user.
    #[default]
    Local,
    /// Also written to `.coqo/walkthroughs/<id>.json` in the repository,
    /// for the user to commit.
    Committed,
}

// ---- the draft: what agents publish (`walkthrough.schema.json`) -------------------

/// A code walkthrough as an agent publishes it with `publish_walkthrough`:
/// the change in the order a reader should follow it (entry point → flow →
/// data → logic → edge cases → tests), every step anchored to code. Coqo
/// fills in the id, the author and the time. Lines are 1-based.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[cfg_attr(test, schemars(title = "Coqo walkthrough"))]
#[serde(deny_unknown_fields)]
pub struct WalkthroughDraft {
    /// What changed, in a few words ("Volume discount for bulk orders").
    #[cfg_attr(test, schemars(length(min = 1, max = 200)))]
    pub title: String,
    /// The change in two to five sentences (markdown): what it does and
    /// why.
    #[cfg_attr(test, schemars(length(min = 1)))]
    pub summary: String,
    /// What the walkthrough covers.
    pub scope: ScopeDraft,
    /// The acceptance criteria the change addresses, by id. Default: every
    /// criterion of this session (`set_acceptance_criteria`). A criterion
    /// that is not the session's needs `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<Vec<CriterionDraft>>,
    /// The steps, in reading order: 1 to 100.
    #[cfg_attr(test, schemars(length(min = 1, max = 100)))]
    pub steps: Vec<StepDraft>,
    /// Changed hunks no step needs to explain (formatting, imports,
    /// renames), so the coverage check does not flag them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, schemars(length(max = 500)))]
    pub trivial_hunks: Vec<TrivialHunkDraft>,
    /// Refuse to publish while a changed hunk of the scope is neither in a
    /// step's anchors nor in `trivial_hunks` (each is listed). Default:
    /// publish, and list the uncovered hunks in the result and to the
    /// reader.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_coverage: bool,
}

/// What a walkthrough covers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ScopeDraft {
    /// `worktree` (uncommitted changes; the usual case right after a
    /// change), `commits` or `pr`.
    pub kind: ScopeKind,
    /// The git revision the change is against (a commit, branch or tag).
    /// Default for `worktree`: `HEAD`. Required for `commits` and `pr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The last revision of the change. Required for `commits` and `pr`;
    /// not allowed for `worktree`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The workspace root (repository) of the change, from
    /// `editor_context`. Default: the first root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootId>,
}

/// An acceptance criterion a walkthrough addresses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CriterionDraft {
    /// Its id: one of the session's (`AC1`) or a new one.
    pub id: String,
    /// What must hold. Default: the session criterion's text; required for
    /// a criterion that is not the session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One step of a walkthrough: code to read, and what, why and the risks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct StepDraft {
    /// Unique within the walkthrough (letters, digits, `.`, `_`, `-`; at
    /// most 32), for `depends_on`: `s1`, `discount-rule`.
    #[cfg_attr(test, schemars(regex(pattern = r"^[A-Za-z0-9._-]{1,32}$")))]
    pub id: String,
    /// The step in a few words.
    #[cfg_attr(test, schemars(length(min = 1, max = 200)))]
    pub title: String,
    /// What the step is about.
    pub kind: StepKind,
    /// The code the step is about: 1 to 16 line ranges.
    #[cfg_attr(test, schemars(length(min = 1, max = 16)))]
    pub anchors: Vec<AnchorDraft>,
    /// What this code does (markdown).
    #[cfg_attr(test, schemars(length(min = 1)))]
    pub what: String,
    /// Why it is done this way: the reasoning, the alternatives turned
    /// down.
    #[cfg_attr(test, schemars(length(min = 1)))]
    pub why: String,
    /// What could go wrong here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, schemars(length(max = 16)))]
    pub risks: Vec<Risk>,
    /// The acceptance criteria (ids) this step helps meet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria_refs: Vec<String>,
    /// Steps (ids) to read before this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// Lines of a file a step is about, as the file is now (open buffers with
/// their unsaved edits).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AnchorDraft {
    /// The file, relative to the root or absolute inside the workspace.
    pub path: String,
    /// Root id from `editor_context`. Default: the scope's root, or the root
    /// an absolute path is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootId>,
    /// First line, 1-based.
    #[cfg_attr(test, schemars(range(min = 1)))]
    pub start_line: u32,
    /// Last line, 1-based, inclusive. Default: `start_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(range(min = 1)))]
    pub end_line: Option<u32>,
    /// Code that must be in those lines (a line of it is enough;
    /// whitespace is ignored), so a stale line number is caught instead of
    /// pinning the wrong code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A changed hunk that needs no explanation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TrivialHunkDraft {
    /// The file, relative to the root or absolute inside the workspace. It
    /// may be deleted.
    pub path: String,
    /// Root id. Default: the scope's root, or the root an absolute path is
    /// in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootId>,
    /// First line of the hunk (1-based, in the new file). Default: the
    /// whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(range(min = 1)))]
    pub start_line: Option<u32>,
    /// Last line, inclusive. Default: `start_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(range(min = 1)))]
    pub end_line: Option<u32>,
    /// Why it is trivial ("formatting", "import order").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ---- the walkthrough as stored and served -----------------------------------------

/// What a stored walkthrough covers, with its revisions resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WalkthroughScope {
    /// Worktree, commits or a pull request.
    pub kind: ScopeKind,
    /// The workspace root (repository) of the change.
    pub root: RootId,
    /// The base revision as the agent named it (`HEAD`, `main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The head revision as named; absent for the working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The base commit when published (absent outside a git repository).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The head commit when published; absent for the working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
}

/// An acceptance criterion a walkthrough addresses, as it was when it was
/// published. Steps refer to it by `id`; the session's live criteria (ADR
/// 0017) say whether it holds now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughCriterion {
    /// Its id, e.g. `AC1`.
    pub id: String,
    /// What must hold.
    pub text: String,
    /// Its status in the session when published; absent for a criterion
    /// only the walkthrough names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CriterionStatus>,
}

/// One code location of a step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct StepAnchor {
    /// The code as published (ADR 0013).
    pub anchor: CodeAnchor,
    /// Where it is now: set by [`Get`], absent elsewhere (and in committed
    /// files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<AnchorResolution>,
}

/// One step of a stored walkthrough.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// Unique within the walkthrough.
    pub id: String,
    /// The step in a few words.
    pub title: String,
    /// What it is about.
    pub kind: StepKind,
    /// The code, at least one location.
    pub anchors: Vec<StepAnchor>,
    /// What the code does (markdown).
    pub what: String,
    /// Why it is done this way (markdown).
    pub why: String,
    /// What could go wrong.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<Risk>,
    /// Acceptance criteria (ids) it helps meet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria_refs: Vec<String>,
    /// Steps (ids) to read first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// A changed hunk the agent marked as needing no explanation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TrivialHunk {
    /// The root `path` is relative to.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// The hunk's lines in the new file, 0-based, `end` inclusive; absent
    /// for the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    /// Why it is trivial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Who published a walkthrough.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughAuthor {
    /// The agent session.
    pub session: SessionId,
    /// Its provider (`claude-code`), or `agent` if the session is unknown.
    pub agent: String,
}

/// Where a walkthrough is kept.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughStorage {
    /// Local, or committed too.
    pub mode: StorageMode,
    /// The committed file, relative to the scope's root
    /// (`.coqo/walkthroughs/<id>.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Why the committed file could not be written; the walkthrough is kept
    /// locally anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A published walkthrough.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Walkthrough {
    /// Id.
    pub id: WalkthroughId,
    /// The workspace.
    pub workspace: WorkspaceId,
    /// What changed, in a few words.
    pub title: String,
    /// The change in a few sentences (markdown).
    pub summary: String,
    /// What it covers.
    pub scope: WalkthroughScope,
    /// The acceptance criteria it addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<WalkthroughCriterion>,
    /// The steps, in reading order.
    pub steps: Vec<Step>,
    /// Hunks that need no explanation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trivial_hunks: Vec<TrivialHunk>,
    /// How much of the scope's diff the steps cover. Computed when
    /// published, and again by `get` for a worktree scope (the work tree
    /// changes); absent from walkthroughs published before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<WalkthroughCoverage>,
    /// Who published it.
    pub created_by: WalkthroughAuthor,
    /// Published, Unix milliseconds.
    pub created_at: i64,
    /// Where it is kept.
    #[serde(default)]
    pub storage: WalkthroughStorage,
    /// The reader's progress by step id; a step not listed is unread. Never
    /// written to committed files.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub step_states: BTreeMap<String, StepState>,
}

/// A file location, for opening a walkthrough where it starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughLocation {
    /// The root.
    pub root: RootId,
    /// `/`-separated path relative to the root.
    pub path: String,
    /// The anchored lines when published, 0-based, `end` inclusive.
    pub lines: LineRange,
}

/// A walkthrough in a list, the transcript and change events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WalkthroughSummary {
    /// Id.
    pub id: WalkthroughId,
    /// The workspace.
    pub workspace: WorkspaceId,
    /// The publishing session.
    pub session: SessionId,
    /// Title.
    pub title: String,
    /// Worktree, commits or a pull request.
    pub scope: ScopeKind,
    /// How many steps.
    pub steps: u32,
    /// How many are marked understood.
    #[serde(default)]
    pub understood: u32,
    /// How many are marked with a question.
    #[serde(default)]
    pub questions: u32,
    /// The first step's first anchor, where a reader starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<WalkthroughLocation>,
    /// Local or committed.
    pub storage: StorageMode,
    /// How much of the diff the steps cover, with at most
    /// [`SUMMARY_UNCOVERED`] of the uncovered hunks listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<WalkthroughCoverage>,
    /// Published, Unix milliseconds.
    pub created_at: i64,
}

// ---- coverage ---------------------------------------------------------------------

/// A changed file a hunk stands for as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WholeFile {
    /// The file is new (all its lines are one hunk).
    Added,
    /// The file is gone: only `trivial_hunks` can cover it.
    Deleted,
    /// Either side is binary (no lines): only `trivial_hunks` can cover it.
    Binary,
    /// Moved, contents unchanged; `renamedFrom` has the old path.
    Renamed,
}

/// A changed hunk no step covers and no trivial hunk lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct UncoveredHunk {
    /// The root.
    pub root: RootId,
    /// `/`-separated path relative to the root (the new path of a rename).
    pub path: String,
    /// Where it is in the new file, 0-based, `end` inclusive: its lines,
    /// or for lines only removed, the line before them (the first line
    /// when they were at the top). This is where to open it; `{0, 0}` for
    /// a deleted file.
    pub new_range: LineRange,
    /// The removed lines in the old file, 0-based, `end` inclusive; absent
    /// when lines were only added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_range: Option<LineRange>,
    /// Lines added.
    pub added: u32,
    /// Lines removed.
    pub removed: u32,
    /// Set when the hunk is a whole file (added, deleted, binary, renamed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whole_file: Option<WholeFile>,
    /// The old path of a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    /// The first changed lines, `+` added and `-` removed, a few at most.
    pub excerpt: String,
}

/// How much of a walkthrough scope's diff its steps cover.
///
/// Every changed hunk of the scope (`git diff` without context, so each run
/// of changed lines is one hunk; untracked files count for a worktree
/// scope) is **covered** when a step's anchor overlaps its lines in the new
/// file (lines only removed: an anchor on the line before or after them),
/// else **trivial** when `trivial_hunks` lists it (by the same overlap, or
/// the whole file), else **uncovered**.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WalkthroughCoverage {
    /// Changed hunks in the scope.
    pub total: u32,
    /// Hunks a step's anchor covers.
    pub covered: u32,
    /// Hunks `trivial_hunks` lists (and no step covers).
    pub trivial: u32,
    /// The uncovered hunks (`total - covered - trivial` of them), at most
    /// [`MAX_UNCOVERED`] (a summary: [`SUMMARY_UNCOVERED`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncovered: Vec<UncoveredHunk>,
    /// Why the diff could not be computed (not a git repository, git
    /// unavailable, no commits yet); the counts are 0 then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

impl WalkthroughCoverage {
    /// Coverage that could not be computed, and why.
    pub fn unavailable(why: impl Into<String>) -> Self {
        Self {
            unavailable: Some(why.into()),
            ..Self::default()
        }
    }

    /// How many hunks are uncovered (the list may be shorter).
    pub fn uncovered_count(&self) -> u32 {
        self.total.saturating_sub(self.covered + self.trivial)
    }

    /// Every hunk is covered or trivial (and the diff was computed).
    pub fn is_complete(&self) -> bool {
        self.unavailable.is_none() && self.uncovered_count() == 0
    }

    /// This, listing at most `n` uncovered hunks.
    pub fn truncated(&self, n: usize) -> Self {
        let mut c = self.clone();
        c.uncovered.truncate(n);
        c
    }
}

impl UncoveredHunk {
    /// `path:12-14` (1-based) as a reader would name it; a deleted, binary
    /// or renamed file is its path alone.
    pub fn location(&self) -> String {
        match self.whole_file {
            Some(WholeFile::Deleted | WholeFile::Binary | WholeFile::Renamed) => self.path.clone(),
            _ if self.added == 0 => format!("{}:{}", self.path, self.new_range.start + 1),
            _ if self.new_range.start == self.new_range.end => {
                format!("{}:{}", self.path, self.new_range.start + 1)
            }
            _ => format!(
                "{}:{}-{}",
                self.path,
                self.new_range.start + 1,
                self.new_range.end + 1
            ),
        }
    }

    /// What changed, in a few words: "+3 −1", "file deleted", "binary".
    pub fn change(&self) -> String {
        match self.whole_file {
            Some(WholeFile::Deleted) => "file deleted".into(),
            Some(WholeFile::Binary) => "binary file".into(),
            Some(WholeFile::Renamed) => match &self.renamed_from {
                Some(from) => format!("renamed from {from}"),
                None => "renamed".into(),
            },
            Some(WholeFile::Added) => format!("new file, {} lines", self.added),
            None => match (self.added, self.removed) {
                (a, 0) => format!("+{a}"),
                (0, r) => format!("\u{2212}{r} removed"),
                (a, r) => format!("+{a} \u{2212}{r}"),
            },
        }
    }
}

// ---- params and results ---------------------------------------------------------

/// Params of [`List`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ListParams {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Only this session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
}

/// Result of [`List`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughList {
    /// Newest first.
    pub walkthroughs: Vec<WalkthroughSummary>,
}

/// Params of [`Get`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughParams {
    /// The walkthrough.
    pub id: WalkthroughId,
}

/// Params of [`Delete`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DeleteParams {
    /// The walkthrough.
    pub id: WalkthroughId,
    /// Also delete its committed file, if it has one. Default: the file
    /// stays (it may be committed already; git is the user's).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete_file: bool,
}

/// Params of [`SetStepState`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SetStepStateParams {
    /// The walkthrough.
    pub id: WalkthroughId,
    /// The step's id.
    pub step: String,
    /// Where the reader is with it.
    pub state: StepState,
}

/// Params of [`Create`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CreateParams {
    /// The publishing session's workspace.
    pub workspace: WorkspaceId,
    /// The publishing agent session.
    pub session: String,
    /// The [`WalkthroughDraft`], as the agent wrote it. Checked by the
    /// daemon, which reports every problem at once, so it is any JSON here.
    #[cfg_attr(test, schemars(with = "WalkthroughDraft"))]
    pub walkthrough: Value,
}

/// One thing wrong with a draft.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughProblem {
    /// Where, as a JSON path into the draft: `steps[2].anchors[0].end_line`
    /// (empty for the whole draft).
    pub path: String,
    /// What is wrong and how to fix it.
    pub message: String,
}

impl std::fmt::Display for WalkthroughProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// The `data` of [`INVALID_WALKTHROUGH`](super::codes::INVALID_WALKTHROUGH).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughProblems {
    /// Every problem found, in document order.
    pub problems: Vec<WalkthroughProblem>,
}

/// Params of the [`Changes`] topic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// Only this workspace's walkthroughs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceId>,
}

/// What happened to a walkthrough.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// An agent published it.
    Published,
    /// A step's state changed.
    Updated,
    /// It was deleted.
    Deleted,
}

/// An event of [`Changes`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct WalkthroughChange {
    /// What happened.
    pub kind: ChangeKind,
    /// The walkthrough's id.
    pub id: WalkthroughId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// It as it is now; absent when deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<WalkthroughSummary>,
}

// ---- methods ---------------------------------------------------------------------

/// `walkthroughs/list`: a workspace's walkthroughs, newest first.
#[derive(Debug)]
pub enum List {}
impl Method for List {
    const NAME: &'static str = "walkthroughs/list";
    type Params = ListParams;
    type Result = WalkthroughList;
}

/// `walkthroughs/get`: one walkthrough, its anchors resolved against the
/// files as they are now. An unknown id is
/// [`UNKNOWN_WALKTHROUGH`](super::codes::UNKNOWN_WALKTHROUGH).
#[derive(Debug)]
pub enum Get {}
impl Method for Get {
    const NAME: &'static str = "walkthroughs/get";
    type Params = WalkthroughParams;
    type Result = Walkthrough;
}

/// `walkthroughs/delete`: forget a walkthrough. Result is true if it
/// existed.
#[derive(Debug)]
pub enum Delete {}
impl Method for Delete {
    const NAME: &'static str = "walkthroughs/delete";
    type Params = DeleteParams;
    type Result = bool;
}

/// `walkthroughs/setStepState`: mark a step unread, understood or with a
/// question.
#[derive(Debug)]
pub enum SetStepState {}
impl Method for SetStepState {
    const NAME: &'static str = "walkthroughs/setStepState";
    type Params = SetStepStateParams;
    type Result = WalkthroughSummary;
}

/// `walkthroughs/create`: an agent publishes a walkthrough (the bridge's
/// `publish_walkthrough`). A draft with problems is
/// [`INVALID_WALKTHROUGH`](super::codes::INVALID_WALKTHROUGH) with every
/// problem in `data`.
#[derive(Debug)]
pub enum Create {}
impl Method for Create {
    const NAME: &'static str = "walkthroughs/create";
    type Params = CreateParams;
    type Result = Walkthrough;
}

/// `walkthroughs/changes`: walkthroughs published, updated or deleted.
/// Live only: list again after (re)subscribing.
#[derive(Debug)]
pub enum Changes {}
impl Topic for Changes {
    const NAME: &'static str = "walkthroughs/changes";
    type Params = ChangesParams;
    type Event = WalkthroughChange;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn draft_json() -> Value {
        json!({
            "title": "Volume discount",
            "summary": "Orders of 100+ units get 10% off, capped at 500 EUR.",
            "scope": { "kind": "worktree" },
            "acceptance_criteria": [{ "id": "AC1" }],
            "steps": [{
                "id": "s1",
                "title": "The discount rule",
                "kind": "logic",
                "anchors": [{ "path": "src/discount.rs", "start_line": 3, "end_line": 9, "text": "fn discount" }],
                "what": "Computes the discount.",
                "why": "One place for the rule.",
                "risks": [{ "severity": "med", "text": "Rounding." }],
                "criteria_refs": ["AC1"],
            }, {
                "id": "s2",
                "title": "Tests",
                "kind": "test",
                "anchors": [{ "path": "tests/discount.rs", "start_line": 1 }],
                "what": "Covers the cap.",
                "why": "The cap is the edge.",
                "depends_on": ["s1"],
            }],
            "trivial_hunks": [{ "path": "src/lib.rs", "start_line": 1, "reason": "import order" }],
        })
    }

    #[test]
    fn draft_round_trips() {
        let v = draft_json();
        let draft: WalkthroughDraft = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(draft.steps[0].kind, StepKind::Logic);
        assert_eq!(draft.steps[0].risks[0].severity, RiskSeverity::Med);
        assert_eq!(draft.steps[1].depends_on, ["s1"]);
        assert_eq!(serde_json::to_value(&draft).unwrap(), v);
        // Unknown fields are refused, so typos do not pass silently.
        let mut bad = v;
        bad["steps"][0]["dependsOn"] = json!([]);
        assert!(serde_json::from_value::<WalkthroughDraft>(bad).is_err());
    }

    #[test]
    fn walkthrough_wire_shape() {
        let w = Walkthrough {
            id: WalkthroughId("volume-discount-3f2a9c1b".into()),
            workspace: WorkspaceId("ws-1".into()),
            title: "Volume discount".into(),
            summary: "s".into(),
            scope: WalkthroughScope {
                kind: ScopeKind::Worktree,
                root: RootId(0),
                base: Some("HEAD".into()),
                head: None,
                base_commit: Some("abc".into()),
                head_commit: None,
            },
            acceptance_criteria: vec![WalkthroughCriterion {
                id: "AC1".into(),
                text: "t".into(),
                status: Some(CriterionStatus::Met),
            }],
            steps: vec![],
            trivial_hunks: vec![TrivialHunk {
                root: RootId(0),
                path: "a.rs".into(),
                lines: Some(LineRange { start: 0, end: 2 }),
                reason: None,
            }],
            coverage: Some(WalkthroughCoverage {
                total: 3,
                covered: 1,
                trivial: 1,
                uncovered: vec![UncoveredHunk {
                    root: RootId(0),
                    path: "b.rs".into(),
                    new_range: LineRange { start: 9, end: 9 },
                    old_range: Some(LineRange { start: 9, end: 11 }),
                    added: 0,
                    removed: 3,
                    whole_file: None,
                    renamed_from: None,
                    excerpt: "-old".into(),
                }],
                unavailable: None,
            }),
            created_by: WalkthroughAuthor {
                session: SessionId("s-1".into()),
                agent: "claude-code".into(),
            },
            created_at: 5,
            storage: WalkthroughStorage {
                mode: StorageMode::Committed,
                path: Some(".coqo/walkthroughs/volume-discount-3f2a9c1b.json".into()),
                error: None,
            },
            step_states: BTreeMap::from([("s1".into(), StepState::Understood)]),
        };
        let v = serde_json::to_value(&w).unwrap();
        assert_eq!(
            v,
            json!({
                "id": "volume-discount-3f2a9c1b",
                "workspace": "ws-1",
                "title": "Volume discount",
                "summary": "s",
                "scope": { "kind": "worktree", "root": 0, "base": "HEAD", "baseCommit": "abc" },
                "acceptanceCriteria": [{ "id": "AC1", "text": "t", "status": "met" }],
                "steps": [],
                "trivialHunks": [{ "root": 0, "path": "a.rs", "lines": { "start": 0, "end": 2 } }],
                "coverage": {
                    "total": 3, "covered": 1, "trivial": 1,
                    "uncovered": [{
                        "root": 0, "path": "b.rs",
                        "newRange": { "start": 9, "end": 9 },
                        "oldRange": { "start": 9, "end": 11 },
                        "added": 0, "removed": 3, "excerpt": "-old",
                    }],
                },
                "createdBy": { "session": "s-1", "agent": "claude-code" },
                "createdAt": 5,
                "storage": { "mode": "committed", "path": ".coqo/walkthroughs/volume-discount-3f2a9c1b.json" },
                "stepStates": { "s1": "understood" },
            })
        );
        assert_eq!(serde_json::from_value::<Walkthrough>(v).unwrap(), w);
        let c = w.coverage.unwrap();
        assert_eq!(c.uncovered_count(), 1);
        assert!(!c.is_complete());
        assert_eq!(c.uncovered[0].location(), "b.rs:10");
        assert_eq!(c.uncovered[0].change(), "\u{2212}3 removed");
        assert!(WalkthroughCoverage::default().is_complete());
        assert!(!WalkthroughCoverage::unavailable("no git").is_complete());
    }

    #[test]
    fn kinds_name_themselves() {
        for kind in StepKind::ALL {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
        }
        for s in RiskSeverity::ALL {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
    }

    #[test]
    fn the_committed_schema_parses() {
        let schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["steps"].is_object());
    }
}
