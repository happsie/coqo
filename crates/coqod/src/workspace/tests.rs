// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: a real protocol client talking to the workspace service
//! through `coqo_protocol::Server::serve` over an in-memory pipe.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{
    self as fsm, ChangesParams, EntryKind, FileTooLarge, PathParams, ReadParams,
};
use coqo_protocol::methods::workspace::{
    self as wsm, AddRootParams, EventsParams, OpenParams, RemoveRootParams, RenameParams, RootId,
    SetStateParams, WorkspaceEvent, WorkspaceInfo, WorkspaceParams,
};
use coqo_protocol::{
    Client, ClientConfig, ErrorObject, Method, ProtocolError, Router, Server, ServerConfig,
    Subscription, Topic,
};
use coqo_storage::DataDirs;
use futures_util::StreamExt;
use serde_json::json;
use tempfile::TempDir;

use super::{WatchConfig, WorkspaceService, register};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    data: TempDir,
    svc: Arc<WorkspaceService>,
    client: Client,
}

fn service(data: &Path) -> Arc<WorkspaceService> {
    let config = WatchConfig {
        debounce: Duration::from_millis(50),
        ..WatchConfig::default()
    };
    Arc::new(WorkspaceService::open_with(DataDirs::at(data), config).unwrap())
}

async fn connect(svc: &Arc<WorkspaceService>) -> Client {
    let router = register(Router::new(), Arc::clone(svc));
    let server = Server::new(router, ServerConfig::new("coqod", "test"));
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move { server.serve(server_io).await });
    let (client, _notifications) = Client::connect(client_io, ClientConfig::new("test", "test"))
        .await
        .unwrap();
    client
}

impl Harness {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let svc = service(data.path());
        let client = connect(&svc).await;
        Self { data, svc, client }
    }

    async fn call<M: Method>(&self, params: M::Params) -> M::Result {
        self.client.request::<M>(params).await.unwrap()
    }

    async fn fail<M: Method>(&self, params: M::Params) -> ErrorObject
    where
        M::Result: std::fmt::Debug,
    {
        match self.client.request::<M>(params).await {
            Err(ProtocolError::Rpc(e)) => e,
            other => panic!("{} should fail, got {other:?}", M::NAME),
        }
    }

    async fn open(&self, path: &Path) -> WorkspaceInfo {
        self.call::<wsm::Open>(OpenParams {
            path: Some(path.to_str().unwrap().to_owned()),
            ..OpenParams::default()
        })
        .await
    }
}

