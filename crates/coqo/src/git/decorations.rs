// SPDX-License-Identifier: Apache-2.0

//! The file tree's git status letters (issue #163, board 1): a row
//! decoration provider ([`crate::files::decorations`]) fed by the window's
//! [`GitModel`], so it is live from `git/changes`.
//!
//! A changed file's row ends in its letter: `M` in `sand`, `A` in `green`,
//! `D` in `rose`, `?` in `text_subtle`, `T` (type change) in `lavender` and
//! `U` (conflict) in `rose`, and `R` (renamed here, #226) in `blue`
//! ([`StatusEntry::letter`]; the old path of a rename has no entry). A
//! folder with changes inside has its name tinted in the colour of the
//! strongest one: a conflict, then a modification, a deletion, a rename, a
//! type change, and new files (added or untracked) in `green`. A deleted
//! file has no row, so its folder's tint is what shows it.

use std::{any::Any, collections::HashMap};

use coqo_protocol::methods::{
    git::{StatusEntry, StatusResult},
    workspace::RootId,
};
use coqo_ui::gpui::{App, Entity, Window};

use super::model::GitModel;
use crate::files::decorations::{self, Badge, RootDecorations, RowDecorations, Tone};

/// The provider's id.
pub const PROVIDER: &str = "git";

/// Register the provider.
pub fn init(cx: &mut App) {
    decorations::register_provider(cx, PROVIDER, attach);
}

/// The colour of a status letter, in the tree and the Git pane.
pub fn tone(letter: char) -> Tone {
    match letter {
        'M' => Tone::Sand,
        'A' => Tone::Green,
        'D' | 'U' => Tone::Rose,
        'T' => Tone::Lavender,
        'R' => Tone::Blue,
        _ => Tone::Subtle,
    }
}

/// How strongly a letter tints its folders, and in what.
fn folder_tint(letter: char) -> (u8, Tone) {
    match letter {
        'U' => (6, Tone::Rose),
        'M' => (5, Tone::Sand),
        'D' => (4, Tone::Rose),
        'R' => (3, Tone::Blue),
        'T' => (2, Tone::Lavender),
        _ => (1, Tone::Green),
    }
}

/// The letters and folder tints of one root's `git/status`.
pub fn root_decorations(status: &StatusResult) -> RootDecorations {
    let prefix = status.repo.prefix.trim_end_matches('/');
    let mut badges = HashMap::new();
    let mut tints: HashMap<String, (u8, Tone)> = HashMap::new();
    for entry in &status.entries {
        let Some(letter) = StatusEntry::letter(entry) else {
            continue;
        };
        // An untracked folder git didn't look into ends in `/`.
        let path = entry.path.trim_end_matches('/');
        let rel = if prefix.is_empty() {
            path
        } else {
            match path
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
            {
                Some(rel) => rel,
                None => continue,
            }
        };
        if rel.is_empty() {
            continue;
        }
        badges.insert(
            rel.to_owned(),
            Badge {
                letter,
                tone: tone(letter),
            },
        );
        let tint = folder_tint(letter);
        let mut dir = rel;
        while !dir.is_empty() {
            dir = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
            match tints.get_mut(dir) {
                // Its folders above are at least as strong already.
                Some(have) if have.0 >= tint.0 => break,
                Some(have) => *have = tint,
                None => {
                    tints.insert(dir.to_owned(), tint);
                }
            }
        }
    }
    RootDecorations {
        badges,
        tints: tints.into_iter().map(|(k, (_, tone))| (k, tone)).collect(),
    }
}

