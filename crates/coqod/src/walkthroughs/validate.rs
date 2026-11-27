// SPDX-License-Identifier: Apache-2.0

//! Checking a walkthrough draft: every problem at once, each with its JSON
//! path and what to do about it, for the agent to fix in one go.
//!
//! Three passes, all pure:
//!
//! 1. [`parse`]: the draft's shape (`walkthrough.schema.json`) from raw
//!    JSON, so one bad field does not hide the rest the way serde's first
//!    error would. Unknown fields are named with the field they probably
//!    meant (`dependsOn` → `depends_on`).
//! 2. [`check`]: what the schema cannot say: unique step ids, `depends_on`
//!    naming existing steps without cycles, `criteria_refs` naming the
//!    walkthrough's criteria (by default the session's, ADR 0017), line
//!    ranges, and the scope's `base` and `head` for its kind.
//! 3. [`check_lines`]: an anchor against the file's current text: lines
//!    inside the file, not blank, and holding the code the agent quoted.
//!
//! The service adds what needs the workspace: paths inside it, files that
//! exist, and git revisions that resolve.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::RangeInclusive;

use coqo_protocol::methods::agents::AcceptanceCriterion;
use coqo_protocol::methods::questions::MAX_ANCHOR_LINES;
use coqo_protocol::methods::walkthroughs::{
    AnchorDraft, CriterionDraft, MAX_NOTE, MAX_RISKS, MAX_STEP_ANCHORS, MAX_STEP_ID, MAX_STEPS,
    MAX_TEXT, MAX_TITLE, MAX_TRIVIAL_HUNKS, Risk, RiskSeverity, ScopeDraft, ScopeKind, StepDraft,
    StepKind, TrivialHunkDraft, WalkthroughCriterion, WalkthroughDraft, WalkthroughProblem,
};
use coqo_protocol::methods::workspace::RootId;
use serde_json::{Map, Value};

/// Problems found so far, in the order found.
#[derive(Debug, Default)]
pub(crate) struct Problems(pub Vec<WalkthroughProblem>);

impl Problems {
    pub fn add(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(WalkthroughProblem {
            path: path.into(),
            message: message.into(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) fn key(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

pub(crate) fn index(path: &str, i: usize) -> String {
    format!("{path}[{i}]")
}

/// Fields Coqo fills in; an agent that sends them is told to leave them
/// out.
const FILLED: &[&str] = &["id", "created_by", "created_at", "workspace", "storage"];

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// `dependsOn`, `depends-on` and `DependsOn` all mean `depends_on`.
fn loose(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// An object's fields, checked against what it may have.
struct Obj<'a> {
    map: &'a Map<String, Value>,
    path: String,
}

impl<'a> Obj<'a> {
    /// `v` as an object of `allowed` fields, or `None` (with a problem).
    fn new(
        v: &'a Value,
        path: &str,
        what: &str,
        allowed: &[&str],
        filled: &[&str],
        pr: &mut Problems,
    ) -> Option<Self> {
        let Value::Object(map) = v else {
            pr.add(
                path,
                format!("must be an object ({what}), not {}", type_name(v)),
            );
            return None;
        };
        for name in map.keys() {
            if allowed.contains(&name.as_str()) {
                continue;
            }
            let at = key(path, name);
            if filled.contains(&name.as_str()) {
                pr.add(at, "is filled in by Coqo; leave it out");
            } else if let Some(meant) = allowed.iter().find(|a| loose(a) == loose(name)) {
                pr.add(at, format!("unknown field; did you mean `{meant}`?"));
            } else {
                pr.add(
                    at,
                    format!("unknown field; {what} has {}", list(allowed.iter())),
                );
            }
        }
        Some(Self {
            map,
            path: path.to_owned(),
        })
    }

    fn at(&self, name: &str) -> String {
        key(&self.path, name)
    }

    /// A field; `null` counts as absent.
    fn get(&self, name: &str) -> Option<&'a Value> {
        self.map.get(name).filter(|v| !v.is_null())
    }

    fn string(&self, name: &str, required: bool, pr: &mut Problems) -> Option<&'a str> {
        match self.get(name) {
            None if required => {
                pr.add(self.at(name), "is required");
                None
            }
            None => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(v) => {
                pr.add(
                    self.at(name),
                    format!("must be a string, not {}", type_name(v)),
                );
                None
            }
        }
    }

    /// A required text: trimmed, not empty, at most `max` (`chars` or
    /// bytes).
    fn text(&self, name: &str, max: usize, chars: bool, pr: &mut Problems) -> String {
        let Some(s) = self.string(name, true, pr) else {
            return String::new();
        };
        let s = s.trim();
        if s.is_empty() {
            pr.add(self.at(name), "is empty");
        }
        let (len, unit) = if chars {
            (s.chars().count(), "characters")
        } else {
            (s.len(), "bytes")
        };
        if len > max {
            pr.add(
                self.at(name),
                format!("is {len} {unit}; keep it to {max} or fewer"),
            );
        }
        s.to_owned()
    }

    fn integer(&self, name: &str, min: u64, pr: &mut Problems) -> Option<u32> {
        let v = self.get(name)?;
        let n = v.as_u64().or_else(|| {
            v.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                .map(|f| f as u64)
        });
        match n.and_then(|n| u32::try_from(n).ok()) {
            Some(n) if u64::from(n) >= min => Some(n),
            _ if v.is_number() || n.is_some() => {
                pr.add(
                    self.at(name),
                    format!("must be an integer of at least {min}"),
                );
                None
            }
            _ => {
                pr.add(
                    self.at(name),
                    format!("must be an integer, not {}", type_name(v)),
                );
                None
            }
        }
    }

    fn root(&self, pr: &mut Problems) -> Option<RootId> {
        self.integer("root", 0, pr).map(RootId)
    }

    fn array(&self, name: &str, required: bool, pr: &mut Problems) -> Option<&'a [Value]> {
        match self.get(name) {
            None if required => {
                pr.add(self.at(name), "is required");
                None
            }
            None => None,
            Some(Value::Array(items)) => Some(items.as_slice()),
            Some(v) => {
                pr.add(
                    self.at(name),
                    format!("must be an array, not {}", type_name(v)),
                );
                None
            }
        }
    }

    fn strings(&self, name: &str, pr: &mut Problems) -> Vec<String> {
        let Some(items) = self.array(name, false, pr) else {
            return Vec::new();
        };
        let at = self.at(name);
        items
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                Value::String(s) => Some(s.trim().to_owned()),
                v => {
                    pr.add(
                        index(&at, i),
                        format!("must be a string id, not {}", type_name(v)),
                    );
                    None
                }
            })
            .collect()
    }
}

