// SPDX-License-Identifier: Apache-2.0

//! A real `coqod` served in-process on a temporary socket and data
//! directory, for the UI tests.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use coqo_client::{DaemonPaths, connect};
use coqo_protocol::{
    Client, ClientConfig,
    daemon::{Shutdown, ShutdownParams},
};
use coqo_storage::{DataDirs, GlobalDb, NewEvent, Retention, Timestamp};
use coqod::IdlePolicy;
use tempfile::TempDir;
use tokio::{runtime::Runtime, task::JoinHandle};

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