/// Fill `decorations` from the window's git model, now and on each change.
fn attach(decorations: &Entity<RowDecorations>, window: &mut Window, cx: &mut App) -> Box<dyn Any> {
    let model = GitModel::for_window(window, cx);
    let target = decorations.downgrade();
    // The statuses last turned into decorations: the model also notifies
    // for branches, stashes and busy states.
    let mut last: Vec<(RootId, Option<StatusResult>)> = Vec::new();
    let mut refresh = move |model: &Entity<GitModel>, cx: &mut App| {
        let now: Vec<(RootId, Option<StatusResult>)> = model
            .read(cx)
            .roots()
            .iter()
            .map(|r| (r.root, r.status.clone()))
            .collect();
        if now == last {
            return;
        }
        let roots = now
            .iter()
            .filter_map(|(root, status)| Some((*root, root_decorations(status.as_ref()?))))
            .collect();
        last = now;
        if let Some(target) = target.upgrade() {
            target.update(cx, |d, cx| d.set(PROVIDER, roots, cx));
        }
    };
    refresh(&model, cx);
    let subscription = cx.observe(&model, move |model, cx| refresh(&model, cx));
    Box::new((model, subscription))
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::git::{Conflict, FileChange, Head, RepoInfo};

    use super::*;

    fn entry(path: &str, staged: Option<FileChange>, unstaged: Option<FileChange>) -> StatusEntry {
        StatusEntry {
            path: path.into(),
            orig_path: None,
            staged,
            unstaged,
            conflict: None,
        }
    }

    fn status(prefix: &str, entries: Vec<StatusEntry>) -> StatusResult {
        StatusResult {
            repo: RepoInfo {
                workdir: "/repo".into(),
                prefix: prefix.into(),
            },
            head: Head::default(),
            entries,
        }
    }

    fn letter(d: &RootDecorations, path: &str) -> Option<char> {
        d.badges.get(path).map(|b| b.letter)
    }

    #[test]
    fn letters_and_folder_tints() {
        use FileChange::*;
        let mut conflict = entry("c/merge.rs", None, None);
        conflict.conflict = Some(Conflict::BothModified);
        let d = root_decorations(&status(
            "",
            vec![
                entry("src/a/new.rs", None, Some(Untracked)),
                entry("src/a/b/lib.rs", None, Some(Modified)),
                entry("src/added.rs", Some(Added), None),
                entry("gone.rs", Some(Deleted), None),
                entry("nested/", None, Some(Untracked)),
                // Staged and changed again: the work tree's letter.
                entry("both.rs", Some(Added), Some(Modified)),
                conflict,
            ],
        ));
        assert_eq!(letter(&d, "src/a/new.rs"), Some('?'));
        assert_eq!(letter(&d, "src/a/b/lib.rs"), Some('M'));
        assert_eq!(letter(&d, "src/added.rs"), Some('A'));
        assert_eq!(letter(&d, "gone.rs"), Some('D'));
        assert_eq!(letter(&d, "nested"), Some('?'));
        assert_eq!(letter(&d, "both.rs"), Some('M'));
        assert_eq!(letter(&d, "c/merge.rs"), Some('U'));
        assert_eq!(letter(&d, "src"), None);
        assert_eq!(d.badges["src/added.rs"].tone, Tone::Green);
        assert_eq!(d.badges["gone.rs"].tone, Tone::Rose);
        assert_eq!(d.badges["src/a/new.rs"].tone, Tone::Subtle);
        assert_eq!(d.badges["src/a/b/lib.rs"].tone, Tone::Sand);
        // The strongest change inside wins, at every level.
        assert_eq!(d.tints.get("src/a/b"), Some(&Tone::Sand));
        assert_eq!(d.tints.get("src/a"), Some(&Tone::Sand));
        assert_eq!(d.tints.get("src"), Some(&Tone::Sand));
        assert_eq!(d.tints.get("c"), Some(&Tone::Rose));
        assert_eq!(d.tints.get(""), Some(&Tone::Rose));
        assert_eq!(d.tints.get("nested"), None);
        assert_eq!(d.tints.len(), 5);
    }

    #[test]
    fn new_files_tint_green_and_lose_to_stronger_changes() {
        use FileChange::*;
        let d = root_decorations(&status(
            "",
            vec![
                entry("docs/new.md", None, Some(Untracked)),
                entry("docs/old.md", Some(Deleted), None),
                entry("tests/t.rs", Some(Added), None),
            ],
        ));
        assert_eq!(d.tints.get("tests"), Some(&Tone::Green));
        assert_eq!(d.tints.get("docs"), Some(&Tone::Rose));
        assert_eq!(d.tints.get(""), Some(&Tone::Rose));
    }

    #[test]
    fn a_rename_is_blue_and_its_old_path_has_no_letter() {
        use FileChange::*;
        let mut renamed = entry("lib/new.rs", Some(Renamed), None);
        renamed.orig_path = Some("src/old.rs".into());
        let d = root_decorations(&status("", vec![renamed]));
        assert_eq!(letter(&d, "lib/new.rs"), Some('R'));
        assert_eq!(d.badges["lib/new.rs"].tone, Tone::Blue);
        assert_eq!(letter(&d, "src/old.rs"), None);
        assert_eq!(d.tints.get("lib"), Some(&Tone::Blue));
    }

    #[test]
    fn a_root_inside_the_work_tree_sees_its_own_paths() {
        use FileChange::*;
        let d = root_decorations(&status(
            "app",
            vec![
                entry("app/src/main.rs", None, Some(Modified)),
                entry("other/x.rs", None, Some(Modified)),
                entry("apple.rs", None, Some(Modified)),
            ],
        ));
        assert_eq!(d.badges.len(), 1);
        assert_eq!(letter(&d, "src/main.rs"), Some('M'));
        assert_eq!(d.tints.get("src"), Some(&Tone::Sand));
        assert_eq!(d.tints.get(""), Some(&Tone::Sand));
    }
}
