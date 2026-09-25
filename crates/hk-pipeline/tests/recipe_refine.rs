//! **T-870 / ADR-0015 §12.9 stage 4 (LP-6): `refine.objective.builtin` + a refinement applied as
//! a hot edit.** SIGNAL-062 (FM broadcast), end to end **through the mock SDR device interface**.
//!
//! A WFM audio recipe declaring `"refine": {"objective": {"builtin": "wfm-pilot"}, ...}` (ADR-0011
//! §8.7) is started on a band the user drew **off** the station — the station's true centre is
//! the fixture's hidden truth, never given to the system. The pipeline's refinement loop (T-070's
//! loop and objective, with Listen's settings) finds the station from the demodulated output and
//! applies the result as an **ordinary hot edit** (ADR-0015 §12.6): the channel is re-plumbed at a
//! chunk boundary, `edit_rev` advances by one per applied result, the refined bandwidth is the
//! running revision's `input.bandwidth_hz`, and the audio stream keeps its consumer, still
//! carrying the programme. An objective that never locks never retunes anything.

mod common;

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::TempDir;
use hk_api::{StreamRegistry, StreamServer, StreamServerConfig, Token};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Pacing};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_pipeline::class::window_class;
use hk_pipeline::recipes::runtime::{RecipeRuntime, Target, parse_recipe};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use hk_stream::record::parse_status_record;
use hk_stream::{OpenerRegistry, Record, StreamReader};
use serde_json::{Value, json};

const TOKEN: &str = "t870-recipe-refine-token-0123456789abcdef";
const CENTER: f64 = 100.0e6;
const FS: f64 = 1.0e6;
/// The station's offset from the tuned centre: the hidden truth.
const STATION_OFFSET_HZ: f64 = 200e3;
/// How far off the station the user's band is drawn.
const DRAWN_OFF_HZ: f64 = 25e3;
const RECORDING_S: f64 = 2.0;
const LIMIT: Duration = Duration::from_secs(180);

/// A bound on waiting for an EVENT (never an assertion about how long something took).
fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The synthetic WFM station of LP-1/LP-2 (`listen_conformance.rs`, `recipe_audio.rs`): 1 kHz +
/// 2.9 kHz programme tones, a 19 kHz pilot, 75 kHz peak deviation, 200 kHz above a 100 MHz
/// 1 Msps ci8 recording.
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

/// A WFM audio recipe (schema 3) whose refinement is the builtin `wfm-pilot` objective.
fn refining_wfm_recipe(id: &str) -> Value {
    json!({
        "schema": "hackriff.recipe", "schema_version": 3, "id": id, "version": 1,
        "name": "T-870 refining WFM audio",
        "input": {"port": "iq", "sample_rate_hz": 240000, "bandwidth_hz": 200000},
        "nodes": [
            {"id": "fm", "block": "fm_demod", "params": {"deviation_hz": 75000}},
            {"id": "sq", "block": "squelch", "params": {"mode": "fm-noise"}},
            {"id": "de", "block": "deemphasis", "params": {"tau_s": 75e-6}},
            {"id": "gain", "block": "agc", "params": {"target_dbfs": -6.0}},
            {"id": "out", "block": "audio_out"}
        ],
        "outputs": [{"id": "audio", "kind": "audio", "from": "out", "channels": "mono",
                     "profile": {"mode": "wfm"}}],
        "output_policy": {"content_class": "unrestricted"},
        "refine": {"objective": {"builtin": "wfm-pilot"}, "tune": ["center_hz", "bandwidth_hz"]}
    })
}

fn band(center: f64, width: f64) -> Target {
    Target::Band {
        f_lo: center - 0.5 * width,
        f_hi: center + 0.5 * width,
    }
}

struct Run {
    handle: Option<PipelineHandle>,
    streams: StreamRegistry,
    rt: Arc<RecipeRuntime>,
    tcp: StreamServer,
    _dir: TempDir,
}

impl Run {
    /// A window-classed, lossless run over the mock SDR (every sample reaches the pipeline).
    fn start(tag: &str) -> Self {
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
        let streams = StreamRegistry::new();
        let reg = streams.clone();
        cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
        let reg = streams.clone();
        cfg.stream_unsink = Some(Arc::new(move |id| {
            reg.unregister(id);
        }));
        let handle = Pipeline::start(
            cfg,
            Box::new(source),
            info,
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        let counters = handle.counters();
        wait("the first second of samples", || {
            counters.source.samples.load(Ordering::Relaxed) >= FS as u64
        });
        let rt = handle.recipe_runtime();
        let tcp = StreamServer::start(
            StreamServerConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                Token::from_config(TOKEN).unwrap(),
            ),
            streams.clone(),
            OpenerRegistry::new(),
        )
        .unwrap();
        Self {
            handle: Some(handle),
            streams,
            rt,
            tcp,
            _dir: dir,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.tcp.local_addr()
    }

    fn pipeline(&self, id: &str) -> Value {
        self.rt.pipeline_json(id).unwrap()
    }

    fn finish(mut self) {
        self.rt.stop_all();
        let handle = self.handle.take().unwrap();
        handle.stop();
        handle.wait().unwrap();
        drop(self.streams);
    }
}

fn open_tcp(addr: SocketAddr, stream_id: &str) -> StreamReader<TcpStream> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(format!("{stream_id}?token={TOKEN}\n").as_bytes())
        .unwrap();
    StreamReader::new(s)
}

