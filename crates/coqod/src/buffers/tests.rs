// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: `ClientReplica`s talking to the buffer service through
//! `coqo_protocol::Server::serve` over in-memory pipes, with real files, the
//! real file watcher and the real journal.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use coqo_buffers::{ClientReplica, UndoGrouping};
use coqo_protocol::methods::buffer::{
    self as bufm, BufferChange, BufferId, ChangesParams, CloseParams, EditParams,
    EditRejectedParams, OpenParams, OpenResult, OriginKind, RevertParams, SaveParams, TextEdit,
};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceId};
use coqo_protocol::{
    Client, ClientConfig, ErrorObject, Notifications, ProtocolError, Router, Server, ServerConfig,
    Subscription,
};
use coqo_storage::DataDirs;
use futures_util::{FutureExt as _, StreamExt as _};
use proptest::prelude::*;
use tempfile::TempDir;

use super::{BufferConfig, BufferService, register};
use crate::workspace::{self, WatchConfig, WorkspaceService};

const TIMEOUT: Duration = Duration::from_secs(10);

/// [`BufferConfig::truncate_settle`] in these tests: long, so a test writer
/// pausing briefly mid-write stays well inside it even on a loaded runner.
const SETTLE: Duration = Duration::from_millis(500);

struct Daemon {
    data: TempDir,
    folder: TempDir,
    workspaces: Arc<WorkspaceService>,
    buffers: Arc<BufferService>,
    workspace: WorkspaceId,
    root: RootId,
}

fn config() -> BufferConfig {
    BufferConfig {
        unload_after: Duration::from_millis(300),
        truncate_settle: SETTLE,
        ..BufferConfig::default()
    }
}

