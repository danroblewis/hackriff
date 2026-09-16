//! The C15 feature vector, `features@N` ([`FEATURES_VERSION`]; ADR-0016 §4.2).
//!
//! One normalised snippet (CFO-corrected, unit mean power, cut to the burst extent) plus the C13
//! and C14 estimates give one vector of named features. **Every feature is a value or an
//! abstention**: a feature that needs an input the caller did not measure (a symbol rate, an OBW)
//! is `None`, and [`crate::density`] then scores the family over the dimensions that are present,
//! rather than inventing a value.
//!
//! The groups are the ADR's:
//! - **Azzouz–Nandi** instantaneous amplitude/phase/frequency statistics (`gamma_max`, `sigma_aa`,
//!   `sigma_ap`, `sigma_dp`, `sigma_af`);
//! - **higher-order cumulants** `C20`, `C40`, `C42` normalised by `C21`, plus the envelope and
//!   sample kurtoses. Cumulants are the classical order discriminators (BPSK Ĉ40 ≈ −2,
//!   QPSK |Ĉ40| ≈ 1, 8PSK ≈ 0) and are insensitive to additive Gaussian noise (C15 card);
//! - **instantaneous-frequency shape**: bimodality, mode count, linear-ramp fit (CSS) and spread;
//! - **spectral moments**: flatness, symmetry, carrier line and mean spectral kurtosis;
//! - **cyclic features**: the strongest C14 cyclic line and OBW/Rs, plus C14's own family scores;
//! - **cyclic-prefix correlation** for OFDM.
//!
//! Cost is µs–ms per snippet on one CPU core (ADR-0007 places this per event, off the ring and
//! DSP threads).

use hk_dsp::{WelchConfig, WindowKind, welch};
use hk_estimate::blind::SymbolParameters;
use num_complex::{Complex32, Complex64};

/// Feature-vector version, and its **single definition**:
/// [`crate::thresholds::FEATURES_VERSION`] — the value written into
/// [`hk_model::classify::ClassProvenance::features_version`] — re-exports this constant instead of
/// restating it, which is what let it sit at `1` through versions 2 and 3 (T-290).
///
/// **2 (T-248):** `symmetry` changed meaning — it is now sideband balance about the carrier (DC)
/// rather than about the occupied band's own mid-point, which measured ~0 by construction for
/// every emission including `ssb`. [`FEATURE_NAMES`] may grow within a version but a dimension may
/// never change meaning within one, so this is a new version and the shipped densities are refitted
/// against it.
///
/// **3 (T-286):** `symmetry` changed meaning again, and for the same class of reason: it was
/// measuring the carrier's own spectral leakage rather than the sidebands. Excluding a single bin
/// at the carrier leaves the rest of its main lobe — 10–30 dB above the sidebands being compared —
/// inside one of the two sums. Measured at 25 dB, `am` (double-sideband **by construction**, truth
/// 0.000) read −0.712 ± 0.023 and `cw` +0.284 ± 0.026, while the held-out VSB-AM read −0.655:
/// indistinguishable from AM on the one dimension that defines it. Guarding the window's whole
/// main lobe ([`CARRIER_GUARD_BINS`]) and integrating both sidebands over the band's widest half
/// restores `am` to +0.02 ± 0.03 and `cw` to +0.04 ± 0.01 and separates VSB-AM at −0.78 ± 0.13.
/// `symmetry` also **abstains where there is no carrier to measure it about**
/// ([`CARRIER_MIN_FRACTION`]), instead of reporting the noise it used to.
///
/// **4 (T-298):** `if_local_bimodality` and `if_local_modality` added — the level structure of the
/// instantaneous frequency measured **about its local trend** ([`IF_LOCAL_WINDOW`]) rather than
/// about the whole record's mean.
///
/// `if_bimodality` and `if_modality` ask whether the instantaneous frequency sits at discrete
/// levels, which is what separates a keyed carrier from an angle modulation. Both are statistics of
/// the histogram over the **entire** snippet, so both answer "no" whenever the levels themselves
/// move, and a keyed carrier's levels move for two ordinary reasons: the carrier drifts, and a long
/// record accumulates enough slow wander that the levels smear into each other. Measured on the dev
/// grid, the held-out chirped-carrier 2-FSK reads `if_modality` 1.04 ± 0.20 — one mode, i.e. *no*
/// level structure at all — where the same snippet detrended reads exactly 2.00 ± 0.00.
///
/// That is a defect of the reference the statistic is measured against, not of the waveform, and it
/// is the same class of defect as the two `symmetry` revisions above: a quantity defined relative to
/// the carrier was being measured relative to something else. An FSK keyed on a carrier that drifts
/// is still keying discrete levels — about its own carrier, which is linear across a short window
/// even when it is not across the record.
///
/// Measured on the dev grid at 20–30 dB (median over windows, mean ± sd over 72 snippets per
/// class), the local pair separates exactly where the global pair does not:
///
/// | class | `if_local_bimodality` | `if_local_modality` |
/// |---|---|---|
/// | `wfm` | 0.51 ± 0.04 | 1.62 ± 0.51 |
/// | `nbfm` | 0.41 ± 0.03 | 1.40 ± 0.57 |
/// | `am` | 0.31 ± 0.02 | 1.07 ± 0.25 |
/// | `2fsk` | 0.82 ± 0.04 | 2.00 ± 0.00 |
/// | `4fsk` | 0.56 ± 0.02 | 3.93 ± 0.25 |
/// | held-out chirped-FSK | **0.74 ± 0.11** | 2.00 ± 0.00 |
/// | held-out 8-FSK | 0.52 ± 0.01 | **4.47 ± 1.33** |
///
/// The two held-out FSK generators are each separated from `wfm` by one of the two dimensions and
/// not by the other, which is why both are added rather than either alone: Sarle's coefficient is a
/// *two*-mode statistic and falls back towards the uniform value as levels are added (8-FSK 0.52,
/// 4-FSK 0.56), while the mode count is what survives that and fails instead when the levels are
/// only two. Neither is a gate — both are density dimensions, so what they change is how far a
/// snippet sits from `wfm`, not what any rule is allowed to conclude.
pub const FEATURES_VERSION: u32 = 4;

/// Bins guarded either side of the carrier when measuring `symmetry`: the **main-lobe half-width
/// of the analysis window**, which [`spectral_features`] configures as [`WindowKind::Hann`].
///
/// A `K`-term cosine-sum window spreads a tone over a main lobe reaching `K + 1` bins either side
/// of it. Hann is the two-term series `0.5 − 0.5·cos x`, so `K = 1` and the half-width is 2 bins.
///
/// This is a property of the window rather than a tuned number: it is exactly the width over which
/// the carrier's own energy is spread, and therefore the width that has to come out before what is
/// left can be called a sideband. The check that it is right is that it puts the two emissions
/// whose sidebands are symmetric *by construction* — `am` and `cw` — back on their true value of
/// zero, which no choice fitted to an out-of-taxonomy generator would do.
pub const CARRIER_GUARD_BINS: usize = 2;

/// Share of the occupied band's power (net of the noise floor) that the strongest line's main lobe
/// must hold before `symmetry` is measured at all.
///
/// `symmetry` is sideband balance **about a carrier**. Where there is no carrier the strongest line
/// is an arbitrary bin and the quantity is undefined, so the feature abstains — `crate::density`
/// then scores the class over its other dimensions, which is this module's rule for every feature
/// whose input is missing.
///
/// Reporting it anyway is not free, and the cost was measured. With the T-286 guard fix but no
/// abstention rule, `symmetry` became tight and real for the carrier-bearing classes (`am` σ 0.032,
/// `cw` σ 0.018) while staying pure noise for the rest (`wfm` σ 0.533, `ssb` σ 0.636, over a
/// feature bounded to ±1). A dimension that is noise for four of the five analog classes still
/// costs them a χ² degree of freedom and a `ln σ` penalty, and known-family top-1 fell 0.9067 →
/// 0.8988, through the ADR-0016 §7 floor.
///
/// **One half, because that is what "carrier" means** — a single line holding more power than the
/// whole rest of the emission put together — and not because of where any measured gap fell.
/// Measured at 15–30 dB: `am` 0.95–0.96, the held-out VSB-AM 0.97–0.99, `cw` 0.58, against `ssb`
/// 0.39, DSB-SC 0.27, `nbfm` 0.12 and `wfm` 0.09. The emission this has to keep measurable is
/// VSB-AM, which sits at the very top of that range.
pub const CARRIER_MIN_FRACTION: f64 = 0.5;

/// Smallest snippet the feature tree will look at.
pub const MIN_SAMPLES: usize = 256;

