//! T-066 (SIGNAL-062): Listen sessions end with their clients, and admission is configurable.
//!
//! - **No leaks:** 50 listen sessions through the real `/ws/open/listen` WebSocket front end, in
//!   sequence and in parallel batches, each ended one of three ways: a clean close, an abrupt
//!   hang-up without a close frame, or a half-open client that goes silent (no pong) with its
//!   socket left open (short test peer timeout). Afterwards no chain runs, no slot or CPU budget
//!   is held, every chain detached, and `open == sum(closed_*)`. T-633: the reasons are not all
//!   one - a close frame and a hang-up are `closed_client`, a half-open client the server reaped
//!   for silence is `closed_unresponsive`.
//! - **Admission:** the default cap is 8 (the 9th request is refused 503 `busy` with the counts);
//!   the cap changes at runtime; a CPU-budget refusal names the budget; a chain nobody subscribes
//!   to closes as `closed_idle`.
//! - **Re-plumb:** requests while the run moves to a new window are refused 503 `replumbing`,
//!   never 410.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::stats::ListenCounters;
use hk_pipeline::{
    ListenConfig, ListenManager, ListenSettings, Pipeline, PipelineConfig, PipelineHandle,
    SourceInfo, TrackInventory, replay_plan,
};
use hk_stream::{Declared, OpenRefusal, OpenRequest, OpenedStream, OpenerRegistry, StreamOpener};
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

const FM: f64 = 100.8e6;
const FS: f64 = 1.0e6;
const OFFSET_HZ: f64 = 150e3;
const TOKEN: &str = "t066-listen-lifecycle-token-0123456789abcdef";
const SESSIONS: u64 = 50;

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A scripted radio playing a carrier 150 kHz above broadcast FM, lossless.
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
        ctl.finish();
        let (s, fired) = wait_guarded(handle, Duration::from_secs(120));
        assert!(!fired, "the run finished on its own");
        assert!(s.errors.is_empty(), "{:?}", s.errors);
    }
}

/// A 10 kHz selection around the carrier.
fn request() -> OpenRequest {
    let f = FM + OFFSET_HZ;
    OpenRequest {
        params: vec![
            ("f_lo".into(), format!("{}", f - 5e3)),
            ("f_hi".into(), format!("{}", f + 5e3)),
        ],
        peer: "t066".into(),
    }
}

struct Discard;

impl Write for Discard {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Keeps a local consumer on `o` so the chain never idles out.
fn subscribe(o: &OpenedStream) {
    o.handle
        .subscribe("t066", Declared::local(Discard), Box::new(|_| {}))
        .unwrap();
}

/// T-065's mode selector intermittently refuses the test carrier with 422 `no-analog-mode`
/// (tracked as T-073); admission and lifecycle are what these tests check, so that one refusal
/// is retried.
const NO_ANALOG_MODE: &str = "no-analog-mode";
const PROBE_RETRIES: usize = 20;

/// Opens a listen stream (retrying `no-analog-mode` only).
fn open_retry(listen: &dyn StreamOpener) -> OpenedStream {
    for _ in 0..PROBE_RETRIES {
        match listen.open(&request()) {
            Ok(o) => return o,
            Err(e) if e.code == NO_ANALOG_MODE => {}
            Err(e) => panic!("listen refused: {e}"),
        }
    }
    panic!("the test carrier was not recognised in {PROBE_RETRIES} probes");
}

/// The refusal of a request that must not be admitted (retrying `no-analog-mode` only).
fn refused_retry(listen: &dyn StreamOpener) -> OpenRefusal {
    for _ in 0..PROBE_RETRIES {
        match listen.open(&request()) {
            Ok(o) => panic!("admitted: {}", o.header.stream_id),
            Err(e) if e.code == NO_ANALOG_MODE => {}
            Err(e) => return e,
        }
    }
    panic!("the test carrier was not recognised in {PROBE_RETRIES} probes");
}

fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::SeqCst)
}

