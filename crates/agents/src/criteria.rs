// SPDX-License-Identifier: Apache-2.0

//! A session's acceptance criteria (`sessions/setCriteria`, the
//! `set_acceptance_criteria` MCP tool; ADR 0017).
//!
//! [`apply`] is pure: it takes the current list and one call's entries and
//! returns the new list, or why the call is wrong. The rules:
//!
//! - An entry whose `id` names a current criterion changes it: the fields it
//!   gives replace that criterion's (`evidence` replaces all of its
//!   evidence), the others stay.
//! - Any other entry adds a criterion: it needs `text`, starts `pending`,
//!   and gets its `id` or the next free `AC<n>`.
//! - Without `replace`, changed criteria stay in place and new ones go at
//!   the end. With `replace`, the list is exactly the entries, in order.
//! - Text is one line (whitespace collapsed) cut with `…`; an evidence path
//!   that is relative is taken from the session's `cwd`; lines are 1-based.
//! - A criterion's `changed_by` and `updated_at` move only when its text,
//!   status or evidence actually change, so a repeated call changes nothing.

use std::collections::HashSet;
use std::path::Path;

use coqo_protocol::methods::agents::{
    AcceptanceCriterion, CriterionAuthor, CriterionEvidence, CriterionInput, MAX_CRITERIA,
    MAX_CRITERION_CHARS, MAX_CRITERION_ID, MAX_EVIDENCE, MAX_EVIDENCE_CHARS,
};

/// Who calls and when, for the criteria a call changes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Change<'a> {
    pub(crate) author: CriterionAuthor,
    pub(crate) now: i64,
    /// The session's working directory, for relative evidence paths.
    pub(crate) cwd: &'a str,
}

/// The criteria after one `sessions/setCriteria` call (see the module
/// docs), or why the call is wrong.
pub(crate) fn apply(
    current: &[AcceptanceCriterion],
    entries: Vec<CriterionInput>,
    replace: bool,
    change: Change<'_>,
) -> Result<Vec<AcceptanceCriterion>, String> {
    let mut named = HashSet::new();
    for entry in &entries {
        if let Some(id) = &entry.id {
            check_id(id)?;
            if !named.insert(id.clone()) {
                return Err(format!("criterion `{id}` is named twice"));
            }
        }
    }
    let mut next = if replace {
        Vec::new()
    } else {
        current.to_vec()
    };
    for (n, entry) in entries.into_iter().enumerate() {
        let known = entry
            .id
            .as_deref()
            .and_then(|id| current.iter().find(|c| c.id == id));
        match known {
            Some(old) => {
                let updated = update(old, entry, change)?;
                match next.iter_mut().find(|c| c.id == updated.id) {
                    Some(slot) => *slot = updated,
                    None => next.push(updated),
                }
            }
            None => {
                let text = entry
                    .text
                    .as_deref()
                    .map(|t| one_line(t, MAX_CRITERION_CHARS))
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| match &entry.id {
                        Some(id) => {
                            format!("there is no criterion `{id}`; a new criterion needs `text`")
                        }
                        None => format!("criteria[{n}] is new and needs `text`"),
                    })?;
                let id = match entry.id {
                    Some(id) => id,
                    None => free_id(&next, &named),
                };
                next.push(AcceptanceCriterion {
                    id,
                    text,
                    status: entry.status.unwrap_or_default(),
                    evidence: evidence(entry.evidence.unwrap_or_default(), change.cwd)?,
                    changed_by: change.author,
                    updated_at: change.now,
                });
            }
        }
    }
    if next.len() > MAX_CRITERIA {
        return Err(format!(
            "a session has at most {MAX_CRITERIA} acceptance criteria"
        ));
    }
    Ok(next)
}

/// `old` with `entry`'s changes.
fn update(
    old: &AcceptanceCriterion,
    entry: CriterionInput,
    change: Change<'_>,
) -> Result<AcceptanceCriterion, String> {
    let mut new = old.clone();
    if let Some(text) = entry.text {
        let text = one_line(&text, MAX_CRITERION_CHARS);
        if text.is_empty() {
            return Err(format!("criterion `{}` needs a non-empty `text`", old.id));
        }
        new.text = text;
    }
    if let Some(status) = entry.status {
        new.status = status;
    }
    if let Some(ev) = entry.evidence {
        new.evidence = evidence(ev, change.cwd)?;
    }
    if (&new.text, new.status, &new.evidence) != (&old.text, old.status, &old.evidence) {
        new.changed_by = change.author;
        new.updated_at = change.now;
    }
    Ok(new)
}

