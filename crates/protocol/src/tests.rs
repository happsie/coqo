// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests: a real client and server over in-memory and Unix
//! socket streams.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use crate::*;

#[derive(Debug)]
enum Add {}
impl Method for Add {
    const NAME: &'static str = "test/add";
    type Params = AddParams;
    type Result = i64;
}

#[derive(Debug, Serialize, Deserialize)]
struct AddParams {
    a: i64,
    b: i64,
}

#[derive(Debug)]
enum Ping {}
impl Method for Ping {
    const NAME: &'static str = "test/ping";
    type Params = ();
    type Result = ();
}

#[derive(Debug)]
enum Fail {}
impl Method for Fail {
    const NAME: &'static str = "test/fail";
    type Params = ();
    type Result = ();
}

#[derive(Debug)]
enum Sleep {}
impl Method for Sleep {
    const NAME: &'static str = "test/sleep";
    type Params = u64;
    type Result = ();
}

#[derive(Debug)]
enum Poke {}
impl NotificationMethod for Poke {
    const NAME: &'static str = "test/poke";
    type Params = String;
}

#[derive(Debug)]
enum Poked {}
impl NotificationMethod for Poked {
    const NAME: &'static str = "test/poked";
    type Params = String;
}

#[derive(Debug)]
enum Ticks {}
impl Topic for Ticks {
    const NAME: &'static str = "test/ticks";
    type Params = ();
    type Event = Tick;
}

#[derive(Debug)]
enum Finite {}
impl Topic for Finite {
    const NAME: &'static str = "test/finite";
    type Params = u64;
    type Event = u64;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Tick {
    n: u32,
    label: String,
}

fn tick(n: u32) -> Tick {
    Tick {
        n,
        label: format!("tick {n}"),
    }
}

struct Fixture {
    log: Arc<SeqLog<Tick>>,
    pokes: Arc<Mutex<Vec<String>>>,
}

fn router(f: &Fixture) -> Router {
    let log = Arc::clone(&f.log);
    let pokes = Arc::clone(&f.pokes);
    Router::new()
        .method::<Add, _, _>(|_, p| async move { Ok(p.a + p.b) })
        .method::<Ping, _, _>(|_, ()| async { Ok(()) })
        .method::<Fail, _, _>(|_, ()| async {
            Err(ErrorObject::new(1234, "nope").with_data(json!({"why": "test"})))
        })
        .method::<Sleep, _, _>(|_, ms| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(())
        })
        .notification::<Poke, _, _>(move |ctx, text| {
            let pokes = Arc::clone(&pokes);
            async move {
                pokes.lock().await.push(text.clone());
                let _ = ctx.peer.notify::<Poked>(format!("poked: {text}")).await;
            }
        })
        .topic::<Ticks, _, _>(move |_, (), since| {
            let log = Arc::clone(&log);
            async move { log.subscribe(since) }
        })
        .topic::<Finite, _, _>(|_, count, _| async move {
            let events = futures_util::stream::iter((1..=count).map(|seq| {
                Ok(Sequenced {
                    seq,
                    event: seq * 100,
                })
            }))
            .boxed();
            Ok(EventFeed {
                head: count,
                events,
            })
        })
}

fn fixture() -> Fixture {
    Fixture {
        log: Arc::new(SeqLog::new(64)),
        pokes: Arc::default(),
    }
}

fn spawn_server(
    f: &Fixture,
    config: ServerConfig,
    io: DuplexStream,
) -> JoinHandle<Result<(), ProtocolError>> {
    let server = Server::new(router(f), config);
    tokio::spawn(async move { server.serve(io).await })
}

async fn connect_with(
    f: &Fixture,
    server: ServerConfig,
    client: ClientConfig,
) -> (Client, Notifications, JoinHandle<Result<(), ProtocolError>>) {
    let (a, b) = tokio::io::duplex(1 << 16);
    let task = spawn_server(f, server, b);
    let (c, n) = Client::connect(a, client).await.expect("connect");
    (c, n, task)
}

async fn connect(f: &Fixture) -> (Client, Notifications, JoinHandle<Result<(), ProtocolError>>) {
    connect_with(
        f,
        ServerConfig::new("test-server", "srv-1"),
        ClientConfig::new("test-client", "cli-1"),
    )
    .await
}

