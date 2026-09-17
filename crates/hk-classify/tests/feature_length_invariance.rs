//! **A feature named as a property of the signal must not move when the signal is watched for
//! less time** (T-313).
//!
//! # Why this test exists
//!
//! Seven features had been found measuring the *observation* rather than the emission, and five of
//! them by accident while chasing something else: `sigma_ap`/`sigma_dp` (T-240, phase residual
//! random-walks with the record), `burst_length` (T-250, grows with how often the emitter was
//! watched), `snr_mean_db` (T-280, a band average, so it falls as the emission widens),
//! `Track.bandwidth_hz` (T-288, a running mean over association history), `if_bimodality` (T-298,
//! levels smear over a long record), `cyclic_db` and the four `blind_*` scores (T-281/T-310/T-328,
//! +11 dB from watching the same burst eight times longer), and the analysis resolution itself
//! (T-312, the transform length was `n/8`).
//!
//! Every one was a real number that described the receiver or the capture window. T-281 built the
//! probe that would have caught them as a scratch tool and deleted it rather than commit a test
//! that asserted nothing. This is that probe with the judgement call made: **per-feature
//! tolerances, each derived from what the estimator can do at that record length, and an exemption
//! list by name.**
//!
//! # It found six more on its first run
//!
//! None of them by accident, which is the point. Each is recorded in [`OBSERVATION_STATISTICS`]
//! with the mechanism and the measurement, so that fixing one makes an exemption **disappear from
//! a diff**:
//!
//! | feature | mechanism |
//! |---|---|
//! | `c20_norm` | a coherent sum, so its loss is the residual carrier offset **times** the record |
//! | `c40_norm` | the same sum at four times the offset, losing coherence twice as fast |
//! | `c42_norm` | subtracts `|C20|²`, so it inherits the above algebraically — 93 % attributed |
//! | `cp_corr` | a maximum over lags, whose **null** level falls as `1/sqrt(record)` |
//! | `if_slope_r2` | any smooth process is better fitted by a line over a shorter window |
//! | `if_local_modality` | a mode **count** off a histogram, which invents and loses whole modes |
//! | `carrier_line_db` | a peak-over-median, biased in the segment count that T-312 left moving |
//!
//! The last two are the ones worth noticing. `if_local_modality` was added by T-298 **as the
//! length-free replacement** for `if_modality`, and inherited the defect through its estimator
//! instead of through its reference — the replacement for an observation statistic was one.
//! `carrier_line_db` is the residue T-312 knew it was leaving: pinning the transform fixes *what*
//! is measured and leaves the segment count moving, which only matters for the two features that
//! are order statistics rather than sums.
//!
//! # The two rules that make this a guard and not a rubber stamp
//!
//! 1. **No tolerance is fitted to current behaviour.** A tolerance taken from what the code
//!    currently produces passes today by construction and passes tomorrow after a regression. Every
//!    number below is the **standard error of the statistic itself** — the spread two honest
//!    measurements of one stationary emission are entitled to, given how many independent looks
//!    each had — times three, for a ~3σ one-sided allowance. [`Basis`] states the scale each is
//!    measured against and [`Looks`] states what the error falls with. A feature that moves more
//!    than its own estimator can account for is measuring something the emission did not do.
//! 2. **Observation statistics are exempt BY NAME.** [`OBSERVATION_STATISTICS`] is the audited list
//!    from T-281, and adding to it is an edit someone reads in a diff. That is the point: the
//!    failure mode this guards is a feature *quietly* drifting into observation-dependence, which
//!    is how all seven were introduced.
//!
//! # What is truncated
//!
//! One waveform per taxonomy class, generated long and then cut to N, N/2, N/4 and N/8. Truncation,
//! not regeneration: every prefix is a **prefix of the same samples**, so the emission, the noise
//! draw, the carrier offset and the gain are identical and the only thing that changed is how long
//! it was watched. That is the comparison the defect family is about — one emitter, seen twice.

use hk_classify::features::{FEATURE_NAMES, FeatureInput, MIN_SAMPLES, feature_fft_len, features};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{Class, DEV_SEEDS, SynthConfig, generate};

