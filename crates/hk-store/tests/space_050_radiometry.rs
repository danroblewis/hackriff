//! SPACE-050 (natural radio noise-floor survey), T-021: the calibrated noise-floor product.
//!
//! IQ → hk-dsp STFT → `NoiseFloorTracker` → calibration → `FloorSeries` (frame level) and
//! `FloorProduct` (tiles) → `floor_vs_time`. Covers: the `injected_floor` synth per segment, the
//! derived p10 bias correction on noise-only frames, a gain change with correct calibration, an
//! uncalibrated period, quantisation-limited input, a gap, uncertainty propagation, and impulsive
//! duty (gate-release flags).

use std::path::PathBuf;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker, QuantisationFloor};
use hk_dsp::radiometry::{
    FloorPoint, FloorSeries, FloorSeriesConfig, PowerCalTable, PowerCalibrations,
    SyntheticCalSegment, UncalibratedReason, combine_uncertainty, exact_percentile_probability,
    gain_setting_of, noise_temperature_k, percentile_bias_db, synthetic_calibration_state,
};
use hk_dsp::synth::{Rng, complex_noise};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_e2e::{Role, SynthRequest, Tolerance, assert_param, synth_or_skip};
use hk_model::{
    CalibrationStateId, FreqRange, GainSetting, PowerUnit, Provenance, SampleTime, Timestamp,
};
use hk_store::{FloorFlags, FloorProduct, FloorProductConfig, FloorStep, FloorVsTime, Resolution};
use num_complex::Complex32;

const SPACE_050: &[&str] = &["SPACE-050"];
const S: i64 = 1_000_000_000;
/// 2026-09-13T12:00:00Z, the synth scenarios' default start.
const T0: i64 = 1_789_300_800 * S;
const FS: f64 = 1e6;
const FC: f64 = 100e6;
const G_A: GainSetting = GainSetting {
    lna_db: 24.0,
    vga_db: 20.0,
    amp_on: false,
};
const G_B: GainSetting = GainSetting {
    lna_db: 32.0,
    vga_db: 20.0,
    amp_on: false,
};

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

fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

fn ts(ns: i64) -> Timestamp {
    Timestamp::from_unix_nanos(ns)
}

fn region() -> FreqRange {
    FreqRange::centered(FC, 0.8 * FS)
}

fn provenance(g: GainSetting, cal: Option<CalibrationStateId>) -> ProvenanceHandle {
    let mut p: Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:t-021",
        "tune": {"center_hz": FC, "sample_rate_hz": FS, "lna_db": g.lna_db, "vga_db": g.vga_db,
                 "amp_on": g.amp_on, "bandwidth_hz": 0.75 * FS},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .expect("provenance JSON");
    p.calibration_state_ref = cal;
    ProvenanceHandle::new(p)
}

/// A calibration version from synthetic `k` per gain over the test band.
fn calibration(ks: &[(GainSetting, f64)], sigma: f64) -> (CalibrationStateId, PowerCalibrations) {
    let segments: Vec<SyntheticCalSegment> = ks
        .iter()
        .map(|&(gain, k_db)| SyntheticCalSegment {
            band: FreqRange::centered(FC, FS),
            gain,
            k_db,
        })
        .collect();
    let state = synthetic_calibration_state("synthetic:t-021", &segments, sigma, ts(T0));
    let mut cals = PowerCalibrations::new();
    cals.insert(PowerCalTable::from_state(&state, None));
    (state.id, cals)
}

/// Noise → STFT → tracker → series + product.
struct Rig {
    stft: StftProcessor,
    tracker: NoiseFloorTracker,
    series: FloorSeries,
    product: FloorProduct,
    rng: Rng,
    index: u64,
    pending: Discontinuity,
}

impl Rig {
    fn new(dir: &TempDir, fft_len: usize, k: usize, cals: PowerCalibrations) -> Self {
        Self::with_floor(dir, fft_len, k, cals, FloorConfig::default())
    }

    fn with_floor(
        dir: &TempDir,
        fft_len: usize,
        k: usize,
        cals: PowerCalibrations,
        floor: FloorConfig,
    ) -> Self {
        Self {
            stft: StftProcessor::new(StftConfig::new(WelchConfig::new(fft_len), k)).unwrap(),
            tracker: NoiseFloorTracker::new(floor).unwrap(),
            series: FloorSeries::new(cals.clone(), FloorSeriesConfig::default()),
            product: FloorProduct::open(&dir.0, FloorProductConfig::default(), cals).unwrap(),
            rng: Rng::new(0x5ace_0021),
            index: 0,
            pending: Discontinuity::STREAM_START,
        }
    }

