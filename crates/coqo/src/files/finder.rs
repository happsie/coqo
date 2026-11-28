// SPDX-License-Identifier: Apache-2.0

//! The file finder (issue #162): the command palette's default mode, on
//! `Ctrl P` (`Cmd P` on macOS), `Ctrl K` and the top bar's search.
//!
//! The daemon does the searching: it keeps an index of the workspace's
//! non-ignored files and answers `fs/find` with the best matches and the
//! matched positions, so a large repository never crosses the socket. The
//! finder shows each match as the file's glyph, its name with the matched
//! characters in `sage`, and its folder (after the root's name, in a
//! multi-root workspace).
//!
//! - **No query:** the recently opened files of the workspace first
//!   (labelled), then the files nearest the roots.
//! - **A query:** the daemon's ranking, recently opened files a little
//!   ahead.
//! - **`path:line`** (and `path:line:column`) opens the file there.
//! - **Enter** opens the file in the active group (or shows its open tab);
//!   **`Ctrl/Cmd+Enter`** opens it in a split to the right.
//!
//! Recently opened files are every file tab opened, kept in
//! `<data dir>/ui/recent-files.json` (the last 100 across workspaces). They
//! follow the tree's renames and deletes.

use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};

use coqo_protocol::methods::{
    editor::{Position, Range},
    fs::{self, FindMatch, FindParams},
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    dock::{AnyTab, Dock, Edge, TabTarget},
    gpui::{self, App, AppContext as _, Entity, Global, SharedString, Task, Window},
};

use super::{
    editor::{self, FileEditor},
    ops::{FileEvent, FileEvents, is_at_or_under, moved_path},
};
use crate::{
    commands::{self, Command, PaletteItem, PaletteProvider, PaletteScope, palette},
    daemon::Daemon,
    workspace::Workspaces,
};

gpui::actions!(
    file_finder,
    [
        /// Open the file finder (the palette's default mode).
        FindFile,
    ]
);

/// Command id of [`FindFile`].
pub const FIND_FILE: &str = "files.find";

#[cfg(target_os = "macos")]
pub const FIND_FILE_KEY: &str = "cmd-p";
#[cfg(not(target_os = "macos"))]
pub const FIND_FILE_KEY: &str = "ctrl-p";

/// How many matches a query lists.
const LIMIT: u32 = 60;
/// How many recently opened files are kept, across workspaces.
const RECENT_MAX: usize = 100;
/// How many of them lead the list with no query.
const RECENT_SHOWN: usize = 20;
/// While the daemon is still building the index: ask again this often…
const INDEXING_RETRY: Duration = Duration::from_millis(250);
/// …this many times.
const INDEXING_RETRIES: usize = 40;

/// Register the finder: the palette's default mode, its command and key,
/// and the recently opened list. Called by [`super::init`].
pub fn init(cx: &mut App) {
    palette::register_provider(cx, Rc::new(FilesProvider::default()));
    cx.on_action(|_: &FindFile, cx| commands::open_in_active_window("", cx));
    commands::register(
        cx,
        [Command::new(FIND_FILE, "Go to file…", "File", FindFile).key(FIND_FILE_KEY)],
    );
    if !cx.has_global::<RecentFiles>() {
        cx.set_global(RecentFiles::default());
    }
    if let Some(events) = FileEvents::global(cx) {
        cx.subscribe(&events, |_, event: &FileEvent, cx| {
            if cx.has_global::<RecentFiles>() {
                cx.global_mut::<RecentFiles>().follow(event);
            }
        })
        .detach();
    }
}

/// Keep the recently opened files at `path` (and load what is there). Call
/// once at start; tests leave it out.
pub fn keep_recent_in(path: PathBuf, cx: &mut App) {
    let entries: Vec<(String, u32, String)> = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut recent = RecentFiles {
        entries: entries
            .into_iter()
            .map(|(workspace, root, path)| RecentFile {
                workspace: WorkspaceId(workspace),
                root: RootId(root),
                path,
            })
            .collect(),
        path: Some(path),
    };
    recent.entries.truncate(RECENT_MAX);
    cx.set_global(recent);
}

/// One recently opened file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFile {
    pub workspace: WorkspaceId,
    pub root: RootId,
    pub path: String,
}

