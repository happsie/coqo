// SPDX-License-Identifier: Apache-2.0

//! The `file` tab kind, for now a read-only preview: the text from
//! `fs/read` in JetBrains Mono with line numbers, reloaded when the file
//! changes. The editor (#41) replaces this view under the same tab kind.

use std::{ops::Range, sync::Arc};

use coqo_protocol::methods::{
    codes,
    fs::{self, FileTooLarge, ReadParams},
    workspace::{RootId, WorkspaceId},
};
use coqo_ui::{
    ActiveTokens as _,
    dock::{AnyTab, TabItem},
    gpui::{
        AnyElement, App, AppContext as _, Context, Entity, FocusHandle, FontWeight,
        InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
        Styled as _, Subscription, Task, UniformListScrollHandle, Window, div, px, uniform_list,
    },
    serde_json::{Value, json},
    widgets::{Icon, TestSupportExt as _, icon},
};

use crate::{
    daemon::{Daemon, DaemonError, DaemonEvent},
    workspace::{Workspaces, WorkspacesEvent},
};

/// The tab kind. Persisted: never change it.
pub const KIND: &str = "file";

/// Lines longer than this are cut for display.
const MAX_LINE_CHARS: usize = 2_000;
/// Height of one line of code.
const LINE_HEIGHT: f32 = 20.;

/// What the preview shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Loading,
    Text(Arc<Vec<SharedString>>),
    Binary { size: u64 },
    TooLarge { size: u64, limit: u64 },
    Missing,
    Error(String),
}

