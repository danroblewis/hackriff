//! `/ws/open/<name>` (T-043): the on-demand stream transport. A fake opener stands in for the
//! pipeline's listen chain: refusals close with `4000 + status` after a JSON reason and attach
//! nothing; an opened stream is served like any bridged stream (header text, binary records,
//! status records); a disconnect drops the session guard; the token is checked first.

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hk_api::stream::audio::{AUDIO_DATATYPE, AUDIO_MAX_FRAME_LEN, AUDIO_SAMPLE_RATE_HZ, AudioInfo};
use hk_api::stream::record::parse_status_record;
use hk_api::stream::{
    BinaryRecord, BinaryRecordHeader, OpenRefusal, OpenRequest, OpenedStream, OpenerRegistry,
    Publisher, PublisherConfig, RecordFlags, StreamHeader, StreamKind, StreamOpener,
};
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::{ContentClass, Timestamp};
use serde_json::{Value, json};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const TOKEN: &str = "t043-ondemand-token-0123456789abcdef";
type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// Opens an audio stream of `class` producing three records, unless `refuse` says otherwise.
struct Fake {
    class: ContentClass,
    refuse: Option<OpenRefusal>,
    opened: AtomicUsize,
    stopped: Arc<AtomicBool>,
}

struct Guard(Arc<AtomicBool>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl StreamOpener for Fake {
    fn open(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        assert_eq!(
            req.param("token"),
            None,
            "the token never reaches an opener"
        );
        if let Some(r) = &self.refuse {
            return Err(r.clone());
        }
        self.opened.fetch_add(1, Ordering::SeqCst);
        let mut h = StreamHeader::new("listen/test", StreamKind::Audio, self.class, "test");
        h.datatype = Some(AUDIO_DATATYPE.into());
        h.sample_rate_hz = Some(AUDIO_SAMPLE_RATE_HZ);
        h.max_frame_len = AUDIO_MAX_FRAME_LEN;
        h.audio = Some(AudioInfo {
            mode: "wfm".into(),
            channels: 1,
            frame_samples: 4,
            ..AudioInfo::default()
        });
        let mut p = Publisher::new(h.clone(), PublisherConfig::default()).unwrap();
        let handle = p.handle();
        let stopped = Arc::clone(&self.stopped);
        std::thread::spawn(move || {
            // Wait for the subscriber, publish, then hold until the session is dropped.
            let t0 = Instant::now();
            while p.handle().open_consumers() == 0 && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            for i in 0..3u64 {
                let payload = [1u8, 0, 2, 0, 3, 0, 4, 0];
                p.publish_binary(BinaryRecord {
                    t: Timestamp::from_unix_nanos(1),
                    sample_index: 4 * i,
                    flags: RecordFlags::empty(),
                    payload: &payload,
                })
                .ok();
            }
            p.publish_status(
                Timestamp::from_unix_nanos(1),
                12,
                &json!({"level_dbfs": -20.5, "squelch_open": true}),
            )
            .unwrap();
            assert!(
                p.publish_status(
                    Timestamp::from_unix_nanos(1),
                    12,
                    &json!({"text": "free text here"})
                )
                .is_err(),
                "status records carry no free text"
            );
            while !stopped.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Ok(OpenedStream {
            header: h,
            handle,
            session: Box::new(Guard(Arc::clone(&self.stopped))),
        })
    }
}

fn fake(class: ContentClass, refuse: Option<OpenRefusal>) -> Arc<Fake> {
    Arc::new(Fake {
        class,
        refuse,
        opened: AtomicUsize::new(0),
        stopped: Arc::new(AtomicBool::new(false)),
    })
}

fn serve(openers: OpenerRegistry) -> Server {
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        on_demand: openers,
        ..ApiState::default()
    };
    Server::start(config, state).unwrap()
}

fn connect(addr: SocketAddr, path: &str) -> Result<Ws, tungstenite::Error> {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}{path}"))?;
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    }
    Ok(ws)
}

