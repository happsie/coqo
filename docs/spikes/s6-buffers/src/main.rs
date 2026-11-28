//! Spike S6 benchmark: op-log (model A) vs yrs CRDT (model B).
//!
//!   cargo run --release            # all measurements, markdown to stdout
//!
//! Throwaway code. The daemon side runs on tokio like coqod; the UI clients are plain
//! threads with blocking sockets (a UI's background I/O thread).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use ropey::Rope;
use spike_s6_buffers::crdt::Replica;
use spike_s6_buffers::oplog::{Changed, Client, Server};
use spike_s6_buffers::ot::Edit;
use spike_s6_buffers::wire::{self, Encoding, Msg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc as tmpsc;

// ---------------------------------------------------------------- allocation counter

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LIVE.fetch_add(n as isize - l.size() as isize, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

/// Heap bytes retained by whatever `f` builds (and returns, so it stays alive).
fn heap_of<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let before = live();
    let v = f();
    (v, (live() - before) as f64 / 1e6)
}

// ---------------------------------------------------------------- helpers

fn gen_file(lines: usize, seed: u64) -> String {
    let mut rng = SmallRng::seed_from_u64(seed);
    let idents = [
        "value", "buffer", "count", "result", "item", "state", "config", "index",
    ];
    let mut s = String::with_capacity(lines * 34);
    for i in 0..lines {
        let a = idents[rng.random_range(0..idents.len())];
        let b = idents[rng.random_range(0..idents.len())];
        match i % 6 {
            0 => s.push_str(&format!("fn {a}_{i}(x: u32) -> u32 {{\n")),
            1 => s.push_str(&format!(
                "    let {a} = {b}.len() + {};\n",
                rng.random_range(0..999)
            )),
            2 => s.push_str(&format!("    // {b} handles the {a} case\n")),
            3 => s.push_str(&format!("    if {a} > x {{ return {b}; }}\n")),
            4 => s.push_str(&format!("    {a}.push({b} * 2);\n")),
            _ => s.push_str("}\n"),
        }
    }
    s
}

fn gen_bytes(bytes: usize) -> String {
    let mut s = gen_file(bytes / 28 + 10, 7);
    let mut cut = bytes.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

fn line_starts(s: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(s.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn apply_rope(r: &mut Rope, edits: &[Edit]) {
    for e in edits.iter().rev() {
        if e.end > e.start {
            r.remove(e.start..e.end);
        }
        if !e.text.is_empty() {
            r.insert(e.start, &e.text);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Model {
    OpLog,
    Crdt,
}

impl Model {
    fn name(self) -> &'static str {
        match self {
            Model::OpLog => "op-log",
            Model::Crdt => "yrs",
        }
    }
}

// ---------------------------------------------------------------- daemon (tokio)

enum DaemonState {
    Ot { server: Server, seen: Vec<u64> },
    Y(Replica),
}

struct Daemon {
    state: DaemonState,
    writers: Vec<tmpsc::UnboundedSender<Arc<Vec<u8>>>>,
    enc: Encoding,
}

impl Daemon {
    fn broadcast(&self, frame: Arc<Vec<u8>>, skip: Option<usize>) {
        for (i, w) in self.writers.iter().enumerate() {
            if Some(i) != skip {
                let _ = w.send(frame.clone());
            }
        }
    }

    fn changed(&self, c: Changed) {
        let frame = wire::encode(
            &Msg::Changed {
                buffer: 1,
                rev: c.rev,
                origin: c.origin,
                seq: c.seq,
                edits: c.edits,
            },
            self.enc,
        );
        self.broadcast(Arc::new(frame), None);
    }

    fn on_message(&mut self, conn: usize, msg: Msg) {
        match (&mut self.state, msg) {
            (
                DaemonState::Ot { server, seen },
                Msg::Edit {
                    base_rev,
                    seq,
                    edits,
                    ..
                },
            ) => {
                let c = server
                    .receive(conn as u32 + 1, seq, base_rev, &edits)
                    .expect("daemon accepts edit");
                seen[conn] = base_rev;
                let min = *seen.iter().min().unwrap();
                server.trim_log(min);
                self.changed(c);
            }
            (DaemonState::Y(doc), Msg::Update { update, .. }) => {
                doc.apply_remote(&update);
                let frame = wire::encode(
                    &Msg::Update {
                        buffer: 1,
                        origin: conn as u32 + 1,
                        update,
                    },
                    self.enc,
                );
                self.broadcast(Arc::new(frame), Some(conn));
            }
            _ => panic!("message does not match the model"),
        }
    }

    fn agent_edit(&mut self, rng: &mut SmallRng, starts: &[usize]) {
        // An agent rewriting a word near the top of the file, away from the typist.
        let line = rng.random_range(100..200);
        let at = starts[line];
        let e = Edit::new(at..at + 2, "fn");
        match &mut self.state {
            DaemonState::Ot { server, .. } => {
                let c = server.local_edit(&[e]).unwrap();
                self.changed(c);
            }
            DaemonState::Y(doc) => {
                let update = doc.agent_edit("agent", &[e]);
                let frame = wire::encode(
                    &Msg::Update {
                        buffer: 1,
                        origin: 0,
                        update,
                    },
                    self.enc,
                );
                self.broadcast(Arc::new(frame), None);
            }
        }
    }
}

fn spawn_daemon(
    path: PathBuf,
    model: Model,
    enc: Encoding,
    text: String,
    clients: usize,
    agent_every: Option<Duration>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            let starts = line_starts(&text);
            let state = match model {
                Model::OpLog => DaemonState::Ot {
                    server: Server::new(&text),
                    seen: vec![0; clients],
                },
                Model::Crdt => DaemonState::Y(Replica::from_text(1, &text)),
            };
            let daemon = Rc::new(RefCell::new(Daemon {
                state,
                writers: Vec::new(),
                enc,
            }));
            for conn in 0..clients {
                let (sock, _) = listener.accept().await.unwrap();
                let (mut rd, mut wr) = sock.into_split();
                let (tx, mut rx) = tmpsc::unbounded_channel::<Arc<Vec<u8>>>();
                daemon.borrow_mut().writers.push(tx);
                tokio::task::spawn_local(async move {
                    while let Some(frame) = rx.recv().await {
                        let mut buf = Vec::with_capacity(frame.len() + 4);
                        buf.extend_from_slice(&(frame.len() as u32).to_be_bytes());
                        buf.extend_from_slice(&frame);
                        if wr.write_all(&buf).await.is_err() {
                            break;
                        }
                    }
                });
                let d = daemon.clone();
                tokio::task::spawn_local(async move {
                    loop {
                        let mut len = [0u8; 4];
                        if rd.read_exact(&mut len).await.is_err() {
                            break;
                        }
                        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
                        rd.read_exact(&mut buf).await.unwrap();
                        let enc = d.borrow().enc;
                        let msg = wire::decode(&buf, enc);
                        d.borrow_mut().on_message(conn, msg);
                    }
                });
            }
            let mut rng = SmallRng::seed_from_u64(3);
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match agent_every {
                    Some(every) => {
                        tokio::time::sleep(every).await;
                        daemon.borrow_mut().agent_edit(&mut rng, &starts);
                    }
                    None => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        });
    })
}

// ---------------------------------------------------------------- UI clients

enum UiState {
    Ot(Client),
    Y { doc: Replica, editor: Rope },
}

struct LatencyResult {
    local_us: Vec<f64>,
    encode_us: Vec<f64>,
    e2e_us: Vec<f64>,
    remote_apply_us: Vec<f64>,
    frame_bytes: usize,
    converged: bool,
}

fn latency_run(
    model: Model,
    enc: Encoding,
    text: &str,
    keystrokes: usize,
    agent_every: Option<Duration>,
) -> LatencyResult {
    let dir = std::env::temp_dir().join(format!(
        "coqo-s6-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("coqod.sock");
    let stop = Arc::new(AtomicBool::new(false));
    let daemon = spawn_daemon(
        path.clone(),
        model,
        enc,
        text.to_string(),
        2,
        agent_every,
        stop.clone(),
    );

    let new_state = |id: u32| match model {
        Model::OpLog => UiState::Ot(Client::new(id, text, 0)),
        Model::Crdt => {
            // Snapshot from the daemon's document: same item ids on every side.
            let d = Replica::from_text(1, text);
            let mut doc = Replica::new(id as u64 + 1);
            doc.apply_remote(&d.full_state());
            UiState::Y {
                doc,
                editor: Rope::from_str(text),
            }
        }
    };

    // Typist (window A) and observer (window B), both connected before typing starts.
    let a_sock = UnixStream::connect(&path).unwrap();
    let b_sock = UnixStream::connect(&path).unwrap();
    let a_state = Arc::new(Mutex::new(new_state(1)));
    let b_state = Arc::new(Mutex::new(new_state(2)));
    let a_writer = Arc::new(Mutex::new(a_sock.try_clone().unwrap()));

    // B: receives, applies to its replica + editor, reports (origin, time, apply cost).
    let (b_tx, b_rx) = mpsc::channel::<(u32, Instant, f64)>();
    {
        let b_state = b_state.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(b_sock);
            while let Ok(frame) = wire::read_frame(&mut r) {
                let msg = wire::decode(&frame, enc);
                let t = Instant::now();
                let origin = {
                    let mut st = b_state.lock().unwrap();
                    match (&mut *st, msg) {
                        (
                            UiState::Ot(c),
                            Msg::Changed {
                                rev,
                                origin,
                                seq,
                                edits,
                                ..
                            },
                        ) => {
                            c.receive(&Changed {
                                rev,
                                origin,
                                seq,
                                edits,
                            })
                            .unwrap();
                            origin
                        }
                        (UiState::Y { doc, editor }, Msg::Update { origin, update, .. }) => {
                            let edits = doc.apply_remote(&update);
                            apply_rope(editor, &edits);
                            origin
                        }
                        _ => panic!(),
                    }
                };
                let _ = b_tx.send((origin, Instant::now(), us(t.elapsed())));
            }
        });
    }
    // A's reader: acks, buffered sends and agent edits.
    {
        let a_state = a_state.clone();
        let a_writer = a_writer.clone();
        let sock = a_sock.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut r = BufReader::new(sock);
            while let Ok(frame) = wire::read_frame(&mut r) {
                let msg = wire::decode(&frame, enc);
                let mut st = a_state.lock().unwrap();
                match (&mut *st, msg) {
                    (
                        UiState::Ot(c),
                        Msg::Changed {
                            rev,
                            origin,
                            seq,
                            edits,
                            ..
                        },
                    ) => {
                        let (_, out) = c
                            .receive(&Changed {
                                rev,
                                origin,
                                seq,
                                edits,
                            })
                            .unwrap();
                        if let Some(out) = out {
                            let f = wire::encode(
                                &Msg::Edit {
                                    buffer: 1,
                                    base_rev: out.base_rev,
                                    seq: out.seq,
                                    edits: out.edits,
                                },
                                enc,
                            );
                            wire::write_frame(&mut *a_writer.lock().unwrap(), &f).unwrap();
                        }
                    }
                    (UiState::Y { doc, editor }, Msg::Update { update, .. }) => {
                        let edits = doc.apply_remote(&update);
                        apply_rope(editor, &edits);
                    }
                    _ => panic!(),
                }
            }
        });
    }

    let starts = line_starts(text);
    let mut cursor = starts[starts.len() / 2] + 4;
    let mut res = LatencyResult {
        local_us: Vec::new(),
        encode_us: Vec::new(),
        e2e_us: Vec::new(),
        remote_apply_us: Vec::new(),
        frame_bytes: 0,
        converged: false,
    };
    let warmup = 200;
    for i in 0..warmup + keystrokes {
        let ch = ["a", "b", "c", " ", "é"][i % 5];
        let e = Edit::new(cursor..cursor, ch);
        cursor += ch.len();
        let t0 = Instant::now();
        let (local, encode, frame) = {
            let mut st = a_state.lock().unwrap();
            match &mut *st {
                UiState::Ot(c) => {
                    let out = c.local_edit(&[e]).unwrap();
                    let local = t0.elapsed();
                    let frame = out.map(|o| {
                        wire::encode(
                            &Msg::Edit {
                                buffer: 1,
                                base_rev: o.base_rev,
                                seq: o.seq,
                                edits: o.edits,
                            },
                            enc,
                        )
                    });
                    (local, t0.elapsed() - local, frame)
                }
                UiState::Y { doc, editor } => {
                    apply_rope(editor, std::slice::from_ref(&e));
                    let update = doc.local_edit(&[e]);
                    let local = t0.elapsed();
                    let frame = wire::encode(
                        &Msg::Update {
                            buffer: 1,
                            origin: 1,
                            update,
                        },
                        enc,
                    );
                    (local, t0.elapsed() - local, Some(frame))
                }
            }
        };
        let sent = frame.is_some();
        if let Some(f) = &frame {
            res.frame_bytes = f.len() + 4;
            wire::write_frame(&mut *a_writer.lock().unwrap(), f).unwrap();
        }
        // Wait until B has applied A's keystroke (skip agent edits B also receives).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = None;
        while got.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            match b_rx.recv_timeout(left) {
                Ok((1, t, apply)) => got = Some((t, apply)),
                Ok(_) => {}
                Err(_) => panic!("window B never applied keystroke {i}"),
            }
        }
        if i >= warmup && sent {
            if let Some((t, apply)) = got {
                res.local_us.push(us(local));
                res.encode_us.push(us(encode));
                res.e2e_us.push(us(t.duration_since(t0)));
                res.remote_apply_us.push(apply);
            }
        }
    }
    // Let agent edits settle, then compare.
    stop.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    let text_of = |s: &UiState| match s {
        UiState::Ot(c) => c.doc.to_string(),
        UiState::Y { editor, .. } => editor.to_string(),
    };
    res.converged = text_of(&a_state.lock().unwrap()) == text_of(&b_state.lock().unwrap());
    drop(a_writer);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = daemon; // detached: the process exits at the end
    res
}

