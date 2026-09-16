//! Deterministic synthetic modulations for fitting and evaluating the classifier (ADR-0016 §7).
//!
//! **Tooling only, never the real-time path.** Every signal is a pure function of
//! `(class, SynthConfig)`, so a seed names a waveform exactly.
//!
//! - [`Class::TAXONOMY`] covers every `hk-mod@1` class: the **dev** grid fits the densities
//!   ([`crate::density`]) and the **acceptance** grid measures accuracy. The two use disjoint seed
//!   ranges ([`DEV_SEEDS`], [`ACCEPTANCE_SEED_BASE`]) so nothing is ever evaluated on what it was
//!   fitted on.
//! - [`Class::HELD_OUT`] are generators deliberately **outside** the taxonomy (3-level ASK, 8-FSK,
//!   a chirped-carrier FSK, Costas hopping, an OFDM with a non-standard cyclic prefix, noise
//!   bursts). They must come back `unknown`, and they are never fitted.
//!
//! SNR is the **in-band** SNR C13 reports: the noise density is scaled so that
//! `signal power / (N₀ · OBW)` is the requested value, which is what the per-family gates in
//! [`crate::thresholds`] are stated in.
//!
//! When T-213's `py/hkpy/synth/amc` grid lands it replaces this module as the fitting and
//! evaluation source (richer HackRF impairments: LO ppm, phase noise, IQ imbalance, blockers);
//! the impairments modelled here are the subset the classifier is most sensitive to.

use std::f64::consts::TAU;

use hk_dsp::synth::Rng;
use num_complex::{Complex32, Complex64};

/// Dev seeds: used to fit densities and set thresholds, never to report accuracy.
pub const DEV_SEEDS: std::ops::Range<u64> = 0..600;

/// Acceptance seeds start here (ADR-0016 §7: disjoint from the dev range).
pub const ACCEPTANCE_SEED_BASE: u64 = 1_000_000;

/// One generated waveform class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Amplitude modulation with carrier.
    Am,
    /// Narrowband FM (5 kHz deviation).
    Nbfm,
    /// Wideband broadcast FM (75 kHz deviation).
    Wfm,
    /// Single sideband, suppressed carrier.
    Ssb,
    /// Keyed continuous wave.
    Cw,
    /// On-off keying.
    Ook,
    /// 4-level amplitude-shift keying.
    Ask4,
    /// Continuous-phase 2-FSK.
    Fsk2,
    /// Gaussian-filtered FSK.
    Gfsk,
    /// Minimum-shift keying (h = 0.5).
    Msk,
    /// 4-level FSK.
    Fsk4,
    /// Root-raised-cosine BPSK.
    Bpsk,
    /// Root-raised-cosine QPSK.
    Qpsk,
    /// Root-raised-cosine 8-PSK.
    Psk8,
    /// Root-raised-cosine 16-QAM.
    Qam16,
    /// Root-raised-cosine 64-QAM.
    Qam64,
    /// OFDM with a standard cyclic prefix.
    Ofdm,
    /// Repeating linear chirp (CSS).
    Chirp,
    /// Pulse-position-modulated pulse train.
    Ppm,
    /// Regular radar-like pulse train.
    Pulse,
    /// Band-limited Gaussian noise.
    NoiseLike,
    /// **Held out:** 3-level ASK.
    Ask3,
    /// **Held out:** 8-level FSK.
    Fsk8,
    /// **Held out:** 2-FSK on a linearly drifting carrier.
    ChirpedFsk,
    /// **Held out:** Costas-hopped tones.
    CostasHop,
    /// **Held out:** OFDM with a non-standard (very short) cyclic prefix.
    OfdmOddCp,
    /// **Held out:** random-phase noise bursts.
    NoiseBurst,
}

