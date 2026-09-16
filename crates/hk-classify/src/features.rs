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
pub const FEATURES_VERSION: u32 = 3;

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
