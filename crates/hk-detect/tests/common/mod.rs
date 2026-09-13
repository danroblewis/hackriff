//! Shared helpers for the hk-detect integration tests: fast Gamma-domain frames with a known
//! floor, a scene driver, the real STFT → floor tracker → detector chain over ci8 IQ, and fixture
//! loading.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_detect::{
    ClipCount, Confirmation, DetectionRecord, Detector, DetectorEvent, IntegratedEvaluation,
    count_clipped_ci8,
};
use hk_dsp::floor::{FloorConfig, FloorFrame, FloorMethod, GainKey, NoiseFloorTracker, gamma};
use hk_dsp::window::{Window, WindowKind};
use hk_dsp::{
    InputInfo, Resolution, Spectrum, SpectrumFrame, StftConfig, StftProcessor, WelchConfig,
};
use hk_e2e::{Cf32, DetectionBox, Fixture};
use hk_model::{DetectionId, Provenance, SampleTime, Timestamp};
use num_complex::Complex;

/// S4 geometry: 20 Msps, 4096 bins (4.88 kHz), 10 averages (2.05 ms frames).
pub const FS: f64 = 20e6;
pub const BINS: usize = 4096;
pub const N_AVG: u32 = 10;

pub fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

pub fn undb(x: f64) -> f64 {
    10f64.powf(x / 10.0)
}

/// A provenance record (built from JSON so model additions do not break the tests).
pub fn provenance_full(
    center_hz: f64,
    sample_rate_hz: f64,
    lna_db: f64,
    bandwidth_hz: f64,
    overload: bool,
    quantisation_limited: bool,
) -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:hk-detect-test",
        "tune": {"center_hz": center_hz, "sample_rate_hz": sample_rate_hz, "lna_db": lna_db,
                 "vga_db": 20.0, "amp_on": false, "bandwidth_hz": bandwidth_hz},
        "overload": overload, "quantisation_limited": quantisation_limited,
        "clock_source": "internal", "clock_locked": true, "timestamp_method": "synthetic",
        "timestamp_error_budget_ns": 0,
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).expect("provenance JSON"))
}

/// 75 % baseband filter (a 15 MHz filter at 20 Msps: usable ±8 MHz).
pub fn provenance(center_hz: f64, sample_rate_hz: f64, lna_db: f64) -> ProvenanceHandle {
    provenance_full(
        center_hz,
        sample_rate_hz,
        lna_db,
        sample_rate_hz * 0.75,
        false,
        false,
    )
}

/// SplitMix64.
#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1).
    #[inline]
    pub fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
}

/// Inverse-CDF table for unit-mean `Gamma(n)`: `x(v)` on a grid of `v = −ln(tail)` for each tail,
/// linearly interpolated (error ≪ 1e-6 relative; tails exact to e^−40).
pub struct GammaTable {
    step: f64,
    lower: Vec<f64>,
    upper: Vec<f64>,
}

impl GammaTable {
    fn build(n: u32) -> Self {
        let step = 0.002;
        let len = (40.0 / step) as usize + 2;
        let a = f64::from(n);
        let mut lower = Vec::with_capacity(len);
        let mut upper = Vec::with_capacity(len);
        for i in 0..len {
            let v = (i as f64 * step).max(std::f64::consts::LN_2);
            let p = (-v).exp();
            lower.push(gamma::inverse_lower(a, p) / a);
            upper.push(gamma::inverse_upper(a, p) / a);
        }
        Self { step, lower, upper }
    }

    pub fn get(n: u32) -> &'static GammaTable {
        static TABLES: OnceLock<Mutex<Vec<(u32, &'static GammaTable)>>> = OnceLock::new();
        let tables = TABLES.get_or_init(|| Mutex::new(Vec::new()));
        let mut t = tables.lock().unwrap();
        if let Some((_, tab)) = t.iter().find(|(k, _)| *k == n) {
            return tab;
        }
        let tab: &'static GammaTable = Box::leak(Box::new(GammaTable::build(n)));
        t.push((n, tab));
        tab
    }