/// Phase-step coherence below which the residual carrier offset is not corrected: the estimate
/// would be dominated by the modulation's own phase transitions rather than by the carrier.
pub const DEROTATE_MIN_COHERENCE: f64 = 0.3;

/// Smallest number of spectrum bins the shape features are measured over. A narrow emission (a
/// carrier, a CW tone) occupies one or two bins, where flatness is trivially 1 and the carrier
/// line trivially 0 dB — both meaningless, and both wrong in the direction of "this looks like
/// noise". Widening the window to its neighbourhood keeps the comparison honest.
pub const MIN_SHAPE_BINS: usize = 16;

/// Window over which the instantaneous frequency's level structure is measured **about the
/// carrier's local trend**, in samples (`if_local_bimodality`, `if_local_modality`).
///
/// This is not a new free parameter: 512 is the longest window [`ramp_linearity`] already fits a
/// straight line over, so `features@N` has asserted since version 1 that a carrier is linear across
/// it. The two requirements that decide the length meet there. The window has to be **long enough**
/// that a fourth central moment is stable, because Sarle's coefficient is built from the third and
/// fourth moments; and **short enough** that a drifting carrier really is a straight line across it.
///
/// It is also measured to be insensitive over the range where both hold, which is what says the
/// length is not doing the work: at 256 samples the same dev grid gives `wfm` 0.49 against the
/// held-out chirped-FSK's 0.71, and at 512 it gives 0.51 against 0.74 — the separation is the
/// same either way.
pub const IF_LOCAL_WINDOW: usize = 512;

/// Names of `features@N`, in vector order. The density files key on these names, so the order may
/// grow but never change meaning within a version.
pub const FEATURE_NAMES: &[&str] = &[
    "gamma_max",
    "sigma_aa",
    "sigma_ap",
    "sigma_dp",
    "sigma_af",
    "mu42_a",
    "env_cv",
    "low_fraction",
    "duty",
    "c20_norm",
    "c40_norm",
    "c42_norm",
    "mu42",
    "if_bimodality",
    "if_modality",
    "if_slope_r2",
    "if_std_norm",
    "flatness",
    "symmetry",
    "carrier_line_db",
    "sk_mean",
    "cp_corr",
    "cyclic_db",
    "obw_over_rs",
    "blind_ook",
    "blind_fsk",
    "blind_bpsk",
    "blind_qpsk",
    "if_local_bimodality",
    "if_local_modality",
];

/// The inputs a feature vector is computed from.
#[derive(Clone, Copy, Debug)]
pub struct FeatureInput<'a> {
    /// Normalised snippet samples (CFO-corrected, unit mean power).
    pub samples: &'a [Complex32],
    /// Sample rate of `samples`, Hz.
    pub sample_rate_hz: f64,
    /// C13 OBW99 of the emission, Hz, when measured.
    pub obw_hz: Option<f64>,
    /// C13 in-band SNR of the analysed extent, dB, when measured.
    pub snr_db: Option<f64>,
    /// C14 symbol estimate, when it ran.
    pub symbols: Option<&'a SymbolParameters>,
}

/// One `features@N` vector.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Features {
    /// Values in [`FEATURE_NAMES`] order; `None` = abstained.
    pub values: Vec<Option<f64>>,
    /// Machine reason codes for what could not be computed (`too_short`, `no_symbol_estimate`, …).
    pub reasons: Vec<String>,
}

impl Features {
    /// An all-abstaining vector with one reason.
    pub fn abstained(reason: &str) -> Self {
        Self {
            values: vec![None; FEATURE_NAMES.len()],
            reasons: vec![reason.to_owned()],
        }
    }

    /// The value of the named feature, if it was computed.
    pub fn get(&self, name: &str) -> Option<f64> {
        let i = FEATURE_NAMES.iter().position(|n| *n == name)?;
        self.values.get(i).copied().flatten()
    }

    /// How many features were computed.
    pub fn present(&self) -> usize {
        self.values.iter().filter(|v| v.is_some()).count()
    }

    fn set(&mut self, name: &str, value: f64) {
        if !value.is_finite() {
            return;
        }
        if let Some(i) = FEATURE_NAMES.iter().position(|n| *n == name) {
            self.values[i] = Some(value);
        }
    }

    /// Named values, abstentions skipped (for reports and provenance).
    pub fn named(&self) -> Vec<(&'static str, f64)> {
        FEATURE_NAMES
            .iter()
            .zip(&self.values)
            .filter_map(|(n, v)| v.map(|v| (*n, v)))
            .collect()
    }
}

/// Computes `features@N` for one normalised snippet.
pub fn features(input: &FeatureInput<'_>) -> Features {
    let n = input.samples.len();
    if n < MIN_SAMPLES || !(input.sample_rate_hz.is_finite() && input.sample_rate_hz > 0.0) {
        return Features::abstained("too_short");
    }
    let mut f = Features {
        values: vec![None; FEATURE_NAMES.len()],
        reasons: Vec::new(),
    };

    // Unit-power copy in f64. The mean is **not** removed: at baseband a residual carrier is part
    // of the modulation (it is what makes AM, OOK and CW look the way they do, and what gives
    // them |Ĉ20| ≈ 1), not a DC artefact to subtract. Subtracting it would turn an OOK gap into a
    // half-amplitude sample and erase the envelope contrast the family is recognised by.
    let raw: Vec<Complex64> = input
        .samples
        .iter()
        .map(|s| Complex64::new(f64::from(s.re), f64::from(s.im)))
        .collect();
    let power = raw.iter().map(|s| s.norm_sqr()).sum::<f64>() / n as f64;
    if !(power.is_finite() && power > 0.0) {
        return Features::abstained("no_signal_power");
    }
    let scale = 1.0 / power.sqrt();
    let unit: Vec<Complex64> = raw.iter().map(|s| s * scale).collect();
    // Remove whatever carrier offset C13's recentring left: the fourth-order cumulants average a
    // spinning phase to zero (four times the offset over the snippet), so even a fraction of a
    // cycle costs real discrimination. The mean instantaneous frequency over the samples whose
    // envelope is up estimates it for every symmetric modulation.
    let x = derotate(&unit);

    amplitude_features(&mut f, &x);
    phase_features(&mut f, &x);
    frequency_features(&mut f, &x, input);
    cumulant_features(&mut f, &x);
    spectral_features(&mut f, input);
    f.set("cp_corr", cyclic_prefix_correlation(&x));
    symbol_features(&mut f, input);

    if input.symbols.is_none() {
        f.reasons.push("no_symbol_estimate".into());
    }
    if input.obw_hz.is_none() {
        f.reasons.push("no_obw".into());
    }
    f
}

/// Removes the mean instantaneous frequency (the residual carrier offset) from a unit-power
/// sequence. Amplitude features are unaffected by the rotation; the phase and cumulant features
/// depend on it entirely.
fn derotate(x: &[Complex64]) -> Vec<Complex64> {
    let a: Vec<f64> = x.iter().map(|s| s.norm()).collect();
    let mean_a = a.iter().sum::<f64>() / a.len() as f64;
    let mut sum = Complex64::new(0.0, 0.0);
    let mut used = 0.0;
    for (i, w) in x.windows(2).enumerate() {
        if a[i] > 0.5 * mean_a && a[i + 1] > 0.5 * mean_a {
            // Averaging the phasor, not the angle, keeps the estimate free of wrapping.
            sum += w[1] * w[0].conj();
            used += (w[1] * w[0].conj()).norm();
        }
    }
    // Only correct when the phase steps agree with each other. A PSK or QAM signal steps by
    // ±90°/180° at random, so their average says nothing about the carrier — de-rotating by it
    // applies a *wrong* ramp and destroys exactly the fourth-order cumulants the modulation is
    // recognised by (measured: QPSK's |Ĉ40| fell from ~1 to 0.04). Angle modulations, whose steps
    // do agree, are corrected as intended.
    let coherence = if used > 0.0 { sum.norm() / used } else { 0.0 };
    if sum.norm() <= 0.0 || coherence < DEROTATE_MIN_COHERENCE {
        return x.to_vec();
    }
    let step = sum.arg();
    x.iter()
        .enumerate()
        .map(|(i, s)| {
            let ph = -step * i as f64;
            s * Complex64::new(ph.cos(), ph.sin())
        })
        .collect()
}

