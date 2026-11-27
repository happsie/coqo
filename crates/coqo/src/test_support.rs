// SPDX-License-Identifier: Apache-2.0

//! A real `coqod` served in-process on a temporary socket and data
//! directory, for the UI tests.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use coqo_agents::{
    ManagerOptions, SessionManager, SessionStore, StaticProviders, testing::ScriptedProvider,
};
use coqo_client::{DaemonPaths, RuntimeDir, connect};
use coqo_mod_api::contrib::{AgentProviderInfo, AgentProviderKind};
use coqo_protocol::{
    Client, ClientConfig, Router, Server, ServerConfig,
    daemon::{Shutdown, ShutdownParams},
};
use coqo_storage::{DataDirs, GlobalDb, NewEvent, Retention, Timestamp};
use coqod::IdlePolicy;
use tempfile::TempDir;
use tokio::{runtime::Runtime, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// How long tests wait for the daemon.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// A `coqod` served on a background runtime.
pub struct TestDaemon {
    pub runtime: Runtime,
    pub paths: DaemonPaths,
    pub data: PathBuf,
    serving: Option<JoinHandle<()>>,
}

impl TestDaemon {
    pub fn start(run: PathBuf, data: PathBuf) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut daemon = Self {
            runtime,
            paths: DaemonPaths::at(run),
            data,
            serving: None,
        };
        daemon.serve();
        daemon
    }

    pub fn serve(&mut self) {
        let (paths, data) = (self.paths.clone(), DataDirs::at(&self.data));
        // Binding needs the runtime's reactor.
        let _entered = self.runtime.enter();
        let lock = coqod::daemon::acquire(&paths).unwrap();
        let listener = coqod::daemon::bind(&paths).unwrap();
        let policy = IdlePolicy {
            stop_services_after: None,
            exit_after: None,
        };
        self.serving = Some(self.runtime.spawn(async move {
            coqod::daemon::serve(paths, data, policy, lock, listener)
                .await
                .unwrap();
        }));
    }

    /// `daemon/shutdown`, and wait until it stopped.
    pub fn stop(&mut self) {
        let paths = self.paths.clone();
        let serving = self.serving.take().expect("serving");
        self.runtime.block_on(async move {
            let (client, _) = connect(&paths, ClientConfig::new("coqo-tests", "test"))
                .await
                .unwrap();
            client
                .request::<Shutdown>(ShutdownParams { force: true })
                .await
                .unwrap();
            serving.await.unwrap();
        });
    }

    /// Run `f` with a client of its own (another UI, the CLI).
    pub fn with_client<R>(&self, f: impl AsyncFnOnce(Client) -> R) -> R {
        let paths = self.paths.clone();
        self.runtime.block_on(async move {
            let (client, _) = connect(&paths, ClientConfig::new("coqo-tests", "test"))
                .await
                .unwrap();
            f(client).await
        })
    }

    /// While stopped: append `count` events after everything a client has
    /// seen, then let retention delete every event, so resuming from any
    /// earlier `seq` is a `RESUME_GAP`.
    pub fn prune_event_log(&self, count: usize) {
        assert!(self.serving.is_none(), "stop the daemon first");
        let db = GlobalDb::open(&DataDirs::at(&self.data)).unwrap();
        let log = db.events();
        for _ in 0..count {
            log.append(&NewEvent {
                workspace: None,
                kind: "test.filler",
                payload: b"{}",
            })
            .unwrap();
        }
        let keep_none = Retention {
            max_events: Some(0),
            max_age: None,
        };
        log.prune(&keep_none, Timestamp::now()).unwrap();
        assert!(log.is_empty().unwrap());
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(serving) = self.serving.take() {
            serving.abort();
        }
    }
}

/// The daemon's workspace service and session manager, served on a socket,
/// with scripted agents as the providers (no language servers, buffers or
/// terminals).
pub struct ScriptedDaemon {
    pub runtime: Runtime,
    pub paths: DaemonPaths,
}

impl ScriptedDaemon {
    /// Serve `providers`, as `(id, name, agent)`, with state under `dir`.
    pub fn start(dir: &Path, providers: &[(&str, &str, &ScriptedProvider)]) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let paths = DaemonPaths::at(dir.join("run"));
        let data = DataDirs::at(dir.join("data"));
        std::fs::create_dir_all(data.root()).unwrap();
        let entered = runtime.enter();
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
        let store = SessionStore::open(&data.global_db()).unwrap();
        let manager =
            SessionManager::open(store, Arc::new(registry), ManagerOptions::default()).unwrap();
        let workspaces = Arc::new(coqod::workspace::WorkspaceService::open(data).unwrap());
        let router = coqod::workspace::register(Router::new(), workspaces);
        let router = coqo_agents::rpc::register(router, manager);
        let server = Server::new(router, ServerConfig::new("coqod", "test"));
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        runtime.spawn(coqo_protocol::unix::serve_unix(server, listener));
        drop(entered);
        Self { runtime, paths }
    }

    /// Run `f` with a client of its own (another UI, the CLI).
    pub fn with_client<R>(&self, f: impl AsyncFnOnce(Client) -> R) -> R {
        let paths = self.paths.clone();
        self.runtime.block_on(async move {
            let (client, _) = connect(&paths, ClientConfig::new("coqo-tests", "test"))
                .await
                .unwrap();
            f(client).await
        })
    }
}

