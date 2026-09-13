//! T-036 item 4: steady noise-like wide emissions against AWARE-006 (GNSS noise-jammer floor rise)
//! now that the detector's floor reference is Wide (T-033).
//!
//! T-033 made a stationary noise-like emission read as a *floor feature* on the detection
//! reference ([`hk_detect::step`] "Limits"), so the CFAR floor branch no longer reports its
//! interior. AWARE-006 does not depend on the detector: the floor tracker's change episodes are
//! computed from the raw block floors, and the T-020 Anomaly lifecycle (`hk_context::FloorAnomalies`)
//! opens an Anomaly on a `NoiseLike` `Rise` (`accepts`: NoiseLike always, Unverified only when
//! configured, Structured never). These tests replay synthetic IQ as ci8 through the real chain
//! (STFT with SK → `NoiseFloorTracker` → `Detector`, default `FloorReference::Wide`) and record,
//! per scenario, the floor episodes, the detector's output and the reference bias under the
//! emission. The Anomaly itself for the broadband case is proven end to end by
//! `crates/hk-context/tests/aware_006_e2e.rs` (same synth, same seed); here the regression pins
//! the episode that lifecycle opens on, on the chain the detector runs.
//!
//! Scenarios (2 Msps at GNSS L1, −40 dBFS noise, 1024-bin FFT × 10 averages = 5.12 ms frames,
//! +10 dB rise from t0 = 1 s, back to the original floor at 3 s, 4.5 s total):
//! - broadband noise jammer: T-023 `noise_floor_rise` as-is (whole 2 MHz span);
//! - partial-band noise jammer: `rise_bandwidth_hz` = 800 kHz, offset +200 kHz (410 bins);
//! - steady OFDM-like signal: 64 QPSK subcarriers × 15.625 kHz (1 MHz, CP 1/4), added in Rust.
//!
//! Tests skip when `uv` is missing (`HK_E2E_REQUIRE_SYNTH=1` makes that a failure).

mod common;

use std::ops::Range;

use common::*;
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_detect::{ClipCount, Detector, DetectorConfig, count_clipped_ci8};
use hk_dsp::floor::{
    EndReason, FloorChangeClass, FloorConfig, FloorEvent, FloorEventKind, NoiseFloorTracker,
};
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_e2e::{Cf32, SynthRequest};
use hk_model::{SampleTime, SurveyId, Timestamp};
use num_complex::Complex;

const AWARE_006: &str = "AWARE-006";
const FFT: usize = 1024;
const AVG: usize = 10;
const T0_S: f64 = 1.0;
const T_BACK_S: f64 = 3.0;
const STEP_DB: f64 = 10.0;
/// The End confirms `end_s` (1 s) after the return at 3 s, at ≈ 4.0 s.
const TOTAL_S: f64 = 4.5;
/// Detections narrower than this are the scenario's weak CW tones, not the wide emission.
const NARROW_HZ: f64 = 20e3;

/// What one replay produced.
struct Replay {
    name: &'static str,
    fs: f64,
    center_hz: f64,
    /// Emission extent, Hz (absolute).
    band: (f64, f64),
    events: Vec<FloorEvent>,
    /// Wide detections (obw ≥ [`NARROW_HZ`]) with `(t_start_s, t_end_s, f_lo_hz, f_hi_hz)`.
    wide_detections: Vec<(f64, f64, f64, f64)>,
    detections: usize,
    frames: u64,
    guarded_frames: u64,
    wide_guarded_frames: u64,
    /// Median over the inner 80 % of the band, over steady frames (t0 + 1.5 s … 3 s − 0.1 s), of
    /// reference dB − the same bins' pre-rise reference dB: floor-branch reference, wide floor,
    /// per-frame floor.
    bias_db: [f64; 3],
}

fn t_of(t: SampleTime, fs: f64) -> f64 {
    t.sample_index as f64 / fs
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        f64::NAN
    } else {
        v[v.len() / 2]
    }
}

