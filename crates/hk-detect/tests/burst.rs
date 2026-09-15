//! T-075 short-burst time-domain detector (SIGNAL-001, AWARE-036): synthetic ADS-B DF17-shaped
//! squitters (8 µs preamble + 112 µs PPM data) and short OOK bursts in complex Gaussian noise at
//! several SNRs, quantised to ci8. The truth (start, length, carrier) stays in the test; the
//! detector sees only samples. Also: the false-alarm rate on noise, long signals left to the STFT
//! path (no duplicates), the rate cap and transitions.

mod common;

use common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_detect::{
    BURST_DETECTOR, BurstConfig, BurstDetector, DetectionRecord, Detector, DetectorConfig,
};
use hk_model::{SampleTime, SurveyId, Timestamp};
use num_complex::Complex;

const FS: f64 = 2.4e6;
const NOISE_DBFS: f64 = -30.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Adsb,
    Ook,
    Long,
}

#[derive(Clone, Copy, Debug)]
struct Truth {
    kind: Kind,
    start: u64,
    len: u64,
    f_hz: f64,
}

struct Gen(Rng);

impl Gen {
    fn u(&mut self) -> f64 {
        ((self.0.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> (f64, f64) {
        let (a, b) = (self.u(), self.u());
        let r = (-2.0 * a.ln()).sqrt();
        let t = std::f64::consts::TAU * b;
        (r * t.cos(), r * t.sin())
    }
}

/// Noise power per sample in ci8 units² (full scale 1 = 128).
fn noise_p() -> f64 {
    10f64.powf(NOISE_DBFS / 10.0) * 128.0 * 128.0
}

fn noise(g: &mut Gen, n: usize) -> Vec<Complex<f64>> {
    let s = (noise_p() / 2.0).sqrt();
    (0..n)
        .map(|_| {
            let (a, b) = g.gauss();
            Complex::new(a * s, b * s)
        })
        .collect()
}

/// DF17-shaped PPM envelope: preamble pulses at 0, 1, 3.5, 4.5 µs; 112 random bits of 1 µs.
fn adsb_env(g: &mut Gen) -> Vec<f64> {
    let bits: Vec<bool> = (0..112).map(|_| g.u() < 0.5).collect();
    let n = (120e-6 * FS).round() as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / FS * 1e6;
            let pre = [0.0, 1.0, 3.5, 4.5].iter().any(|&p| t >= p && t < p + 0.5);
            let data = t >= 8.0 && {
                let b = (t - 8.0).floor() as usize;
                let first_half = (t - 8.0).fract() < 0.5;
                b < 112 && bits[b] == first_half
            };
            f64::from(u8::from(pre || data))
        })
        .collect()
}

/// OOK: 10 symbols of 200 µs, first and last on, never two off in a row.
fn ook_env(g: &mut Gen) -> Vec<f64> {
    let sym = (200e-6 * FS).round() as usize;
    let mut on = [true; 10];
    for i in 1..9 {
        on[i] = !on[i - 1] || g.u() < 0.5;
    }
    on.iter()
        .flat_map(|&o| std::iter::repeat_n(f64::from(u8::from(o)), sym))
        .collect()
}

fn add(x: &mut [Complex<f64>], start: usize, env: &[f64], snr_db: f64, f_hz: f64, g: &mut Gen) {
    let amp = (10f64.powf(snr_db / 10.0) * noise_p()).sqrt();
    let phi = std::f64::consts::TAU * g.u();
    for (i, e) in env.iter().enumerate() {
        let w = std::f64::consts::TAU * f_hz * i as f64 / FS + phi;
        x[start + i] += Complex::from_polar(amp * e, w);
    }
}

fn quantise(x: &[Complex<f64>]) -> Vec<Complex<i8>> {
    x.iter()
        .map(|s| {
            Complex::new(
                s.re.round().clamp(-128.0, 127.0) as i8,
                s.im.round().clamp(-128.0, 127.0) as i8,
            )
        })
        .collect()
}

/// `n_bursts` alternating squitters and OOK bursts from 50 ms, one every `spacing_s` (jittered).
fn scene(seed: u64, dur_s: f64, spacing_s: f64, snr_db: f64) -> (Vec<Complex<i8>>, Vec<Truth>) {
    let mut g = Gen(Rng(seed));
    let n = (dur_s * FS) as usize;
    let mut x = noise(&mut g, n);
    let mut truth = Vec::new();
    let mut t = 0.05;
    let mut k = 0;
    while t + spacing_s < dur_s {
        let start = ((t + g.u() * 0.3 * spacing_s) * FS) as usize;
        let (kind, env, f) = if k % 2 == 0 {
            (Kind::Adsb, adsb_env(&mut g), (g.u() - 0.5) * 100e3)
        } else {
            (Kind::Ook, ook_env(&mut g), (g.u() - 0.5) * 1.6e6)
        };
        add(&mut x, start, &env, snr_db, f, &mut g);
        truth.push(Truth {
            kind,
            start: start as u64,
            len: env.len() as u64,
            f_hz: f,
        });
        t += spacing_s;
        k += 1;
    }
    (quantise(&x), truth)
}

fn prov() -> ProvenanceHandle {
    provenance(1090e6, FS, 24.0)
}

fn run(det: &mut BurstDetector, iq: &[Complex<i8>], p: &ProvenanceHandle) -> Vec<DetectionRecord> {
    let mut out = Vec::new();
    for (i, c) in iq.chunks(65_536).enumerate() {
        let s = (i * 65_536) as u64;
        let time = SampleTime {
            sample_index: s,
            host_time: Timestamp::from_unix_nanos((s as f64 * 1e9 / FS).round() as i64),
        };
        let disc = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        det.push(time, disc, p, c, &mut |r| out.push(r));
    }
    det.finish(&mut |r| out.push(r));
    out
}

fn us(samples: f64) -> f64 {
    samples / FS * 1e6
}

#[test]
fn squitters_and_ook_bursts_detected_with_us_timing_by_snr() {
    let p = prov();
    for (seed, snr_db) in [(11u64, 3.0), (12, 6.0), (13, 10.0), (14, 20.0)] {
        let (iq, truth) = scene(seed, 1.0, 0.02, snr_db);
        let mut det = BurstDetector::new(SurveyId::new(), BurstConfig::default());
        let got = run(&mut det, &iq, &p);
        for d in &got {
            assert!(d.detection.detector_version.starts_with(BURST_DETECTOR));
            assert!(d.frames.is_empty() && d.candidate == hk_detect::Candidate::Unconfirmed);
        }
        let mut used = vec![false; got.len()];
        for kind in [Kind::Adsb, Kind::Ook] {
            let (mut found, mut total) = (0, 0);
            let (mut max_ds, mut sum_ds, mut max_de, mut max_df) = (0.0f64, 0.0, 0.0f64, 0.0f64);
            for t in truth.iter().filter(|t| t.kind == kind) {
                total += 1;
                let m = got
                    .iter()
                    .enumerate()
                    .find(|(_, d)| d.samples.start < t.start + t.len && t.start < d.samples.end);
                let Some((j, d)) = m else { continue };
                used[j] = true;
                found += 1;
                let ds = us((d.samples.start as f64 - t.start as f64).abs());
                let de = us((d.samples.end as f64 - (t.start + t.len) as f64).abs());
                let df = (d.detection.f_center_hz - 1090e6 - t.f_hz).abs();
                // µs timing on the stored row too.
                let row_ds = (d.detection.time.start.as_unix_nanos() as f64
                    - t.start as f64 * 1e9 / FS)
                    .abs()
                    / 1e3;
                assert!((row_ds - ds).abs() < 1.0, "row time {row_ds} vs {ds} µs");
                max_ds = max_ds.max(ds);
                sum_ds += ds;
                max_de = max_de.max(de);
                max_df = max_df.max(df);
                if snr_db >= 10.0 {
                    assert!(ds <= 30.0, "{kind:?} {snr_db} dB start error {ds} µs");
                    let f_tol = if kind == Kind::Ook { 10e3 } else { 50e3 };
                    assert!(df <= f_tol, "{kind:?} {snr_db} dB centre error {df} Hz");
                    assert!(!d.detection.flags.marginal || snr_db < 15.0);
                }
            }
            eprintln!(
                "T-075 {kind:?} SNR {snr_db:>4} dB: detected {found}/{total}; start error mean \
                 {:.1} µs max {max_ds:.1} µs; end error max {max_de:.1} µs; centre error max \
                 {:.1} kHz",
                sum_ds / found.max(1) as f64,
                max_df / 1e3
            );
            if snr_db >= 6.0 {
                assert_eq!(found, total, "{kind:?} at {snr_db} dB");
            }
        }
        for (d, _) in got.iter().zip(&used).filter(|(_, u)| !**u) {
            let near = truth
                .iter()
                .min_by_key(|t| t.start.abs_diff(d.samples.start))
                .unwrap();
            eprintln!(
                "T-075 unmatched {:?} snr {:.1} dB f {:.0} Hz; nearest truth {near:?}",
                d.samples,
                d.detection.snr_peak_db,
                d.detection.f_center_hz - 1090e6
            );
        }
        let extra = used.iter().filter(|u| !**u).count();
        eprintln!(
            "T-075 SNR {snr_db} dB: {} detections, {extra} outside truth; {:?}",
            got.len(),
            det.stats()
        );
        // Noise alarms at the configured rate (≤ 0.01/s) are allowed, not structure: a burst is
        // never split or doubled (each truth above matched one row, the rest are counted here).
        assert!(extra <= 1, "{extra} false detections at {snr_db} dB");
    }
}

#[test]
fn false_alarm_rate_on_noise() {
    let p = prov();
    let dur_s = 6.0;
    let mut g = Gen(Rng(99));
    let iq = quantise(&noise(&mut g, (dur_s * FS) as usize));
    let cfg = BurstConfig::default();
    let target = cfg.false_alarm_rate_hz;
    let mut det = BurstDetector::new(SurveyId::new(), cfg);
    let got = run(&mut det, &iq, &p);
    let rate = got.len() as f64 / dur_s;
    eprintln!(
        "T-075 false alarms on noise ({NOISE_DBFS} dBFS, {FS} sps, {dur_s} s): {} = {rate:.3}/s \
         (target {target}/s); {:?}",
        got.len(),
        det.stats()
    );
    assert!(rate <= 1.0, "false-alarm rate {rate}/s");
}

#[test]
fn long_signals_are_left_to_the_stft_path() {
    let p = prov();
    let mut g = Gen(Rng(5));
    let n = (1.2 * FS) as usize;
    let mut x = noise(&mut g, n);
    let mut truth = Vec::new();
    // 8 ms and 30 ms tone bursts, then a carrier that switches on for good.
    for (t_s, len_s, f) in [
        (0.10, 0.008, 300e3),
        (0.30, 0.030, -400e3),
        (0.70, 0.5, 600e3),
    ] {
        let start = (t_s * FS) as usize;
        let len = ((len_s * FS) as usize).min(n - start);
        add(&mut x, start, &vec![1.0; len], 20.0, f, &mut g);
        truth.push(Truth {
            kind: Kind::Long,
            start: start as u64,
            len: len as u64,
            f_hz: f,
        });
    }
    // A few squitters between them still come out.
    for t_s in [0.2, 0.5, 0.6] {
        let start = (t_s * FS) as usize;
        let env = adsb_env(&mut g);
        add(&mut x, start, &env, 20.0, 0.0, &mut g);
        truth.push(Truth {
            kind: Kind::Adsb,
            start: start as u64,
            len: env.len() as u64,
            f_hz: 0.0,
        });
    }
    let iq = quantise(&x);
    let mut burst = BurstDetector::new(SurveyId::new(), BurstConfig::default());
    let bursts = run(&mut burst, &iq, &p);
    let chain = ChainConfig::new(512, 10);
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
    let (stft, _) = replay_ci8(&iq, FS, &[(0, p.clone())], &chain, &mut det);
    eprintln!(
        "T-075 long signals: {} burst detections {:?}, {} STFT detections; {:?}",
        bursts.len(),
        bursts.iter().map(|d| d.samples.clone()).collect::<Vec<_>>(),
        stft.detections.len(),
        burst.stats()
    );
    for t in truth.iter().filter(|t| t.kind == Kind::Long) {
        let (a, b) = (t.start, t.start + t.len);
        assert!(
            !bursts
                .iter()
                .any(|d| d.samples.start < b && a < d.samples.end),
            "burst path detected the long signal at {a}"
        );
        assert!(
            stft.detections
                .iter()
                .any(|d| d.samples.start < b && a < d.samples.end),
            "STFT path misses the long signal at {a}"
        );
    }
    for d in &bursts {
        assert!(
            !stft
                .detections
                .iter()
                .any(|s| s.samples.start < d.samples.end
                    && d.samples.start < s.samples.end
                    && s.f_lo_hz <= d.f_hi_hz
                    && d.f_lo_hz <= s.f_hi_hz),
            "burst {:?} duplicates an STFT detection",
            d.samples
        );
    }
    let squitters = truth.iter().filter(|t| t.kind == Kind::Adsb).count();
    assert!(burst.stats().dropped_long >= 2);
    assert_eq!(bursts.len(), squitters, "every squitter, nothing else");
}

#[test]
fn rate_cap_and_transitions() {
    let p = prov();
    // 1000 squitters per second at 20 dB against a cap of 100/s with a depth of 10.
    let (iq, truth) = scene(21, 1.0, 0.001, 20.0);
    let cfg = BurstConfig {
        max_rate_hz: 100.0,
        rate_burst: 10.0,
        ..BurstConfig::default()
    };
    let mut det = BurstDetector::new(SurveyId::new(), cfg);
    let got = run(&mut det, &iq, &p);
    let st = det.stats();
    eprintln!(
        "T-075 rate cap: {} truth, {} emitted, {st:?}",
        truth.len(),
        got.len()
    );
    assert!(got.len() <= 10 + 100 + 1, "{} rows", got.len());
    assert!(st.dropped_rate > 0 && st.emitted as usize == got.len());

    // A retune restarts the detector: the open burst is dropped and the noise estimate relearnt.
    let (iq, _) = scene(22, 0.2, 0.02, 20.0);
    let mut det = BurstDetector::new(SurveyId::new(), BurstConfig::default());
    let mut out = Vec::new();
    let cut = 100_000usize;
    let t = |s: u64| SampleTime {
        sample_index: s,
        host_time: Timestamp::from_unix_nanos((s as f64 * 1e9 / FS) as i64),
    };
    det.push(
        t(0),
        Discontinuity::STREAM_START,
        &p,
        &iq[..cut],
        &mut |r| out.push(r),
    );
    assert!(det.noise_power().is_some());
    det.push(
        t(cut as u64),
        Discontinuity::RETUNE,
        &p,
        &iq[cut..cut + 10],
        &mut |r| out.push(r),
    );
    assert!(det.noise_power().is_none());
    assert_eq!(det.stats().resets, 2);
}