    #[inline]
    pub fn sample(&self, u: f64) -> f32 {
        let (v, tab) = if u < 0.5 {
            (-u.ln(), &self.lower)
        } else {
            (-(1.0 - u).ln(), &self.upper)
        };
        let x = v / self.step;
        let i = x as usize;
        if i + 1 >= tab.len() {
            return tab[tab.len() - 1] as f32;
        }
        let f = x - i as f64;
        (tab[i] + f * (tab[i + 1] - tab[i])) as f32
    }
}

/// Gamma-domain spectrum frames: each bin is `profile · Gamma(n)/n`, exactly the statistics of an
/// `n`-average periodogram of complex Gaussian noise without overlap.
pub struct GammaFrames {
    pub fs: f64,
    pub bins: usize,
    pub n_avg: u32,
    pub provenance: ProvenanceHandle,
    pub rng: Rng,
    pub table: &'static GammaTable,
    pub seq: u64,
    pub sample_index: u64,
}

impl GammaFrames {
    pub fn new(bins: usize, n_avg: u32, provenance: ProvenanceHandle, seed: u64) -> Self {
        Self {
            fs: provenance.tune.sample_rate_hz,
            bins,
            n_avg,
            provenance,
            rng: Rng(seed),
            table: GammaTable::get(n_avg),
            seq: 0,
            sample_index: 0,
        }
    }

    pub fn frame_samples(&self) -> u64 {
        u64::from(self.n_avg) * self.bins as u64
    }

    pub fn frame_period_s(&self) -> f64 {
        self.frame_samples() as f64 / self.fs
    }

    pub fn empty_frame(&self) -> SpectrumFrame {
        let n = self.bins;
        let metrics = Window::new(WindowKind::Hann, n).metrics();
        let resolution = Resolution {
            window: WindowKind::Hann,
            fft_len: n,
            overlap: 0,
            n_avg: self.n_avg,
            bin_width_hz: self.fs / n as f64,
            rbw_hz: metrics.enbw_bins * self.fs / n as f64,
            window_metrics: metrics,
        };
        SpectrumFrame {
            seq: 0,
            t: SampleTime {
                sample_index: 0,
                host_time: Timestamp::UNIX_EPOCH,
            },
            sample_count: self.frame_samples(),
            provenance: self.provenance.clone(),
            provenance_changed: false,
            discontinuity: Discontinuity::NONE,
            dropped_samples: 0,
            spectrum: Spectrum {
                f_center_hz: self.provenance.tune.center_hz,
                sample_rate_hz: self.fs,
                resolution,
                psd: vec![0.0; n],
                max_hold: Vec::new(),
                min_hold: Vec::new(),
                sk: Vec::new(),
            },
        }
    }

    /// Regenerates `frame` in place (no allocation) from per-bin means `profile`.
    pub fn fill(&mut self, frame: &mut SpectrumFrame, profile: &[f32], flags: Discontinuity) {
        assert_eq!(profile.len(), self.bins);
        for (p, &m) in frame.spectrum.psd.iter_mut().zip(profile) {
            *p = m * self.table.sample(self.rng.unit());
        }
        self.stamp(frame, flags);
    }

    /// Writes `profile` exactly (no noise).
    pub fn fill_exact(&mut self, frame: &mut SpectrumFrame, profile: &[f32], flags: Discontinuity) {
        frame.spectrum.psd.copy_from_slice(profile);
        self.stamp(frame, flags);
    }

    fn stamp(&mut self, frame: &mut SpectrumFrame, flags: Discontinuity) {
        let index = self.sample_index;
        frame.seq = self.seq;
        frame.t = SampleTime {
            sample_index: index,
            host_time: Timestamp::from_unix_nanos((index as f64 * 1e9 / self.fs).round() as i64),
        };
        if frame.provenance != self.provenance {
            frame.provenance = self.provenance.clone();
        }
        frame.spectrum.f_center_hz = self.provenance.tune.center_hz;
        frame.discontinuity = flags;
        self.seq += 1;
        self.sample_index += self.frame_samples();
    }

    /// Host time of stream sample `index`.
    pub fn time_of(&self, index: u64) -> Timestamp {
        Timestamp::from_unix_nanos((index as f64 * 1e9 / self.fs).round() as i64)
    }
}