impl Class {
    /// Every class of `hk-mod@1`, in taxonomy order.
    pub const TAXONOMY: &'static [Class] = &[
        Class::Am,
        Class::Nbfm,
        Class::Wfm,
        Class::Ssb,
        Class::Cw,
        Class::Ook,
        Class::Ask4,
        Class::Fsk2,
        Class::Gfsk,
        Class::Msk,
        Class::Fsk4,
        Class::Bpsk,
        Class::Qpsk,
        Class::Psk8,
        Class::Qam16,
        Class::Qam64,
        Class::Ofdm,
        Class::Chirp,
        Class::Ppm,
        Class::Pulse,
        Class::NoiseLike,
    ];

    /// Generators deliberately outside the taxonomy: the open-set negatives.
    pub const HELD_OUT: &'static [Class] = &[
        Class::Ask3,
        Class::Fsk8,
        Class::ChirpedFsk,
        Class::CostasHop,
        Class::OfdmOddCp,
        Class::NoiseBurst,
    ];

    /// The `hk-mod@1` class label, or the generator's name for a held-out class.
    pub const fn label(self) -> &'static str {
        match self {
            Class::Am => "am",
            Class::Nbfm => "nbfm",
            Class::Wfm => "wfm",
            Class::Ssb => "ssb",
            Class::Cw => "cw",
            Class::Ook => "ook",
            Class::Ask4 => "ask4",
            Class::Fsk2 => "2fsk",
            Class::Gfsk => "gfsk",
            Class::Msk => "msk",
            Class::Fsk4 => "4fsk",
            Class::Bpsk => "bpsk",
            Class::Qpsk => "qpsk",
            Class::Psk8 => "8psk",
            Class::Qam16 => "qam16",
            Class::Qam64 => "qam64",
            Class::Ofdm => "ofdm",
            Class::Chirp => "chirp",
            Class::Ppm => "ppm",
            Class::Pulse => "pulse",
            Class::NoiseLike => "noise-like",
            Class::Ask3 => "held-out:ask3",
            Class::Fsk8 => "held-out:8fsk",
            Class::ChirpedFsk => "held-out:chirped-fsk",
            Class::CostasHop => "held-out:costas-hop",
            Class::OfdmOddCp => "held-out:ofdm-odd-cp",
            Class::NoiseBurst => "held-out:noise-burst",
        }
    }

    /// For a **held-out** generator, the `hk-mod@1` family it genuinely belongs to, if any.
    ///
    /// The held-out set mixes two things. Some generators are outside the taxonomy altogether — a
    /// Costas-hopped tone set, a 2-FSK carrier that also sweeps, band noise in bursts — and the
    /// only right answer for them is `unknown`. Others are ordinary members of a family that the
    /// **dev grid** happens not to contain: an OFDM with an unusual cyclic-prefix length is still
    /// OFDM, and 3-level ASK is still ASK. Recognising those is generalisation, not a false known,
    /// so the evaluation separates "abstained" from "named the wrong family".
    pub fn nearest_family(self) -> Option<&'static str> {
        match self {
            Class::Ask3 => Some("ook-ask"),
            Class::Fsk8 => Some("fsk"),
            Class::OfdmOddCp => Some("ofdm"),
            // A chirped-carrier FSK, Costas hopping and noise bursts belong to no family here.
            _ => None,
        }
    }

    /// The `hk-mod@1` family this class belongs to; `None` for a held-out generator (whose right
    /// answer is `unknown`).
    pub fn family(self) -> Option<&'static str> {
        match self {
            Class::Am | Class::Nbfm | Class::Wfm | Class::Ssb | Class::Cw => Some("analog"),
            Class::Ook | Class::Ask4 => Some("ook-ask"),
            Class::Fsk2 | Class::Gfsk | Class::Msk | Class::Fsk4 => Some("fsk"),
            Class::Bpsk | Class::Qpsk | Class::Psk8 | Class::Qam16 | Class::Qam64 => {
                Some("psk-qam")
            }
            Class::Ofdm => Some("ofdm"),
            Class::Chirp => Some("css"),
            Class::Ppm | Class::Pulse => Some("pulsed"),
            Class::NoiseLike => Some("noise-like"),
            _ => None,
        }
    }
}

/// Generation settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SynthConfig {
    /// In-band SNR, dB (signal power over the noise power inside the occupied bandwidth).
    pub snr_db: f64,
    /// Seed: the waveform is a pure function of it.
    pub seed: u64,
    /// Samples generated.
    pub samples: usize,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Quantise to 8 bits, as the HackRF front end does.
    pub quantise_8bit: bool,
    /// Residual carrier offset, Hz. The classifier consumes **CFO-corrected** snippets
    /// (ADR-0016 §4.1: C13 recentres them), so this models what C13 leaves behind, not a raw LO
    /// error. It must stay well under one cycle across the snippet: a larger offset spins the
    /// phase through 2π and averages the fourth-order cumulants to zero, which would be a
    /// property of the harness, not of the modulation.
    pub lo_offset_hz: f64,
    /// Amplitude imbalance between I and Q, fraction.
    pub iq_imbalance: f64,
}

impl SynthConfig {
    /// 16 384 samples at 1 Msps with 8-bit quantisation and a small residual CFO.
    pub fn new(snr_db: f64, seed: u64) -> Self {
        Self {
            snr_db,
            seed,
            samples: 16_384,
            sample_rate_hz: 1e6,
            quantise_8bit: true,
            // ≈ 0.16 cycles over 16 384 samples at 1 Msps.
            lo_offset_hz: 10.0,
            iq_imbalance: 0.01,
        }
    }
}