/// Every chain has ended and released its slot; the close reasons add up.
fn assert_drained(lc: &ListenCounters) {
    wait("every listen chain to end", Duration::from_secs(60), || {
        get(&lc.running) == 0 && get(&lc.active) == 0 && get(&lc.detached) == get(&lc.open)
    });
    let closed = [
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
    .sum::<u64>();
    assert_eq!(get(&lc.open), get(&lc.attached), "{}", lc.to_json());
    assert_eq!(closed, get(&lc.open), "{}", lc.to_json());
    assert_eq!(get(&lc.budget_used_mcores), 0, "{}", lc.to_json());
}

#[derive(Clone, Copy, Debug)]
enum Client {
    Clean,
    Abrupt,
    HalfOpen,
}

fn path() -> String {
    let f = FM + OFFSET_HZ;
    format!(
        "/ws/open/listen?token={TOKEN}&f_lo={}&f_hi={}",
        f - 5e3,
        f + 5e3
    )
}

/// Opens one listen session over WebSocket and ends it as `how` (retrying `no-analog-mode`
/// only). A half-open client's socket is returned, still open and silent (it never answers
/// pings).
fn session(addr: SocketAddr, how: Client) -> Option<TcpStream> {
    for _ in 0..PROBE_RETRIES {
        if let Ok(s) = try_session(addr, how) {
            return s;
        }
    }
    panic!("the test carrier was not recognised in {PROBE_RETRIES} probes");
}

/// One attempt of [`session`]; `Err` is a `no-analog-mode` refusal (nothing was attached).
fn try_session(addr: SocketAddr, how: Client) -> Result<Option<TcpStream>, ()> {
    if let Client::Clean = how {
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
        return if retry { Err(()) } else { Ok(None) };
    }
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
        if n == 0
            && got
                .windows(NO_ANALOG_MODE.len())
                .any(|w| w == NO_ANALOG_MODE.as_bytes())
        {
            return Err(());
        }
        assert!(
            n > 0,
            "closed before the header: {}",
            String::from_utf8_lossy(&got)
        );
        got.extend_from_slice(&buf[..n]);
    }
    assert!(got.starts_with(b"HTTP/1.1 101"));
    Ok(match how {
        // Dropped here: a hang-up without a close frame.
        Client::Abrupt => None,
        _ => Some(s),
    })
}

fn serve(listen: Arc<ListenManager>) -> Server {
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    config.max_connections = 256;
    config.ondemand_ping_interval = Duration::from_millis(100);
    config.ondemand_peer_timeout = Duration::from_secs(1);
    let state = ApiState {
        on_demand: OpenerRegistry::new().with("listen", listen),
        ..ApiState::default()
    };
    Server::start(config, state).unwrap()
}

#[test]
fn fifty_sessions_closed_cleanly_abruptly_or_half_open_all_detach() {
    let run = Run::start("t066-leaks");
    let counters = run.handle.counters();
    let lc = &counters.listen;
    // Headroom so half-open clients waiting out their peer timeout never meet the cap.
    let listen = Arc::new(
        Arc::into_inner(run.handle.listen_service())
            .unwrap()
            .with_config(ListenConfig {
                max_listeners: 64,
                cpu_fraction: 64.0,
                probe_s: 0.2,
                ..ListenConfig::default()
            }),
    );
    let server = serve(Arc::clone(&listen));
    let addr = server.local_addr();
    let kinds = [Client::Clean, Client::Abrupt, Client::HalfOpen];
    let started = Instant::now();
    let mut silent = Vec::new();
    for i in 0..20 {
        silent.extend(session(addr, kinds[i % 3]));
    }
    for batch in 0..3 {
        let threads: Vec<_> = (0..10)
            .map(|i| {
                let how = kinds[(batch + i) % 3];
                std::thread::spawn(move || session(addr, how))
            })
            .collect();
        for t in threads {
            silent.extend(t.join().unwrap());
        }
    }
    assert_eq!(get(&lc.open), SESSIONS, "{}", lc.to_json());
    assert_eq!(get(&lc.refused_busy), 0);
    // The half-open sockets are still open: their sessions must end by the peer timeout.
    assert_drained(lc);
    // T-633: the three ways these sessions end are not one reason. A close frame and a hang-up
    // are the client going away; a half-open client that goes silent is one THIS SERVER reaped,
    // and counting that as `closed_client` told an operator their own client had dropped.
    let half_open = silent.len() as u64;
    assert_eq!(
        get(&lc.closed_unresponsive),
        half_open,
        "every half-open session is counted as the peer the server reaped: {}",
        lc.to_json()
    );
    // A hang-up reaches the server as a FIN or, when the socket is dropped with records still
    // unread, as an RST - and an RST is the connection being torn down, which is not the same
    // statement as a close frame. T-633 keeps them apart rather than resolving the unsure one to
    // the convenient label, so the affirmative ends are asserted as their union.
    assert_eq!(
        get(&lc.closed_client) + get(&lc.closed_transport),
        SESSIONS - half_open,
        "only the clean closes and the hang-ups are the client's own end: {}",
        lc.to_json()
    );
    assert!(get(&lc.closed_client) > 0, "{}", lc.to_json());
    assert_eq!(get(&lc.closed_unattributed), 0, "{}", lc.to_json());
    eprintln!(
        "t066: {} sessions ({} half-open) opened and closed in {:.1} s; listen counters: {}",
        SESSIONS,
        silent.len(),
        started.elapsed().as_secs_f64(),
        counters.to_json()["listen"]
    );
    drop(silent);
    drop(server);
    drop(listen);
    run.finish();
}

