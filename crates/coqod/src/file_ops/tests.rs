// SPDX-License-Identifier: Apache-2.0

//! `fs/create`, `fs/rename` and `fs/delete` through a real protocol client,
//! with real files, the real watcher and the buffer service.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use coqo_protocol::methods::buffer::{self as bufm, EditParams, OpenResult, TextEdit};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::fs::{
    self as fsm, CreateParams, DeleteParams, EntryKind, PathParams, RenameParams,
};
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceId};
use coqo_protocol::{Client, ClientConfig, Method, ProtocolError, Router, Server, ServerConfig};
use coqo_storage::DataDirs;
use tempfile::TempDir;

use super::register;
use crate::buffers::{self, BufferConfig, BufferService};
use crate::workspace::{self, Trash, WatchConfig, WorkspaceService};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    _data: TempDir,
    folder: TempDir,
    trash: TempDir,
    buffers: Arc<BufferService>,
    client: Client,
    workspace: WorkspaceId,
    root: RootId,
}

impl Harness {
    async fn new(files: &[(&str, &str)]) -> Self {
        let data = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let trash = tempfile::tempdir().unwrap();
        for (name, text) in files {
            write(folder.path(), name, text);
        }
        let dirs = DataDirs::at(data.path());
        let watch = WatchConfig {
            debounce: Duration::from_millis(30),
            ..WatchConfig::default()
        };
        let workspaces = Arc::new(
            WorkspaceService::open_with(dirs.clone(), watch)
                .unwrap()
                .with_trash(Trash::Folder(trash.path().join("bin"))),
        );
        let buffers = BufferService::new(Arc::clone(&workspaces), dirs, BufferConfig::default());
        let router = workspace::register(Router::new(), Arc::clone(&workspaces));
        let router = buffers::register(router, Arc::clone(&buffers));
        let router = register(router, workspaces, Arc::clone(&buffers));
        let server = Server::new(router, ServerConfig::new("coqod", "test"));
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move { server.serve(server_io).await });
        let (client, _) = Client::connect(client_io, ClientConfig::new("test", "test"))
            .await
            .unwrap();
        let info = client
            .request::<wsm::Open>(wsm::OpenParams {
                path: Some(folder.path().to_str().unwrap().to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        Self {
            _data: data,
            folder,
            trash,
            buffers,
            client,
            workspace: info.id,
            root: info.roots[0].id,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.folder.path().join(rel)
    }

    fn binned(&self, name: &str) -> PathBuf {
        self.trash.path().join("bin").join(name)
    }

    async fn call<M: Method>(&self, params: M::Params) -> Result<M::Result, i64> {
        self.client.request::<M>(params).await.map_err(|e| match e {
            ProtocolError::Rpc(e) => e.code,
            other => panic!("{} failed: {other}", M::NAME),
        })
    }

    async fn create(&self, path: &str, kind: EntryKind) -> Result<String, i64> {
        self.call::<fsm::Create>(CreateParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: path.to_owned(),
            kind,
        })
        .await
        .map(|r| r.path)
    }

    async fn rename(&self, from: &str, to: &str) -> Result<String, i64> {
        self.call::<fsm::Rename>(RenameParams {
            workspace: self.workspace.clone(),
            root: self.root,
            from: from.to_owned(),
            to: to.to_owned(),
        })
        .await
        .map(|r| r.path)
    }

    async fn delete(&self, path: &str, permanent: bool) -> Result<bool, i64> {
        self.call::<fsm::Delete>(DeleteParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: path.to_owned(),
            permanent,
        })
        .await
        .map(|r| r.trashed)
    }

    async fn open(&self, path: &str) -> Result<OpenResult, i64> {
        self.call::<bufm::Open>(bufm::OpenParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: path.to_owned(),
            replica: None,
        })
        .await
    }

    async fn stat(&self, path: &str) -> Result<fsm::FileStat, i64> {
        self.call::<fsm::Stat>(PathParams {
            workspace: self.workspace.clone(),
            root: self.root,
            path: path.to_owned(),
        })
        .await
    }