/// Reads until `want` audio data records arrived **and** a status record satisfying `until`
/// arrived; returns the samples (as f32, full scale 1) and every status record.
///
/// Status records come on a wall-clock tick (`STATUS_INTERVAL`) while this replay is unpaced, so
/// how many audio records separate two of them depends on the machine: the wait is for the
/// status EVENT, never for a fixed count of audio records to contain one.
fn read_audio(
    r: &mut StreamReader<TcpStream>,
    want: usize,
    until: impl Fn(&Value) -> bool,
) -> (Vec<f32>, Vec<Value>) {
    let (mut audio, mut status, mut records) = (Vec::new(), Vec::new(), 0);
    let deadline = Instant::now() + LIMIT;
    while records < want || !status.iter().any(&until) {
        assert!(Instant::now() < deadline, "audio records");
        match r.next_record().unwrap() {
            Some(Record::Binary(b)) => {
                records += 1;
                audio.extend(
                    b.payload
                        .chunks_exact(2)
                        .map(|x| f32::from(i16::from_le_bytes([x[0], x[1]])) / 32768.0),
                );
            }
            Some(Record::Unknown(f)) => {
                if let Some((_, v)) = parse_status_record(&f) {
                    status.push(v);
                }
            }
            Some(_) => {}
            None => panic!("the audio stream ended"),
        }
    }
    (audio, status)
}

/// Tone power at `f` over total power, dB.
fn tone_db(x: &[f32], fs: f64, f: f64) -> f64 {
    let (mut re, mut im, mut total) = (0.0f64, 0.0f64, 0.0f64);
    for (n, &v) in x.iter().enumerate() {
        let ph = std::f64::consts::TAU * f * n as f64 / fs;
        re += f64::from(v) * ph.cos();
        im += f64::from(v) * ph.sin();
        total += f64::from(v).powi(2);
    }
    10.0 * ((2.0 * (re * re + im * im) / x.len() as f64) / total.max(1e-30)).log10()
}

