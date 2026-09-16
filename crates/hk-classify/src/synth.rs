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
//!   bursts, DSB-SC and VSB AM, π/4-DQPSK, 16-APSK, a Barker-coded radar pulse). They must come
//!   back `unknown`, and they are never fitted.
//!   **Every generated family has at least one** ([`Class::probes_family`],
//!   [`open_set_families`]): a family with no negative has an *unmeasured* open set, not a good
//!   one, and T-244 found `analog`, `psk-qam` and `pulsed` in exactly that state.
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

/// T-296: which stage of [`generate`] costs the `psk-qam` grid its EVM.
#[cfg(test)]
mod t296;

/// Dev seeds: used to fit densities and set thresholds, never to report accuracy.
pub const DEV_SEEDS: std::ops::Range<u64> = 0..600;

/// Acceptance seeds start here (ADR-0016 §7: disjoint from the dev range).
pub const ACCEPTANCE_SEED_BASE: u64 = 1_000_000;

/// `hk-mod@1` families this module deliberately generates nothing for, and which therefore cannot
/// have an out-of-taxonomy negative either.
///
/// Only `dsss`: it has no estimator and no generator in M3 and the tree denies it outright
/// (ADR-0016 §1, "may always abstain in M3"), so there is no family boundary to probe. This is a
/// **declared** exemption rather than a silent gap — the whole point of T-244 is that an
/// unmeasured family must say so out loud.
pub const UNGENERATED_FAMILIES: &[&str] = &["dsss"];

/// Every family whose open set must be measured: `hk-mod@1` minus [`UNGENERATED_FAMILIES`].
///
/// Adding a family to the taxonomy adds it here automatically, so the coverage check starts failing
/// until a held-out generator probes it ([`Class::probes_family`]). That is the intended order: a
/// new family with no negative is a hole in the gate, not a family with a perfect open set.
pub fn open_set_families() -> Vec<&'static str> {
    hk_model::classify::HK_MOD_V1
        .families
        .iter()
        .map(|f| f.name)
        .filter(|name| !UNGENERATED_FAMILIES.contains(name))
        .collect()
}

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
    /// **Held out:** double-sideband suppressed-carrier AM.
    DsbSc,
    /// **Held out:** vestigial-sideband AM, carrier retained.
    VsbAm,
    /// **Held out:** π/4-DQPSK.
    Pi4Dqpsk,
    /// **Held out:** 16-APSK (two rings, DVB-S2 geometry).
    Apsk16,
    /// **Held out:** Barker-coded (pulse-compression) radar pulses.
    CodedPulse,
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
        Class::DsbSc,
        Class::VsbAm,
        Class::Pi4Dqpsk,
        Class::Apsk16,
        Class::CodedPulse,
    ];

    /// The held-out generators **ADR-0016 §7 itself enumerates** ("3-level ASK, FSK with a chirped
    /// carrier, Costas-hopped tones, 8-level FSK, OFDM with a non-standard CP, random-phase noise
    /// bursts"), and so the population its two held-out floors — unknown recall ≥ 0.80,
    /// false-known ≤ 0.10 — are stated over.
    ///
    /// T-244 added five more so that `analog`, `psk-qam` and `pulsed` have a negative at all. The
    /// ADR says nothing about those, and a floor stated over one population is neither satisfied
    /// nor broken by another: both are measured, both are reported, and only this one carries the
    /// ADR's floors. Neither is ever loosened. What floor the fuller population should carry is
    /// T-206's decision, with the measured numbers in the build log.
    pub const ADR_HELD_OUT: &'static [Class] = &[
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
            Class::DsbSc => "held-out:dsb-sc",
            Class::VsbAm => "held-out:vsb-am",
            Class::Pi4Dqpsk => "held-out:pi4-dqpsk",
            Class::Apsk16 => "held-out:apsk16",
            Class::CodedPulse => "held-out:coded-pulse",
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
            Class::DsbSc | Class::VsbAm => Some("analog"),
            Class::Pi4Dqpsk | Class::Apsk16 => Some("psk-qam"),
            Class::CodedPulse => Some("pulsed"),
            // A chirped-carrier FSK, Costas hopping and noise bursts belong to no family here.
            _ => None,
        }
    }

    /// For a **held-out** generator, the family whose **open set** it is a negative for: the family
    /// whose model would be asked about it, and which has to answer "not one of mine".
    ///
    /// This is deliberately not [`Class::nearest_family`], which answers a different question —
    /// *does this generator genuinely belong to a family*, so that naming it is generalisation
    /// rather than a false known — and is `None` for the three generators that belong nowhere.
    /// Routing the open-set measurement by that instead dropped those three from every measurement
    /// and left `analog`, `psk-qam` and `pulsed` with no negatives at all, so their AUROC and
    /// false-known rate were never computed while the gate still reported an unknown-recall figure
    /// (T-244). A generator that belongs nowhere still *resembles* something, and that resemblance
    /// is what its family's model must reject.
    ///
    /// Every family in [`open_set_families`] appears here at least once, which
    /// `every_family_with_a_generator_has_an_out_of_taxonomy_negative` and the harness's coverage
    /// check both enforce. A taxonomy class is a negative for nothing: it returns `None`.
    pub fn probes_family(self) -> Option<&'static str> {
        match self {
            Class::Ask3 => Some("ook-ask"),
            // 8-FSK is an unlisted level count; a 2-FSK whose carrier also sweeps is not FSK at
            // all. Both are what an `fsk` model is handed and must not claim.
            Class::Fsk8 | Class::ChirpedFsk => Some("fsk"),
            Class::OfdmOddCp => Some("ofdm"),
            // A Costas hop set is the frequency-agile waveform a chirp model must not accept: it
            // moves in frequency over time, as `chirp` does, but in steps and by a permutation.
            Class::CostasHop => Some("css"),
            Class::NoiseBurst => Some("noise-like"),
            Class::DsbSc | Class::VsbAm => Some("analog"),
            Class::Pi4Dqpsk | Class::Apsk16 => Some("psk-qam"),
            Class::CodedPulse => Some("pulsed"),
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
    /// The same emission at **C14's** geometry ([`crate::symbols::SYMBOL_SAMPLES_PER_OBW`]), for
    /// blind symbol estimation (T-238). This is not a second waveform: it is the same filtered,
    /// recentred samples decimated less far, because a symbol clock is not visible at the
    /// classifier's 2 samples per OBW99. See [`crate::symbols`].
    pub symbol_samples: Vec<Complex32>,
    /// Sample rate of [`SynthSignal::symbol_samples`], Hz.
    pub symbol_sample_rate_hz: f64,
    /// What was generated (truth: for assertions and fitting only).
    pub class: Class,
    /// In-band SNR it was generated at, dB.
    pub snr_db: f64,
}

