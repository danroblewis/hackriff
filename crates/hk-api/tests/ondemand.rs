//! `/ws/open/<name>` (T-043): the on-demand stream transport. A fake opener stands in for the
//! pipeline's listen chain: refusals close with `4000 + status` after a JSON reason and attach
//! nothing; an opened stream is served like any bridged stream (header text, binary records,
//! status records); a disconnect drops the session guard; the token is checked first.

use std::io;
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
            end: hk_stream::SessionEndSlot::default(),
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
    // T-954: a client-initiated close gets a real close frame back, code 1000 — not a bare TCP
    // hang-up, which every browser reports as 1006 ("abnormal closure") whatever the real cause.
    let (_, _, code) = drain(&mut ws);
    assert_eq!(
        code,
        Some(u16::from(CloseCode::Normal)),
        "the server must echo a close frame, not just shut the socket down"
    );
    let t0 = Instant::now();
    while !ok.stopped.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ok.stopped.load(Ordering::SeqCst),
        "disconnect drops the session guard"
    );
}

/// Publishes a few records, then finishes on its own (drops the [`Publisher`]) — the *producer*
/// decides the session is over, never the client. This closes the consumer from that consumer's
/// own writer thread ([`hk_stream::publisher`]'s drain-on-finish), not from anything this test's
/// client does or from `serve`'s own `watch` loop — the race the first review attempt caught
/// (T-954): the subscribe closer used to only shut the raw socket down, racing ahead of `serve`'s
/// close-frame write once `watch` woke on the resulting EOF, so a producer-initiated end still
/// read as `1006` despite the client never having done anything.
struct Finishing;

impl StreamOpener for Finishing {
    fn open(&self, _req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let mut h = StreamHeader::new(
            "listen/finishing",
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
        std::thread::spawn(move || {
            let t0 = Instant::now();
            while p.handle().open_consumers() == 0 && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            for i in 0..3u64 {
                let _ = p.publish_binary(BinaryRecord {
                    t: Timestamp::from_unix_nanos(1),
                    sample_index: 4 * i,
                    flags: RecordFlags::empty(),
                    payload: &[0u8; 8],
                });
            }
            // Dropping `p` here finishes the publisher: the consumer's own writer thread drains
            // it and closes with `CloseReason::PublisherFinished` once empty.
        });
        Ok(OpenedStream {
            header: h,
            handle,
            session: Box::new(()),
            end: hk_stream::SessionEndSlot::default(),
        })
    }
}

#[test]
fn a_producer_that_finishes_on_its_own_still_closes_with_a_real_frame() {
    let server = serve(OpenerRegistry::new().with("listen", Arc::new(Finishing)));
    let mut ws = connect(
        server.local_addr(),
        &format!("/ws/open/listen?token={TOKEN}"),
    )
    .unwrap();
    let Message::Text(_) = ws.read().unwrap() else {
        panic!("header first")
    };
    // Keep reading records; the client never closes or stops reading. The producer alone decides
    // the session is over once it has drained its three records.
    let (_, _, code) = drain(&mut ws);
    assert_eq!(
        code,
        Some(u16::from(CloseCode::Normal)),
        "a producer-initiated end must still close with a real frame, not a bare hang-up"
    );
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
            end: hk_stream::SessionEndSlot::default(),
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

/// `raw_open` reads the raw socket with its own (non-WebSocket-aware) buffer, so any bytes it
/// over-read past the header would leave a fresh [`WebSocket`] wrapper mis-aligned on the frame
/// boundary. This checks the close reason's bytes turn up anywhere in what `s` sends next
/// instead, which needs no frame alignment (T-954).
fn close_reason_seen(mut s: TcpStream, reason: &str) -> bool {
    use std::io::Read;
    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    let needle = reason.as_bytes();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        if buf.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
    }
    buf.windows(needle.len()).any(|w| w == needle)
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
    // T-934: the loop ends on the counted event - more than 20 records received AND at least
    // 1.5 s (almost 4 peer timeouts) attached - not on a 1.5 s wall-clock window whose record
    // count depended on load (9-11 seen at load ~28). The 60 s bound only catches a hang.
    let mut ws = connect(addr, &format!("/ws/open/listen?token={TOKEN}")).unwrap();
    let (t0, mut records) = (Instant::now(), 0);
    while records <= 20 || t0.elapsed() < Duration::from_millis(1500) {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "{records} records in 60 s"
        );
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
    // T-954: reaping a silent peer still sends a real close frame — the session-drop above races
    // ahead of nothing, since the server writes the frame before dropping the session guard.
    assert!(
        close_reason_seen(silent, "no response within the peer timeout"),
        "an unresponsive peer must get a close frame too, not just be left to read 1006"
    );

    // Abrupt: a hang-up without a close frame.
    let gone = raw_open(addr);
    assert!(live_is(1));
    drop(gone);
    assert!(live_is(0), "a hang-up drops the session");
    assert_eq!(opener.opened.load(Ordering::SeqCst), 3);
}

/// Publishes nothing for `quiet`, then floods full-size records until its session drops, so a
/// peer that never reads fills the socket's send buffer and leaves the per-consumer writer thread
/// blocked mid-write — holding the connection's lock — for up to a whole peer timeout (T-954,
/// review attempt 3). Records how its session ended and the instants the transport set that end
/// and dropped the session, read off the server's own calls rather than guessed from outside.
struct Flooding {
    quiet: Duration,
    end: hk_stream::SessionEndSlot,
    dropped: Arc<std::sync::Mutex<Option<Instant>>>,
}

struct FloodGuard(Arc<AtomicBool>, Arc<std::sync::Mutex<Option<Instant>>>);
impl Drop for FloodGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        *self.1.lock().unwrap() = Some(Instant::now());
    }
}