/// SNR the invariance is measured at: well above every family gate, so what moves is the record
/// length and not the noise.
const SNR_DB: f64 = 25.0;

/// Denominators of the truncation ladder: the full record, then halvings. Same ladder T-328 used.
///
/// The full record is whatever [`SynthConfig`]'s default produces — **the geometry the shipped
/// densities are fitted at**, 2328–16 384 normalised samples depending on class, rather than a
/// length chosen to make the ladder convenient. A rung shorter than [`MIN_SAMPLES`] is skipped
/// because `features` abstains wholesale there, and a rung where an individual feature abstains
/// (the spectral four under `FEATURE_MIN_SEGMENTS`) is simply not compared: an abstention is the
/// honest answer, not a failure, and this file never treats it as one.
const PREFIX_DIVISORS: &[usize] = &[2, 4, 8];

/// Samples per **independent look** at the modulation.
///
/// `hk_estimate::normalise` puts the classifier's snippets at ~2 samples per OBW99, so successive
/// samples are correlated over roughly one sample and a record of `n` samples carries about `n/2`
/// independent draws. 4 is that bound made conservative for the pulsed and keyed classes (`ook`,
/// `ppm`, `pulse`), which carry their information in roughly half their samples — the phase and
/// frequency features drop the rest explicitly, at the Azzouz–Nandi envelope threshold.
const SAMPLES_PER_LOOK: f64 = 4.0;

/// 3σ relative standard error of a **second**-order sample statistic over `n` independent Gaussian
/// draws. `Var(m₂)/m₂² = 2/n`, so 3σ is `3·sqrt(2/n)`; this is the constant.
const K_ORDER2: f64 = 4.243;

/// 3σ relative standard error of a **fourth**-order sample statistic. For a Gaussian,
/// `Var(m₄)/m₄² = ((2·4−1)!!/(3!!)² − 1)/n = 10.667/n`, so 3σ is `3·sqrt(10.667/n) = 9.80/sqrt(n)`.
/// Every cumulant and kurtosis feature is a fourth-order statistic and inherits this.
const K_ORDER4: f64 = 9.80;

/// 3σ standard error of a **proportion**: `sd = sqrt(p(1−p)/n) ≤ 0.5/sqrt(n)`, so `3·0.5`.
const K_PROPORTION: f64 = 1.5;

/// Bins a spectral shape feature averages over. [`hk_classify::features`] widens the occupied band
/// to at least `MIN_SHAPE_BINS = 16`, so 16 is the floor — using the floor rather than the actual
/// width keeps the tolerance from depending on the waveform.
const MIN_SHAPE_BINS: f64 = 16.0;