/// Samples per OBW99 the classifier is handed in production, from
/// `hk_estimate::normalise::NormaliseConfig`'s defaults: `samples_per_obw` 2.0 against a
/// `bandwidth_obw` 1.5 channel, and the rate is never allowed below `1.2 × bandwidth` — so
/// `max(2.0, 1.8) = 2.0` times OBW99.
///
/// The dev grid has to land here too. It is not a detail: several `features@1` dimensions are
/// defined **per sample** (`sigma_af` and `sigma_ap` are rad/sample, `if_std_norm` divides by
/// OBW), so the same emission measured at 2 and at 20 samples per OBW gives completely different
/// numbers. Fitting at one geometry and classifying at another compares nothing.
pub const SAMPLES_PER_OBW: f64 = 2.0;

/// Sample rate to generate `class` at, given the configured rate.
///
/// Most classes carry a bandwidth proportional to their symbol rate, so they already sit within a
/// small factor of the analysis geometry and [`generate`]'s decimation finishes the job. The
/// analog classes do not: their bandwidths are fixed by what they carry (a 3 kHz SSB sideband, a
/// 2 kHz keyed tone), and at 1 Msps an SSB emission occupies 0.3 % of the snippet. The in-band SNR
/// is then set over 3 kHz while the *snippet* holds a megahertz of noise, so the measured OBW99
/// stops describing the emission at all — measured, `ssb` fitted at OBW 251 kHz and `cw` at
/// 767 kHz, i.e. those two densities were fitted on band noise rather than on a signal. That is
/// what made them catch-alls wide enough to claim a band-filling multicarrier emission, and it is
/// the whole of the held-out false-known rate (T-235).
///
/// Generating them at a rate commensurate with what they actually occupy is what a receiver does:
/// nobody analyses a 3 kHz SSB channel at 1 Msps.
fn analysis_rate(class: Class, configured_hz: f64) -> f64 {
    let nominal_bw: f64 = match class {
        // A keyed carrier occupies what its **keying** occupies — a few times the 20 WPM element
        // rate, ~150 Hz — not the 2 kHz figure the noise is scaled over. Getting this entry wrong
        // in either direction makes `cw` a catch-all: left at 1 Msps it is fitted on band noise
        // and claims a band-filling multicarrier emission, and given a 2 kHz-derived rate it
        // becomes a generic on-off-keyed carrier and claims the held-out 3-level ASK generator
        // (both measured, T-235).
        Class::Cw => 150.0,
        Class::Ssb => 3e3,
        // The suppressed-carrier and vestigial-sideband generators carry the same 4.5 kHz
        // programme audio as `am`, so they occupy the same band and must be analysed over it: at
        // 1 Msps their density would be fitted on band noise instead, which is the catch-all bug
        // T-235 measured.
        Class::Am | Class::DsbSc | Class::VsbAm => 9e3,
        Class::Nbfm => 16e3,
        // Everything else scales with its symbol rate or already fills the band.
        _ => return configured_hz,
    };
    // Enough headroom for the emission and its skirts before the decimation below trims to the
    // analysis geometry.
    (12.0 * nominal_bw).min(configured_hz)
}