// ---------------------------------------------------------------- sections

fn section_latency(text: &str) {
    println!("## Keystroke latency, 100k-line file, over a Unix socket\n");
    println!(
        "Window A types in the middle of the file; window B is subscribed. `local` = A's replica update incl. the editor rope (what blocks the keystroke); `e2e` = keystroke in A until B has applied it (A -> coqod -> B). 2,000 keystrokes after 200 warm-up.\n"
    );
    println!(
        "| Model | Encoding | Agent edits | local p50 / p99 (µs) | encode p50 (µs) | B apply p50 (µs) | e2e p50 / p99 (µs) | frame A→daemon (B) | converged |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for model in [Model::OpLog, Model::Crdt] {
        for enc in [Encoding::Json, Encoding::MsgPack] {
            for agent in [None, Some(Duration::from_millis(10))] {
                let mut r = latency_run(model, enc, text, 2000, agent);
                println!(
                    "| {} | {:?} | {} | {:.1} / {:.1} | {:.1} | {:.1} | {:.0} / {:.0} | {} | {} |",
                    model.name(),
                    enc,
                    if agent.is_some() { "100/s" } else { "none" },
                    pct(&mut r.local_us, 0.5),
                    pct(&mut r.local_us, 0.99),
                    pct(&mut r.encode_us, 0.5),
                    pct(&mut r.remote_apply_us, 0.5),
                    pct(&mut r.e2e_us, 0.5),
                    pct(&mut r.e2e_us, 0.99),
                    r.frame_bytes,
                    r.converged
                );
            }
        }
    }
    println!();
}