    /// `seconds` of complex noise with variance `variance(t_s)` (per 10 ms chunk) under `prov`.
    fn push(
        &mut self,
        seconds: f64,
        prov: &ProvenanceHandle,
        variance: impl Fn(f64) -> f64,
        flags: Discontinuity,
    ) {
        self.pending = Discontinuity::from_bits_truncate(self.pending.bits() | flags.bits());
        let end = self.index + (seconds * FS).round() as u64;
        while self.index < end {
            let n = 10_000.min(end - self.index);
            let samples: Vec<Complex32> =
                complex_noise(&mut self.rng, n as usize, variance(self.index as f64 / FS));
            let info = InputInfo {
                time: SampleTime {
                    sample_index: self.index,
                    host_time: ts(T0 + (self.index as f64 * 1e9 / FS) as i64),
                },
                discontinuity: std::mem::replace(&mut self.pending, Discontinuity::NONE),
                dropped_before: 0,
                provenance: prov,
            };
            let Self {
                stft,
                tracker,
                series,
                product,
                ..
            } = self;
            stft.push(info, &samples, |frame| {
                let f = tracker.update(frame, |_| {});
                series.push(f);
                product.ingest(frame, f).unwrap();
            });
            self.index += n;
        }
    }

    /// Loses `seconds` of samples (the next push carries `GAP`).
    fn skip(&mut self, seconds: f64) {
        self.index += (seconds * FS).round() as u64;
        self.pending = Discontinuity::GAP;
    }

    fn floor_vs_time(&self, t0_s: i64, t1_s: i64, resolution: Resolution) -> FloorVsTime {
        self.product
            .floor_vs_time(region(), ts(T0 + t0_s * S), ts(T0 + t1_s * S), resolution)
            .unwrap()
    }
}

fn t_s(p: &FloorPoint) -> f64 {
    (p.t.as_unix_nanos() - T0) as f64 * 1e-9
}

fn step_t_s(s: &FloorStep) -> f64 {
    (s.t.as_unix_nanos() - T0) as f64 * 1e-9
}