/// γ_max, σ_aa, envelope duty, low fraction and the envelope kurtosis.
fn amplitude_features(f: &mut Features, x: &[Complex64]) {
    let a: Vec<f64> = x.iter().map(|s| s.norm()).collect();
    let mean_a = a.iter().sum::<f64>() / a.len() as f64;
    if !(mean_a.is_finite() && mean_a > 0.0) {
        return;
    }
    // Azzouz–Nandi centred normalised amplitude a_cn = a/mean(a) − 1.
    let acn: Vec<f64> = a.iter().map(|v| v / mean_a - 1.0).collect();
    let sigma_aa = std_dev(&acn.iter().map(|v| v.abs()).collect::<Vec<_>>());
    f.set("sigma_aa", sigma_aa);
    f.set("env_cv", std_dev(&a) / mean_a);
    let m2 = a.iter().map(|v| v * v).sum::<f64>() / a.len() as f64;
    let m4 = a.iter().map(|v| v.powi(4)).sum::<f64>() / a.len() as f64;
    if m2 > 0.0 {
        f.set("mu42_a", m4 / (m2 * m2));
    }
    f.set(
        "low_fraction",
        a.iter().filter(|v| **v < 0.3 * mean_a).count() as f64 / a.len() as f64,
    );
    f.set(
        "duty",
        a.iter().filter(|v| **v > 0.5 * mean_a).count() as f64 / a.len() as f64,
    );
    // γ_max: the peak-to-mean of the spectrum of a_cn. A tone-modulated envelope (AM, OOK at a
    // fixed rate) concentrates its energy in one line; a constant envelope has none.
    if let Some(psd) = psd_of_real(&acn) {
        let mean = psd.iter().sum::<f64>() / psd.len() as f64;
        let peak = psd.iter().copied().fold(0.0_f64, f64::max);
        if mean > 0.0 {
            f.set("gamma_max", peak / mean);
        }
    }
}

/// σ_ap and σ_dp over the samples whose envelope clears the Azzouz–Nandi amplitude threshold.
fn phase_features(f: &mut Features, x: &[Complex64]) {
    let a: Vec<f64> = x.iter().map(|s| s.norm()).collect();
    let mean_a = a.iter().sum::<f64>() / a.len() as f64;
    let strong: Vec<usize> = (0..x.len()).filter(|i| a[*i] > 0.5 * mean_a).collect();
    if strong.len() < MIN_SAMPLES / 2 {
        return;
    }
    // Unwrapped phase with its linear trend (a residual CFO) removed: what is left is the
    // modulation's phase.
    let mut phase = Vec::with_capacity(x.len());
    let mut acc = 0.0;
    let mut prev = x[0].arg();
    for s in x {
        let p = s.arg();
        let mut d = p - prev;
        while d > std::f64::consts::PI {
            d -= std::f64::consts::TAU;
        }
        while d < -std::f64::consts::PI {
            d += std::f64::consts::TAU;
        }
        acc += d;
        prev = p;
        phase.push(acc);
    }
    // **These two are not length-invariant, and T-248 confirmed that cannot be fixed here.**
    //
    // σ_dp and σ_ap are the spread of the *unwrapped* phase residual. An angle modulator integrates
    // its baseband, so that residual performs a random walk and its spread grows with the
    // observation rather than being a per-sample quantity (`synth` calls σ_ap "rad/sample", which
    // is true of `sigma_af` but never was of these two). Measured on one emission: σ_ap 95.5 over a
    // 4 073-sample snippet against 252.3 over the 381 507-sample production capture, which drove
    // the real FM capture to z +5.06 on this dimension alone — a third of its whole distance from
    // `wfm`. The densities are fitted at ~4 000 samples and production classifies at 381 507, so
    // the two are not comparable, exactly as fitting at one *rate* is not (T-235).
    //
    // T-240 implemented the fix (fixed window, detrended independently, median across windows),
    // measured an open-set regression and reverted it. T-248 re-ran it with the claimed-family
    // plausibility rule and the per-dimension tail term already in place, on the theory that those
    // now reject a chirped carrier deliberately and the drift was no longer load-bearing. **That
    // theory is refuted by measurement.** With the length fix in: `chirped-fsk` abstention
    // 36/36 → 26/36, ten of them returning `analog` — a wrong *family*, not a generalisation —
    // `fsk` open set 0.931 → 0.792, held-out recall 0.8510 → 0.8384.
    //
    // The obvious repair fails too, and for a physical reason worth recording. Carrying the
    // discarded drift as its own dimension (each window's linear slope *is* its mean instantaneous
    // frequency, so the spread of those slopes is how far the carrier wandered) leaves
    // `chirped-fsk` at 26/36 with the same ten wrong-family calls, because the fitted
    // `carrier_drift` of `wfm` is **0.240 ± 0.174**: a wideband angle modulation's carrier
    // genuinely wanders as much as a chirped one does, so "the carrier moves" does not separate
    // them. The drift was never really rejecting a chirp — it was rejecting a *long observation*,
    // and `chirped-fsk` happens to be one.
    //
    // So the length dependence stands, now measured twice and with the replacement ruled out. It is
    // a real defect for long production snippets (the FM fixture abstains because of it) and it
    // needs a dimension that separates a swept carrier from a modulated one — `if_slope_r2` on a
    // per-window basis, or a cyclostationary test — which is new DSP, not a rescaling. Left to its
    // own task rather than smuggled in under an open-set ticket.
    let t: Vec<f64> = (0..phase.len()).map(|i| i as f64).collect();
    let (slope, intercept) = least_squares(&t, &phase);
    let residual: Vec<f64> = strong
        .iter()
        .map(|&i| phase[i] - (slope * t[i] + intercept))
        .collect();
    f.set("sigma_dp", std_dev(&residual));
    let abs: Vec<f64> = residual.iter().map(|v| v.abs()).collect();
    f.set("sigma_ap", std_dev(&abs));
}

/// Instantaneous-frequency shape: bimodality, mode count, linear-ramp fit and spread.
fn frequency_features(f: &mut Features, x: &[Complex64], input: &FeatureInput<'_>) {
    let a: Vec<f64> = x.iter().map(|s| s.norm()).collect();
    let mean_a = a.iter().sum::<f64>() / a.len() as f64;
    // rad/sample; only where the envelope is up, so OOK gaps do not fill the histogram with the
    // noise's phase walk.
    let fi: Vec<f64> = x
        .windows(2)
        .enumerate()
        .filter(|(i, w)| a[*i] > 0.5 * mean_a && a[i + 1] > 0.5 * mean_a && w[0].norm() > 0.0)
        .map(|(_, w)| (w[1] * w[0].conj()).arg())
        .collect();
    if fi.len() < MIN_SAMPLES / 2 {
        f.reasons.push("no_instantaneous_frequency".into());
        return;
    }
    let sigma = std_dev(&fi);
    f.set("sigma_af", sigma);
    if let Some(obw) = input.obw_hz.filter(|o| *o > 0.0) {
        // Spread relative to the occupied bandwidth: FSK deviation is a large fraction of OBW,
        // a linear modulation's is small.
        let hz = sigma * input.sample_rate_hz / std::f64::consts::TAU;
        f.set("if_std_norm", hz / obw);
    }
    f.set("if_bimodality", bimodality(&fi));
    f.set("if_modality", modality(&fi) as f64);
    // The ramp fit runs on a smoothed instantaneous frequency: a sweep is slow by construction,
    // while the per-sample estimate is noisy enough at the gates' SNRs to hide it (at 25 dB the
    // per-sample IF noise is comparable to a chirp's per-window excursion).
    f.set("if_slope_r2", ramp_linearity(&smooth(&fi, 8)));
    // The same level-structure question as `if_bimodality`/`if_modality`, asked about the carrier's
    // own local trend instead of the whole record's mean (T-298). Measured on the raw instantaneous
    // frequency, not the smoothed one: smoothing is what the ramp fit needs to see a slow sweep
    // through per-sample noise, and it would blur the level transitions this is counting.
    if let Some((bimodal, modes)) = local_level_structure(&fi) {
        f.set("if_local_bimodality", bimodal);
        f.set("if_local_modality", modes);
    }
}

/// Normalised cumulants Ĉ20, Ĉ40, Ĉ42 and the sample kurtosis.
fn cumulant_features(f: &mut Features, x: &[Complex64]) {
    let n = x.len() as f64;
    let c21 = x.iter().map(|s| s.norm_sqr()).sum::<f64>() / n;
    if !(c21.is_finite() && c21 > 0.0) {
        return;
    }
    let m2: Complex64 = x.iter().map(|s| s * s).sum::<Complex64>() / n;
    let m4: Complex64 = x.iter().map(|s| s * s * s * s).sum::<Complex64>() / n;
    let m4_abs = x.iter().map(|s| s.norm_sqr() * s.norm_sqr()).sum::<f64>() / n;
    let c20 = m2;
    let c40 = m4 - 3.0 * c20 * c20;
    let c42 = m4_abs - c20.norm_sqr() - 2.0 * c21 * c21;
    f.set("c20_norm", c20.norm() / c21);
    f.set("c40_norm", c40.norm() / (c21 * c21));
    f.set("c42_norm", c42 / (c21 * c21));
    f.set("mu42", m4_abs / (c21 * c21));
}