/// The features that are **observation statistics**, exempt by name.
///
/// Each was audited (T-281) and is *known* to describe the capture rather than the emission. They
/// are kept as density dimensions because they still separate classes measured the same way; what
/// the audit forbids is comparing two *rows* on them. Nothing may be added here without saying
/// which observation it is a statistic of.
const OBSERVATION_STATISTICS: &[(&str, &str)] = &[
    (
        "sigma_ap",
        "spread of the UNWRAPPED phase residual. An angle modulator integrates its baseband, so \
         the residual random-walks and its spread grows with the record: measured 12.1 -> 16.7 -> \
         33.0 -> 252.4 over 679/1357/2715/5430 samples of ONE wfm waveform (T-281). T-240 and \
         T-248 each implemented a fixed-window replacement, measured an open-set regression and \
         reverted it; features.rs carries both measurements. Not a bound that can be tightened - \
         the quantity itself is cumulative.",
    ),
    (
        "sigma_dp",
        "the same unwrapped-phase residual as sigma_ap, before the absolute value - and so the \
         same random walk with the observation, growing as sqrt(record length) rather than \
         settling on a value. Measured 50.9 -> 407.5 over N/8 -> N of one wfm waveform.",
    ),
    (
        "if_bimodality",
        "Sarle's coefficient over the histogram of the WHOLE record's instantaneous frequency. A \
         keyed carrier's levels move as the carrier drifts, so a longer record smears them \
         together and the statistic answers 'no levels' for a waveform that plainly has them \
         (T-298: held-out chirped-FSK reads if_modality 1.04 +/- 0.20 globally against exactly \
         2.00 +/- 0.00 detrended). The length-free form is `if_local_bimodality`, which is \
         asserted below - so this pair is exempt because its REPLACEMENT is guarded, not because \
         the defect was accepted.",
    ),
    (
        "if_modality",
        "mode count of the same whole-record instantaneous-frequency histogram, smearing for the \
         same reason: the levels are counted about the record's own mean, so a carrier that \
         drifts across a long record merges its own levels. `if_local_modality` is the \
         length-free form and is asserted below.",
    ),
    (
        "cyclic_db",
        "significance of C14's strongest cyclic line, in dB above its own whitened floor. Line \
         significance is an integration gain: it grows with how long the line was integrated, \
         reproducibly and by more than the spread it exists to resolve - ook 20.17 -> 24.53 -> \
         26.69 -> 31.44 dB and bpsk 15.32 -> 15.91 -> 23.72 -> 25.08 dB over N/8..N of one \
         waveform (T-281). T-327 removed the part that was a defect (a search band that scaled \
         with the record); the remaining +11 dB is the integration gain itself. T-328 measured \
         that the fix is pooling window lengths at FIT time, which costs open-set recall and is \
         its own task (T-311).",
    ),
    (
        "blind_ook",
        "C14's own family score. It moves DISCONTINUOUSLY with the window because the line C14 \
         locks on changes once the record is long enough: one 2-FSK burst reads blind_fsk 0.10 at \
         N/8, 0.44 at N/4 and 1.00 at N/2 and N (T-281, reproduced across seeds). T-311.",
    ),
    (
        "blind_fsk",
        "as blind_ook - C14's own family score, and the one the discontinuity was measured on: \
         0.10 at N/8, 0.44 at N/4, 1.00 at N/2 and N for a single 2-FSK burst, because the cyclic \
         line C14 locks on changes once the record is long enough. T-311.",
    ),
    (
        "blind_bpsk",
        "as blind_ook - a C14 family score, moving with the observation window because the line \
         it is computed from does. Not separable from cyclic_db's dependence: same estimate, \
         different projection of it. T-311.",
    ),
    (
        "blind_qpsk",
        "as blind_ook - a C14 family score, moving with the observation window because the line \
         it is computed from does. Not separable from cyclic_db's dependence: same estimate, \
         different projection of it. T-311.",
    ),
    (
        "c20_norm",
        "|C20|/C21, and C20 = mean(x^2) is a COHERENT SUM over the record. Derotation leaves a \
         residual carrier offset d; x^2 spins at 2d, so the sum's integration loss is set by the \
         PRODUCT of that residual and the record length, and the residual only falls as \
         1/sqrt(n) while the accumulated phase grows as n - so the loss grows as sqrt(n). FOUND \
         BY THIS TEST (T-313), not previously known. Measured on one `pulse` waveform, seed 0, \
         truncation only: 0.825 at N/8, 0.801 at N/4, 0.737 at N/2 and 0.079 over the full 16 384 \
         samples - the same emission, decaying towards zero purely by being watched longer, with \
         the derotation decision identical (coherence 0.999-1.000) at every rung. The fix is a \
         residual-offset estimate good enough that 2*d*n stays under a radian at the longest \
         record, or cumulants averaged over fixed-length blocks; either is new DSP and a refit, so \
         it is named here rather than smuggled into T-312.",
    ),
    (
        "c40_norm",
        "|C40|/C21^2, and C40 = mean(x^4) - 3*C20^2 is the same coherent sum at FOUR times the \
         residual offset, so it loses coherence twice as fast as c20_norm. FOUND BY THIS TEST \
         (T-313). Measured on `pulse` seed 1: 3.20 / 3.33 / 3.59 at N/8, N/4, N/2 and 16.73 over \
         the full record. features.rs already records that 'even a fraction of a cycle costs real \
         discrimination' for these cumulants; what was not recorded is that the fraction of a \
         cycle is proportional to the record, which makes the dimension a statistic of the \
         observation. Same fix as c20_norm, same task.",
    ),
    (
        "carrier_line_db",
        "10*log10(strongest bin / band median) over an M-averaged periodogram. BOTH ends are \
         order statistics - a maximum and a median over the bins the band happens to hold - so \
         unlike `symmetry`, which is a ratio of SUMS and unbiased in M, this one carries a \
         small-sample bias in the SEGMENT COUNT, and the segment count is a property of the \
         record. T-312 pinned the transform, which fixes the yardstick but leaves this: a 4072- \
         sample prefix of one 2-FSK waveform reads 32.05 dB where the full 16 290 samples read \
         14.13 dB, a 17.9 dB move on a dimension that separates carrier-present from \
         carrier-absent by about 20 dB. FOUND BY THIS TEST (T-313). The fix is either a \
         bias-corrected peak-over-floor (a CFAR statistic rather than a raw ratio) or a segment \
         floor high enough for the ratio to settle, and both need a refit and a section 7 run.",
    ),
    (
        "if_local_modality",
        "the count of instantaneous-frequency modes about the carrier's local trend. The QUANTITY \
         is a property of the emission - a 2-FSK keys two levels however long it is watched - but \
         the ESTIMATOR is a 48-bin histogram scored by prominence, and a histogram of a shorter \
         record has fewer counts per bin, so it invents and loses whole modes: measured on one \
         waveform per class with only the record truncated, `nbfm` reads 1 mode over 2328 samples \
         and 2 over the first 1164; `gfsk`, `msk` and `2fsk` read 2 over the full record and 4 \
         over a quarter of it; `ook` reads 1 and 3. FOUND BY THIS TEST (T-313), and it matters \
         more than the rest because T-298 added this pair as the LENGTH-FREE replacement for \
         `if_modality` - so the replacement inherited the defect in a new form, through its \
         estimator rather than through its reference. `if_local_bimodality`, the continuous half \
         of the same pair, is asserted and passes, so the fix is a mode count built like it \
         (a continuous statistic thresholded once) rather than a count read off a histogram.",
    ),
    (
        "c42_norm",
        "C42/C21^2, and C42 = mean(|x|^4) - |C20|^2 - 2*C21^2 SUBTRACTS the coherent sum that \
         c20_norm is, so it inherits that sum's decay with the record algebraically even though \
         its own leading term, mean(|x|^4), is offset-immune. FOUND BY THIS TEST (T-313), and \
         attributed rather than guessed: on one `ook` waveform truncated to N/2, c42_norm moves \
         -0.137 -> -0.911 while |C20/C21|^2 moves 0.187 -> 0.910, so the C20 term accounts for \
         0.723 of the 0.774 move - 93 % of it. All three normalised cumulants therefore share ONE \
         defect and recover together when it is fixed; this is not three problems.",
    ),
    (
        "cp_corr",
        "the largest cyclic-prefix autocorrelation peak. For the one class that HAS a cyclic \
         prefix this is a real detection; for the other twenty it is the maximum of a correlation \
         against noise over eleven candidate lags, and the null level of such a maximum falls as \
         1/sqrt(record length) - so a short burst of any class reads a cyclic prefix it does not \
         have. FOUND BY THIS TEST (T-313). Measured on one 2-FSK waveform, truncation only: 0.535 \
         at N/8, 0.440 at N/4 and 0.110 over the full 16 290 samples, a 4.9x move against the \
         1/sqrt(8) = 0.35 the null law predicts for an 8x length change. The fix is to report the \
         peak as a SIGNIFICANCE against its own null level rather than as a raw correlation, \
         which is a new statistic and a refit.",
    ),
    (
        "if_slope_r2",
        "R^2 of a straight-line fit to the smoothed instantaneous frequency over the WHOLE \
         record. A shorter window of any smooth process is better approximated by a straight \
         line, so this rises as the record shortens for reasons that have nothing to do with \
         whether the carrier is sweeping - which is what the dimension exists to say. FOUND BY \
         THIS TEST (T-313): one `wfm` waveform reads 0.440 over 5430 samples and 0.790 over the \
         first 678, i.e. a wideband FM carrier looks like a linear chirp once you stop watching \
         it. The length-free form is a fit over a FIXED window with the residual taken across \
         windows, which is what `if_local_bimodality` already does for the level statistic.",
    ),
    (
        "obw_over_rs",
        "OBW99 divided by C14's symbol-rate estimate. OBW and the true Rs are both properties of \
         the emission, but the DENOMINATOR is C14's estimate, which inherits the window \
         dependence of `cyclic_db` above - when the winning line changes, Rs jumps by an integer \
         factor. Exempt as a C14 statistic, not as a feature defect: the fix is T-311's, and this \
         entry is here so that fixing it makes an exemption disappear from a diff.",
    ),
];