#[test]
fn the_cap_defaults_to_8_changes_at_runtime_and_budget_refusals_name_the_budget() {
    let run = Run::start("t066-cap");
    let counters = run.handle.counters();
    let lc = &counters.listen;
    assert_eq!(run.handle.listen_settings(), ListenSettings::default());
    let listen = run.handle.listen_service();
    let budget = counters.to_json()["listen"]["budget"].clone();
    assert_eq!(budget["max_listeners"], 8, "{budget}");
    assert!(budget["cores"].as_f64().unwrap() > 0.0, "{budget}");

    // The default cap: 8 admitted, the 9th refused with the counts.
    let held: Vec<OpenedStream> = (0..8)
        .map(|_| {
            let o = open_retry(&*listen);
            subscribe(&o);
            o
        })
        .collect();
    let e = refused_retry(&*listen);
    assert_eq!((e.status, e.code.as_str()), (503, "busy"), "{e}");
    assert!(e.reason.contains("8 of 8 listeners"), "{e}");
    assert_eq!(counters.to_json()["listen"]["budget"]["listeners"], 8);
    drop(held);
    assert_eq!(
        get(&lc.active),
        0,
        "slots free as soon as the sessions drop"
    );
    assert_drained(lc);

    // A smaller cap, set at runtime (what `--listen-max` does).
    run.handle.set_listen_settings(ListenSettings {
        max_listeners: 1,
        ..ListenSettings::default()
    });
    assert_eq!(counters.to_json()["listen"]["budget"]["max_listeners"], 1);
    let one = open_retry(&*listen);
    subscribe(&one);
    let e = refused_retry(&*listen);
    assert!(e.reason.contains("1 of 1 listeners"), "{e}");
    drop(one);

    // The CPU budget: 1 core per Msps against 1.5 cores, so a second 1 Msps chain does not fit.
    let cores = std::thread::available_parallelism().map_or(1, usize::from) as f64;
    run.handle.set_listen_settings(ListenSettings {
        wfm_cores_per_msps: 1.0,
        narrow_cores_per_msps: 1.0,
        cpu_fraction: 1.5 / cores,
        ..ListenSettings::default()
    });
    let first = open_retry(&*listen);
    subscribe(&first);
    let e = refused_retry(&*listen);
    assert_eq!((e.status, e.code.as_str()), (503, "busy"), "{e}");
    assert!(
        e.reason.contains("CPU budget") && e.reason.contains("of 1.50 cores"),
        "{e}"
    );
    assert_eq!(get(&lc.budget_used_mcores), 1000);
    drop(first);

    // A chain nobody subscribes to closes after the idle timeout.
    run.handle.set_listen_settings(ListenSettings {
        idle_timeout_s: 0.3,
        ..ListenSettings::default()
    });
    let idle = open_retry(&*listen);
    wait(
        "the unsubscribed chain to idle out",
        Duration::from_secs(30),
        || get(&lc.closed_idle) == 1,
    );
    drop(idle);
    assert_drained(lc);
    assert_eq!(get(&lc.refused_busy), 3, "{}", lc.to_json());
    drop(listen);
    run.finish();
}

#[test]
fn requests_during_a_replumb_are_refused_503_replumbing_never_410() {
    let run = Run::start("t066-replumb");
    let listen = run.handle.listen_service();
    let plane = run.handle.controller();
    let done = Arc::new(AtomicBool::new(false));
    let retune = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let outcome = plane.retune(FM, 2.0 * FS);
            done.store(true, Ordering::SeqCst);
            outcome
        })
    };
    let mut refusals = Vec::new();
    while !done.load(Ordering::SeqCst) {
        match listen.open(&request()) {
            Ok(o) => drop(o),
            Err(e) => refusals.push((e.status, e.code)),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let outcome = retune.join().unwrap().expect("a rate change re-plumbs");
    assert!(outcome.replumbed);
    let mut summary = std::collections::BTreeMap::<(u16, &str), usize>::new();
    for (s, c) in &refusals {
        *summary.entry((*s, c.as_str())).or_default() += 1;
    }
    eprintln!("t066: refusals during the re-plumb: {summary:?}");
    assert!(
        !refusals.iter().any(|(s, _)| *s == 410),
        "a re-plumb is not the end of the source: {refusals:?}"
    );
    assert!(
        refusals
            .iter()
            .filter(|(s, _)| *s == 503)
            .all(|(_, c)| c == "replumbing"),
        "{refusals:?}"
    );
    drop(open_retry(&*listen));
    drop(listen);
    run.finish();
}
