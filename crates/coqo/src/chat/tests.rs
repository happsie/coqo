// SPDX-License-Identifier: Apache-2.0

//! The chat tab and the CHATS list against a daemon served in-process on a
//! temporary socket: the real workspace service plus the real session
//! manager, with `ScriptedProvider` as the agent.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use coqo_agents::{
    AgentUpdate, ManagerOptions, PermissionAsk, ProviderError, SessionManager, SessionStore,
    StaticProviders,
    model::{
        AgentState, AuthKind, AuthStatus, AvailableCommand, ConfigValue, ContentBlock,
        PermissionOption, PermissionOptionKind, PermissionOutcome, PlanEntry, PlanEntryStatus,
        ProviderAction, ProviderActionRun, ProviderCapabilities, ProviderStatus, RuleDecision,
        SessionConfigOption, SessionInfo, SetStatusParams, StatusProgress, StatusState, StopReason,
        ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
        ToolKind,
    },
    testing::{Call, ScriptedProvider, Step, test_config_options, test_modes},
};
use coqo_client::{DaemonPaths, RuntimeDir};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::{Router, Server, ServerConfig};
use coqo_storage::DataDirs;
use coqo_ui::{
    Appearance,
    dock::store::{LayoutStore as _, MemoryLayoutStore},
    gpui::{
        AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Focusable as _,
        Keystroke, Point, Window, WindowBounds, WindowOptions, px, size,
    },
    test::{TestAppContext, TestWindowExt as _},
};
use tempfile::TempDir;
use tokio::{runtime::Runtime, sync::Notify};

use super::{
    KIND,
    list::ChatList,
    tab::{ChatTab, config_label},
    transcript::{Entry, NoticeKind},
};
use crate::{
    daemon::{self, DaemonConfig},
    files::editor,
    shell::{DEFAULT_WORKSPACE, Shell},
    workspace::{self, Workspaces},
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The workspace service and the session manager, served on a socket.
struct TestDaemon {
    // Serves until dropped.
    _runtime: Runtime,
    paths: DaemonPaths,
    manager: SessionManager,
}

impl TestDaemon {
    /// Serve `providers` as `(id, name, agent)`.
    fn start(dir: &Path, providers: &[(&str, &str, &ScriptedProvider)]) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let paths = DaemonPaths::at(dir.join("run"));
        let data = DataDirs::at(dir.join("data"));
        std::fs::create_dir_all(data.root()).unwrap();
        let _entered = runtime.enter();
        let mut registry = StaticProviders::new();
        for (id, name, provider) in providers {
            let info = AgentProviderInfo {
                id: id.parse().unwrap(),
                name: (*name).into(),
                kind: AgentProviderKind::Acp,
                description: None,
            };
            registry = registry.with(info, Arc::new((*provider).clone()));
        }
        let providers = registry;
        let store = SessionStore::open(&data.global_db()).unwrap();
        let manager =
            SessionManager::open(store, Arc::new(providers), ManagerOptions::default()).unwrap();
        let workspaces = Arc::new(coqod::workspace::WorkspaceService::open(data).unwrap());
        let router = coqod::workspace::register(Router::new(), workspaces);
        let router = coqo_agents::rpc::register(router, manager.clone());
        let server = Server::new(router, ServerConfig::new("coqod", "test"));
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        runtime.spawn(coqo_protocol::unix::serve_unix(server, listener));
        drop(_entered);
        Self {
            _runtime: runtime,
            paths,
            manager,
        }
    }
}

/// A project folder, a scripted agent and a daemon.
struct Fixture {
    _tmp: TempDir,
    project: PathBuf,
    provider: ScriptedProvider,
    daemon: TestDaemon,
}

impl Fixture {
    fn new(provider: ScriptedProvider) -> Self {
        Self::with_other(provider, None)
    }

    /// `provider` as "fake", and `other` as a second provider, "other".
    fn with_other(provider: ScriptedProvider, other: Option<&ScriptedProvider>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo-project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
        let project = project.canonicalize().unwrap();
        let mut providers = vec![("fake", "Fake agent", &provider)];
        if let Some(other) = other {
            providers.push(("other", "Other agent", other));
        }
        let daemon = TestDaemon::start(tmp.path(), &providers);
        Self {
            _tmp: tmp,
            project,
            provider,
            daemon,
        }
    }

    fn path(&self, rel: &str) -> String {
        self.project.join(rel).to_string_lossy().into_owned()
    }
}

struct Ui {
    cx: TestAppContext,
    window: AnyWindowHandle,
    shell: Entity<Shell>,
}

fn open_ui(fixture: &Fixture, store: MemoryLayoutStore) -> Ui {
    let cx = TestAppContext::single();
    cx.executor().allow_parking();
    let paths = fixture.daemon.paths.clone();
    let folder = fixture.project.clone();
    let (window, shell) = cx.update(|cx| {
        coqo_ui::init(Appearance::Dark, cx);
        let mut config = DaemonConfig::at(paths);
        config.spawn = false;
        daemon::init(config, cx);
        workspace::init(Some(folder), cx);
        crate::shell::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        coqo_ui::open_window(options, cx, |window, cx| {
            cx.new(|cx| Shell::new(Rc::new(store), DEFAULT_WORKSPACE, window, cx))
        })
        .expect("open shell window")
    });
    let mut ui = Ui { cx, window, shell };
    ui.wait_for("the workspace", |ui| {
        ui.cx
            .read(|cx| Workspaces::global(cx).unwrap().read(cx).current().is_some())
    });
    ui
}