/// A project folder and a daemon.
pub struct Fixture {
    pub tmp: TempDir,
    pub project: PathBuf,
    pub daemon: TestDaemon,
}

/// Write `contents` to `root/path`, creating folders.
pub fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("demo-project");
        write(&project, ".gitignore", "target/\n");
        write(&project, "README.md", "# Demo\n");
        write(
            &project,
            "src/main.rs",
            "fn main() {\n\tprintln!(\"hi\");\n}\n",
        );
        write(&project, "src/lib.rs", "pub fn f() {}\n");
        write(&project, "target/out.txt", "build output\n");
        write(
            &project,
            "logo.png",
            [0x89, b'P', b'N', b'G', 0x00, 0xff, 0xfe, 0x00],
        );
        let daemon = TestDaemon::start(tmp.path().join("run"), tmp.path().join("data"));
        Self {
            tmp,
            project,
            daemon,
        }
    }

    /// Another project folder, `name`, with a `README.md`.
    pub fn folder(&self, name: &str) -> PathBuf {
        let path = self.tmp.path().join(name);
        write(&path, "README.md", format!("# {name}\n"));
        path
    }
}

/// The daemon's workspace, session, buffer, question, walkthrough and alert
/// services on a socket, with a scripted agent as the provider "fake".
pub struct AgentDaemon {
    pub runtime: Runtime,
    pub paths: DaemonPaths,
    _stop: CancellationToken,
}

impl AgentDaemon {
    /// Serve `provider` as the "fake" agent, with state under `dir`.
    pub fn start(dir: &Path, provider: &ScriptedProvider) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let paths = DaemonPaths::at(dir.join("run"));
        let data = DataDirs::at(dir.join("data"));
        std::fs::create_dir_all(data.root()).unwrap();
        let stop = CancellationToken::new();
        let entered = runtime.enter();
        let registry = StaticProviders::new().with(
            AgentProviderInfo {
                id: "fake".parse().unwrap(),
                name: "Fake agent".into(),
                kind: AgentProviderKind::Acp,
                description: None,
            },
            Arc::new(provider.clone()),
        );
        let db = data.global_db();
        let store = SessionStore::open(&db).unwrap();
        let manager =
            SessionManager::open(store, Arc::new(registry), ManagerOptions::default()).unwrap();
        let workspaces = Arc::new(coqod::workspace::WorkspaceService::open(data.clone()).unwrap());
        let buffers = coqod::buffers::BufferService::new(
            Arc::clone(&workspaces),
            data,
            coqod::buffers::BufferConfig::default(),
        );
        let questions = coqod::questions::QuestionService::open(
            &db,
            Arc::clone(&workspaces),
            Arc::clone(&buffers),
            Some(manager.clone()),
        )
        .unwrap();
        let walkthroughs = coqod::walkthroughs::WalkthroughService::open(
            &db,
            Arc::clone(&workspaces),
            Arc::clone(&buffers),
            Some(manager.clone()),
        )
        .unwrap();
        let alerts = coqod::alerts::AlertService::open(&db, None).unwrap();
        coqod::alerts::agents::AgentSource::new(
            Arc::clone(&alerts),
            manager.clone(),
            Some(questions.clone()),
        )
        .spawn(stop.clone());
        let router = coqod::workspace::register(Router::new(), workspaces);
        let router = coqo_agents::rpc::register(router, manager);
        let router = coqod::questions::register(router, questions);
        let router = coqod::walkthroughs::register(router, walkthroughs);
        let router = coqod::buffers::register(router, buffers);
        let router = coqod::alerts::register(router, alerts);
        let server = Server::new(router, ServerConfig::new("coqod", "test"));
        RuntimeDir::create(paths.runtime_dir()).unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        runtime.spawn(coqo_protocol::unix::serve_unix(server, listener));
        drop(entered);
        Self {
            runtime,
            paths,
            _stop: stop,
        }
    }

    /// Run `f` with a client of its own (another UI, the CLI, an agent's
    /// bridge).
    pub fn with_client<R>(&self, f: impl AsyncFnOnce(Client) -> R) -> R {
        let paths = self.paths.clone();
        self.runtime.block_on(async move {
            let (client, _) = connect(&paths, ClientConfig::new("coqo-tests", "test"))
                .await
                .unwrap();
            f(client).await
        })
    }
}
