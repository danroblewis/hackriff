//! T-897 (docs/23 §10.6 rule 2): traced paths — a chirp, a sawtooth sweep and a frequency-hop
//! sequence — found **blind, through the mock SDR device interface**, and served by
//! `GET /api/paths` exactly as the route answers (`hk_api::paths::answer` over the run's own
//! repository).
//!
//! The scene is synthesised here and replayed through `MockSdrDriver`; the pipeline is told
//! nothing but the tuning the device reports. The **hidden truth** — each emitter's frequency as a
//! function of time — stays in this file and is read only to assert:
//!
//! | species | truth | must draw |
//! |---|---|---|
//! | chirp | −200 → −110 kHz over 0.3–3.3 s (30 kHz/s) | one `chirp` on its line, end to end |
//! | sawtooth | 3 ramps +20 → +99 kHz, 2.2 s each (36 kHz/s) | one `sweep` of 3 ramps on its ramps |
//! | hop net | 40 contiguous 34 ms dwells over +130/150/170/190 kHz from 4.0 s | one `hop` on the staircase |
//! | carrier | −50 kHz, the whole recording | **nothing** |
//! | stray bursts | 12 ms at −90 / −30 kHz | **nothing** |
//!
//! Frequencies are offsets from the 100 MHz centre; times from the recording's first sample.
mod common;

use common::*;
use hk_api::paths::{PathsQuery, answer};
use hk_core::{MockOptions, MockSdrDriver, Pacing};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{FreqRange, Region, TimeRange, Timestamp};
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, open_mock_replay};
use serde_json::Value;

const FS: f64 = 500_000.0;
const CENTER_HZ: f64 = 100.0e6;
const SECS: f64 = 8.0;

// ---- The hidden truth (offsets from CENTER_HZ, seconds from the first sample). ----

const CHIRP_T0: f64 = 0.3;
const CHIRP_LEN: f64 = 3.0;
const CHIRP_F0: f64 = -200e3;
const CHIRP_RATE: f64 = 30e3;
const SAW_T0: f64 = 0.3;
const SAW_RAMP: f64 = 2.2;
const SAW_RAMPS: usize = 3;
const SAW_F0: f64 = 20e3;
const SAW_RATE: f64 = 36e3;
const HOP_T0: f64 = 4.0;
const HOP_DWELL: f64 = 0.034;
const HOP_COUNT: usize = 40;
const HOP_CHANNELS: [f64; 4] = [130e3, 150e3, 170e3, 190e3];
const CARRIER: f64 = -50e3;
const STRAYS: [(f64, f64); 6] = [
    (1.0, -90e3),
    (2.1, -30e3),
    (3.7, -90e3),
    (5.0, -30e3),
    (6.2, -90e3),
    (7.3, -30e3),
];

/// The hop net's channel sequence: never the same channel twice running.
fn hop_order() -> Vec<f64> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut prev = 0usize;
    (0..HOP_COUNT)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let ch = (prev + 1 + (state % 3) as usize) % 4;
            prev = ch;
            HOP_CHANNELS[ch]
        })
        .collect()
}

struct Tone {
    t0: f64,
    len: f64,
    f: Box<dyn Fn(f64) -> f64>,
    amp: f64,
}