impl Daemon {
    async fn new(files: &[(&str, &str)]) -> Self {
        let data = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = folder.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let mut d = Self::start(data, folder, None).await;
        let client = d.connect().await;
        let info = client
            .request::<wsm::Open>(wsm::OpenParams {
                path: Some(d.folder.path().to_str().unwrap().to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        d.workspace = info.id;
        d.root = info.roots[0].id;
        d
    }

    /// A daemon on existing directories (a restart).
    async fn start(data: TempDir, folder: TempDir, ws: Option<(WorkspaceId, RootId)>) -> Self {
        let watch = WatchConfig {
            debounce: Duration::from_millis(30),
            ..WatchConfig::default()
        };
        let dirs = DataDirs::at(data.path());
        let workspaces = Arc::new(WorkspaceService::open_with(dirs.clone(), watch).unwrap());
        let buffers = BufferService::new(Arc::clone(&workspaces), dirs, config());
        let (workspace, root) = ws.unwrap_or((WorkspaceId(String::new()), RootId(0)));
        Self {
            data,
            folder,
            workspaces,
            buffers,
            workspace,
            root,
        }
    }

    async fn restart(self) -> Self {
        self.buffers.flush().await;
        let ws = Some((self.workspace.clone(), self.root));
        Self::start(self.data, self.folder, ws).await
    }

    async fn connect(&self) -> Client {
        self.connect_with_notes().await.0
    }

    async fn connect_with_notes(&self) -> (Client, Notifications) {
        let router = workspace::register(Router::new(), Arc::clone(&self.workspaces));
        let router = register(router, Arc::clone(&self.buffers));
        let server = Server::new(router, ServerConfig::new("coqod", "test"));
        let (client_io, server_io) = tokio::io::duplex(1 << 22);
        tokio::spawn(async move { server.serve(server_io).await });
        Client::connect(client_io, ClientConfig::new("test", "test"))
            .await
            .unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.folder.path().join(name)
    }

    fn open_params(&self, name: &str) -> OpenParams {
        OpenParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: name.to_owned(),
            replica: None,
        }
    }

    async fn window(&self, name: &str) -> Window {
        let (client, notes) = self.connect_with_notes().await;
        Window::open(client, notes, self.open_params(name)).await
    }
}

/// One editor window: a connection, a replica, and an editor mirror that
/// changes only through the edits the replica hands out.
struct Window {
    client: Client,
    notes: Notifications,
    sub: Subscription<bufm::Changes>,
    replica: ClientReplica,
    editor: String,
    open: OpenParams,
}

fn apply(s: &mut String, edits: &[TextEdit]) {
    for e in edits.iter().rev() {
        s.replace_range(e.start..e.end, &e.text);
    }
}

impl Window {
    async fn open(client: Client, notes: Notifications, open: OpenParams) -> Self {
        let r = client.request::<bufm::Open>(open.clone()).await.unwrap();
        let sub = client
            .subscribe::<bufm::Changes>(ChangesParams { buffer: r.buffer }, Some(r.rev))
            .await
            .unwrap();
        Self {
            editor: r.text.clone(),
            replica: ClientReplica::new(&r),
            client,
            notes,
            sub,
            open,
        }
    }

    fn buffer(&self) -> BufferId {
        self.replica.buffer()
    }

    async fn send(&self, out: Option<EditParams>) {
        if let Some(out) = out {
            self.client.notify::<bufm::Edit>(out).await.unwrap();
        }
    }

    /// Type: apply locally and send.
    async fn edit(&mut self, e: TextEdit) {
        apply(&mut self.editor, std::slice::from_ref(&e));
        let out = self
            .replica
            .local_edit(&[e], UndoGrouping::NewGroup)
            .unwrap();
        self.send(out).await;
    }

    fn take(&mut self, c: &BufferChange) -> Option<EditParams> {
        let inc = self.replica.receive(c).unwrap();
        apply(&mut self.editor, &inc.edits);
        inc.outgoing
    }

    /// Wait for the next change and process it.
    async fn next(&mut self) -> BufferChange {
        let c = tokio::time::timeout(TIMEOUT, self.sub.next())
            .await
            .expect("timed out waiting for buffer/changes")
            .expect("subscription ended")
            .unwrap()
            .event;
        let out = self.take(&c);
        self.send(out).await;
        c
    }

    /// Process changes until the replica is at `rev` with nothing in flight.
    async fn sync_to(&mut self, rev: u64) {
        while self.replica.rev() < rev || !self.replica.is_synced() {
            self.next().await;
        }
        assert_eq!(self.editor, self.replica.text().to_string());
    }

    /// Wait for a change matching `f`, processing everything before it.
    async fn wait_for(&mut self, f: impl Fn(&BufferChange) -> bool) -> BufferChange {
        loop {
            let c = self.next().await;
            if f(&c) {
                return c;
            }
        }
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Replace a file the way many editors do (write aside, rename), so the
/// watcher never sees it half-written. Tests about writers that truncate
/// first use `fs::write`.
fn replace_file(path: &std::path::Path, text: &str) {
    super::disk::write_atomic(path, text.as_bytes()).unwrap();
}

fn rpc_error<T: std::fmt::Debug>(r: Result<T, ProtocolError>) -> ErrorObject {
    match r {
        Err(ProtocolError::Rpc(e)) => e,
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_edit_acknowledge_and_share_between_windows() {
    let d = Daemon::new(&[("src/main.rs", "fn main() {}\n")]).await;
    let mut a = d.window("src/main.rs").await;
    let mut b = d.window("src/main.rs").await;
    assert_eq!(a.buffer(), b.buffer(), "one buffer per file");
    assert_ne!(a.replica.replica(), b.replica.replica());
    assert_eq!(a.editor, "fn main() {}\n");

    a.edit(TextEdit::insert(11, " println!(\"hi\"); ")).await;
    b.edit(TextEdit::insert(0, "// b\n")).await;
    let head = 2;
    a.sync_to(head).await;
    b.sync_to(head).await;
    assert_eq!(a.editor, "// b\nfn main() { println!(\"hi\"); }\n");
    assert_eq!(a.editor, b.editor);
    assert!(a.replica.is_dirty());
    assert_eq!(d.buffers.text(a.buffer()).unwrap(), a.editor);
    // Nothing was written to disk.
    assert_eq!(
        fs::read_to_string(d.path("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_writes_the_file_and_its_own_write_is_no_change() {
    let d = Daemon::new(&[("a.txt", "\u{feff}one\r\ntwo\r\n")]).await;
    let mut w = d.window("a.txt").await;
    assert_eq!(
        w.editor, "one\r\ntwo\r\n",
        "the BOM is not part of the text"
    );
    assert_eq!(w.replica.line_ending(), bufm::LineEnding::Crlf);
    w.edit(TextEdit::insert(5, "1.5\r\n")).await;
    w.sync_to(1).await;
    let saved = w
        .client
        .request::<bufm::Save>(SaveParams {
            buffer: w.buffer(),
            force: false,
        })
        .await
        .unwrap();
    let c = w.wait_for(|c| c.origin.kind == OriginKind::Save).await;
    assert_eq!(c.rev, saved.saved_rev);
    assert!(!c.dirty);
    assert!(!w.replica.is_dirty());
    assert_eq!(
        fs::read(d.path("a.txt")).unwrap(),
        "\u{feff}one\r\n1.5\r\ntwo\r\n".as_bytes(),
        "the BOM and line endings are written back"
    );
    // The watcher sees our own write; it must not become a change.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(d.buffers.rev(w.buffer()), Some(saved.saved_rev));
    assert!(w.sub.next().now_or_never().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_changes_reload_clean_buffers_and_merge_into_dirty_ones() {
    let d = Daemon::new(&[("f.txt", "one\ntwo\nthree\n")]).await;
    let mut w = d.window("f.txt").await;
    // Clean: a plain reload.
    replace_file(&d.path("f.txt"), "one\ntwo\nthree\nfour\n");
    let c = w.wait_for(|c| c.origin.kind == OriginKind::Disk).await;
    assert!(!c.dirty);
    assert_eq!(w.editor, "one\ntwo\nthree\nfour\n");
    // Dirty: the user's unsaved edit survives an external rewrite.
    w.edit(TextEdit::insert(3, "!")).await;
    w.sync_to(c.rev + 1).await;
    replace_file(&d.path("f.txt"), "one\nTWO\nthree\nfour\n");
    let c = w.wait_for(|c| c.origin.kind == OriginKind::Disk).await;
    assert!(c.dirty);
    assert!(!c.conflict);
    assert_eq!(w.editor, "one!\nTWO\nthree\nfour\n");
    // Saving writes both.
    w.client
        .request::<bufm::Save>(SaveParams {
            buffer: w.buffer(),
            force: false,
        })
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(d.path("f.txt")).unwrap(),
        "one!\nTWO\nthree\nfour\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_read_empty_mid_write_is_not_merged_empty() {
    use std::io::Write as _;
    let d = Daemon::new(&[("w.rs", "fn a() {}\n")]).await;
    let path = d.path("w.rs");
    let mut w = d.window("w.rs").await;
    w.edit(TextEdit::insert(0, "// mine\n")).await;
    w.sync_to(1).await;
    // A writer that truncates the file, then writes it.
    let truncate = || drop(fs::File::create(&path).unwrap());
    let write = |text: &str| {
        let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    };

    // An announced agent write is waited for, however long it pauses.
    d.buffers
        .expect_agent_write("s-1", &path, Some("fn a() {}".into()), "fn b() {}".into());
    truncate();
    tokio::time::sleep(SETTLE * 2).await;
    write("fn b() {}\n");
    let c = w.next().await;
    assert_eq!(c.origin.kind, OriginKind::Agent, "{c:?}");
    assert_eq!(c.origin.id.as_deref(), Some("s-1"));
    assert_eq!(w.editor, "// mine\nfn b() {}\n");

    // Any other writer gets the settle time.
    truncate();
    tokio::time::sleep(Duration::from_millis(60)).await;
    write("fn c() {}\n");
    let c = w.next().await;
    assert_eq!(c.origin.kind, OriginKind::Disk);
    assert_eq!(
        w.editor, "// mine\nfn c() {}\n",
        "one change, no empty moment"
    );

    // A file that stays empty was emptied: the deletion is merged.
    truncate();
    let c = w.next().await;
    assert_eq!(c.origin.kind, OriginKind::Disk);
    assert!(c.dirty);
    assert_eq!(w.editor, "// mine\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_disk_writes_are_attributed_and_revertible() {
    let d = Daemon::new(&[("lib.rs", "fn a() {\n    old();\n}\n")]).await;
    let mut w = d.window("lib.rs").await;
    // The user has an unsaved edit at the top.
    w.edit(TextEdit::insert(0, "// mine\n")).await;
    w.sync_to(1).await;
    // Claude Code announces an Edit (tool_call diff), then writes the file.
    d.buffers.expect_agent_write(
        "session-1",
        &d.path("lib.rs"),
        Some("    old();".into()),
        "    new_call();".into(),
    );
    fs::write(d.path("lib.rs"), "fn a() {\n    new_call();\n}\n").unwrap();
    let c = w.wait_for(|c| c.origin.kind == OriginKind::Agent).await;
    assert_eq!(c.origin.id.as_deref(), Some("session-1"));
    assert_eq!(w.editor, "// mine\nfn a() {\n    new_call();\n}\n");
    // The user rejects the agent's change; their own edit stays.
    let r = w
        .client
        .request::<bufm::RevertChange>(RevertParams {
            buffer: w.buffer(),
            rev: c.rev,
        })
        .await
        .unwrap();
    w.sync_to(r.rev).await;
    assert_eq!(w.editor, "// mine\nfn a() {\n    old();\n}\n");
    // An unannounced write is a plain disk change.
    fs::write(d.path("lib.rs"), "fn a() {\n    other();\n}\n").unwrap();
    let c = w.wait_for(|c| c.rev > r.rev).await;
    assert_eq!(c.origin.kind, OriginKind::Disk);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_replace_of_an_open_buffer_is_a_tracked_change() {
    let d = Daemon::new(&[("x.md", "# Title\n\nbody\n")]).await;
    let mut w = d.window("x.md").await;
    assert_eq!(
        d.buffers.open_text(&d.path("x.md")).unwrap(),
        "# Title\n\nbody\n"
    );
    let rev = d
        .buffers
        .agent_replace("s9", &d.path("x.md"), "# Title\n\nnew body\n")
        .unwrap();
    let c = w.wait_for(|c| c.rev == rev).await;
    assert_eq!(c.origin.id.as_deref(), Some("s9"));
    assert_eq!(c.edits, vec![TextEdit::insert(9, "new ")]);
    assert!(
        d.buffers
            .agent_replace("s9", &d.path("missing.md"), "")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saving_over_an_unseen_disk_change_merges_it_first() {
    let d = Daemon::new(&[("c.txt", "a\nb\n")]).await;
    // A closed workspace has no watcher, so nothing picks the change up
    // before the save does.
    let client = d.connect().await;
    client
        .request::<wsm::Close>(wsm::WorkspaceParams {
            id: d.workspace.clone(),
        })
        .await
        .unwrap();
    let mut w = d.window("c.txt").await;
    w.edit(TextEdit::insert(0, "0\n")).await;
    w.sync_to(1).await;
    fs::write(d.path("c.txt"), "a\nb\nc\n").unwrap();
    let save = SaveParams {
        buffer: w.buffer(),
        force: false,
    };
    let e = rpc_error(w.client.request::<bufm::Save>(save.clone()).await);
    assert_eq!(e.code, codes::SAVE_CONFLICT);
    let c = w.wait_for(|c| c.origin.kind == OriginKind::Disk).await;
    assert!(c.dirty);
    assert_eq!(w.editor, "0\na\nb\nc\n");
    w.client.request::<bufm::Save>(save).await.unwrap();
    assert_eq!(fs::read_to_string(d.path("c.txt")).unwrap(), "0\na\nb\nc\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsaved_edits_survive_a_daemon_restart() {
    let d = Daemon::new(&[("keep.rs", "fn keep() {}\n")]).await;
    let mut w = d.window("keep.rs").await;
    for (i, c) in "// unsaved".chars().enumerate() {
        w.edit(TextEdit::insert(i, c.to_string())).await;
    }
    w.sync_to(0).await; // until everything typed is acknowledged
    let rev = w.replica.rev();
    assert_eq!(d.buffers.rev(w.buffer()), Some(rev));
    // Typed offline, never delivered.
    w.editor.insert(0, 'X');
    let lost = w
        .replica
        .local_edit(&[TextEdit::insert(0, "X")], UndoGrouping::NewGroup)
        .unwrap();
    assert!(lost.is_some());
    let old_replica = w.replica;
    let mut editor = w.editor;
    drop(w.sub);

    let d = d.restart().await;
    let (client, _notes) = d.connect_with_notes().await;
    let mut params = d.open_params("keep.rs");
    params.replica = Some(old_replica.replica().clone());
    let r: OpenResult = client.request::<bufm::Open>(params).await.unwrap();
    assert_eq!(r.text, "// unsavedfn keep() {}\n");
    assert!(r.dirty);
    assert_eq!(r.rev, rev, "revisions carry on");
    assert!(r.last_seq >= 1, "the journal remembers the replica's seqs");
    // The old replica rebases its pending edit onto the new daemon's buffer.
    let mut replica = old_replica;
    let applied = replica.resync(&r).unwrap();
    apply(&mut editor, &applied.edits);
    let sub = client
        .subscribe::<bufm::Changes>(ChangesParams { buffer: r.buffer }, Some(r.rev))
        .await
        .unwrap();
    client
        .notify::<bufm::Edit>(applied.outgoing.unwrap())
        .await
        .unwrap();
    let mut w = Window {
        client,
        notes: _notes,
        sub,
        replica,
        editor,
        open: d.open_params("keep.rs"),
    };
    w.sync_to(rev + 1).await;
    assert_eq!(w.editor, "X// unsavedfn keep() {}\n");
    assert_eq!(d.buffers.text(w.buffer()).unwrap(), w.editor);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_are_clear() {
    let d = Daemon::new(&[("latin1.txt", ""), ("ok.txt", "ok")]).await;
    fs::write(d.path("latin1.txt"), b"caf\xe9").unwrap();
    let client = d.connect().await;
    let e = rpc_error(
        client
            .request::<bufm::Open>(d.open_params("latin1.txt"))
            .await,
    );
    assert_eq!(e.code, codes::UNSUPPORTED_ENCODING);
    assert!(e.message.contains("UTF-8"), "{}", e.message);
    let e = rpc_error(
        client
            .request::<bufm::Open>(d.open_params("nope.txt"))
            .await,
    );
    assert_eq!(e.code, codes::NOT_FOUND);
    let e = rpc_error(
        client
            .request::<bufm::Open>(d.open_params("../escape"))
            .await,
    );
    assert_eq!(e.code, codes::PATH_OUTSIDE_WORKSPACE);
    let e = rpc_error(
        client
            .request::<bufm::Save>(SaveParams {
                buffer: BufferId(999),
                force: false,
            })
            .await,
    );
    assert_eq!(e.code, codes::UNKNOWN_BUFFER);
    let e = rpc_error(
        client
            .subscribe::<bufm::Changes>(
                ChangesParams {
                    buffer: BufferId(999),
                },
                None,
            )
            .await,
    );
    assert_eq!(e.code, codes::UNKNOWN_BUFFER);

    // A bad edit is answered with buffer/editRejected.
    let mut w = d.window("ok.txt").await;
    let bad = EditParams {
        buffer: w.buffer(),
        replica: w.replica.replica().clone(),
        base_rev: 0,
        client_seq: 1,
        edits: vec![TextEdit::insert(99, "x")],
    };
    w.client.notify::<bufm::Edit>(bad).await.unwrap();
    let note = tokio::time::timeout(TIMEOUT, w.notes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.method, "buffer/editRejected");
    let p: EditRejectedParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(p.error.code, codes::INVALID_EDIT);
    assert_eq!(p.client_seq, 1);
    let future = EditParams {
        buffer: w.buffer(),
        replica: w.replica.replica().clone(),
        base_rev: 50,
        client_seq: 1,
        edits: vec![],
    };
    w.client.notify::<bufm::Edit>(future).await.unwrap();
    let note = tokio::time::timeout(TIMEOUT, w.notes.recv())
        .await
        .unwrap()
        .unwrap();
    let p: EditRejectedParams = serde_json::from_value(note.params.unwrap()).unwrap();
    assert_eq!(p.error.code, codes::REVISION_GONE);
    assert_eq!(d.buffers.rev(w.buffer()), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_replays_and_resends() {
    let d = Daemon::new(&[("r.txt", "hello\n")]).await;
    let mut a = d.window("r.txt").await;
    let mut b = d.window("r.txt").await;
    a.edit(TextEdit::insert(5, " a")).await;
    a.sync_to(1).await;
    // b's connection drops with an edit in flight that never arrives.
    apply(&mut b.editor, &[TextEdit::insert(0, "b:")]);
    let lost = b
        .replica
        .local_edit(&[TextEdit::insert(0, "b:")], UndoGrouping::NewGroup)
        .unwrap();
    assert!(lost.is_some());
    let since = b.replica.rev();
    let Window {
        replica,
        editor,
        open,
        ..
    } = b;
    // Meanwhile a keeps typing.
    a.edit(TextEdit::insert(0, ">")).await;
    a.sync_to(2).await;
    // Reconnect: resubscribe from the last rev, replay, resend.
    let (client, notes) = d.connect_with_notes().await;
    let sub = client
        .subscribe::<bufm::Changes>(
            ChangesParams {
                buffer: replica.buffer(),
            },
            Some(since),
        )
        .await
        .unwrap();
    let mut b = Window {
        client,
        notes,
        sub,
        replica,
        editor,
        open,
    };
    while b.replica.rev() < 2 {
        b.next().await;
    }
    b.send(b.replica.resend()).await;
    b.sync_to(3).await;
    a.sync_to(3).await;
    // Concurrent inserts at one spot: the later-committed one goes first.
    assert_eq!(a.editor, "b:>hello a\n");
    assert_eq!(b.editor, a.editor);

    // A resend of something that did arrive is not applied twice.
    b.edit(TextEdit::insert(0, "!")).await;
    b.sync_to(4).await;
    let dup = EditParams {
        buffer: b.buffer(),
        replica: b.replica.replica().clone(),
        base_rev: 3,
        client_seq: b.replica.resend().map_or(2, |e| e.client_seq),
        edits: vec![TextEdit::insert(0, "!")],
    };
    let last = d.buffers.rev(b.buffer()).unwrap();
    b.client.notify::<bufm::Edit>(dup).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(d.buffers.rev(b.buffer()), Some(last));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unused_buffers_unload_and_reopen() {
    let d = Daemon::new(&[("u.txt", "u")]).await;
    let mut w = d.window("u.txt").await;
    w.edit(TextEdit::insert(1, "nsaved")).await;
    w.sync_to(1).await;
    let id = w.buffer();
    w.client
        .request::<bufm::Close>(CloseParams {
            buffer: id,
            replica: w.replica.replica().clone(),
        })
        .await
        .unwrap();
    // Still subscribed: stays loaded.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(d.buffers.rev(id).is_some());
    drop(w.sub);
    eventually("the buffer unloads", || d.buffers.rev(id).is_none()).await;
    // Reopening restores the unsaved text from the journal.
    let client = d.connect().await;
    let r = client
        .request::<bufm::Open>(d.open_params("u.txt"))
        .await
        .unwrap();
    assert_ne!(r.buffer, id);
    assert_eq!(r.text, "unsaved");
    assert!(r.dirty);
}

// ---- convergence over the protocol (issue #40 acceptance) -------------------

const CLIENTS: usize = 3;
const PIECES: [&str; 6] = ["a", "é", "🙂", "\n", "xyz", "Q"];
const INITIAL: &str = "fn main() {\n    println!(\"héllo\");\n}\n";

#[derive(Clone, Debug)]
enum Act {
    Edit {
        who: usize,
        pos: u32,
        del: u8,
        ins: u8,
    },
    Agent {
        pos: u32,
        del: u8,
        ins: u8,
    },
    Up {
        who: usize,
    },
    Down {
        who: usize,
    },
    Undo {
        who: usize,
    },
}

fn act(deletes: bool) -> impl Strategy<Value = Act> {
    let del = if deletes { 0u8..6 } else { 0u8..1 };
    prop_oneof![
        5 => (0..CLIENTS, any::<u32>(), del.clone(), 0u8..6)
            .prop_map(|(who, pos, del, ins)| Act::Edit { who, pos, del, ins }),
        1 => (any::<u32>(), del, 0u8..6).prop_map(|(pos, del, ins)| Act::Agent { pos, del, ins }),
        3 => (0..CLIENTS).prop_map(|who| Act::Up { who }),
        3 => (0..CLIENTS).prop_map(|who| Act::Down { who }),
        if deletes { 1 } else { 0 } => (0..CLIENTS).prop_map(|who| Act::Undo { who }),
    ]
}

fn make_edit(s: &str, pos: u32, del: u8, ins: u8) -> TextEdit {
    let mut start = pos as usize % (s.len() + 1);
    while !s.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = start;
    for _ in 0..del {
        match s[end..].chars().next() {
            Some(c) => end += c.len_utf8(),
            None => break,
        }
    }
    let text = if ins == 0 {
        String::new()
    } else {
        PIECES[ins as usize % PIECES.len()].repeat(ins as usize % 3 + 1)
    };
    TextEdit::new(start..end, text)
}

fn char_counts(s: &str) -> BTreeMap<char, usize> {
    let mut m = BTreeMap::new();
    for c in s.chars() {
        *m.entry(c).or_insert(0) += 1;
    }
    m
}

/// A replica whose messages are held back and delivered when the test says.
struct Peer {
    w: Window,
    up: VecDeque<EditParams>,
    down: VecDeque<BufferChange>,
}

impl Peer {
    /// Move whatever the daemon has sent into the local queue.
    fn pull(&mut self) {
        while let Some(Some(item)) = self.w.sub.next().now_or_never() {
            self.down.push_back(item.unwrap().event);
        }
    }
}

async fn run_over_protocol(acts: Vec<Act>) -> (String, String) {
    let d = Daemon::new(&[("conv.rs", INITIAL)]).await;
    let mut peers = Vec::new();
    for _ in 0..CLIENTS {
        let w = d.window("conv.rs").await;
        peers.push(Peer {
            w,
            up: VecDeque::new(),
            down: VecDeque::new(),
        });
    }
    let buffer = peers[0].w.buffer();
    let mut inserted = String::new();
    for a in acts {
        match a {
            Act::Edit { who, pos, del, ins } => {
                let p = &mut peers[who];
                let e = make_edit(&p.w.editor, pos, del, ins);
                inserted.push_str(&e.text);
                apply(&mut p.w.editor, std::slice::from_ref(&e));
                if let Some(out) =
                    p.w.replica
                        .local_edit(&[e], UndoGrouping::NewGroup)
                        .unwrap()
                {
                    p.up.push_back(out);
                }
            }
            Act::Agent { pos, del, ins } => {
                // Client edits commit concurrently, so the text can change
                // between reading it and editing: retry on a stale offset.
                loop {
                    let text = d.buffers.text(buffer).unwrap();
                    let e = make_edit(&text, pos, del, ins);
                    match d
                        .buffers
                        .agent_edit(buffer, "agent", std::slice::from_ref(&e))
                    {
                        Ok(_) => {
                            inserted.push_str(&e.text);
                            break;
                        }
                        Err(err) => assert_eq!(err.code, codes::INVALID_EDIT, "{err}"),
                    }
                }
            }
            Act::Up { who } => {
                if let Some(out) = peers[who].up.pop_front() {
                    peers[who].w.send(Some(out)).await;
                }
            }
            Act::Down { who } => {
                let p = &mut peers[who];
                p.pull();
                let head = d.buffers.rev(buffer).unwrap();
                if p.down.is_empty() && p.w.replica.rev() < head {
                    // The daemon committed something this replica has not
                    // seen yet: wait for it rather than skip.
                    let c = tokio::time::timeout(TIMEOUT, p.w.sub.next())
                        .await
                        .expect("timed out waiting for buffer/changes")
                        .expect("subscription ended")
                        .unwrap();
                    p.down.push_back(c.event);
                }
                if let Some(c) = p.down.pop_front() {
                    let inc = p.w.replica.receive(&c).unwrap();
                    apply(&mut p.w.editor, &inc.edits);
                    if let Some(out) = inc.outgoing {
                        p.up.push_back(out);
                    }
                }
            }
            Act::Undo { who } => {
                let p = &mut peers[who];
                if let Some(applied) = p.w.replica.undo().unwrap() {
                    apply(&mut p.w.editor, &applied.edits);
                    if let Some(out) = applied.outgoing {
                        p.up.push_back(out);
                    }
                }
            }
        }
    }
    // Quiescence: deliver everything held back, then let the rest flow.
    for p in &mut peers {
        p.pull();
        while let Some(out) = p.up.pop_front() {
            p.w.send(Some(out)).await;
        }
        while let Some(c) = p.down.pop_front() {
            let out = p.w.take(&c);
            p.w.send(out).await;
        }
    }
    loop {
        let head = d.buffers.rev(buffer).unwrap();
        for p in &mut peers {
            p.w.sync_to(head).await;
        }
        if d.buffers.rev(buffer).unwrap() == head {
            break;
        }
    }
    let text = d.buffers.text(buffer).unwrap();
    for p in &peers {
        assert!(p.w.replica.is_synced());
        assert_eq!(p.w.replica.text().to_string(), text, "replica diverged");
        assert_eq!(p.w.editor, text, "editor mirror diverged");
    }
    (text, inserted)
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    #[test]
    fn replicas_converge_over_the_protocol(acts in prop::collection::vec(act(true), 1..80)) {
        block_on(run_over_protocol(acts));
    }

    #[test]
    fn no_insert_is_lost_over_the_protocol(acts in prop::collection::vec(act(false), 1..80)) {
        let (text, inserted) = block_on(run_over_protocol(acts));
        let mut expected = char_counts(INITIAL);
        for (c, n) in char_counts(&inserted) {
            *expected.entry(c).or_insert(0) += n;
        }
        prop_assert_eq!(char_counts(&text), expected, "characters lost or duplicated");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_file_traffic_reaches_open_buffers_through_the_agent_fs_bridge() {
    use coqo_acp_host::AgentFs;
    use coqo_protocol::methods::agents::SessionId;

    let d = Daemon::new(&[("a.rs", "fn a() {}\n")]).await;
    let bridge = crate::agents::BufferFs::new(Arc::clone(&d.buffers));
    let session = SessionId("s-1".into());
    let mut w = d.window("a.rs").await;
    w.edit(TextEdit::insert(0, "// mine\n")).await;
    w.sync_to(1).await;
    // fs/read_text_file sees the unsaved edit.
    assert_eq!(
        bridge.open_text(&d.path("a.rs")).as_deref(),
        Some("// mine\nfn a() {}\n")
    );
    // A tool-call diff, then the agent's own disk write: the agent's change.
    bridge.expect_write(
        &session,
        &d.path("a.rs"),
        Some("fn a() {}".into()),
        "fn b() {}".into(),
    );
    fs::write(d.path("a.rs"), "fn b() {}\n").unwrap();
    let c = w.wait_for(|c| c.origin.kind == OriginKind::Agent).await;
    assert_eq!(c.origin.id.as_deref(), Some("s-1"));
    assert_eq!(w.editor, "// mine\nfn b() {}\n");
    // fs/write_text_file into the open buffer.
    assert!(bridge.replace_open(&session, &d.path("a.rs"), "fn c() {}\n"));
    w.wait_for(|c| c.origin.kind == OriginKind::Agent && c.rev > 2)
        .await;
    assert_eq!(w.editor, "fn c() {}\n");
    assert!(!bridge.replace_open(&session, &d.path("closed.rs"), "x"));
}