/// Spectral flatness, symmetry, carrier line and mean spectral kurtosis over the occupied band.
///
/// # The analysis resolution itself moves with the snippet length below n = 8192 (T-281)
///
/// `fft_len` is `(n/8).next_power_of_two().clamp(64, 1024)`, so a snippet shorter than 8192
/// samples is analysed at a *coarser* frequency resolution than a longer one. Every feature below
/// — and `gamma_max`, which sizes its PSD the same way in [`psd_of_real`] — is therefore measured
/// against a different yardstick depending on how long the emission was watched.
///
/// Measured on one 16 290-sample `noise-like` snippet at 25 dB, relative spread of each feature
/// over three prefixes where `fft_len` varies (2036 / 4072 / 8145) against four prefixes where it
/// is pinned at 1024 (8192 / 10240 / 12288 / 16290):
///
/// | feature | spread, `fft_len` varying | spread, `fft_len` pinned |
/// |---|---|---|
/// | `c42_norm` | 1.111 | 0.728 |
/// | `cp_corr` | 0.710 | 0.324 |
/// | `sk_mean` | 0.045 | 0.009 |
/// | `flatness` | 0.006 | 0.017 |
///
/// Pinning the transform removes most of the movement in the shape features but not all of it, so
/// the resolution change is *a* cause and not the only one. The clamp is left alone: widening it
/// would change every fitted density mean at once. Recorded, not tuned.
fn spectral_features(f: &mut Features, input: &FeatureInput<'_>) {
    let n = input.samples.len();
    let fft_len = (n / 8).next_power_of_two().clamp(64, 1024).min(n);
    let cfg = WelchConfig {
        fft_len,
        overlap: fft_len / 2,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let Ok(spectrum) = welch(input.samples, input.sample_rate_hz, 0.0, &cfg) else {
        f.reasons.push("no_spectrum".into());
        return;
    };
    let psd: Vec<f64> = spectrum
        .psd
        .iter()
        .map(|v| f64::from(*v).max(1e-30))
        .collect();
    let bins = psd.len();
    // Shape features are measured over the occupied band only: a wide snippet's empty skirts
    // would otherwise flatten every one of them. The band is widened to [`MIN_SHAPE_BINS`] so a
    // one-bin carrier is compared with its own neighbourhood rather than with itself.
    let (lo, hi) = occupied_band(&psd);
    let (lo, hi) = if hi - lo + 1 >= MIN_SHAPE_BINS {
        (lo, hi)
    } else {
        let centre = (lo + hi) / 2;
        let half = MIN_SHAPE_BINS / 2;
        let lo = centre.saturating_sub(half);
        let hi = (centre + half).min(bins - 1);
        (lo, hi)
    };
    let band = &psd[lo..=hi];
    let ln_mean = band.iter().map(|v| v.ln()).sum::<f64>() / band.len() as f64;
    let arith = band.iter().sum::<f64>() / band.len() as f64;
    if arith > 0.0 {
        f.set("flatness", (ln_mean.exp() / arith).clamp(0.0, 1.0));
    }
    // Sideband balance about the **carrier**: +1 lower-sideband only, −1 upper only, 0 balanced.
    //
    // The carrier is DC. The spectrum is DC-centred (`hk_dsp::fftshift_power`: bin N/2 is DC), and
    // C13 recentres a snippet on its measured spectral peak before handing it here — which
    // `crate::synth` reproduces deliberately — so for any carrier-bearing emission the carrier sits
    // at bin N/2 by construction.
    //
    // This was measured about `(lo + hi) / 2`, the mid-point of the **occupied band itself**
    // (T-248). Asymmetry about a band's own centre is ~0 by construction: the band is found by
    // growing outwards from the peak until 99 % of the power is enclosed, so it re-centres itself
    // on whatever it contains and cancels the very quantity the feature is named for. Measured on
    // the dev grid at 25 dB, the old definition gave `ssb` — one sideband and no carrier, the
    // extreme case this comment used to cite — **+0.046 ± 0.486**, and the held-out VSB-AM
    // −0.009 ± 0.987: zero, with noise-level scatter, for the two emissions whose defining property
    // is sideband asymmetry. The residual was driven by where `occupied_band` happened to land, and
    // `tree::analog_classes` has been calling `ssb` off `symmetry > 0.35` on that noise.
    //
    // The reference is the **strongest line in the band** — the carrier. It does not self-cancel,
    // and that is the whole point: a vestigial-sideband emission keeps its carrier exactly where it
    // is while the retained sideband drags the power centroid away from it, so a carrier reference
    // sees the imbalance that a centroid reference is constructed not to see. It is also
    // independent of any FFT ordering convention. Two earlier references were measured and are
    // wrong for the same underlying reason, that both re-centre themselves on the power they are
    // trying to weigh:
    //
    // - `(lo + hi) / 2`, the mid-point of the occupied band. The band is grown outwards from the
    //   peak until 99 % of the power is enclosed, so its mid-point follows the power and cancels
    //   the asymmetry. Measured on the dev grid at 25 dB it gave `ssb` — one sideband and no
    //   carrier, the extreme case — **+0.046 ± 0.486** and the held-out VSB-AM −0.009 ± 0.987:
    //   zero, with noise-level scatter, for the two emissions whose defining property this is.
    // - the array's mid-point `bins / 2`. `hk_dsp::fftshift_power` documents bin `N/2` as DC, but
    //   measured against it `am` — double-sideband by construction — read a systematic
    //   −0.705 ± 0.009, so the carrier of a recentred snippet does not in fact land there.
    //
    // **The carrier's whole main lobe is guarded, not just its peak bin** (T-286). A carrier is not
    // one bin: the window spreads it over [`CARRIER_GUARD_BINS`] either side, and a tone that does
    // not fall exactly on a bin centre spreads asymmetrically. Excluding only the peak leaves the
    // remainder — which for a carrier-bearing emission is 10–30 dB above the sidebands it is being
    // compared against — inside one of the two sums, so what the feature reports is which side of
    // its peak bin the carrier happened to straddle. Measured at 25 dB with the one-bin rule, `am`
    // — double-sideband **by construction**, so the truth is 0.000 — read a systematic
    // −0.712 ± 0.023, and `cw`, a keyed carrier whose sidebands are equally symmetric, +0.284 ±
    // 0.026. Both are the leak, not the signal; and the held-out VSB-AM read −0.655, i.e.
    // *indistinguishable from AM* on the one dimension that defines it. Guarding the main lobe
    // returns `am` to +0.02 ± 0.03 and `cw` to +0.04 ± 0.01 — their construction truth, which is
    // the check that the guard is a window property and not a number fitted to a generator — while
    // VSB-AM reads −0.78 ± 0.13.
    //
    // The two sums span the same frequency extent, sized to the band's **widest** half. Sizing it
    // to the narrower half (the previous rule) truncates a one-sided emission to its empty side,
    // which is precisely the emission this feature exists to find: VSB-AM's band reaches ~23 bins
    // above the carrier and ~7 below, so its retained sideband was integrated over 7 bins instead
    // of 23 and the estimate scattered by ±0.26 rather than ±0.13.
    //
    // For a suppressed-carrier emission there is no carrier and the quantity is undefined; the
    // strongest line is then an arbitrary bin and the answer is ~0, i.e. "balanced", which is the
    // honest reading. A band with no room to guard the main lobe **abstains** rather than
    // saturating at ±1, so `crate::density` scores the class over its other dimensions instead of
    // being handed an invented value.
    let centre = (lo..=hi)
        .max_by(|a, b| psd[*a].total_cmp(&psd[*b]))
        .unwrap_or((lo + hi) / 2);
    // Is there a carrier to measure a balance *about*? The main lobe's share of the occupied
    // band's power, both net of the noise floor, answers it (see [`CARRIER_MIN_FRACTION`]).
    let floor = median_of(&psd);
    let net = |i: usize| (psd[i] - floor).max(0.0);
    let lobe: f64 = (centre.saturating_sub(CARRIER_GUARD_BINS)
        ..=(centre + CARRIER_GUARD_BINS).min(bins - 1))
        .map(net)
        .sum();
    let band_net: f64 = (lo..=hi).map(net).sum();
    let carrier_fraction = if band_net > 0.0 { lobe / band_net } else { 0.0 };
    let half = centre.saturating_sub(lo).max(hi.saturating_sub(centre));
    let r = half.min(centre).min(bins - 1 - centre);
    if carrier_fraction > CARRIER_MIN_FRACTION && r > CARRIER_GUARD_BINS {
        let lower: f64 = psd[centre - r..=centre - CARRIER_GUARD_BINS - 1]
            .iter()
            .sum();
        let upper: f64 = psd[centre + CARRIER_GUARD_BINS + 1..=centre + r]
            .iter()
            .sum();
        if lower + upper > 0.0 {
            f.set("symmetry", (lower - upper) / (lower + upper));
        }
    }
    let median = median_of(band);
    let peak = band.iter().copied().fold(0.0_f64, f64::max);
    if median > 0.0 {
        f.set("carrier_line_db", 10.0 * (peak / median).log10());
    }
    if !spectrum.sk.is_empty() && bins == spectrum.sk.len() {
        let sk: Vec<f64> = spectrum.sk[lo..=hi].iter().map(|v| f64::from(*v)).collect();
        if !sk.is_empty() {
            f.set("sk_mean", sk.iter().sum::<f64>() / sk.len() as f64);
        }
    }
}

/// C14 evidence: the strongest cyclic line, OBW/Rs and C14's own family scores.
///
/// # `cyclic_db` and the four `blind_*` scores are OBSERVATION STATISTICS (T-281, re-measured T-310, T-328)
///
/// **They move with how long C14 was allowed to look, not only with what was transmitting**, and
/// unlike `sigma_ap`/`sigma_dp` below that was not previously recorded anywhere. Both are
/// nonetheless fitted density dimensions in **21 of 21** classes of the shipped models, so the
/// classifier compares two snippets on them today. The dependence is real. **The law T-281 gave
/// for it is not**, and the difference decides what can be done about it.
///
/// `cyclic_db` is the **largest of four** whitened line significances, each
/// `10·log10(peak / local median)` of a periodogram of a different feature series
/// (`hk_estimate::blind::lines::spectral_line`, over `LineMethod::ALL`: |x|², |d env|²,
/// delay-multiply, |d IF|²). T-281 reasoned that a coherent line's peak grows with the record
/// while the whitened noise median does not, so the ratio should grow about `10·log10(N)`, and
/// measured `ook` 20.17 → 31.44 dB and `bpsk` 15.32 → 25.08 dB over 8× of window on one seed at
/// 25 dB.
///
/// **Those two rows reproduce and are unrepresentative.** Re-measured over 8 dev seeds × 6 SNRs
/// (5–30 dB) × all 21 taxonomy and 11 held-out generators, truncating only the window handed to
/// C14, the growth is neither `10·log10(N)` nor a single law:
///
/// | | slope of `cyclic_db` vs `log10 N`, dB/decade |
/// |---|---|
/// | mean over everything, by SNR | 3.3 (5 dB) → 6.6 (30 dB), median 2.4 → 8.8 |
/// | `chirp` +27.1, `coded-pulse` +26.0, `nbfm` +19.2, `ook` +12.4 | far above 10 |
/// | `4fsk` −19.8, `2fsk` −15.5, `gfsk` −13.5, `msk` −13.0, `wfm` −7.3, `ppm` −7.3 | **negative** |
///
/// **6 of 21 taxonomy classes read a *lower* `cyclic_db` the longer they are watched**, at every
/// SNR. Coherent integration cannot do that, so it is not what is happening.
///
/// ## What is actually happening: the argmax moves, and C14's geometry moves with it
///
/// Two mechanisms, both measured (T-310):
///
/// 1. **`cyclic_db` is a max over four heterogeneous series, and which one wins changes with the
///    window.** At 25 dB the winning `LineMethod` differs across N/8…N for 8 of 8 seeds on `2fsk`,
///    `4fsk`, `chirp`, `cw`, `msk`, `ofdm` and `ppm`, and 7 of 8 on `8psk`, `am`, `ask4`,
///    `noise-like`, `ook` and `pulse`. Two readings of one emitter are then frequently not the
///    same measurement at all. The four series do not share a growth law either (per-method mean
///    slopes 4.0–6.0 dB/decade with **sd 6.2–13.8**).
/// 2. **C14's search band and whitening both scaled as `fs/n`.** The rate search started at
///    `f_min = max(rate_min_cells·fs/n, obw·rate_min_obw)`, so a shorter window searched from a
///    *higher* frequency (`am` 212.1 → 26.5 Hz, `nbfm` 377.3 → 129.1, most keyed classes
///    1964.6 → 245.5 between N/8 and N), and the whitening block was 24 *native* bins, i.e.
///    `24·fs/n` Hz wide.
///
/// This is the same class of defect as the resolution note on [`spectral_features`] (T-281's
/// seventh finding, T-312): **the analysis geometry is a function of the record**. It lives in
/// C14, not here, and **T-327 fixed it** — the band is now `[OBW99/50, min(1.2·OBW99, fs/2.5)]`
/// and the whitening block `OBW99/8`, neither a function of `n`.
///
/// ## What T-327 found when it pinned them: the geometry was not the mechanism
///
/// Measured with one instrument across both geometries (21 classes × 4 dev seeds × N/8…N at 25 dB,
/// C14's window truncated and nothing else changed):
///
/// | | before T-327 | after T-327 |
/// |---|---|---|
/// | mean `\|cyclic_db(N) − cyclic_db(N/8)\|` | 10.16 dB | 9.40 dB |
/// | sd of `cyclic_db` across N/8…N | 4.78 dB | 4.47 dB |
/// | `F` between/within class, fixed length | 89.6 | 92.1 |
/// | `F` between/within class, lengths pooled | 48.5 | 54.6 |
/// | winning `LineMethod` changes across N/8…N | 59 of 84 | 58 of 84 |
/// | `rate_range_hz` lower edge differs N/8 vs N | 51 of 84 | **0 of 84** |
///
/// The reported search band is now exactly length-free and pooling window lengths costs about an
/// eighth less class separation than it did. **But `cyclic_db` is still strongly window-dependent**
/// — 9.40 dB of mean movement against 10.16 — and `2fsk` (−9.2 dB), `gfsk` (−8.6), `msk` (−8.8) and
/// `4fsk` (−13.7) still read *lower* the longer they are watched. `wfm` and `ppm` crossed to
/// positive, so the negative set is 4 of 21 rather than 6. **The geometry was a real defect and not
/// the mechanism.**
///
/// T-310 attributed those negative slopes to the moving floor — "at N/8 the true symbol-rate line
/// is below the floor and only its harmonic is findable". **That explanation does not survive
/// measurement.** On the same grid the 2-FSK lines sit at 27–54 kHz while the floor they were
/// blamed on is 2.5 kHz, twenty times below both, so the floor never excluded them; and the short
/// window's winner is at *half* the long window's (the 0.49×/0.50×/0.57× T-310 recorded), which is
/// a **sub**harmonic below, not a harmonic above. Pinning the floor moves those four classes by
/// about a decibel.
///
/// What survives is T-310's own **first** finding: the argmax over four heterogeneous series moves
/// with the window, and it still does at essentially the old rate (58 of 84 against 59 of 84) with
/// the geometry pinned. `cyclic_db` is a max over four series with no shared growth law, so two
/// readings of one emitter are frequently not the same measurement for that reason alone. Whatever
/// fixes this dimension has to address the max — T-310's four-dimension expansion, or a rule for
/// choosing among the four — and not the geometry, which is now done.
///
/// ## Every length-free form of this statistic was measured, and each loses discrimination
///
/// Separation is reported two ways over the dev grid: `F` = between-class over within-class
/// variance, and the median **pairwise `d'`** across the 210 taxonomy class pairs computed the way
/// [`crate::density`] scores — a diagonal Gaussian per class. "Mixed" pools N, N/2, N/4 and N/8,
/// which is the burst case this dimension is charged with getting wrong.
///
/// | statistic | F fixed | F mixed | sd across N, dB |
/// |---|---|---|---|
/// | `cyclic_db` as shipped | **2.023** | **1.226** | 3.74 |
/// | − `10·log10 N` (T-281's law) | 1.782 | 1.046 | 3.79 |
/// | − `k·log10 N`, best `k` ≈ 4 | 1.942 | 1.227 | 3.45 |
/// | − `10·log10(ln M)`, M = bins searched (the null's own growth) | 1.992 | 1.211 | 3.66 |
/// | median / mean / min / second of the four | 2.126 / 2.167 / 1.521 / 2.189 | 1.425 / 1.305 / 0.609 / 1.415 | 2.83 / 2.64 / 1.99 / 3.16 |
///
/// **Subtracting the law T-281 proposed makes this dimension worse on every measure**, because the
/// law is wrong: a correction fitted to the middle of a −19.8…+27.1 dB/decade spread is applied
/// with the wrong sign to a quarter of the taxonomy. The best scalar exponent buys 8 % of the
/// movement (3.74 → 3.45 dB) for a fitted constant with no physical value, and costs fixed-length
/// separation. Nothing here is a fix, so **nothing is rescaled** and `features@4` is unchanged.
///
/// ## The four-dimension expansion: measured again with the geometry pinned, and refused again
///
/// The max-over-four collapse throws away most of this statistic's *separability*. Carrying the
/// four line significances as four dimensions instead, measured over the dev grid at ≥ 20 dB as a
/// diagonal Gaussian per class — the way [`crate::density`] scores — gives median pairwise `d'`
/// over the 210 taxonomy class pairs **7.03 against 2.81** at fixed length and **4.25 against
/// 1.87** with N, N/2, N/4 and N/8 pooled; the share of pairs at `d' ≥ 2` goes 0.60 → 0.93 and
/// 0.45 → 0.87; the median RMS clamped z of a held-out generator to its nearest taxonomy class goes
/// 0.22 → 0.72. (T-310 reported 3.46 → 7.77, 1.65 → 4.22 and 0.21 → 1.07 for the same quantities
/// before T-327 pinned the geometry. The win is the same size; the baseline moved.)
///
/// **T-310 refused it, T-327 removed the confound, and T-328 refused it again**, on two measured
/// grounds. Both were taken end-to-end through the real classifier — fitting both density models
/// on full windows over dev seeds, scoring disjoint dev seeds — rather than from the `d'`
/// instrument, and neither is a gate number:
///
/// 1. **It rejects genuine short bursts from their own class, worse than before.** Scoring a class's
///    own N/8 bursts against its full-window fit — what happens to every burst shorter than
///    [`crate::symbols::MAX_WINDOW_SAMPLES`] — the cyclic dimensions' mean clamped `Σz²` goes
///    **11.52 → 36.21**, the median plausibility of the burst under its **own** class collapses
///    **1.000 → 0.053**, `m/m_p95` goes 0.89 → 1.28, the second-largest |z| exceeds the class's own
///    `z2_p99` on 22.6 % → 37.3 % of bursts, and the classifier calls a genuine burst `unknown` on
///    **39.4 % → 56.4 %** of them (right family 60.5 % → 41.8 %, right class 39.8 % → 24.5 %,
///    n = 840). That is T-248's signature doing exactly what it is for: a genuine member has one
///    wild dimension, a non-member has two, and a short burst puts a median of **three of the four**
///    line significances more than 2 sd from their full-window mean at once
///    (`tests/cyclic_line_window.rs`). CLAUDE.md makes ephemeral emissions first-class, so a
///    discrimination win that rejects them is not a win.
/// 2. **The separation win does not reach the classifier anyway.** On the same run, full windows:
///    `unknown` 11.9 % → 11.3 %, right family 88.0 % → 88.6 %, right class 56.7 % → 56.7 % (476 of
///    840 either way). The `d'` instrument measures these dimensions in isolation; the other ~26
///    dimensions of `features@4` already supply that separation, so quadrupling this one buys
///    nothing where it would have to pay for itself.
///
/// So the max stays. The winning `LineMethod` still changes across N/8…N on **673 of 1008** class ×
/// seed × SNR cells (T-327's 58 of 84 on its own grid), and that remains the open mechanism — but
/// four dimensions is measured not to be its fix. A **rule** for choosing among the four, which
/// keeps one dimension, is the direction left; it is untried.
///
/// ## What T-328 did find: the harm is the fitting protocol, not the dimension count
///
/// The shipped single dimension **already** rejects genuine short bursts — 39.4 % of N/8 bursts of
/// a taxonomy class come back `unknown`, against 11.9 % of the same class's full windows. Four
/// dimensions make that worse; they did not cause it. What causes it is that every density is
/// fitted on full windows and then asked about bursts.
///
/// Fitting the same models over **pooled** window lengths (N, N/2, N/4, N/8) removes it almost
/// entirely, for both forms: N/8 `unknown` 39.4 % → **12.1 %** with the max (right family 60.5 % →
/// 87.7 %) and 56.4 % → **11.9 %** with four dimensions (41.8 % → 88.0 %), at a cost of about half
/// a point of full-window class accuracy (56.7 % → 56.1 %).
///
/// **It is not free, and that is why it is not done here.** Pooling widens every class, which is
/// the failure mode `bin/fit-densities.rs` documents: held-out unknown recall falls **0.9606 →
/// 0.9000** with the max and 0.9697 → 0.9242 with four dimensions, and on N/8 negatives to 0.8788
/// and 0.8727 — at or through ADR-0016 §7's 0.90 floor. Buying burst recall with open-set recall is
/// a product decision about what the classifier is *for*, not a refit, and it needs its own task
/// and its own gate run. Note that at a pooled fit the ordering reverses — four dimensions then
/// hold the open set better than the max (0.9242 against 0.9000) at the same burst recall — so the
/// expansion is refused **at this fitting protocol**, not on principle.
///
/// The `blind_*` family scores move with the window too, and discontinuously — reproduced across
/// seeds at 25 dB, a 2-FSK burst scores `blind_fsk` 0.10 at N/8, 0.44 at N/4 and 1.00 at N/2 and
/// N — because the line C14 locks on changes once the record is long enough. That is T-311.
///
/// **What this costs today:** [`crate::symbols::MAX_WINDOW_SAMPLES`] caps the window at 65 536
/// samples, so a *continuous* emission is always measured at the cap and is self-consistent. The
/// dependence bites on **bursts shorter than the cap** — the ephemeral emissions CLAUDE.md makes
/// first-class — where one emitter seen as a short burst and again as a long one lands at a
/// different `cyclic_db`, and so at a different Mahalanobis distance from the same class. T-327
/// stopped C14's search band and whitening from scaling with the record, which removes about 7 % of
/// that movement and all of the reported-band defect; the rest is the max-over-four collapse above.
/// **T-328 put a number on the cost: 39.4 % of genuine N/8 bursts of a taxonomy class come back
/// `unknown`, against 11.9 % of that class's full windows** — and measured that pooling window
/// lengths at fit time, not changing this statistic, is what addresses it.
/// `cyclic_line_window.rs` pins the structural findings — that the band **does not** move, and that
/// a short burst puts three of the four line significances off at once — so neither the wrong law,
/// nor the fixed defect, nor the refused expansion can be re-derived from a single seed.
fn symbol_features(f: &mut Features, input: &FeatureInput<'_>) {
    let Some(s) = input.symbols else {
        return;
    };
    let best = s
        .lines
        .iter()
        .map(|l| l.significance_db)
        .fold(f64::NEG_INFINITY, f64::max);
    if best.is_finite() {
        f.set("cyclic_db", best);
    }
    if let (Some(obw), Some(rate)) = (input.obw_hz, s.symbol_rate_bd.value()) {
        if rate > 0.0 {
            f.set("obw_over_rs", obw / rate);
        }
    }
    f.set("blind_ook", s.family_scores.ook);
    f.set("blind_fsk", s.family_scores.fsk);
    f.set("blind_bpsk", s.family_scores.bpsk);
    f.set("blind_qpsk", s.family_scores.qpsk);
}

/// Prominence of the strongest autocorrelation **peak** at a plausible OFDM symbol length.
///
/// A cyclic prefix repeats the last `N_cp` samples of each symbol exactly `N_fft` samples earlier,
/// which puts a *local* peak at that one lag. The raw correlation is not enough on its own: any
/// smoothly modulated signal — broadcast FM, say — is strongly correlated at short lags and decays
/// gradually, which would read as a cyclic prefix. Comparing each lag with its neighbours keeps
/// the sharp peak and drops the smooth decay.
fn cyclic_prefix_correlation(x: &[Complex64]) -> f64 {
    const LAGS: [usize; 11] = [32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024];
    let r: Vec<Option<f64>> = LAGS
        .iter()
        .map(|&lag| {
            if x.len() < 4 * lag {
                return None;
            }
            let acc: Complex64 = x[lag..]
                .iter()
                .zip(x)
                .map(|(a, b)| *a * b.conj())
                .sum::<Complex64>();
            let norm: f64 = x[lag..].iter().map(|s| s.norm_sqr()).sum::<f64>().sqrt()
                * x[..x.len() - lag]
                    .iter()
                    .map(|s| s.norm_sqr())
                    .sum::<f64>()
                    .sqrt();
            (norm > 0.0).then(|| acc.norm() / norm)
        })
        .collect();
    // Interior lags only: a peak needs a neighbour on each side to stand above.
    (1..r.len().saturating_sub(1))
        .filter_map(|i| match (r[i - 1], r[i], r[i + 1]) {
            (Some(before), Some(here), Some(after)) => Some(here - before.max(after)),
            _ => None,
        })
        .fold(0.0_f64, f64::max)
        .max(0.0)
}

/// Welch PSD of a real sequence (used for γ_max), or `None` when it is too short.
fn psd_of_real(v: &[f64]) -> Option<Vec<f64>> {
    let fft_len = (v.len() / 8)
        .next_power_of_two()
        .clamp(64, 1024)
        .min(v.len());
    let samples: Vec<Complex32> = v.iter().map(|x| Complex32::new(*x as f32, 0.0)).collect();
    let cfg = WelchConfig {
        fft_len,
        overlap: fft_len / 2,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let s = welch(&samples, 1.0, 0.0, &cfg).ok()?;
    Some(s.psd.iter().map(|x| f64::from(*x)).collect())
}

/// The bin range holding `fraction` of the **noise-subtracted** power, grown outwards from the
/// strongest bin.
///
/// Subtracting the noise floor first is not a refinement, it is the whole measurement: 99 % of the
/// *raw* power includes the noise spread across the analysis band, so a narrow signal in a wide
/// snippet would report a band as wide as the snippet (a 6 kHz AM carrier at 25 dB in-band SNR
/// carries a third of the total power in noise). hk-estimate makes the same correction for OBW99,
/// and keeps the subtracted PSD **unclipped** so the noise beyond the signal averages to zero
/// instead of adding its positive half to the tails (`hk_estimate` module docs, S5 deviation (a)).
///
/// The floor is the **median** bin. Welch averages many segments, so each noise bin is gamma
/// distributed with the segment count as its shape, and its median sits within a couple of per
/// cent of its mean — while a quantile-plus-exponential-correction (right for a single
/// periodogram) overestimates the floor several-fold once the segments are averaged, subtracts
/// more than the noise, and leaves a negative total.
pub fn occupied_band(psd: &[f64]) -> (usize, usize) {
    let full = (0, psd.len().saturating_sub(1));
    if psd.len() < 8 {
        return full;
    }
    let mut sorted = psd.to_vec();
    sorted.sort_by(f64::total_cmp);
    let floor = sorted[sorted.len() / 2];
    let net: Vec<f64> = psd.iter().map(|v| v - floor).collect();
    let total: f64 = net.iter().sum();
    if !(total.is_finite() && total > 0.0) {
        return full;
    }
    let target = 0.99 * total;
    let peak = net
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(psd.len() / 2, |(i, _)| i);
    let (mut lo, mut hi, mut acc) = (peak, peak, net[peak]);
    while acc < target && (lo > 0 || hi + 1 < psd.len()) {
        let take_low = lo > 0 && (hi + 1 >= psd.len() || net[lo - 1] >= net[hi + 1]);
        if take_low {
            lo -= 1;
            acc += net[lo];
        } else {
            hi += 1;
            acc += net[hi];
        }
    }
    // A carrier legitimately occupies one bin: a narrow answer is a real answer, not a failure.
    (lo, hi)
}

/// Centred moving average over `win` samples.
fn smooth(v: &[f64], win: usize) -> Vec<f64> {
    if win < 2 || v.len() < win {
        return v.to_vec();
    }
    let half = win / 2;
    (0..v.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half).min(v.len() - 1);
            v[lo..=hi].iter().sum::<f64>() / (hi - lo + 1) as f64
        })
        .collect()
}

