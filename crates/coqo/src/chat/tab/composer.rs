// SPDX-License-Identifier: Apache-2.0

//! The chat composer (board 1): the prompt field, its completions and the
//! footer row of chips.
//!
//! - **@-mentions:** `@` opens a popover of fuzzy file matches from the
//!   session's workspace (walked with `fs/list`, at most [`MAX_FILES`]).
//!   Picking one inserts an atomic chip. On send each chip becomes an ACP
//!   `resource_link`, or an embedded `resource` with the file's text when the
//!   provider supports `embeddedContext` and the file is at most
//!   [`EMBED_LIMIT`] bytes.
//! - **Slash commands:** `/` at the start opens the session's
//!   `available_commands` (the agent's commands and the user's own skills).
//!   Tab or Enter completes `/name `; it is sent as text, as ACP expects.
//! - **Provider, settings, mode, model:** until the first prompt the
//!   provider chip is a dropdown, and picking another provider replaces the
//!   chat's empty session with a new one on that provider, in the same tab.
//!   After the first prompt it is a label: start a new chat to use another
//!   provider. Then one chip per session config option, in the agent's
//!   order (`sessions/setConfigOption`): a dropdown for a select (the model
//!   as "Opus", others as "Effort: High"), a toggle for a boolean ("Fast
//!   mode"). The mode chip is a dropdown (`sessions/setMode`) unless a
//!   `mode` config option stands in for it. Without a `model` option the
//!   model is a label, from the provider's usage reports.
//! - **Keys:** while a popover is open, Up and Down choose, Enter and Tab
//!   accept and Esc closes it. Up in an empty composer recalls the last
//!   prompt; Up and Down then step through the earlier ones.

use std::{
    collections::{HashSet, VecDeque},
    ops::Range,
    time::{Duration, Instant},
};

use coqo_protocol::methods::{
    agents::{
        AvailableCommand, Close, ConfigOptionKind, ConfigOptionValue, ConfigValue, ContentBlock,
        Create, CreateSessionParams, Prompt, PromptParams, ProviderInfo, SessionConfigOption,
        SessionInfo, SessionParams, SetConfigOption, SetConfigOptionParams, SetMode, SetModeParams,
        config_category,
    },
    codes as app_codes,
    fs::{self, EntryKind, PathParams, ReadParams},
    workspace::{RootId, WorkspaceId, WorkspaceInfo},
};
use coqo_ui::{
    ActiveTokens as _,
    gpui::{
        AnyElement, App, Context, Focusable as _, FontWeight, HighlightStyle, Hsla,
        InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Point,
        SharedString, StatefulInteractiveElement as _, Styled as _, StyledText, Window, deferred,
        div, prelude::FluentBuilder as _, px, relative,
    },
    input::{Mention, MentionSpan, PromptKey},
    widgets::{Icon, TestSupportExt as _, icon, popover, primary, raised, section_label},
};
use futures_util::future::join_all;

use super::{ChatTab, Transcript, billing_label, usage_label};
use crate::{
    chat::{SIGN_IN_HELP, SessionSwitched, diff::file_name, list::ChatList, transcript::Entry},
    daemon::Daemon,
    files::glyph,
    workspace::{KnownWorkspaces, Workspaces},
};

/// At most this many files are indexed for @-mentions.
pub const MAX_FILES: usize = 20_000;
/// Mentioned files up to this size are embedded when the provider can take
/// embedded context.
pub const EMBED_LIMIT: u64 = 64 * 1024;
/// Rows a completion popover shows at once.
const VISIBLE: usize = 8;
/// Matches kept per query.
const MAX_MATCHES: usize = 50;
/// The file index is rebuilt when a popover opens after this long.
const INDEX_TTL: Duration = Duration::from_secs(30);
/// `fs/list` requests in flight while indexing.
const PARALLEL_LISTS: usize = 16;

// ---- state -----------------------------------------------------------------------

/// A file of the workspace, for @-mentions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedFile {
    workspace: WorkspaceId,
    root: RootId,
    root_path: String,
    /// Relative to the root.
    path: String,
    /// What the popover and the chip show: the path, prefixed with the
    /// root's name when the workspace has several roots.
    display: String,
    size: u64,
}

impl IndexedFile {
    fn abs(&self) -> String {
        format!("{}/{}", self.root_path.trim_end_matches('/'), self.path)
    }
}

/// The workspace's files, walked once per [`INDEX_TTL`].
#[derive(Debug, Default)]
pub struct FileIndex {
    workspace: Option<WorkspaceId>,
    files: Vec<IndexedFile>,
    loading: bool,
    built: Option<Instant>,
    truncated: bool,
}

/// A fuzzy file match: the file and the matched byte offsets in its
/// `display`.
#[derive(Clone, Debug)]
pub struct FileMatch {
    file: IndexedFile,
    positions: Vec<usize>,
}

/// What the composer's popover shows.
#[derive(Clone, Debug)]
pub enum Popover {
    /// Files for the `@query` at `range`.
    Mention {
        range: Range<usize>,
        matches: Vec<FileMatch>,
        selected: usize,
    },
    /// Commands for the `/query` at `range` (the first word).
    Command {
        range: Range<usize>,
        matches: Vec<AvailableCommand>,
        selected: usize,
    },
    /// The provider dropdown.
    Provider { selected: usize },
    /// The mode dropdown.
    Mode { selected: usize },
    /// The dropdown of the select config option `id` (model, effort, …).
    Config { id: String, selected: usize },
}

impl Popover {
    fn selected(&self) -> usize {
        match self {
            Self::Mention { selected, .. }
            | Self::Command { selected, .. }
            | Self::Provider { selected }
            | Self::Mode { selected }
            | Self::Config { selected, .. } => *selected,
        }
    }

    fn set_selected(&mut self, ix: usize) {
        match self {
            Self::Mention { selected, .. }
            | Self::Command { selected, .. }
            | Self::Provider { selected }
            | Self::Mode { selected }
            | Self::Config { selected, .. } => *selected = ix,
        }
    }

    fn is_menu(&self) -> bool {
        matches!(
            self,
            Self::Provider { .. } | Self::Mode { .. } | Self::Config { .. }
        )
    }

