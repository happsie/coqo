// SPDX-License-Identifier: Apache-2.0

//! `syntax/*` over the protocol, on real buffers.

use std::sync::Arc;
use std::time::Duration;

use coqo_protocol::methods::buffer::{self as bufm, BufferId, OpenParams, TextEdit};
use coqo_protocol::methods::codes;
use coqo_protocol::methods::syntax::{self as synm, FoldKind, SymbolKind, SyntaxParams};
use coqo_protocol::methods::workspace::{self as wsm, RootId, WorkspaceId};
use coqo_protocol::{Client, ClientConfig, Router, Server, ServerConfig};
use coqo_storage::DataDirs;
use tempfile::TempDir;

use super::{SyntaxService, register};
use crate::buffers::{self, BufferConfig, BufferService};
use crate::workspace::{self, WatchConfig, WorkspaceService};

const RUST: &str = "\
use std::fmt;
use std::io;

mod shapes {
    pub struct Circle {
        radius: f64,
    }

    impl Circle {
        fn area(&self) -> f64 {
            3.14 * self.radius * self.radius
        }
    }
}
";

struct Daemon {
    _data: TempDir,
    _folder: TempDir,
    buffers: Arc<BufferService>,
    syntax: Arc<SyntaxService>,
    client: Client,
    workspace: WorkspaceId,
    root: RootId,
}

impl Daemon {
    async fn new(files: &[(&str, &str)]) -> Self {
        let data = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(folder.path().join(name), text).unwrap();
        }
        let dirs = DataDirs::at(data.path());
        let watch = WatchConfig {
            debounce: Duration::from_millis(30),
            ..WatchConfig::default()
        };
        let workspaces = Arc::new(WorkspaceService::open_with(dirs.clone(), watch).unwrap());
        let buffers = BufferService::new(Arc::clone(&workspaces), dirs, BufferConfig::default());
        let syntax = SyntaxService::new(Arc::clone(&buffers));
        let router = workspace::register(Router::new(), workspaces);
        let router = register(router, Arc::clone(&syntax));
        let router = buffers::register(router, Arc::clone(&buffers));
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
            _folder: folder,
            buffers,
            syntax,
            client,
            workspace: info.id,
            root: info.roots[0].id,
        }
    }

    async fn open(&self, path: &str) -> BufferId {
        self.client
            .request::<bufm::Open>(OpenParams {
                workspace: self.workspace.clone(),
                root: self.root,
                path: path.to_owned(),
                replica: None,
            })
            .await
            .unwrap()
            .buffer
    }

    async fn symbols(&self, buffer: BufferId) -> synm::SymbolsResult {
        self.client
            .request::<synm::Symbols>(SyntaxParams { buffer })
            .await
            .unwrap()
    }

    fn incremental_parses(&self, buffer: BufferId) -> u64 {
        let parsed = self.syntax.parsed.lock().unwrap();
        let entry = parsed[&buffer].lock().unwrap();
        entry.doc.as_ref().unwrap().incremental_parses()
    }
}

fn qualified(s: &synm::Symbol) -> String {
    let mut path = s.container.clone();
    path.push(s.name.clone());
    path.join("::")
}

#[tokio::test]
async fn symbols_outline_and_folds_of_an_open_buffer() {
    let d = Daemon::new(&[("lib.rs", RUST)]).await;
    let buffer = d.open("lib.rs").await;

    let r = d.symbols(buffer).await;
    assert_eq!(r.rev, 0);
    assert_eq!(r.language.as_deref(), Some("rust"));
    let names: Vec<_> = r.symbols.iter().map(qualified).collect();
    assert_eq!(
        names,
        [
            "shapes",
            "shapes::Circle",
            "shapes::Circle::radius",
            "shapes::Circle",
            "shapes::Circle::area"
        ]
    );
    let area = &r.symbols[4];
    assert_eq!(area.kind, SymbolKind::Method);
    assert_eq!(&RUST[area.name_range.start..area.name_range.end], "area");
    assert_eq!((area.start_line, area.end_line), (9, 11));

    let outline = d
        .client
        .request::<synm::Outline>(SyntaxParams { buffer })
        .await
        .unwrap();
    assert_eq!(outline.items.len(), 1);
    let shapes = &outline.items[0];
    assert_eq!(
        (shapes.name.as_str(), shapes.kind),
        ("shapes", SymbolKind::Module)
    );
    assert_eq!(shapes.children.len(), 2);
    assert_eq!(shapes.children[1].children[0].name, "area");

    let folds = d
        .client
        .request::<synm::Folding>(SyntaxParams { buffer })
        .await
        .unwrap();
    assert_eq!(
        (
            folds.ranges[0].start_line,
            folds.ranges[0].end_line,
            folds.ranges[0].kind
        ),
        (0, 1, FoldKind::Imports)
    );
    assert!(
        folds
            .ranges
            .iter()
            .any(|f| f.start_line == 3 && f.end_line == 13)
    );
}

#[tokio::test]
async fn follows_edits_with_incremental_reparses() {
    let d = Daemon::new(&[("lib.rs", RUST)]).await;
    let buffer = d.open("lib.rs").await;
    assert_eq!(d.symbols(buffer).await.symbols.len(), 5);

    // An agent renames the method, then adds a function, in two changes.
    let at = RUST.find("area").unwrap();
    d.buffers
        .agent_edit(buffer, "s-1", &[TextEdit::new(at..at + 4, "surface")])
        .unwrap();
    let end = RUST.len() + "surface".len() - "area".len();
    let rev = d
        .buffers
        .agent_edit(buffer, "s-1", &[TextEdit::insert(end, "\nfn main() {}\n")])
        .unwrap();

    let r = d.symbols(buffer).await;
    assert_eq!(r.rev, rev);
    let names: Vec<_> = r.symbols.iter().map(qualified).collect();
    assert!(
        names.contains(&"shapes::Circle::surface".to_owned()),
        "{names:?}"
    );
    assert_eq!(names.last().map(String::as_str), Some("main"));
    assert_eq!(d.incremental_parses(buffer), 2);

    // Asking again at the same revision re-parses nothing.
    assert_eq!(d.symbols(buffer).await, r);
    assert_eq!(d.incremental_parses(buffer), 2);
}

#[tokio::test]
async fn plain_text_has_no_symbols() {
    let d = Daemon::new(&[("notes.txt", "fn not_code() {}\n")]).await;
    let buffer = d.open("notes.txt").await;
    let r = d.symbols(buffer).await;
    assert_eq!(r.language, None);
    assert!(r.symbols.is_empty());
    // A shebang makes a script without an extension code.
    let d = Daemon::new(&[("tool", "#!/usr/bin/env node\nfunction run() {}\n")]).await;
    let buffer = d.open("tool").await;
    let r = d.symbols(buffer).await;
    assert_eq!(r.language.as_deref(), Some("javascript"));
    assert_eq!(r.symbols[0].name, "run");
}

// Session tokens and other workspaces: `routes::tests` probes every
// session route, these included.
#[tokio::test]
async fn unknown_buffers_are_refused() {
    let d = Daemon::new(&[("lib.rs", RUST)]).await;
    let err = d
        .client
        .request::<synm::Symbols>(SyntaxParams {
            buffer: BufferId(999),
        })
        .await
        .unwrap_err();
    assert_eq!(
        err.rpc().map(|e| e.code),
        Some(codes::UNKNOWN_BUFFER),
        "{err:?}"
    );
    assert_eq!(d.syntax.parsed_buffers(), 0);
}