fn list<'a>(names: impl Iterator<Item = &'a &'a str>) -> String {
    names
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `v` as one of `options` by name.
fn choice<T: Copy>(
    v: Option<&Value>,
    path: &str,
    what: &str,
    options: &[(&str, T)],
    pr: &mut Problems,
) -> Option<T> {
    let names = || {
        options
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ")
    };
    match v {
        None => {
            pr.add(path, format!("is required: one of {}", names()));
            None
        }
        Some(Value::String(s)) => {
            if let Some((_, t)) = options.iter().find(|(n, _)| *n == s) {
                return Some(*t);
            }
            match options.iter().find(|(n, _)| loose(n) == loose(s)) {
                Some((n, t)) => {
                    pr.add(path, format!("`{s}` is not {what}; did you mean `{n}`?"));
                    Some(*t)
                }
                None => {
                    pr.add(path, format!("`{s}` is not {what}; use one of {}", names()));
                    None
                }
            }
        }
        Some(v) => {
            pr.add(
                path,
                format!("must be a string, one of {}, not {}", names(), type_name(v)),
            );
            None
        }
    }
}

fn step_kinds() -> Vec<(&'static str, StepKind)> {
    StepKind::ALL.iter().map(|k| (k.as_str(), *k)).collect()
}

fn severities() -> Vec<(&'static str, RiskSeverity)> {
    RiskSeverity::ALL.iter().map(|s| (s.as_str(), *s)).collect()
}

