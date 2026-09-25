//! **T-869 / ADR-0015 §12.9 stage 4 (LP-5): the chooser, the flagged opener switch, and
//! attach-don't-duplicate.** SIGNAL-062, through the mock SDR device interface.
//!
//! With `HK_LISTEN_PIPELINE` on, `/ws/open/listen` runs the chooser
//! ([`hk_pipeline::audio::choose`]): it probes the live edge exactly as before, ranks the audio
//! recipes against **what it measured** and, when one fits, serves the audio from an **ephemeral
//! recipe pipeline** instead of the legacy chain. What this file pins:
//!
//! 1. **The switch is a switch.** Off (the default), the opener is today's chain — no pipeline
//!    exists, the stream is `listen/<n>` and `GET /api/pipelines` is empty. On, the same request
//!    is served by a pipeline: the stream is `audio/<pipeline>/<output>` and the header carries
//!    §12.4's additive keys (`pipeline_id`, `recipe`, `output_id`, `edit_rev`).
//! 2. **The wire contract holds on the pipeline path.** `ri16_le` at 48 kS/s, 960-sample data
//!    records, a `sample_index` that never goes backwards and advances one frame per published
//!    record unless the step is **accounted for** — flagged `DISCONTINUITY` (a live-edge seek,
//!    or a broken input) or behind a drop marker (this consumer was too slow) — with `t` and the
//!    index agreeing to within a frame inside each contiguous run, and type-3 status records.
//!    Those are the LP-1 rules that this implementation *does* meet; §5 below says which it does
//!    not. Gaps are expected on a loaded box and are checked, never tolerated silently.
//! 3. **The stream says what was measured, not what the recipe declares.** A hand-started
//!    pipeline reports `mode_rules: recipe-declared` at confidence 0 (it measured nothing); the
//!    chooser's pipeline reports the probe's own mode, rules version, confidence, SNR and
//!    parameters, because the chooser *did* measure them.
//! 4. **Attach, don't duplicate (§12.3).** Two listeners on one station share **one** pipeline
//!    and one DDC; the first to leave does not stop it; the last one does, and only because it
//!    is session-owned.
//! 5. **Stereo asks for legacy (§12.13).** A recipe's `audio` output is mono, so `channels=2`
//!    chooses the chain that can actually deliver two channels rather than quietly serving one.
//! 6. **Noise is still refused.** The chooser's weak-carrier rule (T-869) needs *measured*
//!    energy, so the LP-1 freeze's `4422 no-analog-mode` on noise holds with the switch on.
//! 7. **The weak-carrier rule, through the opener** (the NOAA finding). A narrow drag on a
//!    channel whose band estimate gave up is **demodulated with the squelch armed**, not refused
//!    — and it says the mode was not recognised (`mode_confidence: 0`) while it does so.
//!
//! **What stage 4 does not yet deliver** (ADR-0015 §12.9's stage-4 note, measured): on the
//! recipe path `sample_index` counts the frames the sink published, so a closed squelch is not
//! the flagged **jump** LP-1 §5 freezes, `sample_index == 960 × (frames + squelched_frames)`
//! does not hold, and a gap after a discontinuity need not be a whole number of frames (the
//! sink's `audio_out` drops a partial frame). An **attached** listener also joins an ongoing
//! stream, so its first record need not be index 0. This file therefore pins the record contract
//! that does hold (2 above), and the ADR states the rest as stage 5's work.
//!
//! **Blind.** The station is found by the probe from the user's drag, as in LP-1; nothing here
//! looks a frequency up, and the recipe is chosen by [`hk_recipe::matching::rank`], whose
//! `freq_hz` weight is zero.
//!
//! The source is LP-1's synthetic WFM station (1 kHz + 2.9 kHz programme, a 19 kHz pilot,
//! 75 kHz peak deviation) 200 kHz above a 100 MHz, 1 Msps `ci8` recording, replayed by
//! [`MockSdrDriver`] looping, unpaced and lossless.

