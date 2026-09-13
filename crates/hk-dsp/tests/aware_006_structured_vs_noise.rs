//! T-038 (AWARE-006 precision): steady structured wideband emissions against noise jammers, and
//! the frequency extent of partial-band episodes.
//!
//! IQ is synthesised here and replayed through the real chain the detector runs: 8-bit
//! quantisation → STFT (1024-bin Hann, 10 averages, SK) → `NoiseFloorTracker`. 2 Msps at GNSS L1,
//! −40 dBFS noise, the emission added from 0.8 s at `step` dB of in-band PSD over the floor.
//! `FloorAnomalies` (hk-context) opens an AWARE-006 floor-rise Anomaly on `NoiseLike` episodes
//! only, so the class is the contract:
//! - noise jammers (broadband Gaussian, brick-wall partial-band Gaussian, FM-by-noise with
//!   100/250 kHz modulating noise on a 1 MHz band) → `NoiseLike` Rise (Anomaly opened);
//! - steady OFDM (QPSK, 40–256 subcarriers, 640 kHz–1.5 MHz, with and without cyclic prefix)
//!   → every Rise/Extend `Structured` (no floor-rise Anomaly);
//! - partial-band noise jammers at several widths and positions (including block-straddling
//!   edges) → Rise extent within 20 % of the true bandwidth.
//!
//! The confusion matrix and per-run features are printed (`--nocapture`).

mod common;

use std::fmt::Write as _;

use common::*;
use hk_core::Discontinuity;
use hk_dsp::floor::{FloorChangeClass, FloorConfig, FloorEvent, FloorEventKind, NoiseFloorTracker};
use hk_dsp::synth::{Rng, complex_noise, quantize_ci8};
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use num_complex::Complex32;
use rustfft::FftPlanner;

const AWARE_006: &str = "AWARE-006";
const FS: f64 = 2e6;
const CENTER: f64 = 1575.42e6;
const FFT: usize = 1024;
const AVG: usize = 10;
const NOISE_DBFS: f64 = -40.0;
/// Emission onset; the Rise confirms about 1 s later.
const T0: f64 = 0.8;
const TOTAL: f64 = 2.3;
const STEPS_DB: [f64; 3] = [6.0, 10.0, 15.0];

fn undb(x: f64) -> f64 {
    10f64.powf(x / 10.0)
}

#[derive(Clone, Copy, Debug)]
enum Emission {
    /// Gaussian noise, brick-wall band `off ± bw/2`.
    Noise { bw: f64, off: f64 },
    /// Constant-envelope carrier frequency-modulated by Gaussian noise of bandwidth `fm_bw`, rms
    /// deviation `bw/4`.
    NoiseFm { bw: f64, off: f64, fm_bw: f64 },
    /// QPSK OFDM: `used` subcarriers (±used/2 around DC, DC empty) on an `n`-sample symbol with a
    /// `cp`-sample cyclic prefix.
    Ofdm {
        n: usize,
        used: usize,
        cp: usize,
        off: f64,
    },
}

impl Emission {
    /// Nominal occupied band, Hz relative to the centre.
    fn band(self) -> (f64, f64) {
        match self {
            Emission::Noise { bw, off } | Emission::NoiseFm { bw, off, .. } => {
                (off - bw / 2.0, off + bw / 2.0)
            }
            Emission::Ofdm { n, used, off, .. } => {
                let h = (used as f64 / 2.0 + 0.5) * FS / n as f64;
                (off - h, off + h)
            }
        }
    }
}

/// Complex Gaussian noise of total power `power` in the brick-wall band `off ± bw/2`.
fn band_noise(rng: &mut Rng, len: usize, bw: f64, off: f64, power: f64) -> Vec<Complex32> {
    let mut x = complex_noise(rng, len, 1.0);
    let mut planner = FftPlanner::<f32>::new();
    planner.plan_fft_forward(len).process(&mut x);
    let mut kept = 0usize;
    for (i, v) in x.iter_mut().enumerate() {
        let k = if i < len / 2 {
            i as f64
        } else {
            i as f64 - len as f64
        };
        if (k * FS / len as f64 - off).abs() > bw / 2.0 {
            *v = Complex32::new(0.0, 0.0);
        } else {
            kept += 1;
        }
    }
    planner.plan_fft_inverse(len).process(&mut x);
    // Unnormalised forward and inverse transforms: per-sample variance kept·len.
    let g = (power / (kept as f64 * len as f64)).sqrt() as f32;
    x.iter_mut().for_each(|v| *v *= g);
    x
}