async fn next<T: Topic>(sub: &mut Subscription<T>) -> Sequenced<T::Event> {
    tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("event in time")
        .expect("stream open")
        .expect("event ok")
}

// ---- requests, responses, notifications -----------------------------------

#[tokio::test]
async fn typed_requests_in_every_encoding() {
    for enc in [vec![Encoding::Json], vec![Encoding::MessagePack]] {
        let f = fixture();
        let mut cfg = ClientConfig::new("c", "b");
        cfg.encodings = enc.clone();
        let (client, _n, _t) = connect_with(&f, ServerConfig::new("s", "b"), cfg).await;
        assert_eq!(client.welcome().encoding, enc[0]);
        assert_eq!(
            client
                .request::<Add>(AddParams { a: 2, b: 40 })
                .await
                .unwrap(),
            42
        );
        client.request::<Ping>(()).await.unwrap();
    }
}

#[tokio::test]
async fn welcome_carries_versions_build_and_connection_ids() {
    let f = fixture();
    let server = Server::new(router(&f), ServerConfig::new("coqod", "srv-build"));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (a, b) = tokio::io::duplex(1 << 16);
        let s = server.clone();
        tokio::spawn(async move { s.serve(b).await });
        let (c, _) = Client::connect(a, ClientConfig::new("c", "cli"))
            .await
            .unwrap();
        let w = c.welcome();
        assert_eq!(w.protocol, PROTOCOL_VERSION);
        assert_eq!(w.negotiated, PROTOCOL_VERSION);
        assert_eq!(w.build, "srv-build");
        assert_eq!(w.server, "coqod");
        assert_eq!(w.encoding, Encoding::MessagePack);
        ids.push(w.connection);
    }
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn encoding_falls_back_to_json() {
    let f = fixture();
    let mut server = ServerConfig::new("s", "b");
    server.encodings = vec![Encoding::Json];
    let (client, _n, _t) = connect_with(&f, server, ClientConfig::new("c", "b")).await;
    assert_eq!(client.welcome().encoding, Encoding::Json);
    assert_eq!(
        client
            .request::<Add>(AddParams { a: 1, b: 1 })
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn errors_are_reported_with_codes() {
    let f = fixture();
    let (client, _n, _t) = connect(&f).await;

    let e = client.request::<Fail>(()).await.unwrap_err();
    let rpc = e.rpc().expect("rpc error");
    assert_eq!(rpc.code, 1234);
    assert_eq!(rpc.data, Some(json!({"why": "test"})));

    let e = client.request_raw("test/missing", None).await.unwrap_err();
    assert_eq!(e.rpc().unwrap().code, codes::METHOD_NOT_FOUND);

    let e = client
        .request_raw(Add::NAME, Some(json!({"a": "x"})))
        .await
        .unwrap_err();
    assert_eq!(e.rpc().unwrap().code, codes::INVALID_PARAMS);

    let e = client
        .request_raw(builtin::Hello::NAME, Some(json!({})))
        .await
        .unwrap_err();
    assert_eq!(e.rpc().unwrap().code, codes::INVALID_REQUEST);

    // The connection is still usable.
    assert_eq!(
        client
            .request::<Add>(AddParams { a: 1, b: 2 })
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn concurrent_requests_complete_out_of_order() {
    let f = fixture();
    let (client, _n, _t) = connect(&f).await;
    let slow = {
        let c = client.clone();
        tokio::spawn(async move { c.request::<Sleep>(200).await })
    };
    let fast = client
        .request::<Add>(AddParams { a: 5, b: 5 })
        .await
        .unwrap();
    assert_eq!(fast, 10);
    assert!(!slow.is_finished());
    slow.await.unwrap().unwrap();
}

#[tokio::test]
async fn notifications_flow_both_ways() {
    let f = fixture();
    let (client, mut notifications, _t) = connect(&f).await;
    client.notify::<Poke>("hi".into()).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.method, Poked::NAME);
    assert_eq!(n.params, Some(json!("poked: hi")));
    assert_eq!(*f.pokes.lock().await, vec!["hi".to_owned()]);
}

#[tokio::test(start_paused = true)]
async fn request_timeout() {
    let f = fixture();
    let mut cfg = ClientConfig::new("c", "b");
    cfg.request_timeout = Some(Duration::from_millis(50));
    let (client, _n, _t) = connect_with(&f, ServerConfig::new("s", "b"), cfg).await;
    let e = client.request::<Sleep>(10_000).await.unwrap_err();
    assert!(matches!(e, ProtocolError::RequestTimeout { .. }), "{e}");
    client.request::<Ping>(()).await.unwrap();
}

#[tokio::test]
async fn server_going_away_fails_pending_requests() {
    let f = fixture();
    let (client, _n, task) = connect(&f).await;
    let pending = {
        let c = client.clone();
        tokio::spawn(async move { c.request::<Sleep>(60_000).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();
    let e = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("no hang")
        .unwrap()
        .unwrap_err();
    assert!(matches!(e, ProtocolError::Closed), "{e}");
    tokio::time::timeout(Duration::from_secs(5), client.closed())
        .await
        .unwrap();
    assert!(client.is_closed());
    assert!(matches!(
        client.request::<Ping>(()).await,
        Err(ProtocolError::Closed)
    ));
}

#[tokio::test]
async fn dropping_the_client_ends_the_server_connection() {
    let f = fixture();
    let (client, n, task) = connect(&f).await;
    client.request::<Ping>(()).await.unwrap();
    drop((client, n));
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("server noticed")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
}

// ---- subscriptions --------------------------------------------------------

#[tokio::test]
async fn live_subscription() {
    let f = fixture();
    f.log.publish(tick(0));
    let (client, _n, _t) = connect(&f).await;
    let mut sub = client.subscribe::<Ticks>((), None).await.unwrap();
    assert_eq!(sub.head(), 1);
    f.log.publish(tick(1));
    f.log.publish(tick(2));
    let a = next(&mut sub).await;
    let b = next(&mut sub).await;
    assert_eq!((a.seq, a.event), (2, tick(1)));
    assert_eq!((b.seq, b.event), (3, tick(2)));
    assert_eq!(sub.last_seq(), Some(3));
}

#[tokio::test]
async fn resume_from_seq_replays_missed_events() {
    let f = fixture();
    let (client, _n, _t) = connect(&f).await;

    let mut sub = client.subscribe::<Ticks>((), Some(0)).await.unwrap();
    for i in 1..=3 {
        f.log.publish(tick(i));
    }
    for want in 1..=3 {
        assert_eq!(next(&mut sub).await.seq, want);
    }
    let resume = sub.resume_from();
    assert_eq!(resume, 3);
    drop(sub); // e.g. the UI closes

    for i in 4..=6 {
        f.log.publish(tick(i));
    }

    // Reattach on a new connection from the last seen seq.
    let (client2, _n2, _t2) = connect(&f).await;
    let mut sub = client2.subscribe::<Ticks>((), Some(resume)).await.unwrap();
    assert_eq!(sub.head(), 6);
    let seqs: Vec<u64> = [
        next(&mut sub).await,
        next(&mut sub).await,
        next(&mut sub).await,
    ]
    .iter()
    .map(|s| s.seq)
    .collect();
    assert_eq!(seqs, vec![4, 5, 6]);
    f.log.publish(tick(7));
    assert_eq!(next(&mut sub).await.event, tick(7));
    drop(client);
}

#[tokio::test]
async fn resume_gap_is_a_clear_error() {
    let f = Fixture {
        log: Arc::new(SeqLog::new(2)),
        pokes: Arc::default(),
    };
    for i in 0..10 {
        f.log.publish(tick(i));
    }
    let (client, _n, _t) = connect(&f).await;
    let e = client.subscribe::<Ticks>((), Some(3)).await.unwrap_err();
    assert_eq!(e.rpc().unwrap().code, codes::RESUME_GAP);
    let e = client.subscribe::<Ticks>((), Some(99)).await.unwrap_err();
    assert_eq!(e.rpc().unwrap().code, codes::RESUME_GAP);
}

#[tokio::test]
async fn unknown_topic() {
    let f = fixture();
    let (client, _n, _t) = connect(&f).await;
    #[derive(Debug)]
    enum Nope {}
    impl Topic for Nope {
        const NAME: &'static str = "test/nope";
        type Params = ();
        type Event = ();
    }
    let e = client.subscribe::<Nope>((), None).await.unwrap_err();
    assert_eq!(e.rpc().unwrap().code, codes::UNKNOWN_SUBSCRIPTION);
}

#[tokio::test]
async fn server_ended_feed_ends_the_stream() {
    let f = fixture();
    let (client, _n, _t) = connect(&f).await;
    let mut sub = client.subscribe::<Finite>(3, None).await.unwrap();
    assert_eq!(sub.head(), 3);
    let got: Vec<_> = (&mut sub).map(|r| r.unwrap().event).collect().await;
    assert_eq!(got, vec![100, 200, 300]);
}

#[tokio::test]
async fn unsubscribe_stops_events() {
    let f = fixture();
    let (client, _n, _t) = connect(&f).await;
    let sub = client.subscribe::<Ticks>((), None).await.unwrap();
    assert!(sub.unsubscribe().await.unwrap());
    let e = client
        .request::<builtin::Unsubscribe>(subscription::UnsubscribeParams {
            subscription: SubscriptionId(999),
        })
        .await
        .unwrap();
    assert!(!e);
}

#[tokio::test]
async fn many_subscriptions_are_independent() {
    let f = fixture();
    let (client, _n, _t) = connect(&f).await;
    let mut a = client.subscribe::<Ticks>((), None).await.unwrap();
    let mut b = client.subscribe::<Ticks>((), None).await.unwrap();
    assert_ne!(a.id(), b.id());
    f.log.publish(tick(1));
    assert_eq!(next(&mut a).await.seq, 1);
    assert_eq!(next(&mut b).await.seq, 1);
    drop(a);
    f.log.publish(tick(2));
    assert_eq!(next(&mut b).await.seq, 2);
}

#[tokio::test]
async fn slow_client_gets_lagged_and_can_resume_without_loss() {
    let f = Fixture {
        log: Arc::new(SeqLog::new(1000)),
        pokes: Arc::default(),
    };
    let mut cfg = ClientConfig::new("c", "b");
    cfg.subscription_buffer = 4;
    let (client, _n, _t) = connect_with(&f, ServerConfig::new("s", "b"), cfg).await;
    let mut sub = client.subscribe::<Ticks>((), Some(0)).await.unwrap();
    for i in 1..=50 {
        f.log.publish(tick(i));
    }
    // Let the reader overflow the 4-slot buffer before we consume anything.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut seen = Vec::new();
    let resume_from = loop {
        match sub.next().await.expect("an item") {
            Ok(s) => seen.push(s.seq),
            Err(ProtocolError::Lagged { resume_from }) => break resume_from,
            Err(e) => panic!("unexpected {e}"),
        }
    };
    assert!(sub.next().await.is_none());
    assert_eq!(resume_from, *seen.last().unwrap());

    // Resubscribing from `resume_from` loses nothing, even if it lags again.
    let mut since = resume_from;
    while seen.len() < 50 {
        let mut sub = client.subscribe::<Ticks>((), Some(since)).await.unwrap();
        while seen.len() < 50 {
            match sub.next().await.expect("an item") {
                Ok(s) => seen.push(s.seq),
                Err(ProtocolError::Lagged { resume_from }) => {
                    since = resume_from;
                    break;
                }
                Err(e) => panic!("unexpected {e}"),
            }
        }
    }
    assert_eq!(seen, (1..=50).collect::<Vec<u64>>());
}

// ---- handshake ------------------------------------------------------------

#[tokio::test]
async fn incompatible_versions_fail_fast_on_both_sides() {
    let f = fixture();
    let mut cfg = ClientConfig::new("future-ui", "cli-9");
    cfg.protocol = ProtocolVersion {
        major: PROTOCOL_VERSION.major + 1,
        minor: 0,
    };
    let (a, b) = tokio::io::duplex(1 << 16);
    let server = spawn_server(&f, ServerConfig::new("coqod", "srv-0"), b);

    let e = tokio::time::timeout(Duration::from_secs(5), Client::connect(a, cfg))
        .await
        .expect("no hang")
        .unwrap_err();
    match &e {
        ProtocolError::IncompatibleVersion {
            local,
            peer,
            peer_build,
        } => {
            assert_eq!(local.major, PROTOCOL_VERSION.major + 1);
            assert_eq!(*peer, PROTOCOL_VERSION);
            assert_eq!(peer_build, "srv-0");
        }
        other => panic!("expected IncompatibleVersion, got {other:?}"),
    }
    let msg = e.to_string();
    assert!(msg.contains("incompatible Coqo Protocol version"), "{msg}");
    assert!(msg.contains("srv-0"), "{msg}");

    let server_err = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(server_err, ProtocolError::IncompatibleVersion { ref peer_build, .. } if peer_build == "cli-9"),
        "{server_err:?}"
    );
}

#[tokio::test]
async fn compatible_minor_versions_negotiate_down() {
    let f = fixture();
    let mut cfg = ClientConfig::new("c", "b");
    cfg.protocol = ProtocolVersion {
        major: PROTOCOL_VERSION.major,
        minor: PROTOCOL_VERSION.minor + 3,
    };
    let (client, _n, _t) = connect_with(&f, ServerConfig::new("s", "b"), cfg).await;
    assert_eq!(client.welcome().negotiated, PROTOCOL_VERSION);
}

#[tokio::test(start_paused = true)]
async fn silent_server_is_a_handshake_timeout_not_a_hang() {
    let (a, _b) = tokio::io::duplex(1 << 16);
    let mut cfg = ClientConfig::new("c", "b");
    cfg.handshake_timeout = Duration::from_millis(250);
    let e = Client::connect(a, cfg).await.unwrap_err();
    assert!(matches!(e, ProtocolError::HandshakeTimeout(_)), "{e}");
    assert!(e.to_string().contains("coqo/hello"));
}

#[tokio::test(start_paused = true)]
async fn silent_client_is_a_handshake_timeout() {
    let f = fixture();
    let (_a, b) = tokio::io::duplex(1 << 16);
    let mut cfg = ServerConfig::new("s", "b");
    cfg.handshake_timeout = Duration::from_millis(250);
    let e = spawn_server(&f, cfg, b).await.unwrap().unwrap_err();
    assert!(matches!(e, ProtocolError::HandshakeTimeout(_)), "{e}");
}

#[tokio::test]
async fn non_protocol_server_fails_fast() {
    let (a, mut b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let mut buf = [0u8; 256];
        let _ = b.read(&mut buf).await;
        let _ = b.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let e = tokio::time::timeout(
        Duration::from_secs(5),
        Client::connect(a, ClientConfig::new("c", "b")),
    )
    .await
    .expect("no hang")
    .unwrap_err();
    assert!(matches!(e, ProtocolError::FrameTooLarge { .. }), "{e}");
}

#[tokio::test]
async fn server_closing_during_handshake_is_an_error() {
    let (a, b) = tokio::io::duplex(1 << 16);
    drop(b);
    let e = Client::connect(a, ClientConfig::new("c", "b"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, ProtocolError::Handshake(_) | ProtocolError::Io(_)),
        "{e:?}"
    );
}

// ---- raw wire -------------------------------------------------------------

type Wire = Framed<DuplexStream, FrameCodec>;

async fn raw_server() -> (Wire, JoinHandle<Result<(), ProtocolError>>) {
    let f = fixture();
    let (a, b) = tokio::io::duplex(1 << 16);
    let task = spawn_server(&f, ServerConfig::new("s", "b"), b);
    (Framed::new(a, FrameCodec::default()), task)
}

async fn send_json(wire: &mut Wire, v: Value) {
    wire.send(Bytes::from(serde_json::to_vec(&v).unwrap()))
        .await
        .unwrap();
}

async fn recv_json(wire: &mut Wire) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(5), wire.next())
        .await
        .expect("reply in time")
        .expect("open")
        .unwrap();
    serde_json::from_slice(&frame).unwrap()
}

async fn raw_hello(wire: &mut Wire) {
    send_json(
        wire,
        json!({"jsonrpc": "2.0", "id": 0, "method": "coqo/hello", "params": {
            "protocol": PROTOCOL_VERSION, "build": "raw", "client": "raw", "encodings": ["json"]
        }}),
    )
    .await;
    let reply = recv_json(wire).await;
    assert_eq!(reply["result"]["encoding"], "json", "{reply}");
}

#[tokio::test]
async fn wire_requests_before_hello_get_not_initialized() {
    let (mut wire, task) = raw_server().await;
    send_json(
        &mut wire,
        json!({"jsonrpc": "2.0", "id": 1, "method": "test/add"}),
    )
    .await;
    let reply = recv_json(&mut wire).await;
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["error"]["code"], codes::NOT_INITIALIZED);
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn wire_hello_with_bad_params_is_rejected() {
    let (mut wire, task) = raw_server().await;
    send_json(
        &mut wire,
        json!({"jsonrpc": "2.0", "id": 0, "method": "coqo/hello", "params": {"protocol": 1}}),
    )
    .await;
    let reply = recv_json(&mut wire).await;
    assert_eq!(reply["error"]["code"], codes::INVALID_PARAMS);
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn wire_parse_and_invalid_request_errors() {
    let (mut wire, _task) = raw_server().await;
    raw_hello(&mut wire).await;

    wire.send(Bytes::from_static(b"{nope")).await.unwrap();
    let reply = recv_json(&mut wire).await;
    assert_eq!(reply["id"], Value::Null);
    assert_eq!(reply["error"]["code"], codes::PARSE_ERROR);

    send_json(
        &mut wire,
        json!([{"jsonrpc": "2.0", "id": 1, "method": "x"}]),
    )
    .await;
    let reply = recv_json(&mut wire).await;
    assert_eq!(reply["error"]["code"], codes::INVALID_REQUEST);

    send_json(
        &mut wire,
        json!({"jsonrpc": "2.0", "id": 5, "method": "x", "result": 1}),
    )
    .await;
    let reply = recv_json(&mut wire).await;
    assert_eq!(reply["id"], 5);
    assert_eq!(reply["error"]["code"], codes::INVALID_REQUEST);

    // Still serving.
    send_json(
        &mut wire,
        json!({"jsonrpc": "2.0", "id": 6, "method": "test/add", "params": {"a": 1, "b": 2}}),
    )
    .await;
    assert_eq!(
        recv_json(&mut wire).await,
        json!({"jsonrpc": "2.0", "id": 6, "result": 3})
    );
}

#[tokio::test]
async fn wire_subscription_shape() {
    let (mut wire, _task) = raw_server().await;
    raw_hello(&mut wire).await;
    send_json(
        &mut wire,
        json!({"jsonrpc": "2.0", "id": 1, "method": "coqo/subscribe",
               "params": {"topic": "test/finite", "params": 2}}),
    )
    .await;
    assert_eq!(
        recv_json(&mut wire).await,
        json!({"jsonrpc": "2.0", "id": 1, "result": {"subscription": 1, "head": 2}})
    );
    for seq in 1..=2u64 {
        assert_eq!(
            recv_json(&mut wire).await,
            json!({"jsonrpc": "2.0", "method": "coqo/event",
                   "params": {"subscription": 1, "seq": seq, "event": seq * 100}})
        );
    }
    assert_eq!(
        recv_json(&mut wire).await,
        json!({"jsonrpc": "2.0", "method": "coqo/subscriptionEnded",
               "params": {"subscription": 1}})
    );
}

// ---- unix sockets ---------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn over_a_unix_socket() {
    let dir = std::env::temp_dir().join(format!("coqo-proto-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.sock");
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    let f = fixture();
    let server = Server::new(router(&f), ServerConfig::new("coqod", "b"));
    let accept = tokio::spawn(unix::serve_unix(server, listener));

    let (a, _) = unix::connect_unix(&path, ClientConfig::new("ui", "b"))
        .await
        .unwrap();
    let (b, _) = unix::connect_unix(&path, ClientConfig::new("cli", "b"))
        .await
        .unwrap();
    assert_ne!(a.welcome().connection, b.welcome().connection);
    assert_eq!(a.request::<Add>(AddParams { a: 1, b: 2 }).await.unwrap(), 3);
    let mut sub = b.subscribe::<Ticks>((), None).await.unwrap();
    f.log.publish(tick(1));
    assert_eq!(next(&mut sub).await.event, tick(1));

    accept.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