mod common;

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::TempDir;
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Pacing};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_pipeline::class::window_class;
use hk_pipeline::recipes::runtime::RecipeRuntime;
use hk_pipeline::{
    ListenSettings, Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory,
    replay_plan,
};
use hk_stream::record::parse_status_record;
use hk_stream::{BinaryRecordHeader, OpenerRegistry, RecordFlags};
use serde_json::Value;
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

const TOKEN: &str = "t869-listen-switch-token-0123456789abcdef";
const CENTER: f64 = 100.0e6;
const FS: f64 = 1.0e6;
const STATION_OFFSET_HZ: f64 = 200e3;
const RECORDING_S: f64 = 2.0;
/// The narrowband channel, relative to [`CENTER`]: the NOAA-shaped case (§7 below). Its
/// deviation is wide enough that the occupied bandwidth **fills** the box a 20 kHz drag opens,
/// which is exactly what the explorer saw at 162.4008 MHz — C13's band estimate gives up
/// (`FillsBand`/`LowSnr`), and with it every estimate that abstains for a band reason. It sits
/// 600 kHz from the wideband station so the probe's WFM trial (T-070, which searches outward
/// from the selection) cannot wander onto that station instead.
const NARROW_OFFSET_HZ: f64 = -400e3;
/// Its peak deviation, Hz.
const NARROW_DEVIATION_HZ: f64 = 25e3;
/// Its amplitude (the wideband station's is 40, the noise ±6).
const NARROW_AMPLITUDE: f64 = 3.0;
const RATE_HZ: f64 = 48_000.0;
const FRAME: u64 = 960;
const RECORD_HEADER_LEN: usize = 32;

type Ws = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

/// LP-1's station recording (`listen_conformance.rs`), written here so this file's run is
/// independent of it.
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
    let mut narrow_phase = 0.0f64;
    let mut narrow_lp = 0.0f64;
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let t = i as f64 / FS;
        let m = 0.5 * (tau * 1000.0 * t).sin()
            + 0.3 * (tau * 2900.0 * t).sin()
            + 0.1 * (tau * 19_000.0 * t).sin();
        phase = (phase + tau * (STATION_OFFSET_HZ + 75e3 * m) / FS) % tau;
        // The NOAA-shaped channel (see `nbfm_query`): an FM carrier modulated by BAND-LIMITED
        // NOISE, the way speech is — so it has no dominant line — and wide enough that its
        // occupancy fills the box a 20 kHz drag opens. That pair is exactly the explorer's
        // refusal: "C13 OBW99 abstained and no dominant carrier line".
        narrow_lp += 0.05 * (noise() / 6.0 - narrow_lp);
        let nm = (10.0 * narrow_lp).clamp(-1.0, 1.0);
        narrow_phase =
            (narrow_phase + tau * (NARROW_OFFSET_HZ + NARROW_DEVIATION_HZ * nm) / FS) % tau;
        let re = (40.0 * phase.cos() + NARROW_AMPLITUDE * narrow_phase.cos() + noise())
            .round()
            .clamp(-128.0, 127.0) as i8;
        let im = (40.0 * phase.sin() + NARROW_AMPLITUDE * narrow_phase.sin() + noise())
            .round()
            .clamp(-128.0, 127.0) as i8;
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

/// A live run over the mock SDR with `hk serve`'s listen front end, with the stage-4 switch in
/// the state the test asks for.
struct Run {
    handle: PipelineHandle,
    server: Server,
    runtime: Arc<RecipeRuntime>,
    _dir: TempDir,
}