/// What a tolerance is measured against.
#[derive(Clone, Copy)]
enum Basis {
    /// A fixed scale, in the feature's own units: the size of the difference the dimension exists
    /// to resolve. Stated per feature.
    Abs(f64),
    /// The feature's own full-record value. For features that are ratios, where "how much it
    /// moved" is only meaningful against how big it is.
    Rel,
    /// The feature's own full-record value, but never below the given floor.
    ///
    /// This is the right basis for a moment ratio, and the two halves come from different places.
    /// The **relative** half is the sampling law: `Var(m_p)/m_p^2` is a constant over n, so a
    /// fourth-order statistic's ABSOLUTE error is proportional to the statistic — a feature sitting
    /// at 17 is entitled to seventeen times the wander of one sitting at 1. The **floor** is the
    /// gap the dimension exists to resolve, so a feature passing through zero does not get a
    /// tolerance of zero with it.
    RelFloor(f64),
}

/// What the estimator's error falls with as the record grows.
#[derive(Clone, Copy)]
enum Looks {
    /// Independent draws of the modulation: `n / SAMPLES_PER_LOOK`.
    Samples,
    /// Welch segments — for features computed from the averaged periodogram, where more samples
    /// buy more segments and **not** finer bins (that is pinned; T-312).
    Segments,
    /// Segments times the bins the feature averages over: the spectral **shape** features, which
    /// are sums across the occupied band rather than a single bin.
    SegmentBins,
    /// Independent draws over the samples the instantaneous frequency is actually taken on.
    /// `frequency_features` keeps a PAIR only when **both** of its samples clear the Azzouz-Nandi
    /// envelope threshold, so the usable fraction is the on-fraction squared, not the on-fraction.
    SamplePairs,
    /// The spectral-kurtosis estimator, whose standard deviation at `M` segments is exactly
    /// `sqrt(4M²/((M−1)(M+2)(M+3)))` ([`hk_dsp::sk::std_dev`]) — so the tolerance is *computed*
    /// rather than assumed, and no constant is needed.
    SpectralKurtosis,
}