/// A floor frame with a known floor (`floor == wide_floor == slow_floor`).
pub fn floor_frame(frame: &SpectrumFrame, floor: &[f32], segment: u64) -> FloorFrame {
    FloorFrame {
        seq: frame.seq,
        t: frame.t,
        provenance: frame.provenance.clone(),
        gain: GainKey::of(frame),
        segment,
        frames_in_segment: 1,
        reset: false,
        method: FloorMethod::BlockFcme,
        n_avg_effective: f64::from(frame.spectrum.resolution.n_avg),
        f_center_hz: frame.spectrum.f_center_hz,
        bin_width_hz: frame.spectrum.bin_width_hz(),
        valid: true,
        floor: floor.to_vec(),
        wide_floor: floor.to_vec(),
        band_floor: 1.0,
        block_floor: Vec::new(),
        block_valid: Vec::new(),
        block_iterations: Vec::new(),
        valid_blocks: 1,
        unconverged_blocks: 0,
        slow_floor: floor.to_vec(),
        slow_band_floor: 1.0,
        block_slow: Vec::new(),
        slow_ready: true,
        occupancy: 0.0,
        percentile: None,
        uncertainty_db: 0.5,
        statistical_uncertainty_db: 0.0,
        quantisation_floor_dbfs_per_hz: None,
        quantisation_margin_db: 3.0,
        quantisation_limited: false,
        impulsive: false,
        gate_released: false,
        impulsive_excess_db: 0.0,
        active_episodes: 0,
    }
}

/// Updates a known-floor frame for the next spectrum frame (no allocation).
pub fn refresh_floor(ff: &mut FloorFrame, frame: &SpectrumFrame, segment: u64, impulsive: bool) {
    ff.seq = frame.seq;
    ff.t = frame.t;
    if ff.provenance != frame.provenance {
        ff.provenance = frame.provenance.clone();
    }
    ff.gain = GainKey::of(frame);
    ff.segment = segment;
    ff.f_center_hz = frame.spectrum.f_center_hz;
    ff.impulsive = impulsive;
}

/// Everything a detector emitted.
#[derive(Default, Debug)]
pub struct Collected {
    pub detections: Vec<DetectionRecord>,
    pub confirmations: Vec<Confirmation>,
    pub evaluations: Vec<IntegratedEvaluation>,
}

