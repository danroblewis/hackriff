//! T-633: a Listen chain's end carries the reason it actually had.
//!
//! **The defect.** `/ws/open/listen`'s session guard is a bare `drop`, so every end that arrived
//! through it was counted `closed_client` — "the client went away: WebSocket close, reset,
//! unresponsive peer, Stop". Three of those four are not the client going away. `closed_client`
//! is the counter an operator reads to blame their own client, and under a **starved source**
//! that is exactly what it did: a chain the *server itself* reaped for silence was reported as
//! the user's socket hanging up.
//!
//! **The asymmetry T-603 measured** (an emitter-selected listener on the truth station surviving
//! while a range-selected one on the same station was ended `closed_client` within milliseconds,
//! eight times in a row) is reproduced here without any of its incidentals — no 28-way test run,
//! no inventory, no truth list. The only condition that matters is **starvation plus which socket
//! the client is reading**: `/ws/open/listen` pings every `ondemand_ping_interval` and reaps a
//! peer unheard for `ondemand_peer_timeout`, and a WebSocket client answers a ping only from
//! inside a read. A fed chain gives its client records to read, so the client reads, so it pongs.
//! A **starved** chain publishes nothing, so a client that is waiting on some *other* socket (the
//! emitter one, in T-603's test) never reads this one, never pongs, and is reaped — while the
//! socket it *is* reading survives. Same station, same instant, opposite outcomes, and the
//! difference is the reading, not the target.
//!
//! So the end is **not** the client going away. The tests below assert, by counts and ordering,
//! that it is no longer counted as if it were.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::stats::ListenCounters;
use hk_pipeline::{
    ListenConfig, ListenManager, Pipeline, PipelineConfig, PipelineHandle, SourceInfo,
    TrackInventory, replay_plan,
};
use hk_stream::OpenerRegistry;
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

const FM: f64 = 100.8e6;
const FS: f64 = 1.0e6;
const OFFSET_HZ: f64 = 150e3;
const TOKEN: &str = "t633-listen-end-reason-token-0123456789abcdef";
/// Retries of the mode probe only (T-073: the selector intermittently refuses the test carrier).
const PROBE_RETRIES: usize = 20;
const NO_ANALOG_MODE: &str = "no-analog-mode";

fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::SeqCst)
}

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A scripted radio playing a carrier 150 kHz above broadcast FM, pausable (so "starved" is a
/// fact about the script, not about the clock).
struct Run {
    handle: PipelineHandle,
    ctl: Arc<radio::RadioControl>,
    _dir: TempDir,
}

impl Run {
    fn start(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let (radio, ctl) = radio::Radio::new(FM, FS, 16_384, radio::tone(|_| OFFSET_HZ));
        let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
        let mut cfg = PipelineConfig::new(&dir.0, replay_plan(FM, FS, t0)).unwrap();
        cfg.source_class = window_class(FM, FS);
        cfg.live_window_class = true;
        cfg.lossless = true;
        cfg.settings.chains = Some(Vec::new());
        let handle = Pipeline::start(
            cfg,
            Box::new(radio),
            SourceInfo {
                sample_rate_hz: FS,
                center_hz: FM,
                start_time: t0,
            },
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        let counters = handle.counters();
        wait(
            "the first second of samples",
            Duration::from_secs(120),
            || counters.source.samples.load(Ordering::Relaxed) >= FS as u64,
        );
        Self {
            handle,
            ctl,
            _dir: dir,
        }
    }

    fn finish(self) {
        let Self { handle, ctl, _dir } = self;
        ctl.run_free();
        ctl.finish();
        let (s, fired) = wait_guarded(handle, Duration::from_secs(120));
        assert!(!fired, "the run finished on its own");
        assert!(s.errors.is_empty(), "{:?}", s.errors);
    }
}

fn path() -> String {
    let f = FM + OFFSET_HZ;
    format!(
        "/ws/open/listen?token={TOKEN}&f_lo={}&f_hi={}",
        f - 5e3,
        f + 5e3
    )
}

fn serve(listen: Arc<ListenManager>, peer_timeout: Duration) -> Server {
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    config.max_connections = 64;
    config.ondemand_ping_interval = Duration::from_millis(100);
    config.ondemand_peer_timeout = peer_timeout;
    let state = ApiState {
        on_demand: OpenerRegistry::new().with("listen", listen),
        ..ApiState::default()
    };
    Server::start(config, state).unwrap()
}

fn listen_service(run: &Run) -> Arc<ListenManager> {
    Arc::new(
        Arc::into_inner(run.handle.listen_service())
            .unwrap()
            .with_config(ListenConfig {
                max_listeners: 16,
                cpu_fraction: 64.0,
                probe_s: 0.2,
                // The chains under test are deliberately starved, so neither of the *chain's own*
                // timeouts may fire: the question is what the transport's end is counted as.
                squelch_timeout: None,
                idle_timeout: Duration::from_secs(600),
                ..ListenConfig::default()
            }),
    )
}

/// A raw socket that has received its header and will never be read again: the client is busy on
/// another socket. It never pongs, so the server reaps it — nobody hung up.
fn silent_socket(addr: SocketAddr) -> TcpStream {
    for _ in 0..PROBE_RETRIES {
        if let Ok(s) = try_silent_socket(addr) {
            return s;
        }
    }
    panic!("the test carrier was not recognised in {PROBE_RETRIES} probes");
}

fn try_silent_socket(addr: SocketAddr) -> Result<TcpStream, ()> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    write!(
        s,
        "GET {} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        path()
    )
    .unwrap();
    let (mut got, mut buf) = (Vec::new(), [0u8; 4096]);
    while !got.windows(7).any(|w| w == b"ri16_le") {
        let n = s.read(&mut buf).unwrap();
        if n == 0 {
            return Err(());
        }
        got.extend_from_slice(&buf[..n]);
        if got
            .windows(NO_ANALOG_MODE.len())
            .any(|w| w == NO_ANALOG_MODE.as_bytes())
        {
            return Err(());
        }
    }
    assert!(got.starts_with(b"HTTP/1.1 101"));
    Ok(s)
}

/// Opens a session, reads its header and closes it with a close frame: the client really did go
/// away, and this is the one end `closed_client` is for.
fn clean_session(addr: SocketAddr) {
    for _ in 0..PROBE_RETRIES {
        let (mut ws, _) = tungstenite::connect(format!("ws://{addr}{}", path())).expect("upgrade");
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
        }
        let retry = match ws.read().expect("first message") {
            Message::Text(t) if t.as_str().contains(NO_ANALOG_MODE) => true,
            Message::Text(t) => {
                assert!(t.as_str().contains("ri16_le"), "refused: {t}");
                false
            }
            other => panic!("expected the header, got {other:?}"),
        };
        let _ = ws.close(None);
        while ws.read().is_ok() {}
        if !retry {
            return;
        }
    }
    panic!("the test carrier was not recognised in {PROBE_RETRIES} probes");
}

/// Every end reason, so "the closes add up" is checked against the whole set and not a subset
/// that happens to contain the one under test.
fn closed_total(lc: &ListenCounters) -> u64 {
    [
        &lc.closed_client,
        &lc.closed_unresponsive,
        &lc.closed_transport,
        &lc.closed_unattributed,
        &lc.closed_idle,
        &lc.closed_squelch,
        &lc.closed_retune,
        &lc.closed_segment,
        &lc.closed_source,
        &lc.closed_error,
    ]
    .iter()
    .map(|a| get(a))
    .sum()
}