#[test]
fn a_builtin_wfm_pilot_objective_refines_the_channel_and_applies_it_as_a_hot_edit() {
    let run = Run::start("t870-refine");
    let truth = CENTER + STATION_OFFSET_HZ;
    let drawn = truth + DRAWN_OFF_HZ;
    let id = run
        .rt
        .start(
            parse_recipe(refining_wfm_recipe("t870-wfm")).unwrap(),
            band(drawn, 200e3),
        )
        .unwrap();
    let stream_id = format!("audio/{id}/audio");
    // A consumer attached before the refinement: it must survive the edit.
    let mut r = open_tcp(run.addr(), &stream_id);
    let h = r.read_header().unwrap().clone();
    assert_eq!(h.stream_id, stream_id);

    wait("an applied refinement", || {
        run.pipeline(&id)["refinement"]["updates"]
            .as_u64()
            .is_some_and(|n| n >= 1)
    });
    let p = run.pipeline(&id);
    let rf = &p["refinement"];
    assert_eq!(rf["objective"], json!({"builtin": "wfm-pilot"}), "{rf}");
    let cur = &rf["current"];
    assert_eq!(cur["provenance"], "refined by output analysis", "{cur}");
    assert_eq!(cur["locked"], true, "only a locked result retunes: {cur}");
    let start = cur["start_center_hz"].as_f64().unwrap();
    assert!(
        (start - drawn).abs() < 1.0,
        "the search started from the user's band, not from any database: {start}"
    );
    let refined = cur["center_hz"].as_f64().unwrap();
    assert!(
        (refined - truth).abs() <= 2_000.0,
        "refined {refined} Hz, the station is at {truth} Hz (drawn at {drawn})"
    );
    let refined_bw = cur["bandwidth_hz"].as_f64().unwrap();

    // Applied as an ordinary hot edit: the channel moved, one edit per applied result, and the
    // refined bandwidth is the running revision's input bandwidth.
    let ch = &p["channel"];
    assert_eq!(ch["center_hz"].as_f64(), Some(refined), "{p}");
    assert_eq!(ch["bandwidth_hz"].as_f64(), Some(refined_bw), "{p}");
    assert_eq!(ch["sample_rate_hz"].as_f64(), Some(240_000.0), "{p}");
    assert_eq!(
        p["edit_rev"], rf["applied_edit_rev"],
        "the latest edit is the refinement's: {p}"
    );
    assert_eq!(
        p["edit_rev"].as_u64(),
        rf["updates"].as_u64(),
        "every applied refinement is exactly one edit, and nothing else edited: {p}"
    );
    assert_eq!(p["stats"]["edits"], p["edit_rev"], "{p}");
    assert_eq!(p["state"], "running", "{p}");
    // "Writes the new centre/bandwidth into the pipeline's input": the running revision (saved
    // here) carries the refined bandwidth as `input.bandwidth_hz`.
    let saved = run.rt.save_pipeline_json(&id).unwrap();
    let doc = run
        .rt
        .recipe_json("t870-wfm", saved["version"].as_u64().map(|v| v as u32))
        .unwrap();
    assert_eq!(
        doc["input"]["bandwidth_hz"].as_f64(),
        Some(refined_bw),
        "{doc}"
    );
    assert_eq!(
        doc["refine"]["objective"],
        json!({"builtin": "wfm-pilot"}),
        "{doc}"
    );
    assert_eq!(
        p["outputs"],
        json!([{"id": "audio", "kind": "audio", "stream_id": stream_id}]),
        "the audio output keeps its stream across the edit: {p}"
    );

    // The consumer that attached before the edit keeps receiving the programme on the refined
    // channel, and the status records carry Listen's refinement keys.
    let refined_status = |s: &Value| s["refine_updates"].as_u64().is_some_and(|n| n >= 1);
    let (_, status) = read_audio(&mut r, 1, refined_status);
    // Then a stretch of the programme demodulated after that status record, so the tail is
    // refined-channel audio.
    let (audio, _) = read_audio(&mut r, 150, |_| true);
    let tail = &audio[audio.len() - 48_000..];
    let t1k = tone_db(tail, 48_000.0, 1000.0);
    assert!(
        t1k > -12.0,
        "the 1 kHz programme tone dominates the refined channel's audio: {t1k:.1} dB"
    );
    let s = status
        .iter()
        .rev()
        .find(|s| refined_status(s))
        .unwrap_or_else(|| panic!("a status record after the refinement: {status:?}"));
    let rc = s["refined_center_hz"].as_f64().unwrap();
    assert!((rc - truth).abs() <= 2_000.0, "{s}");
    assert!(s["refined_bandwidth_hz"].as_f64().is_some(), "{s}");

    // Background re-refinement keeps running under hysteresis; whatever it decides, each applied
    // result is one edit and the channel stays on the station.
    wait("a background re-refinement", || {
        run.pipeline(&id)["refinement"]["attempts"]
            .as_u64()
            .is_some_and(|n| n >= 2)
    });
    let p = run.pipeline(&id);
    assert_eq!(
        p["edit_rev"].as_u64(),
        p["refinement"]["updates"].as_u64(),
        "{p}"
    );
    let c = p["channel"]["center_hz"].as_f64().unwrap();
    assert!((c - truth).abs() <= 2_000.0, "{p}");

    // A recipe without a builtin objective has no refinement and is never edited by one.
    let mut plain = refining_wfm_recipe("t870-plain");
    plain.as_object_mut().unwrap().remove("refine");
    let plain_id = run
        .rt
        .start(parse_recipe(plain).unwrap(), band(drawn, 200e3))
        .unwrap();
    let p = run.pipeline(&plain_id);
    assert_eq!(p["refinement"], Value::Null, "{p}");
    assert_eq!(p["channel"]["center_hz"].as_f64(), Some(drawn), "{p}");
    assert_eq!(p["edit_rev"], 0, "{p}");
    run.finish();
}

#[test]
fn a_refinement_that_never_locks_never_retunes_the_pipeline() {
    let run = Run::start("t870-refine-nolock");
    // Noise only: 300 kHz below the tuned centre, nowhere near the station.
    let empty = CENTER - 300e3;
    let id = run
        .rt
        .start(
            parse_recipe(refining_wfm_recipe("t870-empty")).unwrap(),
            band(empty, 200e3),
        )
        .unwrap();
    wait("a completed refinement attempt", || {
        run.pipeline(&id)["refinement"]["attempts"]
            .as_u64()
            .is_some_and(|n| n >= 1)
    });
    let p = run.pipeline(&id);
    let rf = &p["refinement"];
    assert_eq!(rf["last"]["locked"], false, "{rf}");
    assert_eq!(rf["last"]["accepted"], false, "{rf}");
    assert_eq!(rf["updates"], 0, "{rf}");
    assert_eq!(rf["current"], Value::Null, "{rf}");
    assert_eq!(
        p["edit_rev"], 0,
        "nothing retunes to an unlocked result: {p}"
    );
    assert_eq!(p["channel"]["center_hz"].as_f64(), Some(empty), "{p}");
    assert_eq!(p["state"], "running", "{p}");
    run.finish();
}
