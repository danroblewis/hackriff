//! T-005 acceptance on the T-023 synthetic scenarios, replayed as ci8 through hk-dsp's STFT and
//! `NoiseFloorTracker`:
//! - SPACE-050 (`injected_floor`): known per-segment floors at six tunes → recovered floor within
//!   ±1 dB per segment (target ±0.5 dB), calibrated and uncalibrated.
//! - AWARE-006 (`noise_floor_rise`): a broadband (and a partial-band) floor step at `t0` → one
//!   floor-rise event at `t0` ± one frame with the right step, and the slow floor shows the step.
//!
//! Tests skip when `uv` is missing (`HK_E2E_REQUIRE_SYNTH=1` makes that a failure).

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{FloorConfig, FloorKind, FloorRiseEvent, NoiseFloorTracker};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_e2e::{Fixture, Role, SynthRequest, Tolerance, TruthItem, assert_param, synth_or_skip};
use hk_model::{SampleTime, Timestamp};
use num_complex::Complex;

const SPACE_050: &[&str] = &["SPACE-050"];
const AWARE_006: &[&str] = &["AWARE-006"];
const K: usize = 10;

/// The generator writes ci8 as `round(x·127)` (hk-e2e reads `/127`); hk-core normalises ci8 by
/// `/128`, so the same codes read `20·log10(127/128)` = −0.068 dB lower in hk-dsp.
fn ci8_scale_db() -> f64 {
    20.0 * (127.0f64 / 128.0).log10()
}

fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

struct Summary {
    t_s: f64,
    segment: u64,
    reset: bool,
    frame_db: f64,
    slow_db: f64,
    quantisation_limited: bool,
}

struct Replay {
    frames: Vec<Summary>,
    events: Vec<FloorRiseEvent>,
    frame_period_s: f64,
}

/// Truth floor of a `floor` item in hk-dsp's dBFS/Hz (with the 8-bit quantisation noise).
fn truth_dbfs_per_hz(item: &TruthItem) -> f64 {
    item.expect_f64("expected_floor_dbfs") - db(item.bandwidth_hz()) + ci8_scale_db()
}

/// A provenance record for fixtures that carry none (fixed gains: one gain state).
fn synthetic_provenance(center_hz: f64, sample_rate_hz: f64) -> hk_model::Provenance {
    serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:hk-e2e-floor",
        "tune": {"center_hz": center_hz, "sample_rate_hz": sample_rate_hz, "lna_db": 24.0,
                 "vga_db": 20.0, "amp_on": false, "bandwidth_hz": sample_rate_hz * 0.75},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .expect("provenance JSON")
}

fn replay(fx: &Fixture, fft_len: usize) -> Replay {
    let fs = fx.sample_rate;
    let ci8: Vec<Complex<i8>> = fx
        .samples()
        .unwrap()
        .iter()
        .map(|s| Complex::new((s.re * 127.0).round() as i8, (s.im * 127.0).round() as i8))
        .collect();
    let config = StftConfig::new(WelchConfig::new(fft_len), K);
    let frame_period_s = (K * config.welch.hop()) as f64 / fs;
    let mut stft = StftProcessor::new(config).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let (mut frames, mut events) = (Vec::new(), Vec::new());
    let n = ci8.len() as u64;
    let captures = &fx.meta.captures;
    for (i, cap) in captures.iter().enumerate() {
        let end = captures.get(i + 1).map_or(n, |c| c.sample_start);
        let prov = ProvenanceHandle::new(
            cap.provenance
                .clone()
                .or_else(|| fx.meta.global.provenance.clone())
                .unwrap_or_else(|| synthetic_provenance(cap.frequency.unwrap_or(0.0), fs)),
        );
        let mut start = cap.sample_start;
        while start < end {
            let stop = (start + 65_536).min(end);
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: start,
                    host_time: Timestamp::from_unix_nanos((start as f64 * 1e9 / fs) as i64),
                },
                provenance: prov.clone(),
                discontinuity: if start == 0 {
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                },
                dropped_before: 0,
            };
            stft.push(
                InputInfo::from(&header),
                &ci8[start as usize..stop as usize],
                |frame| {
                    let f = tracker.update(frame, |e| events.push(e.clone()));
                    frames.push(Summary {
                        t_s: frame.t.sample_index as f64 / fs,
                        segment: f.segment,
                        reset: f.reset,
                        frame_db: f64::from(f.band_floor_dbfs_per_hz(FloorKind::Frame)),
                        slow_db: f64::from(f.band_floor_dbfs_per_hz(FloorKind::Slow)),
                        quantisation_limited: f.quantisation_limited,
                    });
                },
            );
            start = stop;
        }
    }
    Replay {
        frames,
        events,
        frame_period_s,
    }
}