struct Rule {
    feature: &'static str,
    basis: Basis,
    looks: Looks,
    /// 3σ relative standard error constant of the underlying statistic; unused for
    /// [`Looks::SpectralKurtosis`], which computes its own.
    k: f64,
    /// The physical or statistical variation this admits. Never "what it currently measures".
    why: &'static str,
}

/// Every feature named as a property of the SIGNAL, with the variation its tolerance admits.
const RULES: &[Rule] = &[
    Rule {
        feature: "gamma_max",
        basis: Basis::Rel,
        looks: Looks::Segments,
        k: K_ORDER2,
        why: "peak-to-mean of the envelope spectrum. The mean is unbiased in the segment count; \
              the peak is a chi-square with 2M degrees of freedom, so its relative error falls as \
              1/sqrt(M). Relative because the feature spans 2 (noise-like) to 130 (msk) and 'how \
              far it moved' only means anything against its own size.",
    },
    Rule {
        feature: "sigma_aa",
        basis: Basis::Rel,
        looks: Looks::Samples,
        k: K_ORDER4,
        why: "standard deviation of the centred normalised amplitude - a per-sample statistic of \
              the envelope, so nothing but its own sampling error may move it. FOURTH-order \
              constant for the reason sigma_af carries one: the sampling error of a standard \
              deviation is set by the fourth moment of what is being spread, and a keyed \
              envelope's amplitude distribution is bimodal, not Gaussian - `cw` and `ook` sit at \
              a kurtosis far from 3, which is exactly where K_ORDER2 would be an under-estimate.",
    },
    Rule {
        feature: "sigma_af",
        basis: Basis::Rel,
        looks: Looks::SamplePairs,
        k: K_ORDER4,
        why: "standard deviation of the instantaneous frequency, rad/SAMPLE - a per-sample \
              quantity, unlike sigma_ap/sigma_dp which integrate. FOURTH-order constant despite \
              being a second-order statistic: the sampling error of a variance is \
              (kurtosis - 1 + 2/(n-1))/n, so it is set by the FOURTH moment of what is being \
              spread, and the instantaneous frequency is heavy-tailed - at a keying transition or \
              an envelope edge it is near-uniform on (-pi, pi], not Gaussian. K_ORDER2 would \
              assume a kurtosis of 3 for a quantity whose kurtosis is an order of magnitude \
              higher.",
    },
    Rule {
        feature: "mu42_a",
        basis: Basis::Rel,
        looks: Looks::Samples,
        k: K_ORDER4,
        why: "envelope kurtosis m4/m2^2: a fourth-order per-sample moment ratio of a stationary \
              envelope, so a shorter record changes only which draws it saw, never what they are \
              drawn from. Bounded below by 1, so the relative basis is well defined.",
    },
    Rule {
        feature: "env_cv",
        basis: Basis::Rel,
        looks: Looks::Samples,
        k: K_ORDER4,
        why: "envelope coefficient of variation sd(a)/mean(a): a second-order per-sample statistic \
              of a stationary envelope. Both numerator and denominator are sample means over the \
              same draws, so the ratio's error is second order in the same n.",
    },
    Rule {
        feature: "low_fraction",
        basis: Basis::Abs(1.0),
        looks: Looks::Samples,
        k: K_PROPORTION,
        why: "the fraction of samples whose envelope is under 0.3 of the mean - a proportion, so \
              its scale is 1 and its error is binomial. A keyed emission's gaps are part of the \
              emission, so what a shorter record may change is only which draws it saw.",
    },
    Rule {
        feature: "duty",
        basis: Basis::Abs(1.0),
        looks: Looks::Samples,
        k: K_PROPORTION,
        why: "fraction of samples above half the mean envelope: a proportion, binomial error. \
              NOTE this is the per-snippet envelope duty, not `Track.duty_cycle`, which IS an \
              observation statistic (T-288) and lives in hk-model.",
    },
    Rule {
        feature: "mu42",
        basis: Basis::Rel,
        looks: Looks::Samples,
        k: K_ORDER4,
        why: "sample kurtosis of the complex envelope: a fourth-order per-sample moment ratio, \
              bounded below by 1 so a relative basis is well defined.",
    },
    Rule {
        feature: "if_std_norm",
        basis: Basis::Rel,
        looks: Looks::SamplePairs,
        k: K_ORDER4,
        why: "sigma_af converted to Hz and divided by the OBW passed in, which is constant across \
              prefixes here - so this inherits sigma_af's law exactly.",
    },
    Rule {
        feature: "flatness",
        basis: Basis::Abs(1.0),
        looks: Looks::SegmentBins,
        k: K_ORDER2,
        why: "spectral flatness is bounded to [0, 1] and separates a tone (~0) from noise (~1), so \
              1.0 is its scale. It is a ratio of a geometric to an arithmetic mean over the \
              occupied band, so its error falls with segments TIMES bins. The known small-sample \
              bias of the geometric mean of an M-averaged periodogram, exp(psi(M) - ln M) ~ \
              1 - 1/(2M), is 0.17 at the M = 3 floor and sits well inside this - which is why \
              FEATURE_MIN_SEGMENTS exists rather than a bias correction here.",
    },
    Rule {
        feature: "symmetry",
        basis: Basis::Abs(1.0),
        looks: Looks::SegmentBins,
        k: K_ORDER2,
        why: "sideband balance about the carrier, bounded to [-1, +1] and separating one sideband \
              from two, so 1.0 is the scale. Both sums are linear in the periodogram and \
              therefore unbiased in the segment count; only their variance moves.",
    },
    Rule {
        feature: "sk_mean",
        basis: Basis::RelFloor(1.0),
        looks: Looks::SpectralKurtosis,
        k: 0.0,
        why: "mean spectral kurtosis over the occupied band. SK is 1 for Gaussian noise and its \
              estimator is unbiased in M by construction ((M+1)/(M-1) * (M*S2/S1^2 - 1)), so only \
              its variance moves - and that variance is KNOWN exactly, 4M^2/((M-1)(M+2)(M+3)), \
              reduced by averaging over the band's bins. The tolerance is therefore computed from \
              hk_dsp::sk::std_dev, with no constant chosen here at all.",
    },
    Rule {
        feature: "if_local_bimodality",
        basis: Basis::Abs(1.0),
        looks: Looks::Samples,
        k: K_ORDER4,
        why: "Sarle's coefficient about the carrier's LOCAL trend (T-298), bounded to [0, 1] and \
              separating keyed levels (0.8) from an angle modulation (0.4), so 1.0 is the scale. \
              It is the length-free replacement for `if_bimodality` and the reason that one may be \
              exempt, so it is asserted here rather than exempted. Built from third and fourth \
              moments, hence fourth order; the median across windows is what makes the record \
              length drop out.",
    },
];

