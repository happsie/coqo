// SPDX-License-Identifier: Apache-2.0

//! Undo and redo of the tree's file operations (issue #291, [`History`]).
//!
//! - **Rename or move:** moved back (`fs/rename`); open tabs and everything
//!   anchored to the files follow, as for any rename (#288).
//! - **Create:** an entry still empty (an empty file without unsaved edits
//!   in a tab, an empty folder) is deleted for good; one that changed is
//!   deleted to the trash after the user confirms, in the delete card.
//! - **Delete:** restored with the token `fs/delete` gave (`fs/restore`):
//!   from the OS trash, or from Coqo's copy where the trash can't give it
//!   back. Without a token (deleted for good, too large for a copy) it
//!   can't be undone: the menu shows it greyed with the reason, and the key
//!   says so in the toast.
//!
//! Redo does the operation again. Each success shows a toast for
//! [`TOAST_FOR`] ("Undid rename of “a.rs” → “b.rs”"); a failure puts the
//! operation back and shows why above the tree.

use std::time::Duration;

use coqo_protocol::methods::fs::RestoreParams;

use super::*;
use crate::files::{RedoFileOp, UndoFileOp};

/// How long the toast stays.
pub const TOAST_FOR: Duration = Duration::from_secs(4);

/// What an undo or redo step did on disk.
enum Outcome {
    /// Moved to `to`.
    Moved { from: String, to: String },
    /// Made again.
    Created { path: String },
    /// Deleted; how it can be brought back.
    Deleted { restore: Result<String, String> },
    /// Brought back at `path`.
    Restored { path: String },
    /// A creation to undo is still empty: delete it for good.
    Empty,
    /// A creation to undo changed since: ask first (`dirty` tabs).
    Ask { dirty: Vec<String> },
    /// A creation to undo is not there any more.
    Gone,
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

impl FileTree {
    /// The window's history.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn history(&self) -> &Entity<History> {
        &self.history
    }

