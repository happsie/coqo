// SPDX-License-Identifier: Apache-2.0

//! Agent-drafted commit messages (PLAN §7.7, #224, ADR 0024).
//!
//! The Git pane's "Draft" button asks an agent for a conventional-commit
//! message for what is staged, and puts the reply in the commit box.
//! Nothing is committed until the user presses Commit.
//!
//! Each draft runs in a short-lived session of its own, so the user's chats
//! stay as they were:
//! - the default provider (the user's own Claude Code over ACP when it is
//!   available, PLAN §7.3), titled "Commit message", with no workspace (so
//!   it isn't listed under CHATS and no workspace permission rule applies);
//! - the mode that asks before every change (Claude Code's `default`,
//!   "Manual"), so anything but an answer is a permission request, and
//!   every one is rejected;
//! - the cheapest settings the agent offers, for this session only: a Haiku
//!   model if there is one, else the lowest effort;
//! - one prompt with the staged diff (`HEAD` → index, capped) and the last
//!   commit subjects, asking for the message only and no tools;
//! - closed when the reply is in, or when the user cancels.

use coqo_protocol::{
    Method,
    methods::{
        agents::{
            self as agm, ConfigOptionKind, ConfigValue, ContentBlock, CreateSessionParams,
            PermissionOptionKind, PermissionOutcome, PermissionRequest, ProviderInfo,
            RespondPermissionParams, SessionConfigOption, SessionId, SessionModes, SessionParams,
            SessionUpdate, StopReason, config_category,
        },
        git::{self as gm, ChangedFile, DiffSide, FileDiff, LineKind},
        workspace::{RootId, WorkspaceId},
    },
};
use coqo_ui::gpui::{
    App, AppContext as _, AsyncApp, Context, EventEmitter, Subscription, Task, WeakEntity,
};

use crate::daemon::{Daemon, DaemonEvent};

/// The provider drafts go to when it is available: Claude Code over ACP.
pub const PREFERRED_PROVIDER: &str = "claude-acp";
/// The draft session's title.
pub const SESSION_TITLE: &str = "Commit message";
/// The mode a draft runs in, when the agent has it: Claude Code's "Manual",
/// which asks before every change. Not `plan`: Claude Code runs plan mode
/// on a model of its own, whatever the session's model says.
const ASKING_MODE: &str = "default";

/// The button's tooltip while the provider list loads.
const LOOKING: &str = "Looking for an agent…";

/// How much diff text a prompt carries at most.
const MAX_DIFF_CHARS: usize = 40_000;
/// How much of one file's diff a prompt carries at most.
const MAX_FILE_CHARS: usize = 8_000;
/// How many files' diffs are fetched at most; the rest are only listed.
const MAX_DIFF_FILES: usize = 60;
/// How many changed files are listed at most.
const MAX_LISTED_FILES: usize = 200;
/// How many recent commit subjects show the repository's style.
const RECENT_COMMITS: u32 = 10;

/// What the drafter is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftState {
    Idle,
    /// A draft is on its way.
    Drafting,
    /// The last draft failed, with why.
    Failed(String),
}

/// Emitted by a [`CommitDrafter`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftEvent {
    /// A draft for `root` came in.
    Drafted { root: RootId, message: String },
}

/// What a draft is for.
#[derive(Clone, Debug)]
pub struct DraftRequest {
    pub workspace: WorkspaceId,
    pub root: RootId,
    /// The agent's working directory: the root's folder.
    pub cwd: String,
}