/// The recently opened files, most recent first. A GPUI global.
#[derive(Debug, Clone, Default)]
pub struct RecentFiles {
    entries: Vec<RecentFile>,
    /// Where they are kept, once set.
    path: Option<PathBuf>,
}

impl Global for RecentFiles {}

impl RecentFiles {
    /// `path` under `root` of `workspace` was opened just now.
    pub fn touch(workspace: &WorkspaceId, root: RootId, path: &str, cx: &mut App) {
        if !cx.has_global::<RecentFiles>() {
            return;
        }
        let recent = cx.global_mut::<RecentFiles>();
        let entry = RecentFile {
            workspace: workspace.clone(),
            root,
            path: path.to_owned(),
        };
        if recent.entries.first() == Some(&entry) {
            return;
        }
        recent.entries.retain(|e| *e != entry);
        recent.entries.insert(0, entry);
        recent.entries.truncate(RECENT_MAX);
        recent.save();
    }

    /// `workspace`'s recently opened files, most recent first.
    pub fn of(workspace: &WorkspaceId, cx: &App) -> Vec<(RootId, String)> {
        cx.try_global::<RecentFiles>()
            .map(|r| {
                r.entries
                    .iter()
                    .filter(|e| e.workspace == *workspace)
                    .map(|e| (e.root, e.path.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Follow a rename or a delete in the tree.
    fn follow(&mut self, event: &FileEvent) {
        match event {
            FileEvent::Moved {
                workspace,
                root,
                from,
                to,
            } => {
                for e in &mut self.entries {
                    if e.workspace == *workspace
                        && e.root == *root
                        && let Some(path) = moved_path(&e.path, from, to)
                    {
                        e.path = path;
                    }
                }
            }
            FileEvent::Deleted {
                workspace,
                root,
                path,
            } => self.entries.retain(|e| {
                e.workspace != *workspace || e.root != *root || !is_at_or_under(&e.path, path)
            }),
            // An undone delete: the files come back to the list as they're
            // opened again.
            FileEvent::Restored { .. } => {}
        }
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let entries: Vec<(&str, u32, &str)> = self
            .entries
            .iter()
            .map(|e| (e.workspace.0.as_str(), e.root.0, e.path.as_str()))
            .collect();
        let saved = serde_json::to_vec(&entries)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(path, json)
            });
        if let Err(e) = saved {
            eprintln!("coqo: cannot save recently opened files: {e}");
        }
    }
}

/// A query split into the path to look for and where to go in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub path: String,
    /// 1-based, as typed.
    pub line: Option<u32>,
    /// 1-based, as typed.
    pub column: Option<u32>,
}

/// Split `text` (`src/main.rs:42`, `main:42:7`, `main:`) into the path and
/// the line and column.
pub fn parse_query(text: &str) -> Query {
    let number = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    // Peel up to two `:number` (or a bare `:`, still being typed) off the end.
    let mut path = text.trim();
    let mut numbers = Vec::new();
    for _ in 0..2 {
        match path.rsplit_once(':') {
            Some((head, tail)) if tail.is_empty() || number(tail).is_some() => {
                numbers.insert(0, number(tail));
                path = head;
            }
            _ => break,
        }
    }
    let (line, column) = match numbers.as_slice() {
        [line, column] => (*line, *column),
        [line] => (*line, None),
        _ => (None, None),
    };
    Query {
        path: path.trim().to_owned(),
        line: line.filter(|&l| l > 0),
        column: column.filter(|&c| c > 0),
    }
}

/// The item id of a file: root, line and path, for [`open_item`].
fn item_id(root: RootId, query: &Query, path: &str) -> SharedString {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{path}",
        root.0,
        query.line.unwrap_or(0),
        query.column.unwrap_or(0)
    )
    .into()
}

/// What [`item_id`] encoded.
fn parse_item_id(id: &str) -> Option<(RootId, Option<u32>, Option<u32>, String)> {
    let mut parts = id.splitn(4, '\u{1f}');
    let root = RootId(parts.next()?.parse().ok()?);
    let line = parts.next()?.parse::<u32>().ok().filter(|&l| l > 0);
    let column = parts.next()?.parse::<u32>().ok().filter(|&c| c > 0);
    Some((root, line, column, parts.next()?.to_owned()))
}

/// The root name to show before folders, in a multi-root workspace.
fn root_names(scope: &PaletteScope, cx: &App) -> Vec<(RootId, SharedString)> {
    let Some(info) = Workspaces::for_handle(scope.window, cx)
        .and_then(|w| w.read(cx).current().cloned())
        .filter(|info| Some(&info.id) == scope.workspace.as_ref())
    else {
        return Vec::new();
    };
    if info.roots.len() < 2 {
        return Vec::new();
    }
    info.roots
        .into_iter()
        .map(|r| (r.id, r.name.into()))
        .collect()
}

/// A palette row for `path`.
fn item(
    root: RootId,
    path: &str,
    positions: &[u32],
    query: &Query,
    roots: &[(RootId, SharedString)],
) -> PaletteItem {
    let (folder, name) = path.rsplit_once('/').unwrap_or(("", path));
    let name_at = path.len() - name.len();
    let title = match query.line {
        Some(line) => format!("{name}:{line}"),
        None => name.to_owned(),
    };
    let mut row = PaletteItem::new(item_id(root, query, path), title);
    row.highlights = positions
        .iter()
        .filter_map(|&p| (p as usize).checked_sub(name_at))
        .collect();
    let root_name = roots.iter().find(|(id, _)| *id == root).map(|(_, n)| n);
    row.detail = match (root_name, folder.is_empty()) {
        (Some(root), true) => Some(root.clone()),
        (Some(root), false) => Some(format!("{root} · {folder}").into()),
        (None, true) => None,
        (None, false) => Some(folder.to_owned().into()),
    };
    row.glyph = Some(name.to_owned().into());
    row
}

/// The rows for `query`: the daemon's `matches`, with `recent` (most recent
/// first) leading an empty query and a little ahead in a ranked one.
fn rows(
    query: &Query,
    matches: Vec<FindMatch>,
    recent: &[(RootId, String)],
    roots: &[(RootId, SharedString)],
) -> Vec<PaletteItem> {
    let recency =
        |root: RootId, path: &str| recent.iter().position(|(r, p)| *r == root && p == path);
    if query.path.is_empty() {
        let mut out: Vec<PaletteItem> = recent
            .iter()
            .take(RECENT_SHOWN)
            .map(|(root, path)| {
                let mut row = item(*root, path, &[], query, roots);
                row.recent = true;
                row
            })
            .collect();
        out.extend(
            matches
                .iter()
                .filter(|m| recency(m.root, &m.path).is_none_or(|r| r >= RECENT_SHOWN))
                .map(|m| item(m.root, &m.path, &[], query, roots)),
        );
        out.truncate(LIMIT as usize);
        return out;
    }
    let mut ranked: Vec<(i64, PaletteItem)> = matches
        .iter()
        .map(|m| {
            let r = recency(m.root, &m.path);
            let bonus = r.map_or(0, |r| 20 - r.min(20) as i64);
            let mut row = item(m.root, &m.path, &m.positions, query, roots);
            row.recent = r.is_some_and(|r| r < RECENT_SHOWN);
            (m.score + bonus, row)
        })
        .collect();
    // Stable: the daemon's order breaks ties.
    ranked.sort_by_key(|a| std::cmp::Reverse(a.0));
    ranked.into_iter().map(|(_, row)| row).collect()
}

/// Open `path` under `root` of `workspace` in `dock`: show its tab if one is
/// open, else open one; with `split`, in a new group to the right. With a
/// `line` (1-based), put the cursor there.
#[allow(clippy::too_many_arguments)]
pub fn open_file(
    dock: &Entity<Dock>,
    workspace: &WorkspaceId,
    root: RootId,
    path: &str,
    line: Option<u32>,
    column: Option<u32>,
    split: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<FileEditor>> {
    let key = editor::key(workspace, root, path);
    let open = dock.read(cx).find_tab(editor::KIND, &key, cx);
    let new_tab = |window: &mut Window, cx: &mut App| {
        let (workspace, path) = (workspace.clone(), path.to_owned());
        cx.new(|cx| FileEditor::new(workspace, root, path, window, cx))
    };
    let tab = match (open, split) {
        (Some(id), false) => {
            dock.update(cx, |dock, cx| dock.activate_tab(id, window, cx));
            RecentFiles::touch(workspace, root, path, cx);
            dock.read(cx)
                .tab(id)
                .and_then(AnyTab::downcast::<FileEditor>)
        }
        (Some(id), true) => {
            // A second view of the open file, beside it.
            dock.update(cx, |dock, cx| {
                dock.activate_tab(id, window, cx);
                dock.split(Edge::Right, window, cx);
            });
            RecentFiles::touch(workspace, root, path, cx);
            let dock = dock.read(cx);
            dock.active_tab()
                .and_then(|id| dock.tab(id))
                .and_then(AnyTab::downcast::<FileEditor>)
        }
        (None, false) => {
            let tab = new_tab(window, cx);
            dock.update(cx, |dock, cx| {
                dock.open_tab(AnyTab::new(tab.clone()), window, cx)
            });
            Some(tab)
        }
        (None, true) => {
            let tab = new_tab(window, cx);
            dock.update(cx, |dock, cx| {
                let target = TabTarget::Split {
                    group: dock.active_group(),
                    edge: Edge::Right,
                };
                dock.open_tab_at(AnyTab::new(tab.clone()), target, window, cx)
            });
            Some(tab)
        }
    }?;
    if let Some(line) = line {
        let at = Position::new(line - 1, column.map_or(0, |c| c - 1));
        tab.update(cx, |tab, cx| {
            tab.reveal(Some(Range::new(at, at)), window, cx)
        });
    }
    Some(tab)
}

/// The finder's palette mode. See the module docs.
#[derive(Debug, Default)]
pub struct FilesProvider {
    /// What the list says when it's empty, for the last search.
    empty: RefCell<SharedString>,
}

impl FilesProvider {
    fn open_item(
        &self,
        item: &PaletteItem,
        scope: &PaletteScope,
        split: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (Some(workspace), Some(dock)) = (scope.workspace.as_ref(), scope.dock.upgrade()) else {
            return;
        };
        let Some((root, line, column, path)) = parse_item_id(&item.id) else {
            return;
        };
        open_file(
            &dock, workspace, root, &path, line, column, split, window, cx,
        );
    }
}

impl PaletteProvider for FilesProvider {
    fn prefix(&self) -> &'static str {
        ""
    }

    fn placeholder(&self) -> SharedString {
        "Search files by name (add :line to go to a line)".into()
    }

    fn empty_text(&self) -> SharedString {
        let empty = self.empty.borrow();
        if empty.is_empty() {
            "No file matches".into()
        } else {
            empty.clone()
        }
    }

    fn confirm_hint(&self) -> SharedString {
        "to open".into()
    }

    fn secondary_hint(&self) -> Option<SharedString> {
        Some("to the side".into())
    }

    fn recent_label(&self) -> SharedString {
        "recently opened".into()
    }

    fn search(&self, query: &str, scope: &PaletteScope, cx: &mut App) -> Task<Vec<PaletteItem>> {
        let (Some(workspace), Some(daemon)) = (scope.workspace.clone(), Daemon::global(cx)) else {
            *self.empty.borrow_mut() = "Open a folder to find its files".into();
            return Task::ready(Vec::new());
        };
        *self.empty.borrow_mut() = "No file matches".into();
        let query = parse_query(query);
        let roots = root_names(scope, cx);
        let recent = RecentFiles::of(&workspace, cx);
        let params = FindParams {
            workspace,
            query: query.path.clone(),
            limit: Some(LIMIT),
        };
        cx.spawn(async move |cx| {
            let mut matches = Vec::new();
            for attempt in 0..=INDEXING_RETRIES {
                let call = cx.update(|cx| daemon.read(cx).request::<fs::Find>(params.clone()));
                match call.await {
                    Ok(found) => {
                        matches = found.matches;
                        if !found.indexing || attempt == INDEXING_RETRIES {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("coqo: fs/find failed: {e}");
                        break;
                    }
                }
                cx.background_executor().timer(INDEXING_RETRY).await;
            }
            rows(&query, matches, &recent, &roots)
        })
    }

    fn confirm(&self, item: &PaletteItem, scope: &PaletteScope, window: &mut Window, cx: &mut App) {
        self.open_item(item, scope, false, window, cx);
    }

    fn confirm_secondary(
        &self,
        item: &PaletteItem,
        scope: &PaletteScope,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.open_item(item, scope, true, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(path: &str, line: Option<u32>, column: Option<u32>) -> Query {
        Query {
            path: path.to_owned(),
            line,
            column,
        }
    }

    #[test]
    fn queries_split_off_a_line_and_column() {
        assert_eq!(parse_query("main.rs"), q("main.rs", None, None));
        assert_eq!(
            parse_query("src/main.rs:42"),
            q("src/main.rs", Some(42), None)
        );
        assert_eq!(parse_query("main:42:7"), q("main", Some(42), Some(7)));
        assert_eq!(parse_query("main:"), q("main", None, None));
        assert_eq!(parse_query("main:42:"), q("main", Some(42), None));
        assert_eq!(parse_query(" main.rs:0 "), q("main.rs", None, None));
        assert_eq!(parse_query("a:b"), q("a:b", None, None));
        assert_eq!(parse_query("a:b:3"), q("a:b", Some(3), None));
        assert_eq!(parse_query(":12"), q("", Some(12), None));
        assert_eq!(parse_query(""), q("", None, None));
    }

    #[test]
    fn item_ids_round_trip() {
        let id = item_id(RootId(3), &q("x", Some(9), None), "a/b:c.rs");
        assert_eq!(
            parse_item_id(&id),
            Some((RootId(3), Some(9), None, "a/b:c.rs".to_owned()))
        );
    }

    #[test]
    fn rows_show_names_folders_roots_and_recent_files() {
        let m = |root: u32, path: &str, score: i64, positions: Vec<u32>| FindMatch {
            root: RootId(root),
            path: path.into(),
            score,
            positions,
        };
        let roots = vec![(RootId(1), "api".into()), (RootId(2), "web".into())];
        let query = q("main", Some(4), None);
        let found = rows(
            &query,
            vec![
                m(1, "src/main.rs", 100, vec![4, 5, 6, 7]),
                m(2, "main.ts", 99, vec![0, 1, 2, 3]),
                m(1, "src/domain.rs", 98, vec![6, 7, 8, 9]),
            ],
            &[(RootId(1), "src/domain.rs".into())],
            &roots,
        );
        let titles: Vec<&str> = found.iter().map(|r| r.title.as_ref()).collect();
        // The recent one moves up past a slightly better match.
        assert_eq!(titles, ["domain.rs:4", "main.rs:4", "main.ts:4"]);
        assert!(found[0].recent);
        assert_eq!(found[1].highlights, [0, 1, 2, 3]);
        assert_eq!(found[1].detail.as_deref(), Some("api · src"));
        assert_eq!(found[2].detail.as_deref(), Some("web"));
        assert_eq!(found[1].glyph.as_deref(), Some("main.rs"));

        // No query: recent first, then the rest without repeats.
        let found = rows(
            &q("", None, None),
            vec![m(1, "a.rs", 0, vec![]), m(1, "b.rs", 0, vec![])],
            &[(RootId(1), "b.rs".into())],
            &[],
        );
        let titles: Vec<(&str, bool)> =
            found.iter().map(|r| (r.title.as_ref(), r.recent)).collect();
        assert_eq!(titles, [("b.rs", true), ("a.rs", false)]);
        assert_eq!(found[0].detail, None);
    }

    #[test]
    fn recent_files_follow_moves_and_deletes() {
        let ws = WorkspaceId("ws".into());
        let entry = |path: &str| RecentFile {
            workspace: ws.clone(),
            root: RootId(1),
            path: path.into(),
        };
        let mut recent = RecentFiles {
            entries: vec![entry("src/a.rs"), entry("src/b.rs"), entry("c.rs")],
            path: None,
        };
        recent.follow(&FileEvent::Moved {
            workspace: ws.clone(),
            root: RootId(1),
            from: "src".into(),
            to: "lib".into(),
        });
        recent.follow(&FileEvent::Deleted {
            workspace: ws.clone(),
            root: RootId(1),
            path: "c.rs".into(),
        });
        assert_eq!(recent.entries, [entry("lib/a.rs"), entry("lib/b.rs")]);
    }
}