fn median_of(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}

fn std_dev(v: &[f64]) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt()
}

fn least_squares(x: &[f64], y: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    if n < 2.0 {
        return (0.0, 0.0);
    }
    let mx = x.iter().sum::<f64>() / n;
    let my = y.iter().sum::<f64>() / n;
    let sxy: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let sxx: f64 = x.iter().map(|a| (a - mx).powi(2)).sum();
    if sxx <= 0.0 {
        return (0.0, my);
    }
    let slope = sxy / sxx;
    (slope, my - slope * mx)
}

/// Sarle's bimodality coefficient `(skew² + 1) / kurtosis`: > 0.555 for a uniform/bimodal sample,
/// ≈ 0.33 for a Gaussian one. Two well-separated FSK tones push it towards 1.
fn bimodality(v: &[f64]) -> f64 {
    let n = v.len() as f64;
    if n < 4.0 {
        return 0.0;
    }
    let mean = v.iter().sum::<f64>() / n;
    let m2 = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    if m2 <= 0.0 {
        return 0.0;
    }
    let m3 = v.iter().map(|x| (x - mean).powi(3)).sum::<f64>() / n;
    let m4 = v.iter().map(|x| (x - mean).powi(4)).sum::<f64>() / n;
    let skew = m3 / m2.powf(1.5);
    let kurt = m4 / (m2 * m2);
    if kurt <= 0.0 {
        return 0.0;
    }
    (skew * skew + 1.0) / kurt
}