/// A read-only file tab.
pub struct FilePreview {
    workspace: WorkspaceId,
    root: RootId,
    path: String,
    content: Content,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    _loading: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for FilePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilePreview")
            .field("workspace", &self.workspace)
            .field("root", &self.root)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Recreate a saved file tab in `window`.
pub fn restore(state: &Value, window: &Window, cx: &mut App) -> Option<AnyTab> {
    let workspace = WorkspaceId(state.get("workspace")?.as_str()?.to_owned());
    let root = RootId(u32::try_from(state.get("root")?.as_u64()?).ok()?);
    let path = state.get("path")?.as_str()?.to_owned();
    let workspaces = Workspaces::for_window(window, cx);
    Some(AnyTab::new(cx.new(|cx| {
        FilePreview::new(workspaces.as_ref(), workspace, root, path, cx)
    })))
}

/// The de-duplication key of a file tab.
pub fn key(workspace: &WorkspaceId, root: RootId, path: &str) -> String {
    format!("{workspace}/{root}/{path}")
}

fn lines(text: &str) -> Vec<SharedString> {
    text.lines()
        .map(|line| {
            let line = line.replace('\t', "    ");
            if line.chars().count() > MAX_LINE_CHARS {
                let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
                format!("{cut}…").into()
            } else {
                line.into()
            }
        })
        .collect()
}

impl FilePreview {
    /// A tab for `path`, following changes through the window's `workspaces`.
    pub fn new(
        workspaces: Option<&Entity<Workspaces>>,
        workspace: WorkspaceId,
        root: RootId,
        path: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| {
                if *event == DaemonEvent::Connected {
                    this.reload(cx);
                }
            }));
        }
        if let Some(workspaces) = workspaces {
            subscriptions.push(cx.subscribe(
                workspaces,
                |this, workspaces, event, cx| match event {
                    WorkspacesEvent::CurrentChanged => {
                        let ours =
                            workspaces.read(cx).current().map(|w| &w.id) == Some(&this.workspace);
                        if ours && !matches!(this.content, Content::Text(_)) {
                            this.reload(cx);
                        }
                    }
                    WorkspacesEvent::FsChanged { workspace, changes } => {
                        if *workspace == this.workspace
                            && changes.root == this.root
                            && (changes.rescan || changes.paths.contains(&this.path))
                        {
                            this.reload(cx);
                        }
                    }
                },
            ));
        }
        let mut this = Self {
            workspace,
            root,
            path,
            content: Content::Loading,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _loading: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// What the tab shows.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn content(&self) -> &Content {
        &self.content
    }

    fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let read = daemon.read(cx).request::<fs::ReadFile>(ReadParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: self.path.clone(),
            max_bytes: None,
        });
        self._loading = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |this, cx| this.loaded(result, cx)).ok();
        }));
    }

    fn loaded(&mut self, result: Result<fs::ReadResult, DaemonError>, cx: &mut Context<Self>) {
        self._loading = None;
        self.content = match result {
            Ok(read) => match read.text {
                Some(text) if !read.binary => Content::Text(Arc::new(lines(&text))),
                _ => Content::Binary { size: read.size },
            },
            Err(DaemonError::Rpc(e)) if e.code == codes::FILE_TOO_LARGE => {
                let data = e
                    .data
                    .clone()
                    .and_then(|d| coqo_ui::serde_json::from_value::<FileTooLarge>(d).ok());
                match data {
                    Some(d) => Content::TooLarge {
                        size: d.size,
                        limit: d.limit,
                    },
                    None => Content::TooLarge {
                        size: 0,
                        limit: fs::DEFAULT_READ_LIMIT,
                    },
                }
            }
            Err(e) if e.code() == Some(codes::NOT_FOUND) => Content::Missing,
            // Not reachable yet: keep what's shown; a reconnect reloads.
            Err(DaemonError::NotConnected) => return,
            Err(e) if e.code() == Some(codes::WORKSPACE_CLOSED) => return,
            Err(e) => Content::Error(e.to_string()),
        };
        cx.notify();
    }

    fn render_lines(&self, range: Range<usize>, cx: &App) -> Vec<AnyElement> {
        let Content::Text(lines) = &self.content else {
            return Vec::new();
        };
        let t = cx.tokens();
        let p = &t.palette;
        let digits = lines.len().max(1).to_string().len();
        let gutter = px(14. + 8. * digits as f32);
        range
            .filter_map(|ix| {
                let line = lines.get(ix)?;
                Some(
                    div()
                        .h(px(LINE_HEIGHT))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .w(gutter)
                                .flex_none()
                                .flex()
                                .justify_end()
                                .pr(px(14.))
                                .text_color(p.text_subtle.hsla())
                                .child(SharedString::from((ix + 1).to_string())),
                        )
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_color(p.text.hsla())
                                .child(line.clone()),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024. && unit < UNITS.len() - 1 {
        size /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

impl TabItem for FilePreview {
    fn kind(&self) -> SharedString {
        KIND.into()
    }

    fn title(&self, _: &App) -> SharedString {
        self.file_name().to_owned().into()
    }

    fn key(&self, _: &App) -> Option<SharedString> {
        Some(key(&self.workspace, self.root, &self.path).into())
    }

    fn save(&self, _: &App) -> Value {
        json!({ "workspace": self.workspace.0, "root": self.root.0, "path": self.path })
    }

    fn render_label(&self, _: bool, _: &mut Window, cx: &App) -> Option<AnyElement> {
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(super::glyph::render(self.file_name(), cx))
                .child(self.file_name().to_owned())
                .into_any_element(),
        )
    }

    fn focus_handle(&self, _: &App) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }
}

impl Render for FilePreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let message = |title: String, detail: String| {
            div()
                .id("file-preview-message")
                .test_support()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(icon(Icon::File, px(24.), p.text_subtle.hsla()))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    div()
                        .text_size(m.text_sm)
                        .text_color(p.text_muted.hsla())
                        .child(detail),
                )
                .into_any_element()
        };
        let body = match &self.content {
            Content::Text(lines) => uniform_list(
                "file-preview-lines",
                lines.len(),
                cx.processor(|this, range: Range<usize>, _, cx| this.render_lines(range, cx)),
            )
            .track_scroll(&self.scroll)
            .size_full()
            .py(px(8.))
            .font_family(m.font_mono.clone())
            .text_size(m.text_sm)
            .into_any_element(),
            Content::Loading => message("Loading…".into(), self.path.clone()),
            Content::Binary { size } => message(
                "Binary file".into(),
                format!(
                    "{} isn't text ({}), so it can't be shown.",
                    self.file_name(),
                    human_size(*size)
                ),
            ),
            Content::TooLarge { size, limit } => message(
                "File too large to preview".into(),
                format!(
                    "{} is {}; previews show files up to {}.",
                    self.file_name(),
                    human_size(*size),
                    human_size(*limit)
                ),
            ),
            Content::Missing => message(
                "File not found".into(),
                format!("{} was moved or deleted.", self.path),
            ),
            Content::Error(e) => message("Couldn't open the file".into(), e.clone()),
        };
        div()
            .id("file-preview")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_expand_tabs_and_cut_long_lines() {
        let long = "x".repeat(MAX_LINE_CHARS + 10);
        let text = format!("fn main() {{\n\tprintln!();\n}}\n{long}");
        let lines = lines(&text);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].as_ref(), "    println!();");
        assert_eq!(lines[3].chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(human_size(12), "12 bytes");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