/// Pass 1: the draft's shape. Fields that are wrong get placeholders (an
/// empty string, no anchors) so the other passes can go on; the problems
/// say what was wrong.
pub(crate) fn parse(v: &Value, pr: &mut Problems) -> WalkthroughDraft {
    let mut draft = WalkthroughDraft {
        title: String::new(),
        summary: String::new(),
        scope: ScopeDraft::default(),
        acceptance_criteria: None,
        steps: Vec::new(),
        trivial_hunks: Vec::new(),
    };
    let Some(top) = Obj::new(
        v,
        "",
        "a walkthrough",
        &[
            "title",
            "summary",
            "scope",
            "acceptance_criteria",
            "steps",
            "trivial_hunks",
        ],
        FILLED,
        pr,
    ) else {
        return draft;
    };
    draft.title = top.text("title", MAX_TITLE, true, pr);
    draft.summary = top.text("summary", MAX_TEXT, false, pr);
    match top.get("scope") {
        None => pr.add(
            "scope",
            "is required: {\"kind\": \"worktree\"} for uncommitted changes",
        ),
        Some(v) => draft.scope = parse_scope(v, pr),
    }
    if let Some(items) = top.array("acceptance_criteria", false, pr) {
        let mut out = Vec::new();
        for (i, v) in items.iter().enumerate() {
            let path = index("acceptance_criteria", i);
            let Some(o) = Obj::new(v, &path, "a criterion", &["id", "text"], &[], pr) else {
                continue;
            };
            let id = o.string("id", true, pr).map(str::trim).unwrap_or_default();
            let text = o
                .string("text", false, pr)
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty());
            out.push(CriterionDraft {
                id: id.to_owned(),
                text,
            });
        }
        draft.acceptance_criteria = Some(out);
    }
    if let Some(items) = top.array("steps", true, pr) {
        if items.is_empty() {
            pr.add(
                "steps",
                "is empty; a walkthrough needs at least one step, anchored to the code it explains",
            );
        }
        if items.len() > MAX_STEPS {
            pr.add(
                "steps",
                format!(
                    "has {} steps; keep it to {MAX_STEPS} (group small changes into one step)",
                    items.len()
                ),
            );
        }
        draft.steps = items
            .iter()
            .enumerate()
            .map(|(i, v)| parse_step(v, &index("steps", i), pr))
            .collect();
    }
    if let Some(items) = top.array("trivial_hunks", false, pr) {
        if items.len() > MAX_TRIVIAL_HUNKS {
            pr.add(
                "trivial_hunks",
                format!(
                    "has {} hunks; at most {MAX_TRIVIAL_HUNKS} (list a whole file once)",
                    items.len()
                ),
            );
        }
        draft.trivial_hunks = items
            .iter()
            .enumerate()
            .filter_map(|(i, v)| parse_trivial(v, &index("trivial_hunks", i), pr))
            .collect();
    }
    draft
}

