//! SPACE-050 (natural radio noise-floor survey): the floor-vs-time product T-021 reads from the
//! pyramid's low percentile.

use std::path::PathBuf;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_e2e::{Role, SynthRequest, synth_or_skip};
use hk_model::{FreqRange, PowerUnit, SampleTime, TimeRange, Timestamp};
use hk_store::{FrameInput, Pyramid, PyramidConfig, RegionHistory, RegionQuery, Resolution};
use num_complex::Complex32;

const SPACE_050: &str = "SPACE-050";
const S: i64 = 1_000_000_000;
/// 2026-09-13T12:00:00Z, the synth scenarios' default start.
const T0: i64 = 1_789_300_800 * S;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("hk-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn median_p_low(h: &RegionHistory) -> f64 {
    let mut v: Vec<f32> = h
        .cells
        .iter()
        .filter(|c| c.observed() && c.p_low_db.is_finite())
        .map(|c| c.p_low_db)
        .collect();
    assert!(!v.is_empty(), "{SPACE_050}: no observed cells");
    v.sort_by(f32::total_cmp);
    f64::from(v[v.len() / 2])
}

/// Replays the `injected_floor` scenario through the hk-dsp STFT, adapts the frames with
/// `FrameInput::from_dsp`, and checks the low percentile of each band segment against the injected
/// floor (including 8-bit quantisation noise) at level 0 (exact order statistics) and level 1
/// (histogram-merged).
#[test]
fn space_050_injected_floor_recovered_from_p10_per_segment() {
    let out = synth_or_skip!(
        SynthRequest::new("injected_floor")
            .seed(4)
            .param("segment_duration_s", 0.5)
    );
    let fx = out.fixture(0).unwrap();
    let fs = fx.sample_rate;
    let dir = TempDir::new("space050-dsp");
    let mut p = Pyramid::open(&dir.0, PyramidConfig::default()).unwrap();
    let floors = fx.with_role(Role::Floor);
    assert!(floors.len() >= 6);
    let mut expected = Vec::new();
    for seg in &floors {
        let cap = fx.capture_at(seg.sample_start).unwrap();
        let prov = ProvenanceHandle::new(cap.provenance.clone().expect("capture provenance"));
        let samples: Vec<Complex32> = fx
            .samples_range(seg.sample_start, seg.sample_count)
            .unwrap()
            .iter()
            .map(|c| Complex32::new(c.re, c.im))
            .collect();
        // 1024 bins (977 Hz), K = 32: each 6.25 kHz cell averages ~6 bins of 32 segments.
        let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(1024), 32)).unwrap();
        let t_start = T0 + (seg.sample_start as f64 * 1e9 / fs) as i64;
        let info = InputInfo {
            time: SampleTime {
                sample_index: seg.sample_start,
                host_time: Timestamp::from_unix_nanos(t_start),
            },
            discontinuity: Discontinuity::STREAM_START,
            dropped_before: 0,
            provenance: &prov,
        };
        let mut frames = 0;
        stft.push(info, &samples, |frame| {
            let input = FrameInput::from_dsp(frame);
            assert_eq!(input.unit, PowerUnit::Dbfs);
            p.ingest(&input).unwrap();
            frames += 1;
        });
        assert!(frames >= 20, "{SPACE_050}: {frames} frames");
        let want =
            seg.expect_f64("expected_floor_dbfs") - 10.0 * seg.expect_f64("bandwidth_hz").log10();
        let fc = cap.frequency.unwrap();
        let t_end = T0 + ((seg.sample_start + seg.sample_count) as f64 * 1e9 / fs) as i64;
        expected.push((fc, t_start, t_end, want));
    }
    p.seal_through(Timestamp::from_unix_nanos(T0 + 3600 * S))
        .unwrap();
    for &(fc, t0, t1, want) in &expected {
        for (level, resolution) in [(0u8, Resolution::Level(0)), (1, Resolution::Level(1))] {
            let h = p
                .query(&RegionQuery {
                    freq: FreqRange::centered(fc, 0.8 * fs),
                    time: TimeRange::new(
                        Timestamp::from_unix_nanos(t0),
                        Timestamp::from_unix_nanos(t1),
                    ),
                    resolution,
                })
                .unwrap();
            let got = median_p_low(&h);
            eprintln!(
                "{SPACE_050}: segment {:.2} MHz L{level}: p10 {got:.2} dB/Hz, injected {want:.2} (err {:+.2})",
                fc / 1e6,
                got - want
            );
            assert!(
                (got - want).abs() <= 1.0,
                "{SPACE_050}: L{level} floor at {fc} Hz: p10 {got:.2} vs injected {want:.2} dB/Hz"
            );
            assert_eq!(h.provenance.gain_states.len(), 1, "gain state recorded");
        }
    }
}

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gamma(&mut self, k: u32) -> f64 {
        (0..k).map(|_| -self.unit().max(1e-12).ln()).sum::<f64>() / f64::from(k)
    }
}