/// Modes of a 48-bin histogram of the instantaneous frequency over the 2nd–98th percentile,
/// counted by **prominence**: a local maximum is a mode when it stands at least 25 % of the
/// tallest peak above the deepest valley separating it from a taller peak. 2-FSK gives 2, 4-FSK
/// gives 4, a linear modulation or an FM carrier 1.
///
/// Prominence, not a level threshold, is what makes this work: the arcsine-shaped histogram of a
/// tone-modulated FM carrier has two humps at its excursion limits that a level rule counts as
/// discrete tones, while the shallow valley between them fails the prominence test.
fn modality(v: &[f64]) -> usize {
    const BINS: usize = 48;
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let lo = s[(s.len() as f64 * 0.02) as usize];
    let hi = s[((s.len() as f64 * 0.98) as usize).min(s.len() - 1)];
    if hi <= lo {
        return 1;
    }
    let mut hist = [0.0_f64; BINS];
    for x in v {
        let b = (((x - lo) / (hi - lo)) * (BINS as f64 - 1.0)).round();
        if b.is_finite() && (0.0..BINS as f64).contains(&b) {
            hist[b as usize] += 1.0;
        }
    }
    // 3-bin smoother: a histogram of a noisy estimate is jagged, and every jag would be a mode.
    let h: Vec<f64> = (0..BINS)
        .map(|i| {
            let a = i.saturating_sub(1);
            let b = (i + 1).min(BINS - 1);
            hist[a..=b].iter().sum::<f64>() / (b - a + 1) as f64
        })
        .collect();
    let peak = h.iter().copied().fold(0.0_f64, f64::max);
    if peak <= 0.0 {
        return 1;
    }
    let min_prominence = 0.25 * peak;
    // Local maxima, tallest first; a peak counts when the valley between it and every already
    // counted (taller) peak is at least `min_prominence` below it.
    let mut maxima: Vec<usize> = (0..BINS)
        .filter(|&i| {
            let left = if i == 0 { 0.0 } else { h[i - 1] };
            let right = if i + 1 >= BINS { 0.0 } else { h[i + 1] };
            h[i] >= left && h[i] > right && h[i] > 0.1 * peak
        })
        .collect();
    maxima.sort_by(|a, b| h[*b].total_cmp(&h[*a]));
    let mut kept: Vec<usize> = Vec::new();
    for m in maxima {
        let prominent = kept.iter().all(|&k| {
            let (a, b) = if k < m { (k, m) } else { (m, k) };
            let valley = h[a..=b].iter().copied().fold(f64::INFINITY, f64::min);
            h[m] - valley >= min_prominence
        });
        if prominent {
            kept.push(m);
        }
    }
    kept.len().max(1)
}