fn replay(
    name: &'static str,
    iq: &[Complex<i8>],
    fs: f64,
    prov: ProvenanceHandle,
    band: (f64, f64),
) -> Replay {
    let center_hz = prov.tune.center_hz;
    let welch = WelchConfig {
        fft_len: FFT,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, AVG)).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
    let mut out = Collected::default();
    let mut events = Vec::new();
    let mut frames = 0u64;
    let bin_hz = fs / FFT as f64;
    let bin = |f: f64| ((f - center_hz) / bin_hz + (FFT / 2) as f64).round() as isize;
    let (lo, hi) = (
        bin(band.0).max(0) as usize,
        (bin(band.1).max(0) as usize).min(FFT),
    );
    let inner: Range<usize> = (lo + (hi - lo) / 10)..(hi - (hi - lo) / 10);
    // Pre-rise sums and steady-window samples per reference.
    let mut pre = [vec![0f64; FFT], vec![0f64; FFT], vec![0f64; FFT]];
    let mut pre_n = 0u64;
    let mut steady: [Vec<f64>; 3] = Default::default();
    let n = iq.len() as u64;
    let mut s = 0u64;
    while s < n {
        let e = (s + 65_536).min(n);
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
            &iq[s as usize..e as usize],
            |frame| {
                let f = tracker.update(frame, |ev| events.push(ev.clone()));
                let a = frame.t.sample_index as usize;
                let b = (a + frame.sample_count as usize).min(iq.len());
                let clip = ClipCount::new(count_clipped_ci8(&iq[a..b]), (b - a) as u64);
                det.process(frame, f, clip, &mut out.sink());
                frames += 1;
                let t = t_of(frame.t, fs);
                let refs: [&[f32]; 3] = [det.floor_branch_reference(), &f.wide_floor, &f.floor];
                if (0.5..T0_S - 0.05).contains(&t) {
                    for (acc, r) in pre.iter_mut().zip(refs) {
                        for i in inner.clone() {
                            acc[i] += 10.0 * f64::from(r[i]).log10();
                        }
                    }
                    pre_n += 1;
                } else if (T0_S + 1.5..T_BACK_S - 0.1).contains(&t) && pre_n > 0 {
                    for ((acc, r), st) in pre.iter().zip(refs).zip(steady.iter_mut()) {
                        let mut d: Vec<f64> = inner
                            .clone()
                            .map(|i| 10.0 * f64::from(r[i]).log10() - acc[i] / pre_n as f64)
                            .collect();
                        st.push(median(&mut d));
                    }
                }
            },
        );
        s = e;
    }
    det.finish(&mut out.sink());
    let stats = det.stats();
    let wide_detections = out
        .detections
        .iter()
        .filter(|d| d.detection.obw_hz >= NARROW_HZ || d.f_hi_hz - d.f_lo_hz >= NARROW_HZ)
        .map(|d| (d.t_start_s(fs), d.t_end_s(fs), d.f_lo_hz, d.f_hi_hz))
        .collect();
    Replay {
        name,
        fs,
        center_hz,
        band,
        events,
        wide_detections,
        detections: out.detections.len(),
        frames,
        guarded_frames: stats.guarded_frames,
        wide_guarded_frames: stats.wide_guarded_frames,
        bias_db: steady.map(|mut v| median(&mut v)),
    }
}

impl Replay {
    fn of(&self, kind: FloorEventKind) -> Vec<&FloorEvent> {
        self.events.iter().filter(|e| e.kind == kind).collect()
    }

    fn frame_period_s(&self) -> f64 {
        (FFT * AVG) as f64 / self.fs
    }

    /// Wide detections during the emission (t0 … back) by position: (edge, interior, outside).
    /// An edge detection spans or lies within 2 block widths (512 bins) of a band edge.
    fn cfar(&self) -> (usize, usize, usize) {
        let tol = 512.0 * self.fs / FFT as f64 / 2.0;
        let (lo, hi) = self.band;
        let mut c = (0, 0, 0);
        for &(t0, t1, f_lo, f_hi) in &self.wide_detections {
            if t1 < T0_S || t0 > T_BACK_S {
                continue;
            }
            let near = |edge: f64| f_lo - tol <= edge && edge <= f_hi + tol;
            if (near(lo) && lo > self.center_hz - self.fs / 2.0 + 1.0)
                || (near(hi) && hi < self.center_hz + self.fs / 2.0 - 1.0)
            {
                c.0 += 1;
            } else if f_lo >= lo && f_hi <= hi {
                c.1 += 1;
            } else {
                c.2 += 1;
            }
        }
        c
    }