#[test]
fn space_050_injected_floor_recovered_per_segment() {
    let out = synth_or_skip!(
        SynthRequest::new("injected_floor")
            .seed(4)
            .param("segment_duration_s", 0.2)
    );
    let fx = out.fixture(0).unwrap();
    let r = replay(&fx, 1024);
    assert!(
        r.events.is_empty(),
        "retunes start new segments, not floor rises"
    );
    let floors = fx.with_role(Role::Floor);
    assert_eq!(floors.len(), 6);
    let (mut worst_frame, mut worst_slow) = (0.0f64, 0.0f64);
    let mut segments = Vec::new();
    for (i, truth) in floors.iter().enumerate() {
        let seg: Vec<&Summary> = r
            .frames
            .iter()
            .filter(|s| {
                s.t_s >= truth.t_start_s && s.t_s + r.frame_period_s <= truth.t_end_s + 1e-9
            })
            .collect();
        assert!(seg.len() >= 30, "segment {i}: {} frames", seg.len());
        assert!(seg[0].reset && seg.iter().all(|s| s.segment == seg[0].segment));
        segments.push(seg[0].segment);
        assert!(seg.iter().all(|s| !s.quantisation_limited));
        let want = truth_dbfs_per_hz(truth);
        let mut frame_errs: Vec<f64> = seg.iter().map(|s| s.frame_db - want).collect();
        let frame_worst = frame_errs.iter().fold(0.0f64, |a, &e| a.max(e.abs()));
        let frame_median = median(&mut frame_errs);
        let slow = seg.last().unwrap().slow_db;
        worst_frame = worst_frame.max(frame_worst);
        worst_slow = worst_slow.max((slow - want).abs());
        eprintln!(
            "SPACE-050 segment {i} ({:.2} MHz): truth {want:.2} dBFS/Hz, per-frame median {frame_median:+.3} dB (worst {frame_worst:.3}), slow {:+.3} dB",
            fx.meta.captures[i].frequency.unwrap_or(0.0) / 1e6,
            slow - want
        );
        let name = format!("segment {i} floor dBFS/Hz");
        assert_param(
            SPACE_050,
            &name,
            frame_median + want,
            want,
            Tolerance::Abs(1.0),
        );
        assert_param(SPACE_050, &name, slow, want, Tolerance::Abs(1.0));
        // Calibrated: dBm/Hz = dBFS/Hz (back on the generator's scale) + K.
        let k = truth.expect_f64("calibration_k_db");
        let want_dbm = truth.expect_f64("expected_floor_dbfs") - db(truth.bandwidth_hz()) + k;
        assert_param(
            SPACE_050,
            &format!("segment {i} calibrated floor dBm/Hz"),
            slow - ci8_scale_db() + k,
            want_dbm,
            Tolerance::Abs(1.0),
        );
    }
    segments.dedup();
    assert_eq!(segments.len(), 6, "one segment per tune");
    eprintln!(
        "SPACE-050 achieved: worst per-frame error {worst_frame:.3} dB, worst slow-floor error {worst_slow:.3} dB"
    );
}

fn floor_truth<'a>(fx: &'a Fixture, label: &str) -> &'a TruthItem {
    fx.with_role(Role::Floor)
        .into_iter()
        .find(|t| t.label.as_deref() == Some(label))
        .unwrap_or_else(|| panic!("no {label} truth"))
}