/// How linearly the instantaneous frequency ramps: the best median within-window R² over several
/// window lengths.
///
/// Several lengths are needed because the window has to sit **inside** one sweep: a window longer
/// than the chirp's period spans a sawtooth and fits no line at all, and the sweep rate is not
/// known before the signal is classified.
/// Level structure of the instantaneous frequency **about its local trend**: the median over
/// consecutive [`IF_LOCAL_WINDOW`]-sample windows of Sarle's bimodality coefficient and of the
/// prominence mode count, each measured after that window's own best-fit straight line is removed.
///
/// `(bimodality, modality)`, or `None` when the sequence does not hold one whole window — an
/// abstention, as everywhere else in this module, rather than a value invented from a part-window.
///
/// The median across windows, not the mean, for the reason [`window_r2`] takes one: a snippet may
/// contain a gap, a retune or an interferer, and one ruined window must not decide the feature.
fn local_level_structure(fi: &[f64]) -> Option<(f64, f64)> {
    let mut bimodal = Vec::new();
    let mut modes = Vec::new();
    for chunk in fi.chunks(IF_LOCAL_WINDOW) {
        if chunk.len() < IF_LOCAL_WINDOW {
            break;
        }
        let d = detrend(chunk);
        bimodal.push(bimodality(&d));
        modes.push(modality(&d) as f64);
    }
    (!bimodal.is_empty()).then(|| (median_of(&bimodal), median_of(&modes)))
}