/// `injected_floor` (six band segments with known floors and `calibration_k_db`) → calibration
/// from truth → per-segment calibrated floor from the frame series and from the tiles at levels
/// 0 and 1, each within ±1 dB (target ±0.5).
#[test]
fn space_050_injected_floor_calibrated_product_per_segment() {
    let out = synth_or_skip!(
        SynthRequest::new("injected_floor")
            .seed(4)
            .param("segment_duration_s", 1.0)
    );
    let fx = out.fixture(0).unwrap();
    let fs = fx.sample_rate;
    let floors = fx.with_role(Role::Floor);
    assert_eq!(floors.len(), 6);
    // hk-e2e's cf32 reader matches the generator's dBFS scale, so truth k applies as is.
    let segments: Vec<SyntheticCalSegment> = floors
        .iter()
        .map(|seg| {
            let cap = fx.capture_at(seg.sample_start).unwrap();
            SyntheticCalSegment {
                band: FreqRange::centered(cap.frequency.unwrap(), fs),
                gain: gain_setting_of(cap.provenance.as_ref().expect("capture provenance")),
                k_db: seg.expect_f64("calibration_k_db"),
            }
        })
        .collect();
    let state = synthetic_calibration_state("synthetic:t-021", &segments, 0.1, ts(T0));
    let cals = PowerCalibrations::from_states([&state], None);
    let dir = TempDir::new("t021-injected");
    let mut product =
        FloorProduct::open(&dir.0, FloorProductConfig::default(), cals.clone()).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut series = FloorSeries::new(cals, FloorSeriesConfig::default());
    let mut expected = Vec::new();
    for seg in &floors {
        let cap = fx.capture_at(seg.sample_start).unwrap();
        let mut p = cap.provenance.clone().unwrap();
        p.calibration_state_ref = Some(state.id);
        let prov = ProvenanceHandle::new(p);
        let samples: Vec<Complex32> = fx
            .samples_range(seg.sample_start, seg.sample_count)
            .unwrap()
            .iter()
            .map(|c| Complex32::new(c.re, c.im))
            .collect();
        let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(1024), 32)).unwrap();
        let t_start = T0 + (seg.sample_start as f64 * 1e9 / fs) as i64;
        let t_end = T0 + ((seg.sample_start + seg.sample_count) as f64 * 1e9 / fs) as i64;
        let info = InputInfo {
            time: SampleTime {
                sample_index: seg.sample_start,
                host_time: ts(t_start),
            },
            discontinuity: Discontinuity::STREAM_START,
            dropped_before: 0,
            provenance: &prov,
        };
        let first = series.points().len();
        stft.push(info, &samples, |frame| {
            let f = tracker.update(frame, |_| {});
            series.push(f);
            product.ingest(frame, f).unwrap();
        });
        let pts = &series.points()[first..];
        assert!(pts.len() >= 50, "{} frames", pts.len());
        assert!(pts.iter().all(|p| {
            p.unit == PowerUnit::Dbm
                && !p
                    .flags
                    .intersects(FloorFlags::UNCALIBRATED | FloorFlags::QUANTISATION_LIMITED)
        }));
        let want = seg.expect_f64("expected_floor_dbfs") - db(seg.bandwidth_hz())
            + seg.expect_f64("calibration_k_db");
        let last = pts.last().unwrap().dbm_per_hz().unwrap();
        expected.push((cap.frequency.unwrap(), t_start, t_end, want, last));
    }
    product.seal_through(ts(T0 + 3600 * S)).unwrap();
    let (mut worst_series, mut worst_l0, mut worst_l1) = (0.0f64, 0.0f64, 0.0f64);
    for (i, &(fc, t0, t1, want, series_dbm)) in expected.iter().enumerate() {
        assert_param(
            SPACE_050,
            &format!("segment {i} series calibrated floor dBm/Hz"),
            series_dbm,
            want,
            Tolerance::Abs(1.0),
        );
        worst_series = worst_series.max((series_dbm - want).abs());
        for level in [0u8, 1] {
            let fvt = product
                .floor_vs_time(
                    FreqRange::centered(fc, 0.8 * fs),
                    ts(t0),
                    ts(t1),
                    Resolution::Level(level),
                )
                .unwrap();
            let observed: Vec<&FloorStep> = fvt.steps.iter().filter(|s| !s.is_gap()).collect();
            assert_eq!(observed.len(), 1, "L{level}: one step");
            let st = observed[0];
            let got = st.dbm_per_hz().expect("calibrated");
            eprintln!(
                "SPACE-050 segment {i} ({:.2} MHz) L{level}: {got:.2} dBm/Hz vs truth {want:.2} (err {:+.3}; raw p10 {:+.3}, bias {:.3}, mean {:+.3}; σ {:.2}); series {:+.3}",
                fc / 1e6,
                got - want,
                st.raw_p_low_db_per_hz.unwrap() - want,
                st.bias_db.unwrap(),
                st.mean_db_per_hz.unwrap() - want,
                st.uncertainty_db,
                series_dbm - want
            );
            assert_param(
                SPACE_050,
                &format!("segment {i} L{level} calibrated floor dBm/Hz"),
                got,
                want,
                Tolerance::Abs(1.0),
            );
            assert!(
                !st.flags
                    .intersects(FloorFlags::UNCALIBRATED | FloorFlags::GAP)
            );
            let worst = if level == 0 {
                &mut worst_l0
            } else {
                &mut worst_l1
            };
            *worst = worst.max((got - want).abs());
            // The tile digest keeps the calibration id.
            assert_eq!(fvt.calibrated_provenance.calibration, Some(state.id));
            assert!(!fvt.calibrated_provenance.calibration_mixed);
        }
    }
    eprintln!(
        "SPACE-050 achieved: worst |error| series {worst_series:.3} dB, tiles L0 {worst_l0:.3} dB, L1 {worst_l1:.3} dB"
    );
}