impl Collected {
    pub fn sink(&mut self) -> impl FnMut(DetectorEvent<'_>) + '_ {
        move |e| match e {
            DetectorEvent::Detection(r) => self.detections.push(r),
            DetectorEvent::Confirmed(c) => self.confirmations.push(c),
            DetectorEvent::Integrated(ev) => self.evaluations.push(ev.clone()),
        }
    }

    /// Detections confirmed at emission or later.
    pub fn confirmed(&self) -> Vec<DetectionId> {
        let mut v: Vec<DetectionId> = self
            .detections
            .iter()
            .filter(|d| d.candidate.is_confirmed())
            .map(|d| d.detection.id)
            .chain(self.confirmations.iter().map(|c| c.detection))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn sorted(&self) -> Vec<&DetectionRecord> {
        let mut v: Vec<&DetectionRecord> = self.detections.iter().collect();
        v.sort_by_key(|d| (d.samples.start, d.bins.start));
        v
    }
}

/// A known-floor scene: Gamma frames → detector.
pub struct Scene {
    pub det: Detector,
    pub src: GammaFrames,
    pub frame: SpectrumFrame,
    pub floor: FloorFrame,
    pub segment: u64,
    pub out: Collected,
}

impl Scene {
    pub fn new(config: hk_detect::DetectorConfig, src: GammaFrames) -> Self {
        let frame = src.empty_frame();
        let floor = floor_frame(&frame, &vec![1.0; src.bins], 0);
        Self {
            det: Detector::new(config).expect("config"),
            src,
            frame,
            floor,
            segment: 0,
            out: Collected::default(),
        }
    }

    pub fn step(&mut self, profile: &[f32]) {
        self.step_with(profile, Discontinuity::NONE, ClipCount::NONE, false);
    }

    pub fn step_with(
        &mut self,
        profile: &[f32],
        flags: Discontinuity,
        clip: ClipCount,
        impulsive: bool,
    ) {
        self.src.fill(&mut self.frame, profile, flags);
        refresh_floor(&mut self.floor, &self.frame, self.segment, impulsive);
        self.det
            .process(&self.frame, &self.floor, clip, &mut self.out.sink());
    }

    pub fn step_exact(&mut self, profile: &[f32], flags: Discontinuity) {
        self.src.fill_exact(&mut self.frame, profile, flags);
        refresh_floor(&mut self.floor, &self.frame, self.segment, false);
        self.det.process(
            &self.frame,
            &self.floor,
            ClipCount::NONE,
            &mut self.out.sink(),
        );
    }

    /// Switches to a new provenance (and floor segment), as a gain change or retune does.
    pub fn switch(&mut self, provenance: ProvenanceHandle) {
        self.src.provenance = provenance;
        self.src.fs = self.src.provenance.tune.sample_rate_hz;
        self.segment += 1;
    }

    pub fn finish(&mut self) {
        self.det.finish(&mut self.out.sink());
    }

    pub fn bin_of(&self, f_hz: f64) -> f64 {
        (self.src.bins / 2) as f64
            + (f_hz - self.src.provenance.tune.center_hz) / (self.src.fs / self.src.bins as f64)
    }
}

pub fn flat(bins: usize) -> Vec<f32> {
    vec![1.0; bins]
}

/// Adds a rectangular line `width` bins wide centred on `center` at `snr_db` above the floor.
pub fn add_line(profile: &mut [f32], center: usize, width: usize, snr_db: f64) {
    let lin = undb(snr_db) as f32;
    let lo = center.saturating_sub(width / 2);
    let hi = (lo + width).min(profile.len());
    for p in &mut profile[lo..hi] {
        *p += lin;
    }
}

/// Adds a 3-bin triangular line centred at fractional bin `x` (its centroid is `x`).
pub fn add_fractional_line(profile: &mut [f32], x: f64, snr_db: f64) {
    let lin = undb(snr_db);
    let c = x.round() as isize;
    for b in (c - 2)..=(c + 2) {
        let w = (1.0 - (b as f64 - x).abs() / 1.5).max(0.0);
        if w > 0.0 && b >= 0 && (b as usize) < profile.len() {
            profile[b as usize] += (lin * w) as f32;
        }
    }
}

/// The real chain: ci8 IQ → STFT (Hann, no overlap) → noise-floor tracker → detector.
#[derive(Clone, Copy, Debug)]
pub struct ChainConfig {
    pub fft_len: usize,
    pub averages: usize,
    pub floor: FloorConfig,
}

impl ChainConfig {
    pub fn new(fft_len: usize, averages: usize) -> Self {
        Self {
            fft_len,
            averages,
            floor: FloorConfig::default(),
        }
    }

    pub fn frame_period_s(&self, fs: f64) -> f64 {
        (self.fft_len * self.averages) as f64 / fs
    }
}

/// Replays `samples` (captures start at the given sample indices with their provenance) through
/// the chain into `det`; returns the events and the number of frames.
pub fn replay_ci8(
    samples: &[Complex<i8>],
    fs: f64,
    captures: &[(u64, ProvenanceHandle)],
    chain: &ChainConfig,
    det: &mut Detector,
) -> (Collected, u64) {
    let welch = WelchConfig {
        fft_len: chain.fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, chain.averages)).expect("stft");
    let mut tracker = NoiseFloorTracker::new(chain.floor).expect("floor");
    let mut out = Collected::default();
    let mut frames = 0u64;
    let n = samples.len() as u64;
    for (i, (start, prov)) in captures.iter().enumerate() {
        let end = captures.get(i + 1).map_or(n, |c| c.0);
        let mut s = *start;
        while s < end {
            let e = (s + 65_536).min(end);
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: s,
                    host_time: Timestamp::from_unix_nanos((s as f64 * 1e9 / fs).round() as i64),
                },
                provenance: prov.clone(),
                discontinuity: if s == 0 {
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                },
                dropped_before: 0,
            };
            stft.push(
                InputInfo::from(&header),
                &samples[s as usize..e as usize],
                |frame| {
                    let f = tracker.update(frame, |_| {});
                    let a = frame.t.sample_index as usize;
                    let b = (a + frame.sample_count as usize).min(samples.len());
                    let clip = ClipCount::new(count_clipped_ci8(&samples[a..b]), (b - a) as u64);
                    det.process(frame, f, clip, &mut out.sink());
                    frames += 1;
                },
            );
            s = e;
        }
    }
    det.finish(&mut out.sink());
    (out, frames)
}

