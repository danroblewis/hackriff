//! T-060: the token-authenticated TCP stream server. Fake producers stand in for the pipeline:
//! - refusals (token, handshake, unknown target, opener refusal, local-only) are one framed JSON
//!   object and reveal nothing before the token verifies;
//! - an always-on stream arrives with the contract's header, sequence numbers and records;
//! - an on-demand session gets its parameters (never the token) and is dropped on disconnect;
//! - a client that stops reading never blocks the producer, its drops are counted, and it is
//!   disconnected;
//! - `/api/streams` discovery lists streams with formats, openers and the TCP address;
//! - concurrent streams of one kind from one client are isolated, with per-stream drop counters.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hk_api::StreamRegistry;
use hk_api::stream::{
    BinaryRecord, OpenRefusal, OpenRequest, OpenedStream, OpenerRegistry, Publisher,
    PublisherConfig, Record, RecordFlags, StreamHeader, StreamKind, StreamOpener, StreamReader,
};
use hk_api::{ApiState, Server, ServerConfig, StreamServer, StreamServerConfig, Token};
use hk_model::{ContentClass, Timestamp};
use serde_json::{Value, json};

const TOKEN: &str = "t060-tcp-stream-token-0123456789abcdef";

fn token() -> Token {
    Token::from_config(TOKEN).unwrap()
}

fn serve(streams: StreamRegistry, openers: OpenerRegistry) -> StreamServer {
    StreamServer::start(
        StreamServerConfig::new("127.0.0.1:0".parse().unwrap(), token()),
        streams,
        openers,
    )
    .unwrap()
}

fn connect(addr: SocketAddr, line: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(line.as_bytes()).unwrap();
    s
}