async fn next<T: Topic>(sub: &mut Subscription<T>) -> (u64, T::Event) {
    let item = tokio::time::timeout(TIMEOUT, sub.next())
        .await
        .expect("event in time")
        .expect("subscription still open")
        .expect("event ok");
    (item.seq, item.event)
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

fn canonical(p: &Path) -> String {
    fs::canonicalize(p).unwrap().to_str().unwrap().to_owned()
}

fn at(ws: &WorkspaceInfo, root: usize, path: &str) -> PathParams {
    PathParams {
        workspace: ws.id.clone(),
        root: ws.roots[root].id,
        path: path.to_owned(),
    }
}

fn read(ws: &WorkspaceInfo, path: &str, max_bytes: Option<u64>) -> ReadParams {
    ReadParams {
        workspace: ws.id.clone(),
        root: ws.roots[0].id,
        path: path.to_owned(),
        max_bytes,
    }
}

fn id(ws: &WorkspaceInfo) -> WorkspaceParams {
    WorkspaceParams { id: ws.id.clone() }
}

// ---- workspaces -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn open_is_idempotent_per_canonical_path() {
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let ws = h.open(folder.path()).await;
    assert!(ws.id.0.starts_with("ws-"));
    assert!(ws.open);
    assert_eq!(ws.roots.len(), 1);
    assert_eq!(ws.roots[0].path, canonical(folder.path()));
    assert_eq!(
        ws.name,
        folder.path().file_name().unwrap().to_str().unwrap()
    );

    // The same folder spelled differently is the same workspace.
    let spelled = format!("{}/./", folder.path().display());
    let again = h
        .call::<wsm::Open>(OpenParams {
            path: Some(spelled),
            ..OpenParams::default()
        })
        .await;
    assert_eq!(again.id, ws.id);
    #[cfg(unix)]
    {
        let link_dir = tempfile::tempdir().unwrap();
        let link = link_dir.path().join("link");
        std::os::unix::fs::symlink(folder.path(), &link).unwrap();
        assert_eq!(h.open(&link).await.id, ws.id);
    }

    let list = h.call::<wsm::List>(()).await;
    assert_eq!(list.workspaces.len(), 1);
    assert!(list.seq > 0);
    assert_eq!(h.call::<wsm::Get>(id(&ws)).await.id, ws.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn open_validates_params() {
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let both = OpenParams {
        path: Some(folder.path().to_str().unwrap().into()),
        id: Some("ws-x".into()),
        name: None,
    };
    assert_eq!(
        h.fail::<wsm::Open>(both).await.code,
        coqo_protocol::codes::INVALID_PARAMS
    );
    assert_eq!(
        h.fail::<wsm::Open>(OpenParams::default()).await.code,
        coqo_protocol::codes::INVALID_PARAMS
    );
    let missing = OpenParams {
        path: Some(folder.path().join("nope").to_str().unwrap().into()),
        ..OpenParams::default()
    };
    assert_eq!(h.fail::<wsm::Open>(missing).await.code, codes::NOT_FOUND);
    let unknown = OpenParams {
        id: Some("ws-unknown".into()),
        ..OpenParams::default()
    };
    assert_eq!(
        h.fail::<wsm::Open>(unknown).await.code,
        codes::UNKNOWN_WORKSPACE
    );
    assert_eq!(
        h.fail::<wsm::Get>(WorkspaceParams {
            id: "ws-unknown".into()
        })
        .await
        .code,
        codes::UNKNOWN_WORKSPACE
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn close_reopen_rename_and_remove() {
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let ws = h.open(folder.path()).await;

    let closed = h.call::<wsm::Close>(id(&ws)).await;
    assert!(!closed.open);
    // Closing again is a no-op.
    assert!(!h.call::<wsm::Close>(id(&ws)).await.open);

    let reopened = h
        .call::<wsm::Open>(OpenParams {
            id: Some(ws.id.clone()),
            ..OpenParams::default()
        })
        .await;
    assert!(reopened.open);
    assert!(reopened.last_opened_at >= ws.last_opened_at);

    let renamed = h
        .call::<wsm::Rename>(RenameParams {
            id: ws.id.clone(),
            name: "  Backend  ".into(),
        })
        .await;
    assert_eq!(renamed.name, "Backend");
    let empty = RenameParams {
        id: ws.id.clone(),
        name: " ".into(),
    };
    assert_eq!(
        h.fail::<wsm::Rename>(empty).await.code,
        coqo_protocol::codes::INVALID_PARAMS
    );

    // Daemon-side data goes with the workspace; the folder stays.
    let data_dir = DataDirs::at(h.data.path()).workspace_dir(&ws.id.0).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    write(folder.path(), "keep.txt", b"x");
    assert!(h.call::<wsm::Remove>(id(&ws)).await);
    assert!(!h.call::<wsm::Remove>(id(&ws)).await);
    assert!(!data_dir.exists());
    assert!(folder.path().join("keep.txt").exists());
    assert!(h.call::<wsm::List>(()).await.workspaces.is_empty());
    // A removed workspace's folder gets a new workspace (and a new id).
    assert_ne!(h.open(folder.path()).await.id, ws.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_root_workspaces() {
    let h = Harness::new().await;
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let ws = h.open(a.path()).await;
    let add = |path: &Path| AddRootParams {
        id: ws.id.clone(),
        path: path.to_str().unwrap().into(),
        name: Some("lib".into()),
    };
    let two = h.call::<wsm::AddRoot>(add(b.path())).await;
    assert_eq!(two.roots.len(), 2);
    assert_eq!(two.roots[1].path, canonical(b.path()));
    assert_eq!(two.roots[1].name, "lib");
    assert_ne!(two.roots[0].id, two.roots[1].id);
    // Adding the same folder again is a no-op.
    assert_eq!(h.call::<wsm::AddRoot>(add(b.path())).await, two);

    // Opening the second root's folder makes a separate workspace: only a
    // workspace's first root identifies it.
    assert_ne!(h.open(b.path()).await.id, ws.id);
    assert_eq!(h.open(a.path()).await.id, ws.id);

    write(b.path(), "lib.rs", b"pub fn f() {}");
    let listed = h.call::<fsm::ListDir>(at(&two, 1, "")).await;
    assert_eq!(listed.entries[0].name, "lib.rs");

    let first = two.roots[0].id;
    let one = h
        .call::<wsm::RemoveRoot>(RemoveRootParams {
            id: ws.id.clone(),
            root: first,
        })
        .await;
    assert_eq!(one.roots.len(), 1);
    assert_eq!(one.roots[0].id, two.roots[1].id);
    let last = RemoveRootParams {
        id: ws.id.clone(),
        root: one.roots[0].id,
    };
    assert_eq!(
        h.fail::<wsm::RemoveRoot>(last).await.code,
        coqo_protocol::codes::INVALID_PARAMS
    );
    let gone = RemoveRootParams {
        id: ws.id.clone(),
        root: first,
    };
    assert_eq!(
        h.fail::<wsm::RemoveRoot>(gone).await.code,
        codes::UNKNOWN_ROOT
    );
    // Root ids are never reused.
    let readded = h.call::<wsm::AddRoot>(add(a.path())).await;
    assert!(readded.roots.iter().all(|r| r.id != first));
    assert_eq!(
        h.fail::<fsm::ListDir>(PathParams {
            workspace: ws.id.clone(),
            root: first,
            path: String::new(),
        })
        .await
        .code,
        codes::UNKNOWN_ROOT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn state_document() {
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let ws = h.open(folder.path()).await;
    let set = |key: &str, value| SetStateParams {
        id: ws.id.clone(),
        key: key.into(),
        value,
    };
    h.call::<wsm::SetState>(set("tabs", json!(["src/main.rs", "chat:1"])))
        .await;
    h.call::<wsm::SetState>(set("layout", json!({"sidebar": 240})))
        .await;
    h.call::<wsm::SetState>(set("layout", json!({"sidebar": 300})))
        .await;
    let state = h.call::<wsm::GetState>(id(&ws)).await;
    assert_eq!(state.entries.len(), 2);
    assert_eq!(state.entries["layout"], json!({"sidebar": 300}));
    h.call::<wsm::SetState>(set("tabs", json!(null))).await;
    let state = h.call::<wsm::GetState>(id(&ws)).await;
    assert_eq!(state.entries.keys().collect::<Vec<_>>(), vec!["layout"]);

    assert_eq!(
        h.fail::<wsm::SetState>(set("", json!(1))).await.code,
        coqo_protocol::codes::INVALID_PARAMS
    );
    assert_eq!(
        h.fail::<wsm::SetState>(set(&"k".repeat(129), json!(1)))
            .await
            .code,
        coqo_protocol::codes::INVALID_PARAMS
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn workspaces_and_state_survive_a_daemon_restart() {
    let data = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let (before, seq) = {
        let svc = service(data.path());
        let client = connect(&svc).await;
        let ws = client
            .request::<wsm::Open>(OpenParams {
                path: Some(a.path().to_str().unwrap().into()),
                name: Some("App".into()),
                ..OpenParams::default()
            })
            .await
            .unwrap();
        client
            .request::<wsm::AddRoot>(AddRootParams {
                id: ws.id.clone(),
                path: b.path().to_str().unwrap().into(),
                name: None,
            })
            .await
            .unwrap();
        client
            .request::<wsm::SetState>(SetStateParams {
                id: ws.id.clone(),
                key: "tabs".into(),
                value: json!(["README.md"]),
            })
            .await
            .unwrap();
        let list = client.request::<wsm::List>(()).await.unwrap();
        (list.workspaces, list.seq)
    };

    // A new service on the same data directory, as after a daemon restart.
    let svc = service(data.path());
    let client = connect(&svc).await;
    let list = client.request::<wsm::List>(()).await.unwrap();
    assert_eq!(list.workspaces, before);
    assert_eq!(list.seq, seq);
    assert_eq!(list.workspaces[0].name, "App");
    assert_eq!(list.workspaces[0].roots.len(), 2);
    let state = client
        .request::<wsm::GetState>(WorkspaceParams {
            id: before[0].id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(state.entries["tabs"], json!(["README.md"]));

    // The event history survives too, and new seqs continue after it.
    let mut sub = client
        .subscribe::<wsm::Events>(EventsParams::default(), Some(0))
        .await
        .unwrap();
    let kinds: Vec<_> = [
        next(&mut sub).await.1,
        next(&mut sub).await.1,
        next(&mut sub).await.1,
    ]
    .into_iter()
    .map(|e| super::events::kind(&e))
    .collect();
    assert_eq!(
        kinds,
        [
            "workspace.opened",
            "workspace.changed",
            "workspace.stateChanged"
        ]
    );
    client
        .request::<wsm::Close>(WorkspaceParams {
            id: before[0].id.clone(),
        })
        .await
        .unwrap();
    let (s, ev) = next(&mut sub).await;
    assert!(s > seq);
    assert!(matches!(ev, WorkspaceEvent::Closed { .. }));
}

// ---- workspace/events ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn events_are_live_resumable_and_filterable() {
    let h = Harness::new().await;
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut all = h
        .client
        .subscribe::<wsm::Events>(EventsParams::default(), None)
        .await
        .unwrap();
    assert_eq!(all.head(), 0);

    let wa = h.open(a.path()).await;
    let (s1, e1) = next(&mut all).await;
    assert_eq!(
        e1,
        WorkspaceEvent::Opened {
            workspace: wa.clone()
        }
    );

    let snapshot = h.call::<wsm::List>(()).await;
    assert_eq!(snapshot.seq, s1);

    let mut only_a = h
        .client
        .subscribe::<wsm::Events>(
            EventsParams {
                workspace: Some(wa.id.clone()),
            },
            Some(snapshot.seq),
        )
        .await
        .unwrap();

    let wb = h.open(b.path()).await;
    h.call::<wsm::SetState>(SetStateParams {
        id: wa.id.clone(),
        key: "layout".into(),
        value: json!({"split": true}),
    })
    .await;
    let closed = h.call::<wsm::Close>(id(&wa)).await;

    let (s2, e2) = next(&mut all).await;
    assert_eq!(e2, WorkspaceEvent::Opened { workspace: wb });
    let (s3, e3) = next(&mut all).await;
    assert_eq!(
        e3,
        WorkspaceEvent::StateChanged {
            id: wa.id.clone(),
            key: "layout".into(),
            value: json!({"split": true}),
        }
    );
    let (s4, e4) = next(&mut all).await;
    assert_eq!(e4, WorkspaceEvent::Closed { workspace: closed });
    assert!(s1 < s2 && s2 < s3 && s3 < s4);

    // The filtered feed skips b's event.
    assert_eq!(next(&mut only_a).await.0, s3);
    assert_eq!(next(&mut only_a).await.0, s4);

    // Resuming from an earlier seq replays exactly what came after it.
    let mut resumed = h
        .client
        .subscribe::<wsm::Events>(EventsParams::default(), Some(s2))
        .await
        .unwrap();
    assert_eq!(resumed.head(), s4);
    assert_eq!(next(&mut resumed).await.0, s3);
    assert_eq!(next(&mut resumed).await.0, s4);

    // A seq the log has never reached is a resume gap.
    let err = h
        .client
        .subscribe::<wsm::Events>(EventsParams::default(), Some(s4 + 10))
        .await
        .unwrap_err();
    assert_eq!(err.rpc().unwrap().code, coqo_protocol::codes::RESUME_GAP);
}

#[tokio::test(flavor = "multi_thread")]
async fn events_report_a_gap_when_retention_pruned_them() {
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let ws = h.open(folder.path()).await;
    for i in 0..3 {
        h.call::<wsm::SetState>(SetStateParams {
            id: ws.id.clone(),
            key: "k".into(),
            value: json!(i),
        })
        .await;
    }
    {
        let db = h.svc.db();
        let retention = coqo_storage::Retention {
            max_events: Some(1),
            max_age: None,
        };
        db.events()
            .prune(&retention, coqo_storage::Timestamp::now())
            .unwrap();
    }
    let err = h
        .client
        .subscribe::<wsm::Events>(EventsParams::default(), Some(0))
        .await
        .unwrap_err();
    assert_eq!(err.rpc().unwrap().code, coqo_protocol::codes::RESUME_GAP);
    // From the head it works.
    let head = h.call::<wsm::List>(()).await.seq;
    h.client
        .subscribe::<wsm::Events>(EventsParams::default(), Some(head))
        .await
        .unwrap();
}

// ---- fs/* ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn list_stat_and_read() {
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path();
    write(root, ".gitignore", b"target/\n*.log\n");
    write(root, "src/main.rs", b"fn main() {}\n");
    write(root, "src/lib.rs", b"");
    write(root, "Readme.md", b"# hi\n");
    write(root, "build.log", b"noise");
    write(root, "target/debug/app", b"\x7fELF\xff\xfe");
    write(root, ".git/HEAD", b"ref: refs/heads/main\n");
    write(root, "big.txt", &vec![b'a'; 2048]);
    let ws = h.open(root).await;

    let top = h.call::<fsm::ListDir>(at(&ws, 0, "")).await.entries;
    let names: Vec<_> = top.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        [
            ".git",
            "src",
            "target",
            ".gitignore",
            "big.txt",
            "build.log",
            "Readme.md"
        ]
    );
    let entry = |n: &str| top.iter().find(|e| e.name == n).unwrap();
    assert_eq!(entry("src").kind, EntryKind::Dir);
    assert!(!entry("src").ignored);
    assert!(entry("target").ignored);
    assert!(entry(".git").ignored);
    assert!(entry("build.log").ignored);
    assert!(!entry("Readme.md").ignored);
    assert_eq!(entry("Readme.md").size, 5);
    assert!(entry("Readme.md").mtime.is_some());
    assert!(!entry("Readme.md").symlink);

    let deep = h.call::<fsm::ListDir>(at(&ws, 0, "target/debug")).await;
    assert!(deep.entries.iter().all(|e| e.ignored));

    let stat = h.call::<fsm::Stat>(at(&ws, 0, "src/main.rs")).await;
    assert_eq!(stat.kind, EntryKind::File);
    assert_eq!(stat.size, 13);
    assert!(!stat.ignored);
    assert!(
        h.call::<fsm::Stat>(at(&ws, 0, "target/debug/app"))
            .await
            .ignored
    );
    assert_eq!(
        h.call::<fsm::Stat>(at(&ws, 0, "")).await.kind,
        EntryKind::Dir
    );

    let text = h
        .call::<fsm::ReadFile>(read(&ws, "./src//main.rs", None))
        .await;
    assert_eq!(text.text.as_deref(), Some("fn main() {}\n"));
    assert!(!text.binary);
    assert_eq!(text.size, 13);
    let bin = h
        .call::<fsm::ReadFile>(read(&ws, "target/debug/app", None))
        .await;
    assert!(bin.binary);
    assert_eq!(bin.text, None);
    assert_eq!(bin.size, 6);

    let err = h
        .fail::<fsm::ReadFile>(read(&ws, "big.txt", Some(1024)))
        .await;
    assert_eq!(err.code, codes::FILE_TOO_LARGE);
    let data: FileTooLarge = serde_json::from_value(err.data.unwrap()).unwrap();
    assert_eq!(
        data,
        FileTooLarge {
            size: 2048,
            limit: 1024
        }
    );
    assert_eq!(
        h.fail::<fsm::ReadFile>(read(&ws, "big.txt", Some(fsm::MAX_READ_LIMIT + 1)))
            .await
            .code,
        coqo_protocol::codes::INVALID_PARAMS
    );
    assert_eq!(
        h.fail::<fsm::ReadFile>(read(&ws, "src", None)).await.code,
        codes::NOT_A_FILE
    );
    assert_eq!(
        h.fail::<fsm::ListDir>(at(&ws, 0, "Readme.md")).await.code,
        codes::NOT_A_DIRECTORY
    );
    assert_eq!(
        h.fail::<fsm::Stat>(at(&ws, 0, "missing.rs")).await.code,
        codes::NOT_FOUND
    );
    assert_eq!(
        h.fail::<fsm::Stat>(PathParams {
            workspace: "ws-unknown".into(),
            root: RootId(1),
            path: String::new(),
        })
        .await
        .code,
        codes::UNKNOWN_WORKSPACE
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn paths_cannot_escape_the_workspace() {
    let h = Harness::new().await;
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repo");
    write(&root, "a.txt", b"inside");
    write(outer.path(), "secret.txt", b"outside");
    let ws = h.open(&root).await;
    for bad in ["../secret.txt", "..", "a/../../secret.txt", "/etc/passwd"] {
        assert_eq!(
            h.fail::<fsm::ReadFile>(read(&ws, bad, None)).await.code,
            codes::PATH_OUTSIDE_WORKSPACE,
            "{bad}"
        );
        assert_eq!(
            h.fail::<fsm::Stat>(at(&ws, 0, bad)).await.code,
            codes::PATH_OUTSIDE_WORKSPACE,
            "{bad}"
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn symlinks_are_confined_to_the_roots() {
    use std::os::unix::fs::symlink;
    let h = Harness::new().await;
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repo");
    let other = outer.path().join("other");
    write(&root, "real/a.txt", b"inside");
    write(outer.path(), "secret/key.txt", b"outside");
    write(&other, "b.txt", b"second root");
    symlink(outer.path().join("secret/key.txt"), root.join("leak.txt")).unwrap();
    symlink(outer.path().join("secret"), root.join("leakdir")).unwrap();
    symlink(root.join("real/a.txt"), root.join("alias.txt")).unwrap();
    symlink("real", root.join("realdir")).unwrap();
    symlink(root.join("missing"), root.join("broken")).unwrap();
    symlink(other.join("b.txt"), root.join("to-other.txt")).unwrap();
    let ws = h.open(&root).await;

    let entries = h.call::<fsm::ListDir>(at(&ws, 0, "")).await.entries;
    let entry = |n: &str| entries.iter().find(|e| e.name == n).unwrap().clone();
    assert_eq!(entry("leak.txt").kind, EntryKind::Other);
    assert!(entry("leak.txt").symlink);
    assert_eq!(entry("leak.txt").size, 0);
    assert_eq!(entry("leakdir").kind, EntryKind::Other);
    assert_eq!(entry("broken").kind, EntryKind::Other);
    assert_eq!(entry("alias.txt").kind, EntryKind::File);
    assert_eq!(entry("alias.txt").size, 6);
    assert_eq!(entry("realdir").kind, EntryKind::Dir);
    assert!(entry("realdir").symlink);
    assert_eq!(
        entry("to-other.txt").kind,
        EntryKind::Other,
        "not a root yet"
    );

    for bad in ["leak.txt", "leakdir/key.txt", "to-other.txt"] {
        assert_eq!(
            h.fail::<fsm::ReadFile>(read(&ws, bad, None)).await.code,
            codes::PATH_OUTSIDE_WORKSPACE,
            "{bad}"
        );
    }
    assert_eq!(
        h.fail::<fsm::ListDir>(at(&ws, 0, "leakdir")).await.code,
        codes::PATH_OUTSIDE_WORKSPACE
    );
    assert_eq!(
        h.fail::<fsm::Stat>(at(&ws, 0, "broken")).await.code,
        codes::NOT_FOUND
    );
    let alias = h.call::<fsm::ReadFile>(read(&ws, "alias.txt", None)).await;
    assert_eq!(alias.text.as_deref(), Some("inside"));
    let stat = h.call::<fsm::Stat>(at(&ws, 0, "realdir")).await;
    assert!(stat.symlink);
    assert_eq!(stat.kind, EntryKind::Dir);
    assert_eq!(
        h.call::<fsm::ListDir>(at(&ws, 0, "realdir")).await.entries[0].name,
        "a.txt"
    );

    // Once `other` is a root too, a link into it stays inside the workspace.
    h.call::<wsm::AddRoot>(AddRootParams {
        id: ws.id.clone(),
        path: other.to_str().unwrap().into(),
        name: None,
    })
    .await;
    let linked = h
        .call::<fsm::ReadFile>(read(&ws, "to-other.txt", None))
        .await;
    assert_eq!(linked.text.as_deref(), Some("second root"));
}

// ---- fs/changes ---------------------------------------------------------------

async fn changes_until(sub: &mut Subscription<fsm::Changes>, want: &str) -> Vec<fsm::FsChanges> {
    let mut seen = Vec::new();
    loop {
        let (_, batch) = next(sub).await;
        let done = batch.rescan || batch.paths.iter().any(|p| p == want);
        seen.push(batch);
        if done {
            return seen;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_reports_debounced_changes() {
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path();
    write(root, ".gitignore", b"target/\n");
    write(root, "src/main.rs", b"");
    fs::create_dir_all(root.join("target/debug")).unwrap();
    let ws = h.open(root).await;
    let mut sub = h
        .client
        .subscribe::<fsm::Changes>(
            ChangesParams {
                workspace: ws.id.clone(),
            },
            None,
        )
        .await
        .unwrap();

    // Noise first: inside an ignored dir and inside .git.
    write(root, "target/debug/app.o", b"x");
    write(root, ".git/index", b"x");
    write(root, "src/new.rs", b"fn f() {}");
    let seen = changes_until(&mut sub, "src/new.rs").await;
    let all: Vec<&String> = seen.iter().flat_map(|b| &b.paths).collect();
    assert!(seen.iter().all(|b| b.root == ws.roots[0].id));
    // Match whole path components: `.gitignore` is not inside `.git`, and
    // late setup events (FSEvents may deliver them after we subscribe) can
    // legitimately mention `.gitignore`, `src/main.rs` or the `target` dir
    // itself (an ignored entry appearing in a visible directory).
    let inside = |p: &str, dir: &str| {
        p.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    assert!(
        !all.iter()
            .any(|p| inside(p, "target") || inside(p, ".git") || *p == ".git"),
        "{all:?}"
    );

    fs::remove_file(root.join("src/new.rs")).unwrap();
    changes_until(&mut sub, "src/new.rs").await;

    // A second subscriber shares the watcher.
    let mut second = h
        .client
        .subscribe::<fsm::Changes>(
            ChangesParams {
                workspace: ws.id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    write(root, "Cargo.toml", b"[package]");
    changes_until(&mut second, "Cargo.toml").await;
    changes_until(&mut sub, "Cargo.toml").await;

    // Closing the workspace ends the subscriptions.
    h.call::<wsm::Close>(id(&ws)).await;
    let end = tokio::time::timeout(TIMEOUT, async {
        while let Some(item) = sub.next().await {
            item.unwrap();
        }
    })
    .await;
    assert!(end.is_ok(), "subscription ended");
    let err = h
        .client
        .subscribe::<fsm::Changes>(
            ChangesParams {
                workspace: ws.id.clone(),
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.rpc().unwrap().code, codes::WORKSPACE_CLOSED);
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_follows_added_roots() {
    let h = Harness::new().await;
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let ws = h.open(a.path()).await;
    let mut sub = h
        .client
        .subscribe::<fsm::Changes>(
            ChangesParams {
                workspace: ws.id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    let two = h
        .call::<wsm::AddRoot>(AddRootParams {
            id: ws.id.clone(),
            path: b.path().to_str().unwrap().into(),
            name: None,
        })
        .await;
    write(b.path(), "added.txt", b"x");
    let seen = changes_until(&mut sub, "added.txt").await;
    let last = seen.last().unwrap();
    assert!(last.rescan || last.root == two.roots[1].id);
}

/// Acceptance benchmark for #22: a 200k-file tree. Run with
/// `cargo test -p coqod --release -- --ignored --nocapture watcher_200k`.
///
/// Measures the time to set up the recursive watch, the daemon's CPU time
/// while the tree is idle, and the time to report a burst of 10k changes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: creates 200k files"]
async fn watcher_200k_files_benchmark() {
    use std::time::Instant;
    let h = Harness::new().await;
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path();
    let t = Instant::now();
    for d in 0..2_000 {
        let dir = root.join(format!("pkg{}/mod{}", d / 50, d));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            fs::write(dir.join(format!("f{f}.rs")), b"").unwrap();
        }
    }
    eprintln!("created 200k files in {:?}", t.elapsed());
    let ws = h.open(root).await;

    let cpu0 = cpu_time();
    let t = Instant::now();
    let mut sub = h
        .client
        .subscribe::<fsm::Changes>(
            ChangesParams {
                workspace: ws.id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    eprintln!(
        "watch setup: {:?} wall, {:?} cpu",
        t.elapsed(),
        cpu_time().zip(cpu0).map(|(a, b)| a - b)
    );

    let cpu0 = cpu_time();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let idle = cpu_time().zip(cpu0).map(|(a, b)| a - b);
    eprintln!("idle for 3 s: {idle:?} cpu");
    if let Some(idle) = idle {
        assert!(idle < Duration::from_millis(300), "idle watcher burns CPU");
    }

    let cpu0 = cpu_time();
    let t = Instant::now();
    for d in 0..100 {
        let dir = root.join(format!("pkg{}/mod{}", d / 50, d));
        for f in 0..100 {
            fs::write(dir.join(format!("f{f}.rs")), b"changed").unwrap();
        }
    }
    let mut reported = 0;
    let mut batches = 0;
    while reported < 10_000 {
        let (_, b) = next(&mut sub).await;
        batches += 1;
        if b.rescan {
            break;
        }
        reported += b.paths.len();
    }
    eprintln!(
        "10k changes reported in {batches} batches, {:?} wall, {:?} cpu",
        t.elapsed(),
        cpu_time().zip(cpu0).map(|(a, b)| a - b)
    );
}

/// This process's user + system CPU time (Linux only).
fn cpu_time() -> Option<Duration> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the command name, which is in parentheses.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    // USER_HZ is 100 on every mainstream Linux configuration.
    Some(Duration::from_millis(ticks * 10))
}

// ---- token scopes -----------------------------------------------------------

async fn connect_with_token(
    svc: &Arc<WorkspaceService>,
    auth: &crate::auth::Auth,
    token: coqo_protocol::SessionToken,
) -> Client {
    let router = register(Router::new(), Arc::clone(svc));
    let mut config = ServerConfig::new("coqod", "test");
    config.authenticator = Some(Arc::new(auth.clone()));
    let server = Server::new(router, config);
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move { server.serve(server_io).await });
    let mut config = ClientConfig::new("test", "test");
    config.token = Some(token);
    Client::connect(client_io, config).await.unwrap().0
}

fn assert_forbidden(e: &ProtocolError) {
    assert_eq!(
        e.rpc().map(|e| e.code),
        Some(coqo_protocol::codes::FORBIDDEN),
        "{e}"
    );
}

async fn forbidden<M: Method>(client: &Client, params: M::Params)
where
    M::Result: std::fmt::Debug,
{
    match client.request::<M>(params).await {
        Err(e) => assert_forbidden(&e),
        Ok(r) => panic!("{} should be forbidden, got {r:?}", M::NAME),
    }
}

/// A token scoped to workspace A cannot call into workspace B, nor manage
/// workspaces; the daemon token can do everything.
#[tokio::test]
async fn a_session_token_stays_in_its_workspace() {
    let data = tempfile::tempdir().unwrap();
    let svc = service(data.path());
    let auth = crate::auth::Auth::generate().unwrap();
    let admin = connect_with_token(&svc, &auth, auth.daemon_token()).await;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(dir_a.path(), "a.txt", b"in a");
    write(dir_b.path(), "b.txt", b"in b");
    let open = |dir: &TempDir| OpenParams {
        path: Some(dir.path().to_str().unwrap().to_owned()),
        ..OpenParams::default()
    };
    let a = admin.request::<wsm::Open>(open(&dir_a)).await.unwrap();
    let b = admin.request::<wsm::Open>(open(&dir_b)).await.unwrap();

    let token = auth.mint_session(a.id.0.clone(), "session-1").unwrap();
    let agent = connect_with_token(&svc, &auth, token).await;

    // Its own workspace works.
    agent
        .request::<fsm::ReadFile>(read(&a, "a.txt", None))
        .await
        .unwrap();
    agent.request::<fsm::ListDir>(at(&a, 0, "")).await.unwrap();
    agent
        .request::<wsm::Get>(WorkspaceParams { id: a.id.clone() })
        .await
        .unwrap();
    agent
        .subscribe::<wsm::Events>(
            EventsParams {
                workspace: Some(a.id.clone()),
            },
            None,
        )
        .await
        .unwrap();

    // Workspace B does not.
    forbidden::<fsm::ReadFile>(&agent, read(&b, "b.txt", None)).await;
    forbidden::<fsm::ListDir>(&agent, at(&b, 0, "")).await;
    forbidden::<fsm::Stat>(&agent, at(&b, 0, "b.txt")).await;
    forbidden::<wsm::Get>(&agent, WorkspaceParams { id: b.id.clone() }).await;
    forbidden::<wsm::GetState>(&agent, WorkspaceParams { id: b.id.clone() }).await;
    forbidden::<wsm::SetState>(
        &agent,
        SetStateParams {
            id: b.id.clone(),
            key: "k".into(),
            value: json!(1),
        },
    )
    .await;
    for params in [
        EventsParams {
            workspace: Some(b.id.clone()),
        },
        EventsParams::default(),
    ] {
        let e = agent
            .subscribe::<wsm::Events>(params, None)
            .await
            .unwrap_err();
        assert_forbidden(&e);
    }
    let e = agent
        .subscribe::<fsm::Changes>(
            ChangesParams {
                workspace: b.id.clone(),
            },
            None,
        )
        .await
        .unwrap_err();
    assert_forbidden(&e);

    // Managing workspaces needs the daemon token, even for its own.
    forbidden::<wsm::List>(&agent, ()).await;
    forbidden::<wsm::Open>(&agent, open(&dir_b)).await;
    forbidden::<wsm::AddRoot>(
        &agent,
        AddRootParams {
            id: a.id.clone(),
            path: dir_b.path().to_str().unwrap().to_owned(),
            name: None,
        },
    )
    .await;
    forbidden::<wsm::Close>(&agent, WorkspaceParams { id: a.id.clone() }).await;

    // The daemon token reaches both.
    admin
        .request::<fsm::ReadFile>(read(&b, "b.txt", None))
        .await
        .unwrap();
    assert_eq!(
        admin
            .request::<wsm::List>(())
            .await
            .unwrap()
            .workspaces
            .len(),
        2
    );
}