/// Generates one waveform.
pub fn generate(class: Class, cfg: &SynthConfig) -> SynthSignal {
    let mut rng = Rng::new(cfg.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ class as u64);
    let n = cfg.samples;
    let fs = analysis_rate(class, cfg.sample_rate_hz);
    // Symbol rate varies with the seed so the densities never learn one rate.
    let rate = 25e3 + 75e3 * rng.unit();
    let (mut x, design_bw) = waveform(class, &mut rng, n, fs, rate);
    normalise(&mut x);

    // Noise at the requested in-band SNR: N₀ = σ²/fs, so σ² = P_s·fs/(BW·10^(SNR/10)).
    // Floor relative to the rate rather than an absolute kilohertz: a genuinely narrow emission
    // generated at a commensurate rate (a keyed carrier's ~150 Hz of keying sidebands) would
    // otherwise have its noise scaled over a bandwidth far wider than it occupies, which sets its
    // SNR to something other than the requested one.
    let bw = design_bw.clamp(0.001 * fs, 0.9 * fs);
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
    // Recentred on the emission the way C13 measures it — the carrier line where there is one, the
    // spectral centroid where there is not — and **not** with a deliberate residual offset added.
    // This was the strongest *bin* for every class until T-296, which is a different quantity
    // entirely on a flat-spectrum emission and cost the psk-qam grid 38.5 N₀ before any receiver
    // touched it — see [`recentre_offset_hz`]. Leaving a few per cent of the bandwidth uncorrected
    // was tried (T-235)
    // to widen `symmetry`, which is the dimension a real off-centre detection box lands furthest
    // out on. It is a real effect, but as a grid-wide knob it is destructive: on a narrowband
    // emission a few per cent of the band is a large fraction of the deviation, so `nbfm`'s
    // instantaneous-frequency distribution smeared and its density grew into a catch-all. Measured:
    // held-out unknown recall fell 0.861 -> 0.731, below the ADR-0016 floor, and the real 915 MHz
    // FSK burst came back `analog`/`nbfm` at confidence 1.00 with open-set 0.00 — a confidently
    // wrong label on a real signal, which is the one outcome this classifier may never produce.
    let peak_offset = recentre_offset_hz(&samples, fs);
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
    // Decimation alone is enough below, and is alias-free by construction: it only ever engages
    // when OBW99 is below a quarter of the rate, which is exactly when the channel filter above
    // has already cut everything beyond ±0.75 × OBW99 — comfortably inside the new Nyquist limit.
    // The floor on the output length keeps the feature vector measurable (the shape features need
    // several FFT segments, and `cp_corr` needs four times its longest lag).
    let max_decim = (samples.len() / 2048).max(1);
    // **C14's view, taken before the analysis decimation throws the symbol clock away** (T-238).
    // A symbol rate is, for most classes, within a small factor of the occupied bandwidth, so at
    // [`SAMPLES_PER_OBW`] = 2 a symbol period is about two samples and sits past the top of C14's
    // own search range: there is no cyclic line left to find. Production does not reuse the
    // classifier's snippet for C14 either — `BlindEstimator::prepare` re-normalises to
    // `symbols::SYMBOL_SAMPLES_PER_OBW` — so the dev grid must hand C14 the same geometry, taken
    // from the same filtered, recentred waveform rather than generated separately.
    let symbol_decim = ((fs / (crate::symbols::SYMBOL_SAMPLES_PER_OBW * obw_hz)).floor() as usize)
        .clamp(1, max_decim);
    let (symbol_samples, symbol_sample_rate_hz) = if symbol_decim > 1 {
        (
            samples
                .iter()
                .step_by(symbol_decim)
                .copied()
                .collect::<Vec<_>>(),
            fs / symbol_decim as f64,
        )
    } else {
        (samples.clone(), fs)
    };
    // Resample to the analysis geometry the pipeline actually delivers ([`SAMPLES_PER_OBW`]).
    let decim = ((fs / (SAMPLES_PER_OBW * obw_hz)).floor() as usize).clamp(1, max_decim);
    let (samples, fs) = if decim > 1 {
        (
            samples.iter().step_by(decim).copied().collect::<Vec<_>>(),
            fs / decim as f64,
        )
    } else {
        (samples, fs)
    };
    // OBW99 in hertz does not change with the rate, but re-measuring it at the delivered rate is
    // what C13 would report on this snippet, and it is what the classifier is handed.
    let obw_hz = measured_obw(&samples, fs);
    SynthSignal {
        samples,
        sample_rate_hz: fs,
        obw_hz,
        symbol_samples,
        symbol_sample_rate_hz,
        class,
        snr_db: cfg.snr_db,
    }
}

/// The waveform and its design bandwidth (used only to scale the noise to the requested SNR).
fn waveform(class: Class, rng: &mut Rng, n: usize, fs: f64, rate: f64) -> (Vec<Complex64>, f64) {
    match class {
        Class::Am => (am(rng, n, fs, 0.7), 9e3),
        Class::Nbfm => {
            let dev = 2.5e3 + 2.5e3 * rng.unit();
            (fm(rng, n, fs, dev, 3.4e3), 16e3)
        }
        Class::Wfm => wfm_multiplex(rng, n, fs),
        Class::Ssb => (ssb(rng, n, fs), 3e3),
        Class::Cw => (cw(rng, n, fs), 150.0),
        Class::DsbSc => (dsb_sc(rng, n, fs), 9e3),
        Class::VsbAm => (vsb_am(rng, n, fs), 9e3),
        Class::Ook => (ask(rng, n, fs, rate, &[0.0, 1.0]), 2.0 * rate),
        Class::Ask4 => (ask(rng, n, fs, rate, &[0.25, 0.5, 0.75, 1.0]), 2.0 * rate),
        Class::Ask3 => (ask(rng, n, fs, rate, &[0.0, 0.5, 1.0]), 2.0 * rate),
        // The modulation index h = 2·deviation/Rs is a **free parameter of every deployed FSK
        // system**, not a constant: ISM telemetry runs near 0.5, POCSAG and AIS near 1, older
        // radio-telemetry well above. Fixing it at one value per class (h was exactly 1.0 for
        // `2fsk`, 0.7 for `gfsk`) fits a density to one point of a continuum, and everything that
        // depends on the deviation-to-rate ratio — `flatness`, `if_std_norm`, `if_bimodality`,
        // `carrier_line_db` — then rejects a real burst anywhere else on it. The real 915 MHz
        // sensor bursts in `fixtures/hackrf/2026-09-13` run h ≈ 0.53–0.55, which the grid did not
        // contain at all. The range below is the deployed range, a-priori; it is not centred on
        // any fixture.
        Class::Fsk2 => {
            let h = 0.4 + 1.2 * rng.unit();
            let pre = 0.1 + 0.25 * rng.unit();
            (
                cpfsk(rng, n, fs, rate, 2, rate * h / 2.0, 0.0, pre),
                rate * (1.0 + h),
            )
        }
        Class::Gfsk => {
            let h = 0.3 + 0.6 * rng.unit();
            let bt = 0.3 + 0.3 * rng.unit();
            let pre = 0.1 + 0.25 * rng.unit();
            (
                gfsk(rng, n, fs, rate, rate * h / 2.0, bt, pre),
                rate * (1.0 + h),
            )
        }
        // MSK is the h = 0.5 case by definition (ADR-0016 §1), so its index is not a free
        // parameter and stays fixed.
        Class::Msk => {
            let pre = 0.1 + 0.25 * rng.unit();
            (
                cpfsk(rng, n, fs, rate, 2, rate * 0.25, 0.0, pre),
                1.5 * rate,
            )
        }
        Class::Fsk4 => {
            let h = 0.4 + 1.0 * rng.unit();
            let pre = 0.1 + 0.25 * rng.unit();
            (
                cpfsk(rng, n, fs, rate, 4, rate * h / 2.0, 0.0, pre),
                rate * (1.0 + 1.5 * h),
            )
        }
        Class::Fsk8 => (cpfsk(rng, n, fs, rate, 8, rate * 0.5, 0.0, 0.0), 5.0 * rate),
        Class::ChirpedFsk => (
            cpfsk(rng, n, fs, rate, 2, rate * 0.5, 4.0 * rate / n as f64, 0.0),
            3.0 * rate,
        ),
        Class::Bpsk => (linear(rng, n, fs, rate, 2), 1.35 * rate),
        Class::Qpsk => (linear(rng, n, fs, rate, 4), 1.35 * rate),
        Class::Psk8 => (linear(rng, n, fs, rate, 8), 1.35 * rate),
        Class::Qam16 => (linear(rng, n, fs, rate, 16), 1.35 * rate),
        Class::Qam64 => (linear(rng, n, fs, rate, 64), 1.35 * rate),
        Class::Pi4Dqpsk => (pi4_dqpsk(rng, n, fs, rate), 1.35 * rate),
        Class::Apsk16 => (apsk16(rng, n, fs, rate), 1.35 * rate),
        Class::Ofdm => (ofdm(rng, n, 128, 32, 100), 0.8 * fs),
        Class::OfdmOddCp => (ofdm(rng, n, 96, 3, 70), 0.72 * fs),
        Class::Chirp => (chirp(n, fs, 125e3, 1024), 125e3),
        Class::Ppm => (ppm(rng, n), 0.5 * fs),
        Class::Pulse => (pulses(n, 10, 200), 0.5 * fs),
        Class::CodedPulse => (coded_pulses(n, 4, 300), 0.5 * fs),
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

/// Band-limited programme audio over `[lo_hz, hi_hz]`, peak-normalised to ±1: tones placed
/// log-uniformly (so every octave is represented) with a **1/√f** amplitude weighting.
///
/// Real programme material — speech or music — is bass dominant, its power falling roughly 3 dB
/// per octave above a couple of hundred hertz. That matters here far more than it looks: an angle
/// modulator *integrates* its baseband, so it is the low-frequency energy that decides how far the
/// carrier's phase wanders, and the phase-residual features (`sigma_ap`, `sigma_dp`) measure
/// exactly that. The previous generator summed five equal-weight tones over the **upper** 80 % of
/// the baseband (10.6–53 kHz for broadcast FM), whose phase residual is orders of magnitude
/// smaller than any real transmission's: measured on `fixtures/hackrf/2026-09-13`, a real
/// broadcast station sat at z = +494 in `sigma_ap` against `wfm` and z = +19 against `nbfm`, which
/// was the whole of its distance from the analog family (T-235).
///
/// The spectrum shape is a-priori — the programme-audio spectrum, not anything measured on the
/// fixture — and only the tone placement and a modest per-tone gain vary with the seed.
fn audio_band(rng: &mut Rng, n: usize, fs: f64, lo_hz: f64, hi_hz: f64) -> Vec<f64> {
    let tones: Vec<(f64, f64, f64)> = (0..8)
        .map(|_| {
            let f = lo_hz * (hi_hz / lo_hz).powf(rng.unit());
            let a = (lo_hz / f).sqrt() * (0.5 + rng.unit());
            (f, a, TAU * rng.unit())
        })
        .collect();
    let mut v: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            tones
                .iter()
                .map(|(f, a, p)| a * (TAU * f * t + p).cos())
                .sum::<f64>()
        })
        .collect();
    // Peak-normalised, so a caller's "deviation" really is the peak deviation.
    let peak = v.iter().fold(0.0_f64, |m, x| m.max(x.abs())).max(1e-9);
    for x in v.iter_mut() {
        *x /= peak;
    }
    v
}