/// `v` with its own best-fit straight line removed.
fn detrend(v: &[f64]) -> Vec<f64> {
    let t: Vec<f64> = (0..v.len()).map(|i| i as f64).collect();
    let (slope, intercept) = least_squares(&t, v);
    v.iter()
        .enumerate()
        .map(|(i, y)| y - (slope * i as f64 + intercept))
        .collect()
}

fn ramp_linearity(fi: &[f64]) -> f64 {
    [128usize, 256, 512]
        .into_iter()
        .filter_map(|w| window_r2(fi, w))
        .fold(0.0, f64::max)
}

/// Median R² of a straight-line fit over consecutive `win`-sample windows; `None` when the
/// sequence does not hold at least two of them.
fn window_r2(fi: &[f64], win: usize) -> Option<f64> {
    if fi.len() < 2 * win {
        return None;
    }
    let mut r2s = Vec::new();
    for chunk in fi.chunks(win) {
        if chunk.len() < win {
            break;
        }
        let t: Vec<f64> = (0..chunk.len()).map(|i| i as f64).collect();
        let (slope, intercept) = least_squares(&t, chunk);
        let mean = chunk.iter().sum::<f64>() / chunk.len() as f64;
        let ss_tot: f64 = chunk.iter().map(|y| (y - mean).powi(2)).sum();
        let ss_res: f64 = chunk
            .iter()
            .zip(&t)
            .map(|(y, x)| (y - (slope * x + intercept)).powi(2))
            .sum();
        if ss_tot > 0.0 {
            r2s.push((1.0 - ss_res / ss_tot).clamp(0.0, 1.0));
        }
    }
    (!r2s.is_empty()).then(|| median_of(&r2s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Class, SynthConfig, generate};

    fn of(class: Class, snr_db: f64, seed: u64) -> Features {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        features(&FeatureInput {
            samples: &s.samples,
            sample_rate_hz: s.sample_rate_hz,
            obw_hz: Some(s.obw_hz),
            snr_db: Some(snr_db),
            symbols: None,
        })
    }

    #[test]
    fn a_short_or_empty_snippet_abstains_rather_than_guessing() {
        let f = features(&FeatureInput {
            samples: &[Complex32::new(1.0, 0.0); 16],
            sample_rate_hz: 1e6,
            obw_hz: None,
            snr_db: None,
            symbols: None,
        });
        assert_eq!(f.present(), 0);
        assert_eq!(f.reasons, vec!["too_short".to_owned()]);
    }

    /// The textbook cumulant values (BPSK Ĉ40 = −2, QPSK |Ĉ40| = 1, 8PSK ≈ 0; C15 card) are for
    /// **symbol-rate** samples. On the oversampled snippet this stage actually receives, the
    /// instants between symbols are pulse-shaped mixtures of neighbours, which are close to
    /// Gaussian and drive the fourth-order cumulants towards zero: measured, QPSK's |Ĉ40| is 0.04
    /// rather than 1. What survives oversampling is the **second**-order structure, because a real
    /// constellation keeps `E[x²] ≠ 0` however it is filtered.
    ///
    /// So the feature tree separates the psk-qam *family* on Ĉ20 and shape, and leaves the order
    /// within it to a stage that has a symbol clock: the class call runs only above its own gate
    /// (`thresholds@1` puts it at +5 dB), and T-200's post-sync verifier is where an order is
    /// properly decided. This test pins the separation the features really provide.
    #[test]
    fn second_order_cumulants_separate_real_and_rotationally_symmetric_constellations() {
        // A residual carrier or a real-valued constellation keeps |Ĉ20| high...
        let cw = of(Class::Cw, 30.0, 3).get("c20_norm").unwrap();
        let bpsk = of(Class::Bpsk, 30.0, 7).get("c20_norm").unwrap();
        assert!(cw > 0.6, "CW |C20| {cw}");
        assert!(bpsk > 0.3, "BPSK |C20| {bpsk}");
        // ...while a rotationally symmetric one averages it away.
        for class in [Class::Qpsk, Class::Psk8, Class::Qam16] {
            let c20 = of(class, 30.0, 7).get("c20_norm").unwrap();
            assert!(c20 < 0.2, "{} |C20| {c20}", class.label());
            assert!(c20 < bpsk, "{} must sit below BPSK", class.label());
        }
        // The fourth-order cumulants are computed and stored, but oversampling flattens them: the
        // classifier must not be given a threshold that assumes the textbook values.
        let qpsk_c40 = of(Class::Qpsk, 30.0, 7).get("c40_norm").unwrap();
        assert!(
            qpsk_c40 < 0.5,
            "oversampled QPSK |C40| is small: {qpsk_c40}"
        );
    }

    #[test]
    fn instantaneous_frequency_separates_fsk_chirps_and_linear_modulations() {
        assert!(
            of(Class::Fsk2, 30.0, 11).get("if_bimodality").unwrap() > 0.6,
            "2-FSK is bimodal"
        );
        assert!(
            of(Class::Bpsk, 30.0, 11).get("if_bimodality").unwrap() < 0.6,
            "BPSK is not"
        );
        assert!(
            of(Class::Chirp, 30.0, 11).get("if_slope_r2").unwrap() > 0.8,
            "a chirp ramps linearly"
        );
        assert!(of(Class::Fsk2, 30.0, 11).get("if_slope_r2").unwrap() < 0.6);
    }

    /// T-298: the level structure of the instantaneous frequency measured about its **local** trend
    /// separates a swept or a multi-tone FSK carrier from a wideband angle modulation, where the
    /// whole-record statistics beside it do not.
    ///
    /// Averaged over dev seeds rather than asserted on one, because these are distributional
    /// statements about a generator and a single seed would pin noise.
    #[test]
    fn local_level_structure_separates_swept_and_multi_tone_fsk_from_wideband_fm() {
        let mean = |class: Class, name: &str| {
            let v: Vec<f64> = (0..8u64)
                .filter_map(|seed| of(class, 25.0, seed).get(name))
                .collect();
            assert!(!v.is_empty(), "{} has no {name}", class.label());
            v.iter().sum::<f64>() / v.len() as f64
        };
        // The whole-record statistic cannot see the chirped carrier's two tones at all: the levels
        // themselves move, so it reads a single mode — fewer than wideband FM shows.
        let global = mean(Class::ChirpedFsk, "if_modality");
        assert!(
            global < 1.5,
            "chirped-FSK reads {global} modes over the record"
        );
        // About the local trend they are there, and more sharply two-valued than `wfm` ever is.
        let chirped = mean(Class::ChirpedFsk, "if_local_bimodality");
        let wfm = mean(Class::Wfm, "if_local_bimodality");
        assert!(chirped > wfm + 0.1, "chirped-FSK {chirped} vs wfm {wfm}");
        // Sarle's coefficient is a *two*-mode statistic, so it does not separate 8-FSK; the mode
        // count does. That is why both dimensions are carried rather than either alone.
        let eight = mean(Class::Fsk8, "if_local_modality");
        let wfm_modes = mean(Class::Wfm, "if_local_modality");
        assert!(
            eight > wfm_modes + 1.0,
            "8-FSK {eight} modes vs wfm {wfm_modes}"
        );
    }

    #[test]
    fn envelope_and_spectral_features_separate_ook_ofdm_and_noise() {
        assert!(of(Class::Ook, 25.0, 5).get("low_fraction").unwrap() > 0.2);
        assert!(of(Class::Fsk2, 25.0, 5).get("low_fraction").unwrap() < 0.1);
        assert!(
            of(Class::Ofdm, 25.0, 5).get("cp_corr").unwrap() > 0.15,
            "the cyclic prefix repeats"
        );
        assert!(of(Class::Bpsk, 25.0, 5).get("cp_corr").unwrap() < 0.15);
        let noise = of(Class::NoiseLike, 25.0, 5);
        assert!(noise.get("flatness").unwrap() > 0.5, "noise is flat");
        assert!(of(Class::Cw, 25.0, 5).get("carrier_line_db").unwrap() > 20.0);
    }
}