fn write(dir: &std::path::Path, tones: &[Tone]) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (FS * SECS) as usize;
    let mut re = vec![0f64; n];
    let mut im = vec![0f64; n];
    let ramp = (0.002 * FS) as usize;
    for t in tones {
        let s0 = (t.t0 * FS).round() as usize;
        let len = (t.len * FS).round() as usize;
        let mut ph = 0.0f64;
        for i in 0..len {
            let edge = i.min(len - 1 - i);
            let w = if edge < ramp {
                0.5 - 0.5 * (std::f64::consts::PI * edge as f64 / ramp as f64).cos()
            } else {
                1.0
            };
            let f = (t.f)(i as f64 / FS);
            ph += 2.0 * std::f64::consts::PI * f / FS;
            re[s0 + i] += t.amp * w * ph.cos();
            im[s0 + i] += t.amp * w * ph.sin();
        }
    }
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        data.push((re[i] + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
        data.push((im[i] + noise()).round().clamp(-128.0, 127.0) as i8 as u8);
    }
    std::fs::write(dir.join("scene.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER_HZ),
        datetime: Some("2026-09-24T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("scene.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

fn scene() -> Vec<Tone> {
    let mut v = vec![
        Tone {
            t0: 0.0,
            len: SECS,
            f: Box::new(|_| CARRIER),
            amp: 30.0,
        },
        Tone {
            t0: CHIRP_T0,
            len: CHIRP_LEN,
            f: Box::new(|t| CHIRP_F0 + CHIRP_RATE * t),
            amp: 50.0,
        },
    ];
    for k in 0..SAW_RAMPS {
        v.push(Tone {
            t0: SAW_T0 + SAW_RAMP * k as f64,
            len: SAW_RAMP,
            f: Box::new(|t| SAW_F0 + SAW_RATE * t),
            amp: 50.0,
        });
    }
    for (k, f) in hop_order().into_iter().enumerate() {
        v.push(Tone {
            t0: HOP_T0 + HOP_DWELL * k as f64,
            len: HOP_DWELL,
            f: Box::new(move |_| f),
            amp: 50.0,
        });
    }
    for (t, f) in STRAYS {
        v.push(Tone {
            t0: t,
            len: 0.012,
            f: Box::new(move |_| f),
            amp: 50.0,
        });
    }
    v
}

// ---- A run through the mock SDR device ----

struct Run {
    dir: TempDir,
    start_ns: i64,
}

fn run() -> Run {
    let dir = TempDir::new("t897-paths");
    let rec = write(&dir.0.join("src"), &scene());
    let reference = open_mock_replay(&rec, Pacing::Unpaced, hk_core::MockEnd::Stop).unwrap();
    let driver = MockSdrDriver::new(
        &rec,
        MockOptions {
            block_len: hk_pipeline::replay_block_len(FS),
            pacing: Pacing::Unpaced,
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let info = SourceInfo {
        sample_rate_hz: FS,
        center_hz: CENTER_HZ,
        start_time: source.start_time(),
    };
    let plan = hk_pipeline::replay_plan(CENTER_HZ, FS, info.start_time);
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan).unwrap();
    cfg.source_class = reference.class;
    cfg.lossless = true;
    drop(reference);
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let (summary, stopped) = wait_guarded(handle, std::time::Duration::from_secs(120));
    assert!(!stopped, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    Run {
        dir,
        start_ns: info.start_time.as_unix_nanos(),
    }
}

impl Run {
    /// `GET /api/paths` over `[t0, t1]` s × the whole capture, as the route answers it.
    fn paths(&self, t0: f64, t1: f64) -> Value {
        let at = |s: f64| Timestamp::from_unix_nanos(self.start_ns + (s * 1e9).round() as i64);
        let q = PathsQuery {
            window: Region::new(
                FreqRange::new(CENTER_HZ - FS / 2.0, CENTER_HZ + FS / 2.0),
                TimeRange::new(at(t0), at(t1)),
            ),
            kind: None,
            limit: 200,
        };
        answer(&repo(&self.dir.0.join("data")), &q).unwrap()
    }

    /// A vertex as (seconds from the first sample, offset from the centre).
    fn vertex(&self, v: &Value) -> (f64, f64) {
        (
            v["t_s"].as_f64().unwrap() - self.start_ns as f64 / 1e9,
            v["f_hz"].as_f64().unwrap() - CENTER_HZ,
        )
    }
}

fn of_kind<'a>(paths: &'a [Value], kind: &str) -> Vec<&'a Value> {
    paths.iter().filter(|p| p["kind"] == kind).collect()
}

/// A traced path's vertices lie on the truth. Its end vertices were carried along the measured
/// slope to the box edge, so the whole route — not only the measured centres — is held to it.
const ON_LINE_HZ: f64 = 2_500.0;

#[test]
fn a_chirp_a_sweep_and_a_hop_sequence_are_traced_blind_and_nothing_else_is() {
    let r = run();
    let v = r.paths(0.0, SECS);
    let paths = v["paths"].as_array().unwrap();
    for p in paths {
        let vs: Vec<(f64, f64)> = p["vertices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| r.vertex(x))
            .collect();
        eprintln!(
            "{} ramps={} hops={} rate={} {:?}",
            p["kind"], p["ramps"], p["hops"], p["rate_hz_per_s"], vs
        );
    }
    assert_eq!(v["detections_truncated"], false, "{v}");
    assert_eq!(v["total"], 3, "exactly the three routes in the scene: {v}");

    // -- The chirp: one, on its line, over its whole time extent. --
    let chirps = of_kind(paths, "chirp");
    assert_eq!(chirps.len(), 1, "{v}");
    let c = chirps[0];
    let truth = |t: f64| CHIRP_F0 + CHIRP_RATE * (t - CHIRP_T0);
    let cv: Vec<(f64, f64)> = c["vertices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| r.vertex(x))
        .collect();
    assert!(cv.len() >= 4, "a ladder, not a hull: {cv:?}");
    for &(t, f) in &cv {
        assert!(
            (f - truth(t)).abs() <= ON_LINE_HZ,
            "chirp vertex ({t:.3} s, {f:.0} Hz) is {:.0} Hz off its truth line",
            f - truth(t)
        );
    }
    let (first, last) = (cv[0].0, cv[cv.len() - 1].0);
    assert!(
        first - CHIRP_T0 < 0.05 && CHIRP_T0 + CHIRP_LEN - last < 0.05,
        "the route spans the chirp's time extent: {first:.3}..{last:.3}"
    );
    let rate = c["rate_hz_per_s"].as_f64().unwrap();
    assert!((rate / CHIRP_RATE - 1.0).abs() < 0.03, "rate {rate}");

    // -- The sawtooth: one sweep of three ramps, each vertex on the ramp its detection is in. --
    let sweeps = of_kind(paths, "sweep");
    assert_eq!(sweeps.len(), 1, "{v}");
    let s = sweeps[0];
    assert_eq!(s["ramps"], SAW_RAMPS, "{s}");
    let det_start: std::collections::HashMap<String, f64> = repo(&r.dir.0.join("data"))
        .detections_in_region(&Region::new(
            FreqRange::new(0.0, 7e9),
            TimeRange::new(
                Timestamp::from_unix_nanos(0),
                Timestamp::from_unix_nanos(i64::MAX / 2),
            ),
        ))
        .unwrap()
        .into_iter()
        .map(|d| {
            (
                d.id.to_string(),
                (d.time.start.as_unix_nanos() - r.start_ns) as f64 / 1e9,
            )
        })
        .collect();
    for x in s["vertices"].as_array().unwrap() {
        let (t, f) = r.vertex(x);
        // Which ramp: the one the vertex's own detection started in (a flyback instant is both the
        // top of one ramp and the bottom of the next).
        let d0 = det_start[x["detection"].as_str().unwrap()];
        let k = ((d0 - SAW_T0 + 0.05) / SAW_RAMP)
            .floor()
            .clamp(0.0, (SAW_RAMPS - 1) as f64);
        let truth = SAW_F0 + SAW_RATE * (t - SAW_T0 - SAW_RAMP * k);
        assert!(
            (f - truth).abs() <= ON_LINE_HZ,
            "sweep vertex ({t:.3}, {f:.0}) vs ramp {k} {truth:.0}"
        );
    }
    let rate = s["rate_hz_per_s"].as_f64().unwrap();
    assert!((rate / SAW_RATE - 1.0).abs() < 0.05, "rate {rate}");

    // -- The hop net: one sequence, every vertex on the staircase the net actually took. --
    let hops = of_kind(paths, "hop");
    assert_eq!(hops.len(), 1, "{v}");
    let h = hops[0];
    let order = hop_order();
    let n = h["hops"].as_u64().unwrap() as usize;
    assert!(
        n >= HOP_COUNT * 9 / 10,
        "{n} of {HOP_COUNT} dwells traced: {h}"
    );
    let mut chans: Vec<f64> = h["channels_hz"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() - CENTER_HZ)
        .collect();
    chans.sort_by(f64::total_cmp);
    assert_eq!(chans.len(), 4, "{chans:?}");
    for (c, t) in chans.iter().zip(HOP_CHANNELS) {
        assert!((c - t).abs() < 1_000.0, "{chans:?}");
    }
    for x in h["vertices"].as_array().unwrap() {
        let (t, f) = r.vertex(x);
        let on_staircase = order.iter().enumerate().any(|(k, &ch)| {
            let t0 = HOP_T0 + HOP_DWELL * k as f64;
            (f - ch).abs() < 1_000.0 && t >= t0 - 0.025 && t <= t0 + HOP_DWELL + 0.025
        });
        assert!(
            on_staircase,
            "hop vertex ({t:.3} s, {f:.0} Hz) is not where the net was"
        );
    }

    // -- Nothing is traced through the carrier or the stray bursts. --
    for p in paths {
        for x in p["vertices"].as_array().unwrap() {
            let (_, f) = r.vertex(x);
            for off in [CARRIER, STRAYS[0].1, STRAYS[1].1] {
                assert!(
                    (f - off).abs() > 5_000.0,
                    "a path ran through {off} Hz: {p}"
                );
            }
        }
    }

    // -- Stable across pans: a window holding only the chirp's middle serves the same route. --
    let mid = r.paths(1.6, 2.0);
    let again = of_kind(mid["paths"].as_array().unwrap(), "chirp");
    assert_eq!(again.len(), 1, "{mid}");
    assert_eq!(again[0]["id"], c["id"]);
    assert_eq!(
        again[0]["vertices"], c["vertices"],
        "panning re-shaped the route"
    );
}
