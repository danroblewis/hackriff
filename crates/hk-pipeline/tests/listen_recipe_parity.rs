//! **T-868 / ADR-0015 §12.9 stage 3 (LP-4): the parity harness — legacy Listen versus the
//! `analog-wfm` recipe, on the same IQ.** SIGNAL-062 (FM broadcast), end to end **through the
//! mock SDR device interface**.
//!
//! One pipeline replays the real `fm_100p8M_2p4M_l32g30a1_t1p5_5s` capture (101.3 MHz WFM with
//! RDS, 18 dB) behind [`MockSdrDriver`], looping, unpaced and lossless — channelised offline to
//! 480 kS/s around the blindly-found station first ([`channelised`] says why), so both chains
//! still see the same real samples. On that one ring, two
//! consumers run side by side on the **same** blindly-found channel:
//!
//! - **legacy**: the `listen` opener (`chains/listen.rs`: probe → T-070 refinement → `AudioDemod`),
//!   over TCP `open/listen?f_lo=&f_hi=`;
//! - **recipe**: `recipes/analog-wfm.recipe.json` (LP-3) started as an explicit pipeline, its
//!   `audio` output at `audio/<pipeline>/audio` and its RDS `messages` siblings beside it.
//!
//! What stage 3 compares (§12.9), each against the other or against an oracle — never against a
//! frequency looked up anywhere:
//!
//! 1. **Audio, sample-wise.** Both streams are laid on the one absolute capture-time axis (every
//!    record's `t`), and the normalised cross-correlation of the overlap, searched over ±100 ms of
//!    lag for the two chains' different filter delays, must be ≥ [`MIN_AUDIO_CORRELATION`]: the
//!    two chains deliver the same programme, not merely "some audio". The level envelopes (RMS
//!    per 100 ms, dB) must also track each other.
//! 2. **RDS.** The recipe's `station` output's PS strings and PI key match the `hk_demod::rds`
//!    oracle (`WfmDemod` with RDS, run over the same capture offline) — the PS the oracle
//!    completes most often, and its accepted PI.
//! 3. **Refinement.** The station's centre as each side refined it from its **own output**
//!    (legacy: the header's `audio.refinement`; recipe: the builtin `wfm-pilot` objective applied
//!    as a hot edit, T-870) agree within T-070's centre tolerance (250 Hz,
//!    `Termination::center_tolerance_hz`), and each is within 2 kHz of the hidden truth.
//! 4. **CPU**, in [`the_recipe_chain_costs_within_a_factor_of_the_legacy_chain`]: the recipe's
//!    DDC + graph (audio **and** RDS) against the legacy `AudioDemod` (audio only) on the same
//!    samples, one thread each. That assertion is a throughput bound, so it lives in the `timing`
//!    tier (`.config/nextest.toml`, `just timing`) and never gates a merge (docs/10 §3.6).
//!
//! **The recipe's refine objective.** LP-3's recipe declares `{node: crc, metric: error_rate}`,
//! which the runtime validates but does not run yet (T-870: only `{builtin}` runs). Legacy Listen
//! refines with the WFM pilot objective, so the harness swaps in `{builtin: "wfm-pilot"}` — the
//! objective §12.6 says the recipe declares — and compares like with like. Everything else is the
//! recipe file unchanged.
//!
//! **Blind.** The channel both sides are given is the strongest 200 kHz of a Welch spectrum of the
//! capture; the recording the mock SDR replays carries none of the fixture's annotations (the
//! truth: centre, PI, PS), which are read only by the assertions.