    fn report(&self) {
        let (edge, interior, outside) = self.cfar();
        eprintln!(
            "{AWARE_006} {}: {} frames, band {:.3}-{:.3} MHz; {} detections ({} wide: edge {edge}, interior {interior}, outside {outside}); guarded frames {} (wide {}); steady bias dB vs pre-rise: detector ref {:+.2}, wide {:+.2}, frame {:+.2}",
            self.name,
            self.frames,
            self.band.0 / 1e6,
            self.band.1 / 1e6,
            self.detections,
            self.wide_detections.len(),
            self.guarded_frames,
            self.wide_guarded_frames,
            self.bias_db[0],
            self.bias_db[1],
            self.bias_db[2],
        );
        for e in &self.events {
            eprintln!(
                "  {:?} ep {} {:?} onset {:+.4} s (t {:.4}) confirmed {:.4} s  {:.3}-{:.3} MHz step {:.2} dB sk {:?} excess {:.3} dB end {:?}",
                e.kind,
                e.episode,
                e.class,
                t_of(e.onset_t, self.fs) - T0_S,
                t_of(e.onset_t, self.fs),
                t_of(e.confirmed_t, self.fs),
                e.f_lo_hz / 1e6,
                e.f_hi_hz / 1e6,
                e.step_db,
                e.sk,
                e.excess_std_db,
                e.end_reason,
            );
        }
        for d in &self.wide_detections {
            eprintln!(
                "  det t [{:.4}, {:.4}] f [{:.4}, {:.4}] MHz",
                d.0,
                d.1,
                d.2 / 1e6,
                d.3 / 1e6
            );
        }
    }

    /// The AWARE-006 contract: exactly one Rise, NoiseLike (the class `FloorAnomalies` opens an
    /// Anomaly on), onset within one frame of t0, covering the emission, closed by one Returned
    /// End once the floor is back.
    fn assert_noise_like_episode(&self, min_cover: f64) {
        let rises = self.of(FloorEventKind::Rise);
        let ends = self.of(FloorEventKind::End);
        assert_eq!(
            rises.len(),
            1,
            "{AWARE_006} {}: one Rise: {:?}",
            self.name,
            self.events
        );
        let r = rises[0];
        assert_eq!(
            r.class,
            FloorChangeClass::NoiseLike,
            "{AWARE_006} {}: NoiseLike rise (FloorAnomalies opens an Anomaly only on it)",
            self.name
        );
        let onset_err = t_of(r.onset_t, self.fs) - T0_S;
        assert!(
            onset_err.abs() <= self.frame_period_s() + 1e-6,
            "{AWARE_006} {}: onset {onset_err:+.4} s from t0",
            self.name
        );
        assert!(
            (f64::from(r.step_db) - STEP_DB).abs() <= 1.0,
            "{AWARE_006} {}: step {} dB",
            self.name,
            r.step_db
        );
        let (lo, hi) = self.band;
        let covered = (r.f_hi_hz.min(hi) - r.f_lo_hz.max(lo)).max(0.0) / (hi - lo);
        assert!(
            covered >= min_cover,
            "{AWARE_006} {}: rise covers {covered:.2} of the emission",
            self.name
        );
        let tol = 256.0 * self.fs / FFT as f64;
        assert!(
            r.f_lo_hz >= lo - tol && r.f_hi_hz <= hi + tol,
            "{AWARE_006} {}: rise {:.3}-{:.3} MHz spills outside the emission",
            self.name,
            r.f_lo_hz / 1e6,
            r.f_hi_hz / 1e6
        );
        assert_eq!(
            ends.len(),
            1,
            "{AWARE_006} {}: one End: {:?}",
            self.name,
            self.events
        );
        assert_eq!(ends[0].episode, r.episode);
        assert_eq!(ends[0].end_reason, Some(EndReason::Returned));
    }

