//! **T-866 / ADR-0015 §12.9 stage 2 (LP-2): a recipe that ends in an audio sink is audible.**
//! SIGNAL-062 (FM broadcast), end to end **through the mock SDR device interface**.
//!
//! A hand-started WFM recipe — `fm_demod` → `squelch` (`fm-noise`) → `deemphasis` → `agc` →
//! `audio_out`, one `audio` output, schema 3 — runs on a synthetic station behind
//! [`MockSdrDriver`], and its audio is served at `audio/<pipeline>/<output>` in the stream
//! contract §12.2 profile **unchanged** (`kind: audio`, `ri16_le`, 48 kS/s, 960-sample records,
//! type-3 status records), with ADR-0011 §8.2's additive header keys. Legacy Listen
//! (`/ws/open/listen`) is untouched: both paths exist side by side (stage 2's "observable").
//!
//! - **Audible:** the station's 1 kHz programme tone dominates the decoded 48 kS/s audio and its
//!   19 kHz pilot is gone; records are 960 samples, `sample_index` advances by 960 per record and
//!   jumps only behind a `DISCONTINUITY` flag.
//! - **Status:** one record carries the §12.2 keys and the `<node>.<metric>` batch.
//! - **Liveness (ADR-0011 §8.5):** the audio recipe defaults to `live-edge` at 0.6 s; a
//!   `liveness`-only hot edit changes the bound without re-plumbing the channel, and the audio
//!   stream keeps its consumer. On a live (non-lossless) source that outruns the pipeline, the
//!   reader seeks to the live edge, counts it, and the next audio record is flagged.
//! - **Discovery:** `GET /api/blocks` lists `squelch`, `agc`, `deemphasis`, `audio_out`.

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
use hk_stream::{OpenerRegistry, Record, RecordFlags, StreamKind, StreamReader};
use serde_json::{Value, json};

const TOKEN: &str = "t866-recipe-audio-token-0123456789abcdef";
const CENTER: f64 = 100.0e6;
const FS: f64 = 1.0e6;
const STATION_OFFSET_HZ: f64 = 200e3;
/// Every tone completes whole cycles in it, so the looping recording is a continuous station.
const RECORDING_S: f64 = 2.0;
const LIMIT: Duration = Duration::from_secs(120);