/// A broadcast-FM **multiplex**, not a bare tone-modulated carrier: mono sum, 19 kHz stereo pilot,
/// the L−R difference as DSB-SC on 38 kHz, and an RDS-like subcarrier on 57 kHz. Returns the
/// samples and the Carson bandwidth of what was generated.
///
/// This is the signal a real receiver sees, and it is what the `wfm` density has to be fitted on
/// if a real station is to be recognised. The structure and its deviations are a-priori, from
/// ITU-R BS.450 / EN 50067 and the project's own standards-derived generator
/// (`py/hkpy/synth/scenarios.py::fm_broadcast_rds`): pilot at 9 % of peak deviation, RDS at
/// 1187.5 bd biphase on the third pilot harmonic. Nothing here is fitted to a capture — the RDS
/// bits are random, because the classifier never decodes them and only the subcarrier's spectral
/// footprint reaches `features@1`.
fn wfm_multiplex(rng: &mut Rng, n: usize, fs: f64) -> (Vec<Complex64>, f64) {
    const PILOT_HZ: f64 = 19_000.0;
    const RDS_BD: f64 = 1187.5;
    // Broadcast audio processing, which every station runs: heavy compression and limiting, so
    // the programme sits near **peak** deviation almost all the time instead of peaking there
    // occasionally. It is the single thing that decides how wide the emission actually is — an
    // uncompressed peak-normalised baseband deviates by a small fraction of its peak on average,
    // which measured 87 kHz OBW against the 146 kHz a real station occupies, and left the real
    // capture 18 sigma away in `sigma_af` (T-235). A-priori broadcast practice (ITU-R BS.412),
    // not anything fitted.
    let drive = 3.0 + 3.0 * rng.unit();
    let left = compress(&audio_band(rng, n, fs, 50.0, 15e3), drive);
    let right = compress(&audio_band(rng, n, fs, 50.0, 15e3), drive);
    // Most broadcast stations run stereo; a mono one is a real and common case.
    let stereo = rng.unit() < 0.8;
    let mono_dev = 45e3 + 30e3 * rng.unit();
    let pilot_dev = 0.09 * mono_dev;
    let stereo_dev = 20e3 + 25e3 * rng.unit();
    let rds_dev = 1.5e3 + 1.0e3 * rng.unit();
    // Biphase RDS symbols: each bit is a half-symbol pair (+,−) or (−,+), which is what puts the
    // subcarrier's energy either side of 57 kHz rather than on it.
    let half = (fs / (2.0 * RDS_BD)).max(1.0);
    let bits: Vec<bool> = (0..(n as f64 / (2.0 * half)).ceil() as usize + 2)
        .map(|_| rng.next_u64() & 1 == 1)
        .collect();
    let mut mpx: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            let w = TAU * PILOT_HZ * t;
            let k = (i as f64 / half) as usize;
            let bi = k / 2;
            let first_half = k % 2 == 0;
            let bb = if bits.get(bi).copied().unwrap_or(false) == first_half {
                1.0
            } else {
                -1.0
            };
            let mut v = mono_dev * 0.5 * (left[i] + right[i]);
            if stereo {
                v += pilot_dev * w.sin();
                v += stereo_dev * 0.5 * (left[i] - right[i]) * (2.0 * w).sin();
            }
            v + rds_dev * bb * (3.0 * w).sin()
        })
        .collect();
    // The **total** peak deviation is regulated, not the sum of what each component would like:
    // ITU-R BS.450 caps a broadcast FM carrier at 75 kHz, and a processed station runs just under
    // it. Scaling the finished multiplex to that cap is what makes the emission the width a real
    // station is (~180-220 kHz by Carson) and sets the instantaneous-frequency spread the
    // `sigma_af` and `sigma_ap` features measure. Without it the components' peaks add to an
    // arbitrary total and the deviation is whatever falls out.
    let peak_dev = 60e3 + 15e3 * rng.unit();
    let scale = peak_dev / mpx.iter().fold(0.0_f64, |m, v| m.max(v.abs())).max(1e-9);
    for v in mpx.iter_mut() {
        *v *= scale;
    }
    let mut phase = 0.0;
    let x = mpx
        .iter()
        .map(|v| {
            phase = (phase + TAU * v / fs) % TAU;
            Complex64::new(phase.cos(), phase.sin())
        })
        .collect();
    // Carson over the whole multiplex: the highest baseband component is the RDS subcarrier.
    (x, (2.0 * (peak_dev + 57e3 + 2.4e3)).min(0.45 * fs))
}