    /// The T-033 limit as measured: the CFAR sees a steady noise-like emission only until the
    /// step guard has classified it floor-like (wide detections, if any, start near t0 and end
    /// within 1 s of it); in the steady window no wide detection exists and the floor-branch
    /// reference reads the emission as floor (≥ +8 dB over the pre-rise reference).
    /// `edges`: a partial-band emission's early detections reach its band edges; a span-wide one
    /// has none.
    fn assert_absorbed_into_the_reference(&self, edges: bool) {
        let during: Vec<_> = self
            .wide_detections
            .iter()
            .filter(|d| d.1 >= T0_S && d.0 <= T_BACK_S)
            .collect();
        for d in &during {
            assert!(
                d.0 <= T0_S + 0.2 && d.1 <= T0_S + 1.0,
                "{AWARE_006} {}: wide detection t [{:.3}, {:.3}] s outside the pre-classification window",
                self.name,
                d.0,
                d.1
            );
        }
        let (edge, _, _) = self.cfar();
        if edges {
            assert!(edge >= 1, "{AWARE_006} {}: early edge detection", self.name);
        } else {
            assert!(during.is_empty(), "{AWARE_006} {}: {during:?}", self.name);
        }
        assert!(
            self.bias_db[0] >= 8.0,
            "{AWARE_006} {}: steady floor-branch reference bias {:+.2} dB",
            self.name,
            self.bias_db[0]
        );
    }
}

fn to_ci8(samples: &[Cf32]) -> Vec<Complex<i8>> {
    common::to_ci8(samples)
}

/// `noise_floor_rise` with a +10 dB step at t0 for 3 s, then 2.5 s of the scene without the step.
fn floor_rise(params: &[(&str, f64)]) -> Option<(Vec<Complex<i8>>, f64, ProvenanceHandle)> {
    let with = |r: SynthRequest| params.iter().fold(r, |r, (k, v)| r.param(*k, *v));
    let gen_one = |r: SynthRequest| match r.generate() {
        Ok(out) => Some(out.fixture(0).unwrap()),
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP aware_006_wide_emissions: {err}");
            None
        }
        Err(err) => panic!("{AWARE_006}: synthetic scenario generation failed: {err}"),
    };
    let rise = gen_one(with(
        SynthRequest::new("noise_floor_rise")
            .seed(3)
            .param("duration_s", T_BACK_S)
            .param("t0_s", T0_S),
    ))?;
    let back = gen_one(with(
        SynthRequest::new("noise_floor_rise")
            .seed(3)
            .param("duration_s", TOTAL_S - T_BACK_S)
            .param("t0_s", 0.5)
            .param("step_db", 0.0),
    ))?;
    let mut iq = to_ci8(&rise.samples().unwrap());
    iq.extend(to_ci8(&back.samples().unwrap()));
    let prov = fixture_provenance(&rise);
    Some((iq, rise.sample_rate, prov))
}

#[test]
fn aware_006_broadband_noise_jammer_still_opens_a_noise_like_rise_on_the_wide_reference() {
    let Some((iq, fs, prov)) = floor_rise(&[]) else {
        return;
    };
    let c = prov.tune.center_hz;
    let r = replay("broadband", &iq, fs, prov, (c - fs / 2.0, c + fs / 2.0));
    r.report();
    r.assert_noise_like_episode(0.8);
    r.assert_absorbed_into_the_reference(false);
}

