//! T-060 (AWARE-036, SIGNAL-062): demodulated outputs streamed to external programs, through the
//! **mock SDR device** (`open_mock_replay`), blind (annotations stripped; only the test holds the
//! truth), over the transports an external program uses.
//!
//! - **FSK bits and symbols over TCP.** The synthetic `fsk_burst_train` sensor loops behind the
//!   mock; the user's classification rule vouches for their own sensor band (configuration, not
//!   truth). A TCP client opens `open/bits` and `open/symbols` with no target (every burst), parses
//!   the contract framing, pairs each burst's status record with its data record, packs the
//!   payload bits the status record locates, and checks them against the private truth payloads.
//!   Dropping the clients removes the taps and closes the connections.
//! - **FM audio over WebSocket, and a stalled TCP client.** The FM fixture loops behind the mock;
//!   the station is found by matching `/api/inventory` rows against the private truth. Listen over
//!   `/ws/open/listen` delivers `ri16_le` audio frames with signal energy. A TCP client that opens
//!   the same Listen and never reads does not stop the chain (frames keep being published), its
//!   drops are counted, it is disconnected, and its session ends.
//!
//! `HK_T060_CAPTURE=<path>` writes the raw bytes of the first bits bursts read over TCP (the Python
//! example test fixture `py/tests/data/t060_fsk_bits.hkstream`).

#[path = "acceptance/common.rs"]
mod common;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_api::{
    ApiState, Server, ServerConfig, StreamRegistry, StreamServer, StreamServerConfig, Token,
};
use hk_core::{MockEnd, Pacing};
use hk_e2e::blind::{matching, strip_truth};
use hk_e2e::{Fixture, SynthRequest, TruthItem, synth_or_skip};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, TrackInventory, open_mock_replay, replay_plan,
};
use hk_stream::record::parse_status_record;
use hk_stream::{
    BinaryRecordHeader, OpenerRegistry, Record, StreamHeader, StreamKind, StreamReader,
};
use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const CENTER_TOL_HZ: f64 = 100e3;
const LIMIT: Duration = Duration::from_secs(900);

/// Starts a looping, lossless run of `meta` behind the mock SDR device with `extra` as the plan's
/// user configuration.
fn start_mock_loop(meta: &Path, dir: &Path, extra: Value) -> PipelineHandle {
    let replay = open_mock_replay(meta, Pacing::Unpaced, MockEnd::Loop).unwrap();
    let info = replay.info;
    let mut plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    plan.extra = extra;
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
    cfg.source_class = replay.class;
    // As the T-047 blind harness sets them for a mock-device run.
    cfg.device_id = replay.device.device_id.clone();
    cfg.device_hw = Some(replay.device.hw.clone());
    cfg.lossless = true;
    Pipeline::start(
        cfg,
        Box::new(replay.source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap()
}

fn tcp_server(openers: OpenerRegistry) -> StreamServer {
    StreamServer::start(
        StreamServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(API_TOKEN).unwrap(),
        ),
        StreamRegistry::new(),
        openers,
    )
    .unwrap()
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "[T-060] timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Copies what it reads into `capture` until `limit` bytes.
struct Tee<R> {
    inner: R,
    capture: Arc<Mutex<Vec<u8>>>,
    limit: usize,
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let mut c = self.capture.lock().unwrap();
        let room = self.limit.saturating_sub(c.len());
        c.extend_from_slice(&buf[..n.min(room)]);
        Ok(n)
    }
}

/// One burst as a client reassembles it: the status record and its data record's payload.
struct Burst {
    status: Value,
    payload: Vec<u8>,
}

/// Opens `target` over TCP and reads bursts until `done` says so (on the calling thread).
fn read_bursts(
    addr: SocketAddr,
    target: &str,
    capture: &Arc<Mutex<Vec<u8>>>,
    mut done: impl FnMut(&[Burst]) -> bool,
) -> (StreamHeader, Vec<Burst>, u64) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(LIMIT)).unwrap();
    writeln!(s, "{target}?token={API_TOKEN}").unwrap();
    let mut r = StreamReader::new(Tee {
        inner: s,
        capture: Arc::clone(capture),
        limit: 16 * 1024,
    });
    let header = r
        .read_header()
        .expect("stream header (not a refusal)")
        .clone();
    let (mut bursts, mut dropped, mut pending) = (Vec::new(), 0u64, None);
    while !done(&bursts) {
        match r.next_record().expect("record") {
            Some(Record::Unknown(frame)) => {
                let (_, status) = parse_status_record(&frame).expect("a status record");
                pending = Some(status);
            }
            Some(Record::Binary(b)) => {
                let status = pending.take().expect("a status record precedes each burst");
                assert_eq!(
                    status["symbols"].as_u64(),
                    Some((b.header.payload_len as usize / element_len(&header)) as u64)
                );
                bursts.push(Burst {
                    status,
                    payload: b.payload,
                });
            }
            Some(Record::Dropped(m)) => dropped += m.count,
            Some(other) => panic!("unexpected record {other:?}"),
            None => panic!("the stream ended early"),
        }
    }
    (header, bursts, dropped)
}

