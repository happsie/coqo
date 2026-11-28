// SPDX-License-Identifier: Apache-2.0

//! `search/run` and `search/replace` through a real protocol client, with
//! real files, the buffer service and the built-in search mod.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use coqo_kernel::mods::BuiltinMods;
use coqo_protocol::methods::buffer::{self as bufm, EditParams, OpenResult, OriginKind, TextEdit};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::search::{
    self as sm, MatchAt, ReplaceFile, ReplaceOutcome, ReplaceParams, ReplaceResult, RunParams,
    SearchEvent, SearchQuery, SearchSummary,
};
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceId};
use coqo_protocol::{Client, ClientConfig, ProtocolError, Router, Server, ServerConfig};
use coqo_storage::DataDirs;
use futures_util::StreamExt as _;
use tempfile::TempDir;

use super::{SearchService, register};
use crate::buffers::{self, BufferConfig, BufferService};
use crate::mods::Mods;
use crate::workspace::{self, WatchConfig, WorkspaceService};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    _data: TempDir,
    folder: TempDir,
    buffers: Arc<BufferService>,
    search: Arc<SearchService>,
    client: Client,
    workspace: WorkspaceId,
    root: RootId,
}

fn write(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

/// `(path, [(line, start, end)])` of every file, sorted, and the summary.
type Found = (Vec<(String, Vec<(u32, u32, u32)>)>, SearchSummary);

impl Harness {
    async fn new(files: &[(&str, &str)]) -> Self {
        Self::with_mods(files, true).await
    }

    async fn with_mods(files: &[(&str, &str)], with_search: bool) -> Self {
        let data = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        for (name, text) in files {
            write(folder.path(), name, text);
        }
        let dirs = DataDirs::at(data.path());
        let watch = WatchConfig {
            debounce: Duration::from_millis(30),
            ..WatchConfig::default()
        };
        let workspaces = Arc::new(WorkspaceService::open_with(dirs.clone(), watch).unwrap());
        let buffers = BufferService::new(Arc::clone(&workspaces), dirs, BufferConfig::default());
        let mut builtins = BuiltinMods::new();
        if with_search {
            builtins = builtins.with(coqo_mod_search::MANIFEST, coqo_mod_search::SearchMod::new());
        }
        let mods = Mods::load(builtins);
        mods.start().await;
        let search = SearchService::new(Arc::clone(&workspaces), Arc::clone(&buffers), mods);
        let router = workspace::register(Router::new(), workspaces);
        let router = buffers::register(router, Arc::clone(&buffers));
        let router = register(router, Arc::clone(&search));
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
            buffers,
            search,
            client,
            workspace: info.id,
            root: info.roots[0].id,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.folder.path().join(rel)
    }

    fn query(&self, pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: pattern.into(),
            case_sensitive: true,
            ..SearchQuery::default()
        }
    }

    fn params(&self, query: SearchQuery) -> RunParams {
        RunParams {
            workspace: self.workspace.clone(),
            query,
            roots: None,
            max_matches: None,
        }
    }

    /// Run a search to its end.
    async fn search(&self, query: SearchQuery) -> Result<Found, ProtocolError> {
        let mut sub = self
            .client
            .subscribe::<sm::Run>(self.params(query), None)
            .await?;
        let mut files = Vec::new();
        let mut summary = None;
        let mut last_seq = 0;
        let deadline = Instant::now() + TIMEOUT;
        while let Some(item) = tokio::time::timeout_at(deadline.into(), sub.next())
            .await
            .expect("the search ends")
        {
            let item = item?;
            assert!(item.seq > last_seq, "seqs increase");
            last_seq = item.seq;
            match item.event {
                SearchEvent::Files { files: batch } => {
                    assert!(summary.is_none(), "files after done");
                    for f in batch {
                        assert_eq!(f.root, self.root);
                        let at = f.matches.iter().map(|m| (m.line, m.start, m.end)).collect();
                        files.push((f.path, at));
                    }
                }
                SearchEvent::Done(s) => summary = Some(s),
            }
        }
        files.sort();
        Ok((files, summary.expect("a done event")))
    }

    async fn open(&self, rel: &str) -> OpenResult {
        self.client
            .request::<bufm::Open>(bufm::OpenParams {
                workspace: self.workspace.clone(),
                root: self.root,
                path: rel.to_owned(),
                replica: None,
            })
            .await
            .unwrap()
    }

    /// Type `text` at the start of the open buffer, unsaved.
    async fn type_at_start(&self, open: &OpenResult, text: &str) {
        self.client
            .notify::<bufm::Edit>(EditParams {
                buffer: open.buffer,
                replica: open.replica.clone(),
                base_rev: open.rev,
                client_seq: 1,
                edits: vec![TextEdit::new(0..0, text)],
            })
            .await
            .unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while self.buffers.rev(open.buffer) == Some(open.rev) {
            assert!(Instant::now() < deadline, "the edit lands");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn replace(
        &self,
        query: SearchQuery,
        replacement: &str,
        files: Vec<ReplaceFile>,
        hold: Vec<bufm::BufferId>,
    ) -> ReplaceResult {
        self.client
            .request::<sm::Replace>(ReplaceParams {
                workspace: self.workspace.clone(),
                query,
                replacement: replacement.into(),
                files,
                hold,
            })
            .await
            .unwrap()
    }

    fn file(&self, rel: &str) -> ReplaceFile {
        ReplaceFile {
            root: self.root,
            path: rel.into(),
            matches: None,
        }
    }
}

fn rpc_code(e: &ProtocolError) -> Option<i64> {
    e.rpc().map(|e| e.code)
}

#[tokio::test(flavor = "multi_thread")]
async fn search_respects_gitignore_and_streams_to_done() {
    let h = Harness::new(&[
        (".gitignore", "target/\n*.log\n"),
        ("src/main.rs", "fn main() {\n    let needle = 1;\n}\n"),
        ("src/lib.rs", "pub fn needle() {}\npub fn other() {}\n"),
        ("target/debug/build.rs", "needle\n"),
        ("run.log", "needle\n"),
        ("README.md", "No match here.\n"),
    ])
    .await;
    let (files, summary) = h.search(h.query("needle")).await.unwrap();
    assert_eq!(
        files,
        vec![
            ("src/lib.rs".to_owned(), vec![(0, 7, 13)]),
            ("src/main.rs".to_owned(), vec![(1, 8, 14)]),
        ]
    );
    assert_eq!(summary.files, 2);
    assert_eq!(summary.matches, 2);
    assert!(summary.searched >= 4, "{summary:?}");
    assert!(!summary.limit_hit);
}

#[tokio::test(flavor = "multi_thread")]
async fn regex_case_word_and_globs() {
    let h = Harness::new(&[
        ("a.rs", "Foo foo food fooBar\n"),
        ("b.ts", "foo\n"),
        ("gen/c.rs", "foo\n"),
    ])
    .await;
    let mut q = h.query(r"fo+\w*");
    q.regex = true;
    q.include = vec!["*.rs".into()];
    q.exclude = vec!["gen".into()];
    let (files, _) = h.search(q).await.unwrap();
    assert_eq!(
        files,
        vec![("a.rs".to_owned(), vec![(0, 4, 7), (0, 8, 12), (0, 13, 19)])]
    );

    let mut q = h.query("foo");
    q.whole_word = true;
    q.include = vec!["a.rs".into()];
    let (files, _) = h.search(q.clone()).await.unwrap();
    assert_eq!(files, vec![("a.rs".to_owned(), vec![(0, 4, 7)])]);
    q.case_sensitive = false;
    let (files, _) = h.search(q).await.unwrap();
    assert_eq!(files, vec![("a.rs".to_owned(), vec![(0, 0, 3), (0, 4, 7)])]);

    // A bad regular expression fails the subscribe.
    let mut q = h.query("(unclosed");
    q.regex = true;
    let err = h.search(q).await.unwrap_err();
    assert_eq!(rpc_code(&err), Some(codes::INVALID_QUERY), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unsaved_buffer_text_is_searched() {
    let h = Harness::new(&[("open.rs", "on disk\n"), ("closed.rs", "nothing\n")]).await;
    let open = h.open("open.rs").await;
    h.type_at_start(&open, "unsaved needle\n").await;
    let (files, _) = h.search(h.query("needle")).await.unwrap();
    assert_eq!(files, vec![("open.rs".to_owned(), vec![(0, 8, 14)])]);
    // The disk text is not what is searched while the buffer is open.
    let (files, _) = h.search(h.query("on disk")).await.unwrap();
    assert_eq!(files, vec![("open.rs".to_owned(), vec![(1, 0, 7)])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_across_open_and_unopened_files() {
    let h = Harness::new(&[
        ("open.rs", "let old = old_value;\n"),
        ("closed.rs", "\u{feff}old\r\nkeep\r\nold old\r\n"),
        ("held.rs", "old\n"),
        ("none.rs", "nothing\n"),
    ])
    .await;
    let open = h.open("open.rs").await;
    let held = h.open("held.rs").await;
    let mut q = h.query("old");
    q.whole_word = true;
    let result = h
        .replace(
            q,
            "new",
            vec![
                h.file("open.rs"),
                h.file("closed.rs"),
                h.file("held.rs"),
                h.file("none.rs"),
                h.file("missing.rs"),
            ],
            vec![held.buffer],
        )
        .await;
    assert_eq!(result.replaced, 1 + 3 + 1);
    let outcomes: Vec<_> = result
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.outcome, f.replaced))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            ("open.rs", ReplaceOutcome::Buffer, 1),
            ("closed.rs", ReplaceOutcome::Written, 3),
            ("held.rs", ReplaceOutcome::Held, 1),
            ("none.rs", ReplaceOutcome::Unchanged, 0),
            ("missing.rs", ReplaceOutcome::Failed, 0),
        ]
    );
    assert!(result.files[4].error.is_some());

    // The open buffer changed, unsaved and revertible; the disk did not.
    assert_eq!(
        h.buffers.text(open.buffer).unwrap(),
        "let new = old_value;\n"
    );
    assert_eq!(
        fs::read_to_string(h.path("open.rs")).unwrap(),
        "let old = old_value;\n"
    );
    assert_eq!(result.files[0].buffer, Some(open.buffer));
    let rev = result.files[0].rev.unwrap();
    let mut changes = h
        .client
        .subscribe::<bufm::Changes>(
            bufm::ChangesParams {
                buffer: open.buffer,
            },
            Some(rev - 1),
        )
        .await
        .unwrap();
    let change = changes.next().await.unwrap().unwrap().event;
    assert_eq!(change.origin.kind, OriginKind::Replace);
    assert!(change.dirty);
    h.client
        .request::<bufm::RevertChange>(bufm::RevertParams {
            buffer: open.buffer,
            rev,
        })
        .await
        .unwrap();
    assert_eq!(
        h.buffers.text(open.buffer).unwrap(),
        "let old = old_value;\n"
    );

    // The closed file was written, its BOM and CRLFs kept.
    assert_eq!(
        fs::read(h.path("closed.rs")).unwrap(),
        "\u{feff}new\r\nkeep\r\nnew new\r\n".as_bytes()
    );

    // The held buffer is the caller's to edit: edits at its revision.
    let held_file = &result.files[2];
    assert_eq!(held_file.buffer, Some(held.buffer));
    assert_eq!(held_file.rev, Some(held.rev));
    assert_eq!(held_file.edits, vec![TextEdit::new(0..3, "new")]);
    assert_eq!(h.buffers.text(held.buffer).unwrap(), "old\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_one_match_with_capture_groups() {
    let h = Harness::new(&[("a.txt", "f(1) f(2)\nf(3)\n")]).await;
    let mut q = h.query(r"f\((\d)\)");
    q.regex = true;
    let mut file = h.file("a.txt");
    file.matches = Some(vec![
        MatchAt {
            line: 0,
            start: 5,
            end: 9,
        },
        // Not a match (any more): skipped.
        MatchAt {
            line: 1,
            start: 1,
            end: 5,
        },
    ]);
    let result = h.replace(q, "g($1$1)", vec![file], Vec::new()).await;
    assert_eq!(result.replaced, 1);
    assert_eq!(
        fs::read_to_string(h.path("a.txt")).unwrap(),
        "f(1) g(22)\nf(3)\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_subscription_cancels_the_walk() {
    let files: Vec<(String, String)> = (0..200)
        .map(|i| (format!("d{}/f{i}.txt", i % 10), "needle\n".to_owned()))
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let h = Harness::new(&refs).await;
    // Each walk waits after every file it finds until it is cancelled, so it
    // can't finish before the unsubscribe arrives, however fast it is.
    h.search
        .hold_until_cancelled
        .store(true, std::sync::atomic::Ordering::Release);
    let ended = |n: usize| {
        let search = Arc::clone(&h.search);
        async move {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                if let Some(s) = search.walks.lock().unwrap().get(n).copied() {
                    break s;
                }
                assert!(Instant::now() < deadline, "the walk ends");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    };
    let mut sub = h
        .client
        .subscribe::<sm::Run>(h.params(h.query("needle")), None)
        .await
        .unwrap();
    // The first file comes at once, before the walk is done.
    let first = sub.next().await.unwrap().unwrap();
    assert!(matches!(first.event, SearchEvent::Files { .. }));
    assert!(h.search.walks.lock().unwrap().is_empty(), "still walking");
    assert!(sub.unsubscribe().await.unwrap());
    let summary = ended(0).await;
    assert!(summary.cancelled, "{summary:?}");
    // At most one file per walker thread got through before the cancel.
    assert!(summary.files < 200, "{summary:?}");

    // A feed dropped before anything is read stops the walk too.
    let feed = h.search.run(h.params(h.query("needle"))).await.unwrap();
    drop(feed);
    let summary = ended(1).await;
    assert!(summary.cancelled, "{summary:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_mod_search_is_unavailable() {
    let h = Harness::with_mods(&[("a.txt", "x\n")], false).await;
    let err = h.search(h.query("x")).await.unwrap_err();
    assert_eq!(rpc_code(&err), Some(codes::SEARCH_UNAVAILABLE), "{err}");
}