mod common;

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use common::TempDir;
use hk_api::{StreamRegistry, StreamServer, StreamServerConfig, Token};
use hk_core::{Discontinuity, MockEnd, MockOptions, MockSdrDriver, Pacing, ProvenanceHandle};
use hk_dsp::welch::{WelchConfig, welch};
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_model::sigmf::{Capture as SigmfCapture, Datatype, SigmfMeta};
use hk_model::{Provenance, SampleTime, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::recipes::runtime::{RecipeRuntime, Target, parse_recipe};
use hk_pipeline::{
    ListenSettings, Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory,
    replay_plan,
};
use hk_stream::record::parse_status_record;
use hk_stream::{OpenerRegistry, Record, RecordFlags, StreamReader};
use num_complex::Complex32;
use serde_json::{Value, json};

const TOKEN: &str = "t868-listen-parity-token-0123456789abcdef";
const FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const AUDIO_HZ: f64 = 48_000.0;
/// Capture time both audio streams must cover together.
const OVERLAP_S: f64 = 4.0;
/// Lag searched between the two chains' audio (their filter delays differ), s.
const MAX_LAG_S: f64 = 0.1;
/// The normalised cross-correlation the two chains' audio must reach at the best lag.
const MIN_AUDIO_CORRELATION: f64 = 0.9;
/// T-070's centre tolerance (`hk_demod::refine::Termination::center_tolerance_hz`).
const REFINE_TOLERANCE_HZ: f64 = 250.0;
/// The recipe's audio branch's CPU against the legacy audio chain's, on the same samples. The
/// whole recipe (audio **and** RDS) is measured and reported beside it, but not bounded: the
/// legacy chain decodes no RDS, so that ratio compares unlike work.
const CPU_FACTOR: f64 = 6.0;
const LIMIT: Duration = Duration::from_secs(240);

/// A bound on waiting for an EVENT (never an assertion about how long something took).
fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A recording both chains replay, its IQ, rate and centre, and the fixture's hidden truth.
struct Capture {
    /// The recording the mock SDR replays.
    meta: std::path::PathBuf,
    /// The provenance its samples were taken under (for offline DDCs, which read the input's
    /// rate and centre from it).
    prov: Provenance,
    iq: Vec<Complex32>,
    fs: f64,
    center_hz: f64,
    truth: hk_e2e::TruthItem,
    _dir: Option<TempDir>,
}

fn capture() -> Option<Capture> {
    let meta = common::real_fixture(FIXTURE)?;
    let fx = hk_e2e::Fixture::load(&meta).unwrap();
    let iq = fx
        .samples()
        .unwrap()
        .into_iter()
        .map(|s| Complex32::new(s.re, s.im))
        .collect();
    let truth = fx
        .of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .expect("the truth station");
    let center_hz = fx.meta.captures[0].frequency.unwrap();
    Some(Capture {
        prov: provenance(&meta),
        meta,
        iq,
        fs: fx.sample_rate,
        center_hz,
        truth,
        _dir: None,
    })
}

/// The rate [`channelised`] brings the capture to.
/// 2.4 MS/s ÷ 5, and itself ÷ 2 to the 240 kS/s MPX rate every WFM chain here runs at.
const CHANNEL_FS: f64 = 480e3;
/// Where the station sits in the channelised recording, from its centre. The copy has no DC spur
/// (the HackRF's lies 460 kHz away, filtered out), and the refinement's ±100 kHz search around
/// the ±100 kHz channel stays inside the ±240 kHz window.
const STATION_AT_HZ: f64 = 40e3;

/// The same real IQ, channelised offline to [`CHANNEL_FS`] around the blindly-found station and
/// written as a ci8 SigMF recording (no annotations: nothing to strip) that the mock SDR replays.
/// The station sits [`STATION_AT_HZ`] off the new centre. Why: at 2.4 Msps a debug build of the whole pipeline on a lossless
/// (capture-holding) source ran at 1/20–1/60 of real time on a loaded box, so legacy Listen's
/// one-second probe window outlasted its 20 s `probe-timeout` and each run cost minutes of gate.
/// Both chains still receive exactly the same samples, which is what parity needs.
fn channelised(cap: &Capture, offset_hz: f64, tag: &str) -> Capture {
    let dir = TempDir::new(tag);
    let rec = dir.0.join("rec");
    std::fs::create_dir_all(&rec).unwrap();
    let shift = offset_hz - STATION_AT_HZ;
    let prov = ProvenanceHandle::new(cap.prov.clone());
    let mut ddc = Ddc::new(
        DdcSpec::new(shift, 0.8 * CHANNEL_FS).with_output_rate(CHANNEL_FS),
        cap.fs,
    )
    .unwrap();
    let mut iq = Vec::with_capacity((cap.iq.len() as f64 * CHANNEL_FS / cap.fs) as usize + 1);
    let mut idx = 0u64;
    for (k, c) in cap.iq.chunks(1 << 16).enumerate() {
        let info = InputInfo {
            time: SampleTime {
                sample_index: idx,
                host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
            },
            discontinuity: if k == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
            provenance: &prov,
        };
        idx += c.len() as u64;
        iq.extend_from_slice(ddc.process(info, c).unwrap().samples);
    }
    // ci8 at the fixture's own scale (its samples read back as ±1 full scale).
    let q = |v: f32| (v * 128.0).round().clamp(-128.0, 127.0) as i8 as u8;
    let data: Vec<u8> = iq.iter().flat_map(|z| [q(z.re), q(z.im)]).collect();
    std::fs::write(rec.join("fm.sigmf-data"), data).unwrap();
    let iq: Vec<Complex32> = iq
        .iter()
        .map(|z| {
            let d = |v: f32| f32::from(q(v) as i8) / 128.0;
            Complex32::new(d(z.re), d(z.im))
        })
        .collect();
    let center_hz = cap.center_hz + shift;
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(CHANNEL_FS);
    meta.captures.push(SigmfCapture {
        sample_start: 0,
        frequency: Some(center_hz),
        datetime: Some("2026-09-13T11:10:22.378252Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = rec.join("fm.sigmf-meta");
    meta.write(&path).unwrap();
    let mut prov = cap.prov.clone();
    prov.tune.center_hz = center_hz;
    prov.tune.sample_rate_hz = CHANNEL_FS;
    Capture {
        meta: path,
        prov,
        iq,
        fs: CHANNEL_FS,
        center_hz,
        truth: cap.truth.clone(),
        _dir: Some(dir),
    }
}

/// The strongest 200 kHz of the capture's Welch spectrum, as a power centroid above the floor:
/// the channel offset from the capture centre, Hz. Blind — no annotation is read.
fn strongest_channel(iq: &[Complex32], fs: f64) -> f64 {
    let n = iq.len().min(4 << 20);
    let s = welch(
        &iq[..n],
        fs,
        0.0,
        &WelchConfig {
            holds: false,
            spectral_kurtosis: false,
            ..WelchConfig::new(4096)
        },
    )
    .unwrap();
    let bins = s.psd.len();
    let df = fs / bins as f64;
    let mut sorted = s.psd.clone();
    sorted.sort_by(f32::total_cmp);
    let n0 = f64::from(sorted[bins / 2]);
    let half = (100e3 / df).round() as usize;
    let band = |c: usize| -> f64 {
        s.psd[c - half..=c + half]
            .iter()
            .map(|&p| f64::from(p))
            .sum()
    };
    let mut c = (half..bins - half)
        .max_by(|&a, &b| band(a).total_cmp(&band(b)))
        .unwrap();
    for _ in 0..3 {
        let (mut w, mut m) = (0.0, 0.0);
        for i in c - half..=c + half {
            let p = (f64::from(s.psd[i]) - n0).max(0.0);
            w += p;
            m += p * i as f64;
        }
        c = ((m / w).round() as usize).clamp(half, bins - half - 1);
    }
    (c as f64 - (bins / 2) as f64) * df
}

fn provenance(meta: &Path) -> Provenance {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    serde_json::from_value(v["global"]["hackriff:provenance"].clone()).unwrap()
}

/// `recipes/analog-wfm.recipe.json` (LP-3) with its refinement objective set to the builtin
/// `wfm-pilot` (see the module docs), so both sides refine against the same objective.
fn analog_wfm_recipe() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../recipes/analog-wfm.recipe.json"
    );
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(doc["id"], "analog-wfm");
    doc["refine"] =
        json!({"objective": {"builtin": "wfm-pilot"}, "tune": ["center_hz", "bandwidth_hz"]});
    doc
}

struct Run {
    handle: Option<PipelineHandle>,
    streams: StreamRegistry,
    rt: Arc<RecipeRuntime>,
    tcp: StreamServer,
    /// Capture time of the mock's first sample, ns: sample `n` of the stream (any loop pass) is
    /// at `start_ns + n / fs`.
    start_ns: i64,
    _dir: TempDir,
}

impl Run {
    /// The recording (no truth in it) behind the mock SDR, looping, unpaced, lossless; the TCP
    /// stream server with the always-on registry (recipe outputs) and the `listen` opener.
    fn start(tag: &str, cap: &Capture) -> Self {
        let dir = TempDir::new(tag);
        // The channelised recording carries no annotations: the truth stays with the fixture.
        let driver = MockSdrDriver::new(
            &cap.meta,
            MockOptions {
                end: MockEnd::Loop,
                block_len: 16_384,
                pacing: Pacing::Unpaced,
                ..MockOptions::default()
            },
        )
        .unwrap();
        let source = driver.open_mock(&driver.default_request()).unwrap();
        let start_ns = source.start_time().as_unix_nanos();
        let info = SourceInfo {
            sample_rate_hz: cap.fs,
            center_hz: cap.center_hz,
            start_time: source.start_time(),
        };
        let mut cfg =
            PipelineConfig::new(&dir.0, replay_plan(cap.center_hz, cap.fs, info.start_time))
                .unwrap();
        cfg.source_class = window_class(cap.center_hz, cap.fs);
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
        handle.set_listen_settings(ListenSettings::default());
        let counters = handle.counters();
        let fs = cap.fs;
        wait("the first second of samples", || {
            counters.source.samples.load(Ordering::Relaxed) >= fs as u64
        });
        let rt = handle.recipe_runtime();
        let tcp = StreamServer::start(
            StreamServerConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                Token::from_config(TOKEN).unwrap(),
            ),
            streams.clone(),
            OpenerRegistry::new().with("listen", handle.listen_service()),
        )
        .unwrap();
        Self {
            handle: Some(handle),
            streams,
            rt,
            tcp,
            start_ns,
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

fn open_tcp(addr: SocketAddr, line: &str) -> StreamReader<TcpStream> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    s.write_all(format!("{line}\n").as_bytes()).unwrap();
    StreamReader::new(s)
}

/// One audio data record: capture time of its first sample (ns), flags, samples.
type Audio = (i64, RecordFlags, Vec<i16>);

/// Reads audio until both streams' first records are known (`firsts`, this stream's at `me`)
/// and a record passes the later of them by [`OVERLAP_S`] (plus a quarter for settling).
fn read_until(
    mut r: StreamReader<TcpStream>,
    firsts: &[AtomicI64; 2],
    me: usize,
) -> (Vec<Audio>, Vec<Value>) {
    let (mut audio, mut status) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + LIMIT;
    loop {
        assert!(Instant::now() < deadline, "audio records");
        match r.next_record().unwrap() {
            Some(Record::Binary(b)) => {
                let t = b.header.t.as_unix_nanos();
                let _ = firsts[me].compare_exchange(0, t, Ordering::SeqCst, Ordering::SeqCst);
                let samples = b
                    .payload
                    .chunks_exact(2)
                    .map(|x| i16::from_le_bytes([x[0], x[1]]))
                    .collect();
                audio.push((t, b.header.flags, samples));
                let (a, b) = (
                    firsts[0].load(Ordering::SeqCst),
                    firsts[1].load(Ordering::SeqCst),
                );
                if a > 0 && b > 0 && t >= a.max(b) + (OVERLAP_S * 1.25 * 1e9) as i64 {
                    return (audio, status);
                }
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
}

/// Lays `audio` onto the capture-time grid starting at `t0` (ns), `n` samples of 48 kHz; samples
/// no record covers stay `None` (squelch gaps, loop splices, before the stream began).
fn on_grid(audio: &[Audio], t0: i64, n: usize) -> Vec<Option<f32>> {
    let mut g = vec![None; n];
    for (t, _, s) in audio {
        let start = ((*t - t0) as f64 * 1e-9 * AUDIO_HZ).round() as i64;
        for (k, &v) in s.iter().enumerate() {
            let i = start + k as i64;
            if (0..n as i64).contains(&i) {
                g[i as usize] = Some(f32::from(v) / 32767.0);
            }
        }
    }
    g
}

/// Samples of the grid within `settle_s` after a record flagged `DISCONTINUITY` (a loop splice,
/// a squelch gap): each chain's filters and AGC restart there on their own schedule.
fn settling(audio: &[Audio], t0: i64, n: usize, settle_s: f64) -> Vec<bool> {
    let mut m = vec![false; n];
    for (t, flags, _) in audio {
        if !flags.contains(RecordFlags::DISCONTINUITY) {
            continue;
        }
        let start = ((*t - t0) as f64 * 1e-9 * AUDIO_HZ).round() as i64;
        let end = start + (settle_s * AUDIO_HZ) as i64;
        for i in start.max(0)..end.min(n as i64) {
            m[i as usize] = true;
        }
    }
    m
}

/// Coarse lag of `b` against `a`, samples: the best-correlated 10 ms RMS envelopes over
/// ±`max_lag_s` (cheap enough to search seconds, which a sample-wise search is not).
fn coarse_lag(a: &[Option<f32>], b: &[Option<f32>], max_lag_s: f64) -> (i64, f64) {
    let block = (0.01 * AUDIO_HZ) as usize;
    let env = |x: &[Option<f32>]| -> Vec<Option<f64>> {
        x.chunks(block)
            .map(|c| {
                let v: Vec<f64> = c.iter().flatten().map(|&s| f64::from(s).powi(2)).collect();
                (v.len() == block).then(|| (v.iter().sum::<f64>() / block as f64).sqrt())
            })
            .collect()
    };
    let (ea, eb) = (env(a), env(b));
    let max = (max_lag_s / 0.01) as i64;
    let mut best = (0i64, f64::MIN);
    for lag in -max..=max {
        let pairs: Vec<(f64, f64)> = ea
            .iter()
            .enumerate()
            .filter_map(|(i, x)| {
                let j = i as i64 + lag;
                if j < 0 {
                    return None;
                }
                Some(((*x)?, (*eb.get(j as usize)?)?))
            })
            .collect();
        if pairs.len() < 50 {
            continue;
        }
        let n = pairs.len() as f64;
        let (ma, mb) = (
            pairs.iter().map(|p| p.0).sum::<f64>() / n,
            pairs.iter().map(|p| p.1).sum::<f64>() / n,
        );
        let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
        for (x, y) in &pairs {
            ab += (x - ma) * (y - mb);
            aa += (x - ma).powi(2);
            bb += (y - mb).powi(2);
        }
        let c = ab / (aa * bb).sqrt().max(1e-30);
        if c > best.1 {
            best = (lag, c);
        }
    }
    (best.0 * block as i64, best.1)
}

/// Best normalised cross-correlation of `a` against `b` shifted by `lag` ∈ ±`max_lag` samples,
/// over the samples both hold; returns `(correlation, lag, overlapping samples)`.
fn best_correlation(a: &[Option<f32>], b: &[Option<f32>], max_lag: i64) -> (f64, i64, usize) {
    let mut best = (f64::MIN, 0, 0);
    for lag in -max_lag..=max_lag {
        let (mut ab, mut aa, mut bb, mut n) = (0.0f64, 0.0f64, 0.0f64, 0usize);
        for (i, x) in a.iter().enumerate() {
            let j = i as i64 + lag;
            if j < 0 || j >= b.len() as i64 {
                continue;
            }
            if let (Some(x), Some(y)) = (x, b[j as usize]) {
                let (x, y) = (f64::from(*x), f64::from(y));
                ab += x * y;
                aa += x * x;
                bb += y * y;
                n += 1;
            }
        }
        if n > 0 && aa > 0.0 && bb > 0.0 {
            let c = ab / (aa * bb).sqrt();
            if c > best.0 {
                best = (c, lag, n);
            }
        }
    }
    best
}

/// RMS per 100 ms block of `a` and of `b` shifted by `lag`, dB, for blocks both grids fully hold
/// and no `skip` sample touches.
fn envelopes(a: &[Option<f32>], b: &[Option<f32>], lag: i64, skip: &[bool]) -> Vec<(f64, f64)> {
    let block = (0.1 * AUDIO_HZ) as usize;
    let mut out = Vec::new();
    let mut i = 0;
    while i + block <= a.len() {
        let (mut pa, mut pb, mut n) = (0.0f64, 0.0f64, 0usize);
        for k in i..i + block {
            if skip[k] {
                break;
            }
            let j = k as i64 + lag;
            if let (Some(x), Some(Some(y))) = (a[k], b.get(j.max(0) as usize).filter(|_| j >= 0)) {
                pa += f64::from(x).powi(2);
                pb += f64::from(*y).powi(2);
                n += 1;
            }
        }
        if n == block {
            let db = |p: f64| 10.0 * (p / n as f64).max(1e-12).log10();
            out.push((db(pa), db(pb)));
        }
        i += block;
    }
    out
}

/// The `hk_demod::rds` oracle over the capture's channel at `offset_hz`: the station's accepted PI
/// and its complete PS frames, most frequent first.
fn rds_oracle(cap: &Capture, offset_hz: f64) -> hk_demod::rds::RdsReport {
    let prov = ProvenanceHandle::new(cap.prov.clone());
    let mut ddc = Ddc::new(
        DdcSpec::new(offset_hz, 200e3).with_output_rate(hk_demod::wfm::MPX_RATE_HZ),
        cap.fs,
    )
    .unwrap();
    let mut wfm =
        hk_demod::wfm::WfmDemod::new(Default::default(), hk_demod::wfm::MPX_RATE_HZ).unwrap();
    let mut idx = 0u64;
    for (k, c) in cap.iq.chunks(1 << 16).enumerate() {
        let info = InputInfo {
            time: SampleTime {
                sample_index: idx,
                host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
            },
            discontinuity: if k == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
            provenance: &prov,
        };
        idx += c.len() as u64;
        let block = ddc.process(info, c).unwrap();
        wfm.process(block.samples);
    }
    wfm.report().rds.expect("RDS is enabled by default")
}

/// The oracle's audio for one pass of the capture (48 kHz; sample `i` ≈ capture sample
/// `i × fs / 48000` of the pass, give or take the demodulator's few-ms filter delay).
fn oracle_audio(cap: &Capture, offset_hz: f64) -> Vec<f32> {
    let prov = ProvenanceHandle::new(cap.prov.clone());
    let mut ddc = Ddc::new(
        DdcSpec::new(offset_hz, 200e3).with_output_rate(hk_demod::wfm::MPX_RATE_HZ),
        cap.fs,
    )
    .unwrap();
    let cfg = hk_demod::wfm::WfmConfig {
        rds: None,
        ..Default::default()
    };
    let mut wfm = hk_demod::wfm::WfmDemod::new(cfg, hk_demod::wfm::MPX_RATE_HZ).unwrap();
    let mut out = Vec::new();
    let mut idx = 0u64;
    for (k, c) in cap.iq.chunks(1 << 16).enumerate() {
        let info = InputInfo {
            time: SampleTime {
                sample_index: idx,
                host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
            },
            discontinuity: if k == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
            provenance: &prov,
        };
        idx += c.len() as u64;
        wfm.process(ddc.process(info, c).unwrap().samples);
        out.extend(wfm.take_audio());
    }
    out
}

/// The PS text and PI of a recipe `station` message (`frame_model: rds-ps`).
fn ps_of(v: &Value) -> Option<(String, String)> {
    (v["frame_model"] == "rds-ps").then_some(())?;
    Some((
        v["content"]["text"].as_str()?.to_owned(),
        v["identity"]["value"].as_str()?.to_owned(),
    ))
}

/// Reads `station` messages until `done` says the PS strings seen so far suffice.
fn read_station(mut r: StreamReader<TcpStream>, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let mut msgs = Vec::new();
    let deadline = Instant::now() + LIMIT;
    while !done(&msgs) {
        assert!(Instant::now() < deadline, "station messages: {msgs:?}");
        match r.next_record().unwrap() {
            Some(Record::Message(m)) => msgs.push(m.value),
            Some(Record::Unknown(b)) => {
                if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                    msgs.push(v);
                }
            }
            Some(_) => {}
            None => panic!("the station stream ended"),
        }
    }
    msgs
}

#[test]
fn legacy_listen_and_the_analog_wfm_recipe_agree_on_the_same_iq() {
    let Some(fixture) = capture() else {
        return;
    };
    let cap = channelised(
        &fixture,
        strongest_channel(&fixture.iq, fixture.fs),
        "t868-channel",
    );
    drop(fixture);
    let truth_hz = cap.truth.value["center_hz"].as_f64().unwrap();
    let offset = strongest_channel(&cap.iq, cap.fs);
    let chan = cap.center_hz + offset;
    let (f_lo, f_hi) = (chan - 100e3, chan + 100e3);

    // The oracle: hk_demod's WFM + RDS over the same samples, offline.
    let oracle = rds_oracle(&cap, offset);
    let rms =
        (cap.iq.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / cap.iq.len() as f64).sqrt();
    eprintln!(
        "T-868 channel: offset {offset:.0} Hz, {} samples at {} S/s, rms {rms:.4}",
        cap.iq.len(),
        cap.fs
    );
    let oracle_pi = oracle
        .pi
        .unwrap_or_else(|| panic!("the oracle accepts a PI: {oracle:?}"))
        .pi;
    let oracle_ps = oracle.ps().expect("the oracle completes a PS").to_owned();
    // The oracle itself is checked against the hidden truth, so a parity pass cannot hide a
    // shared regression.
    assert_eq!(
        u64::from(oracle_pi),
        cap.truth.value["rds"]["pi"].as_u64().unwrap()
    );
    assert_eq!(oracle_ps, cap.truth.value["rds"]["ps"].as_str().unwrap());

    let run = Run::start("t868-parity", &cap);

    // The legacy side first: the listen opener on the band. Its probe collects its window before
    // the recipe pipeline starts, so on this lossless (capture-holding) source the two do not
    // compete for the probe's samples (under load that ran into the 20 s `probe-timeout`).
    let mut s = TcpStream::connect(run.addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    s.write_all(format!("open/listen?token={TOKEN}&f_lo={f_lo}&f_hi={f_hi}\n").as_bytes())
        .unwrap();
    // What the opener answered first (a header, or a refusal), kept for the failure message.
    let mut first = [0u8; 512];
    let n = s.peek(&mut first).unwrap_or(0);
    let first = String::from_utf8_lossy(&first[..n]).into_owned();
    let mut legacy = StreamReader::new(s);
    let lh = match legacy.read_header() {
        Ok(h) => h.clone(),
        Err(e) => panic!(
            "legacy listen header: {e:?}; the opener sent {first:?}; counters {}",
            run.handle.as_ref().unwrap().counters().to_json()
        ),
    };

    // The recipe side: an explicit pipeline on the same band, its audio and RDS outputs.
    let id = run
        .rt
        .start(
            parse_recipe(analog_wfm_recipe()).unwrap(),
            Target::Band { f_lo, f_hi },
        )
        .unwrap();
    let p = run.rt.pipeline_json(&id).unwrap();
    let stream_of = |out: &str| -> String {
        p["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == out)
            .unwrap_or_else(|| panic!("output {out}: {p}"))["stream_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let mut recipe_audio = open_tcp(run.addr(), &format!("{}?token={TOKEN}", stream_of("audio")));
    let station = open_tcp(
        run.addr(),
        &format!("{}?token={TOKEN}", stream_of("station")),
    );
    let rh = recipe_audio.read_header().unwrap().clone();
    let la = lh.audio.as_ref().expect("legacy audio profile");
    let ra = rh.audio.as_ref().expect("recipe audio profile");
    assert_eq!(la.mode, "wfm", "the legacy probe chose WFM");
    assert_eq!(ra.mode, "wfm");
    assert_eq!(
        (la.channels, la.frame_samples),
        (ra.channels, ra.frame_samples)
    );
    assert_eq!(lh.sample_rate_hz, rh.sample_rate_hz);
    assert_eq!(lh.datatype, rh.datatype);

    // 1. Audio: both streams read concurrently (each consumer's queue drops rather than blocks,
    //    so one must not wait on the other), each until it passes the later of the two streams'
    //    first records by OVERLAP_S of capture time — read off the records, never the wall clock.
    let firsts = Arc::new([AtomicI64::new(0), AtomicI64::new(0)]);
    let (f_legacy, f_recipe) = (firsts.clone(), firsts.clone());
    let lt = std::thread::spawn(move || read_until(legacy, &f_legacy, 0));
    let rt_reader = std::thread::spawn(move || read_until(recipe_audio, &f_recipe, 1));
    let (legacy_audio, legacy_status) = lt.join().unwrap();
    let (recipe_audio, _) = rt_reader.join().unwrap();

    let t0 = legacy_audio[0].0.max(recipe_audio[0].0);
    let t1 = legacy_audio
        .last()
        .unwrap()
        .0
        .min(recipe_audio.last().unwrap().0);
    // Skip the first half second of the later stream (squelch, AGC and filters settling).
    let t0 = t0 + 500_000_000;
    assert!(
        t1 - t0 >= (OVERLAP_S * 0.5 * 1e9) as i64,
        "the streams overlap by {} s of capture time",
        (t1 - t0) as f64 * 1e-9
    );
    let n = ((t1 - t0) as f64 * 1e-9 * AUDIO_HZ) as usize;
    let a = on_grid(&legacy_audio, t0, n);
    let b = on_grid(&recipe_audio, t0, n);
    let (coarse, coarse_c) = coarse_lag(&a, &b, 3.0);
    eprintln!(
        "T-868 coarse: envelope lag {:.1} ms (correlation {coarse_c:.3})",
        coarse as f64 / AUDIO_HZ * 1e3
    );
    // Each stream against the oracle laid on the mock's own clock (periodic: the mock loops).
    let oracle = oracle_audio(&cap, offset);
    let pass = oracle.len() as i64;
    let base = ((t0 - run.start_ns) as f64 * 1e-9 * AUDIO_HZ).round() as i64;
    let o: Vec<Option<f32>> = (0..n as i64)
        .map(|i| Some(oracle[(base + i).rem_euclid(pass) as usize]))
        .collect();
    for (who, x) in [("legacy", &a), ("recipe", &b)] {
        let (l, c) = coarse_lag(&o, x, 3.0);
        let ms = l as f64 / AUDIO_HZ * 1e3;
        eprintln!(
            "T-868 {who} vs oracle on the capture clock: lag {ms:.1} ms (envelope correlation {c:.3})"
        );
        // Every record carries absolute capture time (CLAUDE.md, one shared time axis): the
        // audio at `t` is the programme captured at `t`, to the 10 ms envelope resolution plus
        // filter delay. Legacy Listen stamped its records one probe window (1 s) early (T-868).
        assert!(
            c >= 0.8,
            "{who} audio follows the programme (envelope correlation {c:.3})"
        );
        assert!(
            ms.abs() <= 20.0,
            "{who} audio is stamped {ms:.0} ms off the capture time it was demodulated from"
        );
    }
    let (corr, lag, both) = best_correlation(&a, &b, (MAX_LAG_S * AUDIO_HZ) as i64);
    eprintln!(
        "T-868 audio: correlation {corr:.4} at lag {lag} samples ({:.1} ms) over {both} samples",
        lag as f64 / AUDIO_HZ * 1e3
    );
    assert!(
        both as f64 >= OVERLAP_S * 0.4 * AUDIO_HZ,
        "the chains' audio overlaps on {both} samples"
    );
    assert!(
        corr >= MIN_AUDIO_CORRELATION,
        "legacy and recipe audio correlate {corr:.4} at lag {lag} (need ≥ {MIN_AUDIO_CORRELATION})"
    );
    // Level/SNR envelope: 100 ms RMS, each normalised by its own mean (the two AGCs may settle at
    // different absolute levels), tracks within 3 dB.
    let skip: Vec<bool> = settling(&legacy_audio, t0, n, 0.5)
        .into_iter()
        .zip(settling(&recipe_audio, t0, n, 0.5))
        .map(|(x, y)| x || y)
        .collect();
    let env = envelopes(&a, &b, lag, &skip);
    assert!(env.len() >= 10, "{} common 100 ms blocks", env.len());
    let mean = |f: fn(&(f64, f64)) -> f64| env.iter().map(f).sum::<f64>() / env.len() as f64;
    let (ma, mb) = (mean(|e| e.0), mean(|e| e.1));
    let err: Vec<f64> = env.iter().map(|(x, y)| (x - ma) - (y - mb)).collect();
    let worst = err.iter().fold(0.0f64, |m, e| m.max(e.abs()));
    let rms = (err.iter().map(|e| e * e).sum::<f64>() / err.len() as f64).sqrt();
    eprintln!(
        "T-868 envelope: mean levels {ma:.1} / {mb:.1} dBFS, tracking error rms {rms:.2} dB, worst {worst:.2} dB"
    );
    // The two AGCs differ in target and time constants, so a single block may stray; the
    // envelope as a whole must track.
    assert!(rms <= 1.5, "level envelopes diverge: rms {rms:.2} dB");
    assert!(
        worst <= 6.0,
        "level envelopes diverge by {worst:.2} dB in one 100 ms block"
    );

    // 2. RDS: the recipe's station output against the oracle.
    let msgs = read_station(station, |m| {
        m.iter().filter_map(ps_of).any(|(ps, _)| ps == oracle_ps)
    });
    let station_ps: Vec<(String, String)> = msgs.iter().filter_map(ps_of).collect();
    eprintln!("T-868 RDS: recipe PS {station_ps:?}; oracle PS {oracle_ps:?}, PI {oracle_pi:04X}");
    let (_, pi) = station_ps
        .iter()
        .find(|(ps, _)| *ps == oracle_ps)
        .expect("the recipe completed the oracle's PS");
    assert_eq!(
        pi.to_uppercase(),
        format!("{oracle_pi:04X}"),
        "the recipe's PS carries the oracle's PI"
    );
    for (ps, pi) in &station_ps {
        assert_eq!(
            pi.to_uppercase(),
            format!("{oracle_pi:04X}"),
            "every PS the recipe completed carries the station's PI ({ps:?})"
        );
    }

    // 3. Refinement: each side refined from its own output; both agree within T-070's tolerance.
    let legacy_rf = la
        .refinement
        .as_ref()
        .expect("legacy Listen refined the channel");
    let legacy_hz = legacy_rf.center_hz;
    wait("the recipe's refinement", || {
        run.rt.pipeline_json(&id).unwrap()["refinement"]["current"]["center_hz"]
            .as_f64()
            .is_some()
    });
    let rf = run.rt.pipeline_json(&id).unwrap()["refinement"].clone();
    let recipe_hz = rf["current"]["center_hz"].as_f64().unwrap();
    eprintln!(
        "T-868 refine: legacy {legacy_hz:.1} Hz, recipe {recipe_hz:.1} Hz, truth {truth_hz:.1} Hz"
    );
    assert_eq!(rf["current"]["locked"], true, "{rf}");
    assert!(
        (legacy_hz - recipe_hz).abs() <= REFINE_TOLERANCE_HZ,
        "legacy refined {legacy_hz} Hz, recipe {recipe_hz} Hz"
    );
    for (who, hz) in [("legacy", legacy_hz), ("recipe", recipe_hz)] {
        assert!(
            (hz - truth_hz).abs() <= 2_000.0,
            "{who} refined {hz} Hz; the station is at {truth_hz} Hz"
        );
    }
    assert!(
        !legacy_status.is_empty(),
        "the legacy stream carried status records"
    );
    run.finish();
}

/// `analog_wfm_recipe()` cut to its audio branch (`fm → sq → de → gain → out`, the `audio`
/// output): the same work the legacy chain does.
fn analog_wfm_audio_only() -> Value {
    let mut doc = analog_wfm_recipe();
    let keep = ["fm", "sq", "de", "gain", "out"];
    let nodes: Vec<Value> = doc["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| keep.contains(&n["id"].as_str().unwrap()))
        .cloned()
        .collect();
    assert_eq!(nodes.len(), keep.len(), "the recipe's audio branch");
    doc["nodes"] = Value::Array(nodes);
    doc["outputs"] = json!([doc["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "audio")
        .unwrap()]);
    doc.as_object_mut().unwrap().remove("field_maps");
    doc
}

/// **Timing tier** (`just timing`): the recipe's audio branch (DDC → `fm_demod` → `squelch` →
/// `deemphasis` → `agc` → `audio_out`) costs within [`CPU_FACTOR`] of the legacy chain
/// (`AudioDemod`: DDC → WFM audio) on the same samples, each on one thread, best of three; the
/// whole recipe (audio + RDS) is reported beside it. A throughput bound, so it never gates a
/// merge.
#[test]
fn the_recipe_chain_costs_within_a_factor_of_the_legacy_chain() {
    use hk_blocks::{ChunkFlags, ChunkMeta, Input, PortInfo, PortSlice, Registry};
    use hk_demod::audio::{AudioConfig, AudioDemod, AudioPlan};
    use hk_demod::mode::AnalogMode;
    use hk_pipeline::recipes::graph::Graph;
    use hk_recipe::PortType;

    let Some(cap) = capture() else {
        return;
    };
    let offset = strongest_channel(&cap.iq, cap.fs);
    let prov = ProvenanceHandle::new(cap.prov.clone());
    const CHUNK: usize = 1 << 16;
    let info_at = |k: usize, idx: u64| InputInfo {
        time: SampleTime {
            sample_index: idx,
            host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        },
        discontinuity: if k == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        },
        dropped_before: 0,
        provenance: &prov,
    };

    let legacy = || {
        let plan = AudioPlan {
            mode: AnalogMode::Wfm,
            sideband: None,
            channel_center_hz: cap.center_hz + offset,
            channel_bandwidth_hz: 200e3,
            noise_power: None,
            agc: true,
            deemphasis_s: Some(75e-6),
        };
        let mut d = AudioDemod::new(plan, AudioConfig::default(), cap.fs, cap.center_hz).unwrap();
        let start = Instant::now();
        let mut idx = 0u64;
        let mut produced = 0usize;
        for (k, c) in cap.iq.chunks(CHUNK).enumerate() {
            d.process(info_at(k, idx), c).unwrap();
            idx += c.len() as u64;
            produced += d.take_audio().len();
        }
        assert!(produced > 0);
        start.elapsed()
    };
    let recipe = |doc: Value| {
        let recipe = Arc::new(parse_recipe(doc).unwrap());
        let rate = recipe.input.sample_rate_hz.unwrap();
        let port = PortInfo {
            ty: PortType::Iq,
            rate_hz: rate,
            max_items: (CHUNK as f64 * rate / cap.fs).ceil() as usize + 1024,
            hold_items: 0,
        };
        let (mut g, _) = Graph::build(recipe, &Registry::builtin(), port).unwrap();
        let mut ddc = Ddc::new(DdcSpec::new(offset, 200e3).with_output_rate(rate), cap.fs).unwrap();
        let start = Instant::now();
        let (mut idx, mut items) = (0u64, 0u64);
        for (k, c) in cap.iq.chunks(CHUNK).enumerate() {
            let block = ddc.process(info_at(k, idx), c).unwrap();
            let meta = ChunkMeta {
                index: items,
                source_index: block.header.time.source_index as f64,
                source_per_item: block.header.time.source_per_output as f64,
                rate_hz: rate,
                channel: 0,
                flags: if k == 0 {
                    ChunkFlags::DISCONTINUITY
                } else {
                    ChunkFlags::NONE
                },
            };
            items += block.samples.len() as u64;
            idx += c.len() as u64;
            g.process(Input {
                meta,
                data: PortSlice::Iq(block.samples),
            })
            .unwrap();
        }
        start.elapsed()
    };
    let best = |f: &dyn Fn() -> Duration| (0..3).map(|_| f()).min().unwrap();
    let l = best(&legacy);
    let r = best(&|| recipe(analog_wfm_audio_only()));
    let whole = best(&|| recipe(analog_wfm_recipe()));
    let ratio = r.as_secs_f64() / l.as_secs_f64();
    let secs = cap.iq.len() as f64 / cap.fs;
    eprintln!(
        "T-868 CPU over {secs:.1} s of IQ: legacy audio {:.3} s, recipe audio {:.3} s \
         (ratio {ratio:.2}), whole recipe with RDS {:.3} s (ratio {:.2})",
        l.as_secs_f64(),
        r.as_secs_f64(),
        whole.as_secs_f64(),
        whole.as_secs_f64() / l.as_secs_f64()
    );
    assert!(
        ratio <= CPU_FACTOR,
        "the recipe's audio branch costs {ratio:.2}× the legacy chain (agreed ≤ {CPU_FACTOR}×)"
    );
}
