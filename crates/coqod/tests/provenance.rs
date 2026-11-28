// SPDX-License-Identifier: Apache-2.0

//! Who changed which file (issue #286), with the scripted fake ACP agent
//! through the ACP host, the session manager, the buffers' `AgentFs` and
//! the provenance service, as the daemon wires them: an
//! `fs/write_text_file`, and a tool call whose diff Claude Code-style
//! writes the file itself. No network.

use std::sync::Arc;
use std::time::Duration;

use coqo_acp_host::{AcpOptions, AcpProvider, AgentCommand, FsHandle};
use coqo_agents::model::*;
use coqo_agents::{ManagerOptions, SessionManager, SessionStore, StaticProviders};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::methods::workspace::{OpenParams, RootId, WorkspaceId};
use coqo_storage::DataDirs;
use coqod::agents::BufferFs;
use coqod::buffers::{BufferConfig, BufferService};
use coqod::provenance::{self, Provenance};
use coqod::workspace::WorkspaceService;
use futures_util::StreamExt;

const TIMEOUT: Duration = Duration::from_secs(20);
const FAKE: &str = env!("CARGO_BIN_EXE_coqod-fake-acp-agent");

struct Env {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    workspace: WorkspaceId,
    manager: SessionManager,
    provenance: Arc<Provenance>,
}

impl Env {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("ws");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
        let data = DataDirs::at(dir.path().join("data"));
        let workspaces = Arc::new(WorkspaceService::open(data.clone()).unwrap());
        let workspace = workspaces
            .open_workspace(OpenParams {
                path: Some(root.to_string_lossy().into_owned()),
                ..OpenParams::default()
            })
            .await
            .unwrap()
            .id;
        let buffers = BufferService::new(
            Arc::clone(&workspaces),
            data.clone(),
            BufferConfig::default(),
        );
        let (signals, rx) = provenance::channel();
        let fs = FsHandle::new();
        let mut options = AcpOptions::new("fake-acp");
        options.start_timeout = TIMEOUT;
        options.request_timeout = TIMEOUT;
        options.fs = fs.clone();
        let provider = AcpProvider::command(AgentCommand::new(FAKE), options);
        let info = AgentProviderInfo {
            id: "fake".parse().unwrap(),
            name: "Fake ACP".into(),
            kind: AgentProviderKind::Acp,
            description: None,
        };
        let providers = StaticProviders::new().with(info, Arc::new(provider));
        let store = SessionStore::open(&data.global_db()).unwrap();
        let manager = SessionManager::open(
            store,
            Arc::new(providers),
            ManagerOptions {
                observer: Some(signals.observer()),
                ..ManagerOptions::default()
            },
        )
        .unwrap();
        // Set after the provider exists, as `routes::router` does.
        fs.set(Arc::new(
            BufferFs::new(Arc::clone(&buffers)).with_provenance(signals.clone()),
        ));
        let provenance = Provenance::open(
            &data.global_db(),
            workspaces,
            Some(buffers),
            manager.clone(),
            &signals,
            rx,
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            workspace,
            manager,
            provenance,
        }
    }

    async fn session(&self, title: &str) -> SessionId {
        let params = CreateSessionParams {
            provider: "fake".into(),
            cwd: self.root.display().to_string(),
            workspace: Some(self.workspace.0.clone()),
            title: Some(title.into()),
            mode: None,
            from: None,
        };
        self.manager.create(params).await.unwrap().id
    }

    /// Runs `script` as a turn, allowing what the agent asks, to its end.
    async fn turn(&self, id: &SessionId, script: &str) {
        let since = self.manager.snapshot(id).unwrap().head;
        self.manager
            .prompt(id, vec![ContentBlock::text(script)])
            .await
            .unwrap();
        let mut feed = self.manager.updates(id, Some(since)).unwrap().events;
        tokio::time::timeout(TIMEOUT, async {
            while let Some(item) = feed.next().await {
                match item.unwrap().event {
                    SessionUpdate::PermissionRequested(request) => {
                        let allow = request
                            .options
                            .iter()
                            .find(|o| o.kind == PermissionOptionKind::AllowOnce)
                            .unwrap();
                        self.manager
                            .respond_permission(
                                id,
                                request.id,
                                PermissionOutcome::Selected {
                                    option_id: allow.id.clone(),
                                },
                            )
                            .unwrap();
                    }
                    SessionUpdate::TurnEnded { .. } => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("the turn ended");
        self.provenance.settled().await;
    }

    async fn changed_by(&self, path: &str) -> Vec<SessionChange> {
        self.provenance
            .changed_by(ChangedByParams {
                workspace: self.workspace.clone(),
                root: RootId(1),
                paths: vec![path.into()],
            })
            .await
            .unwrap()
            .sessions
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_writes_and_tool_call_diffs_are_the_sessions_changes() {
    let env = Env::new().await;
    let writer = env.session("Write the notes").await;
    let editor = env.session("Volume discount").await;

    // `fs/write_text_file`: a new file, written to disk by the host.
    let notes = env.root.join("NOTES.md");
    env.turn(&writer, &format!("write {} hello", notes.display()))
        .await;
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "hello");
    let got = env.changed_by("NOTES.md").await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].session, writer);
    assert_eq!(got[0].strength, ChangeStrength::Write);
    assert_eq!(got[0].title.as_deref(), Some("Write the notes"));
    assert_eq!(got[0].provider, "fake");

    // A tool call with a diff, allowed; the agent writes the file itself.
    let lib = env.root.join("src/lib.rs");
    env.turn(&editor, &format!("edit {} fn b() {{}}", lib.display()))
        .await;
    assert_eq!(std::fs::read_to_string(&lib).unwrap(), "fn b() {}");
    let got = env.changed_by("src/lib.rs").await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].session, editor);
    assert_eq!(got[0].strength, ChangeStrength::Write);
    assert_eq!(got[0].paths, ["src/lib.rs"]);

    // The other file is still the first session's.
    assert_eq!(env.changed_by("NOTES.md").await[0].session, writer);
}