/// A generated waveform.
#[derive(Clone, Debug)]
pub struct SynthSignal {
    /// Samples, unit mean power.
    pub samples: Vec<Complex32>,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Occupied bandwidth **measured** from the generated samples (never the design value): what
    /// C13 would hand the classifier.
    pub obw_hz: f64,
    /// What was generated (truth: for assertions and fitting only).
    pub class: Class,
    /// In-band SNR it was generated at, dB.
    pub snr_db: f64,
}

/// Generates one waveform.
pub fn generate(class: Class, cfg: &SynthConfig) -> SynthSignal {
    let mut rng = Rng::new(cfg.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ class as u64);
    let n = cfg.samples;
    let fs = cfg.sample_rate_hz;
    // Symbol rate varies with the seed so the densities never learn one rate.
    let rate = 25e3 + 75e3 * rng.unit();
    let (mut x, design_bw) = waveform(class, &mut rng, n, fs, rate);
    normalise(&mut x);

    // Noise at the requested in-band SNR: N₀ = σ²/fs, so σ² = P_s·fs/(BW·10^(SNR/10)).
    let bw = design_bw.clamp(1e3, 0.9 * fs);
    let sigma2 = fs / (bw * 10f64.powf(cfg.snr_db / 10.0));
    let sigma = (sigma2 / 2.0).sqrt();
    for s in x.iter_mut() {
        let (a, b) = rng.gaussian_pair();
        *s += Complex64::new(a * sigma, b * sigma);
    }
    // Front-end impairments: residual CFO, IQ amplitude imbalance, 8-bit quantisation.
    if cfg.lo_offset_hz != 0.0 {
        for (i, s) in x.iter_mut().enumerate() {
            let ph = TAU * cfg.lo_offset_hz * i as f64 / fs;
            *s *= Complex64::new(ph.cos(), ph.sin());
        }
    }
    if cfg.iq_imbalance != 0.0 {
        for s in x.iter_mut() {
            *s = Complex64::new(s.re * (1.0 + cfg.iq_imbalance), s.im);
        }
    }
    let mut samples: Vec<Complex32> = x
        .iter()
        .map(|s| Complex32::new(s.re as f32, s.im as f32))
        .collect();
    if cfg.quantise_8bit {
        let peak = samples
            .iter()
            .map(|s| s.re.abs().max(s.im.abs()))
            .fold(0.0_f32, f32::max)
            .max(1e-9);
        // Fill about half of full scale, as a sensibly-gained HackRF capture does.
        let g = 63.0 / peak;
        for s in samples.iter_mut() {
            *s = Complex32::new(
                ((s.re * g).round().clamp(-127.0, 127.0)) / 127.0,
                ((s.im * g).round().clamp(-127.0, 127.0)) / 127.0,
            );
        }
    }
    // Channel-filter to the occupied band, as C13 does before handing a snippet on
    // (`hk_estimate::normalise`: a flat channel of 1.5 × OBW99). Without this the classifier would
    // see the noise of the whole analysis band rather than of the channel — the instantaneous
    // frequency of a 95 kHz FSK burst in a 1 MHz snippet is ~10 dB noisier than in production, and
    // its two tones stop being separable for reasons that belong to the harness, not the signal.
    // C13 recentres a snippet on the emission before filtering it, so the harness must too:
    // otherwise a lowpass at ±0.75·OBW cuts away an emission that sits off centre (a CW tone at
    // 800 Hz, or SSB's 0.3–3 kHz sideband, would be filtered out entirely and the classifier would
    // be handed noise).
    let peak_offset = peak_offset_hz(&samples, fs);
    if peak_offset != 0.0 {
        for (i, s) in samples.iter_mut().enumerate() {
            let ph = -TAU * peak_offset * i as f64 / fs;
            *s *= Complex32::new(ph.cos() as f32, ph.sin() as f32);
        }
    }
    let obw_hz = measured_obw(&samples, fs);
    let cutoff = (0.75 * obw_hz / fs).clamp(0.005, 0.49);
    if cutoff < 0.45 {
        samples = channel_filter(&samples, cutoff);
    }
    let obw_hz = measured_obw(&samples, fs);
    SynthSignal {
        samples,
        sample_rate_hz: fs,
        obw_hz,
        class,
        snr_db: cfg.snr_db,
    }
}

