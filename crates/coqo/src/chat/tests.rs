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
        AgentState, AuthKind, AuthStatus, ContentBlock, PermissionOption, PermissionOptionKind,
        PermissionOutcome, PlanEntry, PlanEntryStatus, SessionInfo, StopReason, ToolCall,
        ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolKind,
    },
    testing::{Call, ScriptedProvider, Step},
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
    tab::ChatTab,
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
}

impl TestDaemon {
    fn start(dir: &Path, provider: &ScriptedProvider) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let paths = DaemonPaths::at(dir.join("run"));
        let data = DataDirs::at(dir.join("data"));
        std::fs::create_dir_all(data.root()).unwrap();
        let _entered = runtime.enter();
        let info = AgentProviderInfo {
            id: "fake".parse().unwrap(),
            name: "Fake agent".into(),
            kind: AgentProviderKind::Acp,
            description: None,
        };
        let providers = StaticProviders::new().with(info, Arc::new(provider.clone()));
        let store = SessionStore::open(&data.global_db()).unwrap();
        let manager =
            SessionManager::open(store, Arc::new(providers), ManagerOptions::default()).unwrap();
        let workspaces = Arc::new(coqod::workspace::WorkspaceService::open(data).unwrap());
        let router = coqod::workspace::register(Router::new(), workspaces);
        let router = coqo_agents::rpc::register(router, manager);
        let server = Server::new(router, ServerConfig::new("coqod", "test"));
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        runtime.spawn(coqo_protocol::unix::serve_unix(server, listener));
        drop(_entered);
        Self {
            _runtime: runtime,
            paths,
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
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo-project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
        let project = project.canonicalize().unwrap();
        let daemon = TestDaemon::start(tmp.path(), &provider);
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
fn tool_calls_show_their_diff_and_open_their_files() {
    let provider = ScriptedProvider::new();
    let fixture = Fixture::new(provider);
    let main = fixture.path("src/main.rs");
    fixture.provider.push_turn([
        Step::Update(AgentUpdate::ToolCall(ToolCall {
            id: ToolCallId("t1".into()),
            title: "Edit src/main.rs".into(),
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

/// Renders a chat tab offscreen to PNGs for visual review against board 1.
/// Needs a GPU: `cargo test -p coqo -- --ignored chat_screenshots`.
/// Writes `coqo-chat-<appearance>.png` to `$COQO_SHOT_DIR` (default: the
/// system temp dir).
#[test]
#[ignore = "needs a GPU; run manually"]
fn chat_screenshots() {
    use coqo_agents::model::{
        PermissionRequest, PermissionRequestId, SessionId, SessionUpdate, Usage,
    };
    use coqo_ui::test::render_offscreen;

    let dir = std::env::var_os("COQO_SHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let info = |id: &str, title: &str, state: AgentState, at: i64| SessionInfo {
        id: SessionId(id.into()),
        provider: "claude-acp".into(),
        workspace: None,
        cwd: "/work/shop".into(),
        title: Some(title.into()),
        state,
        state_reason: None,
        mode: Some("default".into()),
        closed: false,
        created_at: at,
        updated_at: at,
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
            kind,
            status: ToolCallStatus::Completed,
            content,
            locations: vec![],
            raw_input: None,
            raw_output: None,
        })
    };
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
        SessionUpdate::Plan {
            entries: vec![
                step("Threshold in ShippingPolicy", PlanEntryStatus::Completed),
                step("Use it in Cart::totals", PlanEntryStatus::Completed),
                step("Tests for the threshold", PlanEntryStatus::InProgress),
                step("Update the changelog", PlanEntryStatus::Pending),
            ],
        },
        SessionUpdate::PermissionRequested(PermissionRequest {
            id: PermissionRequestId(1),
            title: "Write tests/shipping.rs".into(),
            tool_call: Some(ToolCallUpdate {
                id: ToolCallId("t4".into()),
                content: Some(vec![diff(
                    "tests/shipping.rs",
                    None,
                    "#[test]\nfn free_from_threshold() {\n    assert_eq!(policy().fee(eur(60)), Money::ZERO);\n}\n",
                )]),
                ..ToolCallUpdate::default()
            }),
            options: ask().options,
        }),
        SessionUpdate::State {
            state: AgentState::NeedsPermission,
            reason: None,
        },
    ];
    for (name, appearance) in [("dark", Appearance::Dark), ("light", Appearance::Light)] {
        let updates = updates.clone();
        let image = render_offscreen(appearance, size(1440., 900.), move |window, cx| {
            crate::shell::init(cx);
            let list = ChatList::global(cx).unwrap();
            list.update(cx, |l, cx| {
                l.upsert(info("s3", "Bump the HTTP client", AgentState::Done, 1), cx);
                l.upsert(
                    info("s2", "Fix the flaky login test", AgentState::Working, 2),
                    cx,
                );
                l.upsert(
                    info(
                        "s1",
                        "Free shipping threshold",
                        AgentState::NeedsPermission,
                        3,
                    ),
                    cx,
                );
            });
            let store = Rc::new(MemoryLayoutStore::default());
            cx.new(|cx| {
                let shell = Shell::new(store, DEFAULT_WORKSPACE, window, cx);
                let chat = cx.new(|cx| {
                    let mut chat = ChatTab::new(SessionId("s1".into()), window, cx);
                    chat.set_preview(
                        info(
                            "s1",
                            "Free shipping threshold",
                            AgentState::NeedsPermission,
                            3,
                        ),
                        "Claude Code",
                        cx,
                    );
                    chat.apply((1..).zip(updates).collect(), cx);
                    chat
                });
                shell.dock().update(cx, |dock, cx| {
                    dock.open_tab(coqo_ui::dock::AnyTab::new(chat), window, cx);
                });
                shell
            })
        })
        .expect("offscreen render");
        let path = dir.join(format!("coqo-chat-{name}.png"));
        image.save(&path).expect("save png");
        println!("wrote {}", path.display());
    }
}