/// Reads until close; returns the text messages, binary messages and the close code.
fn drain(ws: &mut Ws) -> (Vec<String>, Vec<Vec<u8>>, Option<u16>) {
    let (mut texts, mut bins, mut code) = (Vec::new(), Vec::new(), None);
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => texts.push(t.to_string()),
            Ok(Message::Binary(b)) => bins.push(b.to_vec()),
            Ok(Message::Close(f)) => code = f.map(|f| u16::from(f.code)),
            Ok(_) => {}
            Err(_) => return (texts, bins, code),
        }
    }
}

#[test]
fn refusals_close_with_a_reason_and_attach_nothing() {
    let gated = fake(
        ContentClass::RestrictedPaging,
        Some(OpenRefusal::gated(
            ContentClass::RestrictedPaging,
            "restricted-paging band: audio is never streamed",
        )),
    );
    let server = serve(OpenerRegistry::new().with("listen", gated.clone()));
    let addr = server.local_addr();

    let mut ws = connect(
        addr,
        &format!("/ws/open/listen?token={TOKEN}&f_lo=930.4e6&f_hi=930.6e6"),
    )
    .expect("refusals still upgrade so browsers can read the reason");
    let (texts, bins, code) = drain(&mut ws);
    assert!(bins.is_empty(), "no audio for a refused request");
    assert_eq!(code, Some(4403));
    let v: Value = serde_json::from_str(&texts[0]).unwrap();
    assert_eq!(v["type"], "refused");
    assert_eq!(v["status"], 403);
    assert_eq!(v["content_class"], "restricted-paging");
    assert_eq!(gated.opened.load(Ordering::SeqCst), 0);

    // No token: 401 before any opener runs; unknown opener: 404.
    match connect(addr, "/ws/open/listen?f_lo=1&f_hi=2") {
        Err(tungstenite::Error::Http(r)) => assert_eq!(r.status().as_u16(), 401),
        other => panic!("expected 401, got {:?}", other.map(|_| ())),
    }
    match connect(addr, &format!("/ws/open/nope?token={TOKEN}")) {
        Err(tungstenite::Error::Http(r)) => assert_eq!(r.status().as_u16(), 404),
        other => panic!("expected 404, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn opened_streams_are_bridged_and_stop_on_disconnect() {
    let ok = fake(ContentClass::Unrestricted, None);
    let server = serve(OpenerRegistry::new().with("listen", ok.clone()));
    let mut ws = connect(
        server.local_addr(),
        &format!("/ws/open/listen?token={TOKEN}&emitter=x"),
    )
    .unwrap();
    let Message::Text(h) = ws.read().unwrap() else {
        panic!("header first")
    };
    let header = StreamHeader::from_json_bytes(h.as_bytes()).unwrap();
    assert_eq!(header.kind, StreamKind::Audio);
    assert_eq!(
        header.version,
        format!(
            "{}.{}",
            hk_api::stream::STREAM_VERSION_MAJOR,
            hk_api::stream::STREAM_VERSION_MINOR
        )
    );
    assert_eq!(header.audio.unwrap().mode, "wfm");
    let mut seqs = Vec::new();
    let mut status = None;
    while status.is_none() {
        let Message::Binary(b) = ws.read().unwrap() else {
            continue;
        };
        let rh = BinaryRecordHeader::decode(&b).unwrap();
        seqs.push(rh.seq);
        if let Some((_, v)) = parse_status_record(&b) {
            status = Some(v);
        } else {
            assert_eq!(rh.record_type, 1);
            assert_eq!(b.len(), 32 + 8);
        }
    }
    assert_eq!(
        seqs,
        vec![0, 1, 2, 3],
        "records and status share the sequence"
    );
    assert_eq!(status.unwrap()["level_dbfs"], -20.5);
    assert!(!ok.stopped.load(Ordering::SeqCst));
    ws.close(None).unwrap();
    let _ = drain(&mut ws);
    let t0 = Instant::now();
    while !ok.stopped.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ok.stopped.load(Ordering::SeqCst),
        "disconnect drops the session guard"
    );
    let _ = CloseCode::Normal;
}

/// Publishes a small record every 20 ms until its session drops; counts live sessions (T-066).
#[derive(Default)]
struct Ticking {
    opened: AtomicUsize,
    live: Arc<AtomicUsize>,
}

struct LiveGuard(Arc<AtomicUsize>, Arc<AtomicBool>);
impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.1.store(true, Ordering::SeqCst);
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl StreamOpener for Ticking {
    fn open(&self, _req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        let mut h = StreamHeader::new(
            "listen/ticking",
            StreamKind::Audio,
            ContentClass::Unrestricted,
            "test",
        );
        h.datatype = Some(AUDIO_DATATYPE.into());
        h.sample_rate_hz = Some(AUDIO_SAMPLE_RATE_HZ);
        h.max_frame_len = AUDIO_MAX_FRAME_LEN;
        h.audio = Some(AudioInfo {
            mode: "nbfm".into(),
            channels: 1,
            frame_samples: 4,
            ..AudioInfo::default()
        });
        let mut p = Publisher::new(h.clone(), PublisherConfig::default()).unwrap();
        let handle = p.handle();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stopped.load(Ordering::SeqCst) {
                let _ = p.publish_binary(BinaryRecord {
                    t: Timestamp::from_unix_nanos(1),
                    sample_index: 4 * i,
                    flags: RecordFlags::empty(),
                    payload: &[0u8; 8],
                });
                i += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(OpenedStream {
            header: h,
            handle,
            session: Box::new(LiveGuard(Arc::clone(&self.live), stop)),
        })
    }
}

/// Upgrades by hand and reads up to the header; the client then never reads or answers again.
fn raw_open(addr: SocketAddr) -> TcpStream {
    use std::io::{Read, Write};
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "GET /ws/open/listen?token={TOKEN} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
    .unwrap();
    let (mut got, mut buf) = (Vec::new(), [0u8; 1024]);
    while !got.windows(7).any(|w| w == b"ri16_le") {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed early: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
    }
    s
}

#[test]
fn silent_peers_are_dropped_after_the_peer_timeout_and_answering_peers_stay() {
    let opener = Arc::new(Ticking::default());
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    config.ondemand_ping_interval = Duration::from_millis(50);
    config.ondemand_peer_timeout = Duration::from_millis(400);
    let state = ApiState {
        on_demand: OpenerRegistry::new().with("listen", opener.clone()),
        ..ApiState::default()
    };
    let server = Server::start(config, state).unwrap();
    let addr = server.local_addr();
    let live_is = |n: usize| {
        let t0 = Instant::now();
        while opener.live.load(Ordering::SeqCst) != n {
            if t0.elapsed() > Duration::from_secs(5) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    };

    // A client that keeps reading answers the pings: still attached well past the peer timeout.
    let mut ws = connect(addr, &format!("/ws/open/listen?token={TOKEN}")).unwrap();
    let (t0, mut records) = (Instant::now(), 0);
    while t0.elapsed() < Duration::from_millis(1500) {
        match ws.read() {
            Ok(Message::Binary(_)) => records += 1,
            Ok(_) => {}
            Err(e) => panic!("an answering client was dropped: {e}"),
        }
    }
    assert!(records > 20, "{records} records");
    assert_eq!(opener.live.load(Ordering::SeqCst), 1);
    ws.close(None).unwrap();
    let _ = drain(&mut ws);
    assert!(live_is(0), "a clean close drops the session");

    // Half-open: the socket stays open but nothing comes back, not even a pong.
    let silent = raw_open(addr);
    assert!(live_is(1));
    let t = Instant::now();
    assert!(live_is(0), "a silent peer's session is dropped");
    eprintln!(
        "silent peer released after {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    // Abrupt: a hang-up without a close frame.
    let gone = raw_open(addr);
    assert!(live_is(1));
    drop(gone);
    assert!(live_is(0), "a hang-up drops the session");
    drop(silent);
    assert_eq!(opener.opened.load(Ordering::SeqCst), 3);
}