/// Floor-vs-time over six hours with a known floor per hour and 10% bursty activity: the low
/// percentile tracks the floor per hour at level 3 (1 h cells) and per 15 min at level 2, within
/// ±1 dB, after rollup. Frames are synthesised directly: 1 frame/s, 256 bins of 1953 Hz, PSD =
/// floor × gamma(64) (a 64-average periodogram).
#[test]
fn space_050_floor_vs_time_survives_rollup() {
    let floors_db = [-150.0, -148.5, -152.0, -145.0, -149.0, -147.0];
    let dir = TempDir::new("space050-time");
    let mut p = Pyramid::open(&dir.0, PyramidConfig::default()).unwrap();
    let (nb, bw, f_lo) = (256usize, 500e3 / 256.0, 99.75e6);
    let mut rng = Rng(50);
    let mut on = vec![false; nb];
    let mut psd = vec![0f32; nb];
    for (h, &floor) in floors_db.iter().enumerate() {
        let n0 = 10f64.powf(floor / 10.0);
        for s in 0..3600i64 {
            for (i, v) in psd.iter_mut().enumerate() {
                on[i] = if on[i] {
                    rng.unit() > 0.1
                } else {
                    rng.unit() < 0.011
                };
                let gain = if on[i] { 100.0 } else { 1.0 };
                *v = (n0 * gain * rng.gamma(64)) as f32;
            }
            let t = T0 + (h as i64 * 3600 + s) * S;
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(t),
                S,
                f_lo,
                bw,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
    }
    let end = T0 + 6 * 3600 * S;
    p.seal_through(Timestamp::from_unix_nanos(end)).unwrap();
    let q = |t_ns| RegionQuery {
        freq: FreqRange::new(99.8e6, 100.2e6),
        time: TimeRange::new(
            Timestamp::from_unix_nanos(T0),
            Timestamp::from_unix_nanos(end),
        ),
        resolution: Resolution::Cell { t_ns, f_hz: 1e6 },
    };
    for (t_ns, level, rows_per_hour) in [(3600 * S, 3u8, 1usize), (900 * S, 2, 4)] {
        let h = p.query(&q(t_ns)).unwrap();
        assert_eq!(h.level, level);
        assert_eq!(h.nt, 6 * rows_per_hour);
        for row in 0..h.nt {
            let want = floors_db[row / rows_per_hour];
            let mut v: Vec<f32> = h
                .row(row)
                .iter()
                .filter(|c| c.observed())
                .map(|c| c.p_low_db)
                .collect();
            v.sort_by(f32::total_cmp);
            let got = f64::from(v[v.len() / 2]);
            assert!(
                (got - want).abs() <= 1.0,
                "{SPACE_050}: L{level} row {row}: p10 {got:.2} vs injected {want:.2} dB/Hz"
            );
            // The activity is still visible in the same cells.
            assert!(h.row(row).iter().any(|c| c.max_db > want as f32 + 15.0));
        }
    }
}