    /// Whether `self` and `other` are the same dropdown.
    fn same_menu(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Provider { .. }, Self::Provider { .. })
            | (Self::Mode { .. }, Self::Mode { .. }) => true,
            (Self::Config { id: a, .. }, Self::Config { id: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// The entries of the select config option `id`, if the session has it.
fn config_entries<'a>(options: &'a [SessionConfigOption], id: &str) -> &'a [ConfigOptionValue] {
    options
        .iter()
        .find(|o| o.id == id)
        .and_then(|o| match &o.kind {
            ConfigOptionKind::Select { options, .. } => Some(options.as_slice()),
            ConfigOptionKind::Boolean { .. } => None,
        })
        .unwrap_or_default()
}

/// A config option's chip label: the model by itself ("Opus", as board 1),
/// the mode as the mode chip says it ("Mode: Plan"), anything else as
/// "Effort: High".
pub fn config_label(option: &SessionConfigOption) -> String {
    let value = option.value_name();
    if option.is(config_category::MODEL) {
        value
    } else {
        format!("{}: {value}", option.name)
    }
}

/// Where a mentioned file lives, for embedding its contents.
#[derive(Clone, Debug)]
pub struct Target {
    workspace: WorkspaceId,
    root: RootId,
    path: String,
    size: u64,
}

/// A mention ready to send: embedded `text`, or a link.
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub uri: String,
    pub name: String,
    pub size: Option<u64>,
    pub text: Option<String>,
}

// ---- pure helpers ----------------------------------------------------------------

/// Score `candidate` against `query`: every query character in order
/// (smart case: case-sensitive only if the query has an upper-case letter),
/// with bonuses for runs, word and path-segment starts and the file name.
/// Returns the score and the matched byte offsets.
pub fn fuzzy(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).collect();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let smart = query.iter().any(|c| c.is_uppercase());
    let eq = |a: char, b: char| {
        if smart {
            a == b
        } else {
            a.to_lowercase().eq(b.to_lowercase())
        }
    };
    let chars: Vec<(usize, char)> = candidate.char_indices().collect();
    let name_start = candidate.rfind('/').map_or(0, |i| i + 1);
    let mut best: Option<(i64, Vec<usize>)> = None;
    let mut starts = 0;
    for start in 0..chars.len() {
        if !eq(query[0], chars[start].1) {
            continue;
        }
        starts += 1;
        if starts > 24 {
            break;
        }
        let (mut qi, mut score, mut prev) = (0, 0i64, None::<usize>);
        let mut positions = Vec::with_capacity(query.len());
        for k in start..chars.len() {
            if qi == query.len() {
                break;
            }
            let (byte, c) = chars[k];
            if !eq(query[qi], c) {
                continue;
            }
            score += 1;
            if k > 0 && prev == Some(k - 1) {
                score += 6;
            }
            let boundary = k == 0
                || matches!(chars[k - 1].1, '/' | '_' | '-' | '.' | ' ')
                || (c.is_uppercase() && chars[k - 1].1.is_lowercase());
            if boundary {
                score += 8;
            }
            if byte >= name_start {
                score += 3;
            }
            positions.push(byte);
            prev = Some(k);
            qi += 1;
        }
        if qi < query.len() {
            // Later starts can only match less.
            break;
        }
        let spread = (positions[positions.len() - 1] - positions[0]) as i64;
        score -= spread / 4 + chars.len() as i64 / 10;
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, positions));
        }
    }
    best
}

/// The `@query` the cursor is in: its byte range (from the `@` to the
/// cursor) and the query. The `@` starts a word and isn't part of a chip.
pub fn mention_trigger(
    text: &str,
    cursor: usize,
    mentions: &[MentionSpan],
) -> Option<(Range<usize>, String)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    if !before[..at].is_empty() && !before[..at].ends_with(|c: char| c.is_whitespace() || c == '(')
    {
        return None;
    }
    if mentions
        .iter()
        .any(|m| m.range.start <= at && at < m.range.end)
    {
        return None;
    }
    Some((at..cursor, query.to_owned()))
}

/// The `/query` the cursor is in, if the text starts with `/` and the cursor
/// is still in the first word: the word's byte range and the query.
pub fn command_trigger(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    if !text.starts_with('/') || cursor == 0 {
        return None;
    }
    let before = text.get(..cursor)?;
    if before.chars().any(char::is_whitespace) {
        return None;
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    Some((0..end, before[1..].to_owned()))
}

/// The commands matching `query`: name prefixes first, then fuzzy matches.
pub fn match_commands(commands: &[AvailableCommand], query: &str) -> Vec<AvailableCommand> {
    let lower = query.to_lowercase();
    let mut scored: Vec<(i64, &AvailableCommand)> = commands
        .iter()
        .filter_map(|c| {
            if c.name.to_lowercase().starts_with(&lower) {
                Some((10_000 - c.name.len() as i64, c))
            } else {
                fuzzy(query, &c.name).map(|(s, _)| (s, c))
            }
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, c)| c.clone())
        .collect()
}

/// A `file://` URI for an absolute path.
pub fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// The path of a `file://` URI.
pub fn uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = rest
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// The prompt's content blocks: the text, then one block per attachment.
pub fn prompt_blocks(text: &str, attachments: Vec<Attachment>) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::text(text)];
    for a in attachments {
        blocks.push(match a.text {
            Some(text) => ContentBlock::Resource {
                uri: a.uri,
                mime_type: None,
                text,
            },
            None => ContentBlock::ResourceLink {
                uri: a.uri,
                name: a.name,
                mime_type: None,
                size: a.size,
            },
        });
    }
    blocks
}

/// An earlier prompt as composer content: its text, with its file
/// references (`@path` words that a resource block points at) as chips.
pub fn recall_content(content: &[ContentBlock]) -> (String, Vec<MentionSpan>) {
    let mut text = String::new();
    let mut uris = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::ResourceLink { uri, .. } | ContentBlock::Resource { uri, .. } => {
                if let Some(path) = uri_path(uri) {
                    uris.push((uri.clone(), path));
                }
            }
            ContentBlock::Image { .. } => {}
        }
    }
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(off) = text[at..].find('@') {
        let start = at + off;
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |e| start + e);
        at = end.max(start + 1);
        let starts_word = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '(');
        let rest = &text[start + 1..end];
        if !starts_word || rest.is_empty() {
            continue;
        }
        let suffix = format!("/{rest}");
        if let Some((uri, _)) = uris.iter().find(|(_, path)| path.ends_with(&suffix)) {
            spans.push(MentionSpan {
                range: start..end,
                mention: Mention {
                    id: uri.clone().into(),
                    text: text[start..end].to_owned().into(),
                    label: format!("@{}", file_name(rest)).into(),
                },
            });
        }
    }
    (text, spans)
}

impl FileIndex {
    /// Up to [`MAX_MATCHES`] files for `query`, best first. An empty query
    /// lists the shallowest files.
    fn matches(&self, query: &str) -> Vec<FileMatch> {
        if query.is_empty() {
            return self
                .files
                .iter()
                .take(MAX_MATCHES)
                .map(|f| FileMatch {
                    file: f.clone(),
                    positions: Vec::new(),
                })
                .collect();
        }
        let mut scored: Vec<(i64, &IndexedFile, Vec<usize>)> = self
            .files
            .iter()
            .filter_map(|f| fuzzy(query, &f.display).map(|(s, p)| (s, f, p)))
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.display.len().cmp(&b.1.display.len()))
                .then_with(|| a.1.display.cmp(&b.1.display))
        });
        scored
            .into_iter()
            .take(MAX_MATCHES)
            .map(|(_, f, positions)| FileMatch {
                file: f.clone(),
                positions,
            })
            .collect()
    }
}