/// The waveform and its design bandwidth (used only to scale the noise to the requested SNR).
fn waveform(class: Class, rng: &mut Rng, n: usize, fs: f64, rate: f64) -> (Vec<Complex64>, f64) {
    match class {
        Class::Am => (am(rng, n, fs, 0.7), 6e3),
        Class::Nbfm => (fm(rng, n, fs, 5e3, 3e3), 16e3),
        Class::Wfm => (fm(rng, n, fs, 75e3, 53e3), 200e3),
        Class::Ssb => (ssb(rng, n, fs), 3e3),
        Class::Cw => (cw(rng, n, fs), 2e3),
        Class::Ook => (ask(rng, n, fs, rate, &[0.0, 1.0]), 2.0 * rate),
        Class::Ask4 => (ask(rng, n, fs, rate, &[0.25, 0.5, 0.75, 1.0]), 2.0 * rate),
        Class::Ask3 => (ask(rng, n, fs, rate, &[0.0, 0.5, 1.0]), 2.0 * rate),
        Class::Fsk2 => (cpfsk(rng, n, fs, rate, 2, rate * 0.5, 0.0), 2.0 * rate),
        Class::Gfsk => (gfsk(rng, n, fs, rate, rate * 0.35), 2.0 * rate),
        Class::Msk => (cpfsk(rng, n, fs, rate, 2, rate * 0.25, 0.0), 1.5 * rate),
        Class::Fsk4 => (cpfsk(rng, n, fs, rate, 4, rate * 0.5, 0.0), 3.0 * rate),
        Class::Fsk8 => (cpfsk(rng, n, fs, rate, 8, rate * 0.5, 0.0), 5.0 * rate),
        Class::ChirpedFsk => (
            cpfsk(rng, n, fs, rate, 2, rate * 0.5, 4.0 * rate / n as f64),
            3.0 * rate,
        ),
        Class::Bpsk => (linear(rng, n, fs, rate, 2), 1.35 * rate),
        Class::Qpsk => (linear(rng, n, fs, rate, 4), 1.35 * rate),
        Class::Psk8 => (linear(rng, n, fs, rate, 8), 1.35 * rate),
        Class::Qam16 => (linear(rng, n, fs, rate, 16), 1.35 * rate),
        Class::Qam64 => (linear(rng, n, fs, rate, 64), 1.35 * rate),
        Class::Ofdm => (ofdm(rng, n, 128, 32, 100), 0.8 * fs),
        Class::OfdmOddCp => (ofdm(rng, n, 96, 3, 70), 0.72 * fs),
        Class::Chirp => (chirp(n, fs, 125e3, 1024), 125e3),
        Class::Ppm => (ppm(rng, n), 0.5 * fs),
        Class::Pulse => (pulses(n, 10, 200), 0.5 * fs),
        Class::CostasHop => (costas(rng, n, fs, 8, 256, 40e3), 320e3),
        Class::NoiseLike => (band_noise(rng, n, 0.25), 0.5 * fs),
        Class::NoiseBurst => (noise_bursts(rng, n, 0.25), 0.5 * fs),
    }
}

fn normalise(x: &mut [Complex64]) {
    let p = x.iter().map(|s| s.norm_sqr()).sum::<f64>() / x.len().max(1) as f64;
    if p > 0.0 {
        let g = 1.0 / p.sqrt();
        for s in x.iter_mut() {
            *s *= g;
        }
    }
}

/// Band-limited "audio": a handful of tones with random amplitudes and phases.
fn audio(rng: &mut Rng, n: usize, fs: f64, max_hz: f64) -> Vec<f64> {
    let tones: Vec<(f64, f64, f64)> = (0..5)
        .map(|_| {
            (
                0.2 * max_hz + 0.8 * max_hz * rng.unit(),
                rng.unit(),
                TAU * rng.unit(),
            )
        })
        .collect();
    let norm: f64 = tones.iter().map(|(_, a, _)| a).sum::<f64>().max(1e-9);
    (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            tones
                .iter()
                .map(|(f, a, p)| a * (TAU * f * t + p).cos())
                .sum::<f64>()
                / norm
        })
        .collect()
}

fn am(rng: &mut Rng, n: usize, fs: f64, depth: f64) -> Vec<Complex64> {
    let a = audio(rng, n, fs, 3e3);
    a.iter()
        .map(|m| Complex64::new(1.0 + depth * m, 0.0))
        .collect()
}

fn fm(rng: &mut Rng, n: usize, fs: f64, deviation_hz: f64, audio_hz: f64) -> Vec<Complex64> {
    let a = audio(rng, n, fs, audio_hz);
    let mut phase = 0.0;
    a.iter()
        .map(|m| {
            phase = (phase + TAU * deviation_hz * m / fs) % TAU;
            Complex64::new(phase.cos(), phase.sin())
        })
        .collect()
}