fn section_memory() {
    println!("## Memory (heap bytes retained, counting allocator)\n");
    println!(
        "`edits` = 10,000 single-character edits (90% inserts, 10% deletes) at random positions, or typed as one contiguous run. The op-log daemon keeps only the log entries a client may still rebase against (trimmed); untrimmed is the worst case if a client never acks. Both include the rope's own growth from fragmented chunks.\n"
    );
    println!(
        "| File | Text | op-log daemon (rope) | op-log UI extra | op-log daemon after 10k random edits, log trimmed / untrimmed | yrs doc after load | yrs doc after 10k random edits | yrs doc after 10k typed run | yrs UI total (doc + editor rope) | yrs encoded state after random edits |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    let files = [
        ("1 MB", gen_bytes(1_000_000)),
        ("100k lines", gen_file(100_000, 1)),
        ("10 MB", gen_bytes(10_000_000)),
    ];
    for (name, text) in &files {
        let (server, rope_mb) = heap_of(|| Server::new(text));
        // The UI's op-log replica *is* the editor rope; extra = client state minus the rope.
        let (client, client_mb) = heap_of(|| Client::new(1, text, 0));
        let ui_extra = client_mb - rope_mb;

        // Untrimmed log after random edits.
        let mut rng = SmallRng::seed_from_u64(9);
        let mut server = server;
        let before = live();
        for _ in 0..10_000 {
            let len = server.doc.len();
            let at = boundary_rope(&server.doc, rng.random_range(0..len));
            let e = if rng.random_bool(0.1) && at < len {
                Edit::new(at..next_char(&server.doc, at), "")
            } else {
                Edit::new(at..at, "x")
            };
            server.local_edit(&[e]).unwrap();
        }
        let untrimmed_mb = rope_mb + (live() - before) as f64 / 1e6;
        let rev = server.rev;
        server.trim_log(rev);
        server.log.shrink_to_fit();
        let trimmed_mb = rope_mb + (live() - before) as f64 / 1e6;

        let (mut doc, y_load) = heap_of(|| Replica::from_text(1, text));
        let before = live();
        let mut rng = SmallRng::seed_from_u64(9);
        for _ in 0..10_000 {
            let s_len = doc.text_len();
            let at = rng.random_range(0..s_len);
            if rng.random_bool(0.1) && at + 1 < s_len {
                doc.local_edit(&[Edit::new(at..at + 1, "")]);
            } else {
                doc.local_edit(&[Edit::new(at..at, "x")]);
            }
        }
        doc.undo.clear_all();
        let y_random = y_load + (live() - before) as f64 / 1e6;
        let encoded = doc.full_state().len() as f64 / 1e6;
        drop(doc);

        let (mut doc2, _) = heap_of(|| Replica::from_text(1, text));
        let before = live();
        let mut at = text.len() / 2;
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        for _ in 0..10_000 {
            doc2.local_edit(&[Edit::new(at..at, "x")]);
            at += 1;
        }
        doc2.undo.clear_all();
        let y_typed = y_load + (live() - before) as f64 / 1e6;
        drop(doc2);
        let (_ui, y_ui) = heap_of(|| {
            let d = Replica::from_text(1, text);
            let mut r = Replica::new(2);
            r.apply_remote(&d.full_state());
            drop(d);
            (r, Rope::from_str(text))
        });
        drop(client);
        println!(
            "| {name} | {:.2} MB | {rope_mb:.2} MB | {:.3} MB | {trimmed_mb:.2} / {untrimmed_mb:.2} MB | {y_load:.2} MB | {y_random:.2} MB | {y_typed:.2} MB | {y_ui:.2} MB | {encoded:.2} MB |",
            text.len() as f64 / 1e6,
            ui_extra.max(0.0),
        );
    }
    println!();
}