/// Noise-only frames: T-017's p10 reads low by the spread of averaged noise; subtracting the
/// Gamma-derived bias `10·log10(P⁻¹(n_c, p)/n_c)` recovers the true PSD within ±0.2 dB, for two
/// spectral geometries with different bias (derived, not fitted).
#[test]
fn space_050_p10_bias_correction_derived_on_noise_only_frames() {
    let v = 1e-4;
    let truth = db(v) - db(FS);
    for (fft_len, k) in [(1024usize, 32usize), (4096, 16)] {
        let dir = TempDir::new(&format!("t021-bias-{fft_len}"));
        let mut rig = Rig::new(&dir, fft_len, k, PowerCalibrations::new());
        let prov = provenance(G_A, None);
        rig.push(6.0, &prov, |_| v, Discontinuity::NONE);
        let shape = rig.product.shape().unwrap();
        let fvt = rig.floor_vs_time(0, 6, Resolution::Level(0));
        assert_eq!(fvt.steps.len(), 6);
        for st in &fvt.steps {
            assert_eq!(st.unit, Some(PowerUnit::Dbfs));
            assert!(st.flags.contains(FloorFlags::UNCALIBRATED));
            let raw = st.raw_p_low_db_per_hz.unwrap() - truth;
            let got = st.value_db_per_hz.unwrap() - truth;
            assert!(raw < -0.25, "{fft_len}: the raw p10 reads low ({raw:+.3})");
            assert_param(
                SPACE_050,
                &format!(
                    "{fft_len} bins row {:.0} s: corrected p10 dBFS/Hz",
                    step_t_s(st)
                ),
                got + truth,
                truth,
                Tolerance::Abs(0.2),
            );
        }
        let st = &fvt.steps[2];
        let frames_per_cell = (1e9 / (k * fft_len / 2) as f64 * 1e-9 * FS).round() as u32;
        let p = exact_percentile_probability(10.0, frames_per_cell);
        eprintln!(
            "SPACE-050 bias ({fft_len} bins, K {k}): n_c {shape:.1}, ~{frames_per_cell} frames/cell → p {p:.4}, bias {:.3} dB; row 2 raw {:+.3} dB → corrected {:+.3} dB (σ_stat {:.3})",
            percentile_bias_db(shape, p),
            st.raw_p_low_db_per_hz.unwrap() - truth,
            st.value_db_per_hz.unwrap() - truth,
            st.statistical_uncertainty_db,
        );
        // Rolled up (level 1, pooled histogram, p = 0.1): within ±0.2 dB plus half a 0.5 dB step.
        rig.product.seal_through(ts(T0 + 3600 * S)).unwrap();
        let l1 = rig.floor_vs_time(0, 6, Resolution::Level(1));
        let st1 = l1.steps.iter().find(|s| !s.is_gap()).unwrap();
        let err1 = st1.value_db_per_hz.unwrap() - truth;
        eprintln!(
            "SPACE-050 bias ({fft_len} bins) L1: raw {:+.3} dB → corrected {err1:+.3} dB (σ_hist {:.3})",
            st1.raw_p_low_db_per_hz.unwrap() - truth,
            st1.histogram_uncertainty_db
        );
        assert!(err1.abs() <= 0.45, "L1 corrected error {err1:+.3}");
    }
}