/// Walk the workspace's roots breadth first with `fs/list`, skipping ignored
/// entries and symlinked folders. Returns the files (shallowest first) and
/// whether the walk stopped at [`MAX_FILES`].
async fn walk(
    daemon: coqo_ui::gpui::Entity<Daemon>,
    workspace: WorkspaceInfo,
    cx: &mut coqo_ui::gpui::AsyncApp,
) -> (Vec<IndexedFile>, bool) {
    let several = workspace.roots.len() > 1;
    let mut queue: VecDeque<(usize, String)> = (0..workspace.roots.len())
        .map(|ix| (ix, String::new()))
        .collect();
    let mut files = Vec::new();
    while !queue.is_empty() {
        let batch: Vec<(usize, String)> = queue.drain(..queue.len().min(PARALLEL_LISTS)).collect();
        let requests: Vec<_> = cx.update(|cx| {
            batch
                .iter()
                .map(|(root, path)| {
                    daemon.read(cx).request::<fs::ListDir>(PathParams {
                        workspace: workspace.id.clone(),
                        root: workspace.roots[*root].id,
                        path: path.clone(),
                    })
                })
                .collect()
        });
        for ((root_ix, dir), result) in batch.into_iter().zip(join_all(requests).await) {
            let Ok(list) = result else { continue };
            let root = &workspace.roots[root_ix];
            for entry in list.entries {
                if entry.ignored {
                    continue;
                }
                let path = if dir.is_empty() {
                    entry.name
                } else {
                    format!("{dir}/{}", entry.name)
                };
                match entry.kind {
                    EntryKind::Dir if !entry.symlink => queue.push_back((root_ix, path)),
                    EntryKind::File => {
                        if files.len() >= MAX_FILES {
                            return (files, true);
                        }
                        files.push(IndexedFile {
                            workspace: workspace.id.clone(),
                            root: root.id,
                            root_path: root.path.clone(),
                            display: if several {
                                format!("{}/{path}", root.name)
                            } else {
                                path.clone()
                            },
                            path,
                            size: entry.size,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    (files, false)
}

// ---- behaviour -------------------------------------------------------------------

impl ChatTab {
    /// The open workspace the session belongs to (or the current one).
    fn workspace(&self, cx: &App) -> Option<WorkspaceInfo> {
        let id = self.info.as_ref().and_then(|i| i.workspace.as_deref());
        let open = id.and_then(|id| {
            let known = KnownWorkspaces::global(cx)?;
            let known = known.read(cx);
            known.open_workspaces().find(|w| w.id.0 == id).cloned()
        });
        open.or_else(|| Workspaces::global(cx).and_then(|w| w.read(cx).current().cloned()))
    }

    /// The session's provider, as `agents/providers` lists it.
    pub(super) fn provider(&self) -> Option<&ProviderInfo> {
        let id = self.info.as_ref()?.provider.as_str();
        self.providers.iter().find(|p| p.id == id)
    }

    fn has_prompts(transcript: &Transcript) -> bool {
        transcript
            .entries
            .iter()
            .any(|e| matches!(e, Entry::User { .. }))
    }

    /// Whether the provider chip is a dropdown: no prompt sent yet and
    /// another provider to pick.
    pub(super) fn can_switch_provider(&self) -> bool {
        !Self::has_prompts(&self.transcript)
            && !self.transcript.running
            && !self.is_closed()
            && !self.switching
            && self.info.is_some()
            && self.providers.iter().filter(|p| p.available).count() > 1
    }

    /// Open the dropdown of the select config option `id` (screenshots).
    #[cfg(test)]
    pub fn set_preview_config_menu(&mut self, id: &str) {
        let selected = self
            .transcript
            .config_options
            .iter()
            .find(|o| o.id == id)
            .and_then(|o| match &o.kind {
                ConfigOptionKind::Select {
                    current_value,
                    options,
                } => options.iter().position(|e| &e.value == current_value),
                ConfigOptionKind::Boolean { .. } => None,
            })
            .unwrap_or(0);
        self.popover = Some(Popover::Config {
            id: id.into(),
            selected,
        });
    }

    /// Use `paths` under `root` as the workspace's files, without a daemon
    /// (screenshots).
    #[cfg(test)]
    pub fn set_preview_files(&mut self, root: &str, paths: &[&str]) {
        self.files = FileIndex {
            workspace: Some(WorkspaceId("preview".into())),
            files: paths
                .iter()
                .map(|p| IndexedFile {
                    workspace: WorkspaceId("preview".into()),
                    root: RootId(0),
                    root_path: root.into(),
                    path: (*p).into(),
                    display: (*p).into(),
                    size: 1,
                })
                .collect(),
            loading: false,
            built: Some(Instant::now()),
            truncated: false,
        };
    }

    /// Start (or refresh) the file index for @-mentions.
    fn ensure_index(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace(cx) else {
            return;
        };
        let same = self.files.workspace.as_ref() == Some(&workspace.id);
        let fresh = self.files.built.is_some_and(|t| t.elapsed() < INDEX_TTL);
        if same && (self.files.loading || fresh) {
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        if !same {
            self.files = FileIndex::default();
        }
        self.files.workspace = Some(workspace.id.clone());
        self.files.loading = true;
        let id = workspace.id.clone();
        self._index = Some(cx.spawn(async move |this, cx| {
            let (mut files, truncated) = walk(daemon, workspace, cx).await;
            files.sort_by(|a, b| {
                let depth = |f: &IndexedFile| f.display.matches('/').count();
                depth(a)
                    .cmp(&depth(b))
                    .then_with(|| a.display.cmp(&b.display))
            });
            this.update(cx, |this, cx| {
                if this.files.workspace.as_ref() != Some(&id) {
                    return;
                }
                this.files.files = files;
                this.files.truncated = truncated;
                this.files.loading = false;
                this.files.built = Some(Instant::now());
                this.refresh_completion(cx);
            })
            .ok();
        }));
    }

    /// Recompute the popover from the text and the cursor.
    pub(super) fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        let composer = self.composer.read(cx);
        let text = composer.text(cx);
        let cursor = composer.cursor(cx);
        let mentions = composer.mentions(cx);
        let recalled = composer.is_recalled(cx);
        if !recalled {
            self.history = None;
        }
        let previous = self.popover.take();
        let mention = (!recalled)
            .then(|| mention_trigger(&text, cursor, &mentions))
            .flatten();
        let command = (!recalled && mention.is_none())
            .then(|| command_trigger(&text, cursor))
            .flatten();
        let trigger_start = mention.as_ref().or(command.as_ref()).map(|(r, _)| r.start);
        if trigger_start.is_none() {
            self.dismissed = None;
        }
        let dismissed = trigger_start.is_some() && self.dismissed == trigger_start;
        self.popover = if dismissed {
            None
        } else if let Some((range, query)) = mention {
            self.ensure_index(cx);
            let matches = self.files.matches(&query);
            let selected = match previous {
                Some(Popover::Mention {
                    range: r, selected, ..
                }) if r.start == range.start => selected.min(matches.len().saturating_sub(1)),
                _ => 0,
            };
            Some(Popover::Mention {
                range,
                matches,
                selected,
            })
        } else if let Some((range, query)) = command {
            let matches = match_commands(&self.transcript.commands, &query);
            (!self.transcript.commands.is_empty()).then_some(Popover::Command {
                range,
                matches,
                selected: 0,
            })
        } else {
            None
        };
        self.sync_capture(cx);
        cx.notify();
    }

    /// Popover keys go to the tab while the popover has something to pick.
    fn sync_capture(&mut self, cx: &mut Context<Self>) {
        let capture = match &self.popover {
            Some(Popover::Mention { matches, .. }) => !matches.is_empty(),
            Some(Popover::Command { matches, .. }) => !matches.is_empty(),
            Some(_) => true,
            None => false,
        };
        self.composer.update(cx, |c, _| c.set_capture(capture));
    }

    fn popover_len(&self) -> usize {
        match &self.popover {
            Some(Popover::Mention { matches, .. }) => matches.len(),
            Some(Popover::Command { matches, .. }) => matches.len(),
            Some(Popover::Provider { .. }) => self.providers.len(),
            Some(Popover::Mode { .. }) => self
                .transcript
                .modes
                .as_ref()
                .map_or(0, |m| m.available_modes.len()),
            Some(Popover::Config { id, .. }) => {
                config_entries(&self.transcript.config_options, id).len()
            }
            None => 0,
        }
    }

    /// Close the popover. `dismiss` keeps a completion closed until the text
    /// leaves its `@` or `/` word.
    pub(super) fn close_popover(&mut self, dismiss: bool, cx: &mut Context<Self>) {
        if let Some(p) = self.popover.take()
            && dismiss
        {
            self.dismissed = match p {
                Popover::Mention { range, .. } | Popover::Command { range, .. } => {
                    Some(range.start)
                }
                _ => self.dismissed,
            };
        }
        self.sync_capture(cx);
        cx.notify();
    }

    /// A key from the composer: popover navigation, or prompt history.
    pub(super) fn on_composer_key(
        &mut self,
        key: PromptKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.popover.is_none() {
            match key {
                PromptKey::Up => self.recall(-1, window, cx),
                PromptKey::Down => self.recall(1, window, cx),
                _ => {}
            }
            return;
        }
        let len = self.popover_len();
        match key {
            PromptKey::Up | PromptKey::Down if len > 0 => {
                if let Some(p) = &mut self.popover {
                    let step = if key == PromptKey::Up { len - 1 } else { 1 };
                    p.set_selected((p.selected() + step) % len);
                }
                self.reveal_selected();
                cx.notify();
            }
            PromptKey::Accept | PromptKey::Complete => {
                let ix = self.popover.as_ref().map_or(0, Popover::selected);
                self.pick(ix, window, cx);
            }
            PromptKey::Dismiss => self.close_popover(true, cx),
            _ => {}
        }
    }

    /// Escape from the composer: close the popover, or stop the turn.
    pub(super) fn on_composer_cancel(&mut self, cx: &mut Context<Self>) {
        if self.popover.is_some() {
            self.close_popover(true, cx);
        } else {
            self.cancel(cx);
        }
    }

    /// Pick row `ix` of the popover.
    fn pick(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popover) = self.popover.take() else {
            return;
        };
        self.sync_capture(cx);
        match popover {
            Popover::Mention { range, matches, .. } => {
                // The cursor may have left the word since (Left/Right).
                let composer = self.composer.read(cx);
                let still = mention_trigger(
                    &composer.text(cx),
                    composer.cursor(cx),
                    &composer.mentions(cx),
                )
                .is_some_and(|(r, _)| r == range);
                if let (true, Some(m)) = (still, matches.get(ix)) {
                    self.insert_file_mention(range, &m.file, window, cx);
                }
            }
            Popover::Command { range, matches, .. } => {
                if let Some(command) = matches.get(ix) {
                    let text = format!("/{} ", command.name);
                    self.composer
                        .update(cx, |c, cx| c.replace_range(range, text, window, cx));
                }
            }
            Popover::Provider { .. } => {
                if let Some(p) = self.providers.get(ix).filter(|p| p.available) {
                    let id = p.id.clone();
                    self.switch_provider(id, window, cx);
                }
            }
            Popover::Mode { .. } => {
                let mode = self
                    .transcript
                    .modes
                    .as_ref()
                    .and_then(|m| m.available_modes.get(ix))
                    .map(|m| m.id.clone());
                if let Some(mode) = mode {
                    self.set_mode(mode, cx);
                }
            }
            Popover::Config { id, .. } => {
                let value = config_entries(&self.transcript.config_options, &id)
                    .get(ix)
                    .map(|e| e.value.clone());
                if let Some(value) = value {
                    self.set_config_option(id, ConfigValue::Value(value), cx);
                }
            }
        }
        let focus = self.composer.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn insert_file_mention(
        &mut self,
        range: Range<usize>,
        file: &IndexedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let uri = file_uri(&file.abs());
        let mention = Mention {
            id: uri.clone().into(),
            text: format!("@{}", file.display).into(),
            label: format!("@{}", file_name(&file.path)).into(),
        };
        self.targets.insert(
            uri,
            Target {
                workspace: file.workspace.clone(),
                root: file.root,
                path: file.path.clone(),
                size: file.size,
            },
        );
        self.composer
            .update(cx, |c, cx| c.insert_mention(range, mention, window, cx));
    }

    /// Step through the prompts sent in this chat: `-1` older, `1` newer.
    fn recall(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let prompts: Vec<&Vec<ContentBlock>> = self
            .transcript
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::User { content } => Some(content),
                _ => None,
            })
            .collect();
        let next = match (self.history, step < 0) {
            (_, _) if prompts.is_empty() => return,
            (None, true) => Some(prompts.len() - 1),
            (None, false) => return,
            (Some(ix), true) => Some(ix.saturating_sub(1)),
            (Some(ix), false) => (ix + 1 < prompts.len()).then_some(ix + 1),
        };
        match next {
            Some(ix) => {
                let (text, mentions) = recall_content(prompts[ix]);
                self.history = Some(ix);
                self.composer
                    .update(cx, |c, cx| c.recall(text, mentions, window, cx));
            }
            None => {
                self.history = None;
                self.composer.update(cx, |c, cx| c.set_text("", window, cx));
            }
        }
    }

    /// Send the composer's text, with its mentions as resource blocks.
    pub(super) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let composer = self.composer.read(cx);
        let text = composer.text(cx);
        let mentions = composer.mentions(cx);
        let prompt = text.trim().to_owned();
        if prompt.is_empty() || self.transcript.running || self.is_closed() || self.switching {
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            self.error = Some("coqod isn't connected; the message wasn't sent".into());
            cx.notify();
            return;
        };
        self.popover = None;
        self.history = None;
        self.error = None;
        self.composer.update(cx, |c, cx| {
            c.set_capture(false);
            c.set_text("", window, cx);
        });
        let embed = self
            .provider()
            .and_then(|p| p.capabilities)
            .is_some_and(|c| c.embedded_context);
        let mut seen = HashSet::new();
        let mut planned = Vec::new();
        for span in &mentions {
            let uri = span.mention.id.to_string();
            if !seen.insert(uri.clone()) {
                continue;
            }
            let target = self.targets.get(&uri).cloned();
            let name = uri_path(&uri)
                .map(|p| file_name(&p).to_owned())
                .unwrap_or_else(|| span.mention.label.trim_start_matches('@').to_owned());
            let read = target
                .as_ref()
                .filter(|t| embed && t.size <= EMBED_LIMIT)
                .map(|t| {
                    daemon.read(cx).request::<fs::ReadFile>(ReadParams {
                        workspace: t.workspace.clone(),
                        root: t.root,
                        path: t.path.clone(),
                        max_bytes: Some(EMBED_LIMIT),
                    })
                });
            planned.push((uri, name, target.map(|t| t.size), read));
        }
        let session = self.session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut attachments = Vec::new();
            for (uri, name, size, read) in planned {
                let text = match read {
                    Some(read) => read.await.ok().and_then(|r| r.text),
                    None => None,
                };
                attachments.push(Attachment {
                    uri,
                    name,
                    size,
                    text,
                });
            }
            let content = prompt_blocks(&prompt, attachments);
            let Ok(request) = this.update(cx, |_, cx| {
                daemon
                    .read(cx)
                    .request::<Prompt>(PromptParams { session, content })
            }) else {
                return;
            };
            if let Err(e) = request.await {
                this.update_in(cx, |this, window, cx| {
                    if e.code() == Some(app_codes::AUTH_REQUIRED) {
                        this.transcript.sign_in = Some(e.to_string());
                    } else {
                        this.error = Some(format!("The message wasn't sent: {e}"));
                    }
                    // Give the text back so nothing is lost.
                    if this.composer.read(cx).text(cx).is_empty() {
                        this.composer
                            .update(cx, |c, cx| c.set_content(text, mentions, window, cx));
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// Switch the session's mode.
    pub(super) fn set_mode(&mut self, mode: String, cx: &mut Context<Self>) {
        if self.transcript.modes.as_ref().map(|m| &m.current_mode_id) == Some(&mode) {
            return;
        }
        if let Some(daemon) = Self::daemon(cx) {
            let call = daemon.read(cx).request::<SetMode>(SetModeParams {
                session: self.session.clone(),
                mode_id: mode,
            });
            self.spawn_request(call, "Couldn't switch the mode", cx);
        }
    }

    /// Change one of the session's config options (model, effort, …).
    pub(super) fn set_config_option(
        &mut self,
        config_id: String,
        value: ConfigValue,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self
            .transcript
            .config_options
            .iter()
            .any(|o| o.id == config_id && o.value() == value);
        if unchanged {
            return;
        }
        if let Some(daemon) = Self::daemon(cx) {
            let call = daemon
                .read(cx)
                .request::<SetConfigOption>(SetConfigOptionParams {
                    session: self.session.clone(),
                    config_id,
                    value,
                });
            self.spawn_request(call, "Couldn't change the setting", cx);
        }
    }

    /// Replace the chat's empty session with a new one on `provider`.
    fn switch_provider(&mut self, provider: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_switch_provider() {
            return;
        }
        let Some(info) = self.info.clone() else {
            return;
        };
        if info.provider == provider {
            return;
        }
        let Some(daemon) = Self::daemon(cx) else {
            return;
        };
        let name = self
            .providers
            .iter()
            .find(|p| p.id == provider)
            .map_or_else(|| provider.clone(), |p| p.name.clone());
        self.switching = true;
        self.error = None;
        let create = daemon.read(cx).request::<Create>(CreateSessionParams {
            provider,
            cwd: info.cwd,
            workspace: info.workspace,
            title: None,
            mode: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = create.await;
            this.update_in(cx, |this, window, cx| {
                this.switching = false;
                match result {
                    Ok(info) => this.replace_session(info, window, cx),
                    Err(e) if e.code() == Some(app_codes::AUTH_REQUIRED) => {
                        this.error = Some(format!("{name} needs you to sign in. {SIGN_IN_HELP}"));
                    }
                    Err(e) => this.error = Some(format!("Couldn't switch to {name}: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Show `info`'s session in this tab instead, and close the old one.
    fn replace_session(&mut self, info: SessionInfo, window: &mut Window, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.session, info.id.clone());
        if let Some(daemon) = Self::daemon(cx) {
            let close = daemon
                .read(cx)
                .request::<Close>(SessionParams { session: old });
            self.spawn_request(close, "Couldn't close the empty chat", cx);
        }
        self._updates = None;
        self.transcript = Transcript::default();
        self.expanded.clear();
        self.list.reset(0);
        self.popover = None;
        self.provider_name = self
            .providers
            .iter()
            .find(|p| p.id == info.provider)
            .map(|p| p.name.clone());
        self.info = Some(info.clone());
        if let Some(list) = ChatList::global(cx) {
            list.update(cx, |l, cx| l.upsert(info, cx));
        }
        self.sync_capture(cx);
        self.attach(cx);
        // The tab's key changed: save the layout.
        window.dispatch_action(Box::new(SessionSwitched), cx);
    }

    /// Open (or close) a dropdown.
    fn toggle_menu(&mut self, menu: Popover, window: &mut Window, cx: &mut Context<Self>) {
        let same = self.popover.as_ref().is_some_and(|p| p.same_menu(&menu));
        self.popover = (!same).then_some(menu);
        self.reveal_selected();
        self.sync_capture(cx);
        let focus = self.composer.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Scroll an open dropdown so its selected row shows.
    fn reveal_selected(&self) {
        let Some(menu) = self.popover.as_ref().filter(|p| p.is_menu()) else {
            return;
        };
        let selected = menu.selected();
        // A config option's group labels are rows of the list too.
        let labels = match menu {
            Popover::Config { id, .. } => {
                let entries = config_entries(&self.transcript.config_options, id);
                let mut group = None;
                entries
                    .iter()
                    .take(selected + 1)
                    .filter(|e| {
                        let new = e.group.is_some() && e.group != group;
                        group.clone_from(&e.group);
                        new
                    })
                    .count()
            }
            _ => 0,
        };
        self.menu_scroll.scroll_to_item(selected + labels);
    }
}

// ---- rendering -------------------------------------------------------------------

/// `text` with the bytes at `positions` (offsets into `text` plus `offset`)
/// in `color`.
fn highlighted(text: &str, offset: usize, positions: &[usize], color: Hsla) -> StyledText {
    let style = HighlightStyle {
        color: Some(color),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let ranges: Vec<(Range<usize>, HighlightStyle)> = positions
        .iter()
        .filter_map(|&p| {
            let p = p.checked_sub(offset)?;
            let len = text.get(p..)?.chars().next()?.len_utf8();
            Some((p..p + len, style))
        })
        .collect();
    StyledText::new(text.to_owned()).with_highlights(ranges)
}

/// The first row to show so that `selected` is visible.
fn window_start(selected: usize) -> usize {
    (selected + 1).saturating_sub(VISIBLE)
}

impl ChatTab {
    fn render_row(
        &self,
        id: SharedString,
        ix: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + coqo_ui::gpui::ParentElement + coqo_ui::gpui::Styled + use<> {
        let p = cx.tokens().palette.clone();
        div()
            .id(coqo_ui::gpui::ElementId::Name(id))
            .test_support()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(10.))
            .cursor_pointer()
            .when(selected, |d| d.bg(p.surface_selected.hsla()))
            .on_click(cx.listener(move |this, _, window, cx| this.pick(ix, window, cx)))
    }

    fn render_hint(&self, text: &'static str, cx: &App) -> AnyElement {
        let t = cx.tokens();
        div()
            .px(px(16.))
            .pt(px(4.))
            .pb(px(10.))
            .text_size(t.metrics.text_label)
            .text_color(t.palette.text_subtle.hsla())
            .child(text)
            .into_any_element()
    }

    /// The @-mention or slash-command popover, above the composer.
    fn render_completion(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let card = popover(cx)
            .id("chat-completion")
            .test_support()
            .w_full()
            .text_size(m.text_sm);
        let card = match self.popover.as_ref()? {
            Popover::Mention {
                matches, selected, ..
            } => {
                let mut list = div().flex().flex_col().px(px(6.));
                let start = window_start(*selected);
                for (ix, fm) in matches.iter().enumerate().skip(start).take(VISIBLE) {
                    let display = &fm.file.display;
                    let name_at = display.rfind('/').map_or(0, |i| i + 1);
                    let name = &display[name_at..];
                    let dir = display[..name_at].trim_end_matches('/');
                    list = list.child(
                        self.render_row(
                            format!("chat-mention-option-{ix}").into(),
                            ix,
                            ix == *selected,
                            cx,
                        )
                        .child(glyph::render(name, cx))
                        .child(
                            div()
                                .flex_none()
                                .text_color(p.text.hsla())
                                .child(highlighted(name, name_at, &fm.positions, p.sage.hsla())),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(p.text_subtle.hsla())
                                .child(highlighted(dir, 0, &fm.positions, p.text_muted.hsla())),
                        ),
                    );
                }
                let status = if matches.is_empty() && self.files.loading {
                    Some("Indexing the workspace…")
                } else if matches.is_empty() {
                    Some("No files match")
                } else {
                    None
                };
                card.child(
                    div()
                        .px(px(16.))
                        .pt(px(12.))
                        .pb(px(6.))
                        .child(section_label("Files", cx)),
                )
                .child(list)
                .children(status.map(|s| {
                    div()
                        .id("chat-completion-empty")
                        .test_support()
                        .px(px(16.))
                        .pb(px(10.))
                        .text_color(p.text_subtle.hsla())
                        .child(s)
                }))
                .when(self.files.truncated, |d| {
                    d.child(self.render_hint("Only the first 20,000 files are searched", cx))
                })
                .when(!matches.is_empty(), |d| {
                    d.child(
                        self.render_hint(
                            "↑↓ to choose · Enter or Tab to mention · Esc to close",
                            cx,
                        ),
                    )
                })
            }
            Popover::Command {
                matches, selected, ..
            } => {
                let mut list = div().flex().flex_col().px(px(6.));
                let start = window_start(*selected);
                for (ix, c) in matches.iter().enumerate().skip(start).take(VISIBLE) {
                    list = list.child(
                        self.render_row(
                            format!("chat-command-option-{ix}").into(),
                            ix,
                            ix == *selected,
                            cx,
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(m.font_mono.clone())
                                .text_color(p.text.hsla())
                                .child(format!("/{}", c.name)),
                        )
                        .children(c.input_hint.clone().map(|h| {
                            div()
                                .flex_none()
                                .font_family(m.font_mono.clone())
                                .text_color(p.text_subtle.hsla())
                                .child(h)
                        }))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(p.text_muted.hsla())
                                .child(c.description.clone()),
                        ),
                    );
                }
                card.child(
                    div()
                        .px(px(16.))
                        .pt(px(12.))
                        .pb(px(6.))
                        .child(section_label("Commands and skills", cx)),
                )
                .child(list)
                .when(matches.is_empty(), |d| {
                    d.child(
                        div()
                            .id("chat-completion-empty")
                            .test_support()
                            .px(px(16.))
                            .pb(px(10.))
                            .text_color(p.text_subtle.hsla())
                            .child("No command matches"),
                    )
                })
                .when(!matches.is_empty(), |d| {
                    d.child(
                        self.render_hint(
                            "↑↓ to choose · Enter or Tab to complete · Esc to close",
                            cx,
                        ),
                    )
                })
            }
            _ => return None,
        };
        Some(
            deferred(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(relative(1.))
                    .pb(px(8.))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// A dropdown above its chip.
    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let popover_state = self.popover.as_ref().filter(|p| p.is_menu())?;
        let selected = popover_state.selected();
        let mut card = popover(cx)
            .id("chat-menu")
            .test_support()
            .w(px(300.))
            .py(px(6.))
            .text_size(m.text_sm)
            .on_mouse_down_out(
                cx.listener(|this, e: &coqo_ui::gpui::MouseDownEvent, _, cx| {
                    this.closed_by = Some(e.position);
                    this.close_popover(false, cx);
                }),
            );
        // At most about seven rows with descriptions, then it scrolls.
        let mut list = div()
            .id("chat-menu-list")
            .flex()
            .flex_col()
            .px(px(6.))
            .max_h(px(360.))
            .overflow_y_scroll()
            .track_scroll(&self.menu_scroll);
        let row = |id: String,
                   ix: usize,
                   current: bool,
                   enabled: bool,
                   name: String,
                   detail: Option<String>,
                   this: &Self,
                   cx: &mut Context<Self>| {
            this.render_row(id.into(), ix, ix == selected, cx)
                .items_start()
                .when(!enabled, |d| d.opacity(0.55).cursor_default())
                .child(div().pt(px(2.)).w(px(14.)).flex_none().when(current, |d| {
                    d.child(icon(Icon::Check, px(13.), p.sage.hsla()))
                }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(div().text_color(p.text.hsla()).child(name))
                        .children(detail.map(|d| {
                            div()
                                .text_size(m.text_xs)
                                .text_color(p.text_subtle.hsla())
                                .child(d)
                        })),
                )
        };
        match popover_state {
            Popover::Provider { .. } => {
                let current = self.info.as_ref().map(|i| i.provider.clone());
                for (ix, provider) in self.providers.iter().enumerate() {
                    let detail = if provider.available {
                        provider.description.clone()
                    } else {
                        Some("Not available".into())
                    };
                    list = list.child(row(
                        format!("chat-provider-option-{}", provider.id),
                        ix,
                        current.as_deref() == Some(provider.id.as_str()),
                        provider.available,
                        provider.name.clone(),
                        detail,
                        self,
                        cx,
                    ));
                }
                card = card.child(list).child(
                    self.render_hint("Applies to this chat until you send the first message", cx),
                );
            }
            Popover::Mode { .. } => {
                if let Some(modes) = &self.transcript.modes {
                    for (ix, mode) in modes.available_modes.iter().enumerate() {
                        list = list.child(row(
                            format!("chat-mode-option-{}", mode.id),
                            ix,
                            mode.id == modes.current_mode_id,
                            true,
                            mode.name.clone(),
                            mode.description.clone(),
                            self,
                            cx,
                        ));
                    }
                }
                card = card.child(list);
            }
            Popover::Config { id, .. } => {
                let option = self.transcript.config_options.iter().find(|o| &o.id == id);
                let current = option.map(SessionConfigOption::value);
                let mut group = None;
                for (ix, entry) in config_entries(&self.transcript.config_options, id)
                    .iter()
                    .enumerate()
                {
                    // Grouped entries get their group's name above them.
                    if entry.group.is_some() && entry.group != group {
                        group.clone_from(&entry.group);
                        list = list.child(
                            div()
                                .px(px(10.))
                                .pt(px(if ix == 0 { 4. } else { 10. }))
                                .pb(px(4.))
                                .child(section_label(entry.group.clone().unwrap_or_default(), cx)),
                        );
                    }
                    list = list.child(row(
                        format!("chat-config-option-{id}-{}", entry.value),
                        ix,
                        current.as_ref() == Some(&ConfigValue::Value(entry.value.clone())),
                        true,
                        entry.name.clone(),
                        entry.description.clone(),
                        self,
                        cx,
                    ));
                }
                card = card.child(list);
                if let Some(description) = option.and_then(|o| o.description.clone()) {
                    card = card.child(
                        div()
                            .px(px(16.))
                            .pt(px(4.))
                            .pb(px(10.))
                            .text_size(m.text_label)
                            .text_color(p.text_subtle.hsla())
                            .child(description),
                    );
                }
            }
            _ => return None,
        }
        Some(
            deferred(
                div()
                    .absolute()
                    .left_0()
                    .bottom(relative(1.))
                    .pb(px(6.))
                    .child(card),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// A footer chip; `menu` makes it a dropdown trigger.
    fn render_chip(
        &self,
        id: impl Into<SharedString>,
        label: String,
        menu: Option<Popover>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id: SharedString = id.into();
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let open = menu.as_ref().is_some_and(|menu| {
            self.popover
                .as_ref()
                .is_some_and(|open| open.same_menu(menu))
        });
        let chip = raised(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .py(px(5.))
                .rounded(m.radius_control)
                .text_size(m.text_sm)
                .text_color(p.text.hsla()),
            cx,
        )
        .id(id)
        .test_support()
        .child(label);
        let Some(menu) = menu else {
            return chip.into_any_element();
        };
        div()
            .relative()
            .child(
                chip.cursor_pointer()
                    .child(icon(Icon::ChevronDown, px(12.), p.text_muted.hsla()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &coqo_ui::gpui::MouseDownEvent, window, cx| {
                            // Keep the focus in the composer: the tab would take it.
                            cx.stop_propagation();
                            // The click that just closed this menu doesn't reopen it.
                            let closed: Option<Point<Pixels>> = this.closed_by.take();
                            if closed == Some(e.position) {
                                return;
                            }
                            this.toggle_menu(menu.clone(), window, cx);
                        }),
                    ),
            )
            .when(open, |d| d.children(self.render_menu(cx)))
            .into_any_element()
    }

    /// A toggle chip for a boolean config option (Fast mode): sage while on.
    fn render_toggle(&self, option: &SessionConfigOption, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        let on = option.value() == ConfigValue::Boolean(true);
        let glyph = if option.is(config_category::SPEED) {
            Icon::Zap
        } else if on {
            Icon::Check
        } else {
            Icon::Circle
        };
        let base = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(5.))
            .rounded(m.radius_control)
            .text_size(m.text_sm);
        let chip = if on {
            base.bg(p.sage.alpha(0.16))
                .border_1()
                .border_color(p.sage.alpha(0.4))
                .text_color(p.sage.hsla())
        } else {
            raised(base, cx).text_color(p.text_muted.hsla())
        };
        let color = if on {
            p.sage.hsla()
        } else {
            p.text_subtle.hsla()
        };
        let id = option.id.clone();
        chip.id(SharedString::from(format!("chat-config-{}", option.id)))
            .test_support()
            .cursor_pointer()
            .child(icon(glyph, px(12.), color))
            .child(option.name.clone())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &coqo_ui::gpui::MouseDownEvent, _, cx| {
                    // Keep the focus in the composer.
                    cx.stop_propagation();
                    this.set_config_option(id.clone(), ConfigValue::Boolean(!on), cx);
                }),
            )
            .into_any_element()
    }

    /// One chip per config option, in the agent's order: a dropdown for a
    /// select, a toggle for a boolean.
    fn render_config_chips(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let options = self.transcript.config_options.clone();
        options
            .iter()
            .map(|option| match &option.kind {
                ConfigOptionKind::Select {
                    current_value,
                    options: entries,
                } => {
                    let selected = entries
                        .iter()
                        .position(|e| &e.value == current_value)
                        .unwrap_or(0);
                    self.render_chip(
                        format!("chat-config-{}", option.id),
                        config_label(option),
                        Some(Popover::Config {
                            id: option.id.clone(),
                            selected,
                        }),
                        cx,
                    )
                }
                ConfigOptionKind::Boolean { .. } => self.render_toggle(option, cx),
            })
            .collect()
    }

    pub(super) fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.tokens();
        let (p, m) = (t.palette.clone(), t.metrics.clone());
        if self.is_closed() {
            return div()
                .id("chat-closed")
                .test_support()
                .text_size(m.text_sm)
                .text_color(p.text_subtle.hsla())
                .child("This chat is closed. Its transcript stays here to read.")
                .into_any_element();
        }
        let provider = if self.switching {
            "Switching…".to_owned()
        } else {
            self.provider_label()
        };
        let provider_menu = self.can_switch_provider().then(|| Popover::Provider {
            selected: self
                .info
                .as_ref()
                .and_then(|i| self.providers.iter().position(|p| p.id == i.provider))
                .unwrap_or(0),
        });
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .text_size(m.text_sm)
            .text_color(p.text_muted.hsla())
            .child(self.render_chip("chat-provider", provider, provider_menu, cx))
            .children(self.render_config_chips(cx));
        let tr = &self.transcript;
        // A `mode` config option's chip stands in for the mode chip.
        if let (Some(mode), Some(modes), false) =
            (tr.mode_name(), tr.modes.as_ref(), tr.has_mode_option())
        {
            let selected = modes
                .available_modes
                .iter()
                .position(|m| m.id == modes.current_mode_id)
                .unwrap_or(0);
            row = row.child(self.render_chip(
                "chat-mode",
                format!("Mode: {mode}"),
                Some(Popover::Mode { selected }),
                cx,
            ));
        }
        // Without a model option the model is a label, from usage reports.
        if let (None, Some(model)) = (
            tr.model_option(),
            tr.usage.as_ref().and_then(|u| u.model.clone()),
        ) {
            row = row.child(
                div()
                    .id("chat-model")
                    .test_support()
                    .px(px(4.))
                    .child(model),
            );
        }
        if let Some(billing) = billing_label(tr) {
            row = row.child(
                div()
                    .id("chat-billing")
                    .test_support()
                    .text_color(p.text_subtle.hsla())
                    .child(billing),
            );
        }
        if let Some(usage) = usage_label(tr) {
            row = row.child(
                div()
                    .id("chat-usage")
                    .test_support()
                    .text_color(p.text_subtle.hsla())
                    .child(usage),
            );
        }
        row = row.child(div().flex_1());
        let button = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(18.))
            .py(px(7.))
            .rounded(m.radius_control)
            .text_size(m.text_sm)
            .cursor_pointer();
        row = if tr.running {
            row.child(
                raised(button, cx)
                    .text_color(p.text.hsla())
                    .id("chat-stop")
                    .test_support()
                    .child(icon(Icon::Square, px(11.), p.text.hsla()))
                    .child("Stop")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
        } else {
            row.child(
                primary(button, cx)
                    .id("chat-send")
                    .test_support()
                    .child("Send")
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
        };
        let completion = self.render_completion(cx);
        div()
            .id("chat-composer")
            .test_support()
            .relative()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(18.))
            .bg(p.surface.hsla())
            .text_size(px(13.5))
            .child(self.composer.clone())
            .child(row)
            .children(completion)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_file_names_and_segment_starts() {
        let (a, _) = fuzzy("main", "src/main.rs").unwrap();
        let (b, _) = fuzzy("main", "src/domain/lib.rs").unwrap();
        assert!(a > b, "{a} > {b}");
        assert!(fuzzy("xyz", "src/main.rs").is_none());
        let (_, positions) = fuzzy("sm", "src/main.rs").unwrap();
        assert_eq!(positions, [0, 4]);
        // Smart case.
        assert!(fuzzy("Main", "src/main.rs").is_none());
        assert!(fuzzy("main", "src/Main.rs").is_some());
    }

    #[test]
    fn triggers_find_the_word_at_the_cursor() {
        assert_eq!(
            mention_trigger("see @ma", 7, &[]),
            Some((4..7, "ma".into()))
        );
        assert_eq!(mention_trigger("@", 1, &[]), Some((0..1, String::new())));
        assert_eq!(
            mention_trigger("a@b", 3, &[]),
            None,
            "an email isn't a mention"
        );
        assert_eq!(mention_trigger("@ma x", 5, &[]), None);
        assert_eq!(command_trigger("/rev", 4), Some((0..4, "rev".into())));
        assert_eq!(command_trigger("/review now", 3), Some((0..7, "re".into())));
        assert_eq!(command_trigger("/review now", 9), None);
        assert_eq!(command_trigger("x /a", 4), None);
    }

    #[test]
    fn commands_match_by_prefix_first() {
        let cmd = |name: &str| AvailableCommand {
            name: name.into(),
            description: String::new(),
            input_hint: None,
        };
        let commands = [
            cmd("compact"),
            cmd("review"),
            cmd("pr-comments"),
            cmd("context"),
        ];
        let names: Vec<_> = match_commands(&commands, "co")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names[..2], ["compact", "context"]);
        assert_eq!(match_commands(&commands, "").len(), 4);
    }

    #[test]
    fn uris_round_trip() {
        let uri = file_uri("/home/me/my project/a#b.rs");
        assert_eq!(uri, "file:///home/me/my%20project/a%23b.rs");
        assert_eq!(uri_path(&uri).unwrap(), "/home/me/my project/a#b.rs");
    }

    #[test]
    fn a_recalled_prompt_gets_its_chips_back() {
        let content = [
            ContentBlock::text("compare @src/a.rs with @b.rs, mail me@x.org"),
            ContentBlock::ResourceLink {
                uri: "file:///p/src/a.rs".into(),
                name: "a.rs".into(),
                mime_type: None,
                size: None,
            },
            ContentBlock::Resource {
                uri: "file:///p/b.rs".into(),
                mime_type: None,
                text: "fn b() {}".into(),
            },
        ];
        let (text, spans) = recall_content(&content);
        assert_eq!(text, "compare @src/a.rs with @b.rs, mail me@x.org");
        let got: Vec<_> = spans
            .iter()
            .map(|s| (s.range.clone(), s.mention.id.to_string()))
            .collect();
        assert_eq!(got, [(8..17, "file:///p/src/a.rs".to_owned())]);
    }

    #[test]
    fn prompt_blocks_link_or_embed() {
        let blocks = prompt_blocks(
            "look",
            vec![
                Attachment {
                    uri: "file:///p/a.rs".into(),
                    name: "a.rs".into(),
                    size: Some(3),
                    text: None,
                },
                Attachment {
                    uri: "file:///p/b.rs".into(),
                    name: "b.rs".into(),
                    size: Some(3),
                    text: Some("b()".into()),
                },
            ],
        );
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[1], ContentBlock::ResourceLink { name, .. } if name == "a.rs"));
        assert!(matches!(&blocks[2], ContentBlock::Resource { text, .. } if text == "b()"));
    }
}