fn element_len(h: &StreamHeader) -> usize {
    if h.datatype.as_deref() == Some("rf32_le") {
        4
    } else {
        1
    }
}

/// The payload bytes a client packs from the bits the status record locates.
fn payload_hex(bits: &[u8], status: &Value) -> Option<String> {
    let at = status["payload_bit"].as_u64()? as usize;
    let n = status["payload_bits"].as_u64()? as usize;
    let lsb = status["bit_order"] == "lsb-first";
    let slice = bits.get(at..at + n)?;
    Some(
        slice
            .chunks_exact(8)
            .map(|c| {
                let byte = c.iter().enumerate().fold(0u8, |a, (i, &b)| {
                    a | ((b & 1) << if lsb { i } else { 7 - i })
                });
                format!("{byte:02x}")
            })
            .collect(),
    )
}

fn bits_of_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .flat_map(|i| {
            let byte = u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
            (0..8).rev().map(move |k| (byte >> k) & 1)
        })
        .collect()
}

#[test]
fn fsk_bursts_stream_as_bits_and_symbols_over_tcp_matching_the_private_truth() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(60)
            .param("snr_db", 20.0)
            .param("duration_s", 2.4)
    );
    let fx = out.fixture(0).unwrap();
    // The truth stays with the test.
    let truth: BTreeSet<String> = fx
        .of_kind("fsk-burst")
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    let n = truth.len();
    assert!(n >= 15, "{n} truth bursts");
    let src = TempDir::new("t060fsk-src");
    let blind = strip_truth(&fx.meta_path, &src.0, "blind", 0.0).unwrap();
    let dir = TempDir::new("t060fsk");
    let handle = start_mock_loop(
        &blind,
        &dir.0,
        json!({ "pipeline": { "classify": [{
            "freq_hz": [433.8e6, 434.1e6],
            "content_class": "unrestricted",
            "by": "test: own synthetic T-060 sensor"
        }] } }),
    );
    let counters = handle.counters();
    let tcp = tcp_server(
        OpenerRegistry::new()
            .with("bits", handle.bits_service())
            .with("symbols", handle.symbols_service()),
    );
    let addr = tcp.local_addr();

    let symbols_reader = std::thread::spawn(move || {
        let capture = Arc::new(Mutex::new(Vec::new()));
        read_bursts(addr, "open/symbols", &capture, |b| b.len() >= 8)
    });
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut matched = BTreeSet::new();
    let (header, bursts, dropped) = read_bursts(addr, "open/bits", &capture, |bursts| {
        if let Some(b) = bursts.last()
            && b.status["crc"] == "valid"
            && let Some(hex) = payload_hex(&b.payload, &b.status)
            && truth.contains(&hex)
        {
            matched.insert(hex);
        }
        matched.len() == n || bursts.len() >= 4 * n
    });
    if let Ok(path) = std::env::var("HK_T060_CAPTURE") {
        std::fs::write(&path, &*capture.lock().unwrap()).unwrap();
        eprintln!("[T-060] wrote the capture to {path}");
    }

    // Header: a bits stream in the contract's shape. T-143 removed the content-gating assertion
    // that sat here (the header class was `unrestricted`): content gating is off by default, so
    // an untargeted tap's header now carries the derived source class verbatim
    // (`metadata-only` for this 433.9 MHz window) and nothing gates on it. The class the user's
    // rule vouches for still reaches each burst's status record; the payload checks below are
    // what the rule used to be needed for.
    assert_eq!(header.kind, StreamKind::Bits);
    assert_eq!(header.datatype.as_deref(), Some("ru8"));
    assert_eq!(
        header.version,
        format!(
            "{}.{}",
            hk_api::stream::STREAM_VERSION_MAJOR,
            hk_api::stream::STREAM_VERSION_MINOR
        )
    );
    assert_eq!(dropped, 0, "a reading client loses nothing");

    // Every CRC-valid burst's payload is a truth payload, and the sync word precedes it.
    let mut valid = 0;
    for b in &bursts {
        assert!(b.payload.iter().all(|&x| x <= 1), "one byte per bit");
        if b.status["crc"] != "valid" {
            continue;
        }
        valid += 1;
        let hex = payload_hex(&b.payload, &b.status).expect("a located payload");
        assert!(
            truth.contains(&hex),
            "[T-060] payload {hex} is not a truth payload"
        );
        let at = b.status["payload_bit"].as_u64().unwrap() as usize;
        assert!(at >= 16);
        assert_eq!(b.payload[at - 16..at], bits_of_hex("2dd4")[..], "sync 2DD4");
    }
    eprintln!(
        "[T-060] bits: {} bursts read, {valid} CRC-valid, {} of {n} truth payloads recovered",
        bursts.len(),
        matched.len()
    );
    assert!(
        matched.len() * 10 >= n * 8,
        "[T-060] recovered {} of {n} truth payloads",
        matched.len()
    );

    // Symbols: soft values whose signs carry the same payloads.
    let (sh, sym, sdropped) = symbols_reader.join().unwrap();
    assert_eq!(sh.kind, StreamKind::Symbols);
    assert_eq!(sh.datatype.as_deref(), Some("rf32_le"));
    assert_eq!(sdropped, 0);
    let mut soft_matched = 0;
    for b in &sym {
        let hard: Vec<u8> = b
            .payload
            .chunks_exact(4)
            .map(|c| u8::from(f32::from_le_bytes(c.try_into().unwrap()) > 0.0))
            .collect();
        if b.status["crc"] == "valid"
            && payload_hex(&hard, &b.status).is_some_and(|h| truth.contains(&h))
        {
            soft_matched += 1;
        }
    }
    eprintln!(
        "[T-060] symbols: {soft_matched} of {} bursts match truth",
        sym.len()
    );
    assert!(soft_matched * 10 >= sym.len() * 7, "[T-060] soft symbols");

    // Clients gone: taps and connections are released while the run continues.
    wait_for("the taps to close", Duration::from_secs(30), || {
        counters.taps.active.load(Ordering::Relaxed) == 0 && tcp.stats().active == 0
    });
    assert_eq!(counters.taps.detached.load(Ordering::Relaxed), 2);
    assert!(counters.taps.records.load(Ordering::Relaxed) > 0);
    handle.stop();
    finish(handle);
}

