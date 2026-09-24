//! **T-865 / ADR-0015 §12.9 stage 0 (LP-1): the Listen conformance freeze.** SIGNAL-062.
//!
//! Pins **today's** Listen behaviour — the legacy `chains/listen.rs` chain behind
//! `/ws/open/listen` — as it is seen *on the wire*, through the mock SDR, **before** any stage of
//! the audio-pipeline migration (§12.9 stages 2–7) touches it. Stage 4 re-runs this file with
//! `HK_LISTEN_PIPELINE=1` (chooser → ephemeral recipe pipeline); both modes must stay green. A
//! failure here is **drift**, not a flaky test: either the change is wrong, or it is a deliberate
//! contract change and this file is updated in the same commit, with the ADR saying why.
//!
//! What is frozen (§12.4's table, §12.9 stage 0's list):
//!
//! 1. **Discovery** — `GET /api/streams` `on_demand[listen]`: `kind`, `datatype`,
//!    `sample_rate_hz`, and `params` exactly `emitter, detection, f_lo, f_hi` (no `mode`, ever).
//! 2. **Header keys** — the header's top-level key set is exactly today's; the `audio` profile's
//!    key set is exactly today's plus only the additive keys ADR-0011 §8.2 names inside it
//!    (`pipeline_id`, `recipe`, `output_id`, `edit_rev` — T-866 serves them there, on a recipe's
//!    `audio` output); `ri16_le` / 48000 Hz / 960-sample frames / **mono** (`channels: 1`, which
//!    LP-10's stereo must keep unless a client opts in, §12.13).
//! 3. **Records** — every binary message is a 32-byte record header plus payload; data records
//!    carry exactly 960 samples (1920 bytes); `sample_index` counts audio samples and `t` advances
//!    exactly `sample_index / 48000` s; `seq` has no gap except behind a drop marker.
//! 4. **Status keys** — every type-3 record carries today's keys (additive per-node metrics are
//!    allowed alongside, §12.4), and `sample_index == 960 × (frames + squelched_frames)`: the
//!    audio clock advances one frame per frame produced, published or squelched.
//! 5. **Squelch** — a closed squelch is a `sample_index` **jump** (a whole number of frames) on
//!    the next published record, flagged `DISCONTINUITY`; no record is published while it is
//!    closed. The test fades the station through the device (the mock's gains down 44 dB, then
//!    back), so the closure is observed in order, not assumed.
//! 6. **Refusals** — the refusal message's keys and each code the opener sends today: `4400`
//!    (unknown parameter such as `mode`, a span over 1 MHz, a malformed id), `4404` (unknown
//!    emitter or detection), `4409` (outside the tuned window), `4503 busy` at the listener cap
//!    (no audio, then the close), `4403` from the legal gate **before** the window check when
//!    content gating is on, and `4422 no-analog-mode` on noise.
//! 7. **Retune** — an in-place retune that still covers the channel **keeps** the stream (the
//!    next record is flagged `DISCONTINUITY`); a retune that leaves the channel, and a re-plumb
//!    (rate change), **end** it: the connection ends, no header is re-sent, and the client must
//!    reconnect (§12.6, docs/20 D5: kept for the whole migration).
//! 8. **Per-consumer** — closing the socket stops the chain (`running` back to 0,
//!    `closed_client`).
//!
//! **Not frozen here:** the audio's *content* (level, SNR) and CPU cost, which stage 3's parity
//! harness compares sample-wise; refinement convergence tolerances (T-070's own tests); emitter
//! targets through the inventory (tests/e2e `acceptance/listen.rs`, which needs a recorded
//! fixture); the TCP opener `open/listen`.
//!
//! **The source.** A synthetic WFM station (1 kHz + 2.9 kHz programme tones, 19 kHz pilot,
//! 75 kHz peak deviation) 200 kHz above a 100 MHz, 1 Msps ci8 SigMF recording, written by the
//! test and served by [`MockSdrDriver`] behind the generic device contract (CLAUDE.md: e2e goes
//! through the SDR device interface), looping, unpaced and lossless — so the chain's gate cursor
//! holds capture and every assertion is about ordering and counts, never wall-clock timing.
//!
//! **Non-vacuity, measured (T-865).** Dropping `gap = true` from the chain's squelched-frame path
//! fails the first test with "sample_index jumped 299520 → 479040 with no DISCONTINUITY flag";
//! setting one more header field (`fft_size`) fails it with "header grew keys [\"fft_size\"]".
//!
//! **Observed and pinned only loosely:** when a retune or a re-plumb ends the stream, today's
//! server resets the connection **without a WebSocket close frame** (tungstenite reports
//! "Connection reset without closing handshake"). The tests require only that the connection
//! ends with no further header, so a later stage may add a close frame without failing here.