impl Ui {
    fn frame<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        self.cx
            .update_window(self.window, |_, window, cx| {
                window.render_frame(cx);
                f(window, cx)
            })
            .unwrap()
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&mut Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.cx.run_until_parked();
            self.frame(|_, _| ());
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn press(&mut self, keys: &'static str) {
        self.frame(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keys).unwrap(), cx);
        });
    }

    fn chats(&mut self) -> Vec<Entity<ChatTab>> {
        let dock = self.cx.read(|cx| self.shell.read(cx).dock().clone());
        self.cx.read(|cx| {
            let dock = dock.read(cx);
            dock.tree()
                .tabs()
                .into_iter()
                .filter(|t| t.kind == KIND)
                .filter_map(|t| dock.tab(t.id)?.downcast::<ChatTab>())
                .collect()
        })
    }

    fn chat(&mut self) -> Entity<ChatTab> {
        self.wait_for("a chat tab", |ui| !ui.chats().is_empty());
        self.chats().remove(0)
    }

    fn entries(&mut self, chat: &Entity<ChatTab>) -> Vec<Entry> {
        self.cx
            .read(|cx| chat.read(cx).transcript().entries.clone())
    }

    fn listed(&mut self) -> Vec<SessionInfo> {
        self.cx
            .read(|cx| ChatList::global(cx).unwrap().read(cx).sessions().to_vec())
    }

    /// Start a chat with Ctrl/Cmd+Shift+N.
    fn new_chat(&mut self) -> Entity<ChatTab> {
        self.press(if cfg!(target_os = "macos") {
            "cmd-shift-n"
        } else {
            "ctrl-shift-n"
        });
        let chat = self.chat();
        self.wait_for("the chat in the sidebar", |ui| ui.listed().len() == 1);
        chat
    }

    /// Type a prompt into the composer and press Enter.
    fn send(&mut self, chat: &Entity<ChatTab>, text: &str) {
        let composer = self.cx.read(|cx| chat.read(cx).composer().clone());
        let text = text.to_owned();
        self.frame(|window, cx| {
            composer.update(cx, |c, cx| c.set_text(text, window, cx));
            let focus = composer.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        self.press("enter");
    }

    /// Focus the composer and type `text` at the cursor, as keystrokes do.
    fn type_text(&mut self, chat: &Entity<ChatTab>, text: &str) {
        let composer = self.cx.read(|cx| chat.read(cx).composer().clone());
        self.frame(|window, cx| {
            let focus = composer.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            window.input(text, cx);
        });
        self.cx.run_until_parked();
    }

    fn composer_text(&mut self, chat: &Entity<ChatTab>) -> String {
        let composer = self.cx.read(|cx| chat.read(cx).composer().clone());
        self.cx.read(|cx| composer.read(cx).text(cx))
    }

    fn shows(&mut self, id: &str) -> bool {
        let id = name(id);
        self.frame(|window, _| window.try_find(id).is_some())
    }

    fn agent_text(&mut self, chat: &Entity<ChatTab>) -> String {
        self.entries(chat)
            .iter()
            .filter_map(|e| match e {
                Entry::Agent { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn entry_index(&mut self, chat: &Entity<ChatTab>, f: impl Fn(&Entry) -> bool) -> usize {
        self.entries(chat)
            .iter()
            .position(f)
            .expect("the entry is in the transcript")
    }
}

fn name(id: &str) -> ElementId {
    ElementId::Name(id.to_owned().into())
}

fn chunk(text: &str) -> Step {
    Step::Update(AgentUpdate::AgentMessageChunk(ContentBlock::text(text)))
}

fn running(ui: &mut Ui, chat: &Entity<ChatTab>) -> bool {
    ui.cx.read(|cx| chat.read(cx).transcript().running)
}

#[test]
fn streams_the_agent_reply_into_the_tab_and_the_sidebar() {
    let next = Arc::new(Notify::new());
    let provider = ScriptedProvider::new().turn([
        Step::Update(AgentUpdate::AgentThoughtChunk(ContentBlock::text(
            "Reading the code",
        ))),
        chunk("Hello"),
        Step::Update(AgentUpdate::Plan(vec![PlanEntry {
            content: "Say hello".into(),
            priority: Default::default(),
            status: PlanEntryStatus::InProgress,
        }])),
        Step::Wait(next.clone()),
        chunk(", **world**"),
        Step::Stop(StopReason::EndTurn),
    ]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    assert_eq!(ui.listed()[0].provider, "fake");

    ui.send(&chat, "Say hello");
    ui.wait_for("the first chunk and the plan", |ui| {
        ui.agent_text(&chat) == "Hello"
            && ui.cx.read(|cx| !chat.read(cx).transcript().plan.is_empty())
    });
    assert!(running(&mut ui, &chat));
    // The user's prompt, the turn's header, the thought and the message.
    let entries = ui.entries(&chat);
    assert!(
        matches!(&entries[0], Entry::User { content } if content == &[ContentBlock::text("Say hello")]),
        "{entries:?}"
    );
    assert!(matches!(entries[2], Entry::Thought { .. }), "{entries:?}");
    let agent = ui.entry_index(&chat, |e| matches!(e, Entry::Agent { .. }));
    ui.frame(|window, _| {
        assert!(window.try_find(("chat-agent", agent)).is_some());
        assert!(
            window.try_find("chat-stop").is_some(),
            "Stop shows while running"
        );
        assert!(window.try_find(("chat-plan-entry", 0usize)).is_some());
    });
    // The composer was cleared on send.
    let composer = ui.cx.read(|cx| chat.read(cx).composer().clone());
    assert_eq!(ui.cx.read(|cx| composer.read(cx).text(cx)), "");

    next.notify_one();
    ui.wait_for("the rest of the reply", |ui| {
        ui.agent_text(&chat) == "Hello, **world**" && !running(ui, &chat)
    });
    ui.wait_for("the done dot", |ui| {
        ui.listed()[0].state == AgentState::Done
    });
    let row = format!("chat-row-{}", ui.listed()[0].id);
    ui.frame(|window, _| {
        assert!(window.try_find("chat-send").is_some());
        assert!(window.try_find(name(&row)).is_some(), "the sidebar row");
    });
    // The thought starts collapsed and expands on click.
    ui.frame(|window, cx| window.click(("chat-thought", 2usize), cx));
    ui.frame(|window, _| {
        assert!(window.try_find(("chat-thought-body", 2usize)).is_some());
    });
}

#[test]
fn the_session_panel_counts_the_coqo_tools_the_agent_used() {
    let call = |id: &str, name: &str| {
        Step::Update(AgentUpdate::ToolCall(ToolCall {
            id: ToolCallId(id.into()),
            title: name.into(),
            name: Some(name.into()),
            kind: ToolKind::Other,
            status: ToolCallStatus::Completed,
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: None,
        }))
    };
    let provider = ScriptedProvider::new().turn([
        call("t1", "mcp__coqo-editor__editor_context"),
        call("t2", "Read"),
        call("t3", "mcp__coqo-editor__set_status"),
        call("t4", "mcp__coqo-editor__set_status"),
        Step::Stop(StopReason::EndTurn),
    ]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.frame(|window, _| {
        assert!(window.try_find("chat-tool-use").is_none(), "no tools yet");
    });
    ui.send(&chat, "Explain this");
    let line = "Coqo tools: set_status 2 · editor_context 1";
    ui.wait_for("the tool-use line", |ui| {
        ui.cx.read(|cx| chat.read(cx).tool_use_line(cx)).as_deref() == Some(line)
    });
    ui.frame(|window, _| {
        assert!(window.try_find("chat-tool-use").is_some());
    });
}

#[test]
fn tool_calls_show_their_diff_and_open_their_files() {
    let provider = ScriptedProvider::new();
    let fixture = Fixture::new(provider);
    let main = fixture.path("src/main.rs");
    fixture.provider.push_turn([
        Step::Update(AgentUpdate::ToolCall(ToolCall {
            id: ToolCallId("t1".into()),
            title: "Edit src/main.rs".into(),
            name: None,
            kind: ToolKind::Edit,
            status: ToolCallStatus::InProgress,
            content: vec![ToolCallContent::Diff {
                path: main.clone(),
                old_text: Some("fn main() {}\n".into()),
                new_text: "fn main() {\n    println!(\"hi\");\n}\n".into(),
            }],
            locations: vec![ToolCallLocation {
                path: main.clone(),
                line: Some(2),
            }],
            raw_input: None,
            raw_output: None,
        })),
        Step::Update(AgentUpdate::ToolCallUpdate(ToolCallUpdate {
            id: ToolCallId("t1".into()),
            status: Some(ToolCallStatus::Completed),
            ..ToolCallUpdate::default()
        })),
        Step::Stop(StopReason::EndTurn),
    ]);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.send(&chat, "Print hi");
    ui.wait_for("the tool call", |ui| {
        ui.entries(&chat)
            .iter()
            .any(|e| matches!(e, Entry::Tool(t) if t.call.status == ToolCallStatus::Completed))
    });
    let ix = ui.entry_index(&chat, |e| matches!(e, Entry::Tool(_)));
    let Entry::Tool(tool) = ui.entries(&chat)[ix].clone() else {
        unreachable!()
    };
    assert_eq!(tool.counts(), (3, 1));
    let location = format!("chat-location-{ix}-0");
    ui.frame(|window, _| {
        assert!(
            window.try_find(("chat-change", 0usize)).is_some(),
            "the changes panel lists it"
        );
        assert!(
            window.try_find(name(&location)).is_none(),
            "the card starts collapsed"
        );
    });

    // Expand the card, then follow the location into a file tab.
    ui.frame(|window, cx| window.click(("chat-tool", ix), cx));
    ui.frame(|window, cx| window.click(name(&location), cx));
    let ws = ui.cx.read(|cx| {
        Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .unwrap()
            .clone()
    });
    let key = editor::key(&ws.id, ws.roots[0].id, "src/main.rs");
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    ui.wait_for("the file tab", |ui| {
        ui.cx
            .read(|cx| dock.read(cx).find_tab(editor::KIND, &key, cx).is_some())
    });
}

#[test]
fn the_agents_status_shows_in_the_panel_the_list_and_the_tab() {
    let next = Arc::new(Notify::new());
    let provider =
        ScriptedProvider::new().turn([Step::Wait(next.clone()), Step::Stop(StopReason::EndTurn)]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.frame(|window, _| {
        assert!(window.try_find("chat-status").is_some(), "the status card");
        assert!(window.try_find("chat-status-progress").is_none());
    });
    ui.send(&chat, "Fix the tests");
    ui.wait_for("the turn", |ui| running(ui, &chat));
    let id = ui.listed()[0].id.clone();
    let set = |status: &str, state, progress| SetStatusParams {
        session: id.clone(),
        status: status.into(),
        state,
        progress,
    };

    // The agent reports progress: the panel, the list row and the tab follow.
    fixture
        .daemon
        .manager
        .set_status(set(
            "Running the tests",
            None,
            Some(StatusProgress {
                current: 2,
                total: 5,
            }),
        ))
        .unwrap();
    ui.wait_for("the status line", |ui| {
        ui.cx.read(|cx| chat.read(cx).status_line(cx)).as_deref()
            == Some("Running the tests · 2 of 5")
    });
    ui.frame(|window, _| {
        assert!(window.try_find("chat-status-text").is_some());
        assert!(window.try_find("chat-status-progress").is_some());
        assert!(window.try_find(("chat-row-status", 0usize)).is_some());
        assert!(
            window.try_find("chat-tab-badge").is_none(),
            "working needs no badge"
        );
    });
    let row = crate::chat::status_line(&ui.listed()[0]);
    assert_eq!(row.as_deref(), Some("Running the tests · 2 of 5"));

    // Blocked: the tab asks for attention, the row says why.
    fixture
        .daemon
        .manager
        .set_status(set("Need DATABASE_URL", Some(StatusState::Blocked), None))
        .unwrap();
    ui.wait_for("blocked", |ui| ui.listed()[0].state == AgentState::Blocked);
    ui.frame(|window, _| {
        assert!(
            window.try_find("chat-tab-badge").is_some(),
            "the blocked badge"
        );
        assert!(window.try_find("chat-status-progress").is_none());
    });
    assert_eq!(
        crate::chat::status_line(&ui.listed()[0]).as_deref(),
        Some("Need DATABASE_URL")
    );

    // The turn ends: blocked stays, with its status.
    next.notify_one();
    ui.wait_for("the end of the turn", |ui| !running(ui, &chat));
    assert_eq!(ui.listed()[0].state, AgentState::Blocked);
    assert_eq!(
        ui.cx.read(|cx| chat.read(cx).status_line(cx)).as_deref(),
        Some("Need DATABASE_URL")
    );
}

#[test]
fn a_tool_calls_terminal_opens_in_a_terminal_tab() {
    let fixture = Fixture::new(ScriptedProvider::new());
    fixture.provider.push_turn([
        Step::Update(AgentUpdate::ToolCall(ToolCall {
            id: ToolCallId("t1".into()),
            title: "cargo test".into(),
            name: None,
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            // An agent's `terminal/create`, mapped to daemon terminal 7.
            content: vec![ToolCallContent::Terminal {
                terminal_id: "7".into(),
            }],
            locations: vec![],
            raw_input: None,
            raw_output: None,
        })),
        Step::Stop(StopReason::EndTurn),
    ]);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    ui.cx.update(|cx| {
        let handle = crate::daemon::Daemon::global(cx).unwrap().read(cx).handle();
        crate::terminal::init(Rc::new(crate::terminal::DaemonBackend::new(handle)), cx);
    });
    let chat = ui.new_chat();
    ui.send(&chat, "Run the tests");
    ui.wait_for("the tool call", |ui| {
        ui.entries(&chat)
            .iter()
            .any(|e| matches!(e, Entry::Tool(_)))
    });
    let ix = ui.entry_index(&chat, |e| matches!(e, Entry::Tool(_)));
    // The link shows on the collapsed card.
    let link = format!("chat-terminal-{ix}-0");
    ui.wait_for("the terminal link", |ui| ui.shows(&link));

    ui.frame(|window, cx| window.click(name(&link), cx));
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    let terminal_tab = |ui: &mut Ui| {
        ui.cx.read(|cx| {
            dock.read(cx)
                .find_tab(crate::terminal::KIND, "terminal:7", cx)
        })
    };
    ui.wait_for("the terminal tab", |ui| terminal_tab(ui).is_some());
    let active = ui.cx.read(|cx| dock.read(cx).active_tab());
    assert_eq!(active, terminal_tab(&mut ui), "it is in front");
}

fn ask() -> PermissionAsk {
    PermissionAsk {
        title: "Run cargo test".into(),
        tool_call: None,
        options: vec![
            PermissionOption {
                id: "reject".into(),
                name: "Deny".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
            PermissionOption {
                id: "allow-once".into(),
                name: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
        ],
    }
}

#[test]
fn permission_buttons_answer_with_the_chosen_option() {
    let provider = ScriptedProvider::new().turn([
        Step::Permission(ask()),
        chunk("Tests pass"),
        Step::Stop(StopReason::EndTurn),
    ]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.send(&chat, "Run the tests");
    ui.wait_for("the permission request", |ui| {
        ui.entries(&chat)
            .iter()
            .any(|e| matches!(e, Entry::Permission { outcome: None, .. }))
    });
    ui.wait_for("the needs-permission dot", |ui| {
        ui.listed()[0].state == AgentState::NeedsPermission
    });
    ui.frame(|window, _| {
        assert!(
            window.try_find("chat-tab-badge").is_some(),
            "the tab says it needs you"
        );
    });
    let ix = ui.entry_index(&chat, |e| matches!(e, Entry::Permission { .. }));
    let button = format!("chat-permission-{ix}-allow-once");
    ui.frame(|window, cx| window.click(name(&button), cx));
    ui.wait_for("the answer", |ui| ui.agent_text(&chat) == "Tests pass");
    assert_eq!(
        fixture.provider.outcomes(),
        [PermissionOutcome::Selected {
            option_id: "allow-once".into()
        }]
    );
    assert!(matches!(
        &ui.entries(&chat)[ix],
        Entry::Permission { outcome: Some(PermissionOutcome::Selected { option_id }), .. }
            if option_id == "allow-once"
    ));
}

/// A request to run `command`, as an ACP agent sends it, with every kind
/// of option.
fn run(id: &str, command: &str) -> Step {
    let option = |id: &str, name: &str, kind| PermissionOption {
        id: id.into(),
        name: name.into(),
        kind,
    };
    Step::Permission(PermissionAsk {
        title: format!("`{command}`"),
        tool_call: Some(ToolCallUpdate {
            id: ToolCallId(id.into()),
            kind: Some(ToolKind::Execute),
            raw_input: Some(coqo_ui::serde_json::json!({ "command": command })),
            ..ToolCallUpdate::default()
        }),
        options: vec![
            option("reject", "Deny", PermissionOptionKind::RejectOnce),
            option("always", "Always allow", PermissionOptionKind::AllowAlways),
            option("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
        ],
    })
}

fn selected(option: &str) -> PermissionOutcome {
    PermissionOutcome::Selected {
        option_id: option.into(),
    }
}

fn pending(ui: &mut Ui, chat: &Entity<ChatTab>) -> Option<usize> {
    ui.entries(chat)
        .iter()
        .position(|e| matches!(e, Entry::Permission { outcome: None, .. }))
}

fn turns_ended(ui: &mut Ui, chat: &Entity<ChatTab>) {
    ui.wait_for("the turn's end", |ui| !running(ui, chat));
}

#[test]
fn focus_jumps_to_a_permission_request_and_enter_or_esc_answer_it() {
    let provider = ScriptedProvider::new()
        .turn([run("t1", "cargo test -p shop"), chunk("Tests pass")])
        .turn([run("t2", "cargo publish"), chunk("Not published")]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();

    ui.send(&chat, "Run the tests");
    ui.wait_for("the permission card", |ui| pending(ui, &chat).is_some());
    let ix = pending(&mut ui, &chat).unwrap();
    ui.wait_for("the focus on the card", |ui| {
        let chat = chat.clone();
        ui.frame(|window, cx| chat.read(cx).permission_focused(window))
    });
    ui.frame(|window, _| {
        assert!(
            window.try_find(("chat-permission-command", ix)).is_some(),
            "the command is previewed"
        );
    });
    ui.press("enter");
    ui.wait_for("the answer", |ui| ui.agent_text(&chat) == "Tests pass");
    turns_ended(&mut ui, &chat);
    ui.wait_for("the focus back in the composer", |ui| {
        let chat = chat.clone();
        ui.frame(|window, cx| {
            let composer = chat.read(cx).composer().read(cx).focus_handle(cx);
            composer.is_focused(window)
        })
    });

    ui.send(&chat, "Publish it");
    ui.wait_for("the second card", |ui| pending(ui, &chat).is_some());
    ui.wait_for("the focus on the card", |ui| {
        let chat = chat.clone();
        ui.frame(|window, cx| chat.read(cx).permission_focused(window))
    });
    ui.press("escape");
    ui.wait_for("the second answer", |ui| {
        ui.agent_text(&chat) == "Tests pass|Not published"
    });
    assert_eq!(
        fixture.provider.outcomes(),
        [selected("allow-once"), selected("reject")],
        "Enter allows once and Esc denies once, by kind; the turn goes on"
    );
}

#[test]
fn a_remembered_rule_answers_the_next_matching_request_without_a_card() {
    let provider = ScriptedProvider::new()
        .turn([run("t1", "cargo test -p shop"), chunk("one")])
        .turn([run("t2", "cargo test"), chunk("two")])
        .turn([run("t3", "cargo build"), chunk("three")])
        .turn([run("t4", "cargo test --release"), chunk("four")]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.wait_for("the empty rules list", |ui| ui.shows("chat-rules"));

    // Asked: tick "remember for this workspace", then allow once.
    ui.send(&chat, "Run the tests");
    ui.wait_for("the permission card", |ui| pending(ui, &chat).is_some());
    let ix = pending(&mut ui, &chat).unwrap();
    let remember = format!("chat-permission-{ix}-remember");
    ui.wait_for("the remember box", |ui| ui.shows(&remember));
    ui.frame(|window, cx| window.click(name(&remember), cx));
    let allow = format!("chat-permission-{ix}-allow-once");
    ui.frame(|window, cx| window.click(name(&allow), cx));
    ui.wait_for("the answer", |ui| ui.agent_text(&chat) == "one");
    ui.wait_for("the rule in the panel", |ui| {
        let chat = chat.clone();
        ui.cx.read(|cx| chat.read(cx).rules().len() == 1)
    });
    let rule = ui.cx.read(|cx| chat.read(cx).rules()[0].clone());
    assert_eq!(
        (rule.tool, rule.pattern.as_str(), rule.decision),
        (ToolKind::Execute, "cargo test", RuleDecision::Allow)
    );
    ui.wait_for("the rule's row", |ui| {
        ui.frame(|window, _| window.try_find(("chat-rule", rule.id.0)).is_some())
    });
    assert!(matches!(
        &ui.entries(&chat)[ix],
        Entry::Permission { remembered: Some(r), .. } if r.id == rule.id
    ));
    turns_ended(&mut ui, &chat);

    // The next matching request never shows a card.
    ui.send(&chat, "Again");
    ui.wait_for("the answer", |ui| ui.agent_text(&chat) == "one|two");
    let auto = ui.entry_index(
        &chat,
        |e| matches!(e, Entry::Permission { by_rule: Some(r), .. } if r.id == rule.id),
    );
    ui.wait_for("the rule's line", |ui| {
        ui.frame(|window, _| window.try_find(("chat-permission-auto", auto)).is_some())
    });
    assert!(ui.listed()[0].state != AgentState::NeedsPermission);

    // Another command is still asked.
    ui.send(&chat, "Build");
    ui.wait_for("the third card", |ui| pending(ui, &chat).is_some());
    let ix = pending(&mut ui, &chat).unwrap();
    ui.wait_for("the needs-permission dot", |ui| {
        ui.listed()[0].state == AgentState::NeedsPermission
    });
    let button = format!("chat-permission-{ix}-always");
    ui.frame(|window, cx| window.click(name(&button), cx));
    ui.wait_for("the answer", |ui| ui.agent_text(&chat) == "one|two|three");

    // Deleted from the panel, the rule no longer answers.
    ui.frame(|window, cx| window.click(("chat-rule-delete", rule.id.0), cx));
    ui.wait_for("the rule gone", |ui| {
        let chat = chat.clone();
        ui.cx.read(|cx| chat.read(cx).rules().is_empty())
    });
    turns_ended(&mut ui, &chat);
    ui.send(&chat, "Release");
    ui.wait_for("the fourth card", |ui| pending(ui, &chat).is_some());
    assert_eq!(
        fixture.provider.outcomes(),
        [
            selected("allow-once"),
            selected("allow-once"),
            selected("always"),
        ],
        "the provider's own \"always\" option goes to the agent unchanged"
    );
}

#[test]
fn escape_and_stop_cancel_the_running_turn() {
    let never = Arc::new(Notify::new());
    let provider = ScriptedProvider::new()
        .turn([chunk("Working on it"), Step::Wait(never.clone())])
        .turn([chunk("Again"), Step::Wait(never.clone())]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();

    ui.send(&chat, "Start");
    ui.wait_for("the turn", |ui| ui.agent_text(&chat) == "Working on it");
    ui.press("escape");
    ui.wait_for("the cancel", |ui| !running(ui, &chat));
    assert!(fixture.provider.calls().contains(&Call::Cancel));
    assert!(ui.entries(&chat).iter().any(|e| matches!(
        e,
        Entry::Notice {
            kind: NoticeKind::Stopped,
            ..
        }
    )));

    ui.send(&chat, "Once more");
    ui.wait_for("the second turn", |ui| {
        ui.agent_text(&chat).ends_with("Again")
    });
    ui.frame(|window, cx| window.click("chat-stop", cx));
    ui.wait_for("the second cancel", |ui| !running(ui, &chat));
}

#[test]
fn a_restarted_ui_replays_the_transcript_and_follows_the_stream() {
    let next = Arc::new(Notify::new());
    let provider = ScriptedProvider::new()
        .turn([
            chunk("First answer"),
            Step::Update(AgentUpdate::AuthStatus(AuthStatus {
                kind: AuthKind::Account,
                label: None,
                plan: Some("Claude Max".into()),
            })),
            Step::Stop(StopReason::EndTurn),
        ])
        .turn([
            chunk("Second"),
            Step::Wait(next.clone()),
            chunk(" answer"),
            Step::Stop(StopReason::EndTurn),
        ]);
    let fixture = Fixture::new(provider);
    let store = MemoryLayoutStore::default();
    let mut first = open_ui(&fixture, store.clone());
    let chat = first.new_chat();
    first.send(&chat, "One");
    first.wait_for("the first answer", |ui| {
        ui.agent_text(&chat) == "First answer" && !running(ui, &chat)
    });
    first.frame(|window, _| {
        assert!(window.try_find("chat-billing").is_some());
    });
    let before = first.entries(&chat);
    first
        .shell
        .update(&mut first.cx, |shell, cx| shell.save_now(cx));
    let session = first.cx.read(|cx| chat.read(cx).session().clone());
    // Layouts are kept per workspace.
    let workspace = first.cx.read(|cx| {
        Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .unwrap()
            .id
            .clone()
    });
    drop(first);
    assert!(store.load(&workspace.0).unwrap().is_some());

    // A new UI process: the layout brings the tab back, and it replays.
    let mut second = open_ui(&fixture, store);
    let chat = second.chat();
    assert_eq!(
        second.cx.read(|cx| chat.read(cx).session().clone()),
        session
    );
    second.wait_for("the replayed transcript", |ui| ui.entries(&chat) == before);

    second.send(&chat, "Two");
    second.wait_for("the live stream", |ui| {
        ui.agent_text(&chat) == "First answer|Second"
    });
    next.notify_one();
    second.wait_for("the end of the turn", |ui| {
        ui.agent_text(&chat) == "First answer|Second answer" && !running(ui, &chat)
    });
}

#[test]
fn a_provider_that_needs_a_login_says_how_to_sign_in() {
    let provider = ScriptedProvider::new().fail_start(ProviderError::AuthRequired(
        "run `claude` and `/login`".into(),
    ));
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    ui.press(if cfg!(target_os = "macos") {
        "cmd-shift-n"
    } else {
        "ctrl-shift-n"
    });
    ui.wait_for("the sign-in guidance", |ui| {
        ui.cx.read(|cx| {
            ChatList::global(cx)
                .unwrap()
                .read(cx)
                .error()
                .is_some_and(|e| e.contains("needs you to sign in") && e.contains("/login"))
        })
    });
    assert!(ui.chats().is_empty());
    ui.frame(|window, _| assert!(window.try_find("chat-list-error").is_some()));
}

fn sign_in_action() -> ProviderAction {
    ProviderAction {
        id: "sign-in".into(),
        label: "Open `claude` in a terminal".into(),
        run: ProviderActionRun::Terminal {
            program: "/usr/local/bin/claude".into(),
            args: vec![],
        },
    }
}

#[test]
fn a_provider_that_isnt_set_up_offers_its_setup_and_starts_after_it() {
    let provider = ScriptedProvider::new()
        .status(ProviderStatus {
            ready: false,
            message: Some("Claude Code chats need the Claude ACP adapter".into()),
            actions: vec![ProviderAction {
                id: "install-adapter".into(),
                label: "Install Claude ACP adapter (npm)".into(),
                run: ProviderActionRun::Daemon,
            }],
            sign_in: Some(sign_in_action()),
        })
        .action(
            "install-adapter",
            Some(ProviderStatus {
                sign_in: Some(sign_in_action()),
                ..ProviderStatus::ready()
            }),
        );
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    ui.press(if cfg!(target_os = "macos") {
        "cmd-shift-n"
    } else {
        "ctrl-shift-n"
    });
    ui.wait_for("the setup message", |ui| {
        ui.shows("chat-action-install-adapter")
    });
    let error = ui.cx.read(|cx| {
        ChatList::global(cx)
            .unwrap()
            .read(cx)
            .error()
            .map(str::to_owned)
    });
    assert_eq!(
        error.as_deref(),
        Some("Claude Code chats need the Claude ACP adapter")
    );
    assert!(ui.chats().is_empty());
    // Nothing runs until the button is pressed.
    assert!(fixture.provider.calls().is_empty());

    ui.frame(|window, cx| window.click(name("chat-action-install-adapter"), cx));
    let chat = ui.chat();
    assert_eq!(
        fixture.provider.calls()[0],
        Call::RunAction("install-adapter".into())
    );
    ui.wait_for("the chat in the sidebar", |ui| ui.listed().len() == 1);
    assert!(!ui.shows("chat-action-install-adapter"));
    ui.send(&chat, "hi");
    ui.wait_for("the reply", |ui| ui.agent_text(&chat) == "ok");
}

#[test]
fn signing_in_opens_the_agents_own_tool_in_a_terminal() {
    let provider = ScriptedProvider::new()
        .status(ProviderStatus {
            sign_in: Some(sign_in_action()),
            ..ProviderStatus::ready()
        })
        .fail_start(ProviderError::AuthRequired("signed out".into()));
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    ui.press(if cfg!(target_os = "macos") {
        "cmd-shift-n"
    } else {
        "ctrl-shift-n"
    });
    ui.wait_for("the sign-in button", |ui| ui.shows("chat-action-sign-in"));
    let actions = ui
        .cx
        .read(|cx| ChatList::global(cx).unwrap().read(cx).actions().to_vec());
    assert_eq!(actions, [("fake".to_owned(), sign_in_action())]);
}

// ---- composer (#64) -----------------------------------------------------------

/// Every prompt the agent received, in order.
fn prompts(provider: &ScriptedProvider) -> Vec<Vec<ContentBlock>> {
    provider
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Prompt(content) => Some(content),
            _ => None,
        })
        .collect()
}

fn no_embedding() -> ProviderCapabilities {
    ProviderCapabilities {
        resume: true,
        modes: true,
        images: false,
        embedded_context: false,
    }
}

#[test]
fn an_at_mention_is_sent_as_a_resource_link() {
    let fixture = Fixture::new(ScriptedProvider::new().capabilities(no_embedding()));
    std::fs::write(fixture.project.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    std::fs::create_dir_all(fixture.project.join("target")).unwrap();
    std::fs::write(fixture.project.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(fixture.project.join("target/main.o"), "").unwrap();
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();

    ui.type_text(&chat, "Look at @mai");
    ui.wait_for("the file matches", |ui| ui.shows("chat-mention-option-0"));
    // Only src/main.rs matches: target/ is ignored.
    assert!(!ui.shows("chat-mention-option-1"));
    // Arrow keys wrap around the one match; Enter picks it.
    ui.press("down");
    ui.press("up");
    ui.press("enter");
    assert_eq!(ui.composer_text(&chat), "Look at @src/main.rs ");
    assert!(!ui.shows("chat-completion"), "the popover closed");
    let composer = ui.cx.read(|cx| chat.read(cx).composer().clone());
    let mentions = ui.cx.read(|cx| composer.read(cx).mentions(cx));
    assert_eq!(mentions.len(), 1, "the mention is a chip");

    ui.type_text(&chat, "please");
    ui.press("enter");
    ui.wait_for("the prompt", |_| !prompts(&fixture.provider).is_empty());
    let main = fixture.path("src/main.rs");
    assert_eq!(
        prompts(&fixture.provider)[0],
        [
            ContentBlock::text("Look at @src/main.rs please"),
            ContentBlock::ResourceLink {
                uri: format!("file://{main}"),
                name: "main.rs".into(),
                mime_type: None,
                size: Some(13),
            },
        ]
    );
    // The transcript shows the mention as a chip on the prompt.
    ui.wait_for("the prompt in the transcript", |ui| {
        ui.entries(&chat)
            .iter()
            .any(|e| matches!(e, Entry::User { content } if content.len() == 2))
    });
}

#[test]
fn a_small_mentioned_file_is_embedded_when_the_provider_takes_context() {
    let fixture = Fixture::new(ScriptedProvider::new());
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.type_text(&chat, "@main");
    ui.wait_for("the file matches", |ui| ui.shows("chat-mention-option-0"));
    ui.press("tab");
    ui.press("enter");
    ui.wait_for("the prompt", |_| !prompts(&fixture.provider).is_empty());
    assert_eq!(
        prompts(&fixture.provider)[0],
        [
            ContentBlock::text("@src/main.rs"),
            ContentBlock::Resource {
                uri: format!("file://{}", fixture.path("src/main.rs")),
                mime_type: None,
                text: "fn main() {}\n".into(),
            },
        ]
    );
}

#[test]
fn slash_commands_complete_from_the_sessions_commands() {
    let command = |name: &str, description: &str| AvailableCommand {
        name: name.into(),
        description: description.into(),
        input_hint: None,
    };
    let provider = ScriptedProvider::new().on_start([
        AgentUpdate::Modes(test_modes()),
        AgentUpdate::AvailableCommands(vec![
            command("compact", "Summarise the conversation"),
            command("review", "Review a pull request"),
            command("release-notes", "A skill: draft release notes"),
        ]),
    ]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.wait_for("the commands", |ui| {
        ui.cx
            .read(|cx| chat.read(cx).transcript().commands.len() == 3)
    });

    // Esc closes the popover without stopping anything.
    ui.type_text(&chat, "/");
    ui.wait_for("the command list", |ui| ui.shows("chat-command-option-2"));
    ui.press("escape");
    assert!(!ui.shows("chat-completion"));
    assert!(!fixture.provider.calls().contains(&Call::Cancel));
    // It stays closed for this word: typing on doesn't reopen it.
    ui.type_text(&chat, "x");
    assert!(!ui.shows("chat-completion"));

    // A new `/` word opens it again.
    ui.press("backspace");
    ui.press("backspace");
    ui.type_text(&chat, "/re");
    ui.wait_for("the filtered list", |ui| {
        ui.shows("chat-command-option-1") && !ui.shows("chat-command-option-2")
    });
    // `review` and `release-notes` both start with "re", the shorter first.
    ui.press("down");
    ui.press("up");
    ui.press("tab");
    assert_eq!(ui.composer_text(&chat), "/review ");
    ui.type_text(&chat, "#42");
    ui.press("enter");
    ui.wait_for("the prompt", |_| !prompts(&fixture.provider).is_empty());
    assert_eq!(
        prompts(&fixture.provider)[0],
        [ContentBlock::text("/review #42")]
    );
}

#[test]
fn the_provider_dropdown_starts_the_chat_on_the_chosen_provider() {
    let other = ScriptedProvider::new().turn([
        chunk("From the other agent"),
        Step::Stop(StopReason::EndTurn),
    ]);
    let fixture = Fixture::with_other(ScriptedProvider::new(), Some(&other));
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    let first = ui.cx.read(|cx| chat.read(cx).session().clone());
    assert_eq!(ui.listed()[0].provider, "fake");
    ui.wait_for("the providers", |ui| {
        ui.cx.read(|cx| chat.read(cx).providers().len() == 2)
    });

    ui.frame(|window, cx| window.click("chat-provider", cx));
    assert!(ui.shows("chat-menu"));
    ui.frame(|window, cx| window.click(name("chat-provider-option-other"), cx));
    ui.wait_for("the new session", |ui| {
        let session = ui.cx.read(|cx| chat.read(cx).session().clone());
        session != first
            && ui
                .listed()
                .iter()
                .any(|s| s.id == session && s.provider == "other")
    });
    ui.wait_for("the empty session to close", |ui| {
        ui.listed().len() == 1 && fixture.provider.calls().contains(&Call::Close)
    });
    assert!(!ui.shows("chat-menu"));

    ui.send(&chat, "Hello");
    ui.wait_for("the other agent's answer", |ui| {
        ui.agent_text(&chat) == "From the other agent"
    });
    assert_eq!(prompts(&other), [vec![ContentBlock::text("Hello")]]);
    assert!(prompts(&fixture.provider).is_empty());
    // After the first prompt the provider is fixed.
    ui.frame(|window, cx| window.click("chat-provider", cx));
    assert!(!ui.shows("chat-menu"));
}

#[test]
fn the_mode_dropdown_switches_the_mode() {
    let fixture = Fixture::new(ScriptedProvider::new());
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.wait_for("the modes", |ui| ui.shows("chat-mode"));
    ui.frame(|window, cx| window.click("chat-mode", cx));
    assert!(ui.shows("chat-mode-option-default"));
    // A second click on the chip closes it again.
    ui.frame(|window, cx| window.click("chat-mode", cx));
    assert!(!ui.shows("chat-menu"));
    ui.frame(|window, cx| window.click("chat-mode", cx));
    ui.frame(|window, cx| window.click(name("chat-mode-option-plan"), cx));
    ui.wait_for("setMode", |ui| {
        fixture
            .provider
            .calls()
            .contains(&Call::SetMode("plan".into()))
            && ui.cx.read(|cx| {
                chat.read(cx)
                    .transcript()
                    .modes
                    .as_ref()
                    .is_some_and(|m| m.current_mode_id == "plan")
            })
    });
    assert!(!ui.shows("chat-menu"));
    // The keyboard works in the dropdown too: back to Manual.
    ui.frame(|window, cx| window.click("chat-mode", cx));
    ui.press("up");
    ui.press("enter");
    ui.wait_for("setMode default", |_| {
        fixture
            .provider
            .calls()
            .contains(&Call::SetMode("default".into()))
    });
}

/// The current value's name of the chat's config option `id`.
fn option_value(ui: &mut Ui, chat: &Entity<ChatTab>, id: &str) -> Option<String> {
    ui.cx.read(|cx| {
        chat.read(cx)
            .transcript()
            .config_options
            .iter()
            .find(|o| o.id == id)
            .map(SessionConfigOption::value_name)
    })
}

#[test]
fn config_option_chips_pick_a_model_an_effort_and_fast_mode() {
    let provider = ScriptedProvider::new().on_start([
        AgentUpdate::Modes(test_modes()),
        AgentUpdate::ConfigOptions(test_config_options()),
    ]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.wait_for("the config chips", |ui| ui.shows("chat-config-model"));
    assert!(ui.shows("chat-config-effort") && ui.shows("chat-config-fast"));
    // No mode option here: the mode chip stays. The model chip replaces the
    // read-only label.
    assert!(ui.shows("chat-mode"));
    assert!(!ui.shows("chat-model"));
    let labels: Vec<String> = ui.cx.read(|cx| {
        chat.read(cx)
            .transcript()
            .config_options
            .iter()
            .map(config_label)
            .collect()
    });
    assert_eq!(labels, ["Opus", "Effort: Medium", "Fast mode: Off"]);

    // The model, by mouse.
    ui.frame(|window, cx| window.click("chat-config-model", cx));
    assert!(ui.shows("chat-config-option-model-opus"));
    ui.frame(|window, cx| window.click(name("chat-config-option-model-sonnet"), cx));
    ui.wait_for("the model", |ui| {
        fixture.provider.calls().contains(&Call::SetConfigOption(
            "model".into(),
            ConfigValue::Value("sonnet".into()),
        )) && option_value(ui, &chat, "model").as_deref() == Some("Sonnet")
    });
    assert!(!ui.shows("chat-menu"));

    // The effort, by keyboard: Medium, Down to High, Enter.
    ui.frame(|window, cx| window.click("chat-config-effort", cx));
    assert!(ui.shows("chat-config-option-effort-low"));
    ui.press("down");
    ui.press("enter");
    ui.wait_for("the effort", |ui| {
        fixture.provider.calls().contains(&Call::SetConfigOption(
            "effort".into(),
            ConfigValue::Value("high".into()),
        )) && option_value(ui, &chat, "effort").as_deref() == Some("High")
    });

    // Fast mode is a toggle.
    ui.frame(|window, cx| window.click("chat-config-fast", cx));
    ui.wait_for("fast mode", |ui| {
        option_value(ui, &chat, "fast").as_deref() == Some("On")
    });
    assert!(fixture.provider.calls().contains(&Call::SetConfigOption(
        "fast".into(),
        ConfigValue::Boolean(true)
    )));

    // The agent changes one itself: the chip follows. The turn header shows
    // the model the turn ran with.
    let mut lowered = test_config_options();
    lowered[0].set_value(&ConfigValue::Value("sonnet".into()));
    lowered[1].set_value(&ConfigValue::Value("low".into()));
    fixture.provider.push_turn([
        Step::Update(AgentUpdate::ConfigOptions(lowered)),
        chunk("Lowered the effort"),
        Step::Stop(StopReason::EndTurn),
    ]);
    ui.send(&chat, "Go faster");
    ui.wait_for("the agent's change", |ui| {
        option_value(ui, &chat, "effort").as_deref() == Some("Low") && !running(ui, &chat)
    });
    let header = ui
        .entries(&chat)
        .into_iter()
        .find_map(|e| match e {
            Entry::TurnStart { model, .. } => Some(model),
            _ => None,
        })
        .expect("a turn header");
    assert_eq!(header.as_deref(), Some("Sonnet"));
}

#[test]
fn a_mode_config_option_replaces_the_mode_chip() {
    use coqo_agents::model::{ConfigOptionKind, ConfigOptionValue, config_category};

    let mut options = test_config_options();
    options.push(SessionConfigOption {
        id: "mode".into(),
        name: "Mode".into(),
        description: None,
        category: Some(config_category::MODE.into()),
        kind: ConfigOptionKind::Select {
            current_value: "default".into(),
            options: [("default", "Manual"), ("plan", "Plan")]
                .map(|(value, name)| ConfigOptionValue {
                    value: value.into(),
                    name: name.into(),
                    description: None,
                    group: None,
                })
                .to_vec(),
        },
    });
    let provider = ScriptedProvider::new().on_start([
        AgentUpdate::Modes(test_modes()),
        AgentUpdate::ConfigOptions(options),
    ]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.wait_for("the mode option", |ui| ui.shows("chat-config-mode"));
    assert!(!ui.shows("chat-mode"));
    ui.frame(|window, cx| window.click("chat-config-mode", cx));
    ui.frame(|window, cx| window.click(name("chat-config-option-mode-plan"), cx));
    ui.wait_for("the mode", |ui| {
        ui.cx.read(|cx| {
            chat.read(cx)
                .transcript()
                .modes
                .as_ref()
                .is_some_and(|m| m.current_mode_id == "plan")
        })
    });
    assert!(fixture.provider.calls().contains(&Call::SetConfigOption(
        "mode".into(),
        ConfigValue::Value("plan".into())
    )));
}

#[test]
fn up_in_an_empty_composer_recalls_earlier_prompts() {
    let fixture = Fixture::new(ScriptedProvider::new());
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    for (n, text) in ["First", "Second"].into_iter().enumerate() {
        ui.send(&chat, text);
        ui.wait_for("the answer", |ui| {
            !running(ui, &chat)
                && ui
                    .entries(&chat)
                    .iter()
                    .filter(|e| matches!(e, Entry::User { .. }))
                    .count()
                    == n + 1
        });
    }
    ui.press("up");
    assert_eq!(ui.composer_text(&chat), "Second");
    ui.press("up");
    assert_eq!(ui.composer_text(&chat), "First");
    ui.press("up");
    assert_eq!(ui.composer_text(&chat), "First", "stays at the oldest");
    ui.press("down");
    assert_eq!(ui.composer_text(&chat), "Second");
    ui.press("down");
    assert_eq!(ui.composer_text(&chat), "");
}

/// Config options shaped like Claude's adapter, for screenshots.
fn screenshot_config_options() -> Vec<SessionConfigOption> {
    use coqo_agents::model::{ConfigOptionKind, ConfigOptionValue, config_category};

    let select =
        |id: &str, name: &str, category: &str, current: &str, values: &[(&str, &str, &str)]| {
            SessionConfigOption {
                id: id.into(),
                name: name.into(),
                description: None,
                category: Some(category.into()),
                kind: ConfigOptionKind::Select {
                    current_value: current.into(),
                    options: values
                        .iter()
                        .map(|(value, name, description)| ConfigOptionValue {
                            value: (*value).into(),
                            name: (*name).into(),
                            description: (!description.is_empty()).then(|| (*description).into()),
                            group: None,
                        })
                        .collect(),
                },
            }
        };
    vec![
        select(
            "model",
            "Model",
            config_category::MODEL,
            "opus",
            &[
                ("default", "Default (recommended)", "Opus for complex work"),
                ("opus", "Opus", "Most capable for complex work"),
                ("sonnet", "Sonnet", "Best for everyday tasks"),
                ("haiku", "Haiku", "Fastest for quick answers"),
                // As long as Claude's list: the card scrolls.
                ("claude-sonnet-5", "Sonnet 5", ""),
                ("claude-opus-5", "Opus 5", ""),
                ("claude-opus-4-8", "Opus 4.8", ""),
                ("claude-opus-4-7", "Opus 4.7", ""),
                ("claude-sonnet-4-6", "Sonnet 4.6", ""),
            ],
        ),
        select(
            "effort",
            "Effort",
            config_category::THOUGHT_LEVEL,
            "high",
            &[
                ("low", "Low", ""),
                ("medium", "Medium", ""),
                ("high", "High", ""),
                ("max", "Max", ""),
            ],
        ),
        SessionConfigOption {
            id: "fast_mode".into(),
            name: "Fast mode".into(),
            description: None,
            category: Some(config_category::SPEED.into()),
            kind: ConfigOptionKind::Boolean {
                current_value: true,
            },
        },
        select(
            "mode",
            "Mode",
            config_category::MODE,
            "default",
            &[
                ("default", "Manual", ""),
                ("acceptEdits", "Accept edits", ""),
                ("plan", "Plan", ""),
            ],
        ),
    ]
}

/// Renders a chat tab offscreen to PNGs for visual review against board 1.
/// Needs a GPU: `cargo test -p coqo -- --ignored chat_screenshots`.
/// Writes `coqo-chat-<appearance>.png`, `coqo-composer-<appearance>.png`
/// (an @-mention popover) and `coqo-settings-<appearance>.png` (config
/// option chips, the model dropdown open) to `$COQO_SHOT_DIR` (default: the
/// system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn chat_screenshots() {
    use coqo_agents::model::{
        PermissionRequest, PermissionRequestId, PermissionRule, PermissionRuleId,
        PermissionRuleSpec, SessionId, SessionUpdate, Usage,
    };
    use coqo_ui::test::render_offscreen;

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let info = |id: &str, title: &str, state: AgentState, at: i64| SessionInfo {
        id: SessionId(id.into()),
        provider: "claude-acp".into(),
        workspace: Some("ws-shop".into()),
        cwd: "/work/shop".into(),
        title: Some(title.into()),
        state,
        state_reason: None,
        state_since: None,
        status: None,
        criteria: Vec::new(),
        tool_use: Vec::new(),
        mode: Some("default".into()),
        closed: false,
        created_at: at,
        updated_at: at,
    };
    let with_status = |mut i: SessionInfo, text: &str, progress: Option<(u64, u64)>| {
        i.status = Some(coqo_agents::model::AgentStatus {
            text: text.into(),
            state: None,
            progress: progress.map(|(current, total)| StatusProgress { current, total }),
            turn: Some(1),
            updated_at: i.updated_at,
        });
        i
    };
    let shipping = move || {
        use coqo_agents::model::{
            AcceptanceCriterion, CriterionAuthor, CriterionEvidence, CriterionStatus,
        };
        let criterion =
            |id: &str, text: &str, status, evidence: Vec<CriterionEvidence>| AcceptanceCriterion {
                id: id.into(),
                text: text.into(),
                status,
                evidence,
                changed_by: CriterionAuthor::Agent,
                updated_at: 3,
            };
        let mut i = with_status(
            info(
                "s1",
                "Free shipping threshold",
                AgentState::NeedsPermission,
                3,
            ),
            "Writing tests for the threshold",
            Some((2, 4)),
        );
        i.criteria = vec![
            criterion(
                "AC1",
                "Orders of 50 EUR or more ship free",
                CriterionStatus::Met,
                vec![CriterionEvidence {
                    text: "test free_from_threshold passes".into(),
                    path: Some("/work/shop/src/shipping.rs".into()),
                    line: Some(7),
                }],
            ),
            criterion(
                "AC2",
                "Smaller orders keep the flat fee",
                CriterionStatus::Pending,
                vec![],
            ),
            criterion(
                "AC3",
                "Totals show the saving",
                CriterionStatus::Failed,
                vec![CriterionEvidence {
                    text: "Cart::totals drops the discount line".into(),
                    path: None,
                    line: None,
                }],
            ),
        ];
        i
    };
    let diff = |path: &str, old: Option<&str>, new: &str| ToolCallContent::Diff {
        path: format!("/work/shop/{path}"),
        old_text: old.map(str::to_owned),
        new_text: new.into(),
    };
    let tool = |id: &str, title: &str, kind, content: Vec<ToolCallContent>| {
        SessionUpdate::ToolCall(ToolCall {
            id: ToolCallId(id.into()),
            title: title.into(),
            name: None,
            kind,
            status: ToolCallStatus::Completed,
            content,
            locations: vec![],
            raw_input: None,
            raw_output: None,
        })
    };
    let rule = |id: u64, tool, pattern: &str, decision| PermissionRule {
        id: PermissionRuleId(id),
        workspace: "ws-shop".into(),
        tool,
        pattern: pattern.into(),
        decision,
        created_at: 0,
    };
    let rules = vec![
        rule(1, ToolKind::Execute, "cargo test", RuleDecision::Allow),
        rule(2, ToolKind::Edit, "src/**", RuleDecision::Allow),
        rule(3, ToolKind::Execute, "cargo publish", RuleDecision::Reject),
    ];
    let step = |content: &str, status| PlanEntry {
        content: content.into(),
        priority: Default::default(),
        status,
    };
    let updates = vec![
        SessionUpdate::UserMessage {
            content: vec![ContentBlock::text(
                "Add a free-shipping threshold to the cart totals. Keep the rule in \
                 `ShippingPolicy` and cover it with tests.",
            )],
        },
        SessionUpdate::TurnStarted { turn: 1 },
        SessionUpdate::Usage(Usage {
            used_tokens: Some(48_200),
            context_size: Some(200_000),
            cost: None,
            model: Some("Opus".into()),
            meta: None,
        }),
        SessionUpdate::AuthStatus(AuthStatus {
            kind: AuthKind::Account,
            label: None,
            plan: Some("Claude plan".into()),
        }),
        SessionUpdate::Modes(test_modes()),
        SessionUpdate::AgentThoughtChunk {
            content: ContentBlock::text("The totals are computed in cart.rs."),
        },
        SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text(
                "I added the threshold to **`ShippingPolicy`** and call it from \
                 `Cart::totals`. Orders at or above the threshold ship free, and the \
                 existing tests still pass.",
            ),
        },
        tool(
            "t1",
            "Edit src/shipping.rs",
            ToolKind::Edit,
            vec![diff(
                "src/shipping.rs",
                Some("pub struct ShippingPolicy {\n    pub flat: Money,\n}\n"),
                "pub struct ShippingPolicy {\n    pub flat: Money,\n    pub free_from: Money,\n}\n\nimpl ShippingPolicy {\n    pub fn fee(&self, subtotal: Money) -> Money {\n        if subtotal >= self.free_from { Money::ZERO } else { self.flat }\n    }\n}\n",
            )],
        ),
        tool(
            "t2",
            "Edit src/cart.rs",
            ToolKind::Edit,
            vec![diff(
                "src/cart.rs",
                Some("let shipping = policy.flat;\n"),
                "let shipping = policy.fee(subtotal);\n",
            )],
        ),
        tool("t3", "cargo test -p shop", ToolKind::Execute, vec![]),
        SessionUpdate::PermissionRequested(PermissionRequest {
            id: PermissionRequestId(1),
            title: "`cargo test -p shop`".into(),
            tool_call: Some(ToolCallUpdate {
                id: ToolCallId("t3".into()),
                raw_input: Some(coqo_ui::serde_json::json!({"command": "cargo test -p shop"})),
                ..ToolCallUpdate::default()
            }),
            options: ask().options,
            suggested_rule: None,
        }),
        SessionUpdate::PermissionResolved {
            request_id: PermissionRequestId(1),
            outcome: PermissionOutcome::Selected {
                option_id: "allow-once".into(),
            },
            by_rule: Some(rules[0].clone()),
            remembered: None,
        },
        SessionUpdate::Walkthrough(Box::new({
            let mut w = walkthrough_summary("s1", "Free shipping threshold", "src/shipping.rs", 5);
            w.coverage = Some(incomplete_coverage("src/shipping.rs"));
            w
        })),
        SessionUpdate::Plan {
            entries: vec![
                step("Threshold in ShippingPolicy", PlanEntryStatus::Completed),
                step("Use it in Cart::totals", PlanEntryStatus::Completed),
                step("Tests for the threshold", PlanEntryStatus::InProgress),
                step("Update the changelog", PlanEntryStatus::Pending),
            ],
        },
        SessionUpdate::PermissionRequested(PermissionRequest {
            id: PermissionRequestId(2),
            title: "Write tests/shipping.rs".into(),
            tool_call: Some(ToolCallUpdate {
                id: ToolCallId("t4".into()),
                content: Some(vec![diff(
                    "tests/shipping.rs",
                    None,
                    "#[test]\nfn free_from_threshold() {\n    assert_eq!(policy().fee(eur(60)), Money::ZERO);\n}\n",
                )]),
                kind: Some(ToolKind::Edit),
                ..ToolCallUpdate::default()
            }),
            options: ask().options,
            suggested_rule: Some(PermissionRuleSpec {
                tool: ToolKind::Edit,
                pattern: "tests/**".into(),
            }),
        }),
        SessionUpdate::State {
            state: AgentState::NeedsPermission,
            reason: None,
        },
    ];
    let shots = [
        ("chat-dark", Appearance::Dark, false),
        ("chat-light", Appearance::Light, false),
        ("composer-dark", Appearance::Dark, true),
        ("composer-light", Appearance::Light, true),
        ("setup-dark", Appearance::Dark, false),
        ("settings-dark", Appearance::Dark, false),
        ("settings-light", Appearance::Light, false),
        ("rename-tab-dark", Appearance::Dark, false),
        ("rename-row-light", Appearance::Light, false),
    ];
    for (name, appearance, composing) in shots {
        let setup = name.starts_with("setup");
        let mut updates = updates.clone();
        if name.starts_with("settings") {
            // Claude's session config options: Model, Effort, Fast mode on,
            // Mode; the model dropdown open.
            updates.push(SessionUpdate::ConfigOptions {
                config_options: screenshot_config_options(),
            });
        }
        let settings = name.starts_with("settings");
        let renaming = match name {
            "rename-tab-dark" => Some(("s1", "Free shipping threshold", super::rename::Place::Tab)),
            "rename-row-light" => {
                Some(("s2", "Fix the flaky login test", super::rename::Place::Row))
            }
            _ => None,
        };
        let rules = rules.clone();
        let image = render_offscreen(appearance, size(1440., 900.), move |window, cx| {
            crate::shell::init(cx);
            let list = ChatList::global(cx).unwrap();
            list.update(cx, |l, cx| {
                if setup {
                    // A first chat with Claude Code before its adapter is
                    // installed.
                    l.set_problem(
                        "Claude Code chats need the Claude ACP adapter: Coqo installs \
                         @agentclientprotocol/claude-agent-acp with npm into its data \
                         directory when you ask it to. It runs your own `claude` and your \
                         own login."
                            .into(),
                        "claude-acp".into(),
                        vec![ProviderAction {
                            id: "install-adapter".into(),
                            label: "Install Claude ACP adapter (npm)".into(),
                            run: ProviderActionRun::Daemon,
                        }],
                        cx,
                    );
                    return;
                }
                l.set_preview_workspace("ws-shop");
                l.upsert(info("s3", "Bump the HTTP client", AgentState::Done, 1), cx);
                l.upsert(
                    with_status(
                        info("s2", "Fix the flaky login test", AgentState::Working, 2),
                        "Running the login tests",
                        Some((3, 8)),
                    ),
                    cx,
                );
                l.upsert(shipping(), cx);
            });
            let store = Rc::new(MemoryLayoutStore::default());
            let shell = cx.new(|cx| {
                let shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                if setup {
                    return shell;
                }
                let chat = cx.new(|cx| {
                    let mut chat = ChatTab::new(SessionId("s1".into()), window, cx);
                    chat.set_preview(shipping(), "Claude Code", cx);
                    chat.apply((1..).zip(updates).collect(), cx);
                    chat.set_preview_rules(rules, cx);
                    if settings {
                        chat.set_preview_config_menu("model");
                    }
                    if composing {
                        // An @-mention chip, and the popover for `@ship`.
                        chat.set_preview_files(
                            "/work/shop",
                            &[
                                "Cargo.toml",
                                "src/cart.rs",
                                "src/shipping.rs",
                                "src/ship_rates.rs",
                                "tests/shipping.rs",
                                "docs/shipping-policy.md",
                            ],
                        );
                        let text = "Compare @src/cart.rs with @ship";
                        let chip = coqo_ui::input::MentionSpan {
                            range: 8..20,
                            mention: coqo_ui::input::Mention {
                                id: "file:///work/shop/src/cart.rs".into(),
                                text: "@src/cart.rs".into(),
                                label: "@cart.rs".into(),
                            },
                        };
                        chat.composer()
                            .clone()
                            .update(cx, |c, cx| c.set_content(text, vec![chip], window, cx));
                    }
                    chat
                });
                shell.dock().update(cx, |dock, cx| {
                    dock.open_tab(coqo_ui::dock::AnyTab::new(chat), window, cx);
                });
                shell
            });
            if let Some((id, title, place)) = renaming {
                list.update(cx, |l, cx| {
                    l.start_rename(SessionId(id.into()), title.into(), place, None, window, cx)
                });
            }
            shell
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}

#[test]
fn acceptance_criteria_show_in_the_panel_and_the_user_can_mark_them() {
    use coqo_agents::model::{
        CriterionAuthor, CriterionEvidence, CriterionInput, CriterionStatus, SetCriteriaParams,
    };
    let next = Arc::new(Notify::new());
    let provider =
        ScriptedProvider::new().turn([Step::Wait(next.clone()), Step::Stop(StopReason::EndTurn)]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.frame(|window, _| {
        assert!(window.try_find("chat-criteria").is_some(), "the empty card");
        assert!(window.try_find(("chat-criterion", 0usize)).is_none());
    });
    ui.send(&chat, "Add the volume discount");
    ui.wait_for("the turn", |ui| running(ui, &chat));
    let id = ui.listed()[0].id.clone();

    // The agent registers two criteria and verifies one (as the MCP tool
    // does, through the manager).
    let manager = &fixture.daemon.manager;
    manager
        .set_criteria(
            SetCriteriaParams {
                session: id.clone(),
                criteria: vec![
                    CriterionInput {
                        text: Some("100+ units get 10% off".into()),
                        status: Some(CriterionStatus::Met),
                        evidence: Some(vec![CriterionEvidence {
                            text: "test volume_discount passes".into(),
                            path: Some("src/main.rs".into()),
                            line: Some(1),
                        }]),
                        ..CriterionInput::default()
                    },
                    CriterionInput {
                        text: Some("Discount capped at 500 EUR".into()),
                        ..CriterionInput::default()
                    },
                ],
                replace: true,
            },
            CriterionAuthor::Agent,
        )
        .unwrap();
    ui.wait_for("the criteria", |ui| {
        ui.cx.read(|cx| chat.read(cx).criteria(cx).len()) == 2
    });
    ui.frame(|window, _| {
        assert!(window.try_find(("chat-criterion", 0usize)).is_some());
        assert!(window.try_find(("chat-criterion", 1usize)).is_some());
        assert!(window.try_find("chat-criteria-count").is_some());
        assert!(
            window
                .try_find(name("chat-criterion-evidence-0-0"))
                .is_some()
        );
        assert!(
            window
                .try_find(("chat-criterion-by-user", 1usize))
                .is_none()
        );
    });

    // The user marks the pending one met: the daemon has it, as theirs.
    ui.frame(|window, cx| window.click(("chat-criterion-mark", 1usize), cx));
    ui.wait_for("the user's mark", |ui| {
        ui.cx
            .read(|cx| chat.read(cx).criteria(cx).get(1).map(|c| c.status))
            == Some(CriterionStatus::Met)
    });
    let snapshot = manager.snapshot(&id).unwrap();
    assert_eq!(snapshot.info.criteria[1].changed_by, CriterionAuthor::User);
    ui.frame(|window, _| {
        assert!(
            window
                .try_find(("chat-criterion-by-user", 1usize))
                .is_some()
        );
    });

    // Evidence with a file opens it.
    ui.frame(|window, cx| window.click(name("chat-criterion-evidence-0-0"), cx));
    let ws = ui.cx.read(|cx| {
        Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .unwrap()
            .clone()
    });
    let key = editor::key(&ws.id, ws.roots[0].id, "src/main.rs");
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    ui.wait_for("the file tab", |ui| {
        ui.cx
            .read(|cx| dock.read(cx).find_tab(editor::KIND, &key, cx).is_some())
    });
    next.notify_one();
    ui.wait_for("the end of the turn", |ui| !running(ui, &chat));
}

#[test]
fn uncovered_hunks_are_flagged_and_open_their_lines() {
    use coqo_ui::gpui::SharedString;

    let next = Arc::new(Notify::new());
    let provider =
        ScriptedProvider::new().turn([Step::Wait(next.clone()), Step::Stop(StopReason::EndTurn)]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.send(&chat, "Add the volume discount");
    ui.wait_for("the turn", |ui| running(ui, &chat));
    let id = ui.listed()[0].id.clone();
    let ws = ui.cx.read(|cx| {
        Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .unwrap()
            .clone()
    });
    let mut summary = walkthrough_summary(&id.0, "Volume discount", "src/other.rs", 0);
    summary.start.as_mut().unwrap().root = ws.roots[0].id;
    let mut coverage = incomplete_coverage("src/main.rs");
    for h in &mut coverage.uncovered {
        h.root = ws.roots[0].id;
    }
    summary.coverage = Some(coverage.clone());
    fixture
        .daemon
        .manager
        .record_walkthrough(&id, summary.clone())
        .unwrap();
    ui.wait_for("the card", |ui| {
        ui.cx
            .read(|cx| chat.read(cx).transcript().walkthroughs.len())
            == 1
    });
    let card = ui
        .cx
        .read(|cx| {
            chat.read(cx)
                .transcript()
                .entries
                .iter()
                .position(|e| matches!(e, Entry::Walkthrough(_)))
        })
        .unwrap();
    assert_eq!(super::tab::coverage_label(&coverage), "Coverage 7/9 hunks");
    let hunk = |k: usize| SharedString::from(format!("chat-walkthrough-hunk-{card}-{k}"));
    ui.frame(|window, _| {
        assert!(
            window
                .try_find(("chat-walkthrough-coverage", card))
                .is_some()
        );
        assert!(
            window
                .try_find(("chat-walkthrough-coverage-row", 0usize))
                .is_some()
        );
        assert!(window.try_find(hunk(1)).is_some());
        assert!(window.try_find("chat-walkthrough-row-hunk-0-0").is_some());
    });

    // "Ask … to explain it" during a turn: the request waits in the
    // composer for the user to send.
    ui.frame(|window, cx| window.click(("chat-walkthrough-ask", card), cx));
    let text = ui.composer_text(&chat);
    assert!(
        text.starts_with("Your walkthrough \"Volume discount\""),
        "{text}"
    );
    assert!(text.contains("\n- src/main.rs:1 (+1)"), "{text}");
    assert!(
        text.contains("\n- src/main.rs:3-4 (+2 \u{2212}1)"),
        "{text}"
    );

    // A hunk opens its lines (not the walkthrough's start).
    ui.frame(|window, cx| window.click(hunk(0), cx));
    let key = editor::key(&ws.id, ws.roots[0].id, "src/main.rs");
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    ui.wait_for("the hunk's file", |ui| {
        ui.cx
            .read(|cx| dock.read(cx).find_tab(editor::KIND, &key, cx).is_some())
    });
    let player = crate::walkthrough::player::key(&summary.id);
    assert!(
        ui.cx.read(|cx| dock
            .read(cx)
            .find_tab(crate::walkthrough::player::KIND, &player, cx)
            .is_none()),
        "the card did not open the player too"
    );
    next.notify_one();
    ui.wait_for("the end of the turn", |ui| !running(ui, &chat));
}

/// A published walkthrough as the transcript carries it.
fn walkthrough_summary(
    session: &str,
    title: &str,
    path: &str,
    line: u32,
) -> coqo_protocol::methods::walkthroughs::WalkthroughSummary {
    use coqo_protocol::methods::editor::LineRange;
    use coqo_protocol::methods::walkthroughs::{
        ScopeKind, StorageMode, WalkthroughId, WalkthroughLocation, WalkthroughSummary,
    };
    use coqo_protocol::methods::workspace::{RootId, WorkspaceId};
    WalkthroughSummary {
        id: WalkthroughId(format!("{}-0001", title.to_lowercase().replace(' ', "-"))),
        workspace: WorkspaceId("ws".into()),
        session: coqo_agents::model::SessionId(session.into()),
        title: title.into(),
        scope: ScopeKind::Worktree,
        steps: 4,
        understood: 0,
        questions: 0,
        start: Some(WalkthroughLocation {
            root: RootId(0),
            path: path.into(),
            lines: LineRange {
                start: line,
                end: line + 2,
            },
        }),
        storage: StorageMode::Local,
        coverage: None,
        created_at: 1,
        revision: 0,
    }
}

/// Coverage with two hunks of `path` (in root 0) uncovered.
fn incomplete_coverage(path: &str) -> coqo_protocol::methods::walkthroughs::WalkthroughCoverage {
    use coqo_protocol::methods::editor::LineRange;
    use coqo_protocol::methods::walkthroughs::{UncoveredHunk, WalkthroughCoverage};
    use coqo_protocol::methods::workspace::RootId;
    let hunk = |start: u32, end: u32, added: u32, removed: u32| UncoveredHunk {
        root: RootId(0),
        path: path.into(),
        new_range: LineRange { start, end },
        old_range: None,
        added,
        removed,
        whole_file: None,
        renamed_from: None,
        excerpt: "+fn free_from() -> u32 {".into(),
    };
    WalkthroughCoverage {
        total: 9,
        covered: 6,
        trivial: 1,
        uncovered: vec![hunk(0, 0, 1, 0), hunk(2, 3, 2, 1)],
        unavailable: None,
    }
}

#[test]
fn a_published_walkthrough_shows_as_a_card_and_opens_its_player() {
    let next = Arc::new(Notify::new());
    let provider =
        ScriptedProvider::new().turn([Step::Wait(next.clone()), Step::Stop(StopReason::EndTurn)]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    ui.frame(|window, _| {
        assert!(
            window.try_find("chat-walkthroughs").is_some(),
            "the empty card"
        );
        assert!(window.try_find(("chat-walkthrough-row", 0usize)).is_none());
    });
    ui.send(&chat, "Add the volume discount");
    ui.wait_for("the turn", |ui| running(ui, &chat));
    let id = ui.listed()[0].id.clone();
    let ws = ui.cx.read(|cx| {
        Workspaces::global(cx)
            .unwrap()
            .read(cx)
            .current()
            .unwrap()
            .clone()
    });

    // The agent publishes one (as `walkthroughs/create` records it).
    let mut summary = walkthrough_summary(&id.0, "Volume discount", "src/main.rs", 0);
    summary.start.as_mut().unwrap().root = ws.roots[0].id;
    summary.start.as_mut().unwrap().lines.end = 0;
    fixture
        .daemon
        .manager
        .record_walkthrough(&id, summary.clone())
        .unwrap();
    ui.wait_for("the card", |ui| {
        ui.cx
            .read(|cx| chat.read(cx).transcript().walkthroughs.len())
            == 1
    });
    let card = ui.cx.read(|cx| {
        chat.read(cx)
            .transcript()
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Walkthrough(_)))
    });
    let card = card.expect("a walkthrough entry");
    assert_eq!(
        super::tab::published_label(&summary),
        "Walkthrough published: Volume discount · 4 steps"
    );
    ui.frame(|window, _| {
        assert!(window.try_find(("chat-walkthrough", card)).is_some());
        assert!(window.try_find(("chat-walkthrough-row", 0usize)).is_some());
    });

    // Clicking the panel's row opens the walkthrough's player (#100).
    ui.frame(|window, cx| window.click(("chat-walkthrough-row", 0usize), cx));
    let key = crate::walkthrough::player::key(&summary.id);
    let dock = ui.cx.read(|cx| ui.shell.read(cx).dock().clone());
    ui.wait_for("the player tab", |ui| {
        ui.cx.read(|cx| {
            dock.read(cx)
                .find_tab(crate::walkthrough::player::KIND, &key, cx)
                .is_some()
        })
    });
    next.notify_one();
    ui.wait_for("the end of the turn", |ui| !running(ui, &chat));
}

fn renaming(ui: &mut Ui) -> Option<super::rename::Place> {
    ui.cx.read(|cx| {
        ChatList::global(cx)
            .unwrap()
            .read(cx)
            .renaming()
            .map(|(_, place)| place)
    })
}

/// Types `text` into the open name field and presses Enter.
fn type_title(ui: &mut Ui, session: &coqo_agents::model::SessionId, text: &str) {
    let text = text.to_owned();
    let session = session.clone();
    ui.frame(|window, cx| {
        let list = ChatList::global(cx).unwrap();
        let place = list.read(cx).renaming().unwrap().1;
        let input = list
            .read(cx)
            .rename_field(&session, place, window)
            .expect("the name field");
        input.update(cx, |i, cx| i.set_text(text, window, cx));
    });
    ui.press("enter");
}

fn stored_title(fixture: &Fixture, session: &coqo_agents::model::SessionId) -> Option<String> {
    fixture.daemon.manager.snapshot(session).unwrap().info.title
}

#[test]
fn a_chat_is_titled_by_its_first_prompt_and_renamed_in_place() {
    let provider = ScriptedProvider::new()
        .turn([chunk("On it"), Step::Stop(StopReason::EndTurn)])
        .turn([
            Step::Update(AgentUpdate::Title("The agent's title".into())),
            Step::Stop(StopReason::EndTurn),
        ]);
    let fixture = Fixture::new(provider);
    let mut ui = open_ui(&fixture, MemoryLayoutStore::default());
    let chat = ui.new_chat();
    let session = ui.cx.read(|cx| chat.read(cx).session().clone());
    assert_eq!(ui.listed()[0].title, None);

    // The sidebar row shows the derived title from `sessions/changes`.
    ui.send(&chat, "Fix the **flaky** login test\n\nIt fails on CI.");
    ui.wait_for("the derived title", |ui| {
        ui.listed()[0].title.as_deref() == Some("Fix the flaky login test") && !running(ui, &chat)
    });
    assert_eq!(
        ui.cx
            .read(|cx| coqo_ui::dock::TabItem::title(chat.read(cx), cx)),
        "Fix the flaky login test"
    );

    // A double-click on the tab label opens the name field there.
    ui.frame(|window, cx| window.double_click("chat-tab-title", cx));
    assert_eq!(renaming(&mut ui), Some(super::rename::Place::Tab));
    assert!(ui.shows("chat-tab-rename"));
    type_title(&mut ui, &session, "  Login test  ");
    ui.wait_for("the renamed chat", |ui| {
        ui.listed()[0].title.as_deref() == Some("Login test")
    });
    assert_eq!(renaming(&mut ui), None);
    assert!(!ui.shows("chat-tab-rename"));
    assert_eq!(
        stored_title(&fixture, &session).as_deref(),
        Some("Login test")
    );
    // The focus is back in the composer.
    let composer = ui.cx.read(|cx| chat.read(cx).composer().clone());
    ui.frame(|window, cx| {
        assert!(composer.read(cx).focus_handle(cx).is_focused(window));
    });

    // The agent's own title no longer replaces the user's.
    ui.send(&chat, "go on");
    ui.wait_for("the second turn", |ui| {
        !running(ui, &chat) && fixture.provider.calls().len() >= 3
    });
    assert_eq!(
        stored_title(&fixture, &session).as_deref(),
        Some("Login test")
    );

    // F2 in the chat opens it too; Esc leaves the title as it was.
    ui.press("f2");
    assert_eq!(renaming(&mut ui), Some(super::rename::Place::Tab));
    ui.press("escape");
    assert_eq!(renaming(&mut ui), None);
    assert_eq!(ui.listed()[0].title.as_deref(), Some("Login test"));

    // A double-click on the CHATS row renames it there.
    let row = format!("chat-row-{session}");
    ui.frame(|window, cx| window.double_click(name(&row), cx));
    assert_eq!(renaming(&mut ui), Some(super::rename::Place::Row));
    assert!(ui.shows("chat-row-rename"));
    type_title(&mut ui, &session, "Login flake");
    ui.wait_for("the row's new title", |ui| {
        ui.listed()[0].title.as_deref() == Some("Login flake")
    });
    assert_eq!(
        stored_title(&fixture, &session).as_deref(),
        Some("Login flake")
    );
    // An empty title changes nothing.
    ui.frame(|window, cx| window.double_click(name(&row), cx));
    type_title(&mut ui, &session, "   ");
    assert_eq!(renaming(&mut ui), None);
    assert_eq!(ui.listed()[0].title.as_deref(), Some("Login flake"));
}

/// #304: features that start their own session (questions, a review sent
/// to a new chat, commit drafts) go to Claude Code (`claude-acp`) even
/// when another provider is available and listed first.
#[test]
fn features_prefer_the_claude_code_provider() {
    use coqo_agents::model::{ProviderInfo, ProviderKind};

    let provider = |id: &str, available: bool| ProviderInfo {
        id: id.into(),
        name: id.into(),
        kind: ProviderKind::Acp,
        description: None,
        available,
        capabilities: None,
        status: None,
    };
    assert_eq!(super::PREFERRED_PROVIDER, "claude-acp");
    let pick =
        |providers: &[ProviderInfo]| super::preferred_provider(providers).map(|p| p.id.clone());

    let both = [provider("codex", true), provider("claude-acp", true)];
    assert_eq!(pick(&both).as_deref(), Some("claude-acp"));
    // Not set up: the first available one instead.
    let claude_down = [provider("codex", true), provider("claude-acp", false)];
    assert_eq!(pick(&claude_down).as_deref(), Some("codex"));
    assert_eq!(pick(&[provider("claude-acp", false)]), None);
}