#[test]
fn aware_006_partial_band_noise_jammer_characterisation() {
    let Some((iq, fs, prov)) = floor_rise(&[
        ("rise_bandwidth_hz", 800e3),
        ("rise_offset_hz", 200e3),
        ("n_weak_signals", 0.0),
    ]) else {
        return;
    };
    let c = prov.tune.center_hz + 200e3;
    let r = replay(
        "partial-band 800 kHz",
        &iq,
        fs,
        prov,
        (c - 400e3, c + 400e3),
    );
    r.report();
    // The episode is built from whole 256-bin blocks, so its extent (375 kHz) under-reports an
    // 800 kHz emission; it stays inside it (checked).
    r.assert_noise_like_episode(0.4);
    r.assert_absorbed_into_the_reference(true);
}

/// 64 QPSK subcarriers (±32 around DC, DC unused) on a 128-sample symbol with a 32-sample cyclic
/// prefix at 2 Msps: 15.625 kHz spacing, 1 MHz occupied, flat top. Power set so the in-band PSD
/// is `STEP_DB` over the −40 dBFS / 2 MHz floor.
fn add_ofdm(samples: &mut [Cf32], fs: f64, noise_dbfs: f64, from: usize, to: usize, seed: u64) {
    const N: usize = 128;
    const CP: usize = 32;
    let used: Vec<i32> = (-32..=32).filter(|&k| k != 0).collect();
    let floor_per_hz = undb(noise_dbfs) / fs;
    let bw = used.len() as f64 * fs / N as f64;
    let power = floor_per_hz * (undb(STEP_DB) - 1.0) * bw;
    let amp = (power / used.len() as f64).sqrt() as f32;
    let table: Vec<Vec<Complex<f32>>> = used
        .iter()
        .map(|&k| {
            (0..N)
                .map(|n| {
                    Complex::from_polar(1.0, std::f32::consts::TAU * k as f32 * n as f32 / N as f32)
                })
                .collect()
        })
        .collect();
    let mut rng = Rng(seed);
    let mut sym = vec![Complex::new(0f32, 0.0); N];
    let mut at = from;
    while at < to {
        sym.iter_mut().for_each(|s| *s = Complex::new(0.0, 0.0));
        for row in &table {
            let bits = rng.next_u64();
            let a = Complex::new(
                if bits & 1 == 0 { amp } else { -amp },
                if bits & 2 == 0 { amp } else { -amp },
            ) * std::f32::consts::FRAC_1_SQRT_2;
            for (s, p) in sym.iter_mut().zip(row) {
                *s += a * p;
            }
        }
        for i in 0..N + CP {
            let idx = at + i;
            if idx >= to {
                break;
            }
            let v = sym[(i + N - CP) % N];
            samples[idx].re += v.re;
            samples[idx].im += v.im;
        }
        at += N + CP;
    }
}

#[test]
fn aware_006_steady_ofdm_characterisation() {
    let base = match SynthRequest::new("noise_floor_rise")
        .seed(3)
        .param("duration_s", TOTAL_S)
        .param("t0_s", T0_S)
        .param("step_db", 0.0)
        .param("n_weak_signals", 0.0)
        .generate()
    {
        Ok(out) => out.fixture(0).unwrap(),
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP aware_006_steady_ofdm_characterisation: {err}");
            return;
        }
        Err(err) => panic!("{AWARE_006}: synthetic scenario generation failed: {err}"),
    };
    let fs = base.sample_rate;
    let mut samples = base.samples().unwrap();
    add_ofdm(
        &mut samples,
        fs,
        -40.0,
        (T0_S * fs) as usize,
        (T_BACK_S * fs) as usize,
        7,
    );
    let prov = fixture_provenance(&base);
    let c = prov.tune.center_hz;
    let half = 32.5 * fs / 128.0;
    let r = replay(
        "OFDM 1 MHz",
        &to_ci8(&samples),
        fs,
        prov,
        (c - half, c + half),
    );
    r.report();
    // Limit, not a goal: a steady OFDM signal's bins are Gaussian enough (SK 0.89, inside the 0.15
    // tolerance) that the tracker calls it NoiseLike, so it would open an AWARE-006 floor-rise
    // Anomaly like a jammer; correlation and priors must explain it. Pinned so a change shows.
    r.assert_noise_like_episode(0.5);
    r.assert_absorbed_into_the_reference(true);
}