/// Reads one frame as JSON (a header or a refusal).
fn frame_json(s: &mut TcpStream) -> Value {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    s.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn refused(addr: SocketAddr, line: &str) -> Value {
    let mut s = connect(addr, line);
    let v = frame_json(&mut s);
    assert_eq!(v["type"], "refused", "{line:?}: {v}");
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    assert!(rest.is_empty(), "nothing follows a refusal");
    v
}

fn bits_header(id: &str, class: ContentClass) -> StreamHeader {
    let mut h = StreamHeader::new(id, StreamKind::Bits, class, "test");
    h.datatype = Some("ru8".into());
    h.max_frame_len = 4096;
    h
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Opens a bits stream per request: `records` records once subscribed, or a flood of 1 KiB
/// records until the session is dropped; logs the request parameters.
struct Fake {
    refuse: Option<OpenRefusal>,
    flood: bool,
    records: u64,
    stopped: Arc<AtomicBool>,
    opened: AtomicUsize,
    params: Mutex<Vec<(String, String)>>,
    published: Arc<AtomicUsize>,
    worst_publish: Arc<Mutex<Duration>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            refuse: None,
            flood: false,
            records: 3,
            stopped: Arc::new(AtomicBool::new(false)),
            opened: AtomicUsize::new(0),
            params: Mutex::new(Vec::new()),
            published: Arc::new(AtomicUsize::new(0)),
            worst_publish: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }
}

struct Guard(Arc<AtomicBool>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl StreamOpener for Fake {
    fn open(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        *self.params.lock().unwrap() = req.params.clone();
        if let Some(r) = &self.refuse {
            return Err(r.clone());
        }
        self.opened.fetch_add(1, Ordering::SeqCst);
        let header = bits_header("bits/fake", ContentClass::Unrestricted);
        let config = PublisherConfig {
            queue_bytes: 64 * 1024,
            disconnect_after: Duration::from_millis(1500),
            max_consumers: 1,
            drain_timeout: Duration::from_millis(200),
            ..PublisherConfig::default()
        };
        let mut p = Publisher::new(header.clone(), config).unwrap();
        let handle = p.handle();
        let (stopped, flood, records) = (Arc::clone(&self.stopped), self.flood, self.records);
        let (published, worst) = (Arc::clone(&self.published), Arc::clone(&self.worst_publish));
        std::thread::spawn(move || {
            let t0 = Instant::now();
            while p.handle().open_consumers() == 0 && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let payload = vec![1u8; if flood { 1024 } else { 16 }];
            let mut i = 0u64;
            while !stopped.load(Ordering::SeqCst) {
                if flood || i < records {
                    let t = Instant::now();
                    let _ = p.publish_binary(BinaryRecord {
                        t: Timestamp::from_unix_nanos(1 + i as i64),
                        sample_index: i,
                        flags: RecordFlags::BURST_START.with(RecordFlags::BURST_END),
                        payload: &payload,
                    });
                    let dt = t.elapsed();
                    let mut w = worst.lock().unwrap();
                    *w = (*w).max(dt);
                    drop(w);
                    published.fetch_add(1, Ordering::SeqCst);
                    i += 1;
                } else {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(Guard(Arc::clone(&self.stopped))),
        })
    }

    fn describe(&self) -> Value {
        json!({ "kind": "bits", "datatype": "ru8" })
    }
}

#[test]
fn refusals_are_one_frame_and_reveal_nothing_before_the_token() {
    let streams = StreamRegistry::new();
    let p = Publisher::new(
        bits_header("bits/real", ContentClass::Unrestricted),
        PublisherConfig::default(),
    )
    .unwrap();
    streams.register(
        &bits_header("bits/real", ContentClass::Unrestricted),
        p.handle(),
    );
    let gate = Arc::new(Fake {
        refuse: Some(OpenRefusal::gated(
            ContentClass::RestrictedPaging,
            "paging band",
        )),
        ..Fake::new()
    });
    let openers = OpenerRegistry::new().with("gated", gate.clone());
    let server = serve(streams, openers);
    let addr = server.local_addr();

    // Wrong or missing token: identical 401 whether or not the target exists.
    let a = refused(addr, "bits/real?token=wrong-token-0123456789\n");
    let b = refused(addr, "bits/nope?token=wrong-token-0123456789\n");
    let c = refused(addr, "open/gated\n");
    assert_eq!(a["status"], 401);
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert_eq!(gate.params.lock().unwrap().len(), 0, "opener not reached");

    assert_eq!(refused(addr, "not a handshake\n")["status"], 400);
    assert_eq!(
        refused(addr, &format!("bits/real?token={TOKEN}\nextra bytes"))["status"],
        400
    );
    assert_eq!(
        refused(addr, &format!("bits/nope?token={TOKEN}\n"))["status"],
        404
    );
    assert_eq!(
        refused(addr, &format!("open/nope?token={TOKEN}\n"))["status"],
        404
    );
    assert_eq!(
        refused(addr, &format!("bits/real?token={TOKEN}&x=1\n"))["status"],
        400
    );
    let g = refused(addr, &format!("open/gated?token={TOKEN}&f_lo=1&f_hi=2\n"));
    assert_eq!(g["status"], 403);
    assert_eq!(g["content_class"], "restricted-paging");
    assert_eq!(gate.opened.load(Ordering::SeqCst), 0);
    assert_eq!(server.stats().refused, 9);
    assert_eq!(server.stats().served, 0);
}

#[test]
fn an_always_on_stream_arrives_with_header_sequence_numbers_and_records() {
    let streams = StreamRegistry::new();
    let header = bits_header("bits/fsk-bursts/test", ContentClass::Unrestricted);
    let mut p = Publisher::new(header.clone(), PublisherConfig::default()).unwrap();
    streams.register(&header, p.handle());
    let server = serve(streams, OpenerRegistry::new());
    // Percent-encoded target and a CRLF terminator are accepted.
    let s = connect(
        server.local_addr(),
        &format!("/bits%2Ffsk-bursts%2Ftest?token={TOKEN}\r\n"),
    );
    wait_for("the TCP consumer", Duration::from_secs(10), || {
        p.handle().open_consumers() == 1
    });
    for i in 0..5u8 {
        p.publish_binary(BinaryRecord {
            t: Timestamp::from_unix_nanos(i64::from(i) + 1),
            sample_index: u64::from(i) * 100,
            flags: RecordFlags::BURST_START.with(RecordFlags::BURST_END),
            payload: &[i, 1, 0, 1],
        })
        .unwrap();
    }
    drop(p);
    let mut r = StreamReader::new(s);
    let h = r.read_header().unwrap().clone();
    assert_eq!(h.stream_id, "bits/fsk-bursts/test");
    assert_eq!(h.kind, StreamKind::Bits);
    let mut n = 0u64;
    while let Some(rec) = r.next_record().unwrap() {
        let Record::Binary(b) = rec else {
            panic!("unexpected record {rec:?}")
        };
        assert_eq!(b.header.seq, n);
        assert_eq!(b.header.sample_index, n * 100);
        assert_eq!(b.payload, vec![n as u8, 1, 0, 1]);
        n += 1;
    }
    assert_eq!(n, 5, "every record, then a clean end of stream");
    assert_eq!(server.stats().served, 1);
}

#[test]
fn an_on_demand_session_gets_its_parameters_and_ends_on_disconnect() {
    let fake = Arc::new(Fake::new());
    let server = serve(
        StreamRegistry::new(),
        OpenerRegistry::new().with("bits", fake.clone()),
    );
    let s = connect(
        server.local_addr(),
        &format!("open/bits?token={TOKEN}&f_lo=433.9e6&f_hi=434.0e6\n"),
    );
    let mut r = StreamReader::new(s);
    assert_eq!(r.read_header().unwrap().stream_id, "bits/fake");
    for seq in 0..3 {
        match r.next_record().unwrap() {
            Some(Record::Binary(b)) => assert_eq!(b.header.seq, seq),
            other => panic!("unexpected {other:?}"),
        }
    }
    let params = fake.params.lock().unwrap().clone();
    assert_eq!(
        params,
        vec![
            ("f_lo".to_owned(), "433.9e6".to_owned()),
            ("f_hi".to_owned(), "434.0e6".to_owned())
        ],
        "parameters reach the opener; the token does not"
    );
    wait_for("an active connection", Duration::from_secs(5), || {
        server.stats().active == 1
    });
    assert!(!fake.stopped.load(Ordering::SeqCst));
    drop(r);
    wait_for("the session to stop", Duration::from_secs(10), || {
        fake.stopped.load(Ordering::SeqCst)
    });
    wait_for("the connection to close", Duration::from_secs(10), || {
        server.stats().active == 0
    });
}

#[test]
fn a_client_that_stops_reading_never_blocks_the_producer_and_its_drops_are_counted() {
    let fake = Arc::new(Fake {
        flood: true,
        ..Fake::new()
    });
    let server = serve(
        StreamRegistry::new(),
        OpenerRegistry::new().with("flood", fake.clone()),
    );
    let mut s = connect(server.local_addr(), &format!("open/flood?token={TOKEN}\n"));
    // Read the header, then never read again.
    let header = frame_json(&mut s);
    assert_eq!(header["schema"], "hackriff.stream");
    wait_for("20k records published", Duration::from_secs(60), || {
        fake.published.load(Ordering::SeqCst) >= 20_000
    });
    let worst = *fake.worst_publish.lock().unwrap();
    assert!(
        worst < Duration::from_millis(250),
        "a publish waited {worst:?} on a slow client"
    );
    wait_for("drops to be counted", Duration::from_secs(20), || {
        server.stats().records_dropped > 0 || fake.stopped.load(Ordering::SeqCst)
    });
    // The stalled consumer is disconnected, which ends the session.
    wait_for(
        "the stalled client to be dropped",
        Duration::from_secs(30),
        || fake.stopped.load(Ordering::SeqCst),
    );
    wait_for("the connection to close", Duration::from_secs(10), || {
        server.stats().active == 0
    });
    let stats = server.stats();
    assert!(stats.records_dropped > 0, "{stats:?}");
    eprintln!(
        "published {} records, worst publish {worst:?}, {stats:?}",
        fake.published.load(Ordering::SeqCst)
    );
    drop(s);
}

#[test]
fn discovery_lists_streams_with_formats_openers_and_the_tcp_address() {
    let streams = StreamRegistry::new();
    let header = bits_header("bits/fsk-bursts/x", ContentClass::Unrestricted);
    let p = Publisher::new(header.clone(), PublisherConfig::default()).unwrap();
    streams.register(&header, p.handle());
    let openers = OpenerRegistry::new().with("bits", Arc::new(Fake::new()));
    let tcp = serve(streams.clone(), openers.clone());
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), token());
    config.stream_tcp = Some(tcp.local_addr());
    let state = ApiState {
        streams,
        on_demand: openers,
        ..ApiState::default()
    };
    let http = Server::start(config, state).unwrap();
    let mut s = TcpStream::connect(http.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET /api/streams HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    assert!(raw.starts_with(b"HTTP/1.1 200"));
    let doc: Value = serde_json::from_slice(&raw[split + 4..]).unwrap();
    let stream = &doc["streams"][0];
    assert_eq!(stream["stream_id"], "bits/fsk-bursts/x");
    assert_eq!(stream["tcp_target"], "bits/fsk-bursts/x");
    assert_eq!(stream["format"]["record_header_len"], 32);
    let od = &doc["on_demand"][0];
    assert_eq!(od["name"], "bits");
    assert_eq!(od["tcp_target"], "open/bits");
    assert_eq!(od["ws_path"], "/ws/open/bits");
    assert_eq!(od["datatype"], "ru8");
    assert_eq!(doc["tcp"]["addr"], tcp.local_addr().to_string());
}

/// One bits stream per request whose records carry the request's `tag` byte: `flood=1` publishes
/// 1 KiB records as fast as it can, otherwise 16 bytes every millisecond.
#[derive(Default)]
struct Tagged {
    sessions: Mutex<Vec<Arc<AtomicBool>>>,
    n: AtomicUsize,
}

impl StreamOpener for Tagged {
    fn open(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let tag: u8 = req
            .param("tag")
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| OpenRefusal::new(400, "bad-request", "tag"))?;
        let flood = req.param("flood") == Some("1");
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        let header = bits_header(&format!("bits/tagged/{n}"), ContentClass::Unrestricted);
        let config = PublisherConfig {
            queue_bytes: 64 * 1024,
            disconnect_after: Duration::from_secs(60),
            max_consumers: 1,
            drain_timeout: Duration::from_millis(200),
            ..PublisherConfig::default()
        };
        let mut p = Publisher::new(header.clone(), config).unwrap();
        let handle = p.handle();
        let stop = Arc::new(AtomicBool::new(false));
        self.sessions.lock().unwrap().push(Arc::clone(&stop));
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            let payload = vec![tag; if flood { 1024 } else { 16 }];
            let mut i = 0u64;
            while !flag.load(Ordering::SeqCst) {
                let _ = p.publish_binary(BinaryRecord {
                    t: Timestamp::from_unix_nanos(1 + i as i64),
                    sample_index: i,
                    flags: RecordFlags::empty(),
                    payload: &payload,
                });
                i += 1;
                if !flood {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(Guard(stop)),
        })
    }
}

#[test]
fn concurrent_streams_of_one_kind_are_isolated_with_per_stream_drop_counters() {
    let opener = Arc::new(Tagged::default());
    let server = serve(
        StreamRegistry::new(),
        OpenerRegistry::new().with("tagged", opener.clone()),
    );
    let addr = server.local_addr();
    // One client, two concurrent streams of the same kind: A reads, B stalls under a flood.
    let a = connect(addr, &format!("open/tagged?token={TOKEN}&tag=1\n"));
    let mut b = connect(addr, &format!("open/tagged?token={TOKEN}&tag=2&flood=1\n"));
    let (a_port, b_port) = (
        a.local_addr().unwrap().port(),
        b.local_addr().unwrap().port(),
    );
    let hb = frame_json(&mut b);
    let mut ra = StreamReader::new(a);
    let ha = ra.read_header().unwrap().clone();
    assert_ne!(ha.stream_id, hb["stream_id"].as_str().unwrap());
    let consumer = |port: u16| {
        server
            .consumers()
            .into_iter()
            .find(|c| c.label.ends_with(&format!(":{port}")))
    };
    wait_for(
        "drops on the stalled stream",
        Duration::from_secs(30),
        || consumer(b_port).is_some_and(|c| c.records_dropped > 0),
    );
    // A keeps receiving only its own records, contiguously, while B drops.
    let mut last = None;
    for _ in 0..300 {
        match ra.next_record().unwrap() {
            Some(Record::Binary(r)) => {
                assert!(r.payload.iter().all(|&x| x == 1), "cross-talk");
                if let Some(l) = last {
                    assert_eq!(r.header.seq, l + 1);
                }
                last = Some(r.header.seq);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(consumer(a_port).unwrap().records_dropped, 0);
    assert!(consumer(b_port).unwrap().records_dropped > 0);
    assert_eq!(server.stats().active, 2);
    drop(ra);
    drop(b);
    wait_for("both sessions to stop", Duration::from_secs(10), || {
        let s = opener.sessions.lock().unwrap();
        s.len() == 2 && s.iter().all(|f| f.load(Ordering::SeqCst))
    });
    wait_for("the connections to close", Duration::from_secs(10), || {
        server.stats().active == 0
    });
}