fn check_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= MAX_CRITERION_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "criterion id `{id}` must be 1 to {MAX_CRITERION_ID} ASCII letters, digits, `.`, `_` or `-`"
        ))
    }
}

/// The next `AC<n>` not taken in `list` or by the call.
fn free_id(list: &[AcceptanceCriterion], named: &HashSet<String>) -> String {
    let highest = list
        .iter()
        .filter_map(|c| c.id.strip_prefix("AC")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    (highest + 1..)
        .map(|n| format!("AC{n}"))
        .find(|id| !named.contains(id) && !list.iter().any(|c| &c.id == id))
        .unwrap_or_else(|| unreachable!("an unbounded range"))
}

fn evidence(list: Vec<CriterionEvidence>, cwd: &str) -> Result<Vec<CriterionEvidence>, String> {
    if list.len() > MAX_EVIDENCE {
        return Err(format!(
            "a criterion has at most {MAX_EVIDENCE} pieces of evidence"
        ));
    }
    list.into_iter()
        .map(|e| {
            let text = one_line(&e.text, MAX_EVIDENCE_CHARS);
            if text.is_empty() {
                return Err("evidence needs a non-empty `text`".to_owned());
            }
            if e.line == Some(0) {
                return Err("evidence `line` is 1-based".to_owned());
            }
            let path = e
                .path
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty())
                .map(|p| {
                    if Path::new(&p).is_absolute() {
                        p
                    } else {
                        Path::new(cwd).join(p).to_string_lossy().into_owned()
                    }
                });
            Ok(CriterionEvidence {
                text,
                line: e.line.filter(|_| path.is_some()),
                path,
            })
        })
        .collect()
}