fn am(rng: &mut Rng, n: usize, fs: f64, depth: f64) -> Vec<Complex64> {
    let a = audio_band(rng, n, fs, 100.0, 4.5e3);
    a.iter()
        .map(|m| Complex64::new(1.0 + depth * m, 0.0))
        .collect()
}

/// Soft compression/limiting, peak-normalised: `tanh(drive·x)/tanh(drive)`. Raises the mean level
/// towards the peak the way an audio processor does, without adding the hard-clipping harmonics a
/// plain clip would.
fn compress(v: &[f64], drive: f64) -> Vec<f64> {
    let k = drive.max(1e-3);
    let norm = k.tanh();
    v.iter().map(|x| (k * x).tanh() / norm).collect()
}

fn fm(rng: &mut Rng, n: usize, fs: f64, deviation_hz: f64, audio_hz: f64) -> Vec<Complex64> {
    // Voice band from 300 Hz: narrowband FM carries speech, and its low end is what sets the
    // phase excursion (see [`audio_band`]).
    //
    // Deliberately **not** compressed, unlike the broadcast multiplex. Voice radios do limit, but
    // adding it here widened `nbfm` — already the broadest constant-envelope angle modulation in
    // the taxonomy — until its density claimed the real 915 MHz 2-FSK burst outright: measured,
    // `nbfm`'s m against that capture fell from 3.3 (abstain) to 1.7 with plausibility 1.000, and
    // the classifier returned `analog` at confidence 1.00 on a signal that is not analog (T-235).
    // Narrowband FM and 2-FSK are the same waveform family, so `nbfm` must stay no wider than its
    // own physics requires.
    let a = audio_band(rng, n, fs, 300.0, audio_hz);
    let mut phase = 0.0;
    a.iter()
        .map(|m| {
            phase = (phase + TAU * deviation_hz * m / fs) % TAU;
            Complex64::new(phase.cos(), phase.sin())
        })
        .collect()
}

/// **Held out:** double-sideband suppressed-carrier AM — the programme audio itself, with no
/// carrier added.
///
/// Legitimately out of `hk-mod@1` rather than a relabelled `am`: the taxonomy's analog classes are
/// `am` (carrier **plus** both sidebands), `ssb` (one sideband, no carrier), `nbfm`/`wfm` (angle
/// modulation) and `cw` (a keyed carrier). DSB-SC is none of them — it keeps both sidebands *and*
/// suppresses the carrier, so its envelope crosses zero and its phase reverses where an AM envelope
/// would merely dip, and it has no carrier line for a density to find. It is a deployed mode (the
/// L−R difference channel of stereo FM is DSB-SC on 38 kHz), not a contrivance.
fn dsb_sc(rng: &mut Rng, n: usize, fs: f64) -> Vec<Complex64> {
    let a = audio_band(rng, n, fs, 100.0, 4.5e3);
    a.iter().map(|m| Complex64::new(*m, 0.0)).collect()
}