fn check_rise(fx: &Fixture, r: &Replay, edge_tolerance_hz: f64) {
    let ev = fx.with_role(Role::Event)[0];
    let t0 = ev.expect_f64("t0_s");
    let before = truth_dbfs_per_hz(floor_truth(fx, "noise-floor-before"));
    let after = truth_dbfs_per_hz(floor_truth(fx, "noise-floor-after"));
    assert_eq!(r.events.len(), 1, "AWARE-006: exactly one floor-rise event");
    let e = &r.events[0];
    let t_event = e.t.sample_index as f64 / fx.sample_rate;
    eprintln!(
        "AWARE-006: t0 {t0:.4} s, event {t_event:.4} s (frame {:.2} ms); step {:+.2} dB vs truth {:+.2}; {:.3}–{:.3} MHz vs {:.3}–{:.3}",
        r.frame_period_s * 1e3,
        e.step_db,
        after - before,
        e.f_lo_hz / 1e6,
        e.f_hi_hz / 1e6,
        ev.f_lo_hz / 1e6,
        ev.f_hi_hz / 1e6
    );
    assert_param(
        AWARE_006,
        "rise time s",
        t_event,
        t0,
        Tolerance::Abs(r.frame_period_s),
    );
    assert_param(
        AWARE_006,
        "rise step dB",
        f64::from(e.step_db),
        after - before,
        Tolerance::Abs(1.0),
    );
    assert_param(
        AWARE_006,
        "rise f_lo Hz",
        e.f_lo_hz,
        ev.f_lo_hz,
        Tolerance::Abs(edge_tolerance_hz),
    );
    assert_param(
        AWARE_006,
        "rise f_hi Hz",
        e.f_hi_hz,
        ev.f_hi_hz,
        Tolerance::Abs(edge_tolerance_hz),
    );
    assert!(!e.quantisation_limited_before);
}

#[test]
fn aware_006_broadband_floor_rise_event_and_slow_floor_step() {
    let out = synth_or_skip!(
        SynthRequest::new("noise_floor_rise")
            .seed(3)
            .param("duration_s", 1.0)
            .param("t0_s", 0.5)
    );
    let fx = out.fixture(0).unwrap();
    let r = replay(&fx, 1024);
    let bin_hz = fx.sample_rate / 1024.0;
    check_rise(&fx, &r, bin_hz);

    let t0 = 0.5;
    let before = truth_dbfs_per_hz(floor_truth(&fx, "noise-floor-before"));
    let after = truth_dbfs_per_hz(floor_truth(&fx, "noise-floor-after"));
    let mut slow_before: Vec<f64> = r
        .frames
        .iter()
        .filter(|s| s.t_s >= 0.2 && s.t_s + r.frame_period_s <= t0)
        .map(|s| s.slow_db)
        .collect();
    let mut slow_after: Vec<f64> = r
        .frames
        .iter()
        .filter(|s| s.t_s >= t0 + 0.1)
        .map(|s| s.slow_db)
        .collect();
    let (sb, sa) = (median(&mut slow_before), median(&mut slow_after));
    eprintln!(
        "AWARE-006 slow floor: before {:+.3} dB, after {:+.3} dB, step {:.2} dB (truth {:.2})",
        sb - before,
        sa - after,
        sa - sb,
        after - before
    );
    assert_param(
        AWARE_006,
        "slow floor before",
        sb,
        before,
        Tolerance::Abs(1.0),
    );
    assert_param(
        AWARE_006,
        "slow floor after",
        sa,
        after,
        Tolerance::Abs(1.0),
    );
    assert_param(
        AWARE_006,
        "slow floor step",
        sa - sb,
        after - before,
        Tolerance::Abs(1.0),
    );
    assert!(
        r.frames.iter().all(|s| s.segment == 0),
        "no resets within the dwell"
    );
}

#[test]
fn aware_006_partial_band_floor_rise_event() {
    let out = synth_or_skip!(
        SynthRequest::new("noise_floor_rise")
            .seed(5)
            .param("duration_s", 1.0)
            .param("t0_s", 0.5)
            .param("rise_bandwidth_hz", 500e3)
            .param("rise_offset_hz", 300e3)
    );
    let fx = out.fixture(0).unwrap();
    let r = replay(&fx, 4096);
    // Edges resolve to about half a 256-bin block (62.5 kHz at 488 Hz bins).
    check_rise(&fx, &r, 150e3);
}