impl Run {
    fn start(tag: &str, pipeline_audio: bool) -> Self {
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
            max_listeners: 8,
            ..Default::default()
        });
        let counters = handle.counters();
        wait("the first second of samples", || {
            counters.source.samples.load(Ordering::Relaxed) >= FS as u64
        });
        let listen = handle.listen_service();
        // The switch is per opener, so the test never touches the process environment (which
        // another test in the same process would see).
        listen.set_pipeline_audio(pipeline_audio);
        assert_eq!(
            listen.pipeline_audio(),
            pipeline_audio,
            "the opener's recipe path must be wired"
        );
        let state = ApiState {
            on_demand: OpenerRegistry::new().with("listen", listen),
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
        let runtime = handle.recipe_runtime();
        Self {
            handle,
            server,
            runtime,
            _dir: dir,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    /// The pipelines the runtime holds, as `GET /api/pipelines` serves them.
    fn pipelines(&self) -> Vec<Value> {
        self.runtime.pipelines_json()["pipelines"]
            .as_array()
            .cloned()
            .unwrap_or_default()
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
    let deadline = Instant::now() + Duration::from_secs(180);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn open(addr: SocketAddr, query: &str) -> Ws {
    let (mut ws, _) =
        tungstenite::connect(format!("ws://{addr}/ws/open/listen?token={TOKEN}&{query}"))
            .expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    }
    ws
}

/// A 150 kHz selection on the station (a user's drag around what they see).
fn station_query() -> String {
    let f = CENTER + STATION_OFFSET_HZ;
    format!("f_lo={}&f_hi={}", f - 75e3, f + 75e3)
}

/// A 20 kHz drag on the narrowband channel — the shape of the explorer's NOAA selection.
fn nbfm_query() -> String {
    let f = CENTER + NARROW_OFFSET_HZ;
    format!("f_lo={}&f_hi={}", f - 10e3, f + 10e3)
}

/// The first message: the header (`Ok`) or the refusal (`Err`).
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

fn open_admitted(addr: SocketAddr, query: &str) -> (Ws, Value) {
    let mut ws = open(addr, query);
    match first(&mut ws) {
        Ok(h) => (ws, h),
        Err(r) => panic!("refused: {r}"),
    }
}

fn close(mut ws: Ws) {
    let _ = ws.close(None);
    let _ = ws.read();
}

/// What a client sees on the wire: data records, drop markers and status records.
#[derive(Default)]
struct Heard {
    data: u64,
    statuses: u64,
    /// Drop markers seen since the last data record: the server telling this consumer it missed
    /// records (a slow reader on a loaded box), so the next index it sees is legitimately ahead.
    drops_pending: u64,
    /// Drop markers over the whole read.
    drops: u64,
    /// Gaps in `sample_index` (each one flagged, or behind a drop marker).
    gaps: u64,
    first_index: Option<u64>,
    last_index: u64,
    last_t: f64,
    /// `(sample_index, capture time)` of the first record of the current contiguous run: the
    /// audio clock's anchor, re-taken after each gap.
    anchor: Option<(u64, f64)>,
}

impl Heard {
    /// Reads until `n` data records and at least one status record have arrived, checking the
    /// §12.2 record contract as it goes.
    fn listen(ws: &mut Ws, n: u64) -> Self {
        Self::read(ws, n, 1)
    }

    /// As [`Self::listen`], with the status records required stated (0: don't wait for one).
    fn read(ws: &mut Ws, n: u64, statuses: u64) -> Self {
        let mut h = Heard::default();
        while h.data < n || h.statuses < statuses {
            let Message::Binary(b) = ws.read().expect("a record") else {
                continue;
            };
            assert!(b.len() >= RECORD_HEADER_LEN, "short record: {}", b.len());
            let rh = BinaryRecordHeader::decode(&b[..RECORD_HEADER_LEN]).expect("record header");
            match rh.record_type {
                1 => {
                    let payload = b.len() - RECORD_HEADER_LEN;
                    assert_eq!(
                        payload,
                        2 * FRAME as usize,
                        "960 mono i16 samples per record"
                    );
                    let t = rh.t.as_unix_nanos() as f64 / 1e9;
                    let flagged = rh.flags.contains(RecordFlags::DISCONTINUITY);
                    // A step other than one frame is a GAP, and a gap must be accounted for:
                    // either the producer flagged it (a live-edge seek, or the input broke) or
                    // the server told this consumer it had dropped records for it. Both happen
                    // on a loaded box, and both are things the product allows — what must never
                    // happen is an unexplained jump, or an index going backwards.
                    let step = if h.first_index.is_some() {
                        Some(
                            rh.sample_index
                                .checked_sub(h.last_index)
                                .unwrap_or_else(|| {
                                    panic!(
                                        "sample_index went backwards: {} < {}",
                                        rh.sample_index, h.last_index
                                    )
                                }),
                        )
                    } else {
                        None
                    };
                    if let Some(step) = step
                        && step != FRAME
                    {
                        assert!(
                            flagged || h.drops_pending > 0,
                            "sample_index jumped {} → {} ({step} samples) with no \
                             DISCONTINUITY flag and no drop marker",
                            h.last_index,
                            rh.sample_index
                        );
                        h.gaps += 1;
                        h.anchor = None;
                    }
                    // `t` is capture time and `sample_index` is the audio clock. Inside a
                    // contiguous run they must agree to within a frame (on the recipe path they
                    // agree no better: the sink's index is its own frame count — see the module
                    // docs' "what stage 4 does not yet deliver"). A gap re-anchors the run
                    // rather than being smuggled into the comparison.
                    let (i0, t0) = *h.anchor.get_or_insert((rh.sample_index, t));
                    let want = t0 + (rh.sample_index - i0) as f64 / RATE_HZ;
                    assert!(
                        (t - want).abs() < FRAME as f64 / RATE_HZ,
                        "t and sample_index disagree by more than a frame within one run: \
                         t {t} vs {want}"
                    );
                    assert!(t >= h.last_t, "capture time went backwards: {t}");
                    h.first_index.get_or_insert(rh.sample_index);
                    h.last_index = rh.sample_index;
                    h.last_t = t;
                    h.drops_pending = 0;
                    h.data += 1;
                }
                2 => {
                    // "dropped N": this consumer was too slow (a 0.6 s queue on a loaded box).
                    h.drops_pending += 1;
                    h.drops += 1;
                }
                3 => {
                    let (_, v) = parse_status_record(&b).expect("status record");
                    for k in ["level_dbfs", "squelch_open", "frames", "latency_ms"] {
                        assert!(v.get(k).is_some(), "status record is missing {k}: {v}");
                    }
                    h.statuses += 1;
                }
                _ => {}
            }
        }
        h
    }
}

/// §1 + §2 + §3: with the switch on, the chooser serves the station from an ephemeral audio
/// pipeline, on the frozen wire contract, saying what it measured.
#[test]
fn the_switch_on_serves_the_station_from_an_ephemeral_audio_pipeline() {
    let run = Run::start("t869-switch-on", true);
    let (mut ws, header) = open_admitted(run.addr(), &station_query());

    // 1. The stream is a pipeline's audio output, and says so.
    let audio = &header["audio"];
    assert!(
        header["stream_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("audio/")),
        "the stream is a pipeline output: {header}"
    );
    assert_eq!(header["datatype"], "ri16_le", "{header}");
    assert_eq!(header["sample_rate_hz"], 48_000.0, "{header}");
    assert_eq!(audio["channels"], 1, "{header}");
    assert_eq!(audio["frame_samples"], 960, "{header}");
    assert_eq!(audio["recipe"], "analog-wfm@1", "{header}");
    assert_eq!(audio["output_id"], "audio", "{header}");
    assert_eq!(audio["edit_rev"], 0, "{header}");
    let pid = audio["pipeline_id"]
        .as_str()
        .expect("pipeline_id")
        .to_owned();

    // 3. The mode is what the chooser MEASURED, not what the recipe declares.
    assert_eq!(audio["mode"], "wfm", "{header}");
    assert_ne!(
        audio["mode_rules"], "recipe-declared",
        "the chooser probed this channel: {header}"
    );
    assert!(
        audio["mode_confidence"].as_f64().unwrap_or(0.0) > 0.0,
        "a measured mode carries its confidence: {header}"
    );
    assert!(audio["snr_db"].is_f64(), "the probe measured SNR: {header}");
    assert!(
        header["provenance_ref"].is_string(),
        "the audio is provenance-linked to the samples it was probed on: {header}"
    );

    // The pipeline is ephemeral: owned by the session that asked for it.
    let pipelines = run.pipelines();
    assert_eq!(pipelines.len(), 1, "one pipeline: {pipelines:?}");
    assert_eq!(pipelines[0]["id"], pid.as_str());
    assert_eq!(pipelines[0]["owner"], "session", "{:?}", pipelines[0]);
    assert_eq!(pipelines[0]["recipe_id"], "analog-wfm");
    assert_eq!(pipelines[0]["state"], "running");

    // 2. The wire contract: the records a browser or a TCP one-liner reads.
    let heard = Heard::listen(&mut ws, 20);
    assert!(heard.statuses > 0, "no status records");

    // 8 (LP-1 §8): closing the socket stops the ephemeral pipeline.
    close(ws);
    wait("the ephemeral pipeline to stop", || {
        run.pipelines().is_empty()
    });
    let lc = &run.handle.counters().listen;
    wait("the listener to be counted out", || {
        lc.running.load(Ordering::SeqCst) == 0
    });
    assert_eq!(lc.detached.load(Ordering::SeqCst), 1, "{}", lc.to_json());
    assert_eq!(lc.attached.load(Ordering::SeqCst), 1, "{}", lc.to_json());
    run.finish();
}

/// §1: the switch off is today's chain, byte for byte — no pipeline is started.
#[test]
fn the_switch_off_serves_the_legacy_chain() {
    let run = Run::start("t869-switch-off", false);
    let (mut ws, header) = open_admitted(run.addr(), &station_query());
    assert!(
        header["stream_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("listen/")),
        "the legacy chain serves it: {header}"
    );
    assert!(
        header["audio"]["pipeline_id"].is_null(),
        "no pipeline is involved: {header}"
    );
    assert!(run.pipelines().is_empty(), "{:?}", run.pipelines());
    let heard = Heard::listen(&mut ws, 10);
    assert!(heard.data >= 10);
    close(ws);
    run.finish();
}

/// §4: two listeners on one station share one pipeline, and only the last one to leave stops it.
#[test]
fn two_listeners_on_one_station_share_one_pipeline() {
    let run = Run::start("t869-attach", true);
    let addr = run.addr();
    let (mut a, ha) = open_admitted(addr, &station_query());
    let first_pid = ha["audio"]["pipeline_id"].as_str().unwrap().to_owned();
    // Hear the first listener before the second arrives, so the attach lands on a pipeline that
    // is demonstrably already producing audio.
    Heard::read(&mut a, 5, 0);

    // A second listener on the same station — a second browser tab.
    let (mut b, hb) = open_admitted(addr, &station_query());
    assert_eq!(
        hb["audio"]["pipeline_id"].as_str().unwrap(),
        first_pid,
        "the second listener must ATTACH, not build a second DDC: {hb}"
    );
    assert_eq!(
        hb["stream_id"], ha["stream_id"],
        "one stream, two consumers"
    );
    let pipelines = run.pipelines();
    assert_eq!(
        pipelines.len(),
        1,
        "attach-don't-duplicate: one pipeline for one station, not two: {pipelines:?}"
    );
    let lc = &run.handle.counters().listen;
    assert_eq!(
        lc.running.load(Ordering::SeqCst),
        2,
        "both listeners count against the listen budget: {}",
        lc.to_json()
    );

    // Both hear the station.
    Heard::read(&mut b, 5, 0);
    Heard::read(&mut a, 5, 0);

    // The first to leave does NOT stop it.
    close(a);
    wait("the first listener to be counted out", || {
        lc.running.load(Ordering::SeqCst) == 1
    });
    assert_eq!(run.pipelines().len(), 1, "one listener is still listening");
    Heard::read(&mut b, 5, 0);

    // The last one does, because it is session-owned.
    close(b);
    wait("the ephemeral pipeline to stop", || {
        run.pipelines().is_empty()
    });
    run.finish();
}

/// §5: stereo asks for the legacy chain, which can deliver two channels (§12.13).
#[test]
fn stereo_asks_for_the_legacy_chain() {
    let run = Run::start("t869-stereo", true);
    let (ws, header) = open_admitted(run.addr(), &format!("{}&channels=2", station_query()));
    assert!(
        header["stream_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("listen/")),
        "a recipe's audio output is mono, so stereo goes to the chain: {header}"
    );
    assert!(
        run.pipelines().is_empty(),
        "no pipeline promises stereo it cannot serve: {:?}",
        run.pipelines()
    );
    close(ws);
    run.finish();
}

/// §7: **the weak-carrier rule, through the real opener** (T-869, the NOAA finding). A narrow
/// drag on a channel whose band estimate gives up is demodulated with the squelch armed instead
/// of being refused `4422 no-analog-mode`.
///
/// The assertions are what make this a test of *the rule* rather than of ordinary mode
/// selection: `mode_confidence` is zero and the plan is NBFM, i.e. nothing recognised the mode
/// and the audio is served anyway, with a squelch armed from the measured noise
/// (`squelch.noise_dbfs`). Red on the first cut of this rule, which asked for a measured
/// `snr_box_db`: that estimate abstains for any band reason, so it was never measured here.
#[test]
fn a_narrow_channel_whose_band_estimate_gave_up_is_demodulated_not_refused() {
    let run = Run::start("t869-weak-carrier", true);
    let mut ws = open(run.addr(), &nbfm_query());
    let header = match first(&mut ws) {
        Ok(h) => h,
        Err(r) => {
            panic!("a narrow selection with measured energy must not be refused (T-869): {r}")
        }
    };
    let audio = &header["audio"];
    assert_eq!(audio["mode"], "nbfm", "{header}");
    assert_eq!(
        audio["mode_confidence"], 0.0,
        "the rule serves audio for a mode NOTHING recognised, and must say so: {header}"
    );
    assert!(
        audio["squelch"]["noise_dbfs"].is_f64(),
        "the squelch is armed from the measured noise power: {header}"
    );
    // No NBFM recipe exists (§12.5), so this is the legacy chain — the chooser's `legacy` answer
    // carrying the fallback plan.
    assert!(
        header["stream_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("listen/")),
        "{header}"
    );
    assert!(run.pipelines().is_empty(), "{:?}", run.pipelines());
    // The chain is live: a status record arrives whether or not the squelch has opened (a weak
    // channel is meant to be silent until it rises, which is the point of arming the squelch
    // rather than refusing).
    let heard = Heard::read(&mut ws, 0, 1);
    assert_eq!(heard.statuses, 1, "the chain reports status");
    close(ws);
    run.finish();
}

/// §6: the weak-carrier rule needs MEASURED energy, so noise is still refused `4422` with the
/// switch on — the LP-1 freeze holds.
#[test]
fn noise_is_still_refused_with_the_switch_on() {
    let run = Run::start("t869-noise", true);
    let mut ws = open(
        run.addr(),
        &format!("f_lo={}&f_hi={}", CENTER - 300e3, CENTER - 280e3),
    );
    let r = first(&mut ws).expect_err("noise has nothing to demodulate");
    assert_eq!(r["status"], 422, "{r}");
    assert_eq!(r["code"], "no-analog-mode", "{r}");
    assert!(run.pipelines().is_empty(), "{:?}", run.pipelines());
    run.finish();
}