/// Segments a record of `n` samples yields at the pinned transform and 50 % overlap.
fn segments(n: usize) -> f64 {
    let fft = feature_fft_len(n);
    if n < fft {
        return 0.0;
    }
    ((n - fft) / (fft / 2) + 1) as f64
}

impl Rule {
    /// The tolerance this rule allows a record of `n` samples, given the full record's value.
    fn tolerance(&self, n: usize, full_value: f64, duty: f64) -> f64 {
        let scale = match self.basis {
            Basis::Abs(s) => s,
            Basis::Rel => full_value.abs(),
            Basis::RelFloor(floor) => full_value.abs().max(floor),
        };
        match self.looks {
            Looks::SamplePairs => {
                scale * self.k / (n as f64 * duty * duty / SAMPLES_PER_LOOK).sqrt()
            }
            Looks::Samples => {
                // Only the samples where the emission is ON carry it, and every per-sample feature
                // here says so explicitly: the phase, frequency and cumulant features are taken
                // over the Azzouz-Nandi strong-envelope subset. A pulse train at 14 % duty gives a
                // seventh of the independent looks a continuous emission of the same length does,
                // so its statistics are entitled to sqrt(7) times the wander. `duty` is the
                // feature's own full-record value - a signal property, asserted below in its own
                // right - not a number chosen to make this pass.
                scale * self.k / (n as f64 * duty / SAMPLES_PER_LOOK).sqrt()
            }
            Looks::Segments => scale * self.k / segments(n).max(1.0).sqrt(),
            Looks::SegmentBins => scale * self.k / (segments(n).max(1.0) * MIN_SHAPE_BINS).sqrt(),
            Looks::SpectralKurtosis => {
                let m = segments(n).max(2.0) as u32;
                // Two named corrections on top of the Gaussian-noise formula, both from what the
                // band actually contains. (1) `hk_dsp::sk::std_dev` is the spread for a bin whose
                // SK is 1 (Gaussian noise); the estimator is a ratio S2/S1^2, so on a bin whose SK
                // is k its spread scales with k - and a modulated band's SK is 1.4-2.8, not 1.
                // (2) Welch's 50 % overlap makes adjacent bins share half their data, so B bins
                // average as B/2 independent ones.
                scale.max(1.0) * 3.0 * hk_dsp::sk::std_dev(m) / (MIN_SHAPE_BINS / 2.0).sqrt()
            }
        }
    }
}