/// A gain change mid-series (and mid-cell) with correct calibration: the dBFS floor steps by the
/// gain, the calibrated floor does not step by more than its uncertainty, in the frame series and
/// in floor-vs-time; the straddling step is flagged; the product reopens with the same result.
#[test]
fn space_050_gain_change_mid_series_no_step_with_correct_calibration() {
    let (id, cals) = calibration(&[(G_A, -70.0), (G_B, -78.0)], 0.2);
    let dir = TempDir::new("t021-gain");
    let mut rig = Rig::new(&dir, 1024, 32, cals.clone());
    let v_a = 1e-4;
    let v_b = v_a * 10f64.powf(0.8);
    let truth_dbm = db(v_a) - db(FS) - 70.0;
    rig.push(
        4.5,
        &provenance(G_A, Some(id)),
        |_| v_a,
        Discontinuity::NONE,
    );
    rig.push(
        4.5,
        &provenance(G_B, Some(id)),
        |_| v_b,
        Discontinuity::GAIN_CHANGE,
    );

    // Frame series.
    let pts = rig.series.points();
    let ready = |p: &&FloorPoint| !p.flags.contains(FloorFlags::NOT_READY);
    let a = pts
        .iter()
        .rev()
        .filter(|p| t_s(p) < 4.4)
        .find(ready)
        .unwrap();
    let first_b = pts.iter().find(|p| p.gain == G_B).unwrap();
    assert!(first_b.flags.contains(FloorFlags::SEGMENT_START));
    let b = pts.iter().filter(|p| p.gain == G_B).find(ready).unwrap();
    assert!(
        (b.dbfs_per_hz - a.dbfs_per_hz - 8.0).abs() < 0.5,
        "dBFS steps by the gain"
    );
    let step = b.dbm_per_hz().unwrap() - a.dbm_per_hz().unwrap();
    eprintln!(
        "SPACE-050 gain change: series dBFS step {:+.2} dB, dBm step {step:+.3} dB (σ {:.2})",
        b.dbfs_per_hz - a.dbfs_per_hz,
        a.uncertainty_db
    );
    assert!(step.abs() <= a.uncertainty_db.max(b.uncertainty_db));

    // Tiles.
    let fvt = rig.floor_vs_time(0, 9, Resolution::Level(0));
    assert_eq!(fvt.steps.len(), 9);
    for st in &fvt.steps {
        let got = st.dbm_per_hz().expect("calibrated throughout");
        assert!(
            (got - truth_dbm).abs() <= st.uncertainty_db,
            "row {:.0}: {:+.3} vs σ {:.2}",
            step_t_s(st),
            got - truth_dbm,
            st.uncertainty_db
        );
        let straddle = step_t_s(st) == 4.0;
        assert_eq!(
            st.flags.contains(FloorFlags::MIXED_GAIN),
            straddle,
            "row {}",
            step_t_s(st)
        );
        assert_eq!(st.gain_states, if straddle { 2 } else { 1 });
    }
    assert!(fvt.steps[4].flags.contains(FloorFlags::SEGMENT_START));
    for w in fvt.steps.windows(2) {
        let d = w[1].value_db_per_hz.unwrap() - w[0].value_db_per_hz.unwrap();
        assert!(
            d.abs() <= w[0].uncertainty_db.max(w[1].uncertainty_db),
            "step {d:+.3} dB"
        );
    }
    let max_step = fvt
        .steps
        .windows(2)
        .map(|w| (w[1].value_db_per_hz.unwrap() - w[0].value_db_per_hz.unwrap()).abs())
        .fold(0.0, f64::max);
    eprintln!("SPACE-050 gain change: largest adjacent tile step {max_step:.3} dB");

    // Close and reopen: values and flags persist (tiles, state log, shape).
    let shape = rig.product.shape();
    let Rig { product, .. } = rig;
    product.close().unwrap();
    let reopened = FloorProduct::open(&dir.0, FloorProductConfig::default(), cals).unwrap();
    assert_eq!(reopened.shape(), shape);
    let again = reopened
        .floor_vs_time(region(), ts(T0), ts(T0 + 9 * S), Resolution::Level(0))
        .unwrap();
    for (x, y) in fvt.steps.iter().zip(&again.steps) {
        assert_eq!(x.flags, y.flags);
        assert_eq!(x.unit, y.unit);
        assert!((x.value_db_per_hz.unwrap() - y.value_db_per_hz.unwrap()).abs() < 0.02);
    }
}