fn boundary_rope(r: &Rope, mut i: usize) -> usize {
    while !r.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char(r: &Rope, i: usize) -> usize {
    let mut j = i + 1;
    while j < r.len() && !r.is_char_boundary(j) {
        j += 1;
    }
    j
}

trait TextLen {
    fn text_len(&self) -> usize;
}
impl TextLen for Replica {
    fn text_len(&self) -> usize {
        use yrs::{Text, Transact};
        self.text.len(&self.doc.transact()) as usize
    }
}

fn section_large_edit(text: &str) {
    println!("## Large agent edit: replace 5,000 lines of the 100k-line file\n");
    let starts = line_starts(text);
    let (from, to) = (starts[40_000], starts[45_000]);
    let replacement = gen_file(5_000, 99);
    let edit = Edit::new(from..to, replacement.clone());
    println!(
        "Replaced {:.0} KB with {:.0} KB. Window A has an unacknowledged keystroke at line 50,000 (so the op-log client must transform).\n",
        (to - from) as f64 / 1e3,
        replacement.len() as f64 / 1e3
    );
    println!("| Step | op-log | yrs |");
    println!("|---|---|---|");

    // op-log
    let mut server = Server::new(text);
    let mut client = Client::new(1, text, 0);
    let ka = starts[50_000];
    client.local_edit(&[Edit::new(ka..ka, "k")]).unwrap();
    let t = Instant::now();
    let changed = server.local_edit(std::slice::from_ref(&edit)).unwrap();
    let ot_daemon = t.elapsed();
    let msg = Msg::Changed {
        buffer: 1,
        rev: changed.rev,
        origin: 0,
        seq: 0,
        edits: changed.edits.clone(),
    };
    let t = Instant::now();
    let ot_json = wire::encode(&msg, Encoding::Json);
    let ot_enc = t.elapsed();
    let ot_mp = wire::encode(&msg, Encoding::MsgPack);
    let t = Instant::now();
    let (ot_editor_edits, _) = client.receive(&changed).unwrap();
    let ot_client = t.elapsed();

    // yrs
    let mut daemon = Replica::from_text(1, text);
    let mut ui = Replica::new(2);
    ui.apply_remote(&daemon.full_state());
    let mut editor = Rope::from_str(text);
    ui.local_edit(&[Edit::new(ka..ka, "k")]);
    apply_rope(&mut editor, &[Edit::new(ka..ka, "k")]);
    let mem_before = live();
    let t = Instant::now();
    let update = daemon.agent_edit("agent", std::slice::from_ref(&edit));
    let y_daemon = t.elapsed();
    let ymsg = Msg::Update {
        buffer: 1,
        origin: 0,
        update: update.clone(),
    };
    let t = Instant::now();
    let y_json = wire::encode(&ymsg, Encoding::Json);
    let y_enc = t.elapsed();
    let y_mp = wire::encode(&ymsg, Encoding::MsgPack);
    let t = Instant::now();
    let y_editor_edits = ui.apply_remote(&update);
    let y_client = t.elapsed();
    let y_growth = (live() - mem_before) as f64 / 1e6;

    let mut ed1 = Rope::from_str(text);
    let t = Instant::now();
    apply_rope(&mut ed1, &ot_editor_edits);
    let rope_apply = t.elapsed();
    apply_rope(&mut editor, &y_editor_edits);
    assert_eq!(editor.to_string(), ui.string());
    assert_eq!(client.doc.to_string().len(), ui.string().len());

    println!(
        "| Daemon applies the edit | {:.2} ms | {:.2} ms |",
        ms(ot_daemon),
        ms(y_daemon)
    );
    println!(
        "| Broadcast size JSON / MessagePack | {:.0} / {:.0} KB | {:.0} / {:.0} KB |",
        ot_json.len() as f64 / 1e3,
        ot_mp.len() as f64 / 1e3,
        y_json.len() as f64 / 1e3,
        y_mp.len() as f64 / 1e3
    );
    println!(
        "| JSON encode | {:.2} ms | {:.2} ms |",
        ms(ot_enc),
        ms(y_enc)
    );
    println!(
        "| UI replica applies (transform / integrate, incl. rope for op-log) | {:.2} ms | {:.2} ms |",
        ms(ot_client),
        ms(y_client)
    );
    println!(
        "| Editor edits produced | {} | {} |",
        ot_editor_edits.len(),
        y_editor_edits.len()
    );
    println!(
        "| Editor rope apply of those edits | {:.2} ms | (same) |",
        ms(rope_apply)
    );
    println!(
        "| Heap growth from the edit on the replica | (log entry ≈ text) | {y_growth:.2} MB |"
    );
    println!();

    // ACP fs/write_text_file sends the whole new file: the daemon must diff.
    let mut new_full = String::with_capacity(text.len());
    new_full.push_str(&text[..from]);
    new_full.push_str(&replacement);
    new_full.push_str(&text[to..]);
    let t = Instant::now();
    let edits = line_diff(text, &new_full);
    let diff_t = t.elapsed();
    let t = Instant::now();
    let _quick = spike_s6_buffers::crdt::naive_diff(text, &new_full);
    let quick_t = t.elapsed();
    println!(
        "**Whole-file agent write (ACP `fs/write_text_file`)** must be diffed into edits so concurrent user edits and cursors survive: line diff (`similar`, Myers) of the 3.3 MB file takes **{:.1} ms** and yields {} edit(s); a prefix/suffix scan takes **{:.2} ms** (1 edit, enough when the agent changed one region).\n",
        ms(diff_t),
        edits.len(),
        ms(quick_t)
    );
    let mut check = text.to_string();
    for e in edits.iter().rev() {
        check.replace_range(e.start..e.end, &e.text);
    }
    assert_eq!(check, new_full);
}

fn line_diff(old: &str, new: &str) -> Vec<Edit> {
    use similar::{Algorithm, DiffOp, TextDiff};
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(Duration::from_millis(500))
        .diff_lines(old, new);
    let old_starts = line_starts(old);
    let new_starts = line_starts(new);
    let ob = |i: usize| *old_starts.get(i).unwrap_or(&old.len());
    let nb = |i: usize| *new_starts.get(i).unwrap_or(&new.len());
    let mut out = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index, old_len, ..
            } => out.push(Edit::new(ob(old_index)..ob(old_index + old_len), "")),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => out.push(Edit::new(
                ob(old_index)..ob(old_index),
                &new[nb(new_index)..nb(new_index + new_len)],
            )),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => out.push(Edit::new(
                ob(old_index)..ob(old_index + old_len),
                &new[nb(new_index)..nb(new_index + new_len)],
            )),
        }
    }
    out
}