/// Every feature is either asserted or exempt, and nothing is silently neither.
#[test]
fn every_feature_is_either_a_signal_property_or_a_named_observation_statistic() {
    let mut unclassified = Vec::new();
    for name in FEATURE_NAMES {
        let asserted = RULES.iter().any(|r| r.feature == *name);
        let exempt = OBSERVATION_STATISTICS.iter().any(|(n, _)| n == name);
        assert!(
            !(asserted && exempt),
            "{name} is both asserted as a signal property and exempt as an observation statistic"
        );
        if !asserted && !exempt {
            unclassified.push(*name);
        }
    }
    assert!(
        unclassified.is_empty(),
        "features@{} added dimension(s) {unclassified:?} that are neither asserted as a signal \
         property in RULES nor named in OBSERVATION_STATISTICS. Decide which, in a diff someone \
         reads - that choice is what this file exists to make visible.",
        hk_classify::features::FEATURES_VERSION,
    );
    for (name, why) in OBSERVATION_STATISTICS {
        assert!(
            FEATURE_NAMES.contains(name),
            "{name} is exempted but is not a feature: delete the exemption"
        );
        assert!(
            why.len() > 80,
            "{name} is exempted without saying which observation it is a statistic of"
        );
    }
    for rule in RULES {
        assert!(
            FEATURE_NAMES.contains(&rule.feature),
            "{} has a tolerance but is not a feature",
            rule.feature
        );
        assert!(rule.why.len() > 80, "{} has no derivation", rule.feature);
    }
}