/// An uncalibrated period (provenance pins no calibration) yields dBFS/Hz with `uncalibrated`,
/// never dBm, in the series and the tiles; calibrated steps around it stay dBm.
#[test]
fn space_050_uncalibrated_period_reports_dbfs_never_dbm() {
    let (id, cals) = calibration(&[(G_A, -70.0)], 0.2);
    let dir = TempDir::new("t021-uncal");
    let mut rig = Rig::new(&dir, 1024, 32, cals);
    let v = 1e-4;
    let truth_dbfs = db(v) - db(FS);
    let (cal, none) = (provenance(G_A, Some(id)), provenance(G_A, None));
    rig.push(3.5, &cal, |_| v, Discontinuity::NONE);
    rig.push(3.0, &none, |_| v, Discontinuity::NONE);
    rig.push(2.5, &cal, |_| v, Discontinuity::NONE);

    for p in rig.series.points() {
        let t = t_s(p);
        if (3.55..6.45).contains(&t) {
            assert_eq!(p.unit, PowerUnit::Dbfs);
            assert!(p.flags.contains(FloorFlags::UNCALIBRATED));
            assert_eq!(p.dbm_per_hz(), None);
            assert_eq!(p.noise_temperature_k(), None);
            assert_eq!(
                p.uncalibrated_reason,
                Some(UncalibratedReason::NoCalibrationRef)
            );
            assert_eq!(p.uncertainty_db, p.estimator_uncertainty_db());
        } else if !(3.4..6.6).contains(&t) {
            assert_eq!(p.unit, PowerUnit::Dbm);
        }
    }
    let fvt = rig.floor_vs_time(0, 9, Resolution::Level(0));
    for st in &fvt.steps {
        let row = step_t_s(st) as i64;
        match row {
            4 | 5 => {
                assert_eq!(st.unit, Some(PowerUnit::Dbfs), "row {row}");
                assert!(st.flags.contains(FloorFlags::UNCALIBRATED));
                assert_eq!(st.dbm_per_hz(), None);
                assert_eq!(st.noise_temperature_k(), None);
                assert_eq!(st.calibration_uncertainty_db, 0.0);
                assert_param(
                    SPACE_050,
                    &format!("uncalibrated row {row} dBFS/Hz"),
                    st.value_db_per_hz.unwrap(),
                    truth_dbfs,
                    Tolerance::Abs(1.0),
                );
            }
            3 | 6 => {
                assert_eq!(st.unit, Some(PowerUnit::Dbm), "row {row}");
                assert!(st.flags.contains(FloorFlags::PARTLY_UNCALIBRATED));
                assert!(!st.flags.contains(FloorFlags::UNCALIBRATED));
            }
            _ => {
                assert_eq!(st.unit, Some(PowerUnit::Dbm), "row {row}");
                assert!(
                    !st.flags
                        .intersects(FloorFlags::UNCALIBRATED | FloorFlags::PARTLY_UNCALIBRATED)
                );
                assert!((st.dbm_per_hz().unwrap() - (truth_dbfs - 70.0)).abs() < 1.0);
            }
        }
    }
    let stats = rig.product.stats();
    assert!(stats.uncalibrated_frames > 150 && stats.calibrated_frames > 300);
}

/// Quantisation-limited input (a low-gain state within 3 dB of the receiver floor) is flagged in
/// the series and in floor-vs-time; the high-gain state is not.
#[test]
fn space_050_quantisation_limited_input_is_flagged() {
    let (id, cals) = calibration(&[(G_B, -70.0), (G_A, -50.0)], 0.2);
    let dir = TempDir::new("t021-quant");
    let v_high = 1e-4;
    let v_low = 1e-6;
    // This receiver's low-gain floor: 1.5 dB under the low-gain reading.
    let floor = FloorConfig {
        quantisation: QuantisationFloor::DbfsPerHz(db(v_low) - db(FS) - 1.5),
        ..FloorConfig::default()
    };
    let mut rig = Rig::with_floor(&dir, 1024, 32, cals, floor);
    rig.push(
        3.0,
        &provenance(G_B, Some(id)),
        |_| v_high,
        Discontinuity::NONE,
    );
    rig.push(
        3.0,
        &provenance(G_A, Some(id)),
        |_| v_low,
        Discontinuity::GAIN_CHANGE,
    );
    for p in rig.series.points() {
        assert_eq!(
            p.flags.contains(FloorFlags::QUANTISATION_LIMITED),
            p.gain == G_A,
            "t {:.3}",
            t_s(p)
        );
    }
    let fvt = rig.floor_vs_time(0, 6, Resolution::Level(0));
    for st in &fvt.steps {
        let low = step_t_s(st) >= 3.0;
        assert_eq!(st.flags.contains(FloorFlags::QUANTISATION_LIMITED), low);
        assert!(st.dbm_per_hz().is_some(), "still reported (an upper bound)");
    }
}