/// **The T-633 case.** A peer the server reaps for silence is not the client going away.
///
/// Asserted by counts over a stated number of opens, never by elapsed time: `SILENT` sockets that
/// are opened and then never read must end up counted `closed_unresponsive` and **zero** of them
/// `closed_client`; `CLEAN` sockets that send a close frame must then move `closed_client` by
/// exactly `CLEAN` and leave `closed_unresponsive` where it was. Before the fix every one of the
/// `SILENT + CLEAN` ends was `closed_client`, so the first assertion fails 6 to 0.
#[test]
fn a_peer_the_server_reaped_is_not_counted_as_the_client_going_away() {
    const SILENT: u64 = 6;
    const CLEAN: u64 = 3;
    let run = Run::start("t633-reaped");
    let counters = run.handle.counters();
    let lc = &counters.listen;
    let listen = listen_service(&run);
    let server = serve(Arc::clone(&listen), Duration::from_secs(1));
    let addr = server.local_addr();

    let silent: Vec<TcpStream> = (0..SILENT).map(|_| silent_socket(addr)).collect();
    assert_eq!(get(&lc.open), SILENT, "{}", lc.to_json());
    // The sockets are still open and still silent: only the server's own reap can end these.
    wait(
        "the silent peers to be reaped",
        Duration::from_secs(60),
        || get(&lc.detached) == SILENT,
    );
    assert_eq!(
        get(&lc.closed_unresponsive),
        SILENT,
        "a peer THIS SERVER reaped for silence is counted as unresponsive: {}",
        lc.to_json()
    );
    assert_eq!(
        get(&lc.closed_client),
        0,
        "not one of {SILENT} reaped peers said it was going away, so `closed_client` - the \
         counter an operator reads to blame their own client - must not have moved: {}",
        lc.to_json()
    );

    // Ordering: the affirmative ends that follow are the ones `closed_client` is for, and they
    // move that counter and no other.
    for _ in 0..CLEAN {
        clean_session(addr);
    }
    wait("the clean sessions to end", Duration::from_secs(60), || {
        get(&lc.detached) == SILENT + CLEAN
    });
    assert_eq!(get(&lc.closed_client), CLEAN, "{}", lc.to_json());
    assert_eq!(get(&lc.closed_unresponsive), SILENT, "{}", lc.to_json());
    assert_eq!(get(&lc.closed_unattributed), 0, "{}", lc.to_json());
    assert_eq!(closed_total(lc), SILENT + CLEAN, "{}", lc.to_json());
    assert_eq!(get(&lc.open), get(&lc.attached), "{}", lc.to_json());

    drop(silent);
    drop(server);
    drop(listen);
    run.finish();
}

/// **The T-603 asymmetry, reproduced from starvation alone.** Two chains on the same carrier at
/// the same instant, under one starved source: the socket the client is reading survives, the
/// socket it is not reading is reaped — and the reap is not counted as the client going away.
///
/// This is the whole of what T-603 saw. The emitter-selected listener in that test survived
/// because the test read it between requests (T-603's own fix); the range-selected one was never
/// read, and starvation is what stretched the step past the peer timeout. Nothing about the
/// *target* differs — as here, where both sockets ask for the identical extent.
#[test]
fn a_starved_source_reaps_only_the_socket_nobody_is_reading_and_does_not_blame_the_client() {
    let run = Run::start("t633-starved");
    let counters = run.handle.counters();
    let lc = &counters.listen;
    let listen = listen_service(&run);
    let server = serve(Arc::clone(&listen), Duration::from_secs(1));
    let addr = server.local_addr();

    // Both chains are opened while the source still runs (the probe reads the live edge), on the
    // same extent.
    let (mut read_ws, _) = tungstenite::connect(format!("ws://{addr}{}", path())).expect("upgrade");
    if let MaybeTlsStream::Plain(s) = read_ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
    }
    match read_ws.read().expect("first message") {
        Message::Text(t) => assert!(t.as_str().contains("ri16_le"), "refused: {t}"),
        other => panic!("expected the header, got {other:?}"),
    }
    let unread = silent_socket(addr);
    assert_eq!(get(&lc.attached), 2, "{}", lc.to_json());

    // STARVE: the radio delivers nothing further, so neither chain publishes audio and neither
    // client has anything to read. This is the only condition the defect needs.
    run.ctl.hold_at(run.ctl.emitted());

    // The reading client keeps answering pings from inside its read; the other cannot.
    let read_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (alive, stop) = (Arc::clone(&read_alive), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match read_ws.read() {
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => {
                        alive.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
        })
    };

    wait(
        "the unread peer to be reaped",
        Duration::from_secs(60),
        || get(&lc.detached) == 1,
    );
    assert_eq!(
        get(&lc.closed_unresponsive),
        1,
        "the unread socket was reaped by the server, not closed by its client: {}",
        lc.to_json()
    );
    assert_eq!(
        get(&lc.closed_client),
        0,
        "nothing about the unread socket was the client going away: {}",
        lc.to_json()
    );
    assert_eq!(
        get(&lc.running),
        1,
        "the socket the client IS reading survives the same starvation: {}",
        lc.to_json()
    );
    assert!(
        read_alive.load(Ordering::SeqCst),
        "the read socket must still be connected: {}",
        lc.to_json()
    );
    assert_eq!(get(&lc.closed_squelch), 0, "{}", lc.to_json());
    assert_eq!(get(&lc.closed_idle), 0, "{}", lc.to_json());

    stop.store(true, Ordering::SeqCst);
    reader.join().unwrap();
    drop(unread);
    drop(server);
    drop(listen);
    run.finish();
}