impl StreamOpener for Flooding {
    fn open(&self, _req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let mut h = StreamHeader::new(
            "listen/flooding",
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
        let quiet = self.quiet;
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let payload =
                vec![0u8; AUDIO_MAX_FRAME_LEN as usize - hk_api::stream::BINARY_RECORD_HEADER_LEN];
            let mut i = 0u64;
            while !stopped.load(Ordering::SeqCst) {
                if t0.elapsed() < quiet {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                for _ in 0..16 {
                    let _ = p.publish_binary(BinaryRecord {
                        t: Timestamp::from_unix_nanos(1),
                        sample_index: 4 * i,
                        flags: RecordFlags::empty(),
                        payload: &payload,
                    });
                    i += 1;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Ok(OpenedStream {
            header: h,
            handle,
            session: Box::new(FloodGuard(stop, Arc::clone(&self.dropped))),
            end: self.end.clone(),
        })
    }
}

/// T-954, review attempt 3: a peer that vanished (no FIN, no RST) while records large enough to
/// fill the send buffer were streaming. The writer thread is then blocked inside a socket write,
/// holding the connection's lock, when `watch` declares the peer unresponsive; `serve` must not
/// wait that write out (and then a second, full-timeout close-frame write) before it releases the
/// session — that held the session guard, the producer chain and its budget slot for up to one
/// extra peer timeout.
///
/// The flood starts at 3/4 of the peer timeout, so the stuck write outlives the reap by about that
/// much: before the fix the session outlived `watch`'s verdict by >= 0.75 x the peer timeout
/// (1.5 s here). The bound asserted, 0.4 x the peer timeout (0.8 s), is the fix's own budget —
/// one bounded lock wait plus one bounded close-frame write, 200 ms each — plus 400 ms of margin
/// for a loaded machine. No pings are sent (their interval is longer than the test), so this
/// measures only the writer-lock path.
#[test]
fn a_vanished_peer_is_released_promptly_even_while_a_write_to_it_is_stuck() {
    let peer_timeout = Duration::from_secs(2);
    let opener = Arc::new(Flooding {
        quiet: peer_timeout * 3 / 4,
        end: hk_stream::SessionEndSlot::default(),
        dropped: Arc::default(),
    });
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    config.ondemand_ping_interval = Duration::from_secs(60);
    config.ondemand_peer_timeout = peer_timeout;
    let state = ApiState {
        on_demand: OpenerRegistry::new().with("listen", opener.clone()),
        ..ApiState::default()
    };
    let server = Server::start(config, state).unwrap();

    // Reads up to the header, then never reads or answers again: the socket stays open.
    let _vanished = raw_open(server.local_addr());

    let deadline = Instant::now() + Duration::from_secs(20);
    let ended_at = loop {
        if opener.end.get() != hk_stream::SessionEnd::Unattributed {
            break Instant::now();
        }
        assert!(Instant::now() < deadline, "the peer was never reaped");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(
        opener.end.get(),
        hk_stream::SessionEnd::Unresponsive,
        "the scenario is a vanished peer reaped for silence"
    );
    let dropped_at = loop {
        if let Some(t) = *opener.dropped.lock().unwrap() {
            break t;
        }
        assert!(Instant::now() < deadline, "the session was never dropped");
        std::thread::sleep(Duration::from_millis(1));
    };
    let held = dropped_at.saturating_duration_since(ended_at);
    eprintln!(
        "vanished peer: session dropped {:.0} ms after it was reaped",
        held.as_secs_f64() * 1e3
    );
    assert!(
        held < peer_timeout * 2 / 5,
        "the session outlived the reap by {held:?}: serve waited on a write stuck on a dead peer"
    );
}