fn parse_scope(v: &Value, pr: &mut Problems) -> ScopeDraft {
    let mut scope = ScopeDraft::default();
    let Some(o) = Obj::new(
        v,
        "scope",
        "a scope",
        &["kind", "base", "head", "root"],
        &[],
        pr,
    ) else {
        return scope;
    };
    let kinds = [
        ("worktree", ScopeKind::Worktree),
        ("commits", ScopeKind::Commits),
        ("pr", ScopeKind::Pr),
    ];
    scope.kind =
        choice(o.get("kind"), "scope.kind", "a scope kind", &kinds, pr).unwrap_or_default();
    let rev = |name: &str, pr: &mut Problems| {
        o.string(name, false, pr)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    scope.base = rev("base", pr);
    scope.head = rev("head", pr);
    scope.root = o.root(pr);
    scope
}

fn parse_step(v: &Value, path: &str, pr: &mut Problems) -> StepDraft {
    let mut step = StepDraft {
        id: String::new(),
        title: String::new(),
        kind: StepKind::Logic,
        anchors: Vec::new(),
        what: String::new(),
        why: String::new(),
        risks: Vec::new(),
        criteria_refs: Vec::new(),
        depends_on: Vec::new(),
    };
    let Some(o) = Obj::new(
        v,
        path,
        "a step",
        &[
            "id",
            "title",
            "kind",
            "anchors",
            "what",
            "why",
            "risks",
            "criteria_refs",
            "depends_on",
        ],
        &[],
        pr,
    ) else {
        return step;
    };
    step.id = o
        .string("id", true, pr)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    step.title = o.text("title", MAX_TITLE, true, pr);
    if let Some(kind) = choice(
        o.get("kind"),
        &o.at("kind"),
        "a step kind",
        &step_kinds(),
        pr,
    ) {
        step.kind = kind;
    }
    step.what = o.text("what", MAX_TEXT, false, pr);
    step.why = o.text("why", MAX_TEXT, false, pr);
    if let Some(items) = o.array("anchors", true, pr) {
        let at = o.at("anchors");
        if items.is_empty() {
            pr.add(
                &at,
                "is empty; anchor the step to the lines it explains ({\"path\", \"start_line\", \"end_line\"})",
            );
        }
        if items.len() > MAX_STEP_ANCHORS {
            pr.add(
                &at,
                format!(
                    "has {} anchors; at most {MAX_STEP_ANCHORS} (split the step)",
                    items.len()
                ),
            );
        }
        step.anchors = items
            .iter()
            .enumerate()
            .map(|(i, v)| parse_anchor(v, &index(&at, i), pr))
            .collect();
    }
    if let Some(items) = o.array("risks", false, pr) {
        let at = o.at("risks");
        if items.len() > MAX_RISKS {
            pr.add(
                &at,
                format!("has {} risks; at most {MAX_RISKS}", items.len()),
            );
        }
        for (i, v) in items.iter().enumerate() {
            let path = index(&at, i);
            let Some(r) = Obj::new(v, &path, "a risk", &["severity", "text"], &[], pr) else {
                continue;
            };
            let severity = match r.get("severity") {
                Some(Value::String(s)) if matches!(s.as_str(), "medium" | "moderate") => {
                    pr.add(
                        r.at("severity"),
                        format!("`{s}` is not a severity; did you mean `med`?"),
                    );
                    Some(RiskSeverity::Med)
                }
                v => choice(v, &r.at("severity"), "a severity", &severities(), pr),
            };
            let text = r.text("text", MAX_NOTE, true, pr);
            if let Some(severity) = severity {
                step.risks.push(Risk { severity, text });
            }
        }
    }
    step.criteria_refs = o.strings("criteria_refs", pr);
    step.depends_on = o.strings("depends_on", pr);
    step
}

/// An anchor; one that is too broken to look for gets an empty path.
fn parse_anchor(v: &Value, path: &str, pr: &mut Problems) -> AnchorDraft {
    let mut anchor = AnchorDraft {
        path: String::new(),
        root: None,
        start_line: 0,
        end_line: None,
        text: None,
    };
    let Some(o) = Obj::new(
        v,
        path,
        "an anchor",
        &["path", "root", "start_line", "end_line", "text"],
        &[],
        pr,
    ) else {
        return anchor;
    };
    let before = pr.0.len();
    let file = o
        .string("path", true, pr)
        .map(str::trim)
        .unwrap_or_default();
    if o.get("path").is_some() && file.is_empty() {
        pr.add(o.at("path"), "is empty");
    }
    anchor.root = o.root(pr);
    let start = o.integer("start_line", 1, pr);
    if start.is_none() && o.get("start_line").is_none() {
        pr.add(o.at("start_line"), "is required (1-based)");
    }
    anchor.end_line = o.integer("end_line", 1, pr);
    anchor.text = o
        .string("text", false, pr)
        .map(str::to_owned)
        .filter(|t| !t.trim().is_empty());
    if let (Some(start), Some(end)) = (start, anchor.end_line)
        && end < start
    {
        pr.add(
            o.at("end_line"),
            format!("{end} is before start_line {start}"),
        );
    }
    if let (Some(start), Some(end)) = (start, anchor.end_line)
        && end >= start
        && end - start >= MAX_ANCHOR_LINES
    {
        pr.add(
            path,
            format!(
                "spans {} lines; anchor at most {MAX_ANCHOR_LINES} (the code the step is about)",
                end - start + 1
            ),
        );
    }
    if pr.0.len() == before {
        anchor.path = file.to_owned();
        anchor.start_line = start.unwrap_or(1);
    }
    anchor
}

fn parse_trivial(v: &Value, path: &str, pr: &mut Problems) -> Option<TrivialHunkDraft> {
    let o = Obj::new(
        v,
        path,
        "a trivial hunk",
        &["path", "root", "start_line", "end_line", "reason"],
        &[],
        pr,
    )?;
    let before = pr.0.len();
    let file = o
        .string("path", true, pr)
        .map(str::trim)
        .unwrap_or_default();
    if o.get("path").is_some() && file.is_empty() {
        pr.add(o.at("path"), "is empty");
    }
    let root = o.root(pr);
    let start = o.integer("start_line", 1, pr);
    let end = o.integer("end_line", 1, pr);
    if end.is_some() && start.is_none() {
        pr.add(o.at("start_line"), "is required with end_line");
    }
    if let (Some(start), Some(end)) = (start, end)
        && end < start
    {
        pr.add(
            o.at("end_line"),
            format!("{end} is before start_line {start}"),
        );
    }
    let reason = o
        .string("reason", false, pr)
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    if reason
        .as_ref()
        .is_some_and(|r| r.chars().count() > MAX_NOTE)
    {
        pr.add(
            o.at("reason"),
            format!("is longer than {MAX_NOTE} characters"),
        );
    }
    (pr.0.len() == before).then(|| TrivialHunkDraft {
        path: file.to_owned(),
        root,
        start_line: start,
        end_line: end,
        reason,
    })
}

fn valid_id(id: &str) -> bool {
    (1..=MAX_STEP_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Pass 2: what the schema cannot say. Returns the walkthrough's acceptance
/// criteria: the ones it names, or by default every one of `session`'s.
pub(crate) fn check(
    draft: &WalkthroughDraft,
    session: &[AcceptanceCriterion],
    pr: &mut Problems,
) -> Vec<WalkthroughCriterion> {
    // Scope: what each kind needs.
    match draft.scope.kind {
        ScopeKind::Worktree => {
            if draft.scope.head.is_some() {
                pr.add(
                    "scope.head",
                    "a worktree scope has no head (the working tree is the head); leave it out, or use kind `commits`",
                );
            }
        }
        ScopeKind::Commits | ScopeKind::Pr => {
            let kind = if draft.scope.kind == ScopeKind::Pr {
                "pr"
            } else {
                "commits"
            };
            for (name, rev) in [("base", &draft.scope.base), ("head", &draft.scope.head)] {
                if rev.is_none() {
                    pr.add(
                        format!("scope.{name}"),
                        format!("is required for a `{kind}` scope (a commit, branch or tag)"),
                    );
                }
            }
        }
    }

    // Acceptance criteria.
    let known: Vec<&str> = session.iter().map(|c| c.id.as_str()).collect();
    let known_list = || {
        if known.is_empty() {
            "this session has none (set them with set_acceptance_criteria)".to_owned()
        } else {
            format!("this session's are {}", known.join(", "))
        }
    };
    let criteria: Vec<WalkthroughCriterion> = match &draft.acceptance_criteria {
        None => session
            .iter()
            .map(|c| WalkthroughCriterion {
                id: c.id.clone(),
                text: c.text.clone(),
                status: Some(c.status),
            })
            .collect(),
        Some(list) => {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for (i, c) in list.iter().enumerate() {
                let path = index("acceptance_criteria", i);
                if c.id.is_empty() {
                    continue;
                }
                if !seen.insert(c.id.as_str()) {
                    pr.add(key(&path, "id"), format!("`{}` is listed twice", c.id));
                    continue;
                }
                let live = session.iter().find(|s| s.id == c.id);
                match (live, &c.text) {
                    (_, Some(text)) => out.push(WalkthroughCriterion {
                        id: c.id.clone(),
                        text: text.clone(),
                        status: live.map(|s| s.status),
                    }),
                    (Some(s), None) => out.push(WalkthroughCriterion {
                        id: s.id.clone(),
                        text: s.text.clone(),
                        status: Some(s.status),
                    }),
                    (None, None) => pr.add(
                        key(&path, "id"),
                        format!(
                            "`{}` is not one of this session's acceptance criteria ({}); give its `text`, or set it with set_acceptance_criteria first",
                            c.id,
                            known_list()
                        ),
                    ),
                }
            }
            out
        }
    };

    // Step ids.
    let mut ids: HashMap<&str, usize> = HashMap::new();
    for (i, s) in draft.steps.iter().enumerate() {
        let path = key(&index("steps", i), "id");
        if s.id.is_empty() {
            continue;
        }
        if !valid_id(&s.id) {
            pr.add(
                &path,
                format!(
                    "`{}` is not a valid step id: use 1 to {MAX_STEP_ID} letters, digits, `.`, `_` or `-` (e.g. `s{}`)",
                    s.id,
                    i + 1
                ),
            );
            continue;
        }
        if let Some(first) = ids.get(s.id.as_str()) {
            pr.add(
                &path,
                format!(
                    "`{}` is also the id of steps[{first}]; step ids must be unique",
                    s.id
                ),
            );
            continue;
        }
        ids.insert(&s.id, i);
    }

    // References.
    let criterion_ids: HashSet<&str> = criteria.iter().map(|c| c.id.as_str()).collect();
    let criterion_list = || {
        if criteria.is_empty() {
            "the walkthrough has none: list them in acceptance_criteria, or set them with set_acceptance_criteria".to_owned()
        } else {
            format!(
                "the walkthrough's are {}",
                criteria
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    let mut edges: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, s) in draft.steps.iter().enumerate() {
        let path = index("steps", i);
        for (k, r) in s.criteria_refs.iter().enumerate() {
            if !criterion_ids.contains(r.as_str()) {
                pr.add(
                    index(&key(&path, "criteria_refs"), k),
                    format!("no acceptance criterion `{r}`; {}", criterion_list()),
                );
            }
        }
        let mut seen = HashSet::new();
        for (k, d) in s.depends_on.iter().enumerate() {
            let at = index(&key(&path, "depends_on"), k);
            if !seen.insert(d.as_str()) {
                pr.add(at, format!("`{d}` is listed twice"));
                continue;
            }
            match ids.get(d.as_str()) {
                Some(&j) if j == i => pr.add(at, "a step cannot depend on itself"),
                Some(&j) => edges.entry(i).or_default().push(j),
                None => {
                    let mut all: Vec<_> = ids.keys().copied().collect();
                    all.sort_unstable();
                    pr.add(
                        at,
                        format!("no step has id `{d}`; the step ids are {}", all.join(", ")),
                    );
                }
            }
        }
    }
    for cycle in cycles(draft.steps.len(), &edges) {
        let first = cycle[0];
        let names: Vec<&str> = cycle
            .iter()
            .chain(std::iter::once(&first))
            .map(|&i| draft.steps[i].id.as_str())
            .collect();
        pr.add(
            key(&index("steps", first), "depends_on"),
            format!(
                "{} is a cycle; drop one of these dependencies so the steps have a reading order",
                names.join(" → ")
            ),
        );
    }
    criteria
}

/// Each cycle of the `depends_on` graph once, as the steps on it starting
/// from its earliest.
fn cycles(n: usize, edges: &BTreeMap<usize, Vec<usize>>) -> Vec<Vec<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    fn visit(
        i: usize,
        edges: &BTreeMap<usize, Vec<usize>>,
        marks: &mut [Mark],
        stack: &mut Vec<usize>,
        out: &mut Vec<Vec<usize>>,
    ) {
        marks[i] = Mark::Open;
        stack.push(i);
        for &j in edges.get(&i).into_iter().flatten() {
            match marks[j] {
                Mark::New => visit(j, edges, marks, stack, out),
                Mark::Open => {
                    let from = stack.iter().position(|&s| s == j).unwrap_or(0);
                    let mut cycle = stack[from..].to_vec();
                    let min = cycle
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, s)| **s)
                        .map_or(0, |(k, _)| k);
                    cycle.rotate_left(min);
                    if !out.contains(&cycle) {
                        out.push(cycle);
                    }
                }
                Mark::Done => {}
            }
        }
        stack.pop();
        marks[i] = Mark::Done;
    }
    let mut marks = vec![Mark::New; n];
    let mut out = Vec::new();
    for i in 0..n {
        if marks[i] == Mark::New {
            visit(i, edges, &mut marks, &mut Vec::new(), &mut out);
        }
    }
    out
}

/// The text without whitespace, and the 0-based line of each byte kept.
fn squeeze(text: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut lines = Vec::new();
    let mut line = 0;
    for c in text.chars() {
        if c == '\n' {
            line += 1;
        }
        if c.is_whitespace() {
            continue;
        }
        out.push(c);
        lines.extend(std::iter::repeat_n(line, c.len_utf8()));
    }
    (out, lines)
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn shorten(s: &str) -> String {
    const MAX: usize = 60;
    if s.chars().count() > MAX {
        let cut: String = s.chars().take(MAX).collect();
        format!("{cut}…")
    } else {
        s.to_owned()
    }
}

/// Pass 3: `anchor`'s lines (0-based, inclusive) in `text`, the file
/// `shown` as it is now, or why they are not the code the step means.
pub(crate) fn check_lines(
    text: &str,
    anchor: &AnchorDraft,
    shown: &str,
) -> Result<RangeInclusive<usize>, (&'static str, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let count = lines.len();
    let start = anchor.start_line as usize;
    let end = anchor.end_line.map_or(start, |e| e as usize);
    if count == 0 {
        return Err(("path", format!("{shown} is empty; anchor code that exists")));
    }
    if start > count {
        return Err((
            "start_line",
            format!("{shown} has {count} lines; line {start} is past its end"),
        ));
    }
    if end > count {
        return Err((
            "end_line",
            format!("{shown} has {count} lines; line {end} is past its end"),
        ));
    }
    let span = &lines[start - 1..end];
    let range = || {
        if start == end {
            format!("line {start}")
        } else {
            format!("lines {start}-{end}")
        }
    };
    if span.iter().all(|l| l.trim().is_empty()) {
        return Err((
            "start_line",
            format!(
                "{} of {shown} {} blank; anchor the code the step explains",
                range(),
                if start == end { "is" } else { "are" }
            ),
        ));
    }
    if let Some(quoted) = &anchor.text {
        let (want, _) = squeeze(quoted);
        let (have, _) = squeeze(&span.join("\n"));
        if !want.is_empty() && !have.contains(&want) {
            let (file, at) = squeeze(text);
            let found: Vec<usize> = file
                .match_indices(&want)
                .map(|(i, _)| at[i] + 1)
                .take(3)
                .collect();
            let hint = match found.as_slice() {
                [] => format!(
                    "it is not in the file at all; read {shown} again and quote the code as it is now"
                ),
                [line] => format!("it is at line {line}; fix start_line and end_line"),
                lines => format!(
                    "it is at lines {}; fix start_line and end_line",
                    lines
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            return Err((
                "text",
                format!(
                    "`{}` is not in {} of {shown} (which start `{}`); {hint}",
                    shorten(first_line(quoted)),
                    range(),
                    shorten(first_line(&span.join("\n")))
                ),
            ));
        }
    }
    Ok(start - 1..=end - 1)
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::agents::{CriterionAuthor, CriterionStatus};
    use serde_json::json;

    use super::*;

    fn draft() -> Value {
        json!({
            "title": "Volume discount",
            "summary": "Orders of 100+ units get 10% off.",
            "scope": { "kind": "worktree" },
            "steps": [{
                "id": "s1",
                "title": "The rule",
                "kind": "logic",
                "anchors": [{ "path": "src/discount.rs", "start_line": 2, "end_line": 4 }],
                "what": "Computes it.",
                "why": "One place.",
                "risks": [{ "severity": "high", "text": "Rounding." }],
                "criteria_refs": ["AC1"],
            }, {
                "id": "s2",
                "title": "Tests",
                "kind": "test",
                "anchors": [{ "path": "tests/d.rs", "start_line": 1 }],
                "what": "Covers the cap.",
                "why": "The edge.",
                "depends_on": ["s1"],
            }],
        })
    }

    fn criterion(id: &str) -> AcceptanceCriterion {
        AcceptanceCriterion {
            id: id.into(),
            text: format!("criterion {id}"),
            status: CriterionStatus::Met,
            evidence: Vec::new(),
            changed_by: CriterionAuthor::Agent,
            updated_at: 0,
        }
    }

    fn problems(v: &Value, session: &[AcceptanceCriterion]) -> Vec<String> {
        let mut pr = Problems::default();
        let d = parse(v, &mut pr);
        check(&d, session, &mut pr);
        pr.0.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_good_draft_parses_as_serde_reads_it() {
        let v = draft();
        let mut pr = Problems::default();
        let d = parse(&v, &mut pr);
        let criteria = check(&d, &[criterion("AC1")], &mut pr);
        assert!(pr.is_empty(), "{:?}", pr.0);
        let serde: WalkthroughDraft = serde_json::from_value(v).unwrap();
        assert_eq!(d, serde);
        assert_eq!(criteria[0].text, "criterion AC1");
        assert_eq!(criteria[0].status, Some(CriterionStatus::Met));
    }

    #[test]
    fn shape_problems_are_all_reported_with_their_paths() {
        let mut v = draft();
        v["id"] = json!("mine");
        v["title"] = json!("");
        v["steps"][0]["kind"] = json!("refactor");
        v["steps"][0]["dependsOn"] = json!([]);
        v["steps"][0]["risks"][0]["severity"] = json!("critical");
        v["steps"][0]["anchors"][0]["end_line"] = json!(1);
        v["steps"][1]["kind"] = json!("edge-case");
        v["steps"][1]["anchors"] = json!([]);
        v["steps"][1]["why"] = json!(3);
        v["scope"] = json!({ "kind": "branch" });
        let got = problems(&v, &[criterion("AC1")]);
        let want = [
            "id: is filled in by Coqo; leave it out",
            "title: is empty",
            "scope.kind: `branch` is not a scope kind; use one of worktree, commits, pr",
            "steps[0].dependsOn: unknown field; did you mean `depends_on`?",
            "steps[0].kind: `refactor` is not a step kind; use one of entry_point, flow, data_model, logic, edge_case, test, config, migration",
            "steps[0].anchors[0].end_line: 1 is before start_line 2",
            "steps[0].risks[0].severity: `critical` is not a severity; use one of low, med, high",
            "steps[1].kind: `edge-case` is not a step kind; did you mean `edge_case`?",
            "steps[1].why: must be a string, not a number",
            "steps[1].anchors: is empty; anchor the step to the lines it explains ({\"path\", \"start_line\", \"end_line\"})",
        ];
        for w in want {
            assert!(got.iter().any(|g| g == w), "missing {w:?} in {got:#?}");
        }
        assert_eq!(got.len(), want.len(), "{got:#?}");
    }

    #[test]
    fn missing_and_empty_parts() {
        assert_eq!(
            problems(&json!([]), &[]),
            ["must be an object (a walkthrough), not an array"]
        );
        let got = problems(&json!({ "steps": [] }), &[]);
        assert_eq!(
            got,
            [
                "title: is required",
                "summary: is required",
                "scope: is required: {\"kind\": \"worktree\"} for uncommitted changes",
                "steps: is empty; a walkthrough needs at least one step, anchored to the code it explains",
            ]
        );
    }

    #[test]
    fn references_and_cycles() {
        let mut v = draft();
        v["steps"][0]["depends_on"] = json!(["s2"]);
        v["steps"][1]["depends_on"] = json!(["s1", "s9", "s1"]);
        v["steps"][1]["criteria_refs"] = json!(["AC7"]);
        v["steps"][1]["id"] = json!("s1");
        let got = problems(&v, &[criterion("AC1")]);
        assert!(
            got.contains(
                &"steps[1].id: `s1` is also the id of steps[0]; step ids must be unique".to_owned()
            ),
            "{got:#?}"
        );
        assert!(
            got.contains(
                &"steps[0].depends_on[0]: no step has id `s2`; the step ids are s1".to_owned()
            ),
            "{got:#?}"
        );
        assert!(
            got.contains(&"steps[1].criteria_refs[0]: no acceptance criterion `AC7`; the walkthrough's are AC1".to_owned()),
            "{got:#?}"
        );
        assert!(
            got.contains(&"steps[1].depends_on[2]: `s1` is listed twice".to_owned()),
            "{got:#?}"
        );

        let mut v = draft();
        v["steps"][0]["depends_on"] = json!(["s2"]);
        let got = problems(&v, &[criterion("AC1")]);
        assert_eq!(
            got,
            [
                "steps[0].depends_on: s1 → s2 → s1 is a cycle; drop one of these dependencies so the steps have a reading order"
            ]
        );
        let mut v = draft();
        v["steps"][0]["depends_on"] = json!(["s1"]);
        assert_eq!(
            problems(&v, &[criterion("AC1")]),
            ["steps[0].depends_on[0]: a step cannot depend on itself"]
        );
    }

    #[test]
    fn criteria_come_from_the_session_unless_named() {
        // No session criteria: a reference has nothing to point at.
        let got = problems(&draft(), &[]);
        assert_eq!(
            got,
            [
                "steps[0].criteria_refs[0]: no acceptance criterion `AC1`; the walkthrough has none: list them in acceptance_criteria, or set them with set_acceptance_criteria"
            ]
        );
        // Named: a session one by id, a new one with text.
        let mut v = draft();
        v["acceptance_criteria"] =
            json!([{ "id": "AC1" }, { "id": "X", "text": "fast" }, { "id": "AC9" }]);
        v["steps"][0]["criteria_refs"] = json!(["AC1", "X"]);
        let mut pr = Problems::default();
        let d = parse(&v, &mut pr);
        let criteria = check(&d, &[criterion("AC1"), criterion("AC2")], &mut pr);
        let got: Vec<String> = pr.0.iter().map(ToString::to_string).collect();
        assert_eq!(
            got,
            [
                "acceptance_criteria[2].id: `AC9` is not one of this session's acceptance criteria (this session's are AC1, AC2); give its `text`, or set it with set_acceptance_criteria first"
            ]
        );
        assert_eq!(criteria.len(), 2);
        assert_eq!(criteria[1].status, None);
    }

    #[test]
    fn scopes_need_what_their_kind_needs() {
        let mut v = draft();
        v["scope"] = json!({ "kind": "commits", "base": "main" });
        assert_eq!(
            problems(&v, &[criterion("AC1")]),
            ["scope.head: is required for a `commits` scope (a commit, branch or tag)"]
        );
        v["scope"] = json!({ "kind": "worktree", "head": "main" });
        assert_eq!(
            problems(&v, &[criterion("AC1")]),
            [
                "scope.head: a worktree scope has no head (the working tree is the head); leave it out, or use kind `commits`"
            ]
        );
    }

    const FILE: &str = "use std::fmt;\n\nfn discount(units: u32) -> u32 {\n    if units >= 100 { 10 } else { 0 }\n}\n\n\nfn cap() {}\n";

    fn anchor(start: u32, end: Option<u32>, text: Option<&str>) -> AnchorDraft {
        AnchorDraft {
            path: "src/d.rs".into(),
            root: None,
            start_line: start,
            end_line: end,
            text: text.map(Into::into),
        }
    }

    #[test]
    fn anchors_must_match_the_file() {
        assert_eq!(
            check_lines(FILE, &anchor(3, Some(5), None), "src/d.rs"),
            Ok(2..=4)
        );
        assert_eq!(
            check_lines(FILE, &anchor(3, Some(4), Some("if units>=100")), "src/d.rs"),
            Ok(2..=3)
        );
        assert_eq!(
            check_lines(FILE, &anchor(7, Some(12), None), "src/d.rs"),
            Err((
                "end_line",
                "src/d.rs has 8 lines; line 12 is past its end".into()
            ))
        );
        assert_eq!(
            check_lines(FILE, &anchor(6, Some(7), None), "src/d.rs"),
            Err((
                "start_line",
                "lines 6-7 of src/d.rs are blank; anchor the code the step explains".into()
            ))
        );
        assert_eq!(
            check_lines(FILE, &anchor(1, Some(2), Some("fn cap()")), "src/d.rs"),
            Err((
                "text",
                "`fn cap()` is not in lines 1-2 of src/d.rs (which start `use std::fmt;`); it is at line 8; fix start_line and end_line".into()
            ))
        );
        let Err((_, gone)) = check_lines(FILE, &anchor(1, None, Some("fn gone()")), "src/d.rs")
        else {
            panic!("a quote that is nowhere");
        };
        assert!(gone.contains("not in the file at all"), "{gone}");
    }
}