/// **Held out:** vestigial-sideband AM — a retained carrier, one full sideband, and only a
/// *vestige* of the other.
///
/// Legitimately out of `hk-mod@1`: it is the one analog emission whose spectrum is deliberately
/// **asymmetric** (ITU-R BT.470 analog-television luma), so `am` describes it wrongly in one
/// direction and `ssb` in the other. Nothing in the taxonomy has a carrier and one-and-a-bit
/// sidebands.
fn vsb_am(rng: &mut Rng, n: usize, fs: f64) -> Vec<Complex64> {
    /// Below this baseband frequency the lower sideband survives in full; above it, not at all.
    const VESTIGE_HZ: f64 = 600.0;
    let tones: Vec<(f64, f64, f64)> = (0..6)
        .map(|_| {
            (
                200.0 + 4.3e3 * rng.unit(),
                0.3 + 0.7 * rng.unit(),
                TAU * rng.unit(),
            )
        })
        .collect();
    // Total modulation under 100 %, as a transmitter must keep it.
    let norm = 0.8 / tones.iter().map(|(_, a, _)| *a).sum::<f64>().max(1e-9);
    (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            let mut v = Complex64::new(1.0, 0.0);
            for (f, a, p) in &tones {
                let ph = TAU * f * t + p;
                let upper = Complex64::new(ph.cos(), ph.sin());
                let lower = Complex64::new(ph.cos(), -ph.sin());
                let vestige = (1.0 - f / VESTIGE_HZ).clamp(0.0, 1.0);
                v += (upper + lower * vestige) * (0.5 * a * norm);
            }
            v
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

/// A Morse-keyed carrier with real timing (ITU-R M.1677): the dit is the unit, a dah is three
/// units, elements within a letter are separated by one unit, letters by three and words by seven.
///
/// The **regularity** is the point. This used to key random-length symbols on and off with
/// probability 3/4 and no inter-element spacing at all, which is slow random OOK rather than
/// Morse. A randomly keyed carrier has a broad, seed-dependent envelope spectrum, so `gamma_max`
/// and `sigma_aa` were fitted with enormous sigmas and `cw` became a catch-all — the class that
/// absorbed whatever the others could not explain (T-235).
fn cw(rng: &mut Rng, n: usize, fs: f64) -> Vec<Complex64> {
    let unit = (0.06 * fs).max(2.0) as usize; // 20 WPM
    let tone = (0.05 * fs).min(800.0);
    let mut on: Vec<bool> = Vec::with_capacity(n + 8 * unit);
    while on.len() < n {
        let elements = 1 + (rng.next_u64() % 4) as usize;
        for e in 0..elements {
            let dah = rng.next_u64() & 1 == 1;
            let mark = (if dah { 3 } else { 1 }) * unit;
            on.resize(on.len() + mark, true);
            if e + 1 < elements {
                on.resize(on.len() + unit, false); // intra-letter gap: one unit
            }
        }
        // Three units between letters, seven between words.
        let gap = if rng.next_u64() % 5 == 0 { 7 } else { 3 };
        on.resize(on.len() + gap * unit, false);
    }
    (0..n)
        .map(|i| {
            let ph = TAU * tone * i as f64 / fs;
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

/// Continuous-phase FSK. The parameters are the independent physical knobs of a CPFSK burst —
/// level count, deviation, carrier drift and preamble length — so there is nothing to group here
/// that would not just be this list behind a name.
#[allow(clippy::too_many_arguments)]
fn cpfsk(
    rng: &mut Rng,
    n: usize,
    fs: f64,
    rate: f64,
    levels: usize,
    deviation_hz: f64,
    drift_hz_per_sample: f64,
    preamble_frac: f64,
) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let count = (n as f64 / sps).ceil() as usize + 1;
    // A real FSK burst is a packet: an alternating preamble for the receiver's clock and AGC,
    // then a sync word, then data. The preamble keys the two outer tones at exactly half the
    // symbol rate, which puts **discrete lines** in the spectrum where i.i.d. data puts a smooth
    // shoulder — the difference between a peaky and a flat channel, which `carrier_line_db` and
    // `flatness` both measure. A grid of nothing but i.i.d. symbols has no such lines at all.
    let preamble = ((count as f64) * preamble_frac.clamp(0.0, 0.9)) as usize;
    let syms: Vec<f64> = (0..count)
        .map(|i| {
            if i < preamble {
                return if i % 2 == 0 { -1.0 } else { 1.0 };
            }
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

fn gfsk(
    rng: &mut Rng,
    n: usize,
    fs: f64,
    rate: f64,
    deviation_hz: f64,
    bt: f64,
    preamble_frac: f64,
) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let count = (n as f64 / sps).ceil() as usize + 1;
    let preamble = ((count as f64) * preamble_frac.clamp(0.0, 0.9)) as usize;
    let syms: Vec<f64> = (0..count)
        .map(|i| {
            if i < preamble {
                return if i % 2 == 0 { -1.0 } else { 1.0 };
            }
            if rng.next_u64() & 1 == 1 { 1.0 } else { -1.0 }
        })
        .collect();
    let nrz: Vec<f64> = (0..n).map(|i| syms[(i as f64 / sps) as usize]).collect();
    // Gaussian pulse shaping at the caller's BT.
    let span = (sps * 2.0) as usize | 1;
    let sigma = sps * 0.5 / (TAU * bt);
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

/// Root-raised-cosine pulse span, symbol periods each side.
const RRC_SPAN: f64 = 6.0;

/// Root-raised-cosine amplitude at `t` **symbol periods** from the pulse centre.
///
/// A continuous function of the delay rather than a fixed tap set, because [`shaped_linear`] has to
/// evaluate it at each symbol's *exact* instant and those are not on the sample grid (T-296).
fn rrc_at(t: f64, alpha: f64) -> f64 {
    use std::f64::consts::PI;
    if t.abs() < 1e-9 {
        return 1.0 - alpha + 4.0 * alpha / PI;
    }
    if (t.abs() - 1.0 / (4.0 * alpha)).abs() < 1e-6 {
        let p = PI / (4.0 * alpha);
        return alpha / 2f64.sqrt() * ((1.0 + 2.0 / PI) * p.sin() + (1.0 - 2.0 / PI) * p.cos());
    }
    let pt = PI * t;
    ((pt * (1.0 - alpha)).sin() + 4.0 * alpha * t * (pt * (1.0 + alpha)).cos())
        / (pt * (1.0 - (4.0 * alpha * t).powi(2)))
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

/// A linearly-modulated burst: `symbol(k)` root-raised-cosine shaped at `sps` samples per symbol.
///
/// Every constellation below shares this, so they differ only in their symbol alphabet and nothing
/// else about the waveform (roll-off, timing, length) can drift between them.
///
/// # Each symbol sits at its exact instant, which is not a sample (T-296)
///
/// This used to upsample by writing each symbol into the **nearest whole sample**,
/// `up[(k·sps).round()]`. `sps` is `sample_rate / symbol_rate` and is an integer essentially never,
/// so `round(k·sps) − k·sps` walks over ±0.5 sample: every symbol in the record was displaced by a
/// different fraction of a symbol period. That is *per-symbol timing jitter*, not a timing offset —
/// no receiver can track it and no oracle over a uniform symbol grid can undo it, because the
/// instants are genuinely not uniformly spaced. Measured against the transmitted symbols at 30 dB
/// it cost **2.59 % EVM** on its own where an ideal waveform reaches 0.49 %.
///
/// Summing the continuous pulse at each symbol's true instant costs the same arithmetic (this is
/// tooling, not the real-time path) and removes it exactly.
fn shaped_linear(
    n: usize,
    sps: f64,
    alpha: f64,
    mut symbol: impl FnMut(usize) -> Complex64,
) -> Vec<Complex64> {
    // The symbol count the old tap-based construction drew, preserved so that only the *placement*
    // changed here and the seed still names the same symbol sequence.
    let taps_len = (2.0 * RRC_SPAN * sps) as usize | 1;
    let symbols = (n as f64 / sps).ceil() as usize + taps_len;
    let syms: Vec<Complex64> = (0..symbols).map(&mut symbol).collect();
    let half = RRC_SPAN * sps;
    let mut out = vec![Complex64::new(0.0, 0.0); n];
    for (k, s) in syms.iter().enumerate() {
        let c = k as f64 * sps;
        if c - half >= n as f64 {
            break;
        }
        let lo = (c - half).ceil().max(0.0) as usize;
        let hi = (c + half).floor().min((n - 1) as f64);
        if hi < lo as f64 {
            continue;
        }
        for (i, o) in out.iter_mut().enumerate().take(hi as usize + 1).skip(lo) {
            *o += *s * rrc_at((i as f64 - c) / sps, alpha);
        }
    }
    out
}

fn linear(rng: &mut Rng, n: usize, fs: f64, rate: f64, order: usize) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let alpha = 0.25 + 0.2 * rng.unit();
    shaped_linear(n, sps, alpha, |_| {
        constellation(order, (rng.next_u64() as usize) % order)
    })
}

/// **Held out:** π/4-DQPSK — every symbol advances the carrier phase by ±π/4 or ±3π/4, so the
/// constellation alternates between two QPSK sets and the trajectory never crosses the origin.
///
/// Legitimately out of `hk-mod@1`: the taxonomy's `psk-qam` classes are the absolute-phase
/// `bpsk`/`qpsk`/`8psk` and the square-lattice `qam16`/`qam64`. This one is **differential** and
/// eight-point-alternating — what TETRA and APCO-25 Phase 2 actually transmit — so no listed class
/// describes its constellation, and a model trained on those five has never seen it.
fn pi4_dqpsk(rng: &mut Rng, n: usize, fs: f64, rate: f64) -> Vec<Complex64> {
    let sps = (fs / rate).max(2.0);
    let alpha = 0.25 + 0.2 * rng.unit();
    let mut phase = 0.0_f64;
    shaped_linear(n, sps, alpha, |_| {
        // The four differential symbols: ±π/4 and ±3π/4.
        phase += std::f64::consts::FRAC_PI_4 * (2.0 * ((rng.next_u64() % 4) as f64) - 3.0);
        Complex64::new(phase.cos(), phase.sin())
    })
}

/// **Held out:** 16-APSK in the DVB-S2 geometry — 4 points on an inner ring, 12 on an outer ring at
/// γ = 2.85 times the radius, unit average power.
///
/// Legitimately out of `hk-mod@1`: it is neither a constant-modulus PSK nor a square QAM lattice,
/// so `8psk` and `qam16` are both wrong about it in a way that matters — two amplitude rings, and
/// twelve phases on the outer one. It is the satellite-downlink workhorse, not an invented shape.
fn apsk16(rng: &mut Rng, n: usize, fs: f64, rate: f64) -> Vec<Complex64> {
    const GAMMA: f64 = 2.85;
    // (4·r₁² + 12·(γr₁)²)/16 = 1.
    let r1 = (16.0 / (4.0 + 12.0 * GAMMA * GAMMA)).sqrt();
    let sps = (fs / rate).max(2.0);
    let alpha = 0.25 + 0.2 * rng.unit();
    shaped_linear(n, sps, alpha, |_| {
        let k = (rng.next_u64() as usize) % 16;
        let (r, ph) = if k < 4 {
            (r1, TAU * k as f64 / 4.0 + std::f64::consts::FRAC_PI_4)
        } else {
            (
                r1 * GAMMA,
                TAU * (k - 4) as f64 / 12.0 + std::f64::consts::PI / 12.0,
            )
        };
        Complex64::new(r * ph.cos(), r * ph.sin())
    })
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

/// **Held out:** a pulse-compression radar train — each pulse carries a 13-chip Barker phase code.
///
/// Legitimately out of `hk-mod@1`: the `pulsed` family's classes are `pulse` (an unmodulated
/// rectangular train) and `ppm` (information in the pulse *position*). A Barker-coded pulse is
/// neither — its envelope is a plain rectangle at a fixed position, and all of its information is
/// intra-pulse phase, which widens the spectrum by the chip rate while leaving the envelope
/// identical to `pulse`. That is exactly the discrimination a `pulsed` model has to make and was
/// never asked to.
fn coded_pulses(n: usize, chip: usize, pri: usize) -> Vec<Complex64> {
    const BARKER13: [f64; 13] = [
        1.0, 1.0, 1.0, 1.0, 1.0, -1.0, -1.0, 1.0, 1.0, -1.0, 1.0, -1.0, 1.0,
    ];
    let mut x = vec![Complex64::new(0.0, 0.0); n];
    let mut i = 0;
    while i < n {
        for (c, code) in BARKER13.iter().enumerate() {
            for s in x.iter_mut().skip(i + c * chip).take(chip) {
                *s = Complex64::new(*code, 0.0);
            }
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

/// Offset of the emission from the snippet centre, Hz: the harness's stand-in for C13's measured
/// carrier offset. **The carrier line where there is one, the spectral centroid where there is
/// not.**
///
/// # This was the strongest bin unconditionally, and that is what broke the `psk-qam` grid (T-296)
///
/// C13 does not use one estimator for everything: `params::finish_cfo` picks a squared line for
/// DSB, a fourth-power line for QPSK, an FSK midpoint for FSK, and falls back to
/// `Method::CfoCentroid` — a power-weighted centroid over the occupied band, with unclipped
/// noise-subtracted weights — only when nothing is known about the emission. The harness took the
/// **argmax bin** for every class, which is a different quantity entirely on an emission whose
/// spectrum is not a line.
///
/// A root-raised-cosine-shaped random data stream has a spectrum that is *flat* across its
/// passband, so its strongest bin is a coin toss among hundreds of statistically identical ones.
/// Measured over eight seeds the argmax landed 0.24–0.39 **symbol rates** off centre on six of
/// them. The harness then rotated the snippet by that spurious offset, so the constellation spun
/// through a large fraction of a cycle per symbol and arrived at the classifier as a ring:
/// **19.63 % ± 14.30 % EVM against the transmitted symbols, 38.53 N₀**, where the same waveform
/// recentred properly reads 3.00 % ± 0.24 % (0.90 N₀). Its seed-to-seed randomness is the whole of
/// that ±14.30 %, and it is why π/4-DQPSK was indistinguishable from `qpsk` and 16-APSK from
/// `qam16`: the densities were fitted on smeared clouds and the held-out generators were smeared
/// identically.
///
/// # Why this is not simply "use the centroid"
///
/// Switching every class to the centroid was measured too, and it is wrong in the other direction.
/// For a **carrier-bearing** emission the strongest bin *is* the carrier and the argmax is exactly
/// right, while a centroid is dragged off it by whatever the sidebands do. Nudging the carrier off
/// centre moves `symmetry`, which is the single dimension that separates the held-out VSB-AM from
/// `am` (T-286 measured it at z ≈ −18 there and on no other): a blanket centroid dropped VSB-AM's
/// abstention from 25/36 to 6/36 and the `analog` open set from 0.861 to 0.583.
///
/// So the rule switches on **whether there is a carrier at all**, which is T-286's own measured
/// test ([`crate::features::CARRIER_MIN_FRACTION`]: the strongest line's main lobe holding more
/// power than the whole rest of the occupied band). That is a property of the spectrum, not of the
/// class — the harness never looks at what it generated — and it reproduces the argmax bit-for-bit
/// on every carrier-bearing class while giving the flat-spectrum ones a centre that means
/// something.
fn recentre_offset_hz(samples: &[Complex32], fs: f64) -> f64 {
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
    let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
    let bins = psd.len();
    let (lo, hi) = crate::features::occupied_band(&psd);
    // The same median noise floor `occupied_band` subtracts, and left **unclipped** for the
    // centroid for the same reason C13 leaves it unclipped: the noise either side of the emission
    // then averages to zero instead of dragging the centroid towards the band centre.
    let mut sorted = psd.clone();
    sorted.sort_by(f64::total_cmp);
    let floor = sorted[sorted.len() / 2];
    let strongest = (lo..=hi)
        .max_by(|a, b| psd[*a].total_cmp(&psd[*b]))
        .unwrap_or((lo + hi) / 2);
    // Is there a carrier? Exactly `features::spectral_features`' test, on the same quantities.
    let net = |i: usize| (psd[i] - floor).max(0.0);
    let guard = crate::features::CARRIER_GUARD_BINS;
    let lobe: f64 = (strongest.saturating_sub(guard)..=(strongest + guard).min(bins - 1))
        .map(net)
        .sum();
    let band_net: f64 = (lo..=hi).map(net).sum();
    if band_net > 0.0 && lobe / band_net > crate::features::CARRIER_MIN_FRACTION {
        return s.bin_offset_hz(strongest);
    }
    let wsum: f64 = (lo..=hi).map(|i| psd[i] - floor).sum();
    if !(wsum.is_finite() && wsum > 0.0) {
        return 0.0;
    }
    (lo..=hi)
        .map(|i| (psd[i] - floor) * s.bin_offset_hz(i))
        .sum::<f64>()
        / wsum
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
            // The channel filter trims its own edge transients and the snippet is then decimated
            // to the analysis geometry ([`SAMPLES_PER_OBW`]), so the length is at most the
            // requested one and never below the floor that keeps the features measurable.
            assert!(
                (2000..=cfg.samples).contains(&a.samples.len()),
                "{}: {} samples",
                class.label(),
                a.samples.len()
            );
            assert!(
                a.sample_rate_hz > 0.0 && a.sample_rate_hz <= cfg.sample_rate_hz,
                "{}: rate {}",
                class.label(),
                a.sample_rate_hz
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

    /// **The T-244 coverage rule, enforced statically.** A family with no out-of-taxonomy generator
    /// has an *unmeasured* open set, and an unmeasured open set reported as a number reads like a
    /// pass. Before this test, `analog`, `psk-qam` and `pulsed` had no negative at all.
    #[test]
    fn every_family_with_a_generator_has_an_out_of_taxonomy_negative() {
        for family in open_set_families() {
            assert!(
                Class::HELD_OUT
                    .iter()
                    .any(|c| c.probes_family() == Some(family)),
                "no held-out generator probes {family}: its open set cannot be measured, so any \
                 unknown-recall or AUROC figure reported for it would be untested"
            );
        }
        let families = open_set_families();
        for c in Class::HELD_OUT {
            let probed = c
                .probes_family()
                .unwrap_or_else(|| panic!("{} probes no family", c.label()));
            assert!(
                families.contains(&probed),
                "{} probes {probed}, which is not a measurable family",
                c.label()
            );
            // A generator that genuinely belongs to a family probes that same family: the two
            // notions may differ only where membership is `None`.
            if let Some(near) = c.nearest_family() {
                assert_eq!(near, probed, "{}", c.label());
            }
        }
        // A taxonomy class is a negative for nothing.
        for c in Class::TAXONOMY {
            assert_eq!(c.probes_family(), None, "{}", c.label());
        }
        // The one declared exemption, and the reason it is safe: nothing is generated for it, so
        // the classifier can never be asked to hold a boundary there.
        assert_eq!(UNGENERATED_FAMILIES, &["dsss"]);
        assert!(!open_set_families().contains(&"dsss"));

        // The ADR's own population is a subset of what is generated, so its floors are asserted
        // over exactly the generators it names and nothing is quietly added to or removed from it.
        assert_eq!(Class::ADR_HELD_OUT.len(), 6);
        for c in Class::ADR_HELD_OUT {
            assert!(Class::HELD_OUT.contains(c), "{}", c.label());
        }
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