fn emission(rng: &mut Rng, e: Emission, len: usize, step_db: f64) -> Vec<Complex32> {
    let (lo, hi) = e.band();
    let power = undb(NOISE_DBFS) / FS * (undb(step_db) - 1.0) * (hi - lo);
    match e {
        Emission::Noise { bw, off } => band_noise(rng, len, bw, off, power),
        Emission::NoiseFm { bw, off, fm_bw } => {
            // The real part of unit-power complex noise has variance 1/2: scale to unit rms.
            let m = band_noise(rng, len, fm_bw, 0.0, 2.0);
            let a = power.sqrt() as f32;
            let mut phase = 0f64;
            m.iter()
                .map(|v| {
                    phase += std::f64::consts::TAU * (off + bw / 4.0 * f64::from(v.re)) / FS;
                    Complex32::from_polar(a, phase as f32)
                })
                .collect()
        }
        Emission::Ofdm { n, used, cp, off } => {
            let inverse = FftPlanner::<f32>::new().plan_fft_inverse(n);
            // Unnormalised inverse: per-sample variance = subcarriers used.
            let g = (power / used as f64).sqrt() as f32;
            let half = used as i64 / 2;
            let mut out = Vec::with_capacity(len + n + cp);
            let mut sym = vec![Complex32::new(0.0, 0.0); n];
            while out.len() < len {
                sym.iter_mut().for_each(|s| *s = Complex32::new(0.0, 0.0));
                for k in (-half..=half).filter(|&k| k != 0) {
                    let b = rng.next_u64();
                    let re = if b & 1 == 0 { 1.0 } else { -1.0 };
                    let im = if b & 2 == 0 { 1.0 } else { -1.0 };
                    sym[k.rem_euclid(n as i64) as usize] =
                        Complex32::new(re, im) * std::f32::consts::FRAC_1_SQRT_2 * g;
                }
                inverse.process(&mut sym);
                out.extend_from_slice(&sym[n - cp..]);
                out.extend_from_slice(&sym);
            }
            out.truncate(len);
            for (i, v) in out.iter_mut().enumerate() {
                *v *= Complex32::from_polar(
                    1.0,
                    (std::f64::consts::TAU * off * i as f64 / FS) as f32,
                );
            }
            out
        }
    }
}

/// Floor events of one replay.
fn replay(e: Emission, step_db: f64, seed: u64) -> Vec<FloorEvent> {
    let n = (TOTAL * FS) as usize;
    let s0 = (T0 * FS) as usize;
    let mut rng = Rng::new(seed);
    let mut x = complex_noise(&mut rng, n, undb(NOISE_DBFS));
    for (a, b) in x[s0..]
        .iter_mut()
        .zip(emission(&mut rng, e, n - s0, step_db))
    {
        *a += b;
    }
    let (iq, _) = quantize_ci8(&x);
    drop(x);
    let prov = provenance(CENTER, FS);
    let welch = WelchConfig {
        fft_len: FFT,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, AVG)).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut events = Vec::new();
    for (k, chunk) in iq.chunks(65_536).enumerate() {
        let first = (k * 65_536) as u64;
        let flags = if k == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        let h = header(first, &prov, flags);
        stft.push(InputInfo::from(&h), chunk, |frame| {
            tracker.update(frame, |ev| events.push(ev.clone()));
        });
    }
    events
}

struct Run {
    name: String,
    emission: Emission,
    step_db: f64,
    events: Vec<FloorEvent>,
}

impl Run {
    fn rise(&self) -> Option<&FloorEvent> {
        self.events.iter().find(|e| e.kind == FloorEventKind::Rise)
    }

    /// The opening class, or `None` without a Rise.
    fn class(&self) -> Option<FloorChangeClass> {
        self.rise().map(|r| r.class)
    }

    fn opening(&self) -> impl Iterator<Item = &FloorEvent> {
        self.events
            .iter()
            .filter(|e| matches!(e.kind, FloorEventKind::Rise | FloorEventKind::Extend))
    }