fn section_open(text: &str) {
    println!("## Opening the 100k-line file and attaching a UI\n");
    println!("| Step | op-log | yrs |");
    println!("|---|---|---|");
    let t = Instant::now();
    let server = Server::new(text);
    let ot_load = t.elapsed();
    let t = Instant::now();
    let snap = server.doc.to_string();
    let snap_json = serde_json::to_vec(&serde_json::json!({"rev": 0, "text": snap})).unwrap();
    let ot_snap = t.elapsed();
    let t = Instant::now();
    let _c = Client::new(1, text, 0);
    let ot_attach = t.elapsed();

    let t = Instant::now();
    let d = Replica::from_text(1, text);
    let y_load = t.elapsed();
    let t = Instant::now();
    let state = d.full_state();
    let y_snap = t.elapsed();
    let t = Instant::now();
    let mut r = Replica::new(2);
    r.apply_remote(&state);
    let _editor = Rope::from_str(&r.string());
    let y_attach = t.elapsed();
    println!(
        "| Daemon loads the file | {:.1} ms | {:.1} ms |",
        ms(ot_load),
        ms(y_load)
    );
    println!(
        "| Snapshot for a new client | {:.1} ms, {:.2} MB (JSON) | {:.1} ms, {:.2} MB (v1 update) |",
        ms(ot_snap),
        snap_json.len() as f64 / 1e6,
        ms(y_snap),
        state.len() as f64 / 1e6
    );
    println!(
        "| UI builds its replica + editor text | {:.1} ms | {:.1} ms |",
        ms(ot_attach),
        ms(y_attach)
    );
    println!();
}