    /// The toast shown now, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn toast(&self) -> Option<&str> {
        self.toast.as_deref()
    }

    /// Record `op` as if the tree did it.
    #[cfg(test)]
    pub fn record_for_tests(&mut self, op: FileOp, cx: &mut Context<Self>) {
        self.history.update(cx, |h, _| h.record(op));
    }

    pub(super) fn undo(&mut self, _: &UndoFileOp, window: &mut Window, cx: &mut Context<Self>) {
        self.step(true, window, cx);
    }

    pub(super) fn redo(&mut self, _: &RedoFileOp, window: &mut Window, cx: &mut Context<Self>) {
        self.step(false, window, cx);
    }

    /// Show `text` as the toast for a while.
    fn show_toast(&mut self, text: String, cx: &mut Context<Self>) {
        let text: SharedString = text.into();
        self.toast = Some(text.clone());
        self._toast_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_FOR).await;
            this.update(cx, |this, cx| {
                if this.toast.as_ref() == Some(&text) {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// `op` was undone (a confirmed undo of a creation, too): it can be
    /// redone.
    pub(super) fn undone(&mut self, op: FileOp, cx: &mut Context<Self>) {
        let text = format!("Undid {}", op.describe());
        self.history.update(cx, |h, _| h.undone(op));
        self.show_toast(text, cx);
    }

    /// Undo (`undo`) or redo the next operation.
    fn step(&mut self, undo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.undoing || self.editing.is_some() || self.pending_delete.is_some() {
            return;
        }
        self.menu = None;
        let Some((workspace, daemon)) = self.session(cx) else {
            return;
        };
        let taken = if undo {
            self.history.update(cx, |h, _| h.take_undo())
        } else {
            self.history.update(cx, |h, _| h.take_redo())
        };
        let Some(op) = taken else {
            return;
        };
        if undo && let Some(why) = op.cannot_undo() {
            // There's no way back: it leaves the history.
            let text = format!("Can't undo the {}. {why}", op.describe());
            self.show_toast(text, cx);
            return;
        }
        self.undoing = true;
        self.op_error = None;
        let daemon = daemon.read(cx);
        let ws = workspace.clone();
        let step: Task<Result<Outcome, DaemonError>> = match (&op, undo) {
            (FileOp::Move { root, from, to }, _) => {
                let (from, to) = if undo {
                    (to.clone(), from.clone())
                } else {
                    (from.clone(), to.clone())
                };
                let rename = daemon.request::<fs::Rename>(RenameParams {
                    workspace: ws,
                    root: *root,
                    from: from.clone(),
                    to,
                });
                cx.background_spawn(async move {
                    rename.await.map(|r| Outcome::Moved { from, to: r.path })
                })
            }
            (FileOp::Create { root, path, kind }, false) => {
                let create = daemon.request::<fs::Create>(CreateParams {
                    workspace: ws,
                    root: *root,
                    path: path.clone(),
                    kind: *kind,
                });
                cx.background_spawn(async move {
                    create.await.map(|r| Outcome::Created { path: r.path })
                })
            }
            (FileOp::Create { root, path, kind }, true) => {
                let target = Target {
                    root: *root,
                    path: path.clone(),
                    kind: *kind,
                };
                let dirty = self.dirty_tabs(&target, cx);
                let at = PathParams {
                    workspace: ws.clone(),
                    root: *root,
                    path: path.clone(),
                };
                let stat = daemon.request::<fs::Stat>(at.clone());
                let list = (*kind == EntryKind::Dir).then(|| daemon.request::<fs::ListDir>(at));
                let kind = *kind;
                cx.background_spawn(async move {
                    if !dirty.is_empty() {
                        return Ok(Outcome::Ask { dirty });
                    }
                    let stat = match stat.await {
                        Ok(stat) => stat,
                        Err(e) if e.code() == Some(codes::NOT_FOUND) => return Ok(Outcome::Gone),
                        Err(e) => return Err(e),
                    };
                    let empty = match (kind, list) {
                        (EntryKind::Dir, Some(list)) => list.await?.entries.is_empty(),
                        _ => stat.size == 0,
                    };
                    Ok(if stat.kind == kind && empty {
                        Outcome::Empty
                    } else {
                        Outcome::Ask { dirty }
                    })
                })
            }
            (FileOp::Delete { root, path, .. }, true) => {
                let token = match &op {
                    FileOp::Delete {
                        restore: Ok(token), ..
                    } => token.clone(),
                    _ => unreachable!("checked by cannot_undo"),
                };
                let restore = daemon.request::<fs::Restore>(RestoreParams {
                    workspace: ws,
                    root: *root,
                    path: path.clone(),
                    restore: token,
                });
                cx.background_spawn(async move {
                    restore.await.map(|r| Outcome::Restored { path: r.path })
                })
            }
            (FileOp::Delete { root, path, .. }, false) => {
                let delete = daemon.request::<fs::Delete>(DeleteParams {
                    workspace: ws,
                    root: *root,
                    path: path.clone(),
                    permanent: false,
                });
                cx.background_spawn(async move {
                    delete.await.map(|r| Outcome::Deleted {
                        restore: r.restore.ok_or_else(|| r.no_restore.unwrap_or_default()),
                    })
                })
            }
        };
        self.finish_with(workspace, op, undo, step, window, cx);
    }

    /// Run `step`, then [`stepped`](Self::stepped).
    fn finish_with(
        &mut self,
        workspace: WorkspaceId,
        op: FileOp,
        undo: bool,
        step: Task<Result<Outcome, DaemonError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undoing = true;
        self._op = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = step.await;
            this.update_in(cx, |this, window, cx| {
                this.stepped(workspace, op, undo, outcome, window, cx)
            })
            .ok();
        }));
        cx.notify();
    }

    fn stepped(
        &mut self,
        workspace: WorkspaceId,
        op: FileOp,
        undo: bool,
        outcome: Result<Outcome, DaemonError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.undoing = false;
        let what = op.describe();
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                let name = match &op {
                    FileOp::Move { to, from, .. } => base_name(if undo { to } else { from }),
                    FileOp::Create { path, .. } | FileOp::Delete { path, .. } => base_name(path),
                }
                .to_owned();
                let verb = if undo { "undo" } else { "redo" };
                self.op_error = Some(format!(
                    "Couldn't {verb} the {what}. {}",
                    op_message(&e, &name)
                ));
                self.history.update(cx, |h, _| h.put_back(op, undo));
                cx.notify();
                return;
            }
        };
        let root = op.root();
        let done = match (outcome, op) {
            (Outcome::Moved { from, to }, op) => {
                self.apply_move(workspace, root, &from, to, window, cx);
                op
            }
            (Outcome::Created { path }, op) => {
                self.relist((root, parent(&path).to_owned()), cx);
                self.pending_select = Some((root, path));
                window.focus(&self.focus, cx);
                self.rebuild(cx);
                op
            }
            (Outcome::Restored { path }, op) => {
                let dir = (root, parent(&path).to_owned());
                self.expanded.insert(dir.clone());
                self.relist(dir, cx);
                FileEvents::announce(
                    FileEvent::Restored {
                        workspace,
                        root,
                        path: path.clone(),
                    },
                    cx,
                );
                self.pending_select = Some((root, path));
                window.focus(&self.focus, cx);
                self.rebuild(cx);
                op
            }
            (Outcome::Deleted { restore }, op) => {
                let (path, kind) = match &op {
                    FileOp::Create { path, kind, .. } | FileOp::Delete { path, kind, .. } => {
                        (path.clone(), *kind)
                    }
                    FileOp::Move { to, .. } => (to.clone(), EntryKind::File),
                };
                let target = Target { root, path, kind };
                self.apply_delete(workspace, target, window, cx);
                match op {
                    // Redone: a fresh way back.
                    FileOp::Delete {
                        root, path, kind, ..
                    } => FileOp::Delete {
                        root,
                        path,
                        kind,
                        restore,
                    },
                    op => op,
                }
            }
            (Outcome::Ask { dirty }, op) => {
                let FileOp::Create { root, path, kind } = op.clone() else {
                    return;
                };
                self.selected = Some((root, path.clone()));
                self.pending_delete = Some(PendingDelete {
                    target: Target { root, path, kind },
                    dirty,
                    permanent: false,
                    error: None,
                    undo: Some(op),
                    busy: false,
                });
                window.focus(&self.focus, cx);
                cx.notify();
                return;
            }
            (Outcome::Empty, op) => {
                // Nothing in it to lose: gone for good.
                let FileOp::Create { root, path, .. } = &op else {
                    return;
                };
                let Some(daemon) = Daemon::global(cx) else {
                    return;
                };
                let delete = daemon.read(cx).request::<fs::Delete>(DeleteParams {
                    workspace: workspace.clone(),
                    root: *root,
                    path: path.clone(),
                    permanent: true,
                });
                let step = cx.background_spawn(async move {
                    delete.await.map(|r| Outcome::Deleted {
                        restore: r.restore.ok_or_else(|| r.no_restore.unwrap_or_default()),
                    })
                });
                self.finish_with(workspace, op, true, step, window, cx);
                return;
            }
            (Outcome::Gone, op) => op,
        };
        if undo {
            self.undone(done, cx);
        } else {
            let text = format!("Redid {what}");
            self.history.update(cx, |h, _| h.redone(done));
            self.show_toast(text, cx);
        }
    }

    /// The menu's Undo and Redo rows, when there is something to undo or
    /// redo. An operation that can't be undone shows greyed, with why.
    pub(super) fn render_undo_items(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let row =
            |id: &'static str, label: String, key: Option<SharedString>, why: Option<&str>| {
                let enabled = why.is_none();
                let text = if enabled { p.text } else { p.text_subtle };
                div()
                    .id(id)
                    .test_support()
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .mx(px(6.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .text_color(text.hsla())
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(p.surface_selected.hsla()))
                    })
                    .child(icon(Icon::History, px(14.), p.text_muted.hsla()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(label)
                            .children(why.map(|why| {
                                div()
                                    .id("file-menu-undo-why")
                                    .test_support()
                                    .text_size(m.text_xs)
                                    .text_color(p.text_subtle.hsla())
                                    .child(why.to_owned())
                            })),
                    )
                    .children(key.map(|key| {
                        div()
                            .text_size(m.text_xs)
                            .text_color(p.text_subtle.hsla())
                            .child(key)
                    }))
            };
        let mut rows = Vec::new();
        let (next_undo, next_redo) = {
            let h = self.history.read(cx);
            (h.next_undo().cloned(), h.next_redo().cloned())
        };
        if let Some(op) = next_undo {
            let why = op.cannot_undo();
            let label = format!("Undo {}", op.describe());
            let el = row(
                "file-menu-undo",
                label,
                crate::commands::keys_label("files.undo", cx),
                why,
            );
            rows.push(if why.is_none() {
                el.on_click(cx.listener(|this, _, window, cx| this.step(true, window, cx)))
                    .into_any_element()
            } else {
                el.into_any_element()
            });
        }
        if let Some(op) = next_redo {
            let label = format!("Redo {}", op.describe());
            rows.push(
                row(
                    "file-menu-redo",
                    label,
                    crate::commands::keys_label("files.redo", cx),
                    None,
                )
                .on_click(cx.listener(|this, _, window, cx| this.step(false, window, cx)))
                .into_any_element(),
            );
        }
        rows
    }

    /// The toast after an undo or redo, at the bottom of the pane.
    pub(super) fn render_toast(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.toast.clone()?;
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        Some(
            popover(cx)
                .id("file-undo-toast")
                .test_support()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.))
                .mx(px(6.))
                .mb(px(6.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(9.))
                .text_size(m.text_xs)
                .text_color(p.text.hsla())
                .child(icon(Icon::History, px(13.), p.text_muted.hsla()))
                .child(div().flex_1().min_w_0().child(capitalised(&text)))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn capitalises_the_first_letter() {
        assert_eq!(super::capitalised("undid x"), "Undid x");
        assert_eq!(super::capitalised(""), "");
    }
}