fn ssb(rng: &mut Rng, n: usize, fs: f64) -> Vec<Complex64> {
    // Upper sideband, suppressed carrier: only positive audio frequencies are present.
    let tones: Vec<(f64, f64, f64)> = (0..6)
        .map(|_| (300.0 + 2700.0 * rng.unit(), rng.unit(), TAU * rng.unit()))
        .collect();
    (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            tones
                .iter()
                .map(|(f, a, p)| {
                    let ph = TAU * f * t + p;
                    Complex64::new(a * ph.cos(), a * ph.sin())
                })
                .sum::<Complex64>()
        })
        .collect()
}

fn cw(rng: &mut Rng, n: usize, fs: f64) -> Vec<Complex64> {
    let dit = (0.06 * fs) as usize; // ~20 WPM
    let mut on = Vec::with_capacity(n);
    while on.len() < n {
        let symbol = if rng.next_u64() & 1 == 0 {
            dit
        } else {
            3 * dit
        };
        let keyed = rng.next_u64() % 4 != 0;
        for _ in 0..symbol {
            on.push(keyed);
        }
    }
    (0..n)
        .map(|i| {
            let ph = TAU * 800.0 * i as f64 / fs;
            let a = if on[i] { 1.0 } else { 0.0 };
            Complex64::new(a * ph.cos(), a * ph.sin())
        })
        .collect()
}

fn ask(rng: &mut Rng, n: usize, fs: f64, rate: f64, levels: &[f64]) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let symbols: Vec<f64> = (0..(n as f64 / sps).ceil() as usize + 1)
        .map(|_| levels[(rng.next_u64() as usize) % levels.len()])
        .collect();
    // A raised-cosine-ish symbol transition (half a cosine over 20 % of a symbol) keeps the
    // spectrum finite without hiding the amplitude levels.
    let edge = (0.2 * sps).max(1.0);
    (0..n)
        .map(|i| {
            let pos = i as f64 / sps;
            let k = pos.floor() as usize;
            let frac = (pos - k as f64) * sps;
            let a = if frac < edge && k > 0 {
                let w = 0.5 - 0.5 * (std::f64::consts::PI * frac / edge).cos();
                symbols[k - 1] + (symbols[k] - symbols[k - 1]) * w
            } else {
                symbols[k]
            };
            Complex64::new(a, 0.0)
        })
        .collect()
}

fn cpfsk(
    rng: &mut Rng,
    n: usize,
    fs: f64,
    rate: f64,
    levels: usize,
    deviation_hz: f64,
    drift_hz_per_sample: f64,
) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let syms: Vec<f64> = (0..(n as f64 / sps).ceil() as usize + 1)
        .map(|_| {
            let l = (rng.next_u64() as usize) % levels;
            // Symmetric levels: −1, …, +1.
            2.0 * (l as f64 / (levels - 1).max(1) as f64) - 1.0
        })
        .collect();
    let mut phase = 0.0;
    (0..n)
        .map(|i| {
            let s = syms[(i as f64 / sps) as usize];
            let f = s * deviation_hz + drift_hz_per_sample * i as f64;
            phase = (phase + TAU * f / fs) % TAU;
            Complex64::new(phase.cos(), phase.sin())
        })
        .collect()
}

fn gfsk(rng: &mut Rng, n: usize, fs: f64, rate: f64, deviation_hz: f64) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let syms: Vec<f64> = (0..(n as f64 / sps).ceil() as usize + 1)
        .map(|_| if rng.next_u64() & 1 == 1 { 1.0 } else { -1.0 })
        .collect();
    let nrz: Vec<f64> = (0..n).map(|i| syms[(i as f64 / sps) as usize]).collect();
    // Gaussian pulse shaping, BT ≈ 0.5.
    let span = (sps * 2.0) as usize | 1;
    let sigma = sps * 0.5 / (TAU * 0.5);
    let taps: Vec<f64> = (0..span)
        .map(|i| {
            let t = i as f64 - (span / 2) as f64;
            (-0.5 * (t / sigma).powi(2)).exp()
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    let shaped: Vec<f64> = (0..n)
        .map(|i| {
            taps.iter()
                .enumerate()
                .map(|(k, w)| {
                    let j = i as isize + k as isize - (span / 2) as isize;
                    let v = if j < 0 {
                        nrz[0]
                    } else if j as usize >= n {
                        nrz[n - 1]
                    } else {
                        nrz[j as usize]
                    };
                    v * w
                })
                .sum::<f64>()
                / sum
        })
        .collect();
    let mut phase = 0.0;
    shaped
        .iter()
        .map(|s| {
            phase = (phase + TAU * s * deviation_hz / fs) % TAU;
            Complex64::new(phase.cos(), phase.sin())
        })
        .collect()
}

/// Root-raised-cosine taps, unit energy.
fn rrc(sps: f64, alpha: f64, span: usize) -> Vec<f64> {
    let n = (2.0 * span as f64 * sps) as usize | 1;
    let mid = (n / 2) as f64;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps;
            if t.abs() < 1e-9 {
                1.0 - alpha + 4.0 * alpha / std::f64::consts::PI
            } else if (t.abs() - 1.0 / (4.0 * alpha)).abs() < 1e-6 {
                let p = std::f64::consts::PI / (4.0 * alpha);
                alpha / 2f64.sqrt()
                    * ((1.0 + 2.0 / std::f64::consts::PI) * p.sin()
                        + (1.0 - 2.0 / std::f64::consts::PI) * p.cos())
            } else {
                let pt = std::f64::consts::PI * t;
                ((pt * (1.0 - alpha)).sin() + 4.0 * alpha * t * (pt * (1.0 + alpha)).cos())
                    / (pt * (1.0 - (4.0 * alpha * t).powi(2)))
            }
        })
        .collect();
    let e: f64 = h.iter().map(|v| v * v).sum::<f64>().sqrt();
    h.iter_mut().for_each(|v| *v /= e);
    h
}