/// `text` as one line: whitespace collapsed, cut at `max` characters with
/// `…`. Empty if it has no visible text.
fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_protocol::methods::agents::CriterionStatus;

    const AGENT: Change<'static> = Change {
        author: CriterionAuthor::Agent,
        now: 10,
        cwd: "/w",
    };

    fn new(text: &str) -> CriterionInput {
        CriterionInput {
            text: Some(text.into()),
            ..CriterionInput::default()
        }
    }

    fn with_id(id: &str) -> CriterionInput {
        CriterionInput {
            id: Some(id.into()),
            ..CriterionInput::default()
        }
    }

    fn ids(list: &[AcceptanceCriterion]) -> Vec<&str> {
        list.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn new_criteria_get_ids_and_start_pending() {
        let list = apply(
            &[],
            vec![
                new("100+ units get 10% off"),
                new("  Capped at\n500 EUR "),
                CriterionInput {
                    id: Some("loyalty".into()),
                    ..new("Members get +2%")
                },
            ],
            false,
            AGENT,
        )
        .unwrap();
        assert_eq!(ids(&list), ["AC1", "AC2", "loyalty"]);
        assert_eq!(list[1].text, "Capped at 500 EUR");
        assert!(list.iter().all(|c| c.status == CriterionStatus::Pending));
        assert!(
            list.iter()
                .all(|c| c.changed_by == CriterionAuthor::Agent && c.updated_at == 10)
        );
        // More are added at the end, after the highest AC<n>.
        let more = apply(&list, vec![new("Logged")], false, AGENT).unwrap();
        assert_eq!(ids(&more), ["AC1", "AC2", "loyalty", "AC3"]);
        // A new criterion's id skips one the same call names.
        let named = apply(
            &[],
            vec![
                new("a"),
                CriterionInput {
                    id: Some("AC1".into()),
                    ..new("b")
                },
            ],
            false,
            AGENT,
        )
        .unwrap();
        assert_eq!(ids(&named), ["AC2", "AC1"]);
    }

    #[test]
    fn updates_change_only_what_they_give() {
        let list = apply(&[], vec![new("a"), new("b")], false, AGENT).unwrap();
        let later = Change { now: 20, ..AGENT };
        let met = apply(
            &list,
            vec![CriterionInput {
                status: Some(CriterionStatus::Met),
                evidence: Some(vec![CriterionEvidence {
                    text: "test `a` passes".into(),
                    path: Some("src/a.rs".into()),
                    line: Some(4),
                }]),
                ..with_id("AC2")
            }],
            false,
            later,
        )
        .unwrap();
        assert_eq!(ids(&met), ["AC1", "AC2"]);
        assert_eq!(met[0], list[0]);
        let b = &met[1];
        assert_eq!((b.text.as_str(), b.status), ("b", CriterionStatus::Met));
        assert_eq!(b.evidence[0].path.as_deref(), Some("/w/src/a.rs"));
        assert_eq!(b.evidence[0].line, Some(4));
        assert_eq!(b.updated_at, 20);

        // The same again changes nothing, not even the time.
        let again = apply(
            &met,
            vec![CriterionInput {
                status: Some(CriterionStatus::Met),
                ..with_id("AC2")
            }],
            false,
            Change { now: 30, ..AGENT },
        )
        .unwrap();
        assert_eq!(again, met);

        // The user marks it failed: theirs now.
        let user = Change {
            author: CriterionAuthor::User,
            now: 40,
            cwd: "/w",
        };
        let failed = apply(
            &met,
            vec![CriterionInput {
                status: Some(CriterionStatus::Failed),
                ..with_id("AC2")
            }],
            false,
            user,
        )
        .unwrap();
        assert_eq!(failed[1].changed_by, CriterionAuthor::User);
        assert_eq!(failed[1].evidence, met[1].evidence, "kept");

        // `evidence: []` clears it.
        let cleared = apply(
            &met,
            vec![CriterionInput {
                evidence: Some(Vec::new()),
                ..with_id("AC2")
            }],
            false,
            AGENT,
        )
        .unwrap();
        assert!(cleared[1].evidence.is_empty());
    }

    #[test]
    fn replace_keeps_only_the_named_in_order() {
        let list = apply(&[], vec![new("a"), new("b"), new("c")], false, AGENT).unwrap();
        let met = apply(
            &list,
            vec![CriterionInput {
                status: Some(CriterionStatus::Met),
                ..with_id("AC3")
            }],
            false,
            AGENT,
        )
        .unwrap();
        let replaced = apply(&met, vec![with_id("AC3"), new("d")], true, AGENT).unwrap();
        assert_eq!(ids(&replaced), ["AC3", "AC4"]);
        assert_eq!(replaced[0].status, CriterionStatus::Met, "kept");
        assert!(apply(&met, Vec::new(), true, AGENT).unwrap().is_empty());
    }

    #[test]
    fn bad_calls_say_why() {
        let list = apply(&[], vec![new("a")], false, AGENT).unwrap();
        let err = |entries: Vec<CriterionInput>| apply(&list, entries, false, AGENT).unwrap_err();
        assert!(err(vec![with_id("AC9")]).contains("needs `text`"));
        assert!(err(vec![CriterionInput::default()]).contains("needs `text`"));
        assert!(err(vec![new("  ")]).contains("needs `text`"));
        assert!(err(vec![with_id("has space")]).contains("must be"));
        assert!(err(vec![with_id("AC1"), with_id("AC1")]).contains("twice"));
        assert!(
            err(vec![CriterionInput {
                text: Some(" ".into()),
                ..with_id("AC1")
            }])
            .contains("non-empty")
        );
        let ev = |text: &str, line| CriterionEvidence {
            text: text.into(),
            path: Some("a.rs".into()),
            line,
        };
        let with_ev = |e: Vec<CriterionEvidence>| CriterionInput {
            evidence: Some(e),
            ..with_id("AC1")
        };
        assert!(err(vec![with_ev(vec![ev("", None)])]).contains("non-empty"));
        assert!(err(vec![with_ev(vec![ev("x", Some(0))])]).contains("1-based"));
        assert!(err(vec![with_ev(vec![ev("x", None); MAX_EVIDENCE + 1])]).contains("at most"));
        let many: Vec<_> = (0..MAX_CRITERIA).map(|_| new("x")).collect();
        assert!(err(many).contains("at most"));
    }

    #[test]
    fn long_text_is_cut() {
        let long = "word ".repeat(100);
        let list = apply(&[], vec![new(&long)], false, AGENT).unwrap();
        assert_eq!(list[0].text.chars().count(), MAX_CRITERION_CHARS);
        assert!(list[0].text.ends_with('…'));
    }
}