/// Drafts commit messages for one Git pane.
pub struct CommitDrafter {
    /// The agent providers; `None` until they are known.
    providers: Option<Vec<ProviderInfo>>,
    state: DraftState,
    /// Bumped by every start and cancel: a draft whose generation is gone
    /// was cancelled.
    generation: u64,
    /// The running draft's session, once it exists.
    session: Option<SessionId>,
    _providers: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for CommitDrafter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitDrafter")
            .field("state", &self.state)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<DraftEvent> for CommitDrafter {}

impl CommitDrafter {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(daemon) = Daemon::global(cx) {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event, cx| match event {
                DaemonEvent::Connected => this.load_providers(cx),
                DaemonEvent::Disconnected => cx.notify(),
            }));
        }
        let mut this = Self {
            providers: None,
            state: DraftState::Idle,
            generation: 0,
            session: None,
            _providers: None,
            _subscriptions: subscriptions,
        };
        this.load_providers(cx);
        this
    }

    fn load_providers(&mut self, cx: &mut Context<Self>) {
        let Some(daemon) = Daemon::global(cx).filter(|d| d.read(cx).is_connected()) else {
            return;
        };
        let call = daemon.read(cx).request::<agm::Providers>(());
        self._providers = Some(cx.spawn(async move |this, cx| {
            if let Ok(providers) = call.await {
                this.update(cx, |this, cx| {
                    this.providers = Some(providers);
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    pub fn state(&self) -> &DraftState {
        &self.state
    }

    pub fn is_drafting(&self) -> bool {
        self.state == DraftState::Drafting
    }

    /// The provider a draft goes to: Claude Code when it is available, else
    /// the first available one.
    pub fn provider(&self) -> Option<&ProviderInfo> {
        pick_provider(self.providers.as_deref()?)
    }

    /// Why the button can't draft now, for its tooltip; `None` when it
    /// can. `staged` is the number of staged files.
    pub fn unavailable(&self, staged: usize, cx: &App) -> Option<String> {
        if !Daemon::global(cx).is_some_and(|d| d.read(cx).is_connected()) {
            return Some("coqod isn't connected".into());
        }
        let Some(providers) = &self.providers else {
            return Some(LOOKING.into());
        };
        if pick_provider(providers).is_none() {
            let why = providers
                .iter()
                .find_map(|p| {
                    let status = p.status.as_ref().filter(|s| !s.ready)?;
                    Some(
                        status
                            .message
                            .clone()
                            .unwrap_or_else(|| format!("{} isn't set up yet", p.name)),
                    )
                })
                .unwrap_or_else(|| "No agent provider is installed yet".into());
            return Some(format!("No agent to draft with: {why}"));
        }
        (staged == 0).then(|| "Stage changes to draft a message".into())
    }

    /// Whether a draft may start: the button is enabled, or only the
    /// provider list is still on its way (a draft looks the provider up
    /// itself, so the command works on a pane that was just built).
    pub fn can_start(&self, staged: usize, cx: &App) -> bool {
        match self.unavailable(staged, cx) {
            None => true,
            Some(why) => why == LOOKING && staged > 0,
        }
    }

    /// Start drafting a message for what `request.root` has staged. A draft
    /// already running is cancelled first.
    pub fn start(&mut self, request: DraftRequest, cx: &mut Context<Self>) {
        self.cancel(cx);
        self.generation += 1;
        let generation = self.generation;
        self.state = DraftState::Drafting;
        let root = request.root;
        // Detached, not dropped on cancel: a request already sent (a
        // session being created) must still be seen through, so the task
        // can close what it started.
        cx.spawn(async move |this, cx| {
            let result = draft(&this, generation, request, cx).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.session = None;
                match result {
                    Ok(message) => {
                        this.state = DraftState::Idle;
                        cx.emit(DraftEvent::Drafted { root, message });
                    }
                    Err(err) => this.state = DraftState::Failed(err),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Stop the running draft and close its session. The commit box keeps
    /// what it has.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.state == DraftState::Drafting {
            self.generation += 1;
        }
        // The task closes a session it creates after this; one it made
        // already is closed here.
        if let Some(session) = self.session.take() {
            close(session, cx);
        }
        if matches!(self.state, DraftState::Drafting | DraftState::Failed(_)) {
            self.state = DraftState::Idle;
            cx.notify();
        }
    }
}

/// Close `session` (cancelling its turn), in the background.
fn close(session: SessionId, cx: &App) {
    if let Some(daemon) = Daemon::global(cx) {
        let call = daemon
            .read(cx)
            .request::<agm::Close>(SessionParams { session });
        cx.background_spawn(async move {
            let _ = call.await;
        })
        .detach();
    }
}

fn pick_provider(providers: &[ProviderInfo]) -> Option<&ProviderInfo> {
    providers
        .iter()
        .filter(|p| p.available)
        .find(|p| p.id == PREFERRED_PROVIDER)
        .or_else(|| providers.iter().find(|p| p.available))
}

/// Call `M` on the daemon.
async fn call<M: Method>(params: M::Params, cx: &mut AsyncApp) -> Result<M::Result, String> {
    let call = cx.update(|cx| {
        Daemon::global(cx)
            .filter(|d| d.read(cx).is_connected())
            .map(|d| d.read(cx).request::<M>(params))
    });
    match call {
        Some(call) => call.await.map_err(|e| e.to_string()),
        None => Err("coqod isn't connected".into()),
    }
}

/// Whether draft `generation` still runs.
fn current(this: &WeakEntity<CommitDrafter>, generation: u64, cx: &mut AsyncApp) -> bool {
    this.update(cx, |this, _| this.generation == generation)
        .unwrap_or(false)
}

const CANCELLED: &str = "Cancelled";

async fn draft(
    this: &WeakEntity<CommitDrafter>,
    generation: u64,
    request: DraftRequest,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let prompt = staged_prompt(&request, cx).await?;
    if !current(this, generation, cx) {
        return Err(CANCELLED.into());
    }
    let providers = call::<agm::Providers>((), cx)
        .await
        .map_err(|e| format!("Couldn't find an agent: {e}"))?;
    let provider = pick_provider(&providers)
        .ok_or_else(|| "No agent is available to draft a message".to_owned())?
        .id
        .clone();
    let info = call::<agm::Create>(
        CreateSessionParams {
            provider,
            cwd: request.cwd.clone(),
            workspace: None,
            title: Some(SESSION_TITLE.into()),
            mode: None,
        },
        cx,
    )
    .await
    .map_err(|e| format!("Couldn't start the agent: {e}"))?;
    let session = info.id;
    let tracked = this
        .update(cx, |this, _| {
            let live = this.generation == generation;
            if live {
                this.session = Some(session.clone());
            }
            live
        })
        .unwrap_or(false);
    if !tracked {
        // Cancelled while the session started.
        let _ = call::<agm::Close>(SessionParams { session }, cx).await;
        return Err(CANCELLED.into());
    }
    let reply = converse(&session, prompt, cx).await;
    // Whatever happened, the session is done (closing it again after a
    // cancel is harmless).
    let _ = call::<agm::Close>(SessionParams { session }, cx).await;
    if !current(this, generation, cx) {
        return Err(CANCELLED.into());
    }
    let message = clean_reply(&reply?);
    if message.is_empty() {
        return Err("The agent didn't suggest a message".into());
    }
    Ok(message)
}

/// Set the session up, send `prompt`, and collect the reply.
async fn converse(
    session: &SessionId,
    prompt: String,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let params = || SessionParams {
        session: session.clone(),
    };
    // The provider has started: its modes and config options are in. The
    // mode goes first, as a mode can change the model: the cheap settings
    // are chosen from the options as they are after it.
    let snapshot = call::<agm::Get>(params(), cx).await?;
    let snapshot = match asking_mode(snapshot.modes.as_ref()) {
        Some(mode) => {
            // Best effort: a refused setting leaves the agent's default.
            let _ = call::<agm::SetMode>(
                agm::SetModeParams {
                    session: session.clone(),
                    mode_id: mode,
                },
                cx,
            )
            .await;
            call::<agm::Get>(params(), cx).await?
        }
        None => snapshot,
    };
    for (config_id, value) in cheap_settings(&snapshot.config_options) {
        let _ = call::<agm::SetConfigOption>(
            agm::SetConfigOptionParams {
                session: session.clone(),
                config_id,
                value,
            },
            cx,
        )
        .await;
    }
    let head = call::<agm::Get>(params(), cx).await?.head;
    let subscribe = cx.update(|cx| {
        Daemon::global(cx).map(|d| d.read(cx).subscribe_since::<agm::Updates>(params(), head))
    });
    let mut updates = match subscribe {
        Some(s) => s.await.map_err(|e| e.to_string())?,
        None => return Err("coqod isn't connected".into()),
    };
    call::<agm::Prompt>(
        agm::PromptParams {
            session: session.clone(),
            content: vec![ContentBlock::text(prompt)],
        },
        cx,
    )
    .await
    .map_err(|e| format!("Couldn't ask the agent: {e}"))?;
    let mut reply = String::new();
    while let Some(update) = updates.next().await {
        let update = update.map_err(|e| e.to_string())?;
        match update.event {
            SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text { text },
            } => reply.push_str(&text),
            SessionUpdate::PermissionRequested(request) => {
                // No tools: everything the agent needs is in the prompt.
                let _ = call::<agm::RespondPermission>(
                    RespondPermissionParams {
                        session: session.clone(),
                        request_id: request.id,
                        outcome: reject(&request),
                        remember: false,
                    },
                    cx,
                )
                .await;
            }
            SessionUpdate::TurnEnded {
                stop_reason, error, ..
            } => {
                return match stop_reason {
                    StopReason::EndTurn | StopReason::MaxTokens => Ok(reply),
                    StopReason::Cancelled => Err(CANCELLED.into()),
                    _ => Err(error.unwrap_or_else(|| "The agent stopped early".into())),
                };
            }
            SessionUpdate::Closed => return Err(CANCELLED.into()),
            _ => {}
        }
    }
    Err("Lost the agent's reply".into())
}

/// The answer that turns `request` down: a reject option, else cancelling
/// the request.
fn reject(request: &PermissionRequest) -> PermissionOutcome {
    request
        .options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::RejectOnce)
        .or_else(|| {
            request
                .options
                .iter()
                .find(|o| o.kind == PermissionOptionKind::RejectAlways)
        })
        .map_or(PermissionOutcome::Cancelled, |o| {
            PermissionOutcome::Selected {
                option_id: o.id.clone(),
            }
        })
}

/// The settings that make a draft cheap and quick, for this session only:
/// a Haiku model if the agent offers one, else the lowest effort.
pub fn cheap_settings(options: &[SessionConfigOption]) -> Vec<(String, ConfigValue)> {
    let select = |o: &SessionConfigOption| match &o.kind {
        ConfigOptionKind::Select {
            current_value,
            options,
        } => Some((current_value.clone(), options.clone())),
        ConfigOptionKind::Boolean { .. } => None,
    };
    let model = options
        .iter()
        .filter(|o| o.is(config_category::MODEL) || o.id == "model")
        .find_map(|o| {
            let (current, values) = select(o)?;
            let haiku = values.iter().find(|v| {
                v.value.to_lowercase().contains("haiku") || v.name.to_lowercase().contains("haiku")
            })?;
            (haiku.value != current)
                .then(|| (o.id.clone(), ConfigValue::Value(haiku.value.clone())))
        });
    let has_haiku = model.is_some()
        || options.iter().any(|o| {
            (o.is(config_category::MODEL) || o.id == "model")
                && select(o).is_some_and(|(current, _)| current.to_lowercase().contains("haiku"))
        });
    if has_haiku {
        return model.into_iter().collect();
    }
    options
        .iter()
        .filter(|o| o.is(config_category::THOUGHT_LEVEL))
        .find_map(|o| {
            let (current, values) = select(o)?;
            let low = values
                .iter()
                .find(|v| v.value.eq_ignore_ascii_case("low"))
                .or_else(|| values.first())?;
            (low.value != current).then(|| (o.id.clone(), ConfigValue::Value(low.value.clone())))
        })
        .into_iter()
        .collect()
}

/// The agent's mode that asks before every change, if it has one and isn't
/// in it (the user's default may be one that acts on its own, such as
/// Claude Code's `auto` or `acceptEdits`).
fn asking_mode(modes: Option<&SessionModes>) -> Option<String> {
    let modes = modes?;
    (modes.current_mode_id != ASKING_MODE
        && modes.available_modes.iter().any(|m| m.id == ASKING_MODE))
    .then(|| ASKING_MODE.to_owned())
}

/// The prompt for what `request.root` has staged.
async fn staged_prompt(request: &DraftRequest, cx: &mut AsyncApp) -> Result<String, String> {
    let head = || DiffSide::Rev("HEAD".into());
    let files = call::<gm::DiffFiles>(
        gm::DiffFilesParams {
            workspace: request.workspace.clone(),
            root: request.root,
            base: head(),
            head: DiffSide::Index,
        },
        cx,
    )
    .await
    .map_err(|e| format!("Couldn't read the staged changes: {e}"))?
    .files;
    if files.is_empty() {
        return Err("Nothing is staged".into());
    }
    let mut diffs = Vec::new();
    let mut budget = MAX_DIFF_CHARS;
    for file in files.iter().take(MAX_DIFF_FILES) {
        if budget == 0 {
            break;
        }
        let diff = call::<gm::Diff>(
            gm::DiffParams {
                workspace: request.workspace.clone(),
                root: request.root,
                path: file.path.clone(),
                base: head(),
                head: DiffSide::Index,
                context: None,
            },
            cx,
        )
        .await;
        let Ok(diff) = diff else { continue };
        let text = capped(&unified(&diff), MAX_FILE_CHARS.min(budget));
        budget = budget.saturating_sub(text.len());
        diffs.push(text);
    }
    let commits: Vec<String> = call::<gm::Log>(
        gm::LogParams {
            workspace: request.workspace.clone(),
            root: request.root,
            revs: Vec::new(),
            skip: 0,
            limit: Some(RECENT_COMMITS),
        },
        cx,
    )
    .await
    // A first commit has no history to match.
    .map(|log| log.commits.into_iter().map(|c| c.summary).collect())
    .unwrap_or_default();
    Ok(prompt(&files, &diffs, &commits))
}

/// The words sent to the agent.
pub fn prompt(files: &[ChangedFile], diffs: &[String], commits: &[String]) -> String {
    let mut out = String::from(
        "Write a commit message for the staged changes below.\n\n\
         Reply with the commit message only: no preamble, no explanation, no code fences. \
         Use the Conventional Commits format: `type(scope): summary` on the first line, \
         in the imperative and at most 72 characters, then, only if it helps, a blank line \
         and a short body wrapped at 72 columns. Match the style of the repository's recent \
         commits.\n\n\
         Don't use any tools and don't change any files: everything you need is here.\n",
    );
    if !commits.is_empty() {
        out.push_str("\nRecent commits, newest first:\n");
        for subject in commits {
            out.push_str(&format!("- {subject}\n"));
        }
    }
    let (adds, dels) = files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    out.push_str(&format!(
        "\nStaged files ({}, +{adds} −{dels}):\n",
        files.len()
    ));
    for f in files.iter().take(MAX_LISTED_FILES) {
        out.push_str(&format!(
            "- {} ({}, +{} −{})\n",
            f.path,
            change_word(f.change),
            f.additions,
            f.deletions
        ));
    }
    if files.len() > MAX_LISTED_FILES {
        out.push_str(&format!(
            "- … and {} more\n",
            files.len() - MAX_LISTED_FILES
        ));
    }
    out.push_str("\nThe staged diff");
    if diffs.len() < files.len() {
        out.push_str(&format!(
            " (the first {} files; the rest left out for length)",
            diffs.len()
        ));
    }
    out.push_str(":\n\n```diff\n");
    for d in diffs {
        out.push_str(d);
        if !d.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("```\n");
    out
}

fn change_word(change: gm::FileChange) -> &'static str {
    match change {
        gm::FileChange::Added => "added",
        gm::FileChange::Modified => "modified",
        gm::FileChange::Deleted => "deleted",
        gm::FileChange::TypeChanged => "type changed",
        gm::FileChange::Untracked => "untracked",
    }
}

/// `diff` as `git diff` prints it.
pub fn unified(diff: &FileDiff) -> String {
    let mut out = format!("diff --git a/{0} b/{0}\n", diff.path);
    if diff.binary {
        out.push_str(&format!("Binary file {} changed\n", diff.path));
        return out;
    }
    let old = if diff.old.is_some() {
        format!("a/{}", diff.path)
    } else {
        "/dev/null".into()
    };
    let new = if diff.new.is_some() {
        format!("b/{}", diff.path)
    } else {
        "/dev/null".into()
    };
    out.push_str(&format!("--- {old}\n+++ {new}\n"));
    for hunk in &diff.hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ));
        if let Some(section) = hunk.section.as_deref().filter(|s| !s.is_empty()) {
            out.push(' ');
            out.push_str(section);
        }
        out.push('\n');
        for line in &hunk.lines {
            out.push(match line.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            out.push_str(&line.text);
            out.push('\n');
            if !line.newline {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// `text` cut to about `max` bytes at a line end, saying so.
fn capped(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].rfind('\n').map_or(end, |i| i + 1);
    format!("{}… (cut for length)\n", &text[..cut])
}

/// The commit message in an agent's reply: trimmed, out of a code fence or
/// quotes if it came in one.
pub fn clean_reply(reply: &str) -> String {
    let mut text = reply.trim();
    if let Some(rest) = text.strip_prefix("```") {
        // Skip the fence's info string.
        let rest = rest.split_once('\n').map_or("", |(_, body)| body);
        text = rest.trim_end().strip_suffix("```").unwrap_or(rest).trim();
    }
    if !text.contains('\n') {
        for quote in ['`', '"'] {
            if let Some(inner) = text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) {
                text = inner.trim();
            }
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use coqo_protocol::methods::{
        agents::ConfigOptionValue,
        git::{DiffLine, FileChange, FileSide, Hunk},
    };

    use super::*;

    fn select(id: &str, category: &str, current: &str, values: &[&str]) -> SessionConfigOption {
        SessionConfigOption {
            id: id.into(),
            name: id.into(),
            description: None,
            category: Some(category.into()),
            kind: ConfigOptionKind::Select {
                current_value: current.into(),
                options: values
                    .iter()
                    .map(|v| ConfigOptionValue {
                        value: (*v).into(),
                        name: v.to_uppercase(),
                        description: None,
                        group: None,
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn cheap_settings_prefer_haiku_then_low_effort() {
        let model = |current| {
            select(
                "model",
                config_category::MODEL,
                current,
                &["default", "opus", "claude-haiku-4-5"],
            )
        };
        let effort = select(
            "effort",
            config_category::THOUGHT_LEVEL,
            "high",
            &["low", "medium", "high"],
        );
        assert_eq!(
            cheap_settings(&[model("opus"), effort.clone()]),
            [(
                "model".to_owned(),
                ConfigValue::Value("claude-haiku-4-5".into())
            )]
        );
        // Already on Haiku: nothing to change.
        assert!(cheap_settings(&[model("claude-haiku-4-5"), effort.clone()]).is_empty());
        // No Haiku: the lowest effort.
        let no_haiku = select("model", config_category::MODEL, "opus", &["opus", "sonnet"]);
        assert_eq!(
            cheap_settings(&[no_haiku, effort]),
            [("effort".to_owned(), ConfigValue::Value("low".into()))]
        );
        assert!(cheap_settings(&[]).is_empty());
    }

    #[test]
    fn a_diff_reads_like_git_diff() {
        let line = |kind, text: &str, newline| DiffLine {
            kind,
            old_line: None,
            new_line: None,
            text: text.into(),
            newline,
        };
        let side = Some(FileSide {
            mode: 0o100_644,
            oid: Some("0".repeat(40)),
        });
        let diff = FileDiff {
            path: "src/lib.rs".into(),
            old: side.clone(),
            new: side,
            binary: false,
            additions: 1,
            deletions: 1,
            hunks: vec![Hunk {
                old_start: 1,
                old_lines: 2,
                new_start: 1,
                new_lines: 2,
                section: Some("fn f()".into()),
                lines: vec![
                    line(LineKind::Context, "fn f() {", true),
                    line(LineKind::Removed, "}", true),
                    line(LineKind::Added, "}", false),
                ],
            }],
        };
        assert_eq!(
            unified(&diff),
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n\
             @@ -1,2 +1,2 @@ fn f()\n fn f() {\n-}\n+}\n\\ No newline at end of file\n"
        );
        let added = FileDiff {
            old: None,
            hunks: Vec::new(),
            ..diff
        };
        assert!(unified(&added).contains("--- /dev/null\n+++ b/src/lib.rs\n"));
    }

    #[test]
    fn the_prompt_lists_files_commits_and_the_capped_diff() {
        let files = vec![ChangedFile {
            path: "README.md".into(),
            change: FileChange::Modified,
            additions: 2,
            deletions: 0,
            binary: false,
        }];
        let text = prompt(&files, &["+more".into()], &["feat(ui): a thing".into()]);
        assert!(text.contains("Conventional Commits"));
        assert!(text.contains("Don't use any tools"));
        assert!(text.contains("- feat(ui): a thing\n"));
        assert!(text.contains("- README.md (modified, +2 −0)\n"));
        assert!(text.contains("```diff\n+more\n```"));

        let long = "x\n".repeat(10_000);
        let cut = capped(&long, 100);
        assert!(
            cut.len() < 140 && cut.ends_with("(cut for length)\n"),
            "{cut}"
        );
    }

    #[test]
    fn replies_lose_fences_and_quotes() {
        assert_eq!(clean_reply("  feat: a\n\nbody\n"), "feat: a\n\nbody");
        assert_eq!(clean_reply("```\nfix(x): y\n```"), "fix(x): y");
        assert_eq!(
            clean_reply("```text\nfix(x): y\n\nmore\n```\n"),
            "fix(x): y\n\nmore"
        );
        assert_eq!(clean_reply("`docs: z`"), "docs: z");
        assert_eq!(clean_reply("\"docs: z\""), "docs: z");
    }
}