/// A constellation of `order` points, unit average power.
fn constellation(order: usize, k: usize) -> Complex64 {
    match order {
        2 => Complex64::new(if k == 0 { -1.0 } else { 1.0 }, 0.0),
        4 | 8 => {
            let ph = TAU * k as f64 / order as f64 + std::f64::consts::FRAC_PI_4;
            Complex64::new(ph.cos(), ph.sin())
        }
        _ => {
            let side = (order as f64).sqrt() as usize;
            let i = (k % side) as f64 - (side as f64 - 1.0) / 2.0;
            let q = (k / side) as f64 - (side as f64 - 1.0) / 2.0;
            let norm = ((side * side - 1) as f64 / 6.0).sqrt();
            Complex64::new(i / norm, q / norm)
        }
    }
}

fn linear(rng: &mut Rng, n: usize, fs: f64, rate: f64, order: usize) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let alpha = 0.25 + 0.2 * rng.unit();
    let taps = rrc(sps, alpha, 6);
    let symbols = (n as f64 / sps).ceil() as usize + taps.len();
    let syms: Vec<Complex64> = (0..symbols)
        .map(|_| constellation(order, (rng.next_u64() as usize) % order))
        .collect();
    // Upsample by inserting symbols at the nearest sample, then filter.
    let mut up = vec![Complex64::new(0.0, 0.0); n + taps.len()];
    for (k, s) in syms.iter().enumerate() {
        let i = (k as f64 * sps).round() as usize;
        if i < up.len() {
            up[i] = *s;
        }
    }
    let d = taps.len() / 2;
    (0..n)
        .map(|i| {
            taps.iter()
                .enumerate()
                .map(|(m, w)| {
                    let j = i + d;
                    if j >= m && j - m < up.len() {
                        up[j - m] * *w
                    } else {
                        Complex64::new(0.0, 0.0)
                    }
                })
                .sum::<Complex64>()
        })
        .collect()
}

fn ofdm(rng: &mut Rng, n: usize, nfft: usize, cp: usize, active: usize) -> Vec<Complex64> {
    let mut out = Vec::with_capacity(n + nfft + cp);
    while out.len() < n {
        // One symbol: QPSK on `active` central subcarriers, IDFT by direct summation (nfft is
        // small and this is tooling, not the real-time path).
        let carriers: Vec<Complex64> = (0..active)
            .map(|_| constellation(4, (rng.next_u64() as usize) % 4))
            .collect();
        let first = (nfft - active) / 2;
        let sym: Vec<Complex64> = (0..nfft)
            .map(|t| {
                carriers
                    .iter()
                    .enumerate()
                    .map(|(c, v)| {
                        let k = (first + c) as f64 - nfft as f64 / 2.0;
                        let ph = TAU * k * t as f64 / nfft as f64;
                        v * Complex64::new(ph.cos(), ph.sin())
                    })
                    .sum::<Complex64>()
                    / (active as f64).sqrt()
            })
            .collect();
        out.extend_from_slice(&sym[nfft - cp..]);
        out.extend_from_slice(&sym);
    }
    out.truncate(n);
    out
}

fn chirp(n: usize, fs: f64, sweep_hz: f64, period: usize) -> Vec<Complex64> {
    let mut phase = 0.0;
    (0..n)
        .map(|i| {
            let k = (i % period) as f64 / period as f64;
            let f = -sweep_hz / 2.0 + sweep_hz * k;
            phase = (phase + TAU * f / fs) % TAU;
            Complex64::new(phase.cos(), phase.sin())
        })
        .collect()
}