mod common;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::TempDir;
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_core::source::mock::UNRECORDED_GAINS;
use hk_core::{Gains, MockEnd, MockOptions, MockSdrControl, MockSdrDriver, Pacing, SourceControl};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_pipeline::class::window_class;
use hk_pipeline::stats::ListenCounters;
use hk_pipeline::{
    ListenSettings, Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory,
    replay_plan,
};
use hk_stream::record::parse_status_record;
use hk_stream::{BinaryRecordHeader, OpenerRegistry, RecordFlags};
use serde_json::Value;
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

const TOKEN: &str = "t865-listen-conformance-token-0123456789abcdef";
/// The recording's (and the device's opening) centre and rate.
const CENTER: f64 = 100.0e6;
const FS: f64 = 1.0e6;
/// The station, relative to [`CENTER`].
const STATION_OFFSET_HZ: f64 = 200e3;
/// The recording's length. Every tone completes whole cycles in it, so the loop is seamless and
/// the station is continuous: wherever the probe lands, it lands on the station.
const RECORDING_S: f64 = 2.0;
/// The gains the station fades to (the recording's own are [`UNRECORDED_GAINS`]): 44 dB down puts
/// the channel far under the noise the squelch measures against.
const FADED: Gains = Gains {
    lna_db: 0.0,
    vga_db: 0.0,
    amp_on: false,
};

/// The frozen wire constants (stream contract §12.2).
const DATATYPE: &str = "ri16_le";
const RATE_HZ: f64 = 48_000.0;
const FRAME: u64 = 960;
const RECORD_HEADER_LEN: usize = 32;
/// Record header plus two frames of `i16`.
const MAX_FRAME_LEN: u64 = 32 + 4 * 960;

/// The header's top-level keys today (a range target: no `emitter_id`).
const HEADER_KEYS: &[&str] = &[
    "schema",
    "version",
    "stream_id",
    "kind",
    "content_class",
    "source",
    "provenance_ref",
    "datatype",
    "sample_rate_hz",
    "center_hz",
    "bandwidth_hz",
    "audio",
    "max_frame_len",
    "record_header_len",
    "t_start",
    "hackriff_version",
];
/// `audio` profile keys a later stage may ADD (ADR-0015 §12.4, ADR-0011 §8.2: "the `audio` object
/// gains `pipeline_id`, `recipe`, `output_id`, `edit_rev`"). Nothing else may appear. (T-865 first
/// allowed them at the header's top level; T-866, which serves them, aligned this with the ADR.)
const AUDIO_ADDITIVE: &[&str] = &["pipeline_id", "recipe", "output_id", "edit_rev"];
/// The `audio` profile's keys today, for a refined WFM station.
const AUDIO_KEYS: &[&str] = &[
    "channels",
    "frame_samples",
    "mode",
    "mode_confidence",
    "mode_rules",
    "params",
    "snr_db",
    "squelch",
    "agc",
    "deemphasis_s",
    "demod",
    "refinement",
];
const SQUELCH_KEYS: &[&str] = &["open_snr_db", "hysteresis_db", "noise_dbfs"];
const AGC_KEYS: &[&str] = &["enabled", "target_dbfs", "max_gain_db"];
const REFINEMENT_KEYS: &[&str] = &[
    "provenance",
    "objective",
    "center_hz",
    "bandwidth_hz",
    "start_center_hz",
    "start_bandwidth_hz",
    "quality",
    "converged",
    "iterations",
    "evaluations",
    "elapsed_s",
    "mode_params",
    "labels",
];
/// Status-record keys always present today, and those present when known (`snr_db` with a noise
/// estimate, `refined_*` once refined).
const STATUS_KEYS: &[&str] = &[
    "level_dbfs",
    "squelch_open",
    "agc_gain_db",
    "frames",
    "squelched_frames",
    "lost_samples",
    "latency_ms",
    "backlog_s",
    "refine_updates",
];
const STATUS_OPTIONAL_KEYS: &[&str] = &["snr_db", "refined_center_hz", "refined_bandwidth_hz"];
/// The refusal message's keys.
const REFUSAL_KEYS: &[&str] = &["type", "status", "code", "reason", "content_class"];

type Ws = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

// ---------------------------------------------------------------------------------------------
// The source: a synthetic WFM station, served by the mock SDR
// ---------------------------------------------------------------------------------------------