/// A bound on waiting for an EVENT (never an assertion about how long something took).
fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The synthetic WFM station (LP-1's, `listen_conformance.rs`): 1 kHz + 2.9 kHz programme
/// tones, a 19 kHz pilot, 75 kHz peak deviation, 200 kHz above a 100 MHz 1 Msps ci8 recording.
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

/// The hand-started WFM audio recipe (schema 3). LP-3 (`recipes/analog-wfm.recipe.json`) is the
/// built-in one; this test only needs the blocks and the output kind.
fn wfm_audio_recipe() -> Value {
    json!({
        "schema": "hackriff.recipe", "schema_version": 3, "id": "t866-wfm", "version": 1,
        "name": "T-866 WFM audio",
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
        "output_policy": {"content_class": "unrestricted"}
    })
}

fn station() -> Target {
    let f = CENTER + STATION_OFFSET_HZ;
    Target::Band {
        f_lo: f - 100e3,
        f_hi: f + 100e3,
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
    /// A window-classed run over the mock SDR: `lossless` holds capture for the chain (every
    /// sample reaches the pipeline); without it the unpaced source outruns the pipeline.
    fn start(tag: &str, lossless: bool) -> Self {
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
        cfg.lossless = lossless;
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

/// One audio data record: `(sample_index, flags, samples)`.
type Audio = (u64, RecordFlags, Vec<i16>);

/// Reads until `want` data records arrived; returns them and every status record.
fn read_audio(r: &mut StreamReader<TcpStream>, want: usize) -> (Vec<Audio>, Vec<Value>) {
    let (mut audio, mut status) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + LIMIT;
    while audio.len() < want {
        assert!(Instant::now() < deadline, "audio records");
        match r.next_record().unwrap() {
            Some(Record::Binary(b)) => audio.push((
                b.header.sample_index,
                b.header.flags,
                b.payload
                    .chunks_exact(2)
                    .map(|x| i16::from_le_bytes([x[0], x[1]]))
                    .collect(),
            )),
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
fn a_hand_started_wfm_recipe_streams_audible_audio_in_the_listen_profile() {
    let run = Run::start("t866-audio", true);
    let blocks = run.rt.blocks_json().to_string();
    for b in ["squelch", "agc", "deemphasis", "audio_out"] {
        assert!(
            blocks.contains(&format!("\"{b}\"")),
            "GET /api/blocks lists {b}"
        );
    }
    let id = run
        .rt
        .start(parse_recipe(wfm_audio_recipe()).unwrap(), station())
        .unwrap();
    let p = run.rt.pipeline_json(&id).unwrap();
    let stream_id = format!("audio/{id}/audio");
    assert_eq!(
        p["outputs"],
        json!([{"id": "audio", "kind": "audio", "stream_id": stream_id}]),
        "{p}"
    );
    assert_eq!(
        p["liveness"],
        json!({"mode": "live-edge", "max_backlog_s": 0.6}),
        "an audio output defaults to live-edge (ADR-0011 §8.5)"
    );

    // The header: the §12.2 audio profile unchanged, plus §8.2's additive keys.
    let mut r = open_tcp(run.addr(), &stream_id);
    let h = r.read_header().unwrap().clone();
    assert_eq!(h.stream_id, stream_id);
    assert_eq!(h.kind, StreamKind::Audio);
    assert_eq!(h.datatype.as_deref(), Some("ri16_le"));
    assert_eq!(h.sample_rate_hz, Some(48_000.0));
    assert_eq!(h.source, "hk-pipeline:recipe:t866-wfm@1");
    let a = h.audio.as_ref().expect("the audio profile");
    assert_eq!((a.channels, a.frame_samples), (1, 960));
    assert_eq!(a.mode, "wfm");
    assert_eq!(a.mode_rules, "recipe-declared", "declared, not estimated");
    assert_eq!(a.deemphasis_s, Some(75e-6));
    assert!(a.agc.enabled);
    assert_eq!(a.squelch.open_snr_db, 6.0);
    assert_eq!(a.demod, "recipe:t866-wfm@1");
    assert_eq!(a.pipeline_id.as_deref(), Some(id.as_str()));
    assert_eq!(a.recipe.as_deref(), Some("t866-wfm@1"));
    assert_eq!(a.output_id.as_deref(), Some("audio"));
    assert_eq!(a.edit_rev, Some(0));
    let hv = serde_json::to_value(&h).unwrap();
    assert!(
        hv.get("pipeline_id").is_none(),
        "the additive keys live in `audio`: {hv}"
    );

    // Records: 960 samples each; `sample_index` +960 per record unless flagged.
    let (audio, status) = read_audio(&mut r, 80);
    for w in audio.windows(2) {
        assert_eq!(w[1].2.len(), 960);
        let step = w[1].0 - w[0].0;
        assert!(
            step == 960 || w[1].1.contains(RecordFlags::DISCONTINUITY),
            "sample_index jumped {} → {} with no DISCONTINUITY flag",
            w[0].0,
            w[1].0
        );
    }
    // Audible: the programme's 1 kHz tone dominates the settled audio; the pilot is gone.
    let pcm: Vec<f32> = audio[30..]
        .iter()
        .flat_map(|(_, _, s)| s.iter().map(|&v| f32::from(v) / 32767.0))
        .collect();
    let peak = pcm.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(peak > 0.1, "audible level (peak {peak})");
    let one_k = tone_db(&pcm, 48_000.0, 1_000.0);
    assert!(
        one_k > -4.0,
        "the 1 kHz programme tone ({one_k} dB of the audio)"
    );
    let pilot = tone_db(&pcm, 48_000.0, 19_000.0);
    assert!(pilot < -40.0, "the 19 kHz pilot is rejected ({pilot} dB)");

    // Status: §12.2's keys plus the node batch, in one record.
    let s = status
        .last()
        .cloned()
        .or_else(|| read_audio_status(&mut r))
        .expect("a status record");
    for k in [
        "level_dbfs",
        "squelch_open",
        "agc_gain_db",
        "frames",
        "squelched_frames",
        "lost_samples",
        "latency_ms",
        "backlog_s",
        "refine_updates",
    ] {
        assert!(s.get(k).is_some(), "status key {k}: {s}");
    }
    assert_eq!(
        s["squelch_open"],
        json!(true),
        "a captured station opens it: {s}"
    );
    for k in ["fm.deviation_hz", "sq.open", "gain.gain_db", "out.frames"] {
        assert!(s.get(k).is_some(), "node metric {k}: {s}");
    }

    // A liveness-only hot edit: no re-plumb, the stream keeps its consumer.
    let mut draft = wfm_audio_recipe();
    draft["input"]["liveness"] = json!({"mode": "live-edge", "max_backlog_s": 0.3});
    let e = run.rt.edit_json(&id, draft).unwrap();
    assert_eq!(e["plan"]["input_changed"], json!(false), "{e}");
    assert_eq!(
        run.rt.pipeline_json(&id).unwrap()["liveness"]["max_backlog_s"],
        json!(0.3)
    );
    let (more, _) = read_audio(&mut r, 10);
    assert_eq!(more.len(), 10, "the same consumer keeps receiving audio");

    // A cold edit that renegotiates every port downstream (the discriminator decimates to
    // 120 kS/s) rebuilds `audio_out`, whose fresh instance counts from 0: the stream keeps its
    // consumer and its `sample_index` never goes backwards (the re-based record is flagged).
    let mut draft = wfm_audio_recipe();
    draft["input"]["liveness"] = json!({"mode": "live-edge", "max_backlog_s": 0.3});
    draft["nodes"][0]["params"]["output_rate_hz"] = json!(120000);
    let e = run.rt.edit_json(&id, draft).unwrap();
    assert_eq!(
        e["swap"]["rebuilt"],
        json!(5),
        "every node, audio_out included, is rebuilt: {e}"
    );
    let last = more.last().unwrap().0;
    let (after, _) = read_audio(&mut r, 40);
    let mut prev = last;
    for (idx, flags, _) in &after {
        assert!(*idx >= prev, "sample_index went backwards {prev} → {idx}");
        assert!(
            *idx == prev || *idx == prev + 960 || flags.contains(RecordFlags::DISCONTINUITY),
            "an unflagged jump {prev} → {idx}"
        );
        prev = *idx;
    }
    run.finish();
}

fn read_audio_status(r: &mut StreamReader<TcpStream>) -> Option<Value> {
    let deadline = Instant::now() + LIMIT;
    while Instant::now() < deadline {
        if let Some(Record::Unknown(f)) = r.next_record().unwrap()
            && let Some((_, v)) = parse_status_record(&f)
        {
            return Some(v);
        }
    }
    None
}

/// On a live source the unpaced mock writes far faster than real time, so the pipeline falls
/// behind: a `live-edge` reader seeks to the live edge (counted), and the audio after a seek is
/// a flagged `sample_index` jump — never a growing backlog played out.
#[test]
fn a_live_edge_audio_stream_seeks_to_the_live_edge_instead_of_lagging() {
    let run = Run::start("t866-live-edge", false);
    let id = run
        .rt
        .start(parse_recipe(wfm_audio_recipe()).unwrap(), station())
        .unwrap();
    let mut r = open_tcp(run.addr(), &format!("audio/{id}/audio"));
    r.read_header().unwrap();
    wait("a live-edge seek", || {
        run.rt.stats_json(&id).unwrap()["live_edge_seeks"]
            .as_u64()
            .unwrap_or(0)
            > 0
    });
    let seeks_before = run.rt.stats_json(&id).unwrap()["live_edge_seeks"]
        .as_u64()
        .unwrap();
    let deadline = Instant::now() + LIMIT;
    let mut flagged_jump = false;
    let mut last: Option<u64> = None;
    while !flagged_jump {
        assert!(Instant::now() < deadline, "a flagged jump after a seek");
        let (audio, _) = read_audio(&mut r, 1);
        let (idx, flags, _) = &audio[0];
        if let Some(prev) = last
            && *idx > prev + 960
        {
            assert!(
                flags.contains(RecordFlags::DISCONTINUITY),
                "a jump {prev} → {idx} is flagged"
            );
            flagged_jump = true;
        }
        last = Some(*idx);
    }
    let stats = run.rt.stats_json(&id).unwrap();
    assert!(stats["live_edge_seeks"].as_u64().unwrap() >= seeks_before);
    assert!(stats["skipped_samples"].as_u64().unwrap() > 0, "{stats}");
    run.finish();
}