/// `round(x·127)` (the generator's ci8 scaling; hk-e2e reads ci8 as `/127`).
pub fn to_ci8(samples: &[Cf32]) -> Vec<Complex<i8>> {
    samples
        .iter()
        .map(|s| {
            Complex::new(
                (s.re * 127.0).round().clamp(-128.0, 127.0) as i8,
                (s.im * 127.0).round().clamp(-128.0, 127.0) as i8,
            )
        })
        .collect()
}

/// A fixture's provenance (global, else synthetic from the first capture).
pub fn fixture_provenance(fx: &Fixture) -> ProvenanceHandle {
    let cap = &fx.meta.captures[0];
    match cap
        .provenance
        .clone()
        .or_else(|| fx.meta.global.provenance.clone())
    {
        Some(p) => ProvenanceHandle::new(p),
        None => provenance(cap.frequency.unwrap_or(0.0), fx.sample_rate, 24.0),
    }
}

pub fn hackrf_fixture(name: &str) -> PathBuf {
    hk_e2e::paths::repo_root()
        .join("fixtures/hackrf/2026-09-13")
        .join(format!("{name}.sigmf-meta"))
}

/// Loads a ci8 HackRF fixture as raw codes, or `None` (with a skip message) when the data is an
/// unfetched Git LFS pointer. `HK_REQUIRE_FIXTURES=1` turns the skip into a failure.
pub fn load_ci8_fixture(name: &str) -> Option<(Fixture, Vec<Complex<i8>>)> {
    let fx = Fixture::load(hackrf_fixture(name)).expect("fixture metadata");
    let bytes = std::fs::read(fx.data_path()).expect("fixture data");
    let expected = fx
        .scenario()
        .and_then(|s| s.f64("n_samples"))
        .map(|n| n as usize * 2);
    if expected.is_some_and(|e| e != bytes.len()) || bytes.starts_with(b"version https://git-lfs") {
        if std::env::var("HK_REQUIRE_FIXTURES").is_ok_and(|v| v == "1") {
            panic!("{name}: fixture data not fetched (git lfs pull)");
        }
        eprintln!("SKIP {name}: fixture data is not fetched (git lfs pull)");
        return None;
    }
    let iq = bytes
        .chunks_exact(2)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect();
    Some((fx, iq))
}

/// A record as a harness box (seconds from sample 0).
pub fn to_box(d: &DetectionRecord, fs: f64) -> DetectionBox {
    let f = &d.detection.flags;
    let mut flags = Vec::new();
    for (on, name) in [
        (f.clipped, "clipped"),
        (f.spur_candidate, "spur_candidate"),
        (f.image_candidate, "image_candidate"),
        (f.marginal, "marginal"),
        (f.impulsive, "impulsive"),
        (f.edge, "edge"),
    ] {
        if on {
            flags.push(name.to_owned());
        }
    }
    DetectionBox {
        t_start_s: d.t_start_s(fs),
        t_end_s: d.t_end_s(fs),
        f_lo_hz: d.f_lo_hz,
        f_hi_hz: d.f_hi_hz,
        snr_db: Some(d.detection.snr_peak_db),
        flags,
    }
}

/// One-line description of a record.
pub fn describe(d: &DetectionRecord, fs: f64) -> String {
    let f = &d.detection.flags;
    format!(
        "{:.4} MHz [{:.4}, {:.4}] obw {:.1} kHz t [{:.4}, {:.4}] s snr {:.1}/{:.1} dB lvl {:.1} dBFS clip {} {:?}{}{}{}{}{} cand {:?} {:?}",
        d.detection.f_center_hz / 1e6,
        d.f_lo_hz / 1e6,
        d.f_hi_hz / 1e6,
        d.detection.obw_hz / 1e3,
        d.t_start_s(fs),
        d.t_end_s(fs),
        d.detection.snr_peak_db,
        d.detection.snr_mean_db,
        d.detection.peak_level_dbfs,
        d.detection.clip_count,
        f.spur_reason.map(|r| r.kind_str()),
        if f.clipped { " clipped" } else { "" },
        if f.marginal { " marginal" } else { "" },
        if f.edge { " edge" } else { "" },
        if f.image_candidate { " image" } else { "" },
        if f.impulsive { " impulsive" } else { "" },
        d.candidate,
        d.close,
    )
}