/// A gap in the stream is a gap: no floor-vs-time value and a `gap` flag for the missing steps,
/// a recorded gap and `gap-before` in the series, and no interpolation.
#[test]
fn space_050_gap_in_series_is_a_gap_not_interpolated() {
    let (id, cals) = calibration(&[(G_A, -70.0)], 0.2);
    let dir = TempDir::new("t021-gap");
    let mut rig = Rig::new(&dir, 1024, 32, cals);
    let prov = provenance(G_A, Some(id));
    rig.push(3.0, &prov, |_| 1e-4, Discontinuity::NONE);
    rig.skip(4.0);
    rig.push(3.0, &prov, |_| 1e-4, Discontinuity::NONE);

    let gaps = rig.series.gaps();
    assert_eq!(gaps.len(), 1);
    let (g0, g1) = (
        (gaps[0].start.as_unix_nanos() - T0) as f64 * 1e-9,
        (gaps[0].end.as_unix_nanos() - T0) as f64 * 1e-9,
    );
    assert!(
        (2.95..=3.01).contains(&g0) && (7.0..7.02).contains(&g1),
        "gap {g0}–{g1}"
    );
    assert!(
        rig.series
            .points()
            .iter()
            .all(|p| !(3.01..7.0).contains(&t_s(p)))
    );
    let after = rig.series.points().iter().find(|p| t_s(p) >= 7.0).unwrap();
    assert!(
        after
            .flags
            .contains(FloorFlags::GAP_BEFORE | FloorFlags::SEGMENT_START)
    );

    let fvt = rig.floor_vs_time(0, 10, Resolution::Level(0));
    assert_eq!(fvt.steps.len(), 10);
    for st in &fvt.steps {
        let row = step_t_s(st) as i64;
        if (3..7).contains(&row) {
            assert!(st.is_gap(), "row {row}");
            assert_eq!(st.flags, FloorFlags::GAP);
            assert_eq!((st.unit, st.dbm_per_hz()), (None, None));
            assert!(st.uncertainty_db.is_nan());
        } else {
            assert!(st.dbm_per_hz().is_some(), "row {row}");
            assert!(!st.flags.contains(FloorFlags::GAP));
        }
    }
}

/// Uncertainty propagation: series `σ = √(σ_model² + σ_stat² + σ_cal²)`; floor-vs-time
/// `σ = √(σ_model² + σ_cal² + σ_hist² + σ_stat²)` with `σ_hist = step/√12` only for rolled-up
/// cells; temperature and its bounds follow from the value and `σ`.
#[test]
fn space_050_uncertainty_propagation_matches_formula() {
    let sigma_cal = 0.3;
    let (id, cals) = calibration(&[(G_A, -70.0)], sigma_cal);
    let dir = TempDir::new("t021-sigma");
    let mut rig = Rig::new(&dir, 1024, 32, cals);
    rig.push(
        6.0,
        &provenance(G_A, Some(id)),
        |_| 1e-4,
        Discontinuity::NONE,
    );

    let p = rig.series.points().last().unwrap();
    assert_eq!(p.model_uncertainty_db, 0.5);
    assert_eq!(p.calibration_uncertainty_db(), sigma_cal);
    let want = combine_uncertainty(&[0.5, p.statistical_uncertainty_db, sigma_cal]);
    assert!((p.uncertainty_db - want).abs() < 1e-12);
    let t = noise_temperature_k(p.dbm_per_hz().unwrap());
    let (lo, hi) = p.noise_temperature_bounds_k().unwrap();
    let r = 10f64.powf(p.uncertainty_db / 10.0);
    assert!((lo - t / r).abs() < 1e-9 * t && (hi - t * r).abs() < 1e-9 * t);
    assert!((p.db_above_kt0().unwrap() - (p.dbm_per_hz().unwrap() + 173.975)).abs() < 1e-3);

    let l0 = rig.floor_vs_time(0, 6, Resolution::Level(0));
    let st = &l0.steps[3];
    assert_eq!(st.histogram_uncertainty_db, 0.0);
    assert_eq!(st.calibration_uncertainty_db, sigma_cal);
    assert!(st.statistical_uncertainty_db > 0.0 && st.statistical_uncertainty_db < 0.1);
    let want = (0.5f64.powi(2) + sigma_cal.powi(2) + st.statistical_uncertainty_db.powi(2)).sqrt();
    assert!((st.uncertainty_db - want).abs() < 1e-12);
    assert_eq!(
        st.noise_temperature_k(),
        st.dbm_per_hz().map(noise_temperature_k)
    );

    rig.product.seal_through(ts(T0 + 3600 * S)).unwrap();
    let l1 = rig.floor_vs_time(0, 6, Resolution::Level(1));
    let st = l1.steps.iter().find(|s| !s.is_gap()).unwrap();
    let hist = 0.5 / 12f64.sqrt();
    assert!((st.histogram_uncertainty_db - hist).abs() < 1e-12);
    let want = combine_uncertainty(&[0.5, sigma_cal, hist, st.statistical_uncertainty_db]);
    assert!((st.uncertainty_db - want).abs() < 1e-12);
    eprintln!(
        "SPACE-050 σ: series {:.3} dB (stat {:.3}); L0 {:.3} dB; L1 {:.3} dB (hist {hist:.3})",
        p.uncertainty_db,
        p.statistical_uncertainty_db,
        l0.steps[3].uncertainty_db,
        st.uncertainty_db
    );
}