fn section_reconnect(text: &str) {
    println!("## Reattach with unsaved edits\n");
    // UI restart: unsaved edits live in the daemon, so a new UI just takes a snapshot
    // (see the open section). Network-style disconnect with offline typing:
    let starts = line_starts(text);
    let mut server = Server::new(text);
    let mut c = Client::new(1, text, 0);
    let mut rng = SmallRng::seed_from_u64(5);
    let mut at = starts[60_000];
    for _ in 0..500 {
        c.local_edit(&[Edit::new(at..at, "z")]).unwrap();
        at += 1;
    }
    let mut missed = Vec::new();
    for _ in 0..500 {
        let p = starts[rng.random_range(0..30_000)];
        missed.push(server.local_edit(&[Edit::new(p..p, "a")]).unwrap());
    }
    let t = Instant::now();
    for m in &missed {
        c.receive(m).unwrap();
    }
    let out = c.resend().unwrap();
    let ack = server
        .receive(1, out.seq, out.base_rev, &out.edits)
        .unwrap();
    c.receive(&ack).unwrap();
    let ot_t = t.elapsed();
    assert_eq!(c.doc.to_string(), server.doc.to_string());

    let mut d = Replica::from_text(1, text);
    let mut u = Replica::new(2);
    u.apply_remote(&d.full_state());
    let mut at = starts[60_000];
    for _ in 0..500 {
        u.local_edit(&[Edit::new(at..at, "z")]);
        at += 1;
    }
    let mut rng = SmallRng::seed_from_u64(5);
    for _ in 0..500 {
        let p = starts[rng.random_range(0..30_000)];
        d.agent_edit("agent", &[Edit::new(p..p, "a")]);
    }
    let t = Instant::now();
    let to_u = d.diff_since(&u.state_vector());
    let to_d = u.diff_since(&d.state_vector());
    d.apply_remote(&to_d);
    u.apply_remote(&to_u);
    let y_t = t.elapsed();
    assert_eq!(u.string(), d.string());
    println!(
        "500 offline keystrokes in the UI while the daemon took 500 agent edits, then reconnect: op-log replays the missed revisions and resends the pending change in **{:.1} ms**; yrs exchanges state vectors and diffs in **{:.1} ms** (diffs {} + {} bytes). Both converge.\n",
        ms(ot_t),
        ms(y_t),
        to_u.len(),
        to_d.len()
    );
}

fn main() {
    println!("# Spike S6 measurements\n");
    println!(
        "Machine: {} logical CPUs, release build.\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    let text = gen_file(100_000, 1);
    println!("100k-line file: {:.2} MB.\n", text.len() as f64 / 1e6);
    section_open(&text);
    section_latency(&text);
    section_large_edit(&text);
    section_reconnect(&text);
    section_memory();
}