fn ppm(rng: &mut Rng, n: usize) -> Vec<Complex64> {
    // Short pulses in one of two positions per bit period, in frames with a preamble gap.
    let pulse = 8usize;
    let slot = 2 * pulse;
    let frame = 112 * slot;
    let mut x = vec![Complex64::new(0.0, 0.0); n];
    let mut i = 0;
    while i + frame < n {
        for bit in 0..112 {
            let at = i + bit * slot + if rng.next_u64() & 1 == 1 { pulse } else { 0 };
            for s in x.iter_mut().skip(at).take(pulse) {
                *s = Complex64::new(1.0, 0.0);
            }
        }
        i += frame + frame / 2;
    }
    x
}

fn pulses(n: usize, width: usize, pri: usize) -> Vec<Complex64> {
    let mut x = vec![Complex64::new(0.0, 0.0); n];
    let mut i = 0;
    while i < n {
        for s in x.iter_mut().skip(i).take(width) {
            *s = Complex64::new(1.0, 0.0);
        }
        i += pri;
    }
    x
}

fn costas(
    rng: &mut Rng,
    n: usize,
    fs: f64,
    hops: usize,
    dwell: usize,
    spacing: f64,
) -> Vec<Complex64> {
    let mut phase = 0.0;
    (0..n)
        .map(|i| {
            if i % dwell == 0 {
                let _ = rng.next_u64();
            }
            let h = ((i / dwell) * 5 + 3) % hops; // a Costas-like permutation
            let f = (h as f64 - hops as f64 / 2.0) * spacing;
            phase = (phase + TAU * f / fs) % TAU;
            Complex64::new(phase.cos(), phase.sin())
        })
        .collect()
}

/// Band-limited Gaussian noise: white noise through a windowed-sinc lowpass at `cutoff` × fs.
fn band_noise(rng: &mut Rng, n: usize, cutoff: f64) -> Vec<Complex64> {
    let white: Vec<Complex64> = (0..n + 64)
        .map(|_| {
            let (a, b) = rng.gaussian_pair();
            Complex64::new(a, b)
        })
        .collect();
    let taps: Vec<f64> = (0..65)
        .map(|i| {
            let t = i as f64 - 32.0;
            let sinc = if t.abs() < 1e-9 {
                1.0
            } else {
                (TAU * cutoff * t).sin() / (std::f64::consts::PI * t)
            };
            let w = 0.54 - 0.46 * (TAU * i as f64 / 64.0).cos();
            sinc * w
        })
        .collect();
    (0..n)
        .map(|i| {
            taps.iter()
                .enumerate()
                .map(|(k, w)| white[i + k] * *w)
                .sum::<Complex64>()
        })
        .collect()
}

fn noise_bursts(rng: &mut Rng, n: usize, cutoff: f64) -> Vec<Complex64> {
    let mut x = band_noise(rng, n, cutoff);
    let burst = 1024;
    for (i, s) in x.iter_mut().enumerate() {
        if (i / burst) % 3 != 0 {
            *s = Complex64::new(0.0, 0.0);
        }
    }
    x
}

/// Windowed-sinc lowpass at `cutoff` × fs (half-bandwidth), then rescaled to unit mean power.
fn channel_filter(x: &[Complex32], cutoff: f64) -> Vec<Complex32> {
    const N: usize = 95;
    let taps: Vec<f64> = (0..N)
        .map(|i| {
            let t = i as f64 - (N / 2) as f64;
            let sinc = if t.abs() < 1e-9 {
                2.0 * cutoff
            } else {
                (TAU * cutoff * t).sin() / (std::f64::consts::PI * t)
            };
            let w = 0.54 - 0.46 * (TAU * i as f64 / (N - 1) as f64).cos();
            sinc * w
        })
        .collect();
    let gain: f64 = taps.iter().sum();
    let half = N / 2;
    let mut out: Vec<Complex32> = (0..x.len())
        .map(|i| {
            let mut acc = Complex64::new(0.0, 0.0);
            for (k, w) in taps.iter().enumerate() {
                let j = i as isize + k as isize - half as isize;
                if j >= 0 && (j as usize) < x.len() {
                    let s = x[j as usize];
                    acc += Complex64::new(f64::from(s.re), f64::from(s.im)) * *w;
                }
            }
            let g = if gain.abs() > 1e-12 { gain } else { 1.0 };
            Complex32::new((acc.re / g) as f32, (acc.im / g) as f32)
        })
        .collect();
    // Drop the filter's edge transients, which are not part of the emission.
    if out.len() > 4 * N {
        out = out[half..out.len() - half].to_vec();
    }
    let p = out
        .iter()
        .map(|s| f64::from(s.norm_sqr()))
        .sum::<f64>()
        .max(1e-30)
        / out.len() as f64;
    let g = (1.0 / p.sqrt()) as f32;
    for s in out.iter_mut() {
        *s *= g;
    }
    out
}