    fn line(&self) -> String {
        let (lo, hi) = self.emission.band();
        match self.rise() {
            Some(r) => format!(
                "{:>18} {:>4} dB: {:?} sk {:.3} excess {:.2} dB, extent {:.0}..{:.0} kHz (true {:.0}..{:.0}), {} events",
                self.name,
                self.step_db,
                r.class,
                r.sk.unwrap_or(f32::NAN),
                r.excess_std_db,
                (r.f_lo_hz - CENTER) / 1e3,
                (r.f_hi_hz - CENTER) / 1e3,
                lo / 1e3,
                hi / 1e3,
                self.events.len()
            ),
            None => format!("{:>18} {:>4} dB: no Rise", self.name, self.step_db),
        }
    }
}

/// Replays every `(name, emission, step)` in parallel.
fn replay_all(cases: Vec<(String, Emission, f64)>) -> Vec<Run> {
    std::thread::scope(|s| {
        let handles: Vec<_> = cases
            .into_iter()
            .enumerate()
            .map(|(k, (name, emission, step_db))| {
                s.spawn(move || Run {
                    events: replay(emission, step_db, 0x7038 + k as u64),
                    name,
                    emission,
                    step_db,
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

fn grid(scenarios: &[(&str, Emission)]) -> Vec<(String, Emission, f64)> {
    scenarios
        .iter()
        .flat_map(|&(name, e)| STEPS_DB.map(|s| (name.to_owned(), e, s)))
        .collect()
}

const NOISE_JAMMERS: &[(&str, Emission)] = &[
    ("noise broadband", Emission::Noise { bw: FS, off: 0.0 }),
    (
        "noise 800k +200k",
        Emission::Noise {
            bw: 800e3,
            off: 200e3,
        },
    ),
    (
        "noise-FM 1M/100k",
        Emission::NoiseFm {
            bw: 1e6,
            off: 0.0,
            fm_bw: 100e3,
        },
    ),
    (
        "noise-FM 1M/250k",
        Emission::NoiseFm {
            bw: 1e6,
            off: 0.0,
            fm_bw: 250e3,
        },
    ),
];

const OFDM: &[(&str, Emission)] = &[
    (
        "OFDM 64/128 CP32",
        Emission::Ofdm {
            n: 128,
            used: 64,
            cp: 32,
            off: 0.0,
        },
    ),
    (
        "OFDM 64/128 no CP",
        Emission::Ofdm {
            n: 128,
            used: 64,
            cp: 0,
            off: 0.0,
        },
    ),
    (
        "OFDM 32/64 CP16",
        Emission::Ofdm {
            n: 64,
            used: 32,
            cp: 16,
            off: 0.0,
        },
    ),
    (
        "OFDM 256/512 CP64",
        Emission::Ofdm {
            n: 512,
            used: 256,
            cp: 64,
            off: 0.0,
        },
    ),
    (
        "OFDM 96/128 CP32",
        Emission::Ofdm {
            n: 128,
            used: 96,
            cp: 32,
            off: 0.0,
        },
    ),
    (
        "OFDM 40/128 +100k",
        Emission::Ofdm {
            n: 128,
            used: 40,
            cp: 0,
            off: 100e3,
        },
    ),
];

/// Printed for the record, not asserted: slow FM-by-noise (a wandering carrier: SK and excess
/// noise rise with the modulation index) and OFDM with a 32-sample symbol (fluctuation
/// correlation close to the threshold).
const CHARACTERISED: &[(&str, Emission)] = &[
    (
        "noise-FM 1M/20k",
        Emission::NoiseFm {
            bw: 1e6,
            off: 0.0,
            fm_bw: 20e3,
        },
    ),
    (
        "noise-FM 1M/50k",
        Emission::NoiseFm {
            bw: 1e6,
            off: 0.0,
            fm_bw: 50e3,
        },
    ),
    (
        "OFDM 16/32",
        Emission::Ofdm {
            n: 32,
            used: 16,
            cp: 0,
            off: 0.0,
        },
    ),
];

#[test]
fn aware_006_steady_ofdm_is_structured_and_noise_jammers_stay_noise_like() {
    let noise = replay_all(grid(NOISE_JAMMERS));
    let ofdm = replay_all(grid(OFDM));
    let other = replay_all(grid(CHARACTERISED));
    let mut report = String::new();
    let classes = [
        Some(FloorChangeClass::NoiseLike),
        Some(FloorChangeClass::Structured),
        Some(FloorChangeClass::Unverified),
        None,
    ];
    let _ = writeln!(
        report,
        "{AWARE_006} T-038 confusion matrix (rows: truth; columns: NoiseLike / Structured / Unverified / no Rise)"
    );
    for (label, runs) in [("noise jammer", &noise), ("OFDM", &ofdm)] {
        let counts = classes.map(|c| runs.iter().filter(|r| r.class() == c).count());
        let _ = writeln!(report, "  {label:>12}: {counts:?} of {}", runs.len());
    }
    for r in noise.iter().chain(&ofdm).chain(&other) {
        let _ = writeln!(report, "  {}", r.line());
    }
    eprintln!("{report}");

    for r in &noise {
        let rise = r
            .rise()
            .unwrap_or_else(|| panic!("{AWARE_006}: no Rise for {}\n{report}", r.line()));
        assert_eq!(
            rise.class,
            FloorChangeClass::NoiseLike,
            "{AWARE_006}: a noise jammer opens a NoiseLike Rise (the Anomaly): {}",
            r.line()
        );
        let (lo, hi) = r.emission.band();
        let covered = ((rise.f_hi_hz - CENTER).min(hi) - (rise.f_lo_hz - CENTER).max(lo)).max(0.0);
        assert!(
            covered >= 0.8 * (hi - lo),
            "{AWARE_006}: the Rise covers the jammer: {}",
            r.line()
        );
    }
    for r in &ofdm {
        assert!(r.rise().is_some(), "{AWARE_006}: no Rise for {}", r.line());
        for e in r.opening() {
            assert_eq!(
                e.class,
                FloorChangeClass::Structured,
                "{AWARE_006}: steady OFDM is a structured emitter, never a floor rise: {}",
                r.line()
            );
        }
    }
}

#[test]
fn aware_006_partial_band_extent_within_20_percent() {
    let cases: Vec<(String, Emission, f64)> = [
        (600e3, -310e3),
        (700e3, 97e3),
        (800e3, 200e3),
        (800e3, 433e3),
        (800e3, -577e3),
        (1200e3, -150e3),
    ]
    .into_iter()
    .flat_map(|(bw, off)| {
        let steps: &[f64] = if bw == 800e3 && off == 200e3 {
            &STEPS_DB
        } else {
            &[10.0]
        };
        steps.iter().map(move |&s| {
            (
                format!("noise {:.0}k {:+.0}k", bw / 1e3, off / 1e3),
                Emission::Noise { bw, off },
                s,
            )
        })
    })
    .collect();
    let runs = replay_all(cases);
    let mut report = String::new();
    let mut worst = 0f64;
    for r in &runs {
        let _ = writeln!(report, "  {}", r.line());
        let (lo, hi) = r.emission.band();
        let bw = hi - lo;
        let rise = r
            .rise()
            .unwrap_or_else(|| panic!("{AWARE_006}: no Rise for {}", r.line()));
        let err_lo = (rise.f_lo_hz - CENTER - lo).abs() / bw;
        let err_hi = (rise.f_hi_hz - CENTER - hi).abs() / bw;
        let err_w = ((rise.f_hi_hz - rise.f_lo_hz) - bw).abs() / bw;
        worst = worst.max(err_lo).max(err_hi).max(err_w);
        assert!(
            err_lo <= 0.2 && err_hi <= 0.2 && err_w <= 0.2,
            "{AWARE_006}: extent within 20 % of the bandwidth (edges {err_lo:.3}/{err_hi:.3}, width {err_w:.3}): {}",
            r.line()
        );
        // The block hull stays the episode's bins (invariants); the refined edges bracket it.
        let bin_hz = FS / FFT as f64;
        let hull_lo = CENTER - FS / 2.0 + rise.bins.start as f64 * bin_hz;
        let hull_hi = CENTER - FS / 2.0 + rise.bins.end as f64 * bin_hz;
        assert!(
            rise.f_lo_hz <= hull_lo + 32.0 * bin_hz && rise.f_hi_hz >= hull_hi - 32.0 * bin_hz,
            "{AWARE_006}: refined extent keeps the block hull: {}",
            r.line()
        );
    }
    eprintln!(
        "{AWARE_006} T-038 partial-band extent (worst edge/width error {worst:.3} of the bandwidth):\n{report}"
    );
}