const BURST_START_S: f64 = 2.3;

/// +2 dB band-wide bursts of 200 ms at 20 % duty from 2.3 s.
fn bursty(t: f64) -> f64 {
    let on = t >= BURST_START_S && (t - BURST_START_S).rem_euclid(1.0) < 0.2;
    1e-4 * if on { 10f64.powf(0.2) } else { 1.0 }
}

fn bursty_rig(dir: &TempDir) -> (Rig, f64) {
    let (id, cals) = calibration(&[(G_A, -70.0)], 0.2);
    let mut rig = Rig::new(dir, 1024, 32, cals);
    rig.push(8.0, &provenance(G_A, Some(id)), bursty, Discontinuity::NONE);
    (rig, db(1e-4) - db(FS) - 70.0)
}

/// Impulsive duty: every burst period is flagged `gate-released` (and impulsive) in the series,
/// and the floor-vs-time steps holding bursts carry `gate-released`. The tile product's p10 is
/// robust to 20 % duty and stays within ±0.3 dB.
#[test]
fn space_050_impulsive_duty_flags_gate_released_periods() {
    let dir = TempDir::new("t021-bursts");
    let (rig, truth) = bursty_rig(&dir);
    let pts = rig.series.points();
    for b in 0..6 {
        let start = BURST_START_S + f64::from(b);
        let in_burst: Vec<&FloorPoint> = pts
            .iter()
            .filter(|p| (start..start + 0.2).contains(&t_s(p)))
            .collect();
        assert!(
            in_burst
                .iter()
                .any(|p| p.flags.contains(FloorFlags::GATE_RELEASED)),
            "burst {b} at {start:.1} s: gate release flagged"
        );
        assert!(
            in_burst[1..]
                .iter()
                .any(|p| p.flags.contains(FloorFlags::IMPULSIVE_PAUSED))
        );
    }
    // Frames (16.9 ms) that end before the first burst carry neither flag.
    let early: Vec<(f64, FloorFlags)> = pts
        .iter()
        .filter(|p| t_s(p) + 0.02 < BURST_START_S)
        .filter(|p| {
            p.flags
                .intersects(FloorFlags::GATE_RELEASED | FloorFlags::IMPULSIVE_PAUSED)
        })
        .map(|p| (t_s(p), p.flags))
        .collect();
    assert!(early.is_empty(), "flagged before the bursts: {early:?}");
    let fvt = rig.floor_vs_time(0, 8, Resolution::Level(0));
    for st in &fvt.steps {
        let row = step_t_s(st);
        assert_eq!(
            st.flags.contains(FloorFlags::GATE_RELEASED),
            row >= 2.0,
            "row {row}"
        );
        let err = st.dbm_per_hz().unwrap() - truth;
        assert!(
            err.abs() <= 0.3,
            "row {row}: tile floor under bursts {err:+.3} dB"
        );
    }
}

/// The frame series' slow floor under the same bursts: currently biased by T-005's gate-release
/// re-seed (+0.2 to +1.6 dB mean in its re-review). Enable when that fix lands.
#[test]
#[ignore = "depends on the T-005 slow-floor gate-release fix (target mean error ≤ 0.2 dB, p95 ≤ 0.5 dB)"]
fn space_050_series_slow_floor_under_bursts_within_half_db() {
    let dir = TempDir::new("t021-bursts-series");
    let (rig, truth) = bursty_rig(&dir);
    let mut errs: Vec<f64> = rig
        .series
        .points()
        .iter()
        .filter(|p| !p.flags.contains(FloorFlags::NOT_READY))
        .map(|p| p.dbm_per_hz().unwrap() - truth)
        .collect();
    errs.sort_by(f64::total_cmp);
    let mean = errs.iter().sum::<f64>() / errs.len() as f64;
    let p95 = errs[(errs.len() as f64 * 0.95) as usize];
    assert!(
        mean.abs() <= 0.2 && p95 <= 0.5,
        "mean {mean:+.3}, p95 {p95:+.3}"
    );
}