/// The guard: truncate one waveform and every signal-property feature stays inside the spread its
/// own estimator can account for.
#[test]
fn signal_features_survive_truncation_of_one_waveform() {
    let mut c14 = SymbolEstimator::default();
    let mut failures: Vec<String> = Vec::new();
    let mut worst: Vec<(f64, &str, String)> = Vec::new();
    let mut compared = 0usize;
    for class in Class::TAXONOMY {
        for seed in DEV_SEEDS.start..DEV_SEEDS.start + 3 {
            let s = generate(*class, &SynthConfig::new(SNR_DB, seed));
            let n_full = s.samples.len();
            let ratio = s.symbol_sample_rate_hz / s.sample_rate_hz;
            let measure = |c14: &mut SymbolEstimator, take: usize| {
                let sym_take = ((take as f64 * ratio) as usize).min(s.symbol_samples.len());
                let symbols = c14.from_samples(
                    &s.symbol_samples[..sym_take],
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(SNR_DB),
                );
                features(&FeatureInput {
                    samples: &s.samples[..take],
                    sample_rate_hz: s.sample_rate_hz,
                    obw_hz: Some(s.obw_hz),
                    snr_db: Some(SNR_DB),
                    symbols: symbols.as_ref(),
                })
            };
            let full = measure(&mut c14, n_full);
            // The emission's own on-fraction, measured over the full record and used below to
            // convert samples into independent looks.
            let duty = full.get("duty").unwrap_or(1.0).clamp(0.05, 1.0);
            for div in PREFIX_DIVISORS {
                let take = n_full / div;
                if take < MIN_SAMPLES {
                    continue;
                }
                let cut = measure(&mut c14, take);
                for rule in RULES {
                    let (Some(a), Some(b)) = (full.get(rule.feature), cut.get(rule.feature)) else {
                        continue;
                    };
                    compared += 1;
                    let moved = (b - a).abs();
                    let tol = rule.tolerance(take, a, duty);
                    worst.push((
                        moved / tol.max(f64::MIN_POSITIVE),
                        rule.feature,
                        format!("{class:?}/s{seed}/N over {div}"),
                    ));
                    if moved > tol {
                        failures.push(format!(
                            "{:<20} {class:?}/seed {seed}: N/{div} ({take} samples) reads {b:.4} \
                             against {a:.4} over the full {n_full}; moved {moved:.4}, tolerance \
                             {tol:.4} ({:.1}x). {}",
                            rule.feature,
                            moved / tol,
                            rule.why,
                        ));
                    }
                }
            }
        }
    }
    assert!(
        compared > 1000,
        "only {compared} comparisons: the ladder collapsed"
    );
    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!("tightest margins over {compared} comparisons (moved / tolerance):");
    for (ratio, feature, at) in worst.iter().take(12) {
        eprintln!("  {feature:<20} {ratio:>6.2}  {at}");
    }
    assert!(
        failures.is_empty(),
        "{} feature(s) named as a property of the signal moved further under truncation than \
         their own estimator can account for. Either the feature is measuring the observation - \
         in which case fix it, or name it in OBSERVATION_STATISTICS with what it is a statistic \
         of - or the tolerance's derivation is wrong. Do NOT widen a tolerance to the number it \
         happens to need.\n\n{}",
        failures.len(),
        failures.join("\n\n"),
    );
}