/// Offset of the strongest spectrum bin from the centre, Hz: the harness's stand-in for C13's
/// measured carrier offset.
fn peak_offset_hz(samples: &[Complex32], fs: f64) -> f64 {
    let fft_len = (samples.len() / 8).next_power_of_two().clamp(64, 2048);
    let cfg = hk_dsp::WelchConfig {
        fft_len,
        overlap: fft_len / 2,
        window: hk_dsp::WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let Ok(s) = hk_dsp::welch(samples, fs, 0.0, &cfg) else {
        return 0.0;
    };
    let peak = s
        .psd
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(s.psd.len() / 2, |(i, _)| i);
    s.bin_offset_hz(peak)
}

/// OBW99 measured from the samples: the narrowest central band holding 99 % of the power.
fn measured_obw(samples: &[Complex32], fs: f64) -> f64 {
    let fft_len = (samples.len() / 8).next_power_of_two().clamp(64, 2048);
    let cfg = hk_dsp::WelchConfig {
        fft_len,
        overlap: fft_len / 2,
        window: hk_dsp::WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let Ok(s) = hk_dsp::welch(samples, fs, 0.0, &cfg) else {
        return fs / 2.0;
    };
    // Noise-subtracted, as C13 measures OBW99: integrating the raw PSD would add the whole band's
    // noise to the tails and widen the answer towards the snippet width
    // ([`crate::features::occupied_band`]).
    let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
    let (lo, hi) = crate::features::occupied_band(&psd);
    ((hi - lo + 1) as f64 * s.bin_width_hz()).max(fs / psd.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_and_unit_scaled() {
        let cfg = SynthConfig::new(20.0, 42);
        for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
            let a = generate(*class, &cfg);
            let b = generate(*class, &cfg);
            assert_eq!(
                a.samples,
                b.samples,
                "{} is not deterministic",
                class.label()
            );
            // The channel filter trims its own edge transients, so the length is at most the
            // requested one.
            assert!(
                (cfg.samples - 128..=cfg.samples).contains(&a.samples.len()),
                "{}: {} samples",
                class.label(),
                a.samples.len()
            );
            let p = a
                .samples
                .iter()
                .map(|s| f64::from(s.norm_sqr()))
                .sum::<f64>()
                / a.samples.len() as f64;
            assert!(p > 0.0 && p.is_finite(), "{}: power {p}", class.label());
            assert!(
                a.obw_hz > 0.0 && a.obw_hz <= cfg.sample_rate_hz,
                "{}: obw {}",
                class.label(),
                a.obw_hz
            );
        }
        // A different seed is a different waveform.
        assert_ne!(
            generate(Class::Fsk2, &SynthConfig::new(20.0, 1)).samples,
            generate(Class::Fsk2, &SynthConfig::new(20.0, 2)).samples
        );
    }

    #[test]
    fn every_taxonomy_class_maps_to_its_family_and_held_out_ones_do_not() {
        let tax = &hk_model::classify::HK_MOD_V1;
        for c in Class::TAXONOMY {
            let family = c.family().unwrap_or_else(|| panic!("{}", c.label()));
            assert_eq!(
                tax.family_of(c.label()),
                Some(family),
                "{} is not in hk-mod@1",
                c.label()
            );
        }
        for c in Class::HELD_OUT {
            assert_eq!(c.family(), None, "{} must be held out", c.label());
        }
        // Every hk-mod@1 class is generated, except DSSS: it has no estimator and no generator in
        // M3 and the tree denies it outright (ADR-0016 §1, "may always abstain in M3"), so
        // generating it would only produce a family the classifier must never claim.
        for f in tax.families.iter().filter(|f| f.name != "dsss") {
            for class in f.classes {
                assert!(
                    Class::TAXONOMY.iter().any(|c| c.label() == *class),
                    "no generator for {class}"
                );
            }
        }
        assert!(
            !Class::TAXONOMY.iter().any(|c| c.family() == Some("dsss")),
            "dsss is deliberately not generated"
        );
    }

    #[test]
    fn the_dev_and_acceptance_seed_ranges_are_disjoint() {
        const { assert!(DEV_SEEDS.end < ACCEPTANCE_SEED_BASE) };
    }

    #[test]
    fn bandwidth_grows_with_the_modulation_it_should() {
        let cfg = SynthConfig::new(30.0, 9);
        let nbfm = generate(Class::Nbfm, &cfg).obw_hz;
        let wfm = generate(Class::Wfm, &cfg).obw_hz;
        assert!(wfm > 4.0 * nbfm, "nbfm {nbfm} wfm {wfm}");
    }
}