/// Writes the station recording (see the module docs) as `wfm.sigmf-meta/-data` in `dir`.
fn wfm_recording(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (RECORDING_S * FS) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let tau = std::f64::consts::TAU;
    let mut phase = 0.0f64;
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let t = i as f64 / FS;
        // Programme (two tones) plus a 19 kHz pilot at 10 % of the peak deviation.
        let m = 0.5 * (tau * 1000.0 * t).sin()
            + 0.3 * (tau * 2900.0 * t).sin()
            + 0.1 * (tau * 19_000.0 * t).sin();
        phase = (phase + tau * (STATION_OFFSET_HZ + 75e3 * m) / FS) % tau;
        let re = (40.0 * phase.cos() + noise()).round().clamp(-128.0, 127.0) as i8;
        let im = (40.0 * phase.sin() + noise()).round().clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join("wfm.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("wfm.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

/// A live, window-classed run (the configuration `hk serve --device mock:…` runs) over the mock
/// SDR, and `hk serve`'s `/ws/open/listen` + `/api/streams` front end on it.
struct Run {
    handle: PipelineHandle,
    server: Server,
    /// The device's own control (the same contract the HackRF source implements).
    device: Arc<MockSdrControl>,
    _dir: TempDir,
}

impl Run {
    fn start(tag: &str, max_listeners: usize) -> Self {
        let dir = TempDir::new(tag);
        let meta = wfm_recording(&dir.0.join("rec"));
        let driver = MockSdrDriver::new(
            &meta,
            MockOptions {
                end: MockEnd::Loop,
                block_len: 16_384,
                pacing: Pacing::Unpaced,
                ..MockOptions::default()
            },
        )
        .unwrap();
        let source = driver.open_mock(&driver.default_request()).unwrap();
        let device = source.mock_control();
        let info = SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: source.start_time(),
        };
        let mut cfg =
            PipelineConfig::new(&dir.0, replay_plan(CENTER, FS, info.start_time)).unwrap();
        cfg.source_class = window_class(CENTER, FS);
        cfg.live_window_class = true;
        cfg.lossless = true;
        cfg.settings.chains = Some(Vec::new());
        let handle = Pipeline::start(
            cfg,
            Box::new(source),
            info,
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        handle.set_listen_settings(ListenSettings {
            max_listeners,
            ..Default::default()
        });
        let counters = handle.counters();
        wait("the first second of samples", || {
            counters.source.samples.load(Ordering::Relaxed) >= FS as u64
        });
        let status_counters = handle.counters();
        let state = ApiState {
            status: Some(Arc::new(move || status_counters.to_json())),
            on_demand: OpenerRegistry::new().with("listen", handle.listen_service()),
            ..ApiState::default()
        };
        let server = Server::start(
            ServerConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                Token::from_config(TOKEN).unwrap(),
            ),
            state,
        )
        .unwrap();
        Self {
            handle,
            server,
            device,
            _dir: dir,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    fn listen(&self) -> Arc<hk_pipeline::stats::Counters> {
        self.handle.counters()
    }

    fn finish(self) {
        let Self {
            handle,
            server,
            _dir,
            ..
        } = self;
        drop(server);
        handle.stop();
        handle.wait().unwrap();
    }
}

/// A bound on waiting for an EVENT (never an assertion about how long something took).
fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------------------------
// The client side: what a browser or a TCP one-liner sees
// ---------------------------------------------------------------------------------------------

fn open(addr: SocketAddr, query: &str) -> Ws {
    let (mut ws, _) =
        tungstenite::connect(format!("ws://{addr}/ws/open/listen?token={TOKEN}&{query}"))
            .expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    }
    ws
}

/// A 150 kHz selection on the station (a user's drag around what they see).
fn station_query() -> String {
    let f = CENTER + STATION_OFFSET_HZ;
    format!("f_lo={}&f_hi={}", f - 75e3, f + 75e3)
}

/// The first message: the header (`Ok`) or the refusal (`Err`), both as the JSON on the wire.
fn first(ws: &mut Ws) -> Result<Value, Value> {
    loop {
        if let Message::Text(t) = ws.read().expect("first message") {
            let v: Value = serde_json::from_str(&t).expect("the first message is JSON");
            return if v["type"] == "refused" {
                Err(v)
            } else {
                Ok(v)
            };
        }
    }
}

/// Opens a listener that must be admitted and returns it with its header.
fn open_admitted(addr: SocketAddr, query: &str) -> (Ws, Value) {
    let mut ws = open(addr, query);
    match first(&mut ws) {
        Ok(h) => (ws, h),
        Err(r) => panic!("the station was refused: {r}"),
    }
}

/// Reads to the end of the connection: `(binary messages, close code)`; a connection that ends
/// without a close frame has no code.
fn drain(ws: &mut Ws) -> (usize, Option<u16>) {
    let (mut bins, mut code) = (0, None);
    loop {
        match ws.read() {
            Ok(Message::Binary(_)) => bins += 1,
            Ok(Message::Close(f)) => code = f.map(|f| u16::from(f.code)),
            Ok(_) => {}
            Err(_) => return (bins, code),
        }
    }
}

fn close(mut ws: Ws) {
    let _ = ws.close(None);
    let _ = drain(&mut ws);
}

/// A refused request: the refusal message, then its close code and the binary messages that came
/// with it (none, for a refusal).
fn refused(addr: SocketAddr, query: &str) -> (Value, Option<u16>, usize) {
    let mut ws = open(addr, query);
    let r = match first(&mut ws) {
        Err(r) => r,
        Ok(h) => panic!("{query:?} was admitted: {h}"),
    };
    let (bins, code) = drain(&mut ws);
    (r, code, bins)
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"))
        .keys()
        .cloned()
        .collect()
}

fn set(k: &[&str]) -> BTreeSet<String> {
    k.iter().map(|s| (*s).to_owned()).collect()
}

fn api_get(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    (status, raw[split + 4..].to_vec())
}

/// One record as the client decodes it.
enum Rec {
    Data {
        seq: u64,
        index: u64,
        t: i64,
        discontinuity: bool,
        payload_len: usize,
    },
    Dropped {
        seq: u64,
    },
    Status {
        seq: u64,
        index: u64,
        v: Value,
    },
}

impl Rec {
    fn seq(&self) -> u64 {
        match self {
            Rec::Data { seq, .. } | Rec::Dropped { seq } | Rec::Status { seq, .. } => *seq,
        }
    }
}

/// The next record, or `None` once the connection has ended.
fn next(ws: &mut Ws) -> Option<Rec> {
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => return Some(rec_of(&b)),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The contract checker: every record of a stream, against the frozen rules
// ---------------------------------------------------------------------------------------------

/// Follows one stream's records and checks each against §§3–5 of the module docs.
#[derive(Default)]
struct Checker {
    last_seq: Option<u64>,
    drops: u64,
    /// The index the next data record has if nothing was squelched (0 at the start).
    next_index: u64,
    first_data: Option<(u64, i64)>,
    data: u64,
    /// Data records whose index jumped (all flagged `DISCONTINUITY`): the squelch's gaps.
    jumps: u64,
    /// Frames those jumps skipped.
    jumped_frames: u64,
    /// Flagged records with no jump (an in-place retune or refinement rebuilt the demodulator).
    flagged_contiguous: u64,
    statuses: u64,
    squelch_closed_seen: bool,
    last_status: Option<Value>,
}

impl Checker {
    fn check(&mut self, r: &Rec) {
        let seq = r.seq();
        if let Some(prev) = self.last_seq {
            assert!(seq > prev, "seq went backwards: {prev} → {seq}");
            if seq != prev + 1 {
                assert!(
                    self.drops > 0 || matches!(r, Rec::Dropped { .. }),
                    "a gap in seq ({prev} → {seq}) with no drop marker: loss must be marked"
                );
            }
        }
        self.last_seq = Some(seq);
        match r {
            Rec::Dropped { .. } => self.drops += 1,
            Rec::Data {
                index,
                t,
                discontinuity,
                payload_len,
                ..
            } => {
                assert_eq!(
                    *payload_len,
                    2 * FRAME as usize,
                    "a data record is one 20 ms frame of {FRAME} i16 LE mono samples"
                );
                assert_eq!(index % FRAME, 0, "sample_index {index} is not on a frame");
                assert!(
                    *index >= self.next_index,
                    "sample_index went backwards: {index} < {}",
                    self.next_index
                );
                // THE SQUELCH RULE: a jump in sample_index is a gap and is flagged.
                if *index != self.next_index && self.drops == 0 {
                    assert!(
                        *discontinuity,
                        "sample_index jumped {} → {index} with no DISCONTINUITY flag",
                        self.next_index
                    );
                    self.jumps += 1;
                    self.jumped_frames += (index - self.next_index) / FRAME;
                } else if *discontinuity && self.data > 0 {
                    self.flagged_contiguous += 1;
                }
                // `t` is the first sample's time; audio time advances at exactly 48 kHz.
                let (i0, t0) = *self.first_data.get_or_insert((*index, *t));
                let want = ((index - i0) as f64 * 1e9 / RATE_HZ).round() as i64;
                assert!(
                    (t - t0 - want).abs() <= 2,
                    "record t does not advance at {RATE_HZ} Hz: index {index}, Δt {} ns, want \
                     {want} ns",
                    t - t0
                );
                self.next_index = index + FRAME;
                self.data += 1;
            }
            Rec::Status { index, v, .. } => {
                let k = keys(v);
                let missing: Vec<_> = set(STATUS_KEYS).difference(&k).cloned().collect();
                assert!(
                    missing.is_empty(),
                    "status record lost keys {missing:?}: {v}"
                );
                for key in STATUS_KEYS.iter().chain(STATUS_OPTIONAL_KEYS) {
                    let Some(x) = v.get(*key) else { continue };
                    let ok = match *key {
                        "squelch_open" => x.is_boolean(),
                        "frames" | "squelched_frames" | "lost_samples" | "refine_updates" => {
                            x.is_u64()
                        }
                        _ => x.is_number(),
                    };
                    assert!(ok, "status `{key}` changed type: {v}");
                }
                let (frames, squelched) = (
                    v["frames"].as_u64().unwrap(),
                    v["squelched_frames"].as_u64().unwrap(),
                );
                assert_eq!(
                    *index,
                    FRAME * (frames + squelched),
                    "the audio clock advances one frame per frame produced, published or \
                     squelched: {v}"
                );
                if self.drops == 0 {
                    assert_eq!(
                        frames, self.data,
                        "status `frames` counts the data records published before it: {v}"
                    );
                }
                assert_eq!(v["lost_samples"], 0, "a lossless run loses nothing: {v}");
                if v["squelch_open"] == false {
                    self.squelch_closed_seen = true;
                }
                self.statuses += 1;
                self.last_status = Some(v.clone());
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The frozen behaviour
// ---------------------------------------------------------------------------------------------

/// §§1–5 and 8: discovery, header, records, status, squelch, detach.
#[test]
fn listen_freeze_discovery_header_records_status_squelch_and_detach() {
    let run = Run::start("t865-wire", 8);
    let addr = run.addr();

    // 1. Discovery.
    let (code, body) = api_get(addr, "/api/streams");
    assert_eq!(code, 200);
    let streams: Value = serde_json::from_slice(&body).unwrap();
    let listen = streams["on_demand"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["name"] == "listen"))
        .unwrap_or_else(|| panic!("no on_demand[listen]: {streams}"));
    assert_eq!(listen["kind"], "audio", "{listen}");
    assert_eq!(listen["datatype"], DATATYPE, "{listen}");
    assert_eq!(listen["sample_rate_hz"], RATE_HZ, "{listen}");
    assert_eq!(
        listen["params"],
        serde_json::json!(["emitter", "detection", "f_lo", "f_hi"]),
        "listen takes a target and nothing else — never a mode: {listen}"
    );
    assert_eq!(listen["ws_path"], "/ws/open/listen", "{listen}");
    assert_eq!(listen["tcp_target"], "open/listen", "{listen}");

    // 2. The header.
    let (mut ws, h) = open_admitted(addr, &station_query());
    eprintln!("[T-865] header: {h}");
    let k = keys(&h);
    let missing: Vec<_> = set(HEADER_KEYS).difference(&k).cloned().collect();
    let unknown: Vec<_> = k.difference(&set(HEADER_KEYS)).cloned().collect();
    assert!(missing.is_empty(), "header lost keys {missing:?}: {h}");
    assert!(unknown.is_empty(), "header grew keys {unknown:?}: {h}");
    assert_eq!(h["schema"], "hackriff.stream");
    assert!(
        h["version"].as_str().is_some_and(|v| v.starts_with("1.")),
        "stream contract major version 1: {h}"
    );
    assert!(h["stream_id"].is_string() && h["source"].is_string(), "{h}");
    assert!(h["provenance_ref"].is_string(), "{h}");
    assert_eq!(h["kind"], "audio");
    assert_eq!(h["content_class"], "unrestricted");
    assert_eq!(h["datatype"], DATATYPE);
    assert_eq!(h["sample_rate_hz"], RATE_HZ);
    assert_eq!(h["record_header_len"], RECORD_HEADER_LEN as u64);
    assert_eq!(h["max_frame_len"], MAX_FRAME_LEN);
    assert!(h["t_start"].is_i64() || h["t_start"].is_u64(), "{h}");
    let station = CENTER + STATION_OFFSET_HZ;
    let center = h["center_hz"].as_f64().unwrap();
    assert!(
        (center - station).abs() < 10e3,
        "center_hz is the demodulated channel: {center} vs the station at {station}"
    );
    let bw = h["bandwidth_hz"].as_f64().unwrap();
    assert!((50e3..=250e3).contains(&bw), "bandwidth_hz {bw}");

    let a = &h["audio"];
    let ak = keys(a);
    let allowed: BTreeSet<String> = set(AUDIO_KEYS)
        .union(&set(AUDIO_ADDITIVE))
        .cloned()
        .collect();
    let lost: Vec<_> = set(AUDIO_KEYS).difference(&ak).cloned().collect();
    let grew: Vec<_> = ak.difference(&allowed).cloned().collect();
    assert!(lost.is_empty(), "audio profile lost keys {lost:?}: {a}");
    assert!(
        grew.is_empty(),
        "audio profile grew keys {grew:?} that are not the named additive ones: {a}"
    );
    assert_eq!(a["channels"], 1, "mono unless a client opts in (§12.13)");
    assert_eq!(a["frame_samples"], FRAME);
    assert_eq!(
        a["mode"], "wfm",
        "the mode is chosen from the signal, not by the user"
    );
    assert!(
        a["mode_confidence"].is_number() && a["mode_rules"].is_string(),
        "{a}"
    );
    assert!(a["demod"].is_string() && a["snr_db"].is_number(), "{a}");
    let pilot = a["params"]["pilot_hz"]
        .as_f64()
        .expect("the probe found the pilot");
    assert!((pilot - 19_000.0).abs() < 20.0, "pilot {pilot}");
    assert_eq!(keys(&a["squelch"]), set(SQUELCH_KEYS), "{a}");
    assert_eq!(keys(&a["agc"]), set(AGC_KEYS), "{a}");
    assert_eq!(
        a["agc"]["enabled"], false,
        "WFM is scaled by deviation, not AGC"
    );
    assert_eq!(a["deemphasis_s"], 75e-6, "WFM de-emphasis");
    let r = &a["refinement"];
    assert_eq!(keys(r), set(REFINEMENT_KEYS), "{r}");
    assert_eq!(r["provenance"], "refined by output analysis");
    assert_eq!(
        r["center_hz"], h["center_hz"],
        "the refined centre is the header's centre"
    );

    // 3–4. Records and status over two loops of the recording, counted in records.
    let mut c = Checker::default();
    let loops = 2 * (RECORDING_S * RATE_HZ) as u64 / FRAME;
    while c.data < loops {
        c.check(&next(&mut ws).expect("the stream ended while streaming"));
    }
    assert_eq!(c.jumps, 0, "the squelch closed on a continuous station");

    // 5. The station fades (through the device): the squelch closes, nothing is published while
    // it is closed, and when the station returns the next record jumps and is flagged. Bounded in
    // records: lossless, so the chain first drains what the ring held before the fade.
    run.device.set_gains(&FADED).unwrap();
    let (mut n, bound) = (0u64, 50 * loops);
    while !c.squelch_closed_seen {
        c.check(&next(&mut ws).expect("the stream ended while faded"));
        n += 1;
        assert!(n < bound, "the squelch never closed on the faded station");
    }
    run.device.set_gains(&UNRECORDED_GAINS).unwrap();
    while c.jumps == 0 {
        c.check(&next(&mut ws).expect("the stream ended after the fade"));
        n += 1;
        assert!(n < bound, "the squelch never reopened");
    }
    // The jump IS the squelched frames: the next status counts exactly the frames it skipped.
    let statuses = c.statuses;
    while c.statuses == statuses {
        c.check(&next(&mut ws).expect("the stream ended after the fade"));
    }
    let last = c.last_status.as_ref().unwrap();
    assert_eq!(
        last["squelched_frames"].as_u64(),
        Some(c.jumped_frames),
        "the sample_index jump is the frames withheld while the squelch was closed: {last}"
    );
    eprintln!(
        "[T-865] {} data, {} statuses, {} squelch jumps, {} drop markers; last status {}",
        c.data,
        c.statuses,
        c.jumps,
        c.drops,
        c.last_status.as_ref().unwrap()
    );
    assert!(c.statuses > 0, "no status records");

    // 8. Per-consumer: closing the socket stops the chain.
    let counters = run.listen();
    let lc: &ListenCounters = &counters.listen;
    assert_eq!(get(&lc.running), 1);
    close(ws);
    wait("the chain to stop on detach", || get(&lc.running) == 0);
    assert_eq!(get(&lc.detached), 1, "{}", lc.to_json());
    assert_eq!(get(&lc.closed_client), 1, "{}", lc.to_json());
    assert_eq!(get(&lc.active), 0, "the slot is freed");
    run.finish();
}

/// §6: every refusal code the opener sends today, and the refusal message's shape.
#[test]
fn listen_freeze_refusal_codes() {
    // A cap of one, so the second listener meets the cap.
    let run = Run::start("t865-refusals", 1);
    let addr = run.addr();
    let check = |query: &str, status: u64, code: &str| -> Value {
        let (r, close_code, bins) = refused(addr, query);
        eprintln!("[T-865] {query} → {r}");
        assert_eq!(keys(&r), set(REFUSAL_KEYS), "refusal message keys: {r}");
        assert_eq!(r["type"], "refused");
        assert_eq!(r["status"], status, "{query}: {r}");
        assert_eq!(r["code"], code, "{query}: {r}");
        assert!(r["reason"].is_string(), "{r}");
        assert_eq!(bins, 0, "{query}: a refusal carries no audio");
        assert_eq!(
            close_code,
            Some(4000 + status as u16),
            "{query}: the close code is 4000 + status"
        );
        r
    };

    // 4400: the mode is estimated, never requested; spans over 1 MHz; malformed targets.
    let r = check(&format!("{}&mode=wfm", station_query()), 400, "bad-request");
    assert!(r["reason"].as_str().unwrap().contains("mode"), "{r}");
    check(
        &format!("f_lo={}&f_hi={}", CENTER - 0.6e6, CENTER + 0.6e6),
        400,
        "bad-request",
    );
    check("emitter=x", 400, "bad-request");
    check("", 400, "bad-request");
    // 4404: well-formed ids nobody knows.
    check(
        &format!("emitter={}", hk_model::EmitterId::new()),
        404,
        "not-found",
    );
    check(
        &format!("detection={}", hk_model::DetectionId::new()),
        404,
        "not-found",
    );
    // 4409: a selection the tuned window (100 MHz ± 0.5 MHz) does not cover.
    check("f_lo=102000000&f_hi=102150000", 409, "outside-window");
    // 4422: noise only — nothing to demodulate.
    check(
        &format!("f_lo={}&f_hi={}", CENTER - 300e3, CENTER - 280e3),
        422,
        "no-analog-mode",
    );

    // 4403: the legal gate runs on the requested extent BEFORE the window check — with content
    // gating on, a paging selection is refused 403 although it is also outside the window; with
    // it off (the default, T-143) the same request is merely outside the window.
    let paging = "f_lo=152230000&f_hi=152250000";
    hk_model::set_content_gating(false);
    check(paging, 409, "outside-window");
    hk_model::set_content_gating(true);
    let r = check(paging, 403, "restricted-class");
    hk_model::set_content_gating(false);
    assert_eq!(r["content_class"], "restricted-paging", "{r}");

    // 4503: past the listener cap — the refusal names the listener limit, sends no audio, closes.
    let (ws, _) = open_admitted(addr, &station_query());
    let r = check(&station_query(), 503, "busy");
    assert!(
        r["reason"].as_str().unwrap().contains("listener limit"),
        "{r}"
    );
    close(ws);

    let counters = run.listen();
    let lc = &counters.listen;
    wait("the admitted chain to stop", || get(&lc.running) == 0);
    assert_eq!(get(&lc.refused_busy), 1, "{}", lc.to_json());
    assert_eq!(get(&lc.refused_class), 1, "{}", lc.to_json());
    run.finish();
}

/// §7: in-place retune keeps the stream; a retune off the channel ends it.
#[test]
fn listen_freeze_in_place_retune_keeps_the_stream_and_leaving_the_channel_ends_it() {
    let run = Run::start("t865-retune", 8);
    let addr = run.addr();
    let counters = run.listen();
    let lc = &counters.listen;
    let controller = run.handle.controller();
    let (mut ws, _) = open_admitted(addr, &station_query());
    let mut c = Checker::default();
    while c.data < 20 {
        c.check(&next(&mut ws).expect("streaming"));
    }

    // In place: same rate and class, and 100.05 MHz ± 0.49 MHz still covers the channel.
    let out = controller
        .retune(CENTER + 50e3, FS)
        .expect("in-place retune");
    assert!(!out.replumbed, "{out:?}");
    // The demodulator is rebuilt on the new window and the next record says so. Bounded in
    // records: lossless, so the chain first drains what the ring held before the retune.
    let before = c.flagged_contiguous;
    let mut after = 0u64;
    while c.flagged_contiguous == before || after < 50 {
        let r = next(&mut ws).expect("an in-place retune must not end the stream");
        c.check(&r);
        if c.flagged_contiguous > before && matches!(r, Rec::Data { .. }) {
            after += 1;
        }
        assert!(
            c.data < 20_000,
            "no DISCONTINUITY-flagged record after the in-place retune in {} records",
            c.data
        );
    }
    assert_eq!(get(&lc.retune_ends), 0, "{}", lc.to_json());
    assert_eq!(get(&lc.running), 1, "{}", lc.to_json());

    // Off the channel: same rate and class, but 99.5 MHz ± 0.49 MHz no longer covers it. The
    // stream ends; nothing is re-sent; the client must reconnect.
    let out = controller
        .retune(CENTER - 500e3, FS)
        .expect("in-place retune");
    assert!(!out.replumbed, "{out:?}");
    let mut texts = 0;
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => c.check(&rec_of(&b)),
            Ok(Message::Text(_)) => texts += 1,
            Ok(Message::Close(f)) => {
                eprintln!("[T-865] retune-ends: close frame {f:?}");
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[T-865] retune-ends: the connection ended: {e}");
                break;
            }
        }
    }
    assert_eq!(
        texts, 0,
        "no second header: the stream ended, it did not restart"
    );
    wait("the chain to end", || get(&lc.running) == 0);
    assert_eq!(get(&lc.retune_ends), 1, "{}", lc.to_json());
    assert_eq!(get(&lc.closed_retune), 1, "{}", lc.to_json());
    // Reconnecting to the old selection is now outside the window.
    let (r, code, _) = refused(addr, &station_query());
    assert_eq!((r["status"].as_u64(), code), (Some(409), Some(4409)), "{r}");
    run.finish();
}

/// §7: a re-plumb (rate change) ends the stream; a new request is served on the new segment.
#[test]
fn listen_freeze_replumb_ends_the_stream_and_the_client_reconnects() {
    let run = Run::start("t865-replumb", 8);
    let addr = run.addr();
    let counters = run.listen();
    let lc = &counters.listen;
    let controller = run.handle.controller();
    let (mut ws, h1) = open_admitted(addr, &station_query());
    let mut c = Checker::default();
    while c.data < 20 {
        c.check(&next(&mut ws).expect("streaming"));
    }

    let out = controller.retune(CENTER, 2.0 * FS).expect("rate change");
    assert!(out.replumbed, "a rate change re-plumbs: {out:?}");
    let mut texts = 0;
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => c.check(&rec_of(&b)),
            Ok(Message::Text(_)) => texts += 1,
            Ok(_) => {}
            Err(e) => {
                eprintln!("[T-865] replumb-ends: the connection ended: {e}");
                break;
            }
        }
    }
    assert_eq!(
        texts, 0,
        "no second header: the stream ended, it did not restart"
    );
    wait("the chain to end", || get(&lc.running) == 0);
    assert_eq!(get(&lc.retune_ends), 1, "{}", lc.to_json());
    assert_eq!(get(&lc.closed_segment), 1, "{}", lc.to_json());

    // The client reconnects: a fresh stream on the new segment, same contract.
    let (ws2, h2) = open_admitted(addr, &station_query());
    assert_ne!(
        h1["stream_id"], h2["stream_id"],
        "a new stream, not a resumed one"
    );
    assert_eq!(h2["datatype"], DATATYPE);
    assert_eq!(h2["audio"]["mode"], "wfm");
    close(ws2);
    run.finish();
}

/// Decodes one binary message as the client does.
fn rec_of(b: &[u8]) -> Rec {
    assert!(
        b.len() >= RECORD_HEADER_LEN,
        "a record shorter than its header"
    );
    let h = BinaryRecordHeader::decode(b).expect("record header");
    match h.record_type {
        1 => Rec::Data {
            seq: h.seq,
            index: h.sample_index,
            t: h.t.as_unix_nanos(),
            discontinuity: h.flags.contains(RecordFlags::DISCONTINUITY),
            payload_len: b.len() - RECORD_HEADER_LEN,
        },
        2 => Rec::Dropped { seq: h.seq },
        3 => {
            let (_, v) = parse_status_record(b).expect("status record");
            Rec::Status {
                seq: h.seq,
                index: h.sample_index,
                v,
            }
        }
        other => panic!("record type {other} on an audio stream"),
    }
}