    /// Type `text` at the start of the buffer and wait for the daemon to
    /// have it.
    async fn type_in(&self, buffer: &OpenResult, text: &str) {
        let want = format!("{text}{}", buffer.text);
        self.client
            .notify::<bufm::Edit>(EditParams {
                buffer: buffer.buffer,
                replica: buffer.replica.clone(),
                base_rev: buffer.rev,
                client_seq: 1,
                edits: vec![TextEdit::insert(0, text)],
            })
            .await
            .unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while self.buffers.text(buffer.buffer).as_deref() != Some(want.as_str()) {
            assert!(Instant::now() < deadline, "the edit never landed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn write(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn creates_files_and_folders() {
    let h = Harness::new(&[("README.md", "# hi\n")]).await;

    assert_eq!(
        h.create("notes.txt", EntryKind::File).await,
        Ok("notes.txt".into())
    );
    assert_eq!(fs::read_to_string(h.path("notes.txt")).unwrap(), "");
    assert_eq!(h.create("src", EntryKind::Dir).await, Ok("src".into()));
    assert!(h.path("src").is_dir());
    // Missing folders on the way are made; the path comes back normalised.
    assert_eq!(
        h.create("./a//b/c.rs", EntryKind::File).await,
        Ok("a/b/c.rs".into())
    );
    assert!(h.path("a/b/c.rs").is_file());

    // Name clashes, with a file or a folder, never overwrite.
    assert_eq!(
        h.create("README.md", EntryKind::File).await,
        Err(codes::ALREADY_EXISTS)
    );
    assert_eq!(
        h.create("src", EntryKind::File).await,
        Err(codes::ALREADY_EXISTS)
    );
    assert_eq!(
        h.create("README.md", EntryKind::Dir).await,
        Err(codes::ALREADY_EXISTS)
    );
    assert_eq!(fs::read_to_string(h.path("README.md")).unwrap(), "# hi\n");
    // A file is not a folder to create in.
    assert_eq!(
        h.create("README.md/x", EntryKind::File).await,
        Err(codes::NOT_A_DIRECTORY)
    );
    assert_eq!(
        h.create("", EntryKind::Dir).await,
        Err(coqo_protocol::codes::INVALID_PARAMS)
    );
    assert_eq!(
        h.create("odd", EntryKind::Other).await,
        Err(coqo_protocol::codes::INVALID_PARAMS)
    );
    assert!(!h.path("odd").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn renames_and_moves() {
    let h = Harness::new(&[
        ("a.txt", "a\n"),
        ("b.txt", "b\n"),
        ("src/lib.rs", "pub fn f() {}\n"),
        ("docs/guide.md", "# guide\n"),
    ])
    .await;

    assert_eq!(h.rename("a.txt", "c.txt").await, Ok("c.txt".into()));
    assert!(!h.path("a.txt").exists());
    assert_eq!(fs::read_to_string(h.path("c.txt")).unwrap(), "a\n");
    // A move into a folder.
    assert_eq!(h.rename("c.txt", "src/c.txt").await, Ok("src/c.txt".into()));
    assert!(h.path("src/c.txt").is_file());
    // A folder, with what is in it.
    assert_eq!(h.rename("src", "docs/src").await, Ok("docs/src".into()));
    assert!(h.path("docs/src/lib.rs").is_file());

    // Clashes, missing entries and impossible moves change nothing.
    assert_eq!(
        h.rename("b.txt", "docs/guide.md").await,
        Err(codes::ALREADY_EXISTS)
    );
    assert_eq!(h.rename("b.txt", "docs").await, Err(codes::ALREADY_EXISTS));
    assert_eq!(
        fs::read_to_string(h.path("docs/guide.md")).unwrap(),
        "# guide\n"
    );
    assert_eq!(h.rename("nope.txt", "x.txt").await, Err(codes::NOT_FOUND));
    assert_eq!(
        h.rename("b.txt", "missing/b.txt").await,
        Err(codes::NOT_FOUND)
    );
    assert_eq!(
        h.rename("docs", "docs/src/docs").await,
        Err(coqo_protocol::codes::INVALID_PARAMS)
    );
    assert_eq!(
        h.rename("", "root").await,
        Err(coqo_protocol::codes::INVALID_PARAMS)
    );
    assert!(h.path("b.txt").is_file());
    // Renaming to itself is a no-op.
    assert_eq!(h.rename("b.txt", "./b.txt").await, Ok("b.txt".into()));
    assert!(h.path("b.txt").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn deletes_to_the_trash_or_for_good() {
    let h = Harness::new(&[
        ("a.txt", "a\n"),
        ("dir/one.txt", "1\n"),
        ("dir/sub/two.txt", "2\n"),
        ("keep/a.txt", "other a\n"),
    ])
    .await;

    assert_eq!(h.delete("a.txt", false).await, Ok(true));
    assert!(!h.path("a.txt").exists());
    assert_eq!(fs::read_to_string(h.binned("a.txt")).unwrap(), "a\n");
    // A second entry of the same name gets a name of its own in the trash.
    assert_eq!(h.delete("keep/a.txt", false).await, Ok(true));
    assert_eq!(
        fs::read_to_string(h.binned("a.txt.1")).unwrap(),
        "other a\n"
    );
    assert_eq!(
        fs::read_to_string(h.binned("a.txt")).unwrap(),
        "a\n",
        "the first one is kept"
    );

    assert_eq!(h.delete("dir", true).await, Ok(false));
    assert!(!h.path("dir").exists());
    assert!(!h.binned("dir").exists());

    assert_eq!(h.delete("gone.txt", false).await, Err(codes::NOT_FOUND));
    assert_eq!(
        h.delete("", true).await,
        Err(coqo_protocol::codes::INVALID_PARAMS)
    );
    assert!(h.folder.path().is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn paths_and_symlinks_cannot_escape_the_workspace() {
    let h = Harness::new(&[("a.txt", "a\n"), ("src/main.rs", "fn main() {}\n")]).await;
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "secret.txt", "secret\n");
    std::os::unix::fs::symlink(outside.path(), h.path("out")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), h.path("secret-link")).unwrap();

    for bad in ["../x", "/etc/x", "src/../../x"] {
        assert_eq!(
            h.create(bad, EntryKind::File).await,
            Err(codes::PATH_OUTSIDE_WORKSPACE),
            "{bad}"
        );
        assert_eq!(
            h.rename("a.txt", bad).await,
            Err(codes::PATH_OUTSIDE_WORKSPACE),
            "{bad}"
        );
        assert_eq!(
            h.rename(bad, "b.txt").await,
            Err(codes::PATH_OUTSIDE_WORKSPACE),
            "{bad}"
        );
        assert_eq!(
            h.delete(bad, true).await,
            Err(codes::PATH_OUTSIDE_WORKSPACE),
            "{bad}"
        );
    }

    // Through a symlink to a folder outside: nothing is made, moved or
    // deleted there.
    assert_eq!(
        h.create("out/new.txt", EntryKind::File).await,
        Err(codes::PATH_OUTSIDE_WORKSPACE)
    );
    assert_eq!(
        h.create("out/deep/new.txt", EntryKind::File).await,
        Err(codes::PATH_OUTSIDE_WORKSPACE)
    );
    assert_eq!(
        h.rename("a.txt", "out/a.txt").await,
        Err(codes::PATH_OUTSIDE_WORKSPACE)
    );
    assert_eq!(
        h.rename("out/secret.txt", "stolen.txt").await,
        Err(codes::PATH_OUTSIDE_WORKSPACE)
    );
    assert_eq!(
        h.delete("out/secret.txt", true).await,
        Err(codes::PATH_OUTSIDE_WORKSPACE)
    );
    assert_eq!(
        fs::read_dir(outside.path()).unwrap().count(),
        1,
        "only secret.txt is outside"
    );
    assert!(h.path("a.txt").is_file());

    // A symlink itself is the entry: renaming or deleting it acts on the
    // link, never on its target.
    assert_eq!(
        h.rename("secret-link", "src/link").await,
        Ok("src/link".into())
    );
    assert!(
        fs::symlink_metadata(h.path("src/link"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(h.delete("src/link", true).await, Ok(false));
    assert_eq!(h.delete("out", false).await, Ok(true));
    assert!(fs::symlink_metadata(h.path("out")).is_err());
    assert_eq!(
        fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "secret\n"
    );
}

/// An open buffer follows its file through a rename and a folder move: the
/// same buffer, with its unsaved edits, under the new path; saving writes
/// the new file.
#[tokio::test(flavor = "multi_thread")]
async fn open_buffers_follow_a_rename() {
    let h = Harness::new(&[("src/main.rs", "fn main() {}\n"), ("README.md", "# hi\n")]).await;
    let main = h.open("src/main.rs").await.unwrap();
    h.type_in(&main, "// edited\n").await;
    let readme = h.open("README.md").await.unwrap();

    assert_eq!(h.rename("src", "app").await, Ok("app".into()));
    assert_eq!(
        h.rename("app/main.rs", "app/start.rs").await,
        Ok("app/start.rs".into())
    );
    assert_eq!(h.rename("README.md", "docs.md").await, Ok("docs.md".into()));

    let moved = h.open("app/start.rs").await.unwrap();
    assert_eq!(moved.buffer, main.buffer, "the same buffer");
    assert!(moved.dirty);
    assert_eq!(moved.text, "// edited\nfn main() {}\n");
    assert_eq!(h.open("docs.md").await.unwrap().buffer, readme.buffer);
    assert_eq!(h.open("src/main.rs").await, Err(codes::NOT_FOUND));

    h.call::<bufm::Save>(bufm::SaveParams {
        buffer: main.buffer,
        force: false,
    })
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(h.path("app/start.rs")).unwrap(),
        "// edited\nfn main() {}\n"
    );
    assert!(!h.path("src").exists());

    // Changes on disk at the new path still reach the buffer.
    fs::write(h.path("docs.md"), "# changed\n").unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while h.buffers.text(readme.buffer).as_deref() != Some("# changed\n") {
        assert!(Instant::now() < deadline, "the disk change never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Deleting a file with unsaved edits keeps them in its buffer; saving
/// writes the file again.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_files_buffer_keeps_its_edits() {
    let h = Harness::new(&[("notes.md", "one\n")]).await;
    let notes = h.open("notes.md").await.unwrap();
    h.type_in(&notes, "zero\n").await;

    assert_eq!(h.delete("notes.md", false).await, Ok(true));
    assert_eq!(h.stat("notes.md").await.unwrap_err(), codes::NOT_FOUND);
    // Give the watcher time to report the deletion.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.buffers.text(notes.buffer).as_deref(), Some("zero\none\n"));

    h.call::<bufm::Save>(bufm::SaveParams {
        buffer: notes.buffer,
        force: false,
    })
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(h.path("notes.md")).unwrap(),
        "zero\none\n"
    );
}