fn station(fx: &Fixture) -> TruthItem {
    fx.of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .expect("fixture truth has a wfm-broadcast station")
}

/// Polls `/api/inventory` until an emitter matches the private truth; returns its id.
fn found_blind(addr: SocketAddr, truth: &TruthItem) -> String {
    let t0 = Instant::now();
    loop {
        let (_, rows) = api_inventory(addr);
        let hits = matching(
            truth,
            0.0,
            &rows,
            |r| {
                (
                    r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0),
                )
            },
            CENTER_TOL_HZ,
        );
        if let Some(best) = hits
            .iter()
            .max_by_key(|r| r["bandwidth_hz"].as_f64().unwrap_or(0.0) as u64)
        {
            return best["id"].as_str().unwrap().to_owned();
        }
        assert!(
            t0.elapsed() < LIMIT,
            "[T-060] the station was not found blind"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn listen_counter(handle: &PipelineHandle, pick: fn(&hk_pipeline::Counters) -> u64) -> u64 {
    pick(&handle.counters())
}

#[test]
fn fm_audio_streams_over_websocket_and_a_stalled_tcp_client_never_blocks_the_chain() {
    let Some(meta) = real_fixture(FM_FIXTURE) else {
        return;
    };
    let truth = station(&Fixture::load(&meta).unwrap());
    let src = TempDir::new("t060fm-src");
    let blind = strip_truth(&meta, &src.0, "blind", 0.0).unwrap();
    let dir = TempDir::new("t060fm");
    let handle = start_mock_loop(&blind, &dir.0, json!({}));
    let openers = OpenerRegistry::new().with("listen", handle.listen_service());
    let tcp = tcp_server(openers.clone());
    let counters = handle.counters();
    let status_counters = handle.counters();
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(API_TOKEN).unwrap(),
    );
    config.stream_tcp = Some(tcp.local_addr());
    let http = Server::start(
        config,
        ApiState {
            inventory: Some(Arc::new(Mutex::new(repo(&dir.0)))),
            status: Some(Arc::new(move || status_counters.to_json())),
            on_demand: openers,
            ..ApiState::default()
        },
    )
    .unwrap();
    let addr = http.local_addr();
    let (code, body) = api_get(addr, "/api/streams");
    assert_eq!(code, 200);
    let doc: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["on_demand"][0]["name"], "listen");
    assert_eq!(doc["on_demand"][0]["datatype"], "ri16_le");
    assert_eq!(doc["tcp"]["addr"], tcp.local_addr().to_string());

    let emitter = found_blind(addr, &truth);

    // WebSocket: audio frames with signal energy.
    let (mut ws, _) = tungstenite::connect(format!(
        "ws://{addr}/ws/open/listen?token={API_TOKEN}&emitter={emitter}"
    ))
    .expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(LIMIT)).unwrap();
    }
    let header = loop {
        if let Message::Text(t) = ws.read().unwrap() {
            break StreamHeader::from_json_bytes(t.as_bytes())
                .unwrap_or_else(|_| panic!("refused: {t}"));
        }
    };
    assert_eq!(header.kind, StreamKind::Audio);
    assert_eq!(header.datatype.as_deref(), Some("ri16_le"));
    let (mut frames, mut energy, mut samples) = (0, 0.0f64, 0usize);
    while frames < 100 {
        if let Message::Binary(b) = ws.read().unwrap() {
            let h = BinaryRecordHeader::decode(&b).unwrap();
            if h.record_type != 1 {
                continue;
            }
            let pcm = &b[32..];
            assert_eq!(pcm.len(), 1920, "960 i16 samples");
            for c in pcm.chunks_exact(2) {
                let v = f64::from(i16::from_le_bytes([c[0], c[1]])) / 32767.0;
                energy += v * v;
                samples += 1;
            }
            frames += 1;
        }
    }
    let rms = (energy / samples as f64).sqrt();
    eprintln!(
        "[T-060] websocket: {frames} audio frames, rms {rms:.3}, mode {:?}",
        header.audio.as_ref().map(|a| a.mode.clone())
    );
    assert!(rms > 0.01, "[T-060] audio has signal energy");
    let _ = ws.close(None);
    drop(ws);
    wait_for(
        "the WebSocket listener to detach",
        Duration::from_secs(60),
        || counters.listen.active.load(Ordering::Relaxed) == 0,
    );

    // TCP: a client that never reads.
    let mut slow = TcpStream::connect(tcp.local_addr()).unwrap();
    writeln!(slow, "open/listen?token={API_TOKEN}&emitter={emitter}").unwrap();
    wait_for("the TCP listener to attach", LIMIT, || {
        counters.listen.active.load(Ordering::Relaxed) == 1 && tcp.stats().served == 1
    });
    wait_for("drops for the stalled client", LIMIT, || {
        tcp.stats().records_dropped > 0 || tcp.stats().active == 0
    });
    // The chain keeps publishing while its consumer is stalled or gone.
    let before = listen_counter(&handle, |c| c.listen.frames.load(Ordering::Relaxed));
    wait_for("the chain to keep publishing", LIMIT, || {
        listen_counter(&handle, |c| c.listen.frames.load(Ordering::Relaxed)) >= before + 50
            || counters.listen.active.load(Ordering::Relaxed) == 0
    });
    // T-074: the pipeline only folds a closed consumer's drops into `consumer_dropped` when the
    // session itself tears down (after the idle timeout past disconnect), and it clears
    // `listen.active` a few instructions *before* that fold-in. Waiting on `active == 0` as a
    // proxy for "drops are counted" was the flaky assertion at the end of this test: poll the
    // counter we actually assert on, not a proxy that can settle first.
    wait_for(
        "the pipeline to fold the stalled consumer's drops into consumer_dropped",
        LIMIT,
        || counters.listen.consumer_dropped.load(Ordering::Relaxed) > 0,
    );
    // The stalled consumer is disconnected and its session ends.
    wait_for("the stalled client to be released", LIMIT, || {
        tcp.stats().active == 0 && counters.listen.active.load(Ordering::Relaxed) == 0
    });
    let stats = tcp.stats();
    eprintln!(
        "[T-060] stalled TCP client: {stats:?}; listen counters {}",
        counters.listen.to_json()
    );
    assert!(
        stats.records_dropped > 0,
        "[T-060] drops counted: {stats:?}"
    );
    assert!(counters.listen.consumer_dropped.load(Ordering::Relaxed) > 0);
    assert!(
        counters.listen.frames.load(Ordering::Relaxed) > before,
        "[T-060] the chain was not blocked"
    );
    drop(slow);
    handle.stop();
    finish(handle);
}
