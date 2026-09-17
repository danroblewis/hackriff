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
//!
//! # Which of these are properties of the signal, and which of the observation
//!
//! `tests/feature_length_invariance.rs` is the answer, and it is executable: every dimension of
//! [`FEATURE_NAMES`] is either asserted to survive truncation of one waveform inside a derived
//! tolerance, or named in that file's `OBSERVATION_STATISTICS` with the mechanism that makes it a
//! statistic of the capture and the measurement that showed it. A new dimension that is in neither
//! list fails the test, so the choice cannot be made silently — which is how all seven of the
//! defects the audit (T-281) found were introduced. **Ten of the thirty are exempt and twenty are
//! asserted**, after T-404 fixed all seven the guard found on its first run: `c20_norm`, `c40_norm`
//! and `c42_norm` shared **one** defect (`C20 = mean(x²)` is a coherent sum, so its integration
//! loss was the residual carrier offset times the record length) and recovered together under
//! [`CUMULANT_BLOCK`]; `cp_corr`, `if_slope_r2`, `if_local_modality` and `carrier_line_db` each had
//! their own, recorded at the constant or function that fixes it. What is still exempt is the
//! phase-residual pair, the whole-record IF histogram pair, and the six C14 statistics, each of
//! which names the observation it is a statistic of.

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
///
/// **5 (T-312):** the five spectral features — `flatness`, `symmetry`, `carrier_line_db`,
/// `sk_mean` and `gamma_max` — are now measured at a **fixed** transform length
/// ([`FEATURE_FFT_LEN`]) instead of one derived from the snippet. No definition moved; the
/// yardstick did, which is the same class of defect by a different route, and it reached further
/// than the ticket that found it supposed.
///
/// `fft_len` was `(n/8).next_power_of_two().clamp(64, 1024)`, so a shape measured in bins was
/// measured against a bin width set by the record length. That was already known to make one
/// emission read differently when watched twice. What was **not** known is that it also splits the
/// fitted corpus: [`crate::synth`]'s normalised snippets are 2328–16 384 samples depending on class
/// *and seed*, so under the old rule `am`, `nbfm`, `cw`, `bpsk`, `qpsk` and the rest were fitted
/// entirely at `fft_len` 512 while `wfm`, `ofdm`, `chirp`, `ppm` and `pulse` were fitted entirely
/// at 1024 — and `ssb`, `fsk2`, `fsk4`, `ook`, `dsb-sc` and `vsb-am` were fitted at **both, mixed
/// by seed**. A single Gaussian per class was being fitted over a mixture of two resolutions, with
/// the mixture proportion decided by where the generator's decimation happened to land. Every one
/// of those classes changes value here, which is why this is a version and a refit rather than a
/// repair.
///
/// The bump follows the T-286 precedent rather than setting a new rule: a feature that stops
/// measuring the wrong thing gets a version, because a stored `features@N` vector has to identify
/// one computation.
///
/// **6 (T-404):** seven dimensions changed meaning at once, all of them to stop measuring the
/// observation — the family T-313's guard found on its first run. `c20_norm`, `c40_norm` and
/// `c42_norm` are now accumulated over [`CUMULANT_BLOCK`]-sample blocks and combined incoherently,
/// so a coherent sum's integration loss is set by the block and not by the record. `cp_corr`
/// reports each lag as a bias-corrected coherence, so a lag with no cyclic prefix behind it reads
/// 0 rather than `1/sqrt(record)`. `if_slope_r2` reports the maximum over **all three** of
/// [`RAMP_WINDOWS`] or abstains, instead of a maximum over whichever lengths happened to fit.
/// `if_local_modality` counts modes on a kernel-smoothed histogram and averages across windows
/// rather than reading a median of integers off a raw one. `carrier_line_db` reports the line's
/// excess over the peak a band of pure noise would have shown at that segment count. Both density
/// files are refitted against it.
pub const FEATURES_VERSION: u32 = 6;

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

/// Samples the cumulant sums integrate over before they are combined incoherently (T-404).
///
/// # Why the whole record was the wrong integration length
///
/// `C20 = mean(x²)` and `C40`'s leading term `mean(x⁴)` are **coherent** sums. With a residual
/// carrier offset `δ` left by [`derotate`], `x²` spins at `2δ` and `x⁴` at `4δ`, so summing over
/// `N` samples multiplies the answer by `sinc(2δN)` — a function of the **record length**, not of
/// the emission. That is what T-313's guard measured: one `pulse` waveform, truncation only and
/// the de-rotation decision identical at every rung, read `c20_norm` 0.825 / 0.801 / 0.737 at N/8,
/// N/4, N/2 and **0.079** over the full record; `c40_norm` on another read 3.20 / 3.33 / 3.59 and
/// **16.73**; and `c42_norm`, which subtracts `|C20|²`, inherited 93 % of its own move from that
/// term. One defect, three dimensions.
///
/// The offset is real and is not going away. [`derotate`] fires on every class (measured coherence
/// 0.70–1.00 across the taxonomy), but what it removes is the **mean instantaneous frequency**,
/// whose estimate is dominated by the modulation's own phase steps and therefore carries a
/// standard error of about `1/sqrt(n)` rad/sample — 0.003 cycles/sample on a 3000-sample snippet,
/// measured directly. Multiply that by the record and `C20` integrates through whole cycles.
///
/// # Blocks fix the integration length; they do not fix the offset
///
/// Summing over a **fixed** block and averaging the block magnitudes leaves each block's coherence
/// loss at `sinc(2δL)` — a constant of the emission and the receiver, which a fitted density
/// absorbs as its own mean — instead of a function of how long the emitter was watched. This is the
/// same trade T-312 made for the transform length: **bias for variance, because only variance can
/// be absorbed by a fitted density.** The alternative the exemption named, estimating `δ` to a
/// tighter residual, is not available: for a suppressed-carrier modulation the estimators that work
/// are maxima over a search grid whose size grows with the record, which trades this defect for a
/// weaker version of `cp_corr`'s.
///
/// The price is a **noise floor** on the two magnitudes: for an emission whose true `C20` is zero,
/// `mean|Ĉ20_b|` converges to `sqrt(π/4L)` — 0.157 for a constant-modulus constellation at this
/// block, against the 0.996 a BPSK reads — rather than falling towards 0 as `1/sqrt(N)`. That
/// floor is fixed by `L`, so it is the same number for every record, which is the whole point, and
/// the fitted densities carry it.
///
/// # Value
///
/// `2δL` has to stay well under a cycle at the **shortest** record, because `δ`'s own estimation
/// error grows as the record shrinks: at `n = 291` (an eighth of the shortest snippet in the
/// corpus) that error is ~0.009 cycles/sample, so `2δL` is 0.6 cycles at `L = 32` and 2.4 at
/// `L = 128`. 32 is where the coherence loss the offset costs sits inside the moment's own sampling
/// error at every length the classifier accepts, and it still leaves eight blocks in the shortest
/// record [`MIN_SAMPLES`] admits. Measured across the ladder, it takes `bpsk` from an erratic
/// 0.09–0.93 to 0.992–0.997 and `am` from 0.92–1.02 to 0.997–1.000.
pub const CUMULANT_BLOCK: usize = 32;

/// Independent products a lagged correlation gets per sample of record.
///
/// `hk_estimate::normalise` puts the classifier's snippets at ~2 samples per OBW99, so the
/// products `x[k+ℓ]·x[k]*` that [`cyclic_prefix_correlation`] sums are correlated over roughly one
/// sample and `n` of them carry about `n/2` independent draws. The same bound the length-invariance
/// guard states for [`FEATURE_NAMES`]'s per-sample statistics, used here for the null level of a
/// correlation rather than for a tolerance.
pub const CORRELATION_LOOKS_PER_SAMPLE: f64 = 0.5;

/// Bins either side of the strongest line that [`spectral_features`] reports `carrier_line_db` over
/// — the Hann main lobe, the same width and for the same reason as [`CARRIER_GUARD_BINS`].
///
/// The line's power is spread over its whole main lobe by the window, so a single bin is not the
/// line; and the **mean** over a fixed lobe is linear in the periodogram, where a single bin picked
/// as the maximum is an order statistic whose upward bias depends on the segment count (T-404).
pub const CARRIER_LOBE_BINS: usize = CARRIER_GUARD_BINS * 2 + 1;

/// Smallest number of spectrum bins the shape features are measured over. A narrow emission (a
/// carrier, a CW tone) occupies one or two bins, where flatness is trivially 1 and the carrier
/// line trivially 0 dB — both meaningless, and both wrong in the direction of "this looks like
/// noise". Widening the window to its neighbourhood keeps the comparison honest.
pub const MIN_SHAPE_BINS: usize = 16;

/// The analysis transform length every spectral feature is measured at, **independent of the
/// snippet length** (T-312).
///
/// # Why it is pinned
///
/// It used to be `(n/8).next_power_of_two().clamp(64, 1024)`, so a snippet shorter than 8192
/// samples was analysed at a *coarser* frequency resolution than a longer one: `flatness`,
/// `symmetry`, `carrier_line_db`, `sk_mean` and `gamma_max` (which sizes its PSD the same way in
/// [`psd_of_real`]) were each measured against a yardstick that changed with how long the emission
/// happened to be watched. A shape measured in bins is not a property of the emission when the bin
/// width moves; it is a property of the observation. That is the defect family T-281 audited, and
/// this was its seventh instance.
///
/// # Pinning, not per-resolution densities
///
/// The ticket allowed either. Pinning is chosen for a reason that is not convenience:
///
/// **What pinning trades away is bias for variance, and only variance can be absorbed by a fitted
/// density.** The product `fft_len × segments ≈ n` is fixed by the record, so one of the two has to
/// move with `n`. Letting the *transform* move changes the quantity being estimated — a Hann bin is
/// 8× wider at `n = 1024` than at `n = 8192`, so `flatness` and `carrier_line_db` have different
/// *expected values* at the two lengths, and no amount of fitting reconciles them. Letting the
/// *segment count* move leaves the estimand fixed and changes only the estimator's spread about it,
/// which a Gaussian class density already models as its own σ. A length-dependent bias is a
/// different measurement; length-dependent variance is the same measurement, less certainly.
///
/// Per-resolution-band densities were the alternative and are rejected on cost, not on principle:
/// they multiply every fitted model by the number of bands, split the fitting corpus between them,
/// and put the band into [`hk_model::classify::ClassProvenance`] so two readings of one emitter can
/// be told apart — all to keep a resolution that the measurement above says is not worth keeping.
///
/// # What the pinned length costs a short burst
///
/// `sk_mean` is the one feature that pays. Spectral kurtosis is estimated **across segments**
/// ([`hk_dsp::sk`]); its standard deviation on noise is `sqrt(4M²/((M−1)(M+2)(M+3)))` for `M`
/// segments, so at 50 % overlap a 16 384-sample snippet gives `M = 31` (σ 0.34), 4096 gives
/// `M = 7` (σ 0.62), 2048 gives `M = 3` (σ 0.78), and 1024 gives `M = 1`, where
/// [`hk_dsp::sk::estimate`] returns `NaN` and the feature **abstains**. Under the old sizing the
/// same snippets kept `M ≈ 15` throughout by shrinking the transform instead — a steadier number,
/// but a steady estimate of a moving quantity. Abstention is the honest outcome and the module's
/// existing rule: [`crate::density`] scores the class over the dimensions that are present.
///
/// The other four spectral features lose resolution *cells*, not resolution: a 2048-sample burst
/// is analysed over 1024 bins from 3 segments instead of 256 bins from 15, so `flatness` and
/// `sk_mean` are noisier while `symmetry` and `carrier_line_db` — both ratios of a line to its
/// neighbourhood — get *better*, because the line is no longer smeared across a bin 8× too wide.
///
/// # Value
///
/// 1024 is the old clamp ceiling, which is what every snippet of 8192 samples or more already used.
/// Every fitted density, the whole ADR-0016 §7 grid and `crate::synth`'s 16 384-sample default are
/// therefore **bit-identical** across this change, and no refit is needed: the pin moves only the
/// short snippets that were wrong. Choosing any other value would have changed every fitted mean at
/// once for no measured gain — the same reason T-281 left the clamp alone.
pub const FEATURE_FFT_LEN: usize = 1024;

/// Welch segments the spectral features require before they are measured at all.
///
/// Pinning [`FEATURE_FFT_LEN`] fixes *what* is measured; it does not fix how many segments a given
/// record yields, and `flatness`, `carrier_line_db` and `gamma_max` are **non-linear** functionals
/// of the periodogram, so their estimator carries a small-sample bias that depends on the segment
/// count `M`. For `flatness` that bias is exactly known: an `M`-averaged periodogram bin is
/// `Gamma(M)/M`, so the geometric/arithmetic ratio of a flat band converges to
/// `exp(ψ(M) − ln M) ≈ 1 − 1/(2M)` rather than to 1 — **0.98 at M = 31, 0.94 at M = 8, 0.84 at
/// M = 3 and 0.56 at M = 1**.
///
/// Three is where that stops being a correction and starts being the answer. At `M = 1` the
/// estimate is a raw periodogram: the 44 % geometric-mean bias is larger than the range `flatness`
/// separates classes over (measured on `wfm`, 5430 samples, the one-segment prefix reads 0.156
/// against 0.729 from seven segments of the same waveform), the per-bin variance is 100 %, and
/// [`hk_dsp::sk::estimate`] is undefined below `M = 2` by construction. A record that cannot supply
/// three segments cannot supply a spectral shape, and the honest report is the module's standing
/// one — abstain, and let [`crate::density`] score the class over the dimensions that are present.
///
/// At 50 % overlap this asks for `1024 + 2×512 = 2048` samples. Nothing in the fitted corpus is
/// shorter (the shortest normalised snippet `crate::synth` produces is 2328 samples), so this
/// removes no fitted dimension; it removes the four spectral features from bursts under ~2 ms at
/// the classifier's normalised rate, which previously got them from a transform 4–8× too coarse.
pub const FEATURE_MIN_SEGMENTS: usize = 3;

/// Whether `n` samples can supply [`FEATURE_MIN_SEGMENTS`] segments of `fft_len` at 50 % overlap.
fn enough_segments(n: usize, fft_len: usize) -> bool {
    n >= fft_len + (FEATURE_MIN_SEGMENTS - 1) * (fft_len / 2)
}

/// The transform length `n` samples are analysed with: [`FEATURE_FFT_LEN`] whenever the record can
/// supply it, and otherwise the largest power of two that fits.
///
/// Below [`FEATURE_FFT_LEN`] the *record itself* is the resolution limit — a 512-sample burst
/// cannot be resolved to 1024 bins by any choice made here — so the residual length dependence in
/// `[MIN_SAMPLES, FEATURE_FFT_LEN)` is physics rather than a decision. [`spectral_features`] says
/// so out loud with the `spectrum_resolution_limited` reason code, which is what the ticket's
/// "say which band a measurement came from" asks for, applied to the one band where it is
/// unavoidable.
pub fn feature_fft_len(n: usize) -> usize {
    if n >= FEATURE_FFT_LEN {
        return FEATURE_FFT_LEN;
    }
    let mut len = 64;
    while len * 2 <= n {
        len *= 2;
    }
    len
}

/// Leading constant of the kernel bandwidth [`modality`] smooths its histogram with, as a multiple
/// of `robust scale × n^(−1/5)`.
///
/// # Why not Silverman's 0.9
///
/// The rule of thumb is derived for a **unimodal** normal target, and the case it is known to
/// over-smooth is precisely the one this feature exists to measure. For a `K`-level sample the
/// scale that sets the bandwidth is the spread of the *whole* level set, which grows with `K` —
/// `σ/spacing` is 0.5 for two levels, 1.12 for four and 2.29 for eight — so the rule widens the
/// kernel exactly as the levels it must resolve get closer together. At 0.9 the four levels of a
/// `4fsk` merge into two and the dimension stops telling it from `2fsk` at all.
///
/// # What sets this value
///
/// The finest spacing in the taxonomy, which is `8fsk`'s: eight levels across the deviation gives
/// `σ = sqrt(21)·d` for a spacing of `2d`, so at a window of [`IF_LOCAL_WINDOW`] samples
/// (`n^(−1/5) = 0.287`) a bandwidth of `c·σ·n^(−1/5)` stays under half the spacing while
/// `c < 0.38`. **Nothing else is asked of it**: rejecting the histogram's own roughness is the
/// 25 % prominence rule's job, not the kernel's, and it does it — at this bandwidth `wfm`, `bpsk`,
/// `ook`, `chirp` and `noise-like` read exactly 1.00 modes on all eight dev seeds, while `2fsk`,
/// `gfsk` and `msk` read exactly 2.00, `4fsk` 3.81 and the held-out `8fsk` 3.31.
pub const KDE_BANDWIDTH: f64 = 0.30;

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
///
/// This is the shipped list and the whole of [`FEATURE_NAMES`] in a default build. The
/// experiment-only `cyclic-dims` feature appends four names to it (T-364); nothing else may.
const BASE_FEATURE_NAMES: [&str; 30] = [
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

/// Names of `features@N`, in vector order.
#[cfg(not(feature = "cyclic-dims"))]
pub const FEATURE_NAMES: &[&str] = &BASE_FEATURE_NAMES;

/// The four line significances carried as four dimensions in place of their max — T-310's
/// expansion, refused by T-328, re-priced by T-364. Indexed by `LineMethod as usize`.
#[cfg(feature = "cyclic-dims")]
const CYCLIC_DIM_NAMES: [&str; 4] = [
    "cyclic_db_m0",
    "cyclic_db_m1",
    "cyclic_db_m2",
    "cyclic_db_m3",
];

/// [`BASE_FEATURE_NAMES`] with [`CYCLIC_DIM_NAMES`] appended (T-364's `cyclic-dims` experiment).
#[cfg(feature = "cyclic-dims")]
const fn feature_names_with_cyclic_dims() -> [&'static str; 34] {
    let mut out = [""; 34];
    let mut i = 0;
    while i < BASE_FEATURE_NAMES.len() {
        out[i] = BASE_FEATURE_NAMES[i];
        i += 1;
    }
    let mut j = 0;
    while j < CYCLIC_DIM_NAMES.len() {
        out[BASE_FEATURE_NAMES.len() + j] = CYCLIC_DIM_NAMES[j];
        j += 1;
    }
    out
}

/// Names of `features@N`, in vector order.
///
/// **Experiment build only** (`--features cyclic-dims`, T-364). `cyclic_db` is still in the list —
/// names never change meaning within a version — but [`symbol_features`] leaves it abstaining and
/// sets the four `cyclic_db_m*` dimensions instead, so a model fitted in this build scores the
/// expansion and a model fitted in a default build scores the max. Nothing ships from here: the
/// shipped density files and every default build are untouched, because a name that always
/// abstains is dropped by `density::fit`'s `MIN_PRESENCE`.
#[cfg(feature = "cyclic-dims")]
pub const FEATURE_NAMES: &[&str] = &feature_names_with_cyclic_dims();

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
    if let Some(r2) = ramp_linearity(&smooth(&fi, 8)) {
        f.set("if_slope_r2", r2);
    } else {
        f.reasons.push("no_ramp_windows".into());
    }
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
///
/// The three normalised cumulants are accumulated over [`CUMULANT_BLOCK`]-sample blocks and
/// combined **incoherently**; see that constant for why the whole-record sum was a statistic of the
/// record. `mu42` needs no such treatment: `mean(|x|⁴)` has no phase in it, and it is asserted
/// length-invariant today.
fn cumulant_features(f: &mut Features, x: &[Complex64]) {
    let n = x.len() as f64;
    let c21 = x.iter().map(|s| s.norm_sqr()).sum::<f64>() / n;
    if !(c21.is_finite() && c21 > 0.0) {
        return;
    }
    let m4_abs = x.iter().map(|s| s.norm_sqr() * s.norm_sqr()).sum::<f64>() / n;
    f.set("mu42", m4_abs / (c21 * c21));

    // Blocks, not the record. Every block is the same length, so the coherence loss `sinc(2δL)` a
    // residual offset δ costs the Ĉ20 sum — and `sinc(4δL)` for Ĉ40 — is the same for a 300-sample
    // burst and a 16 000-sample one. `chunks_exact` drops a partial tail rather than letting one
    // short block average in at a different coherence.
    let block = CUMULANT_BLOCK.min(x.len());
    let (mut c20, mut c40, mut c42, mut blocks) = (0.0, 0.0, 0.0, 0.0);
    for b in x.chunks_exact(block) {
        let bl = b.len() as f64;
        let m2_b: Complex64 = b.iter().map(|s| s * s).sum::<Complex64>() / bl;
        let m4_b: Complex64 = b.iter().map(|s| s * s * s * s).sum::<Complex64>() / bl;
        let m4abs_b = b.iter().map(|s| s.norm_sqr() * s.norm_sqr()).sum::<f64>() / bl;
        c20 += m2_b.norm();
        c40 += (m4_b - 3.0 * m2_b * m2_b).norm();
        // The **record's** C21 in the subtracted term, not the block's: only the |C20|² term is
        // being taken block-wise, because only it carries the phase. Using a per-block C21 would
        // additionally replace `2·C21²` with `2·mean(C21_b²)`, which for a keyed emission is a
        // different quantity (Jensen) and has nothing to do with this defect.
        c42 += m4abs_b - m2_b.norm_sqr() - 2.0 * c21 * c21;
        blocks += 1.0;
    }
    if blocks <= 0.0 {
        return;
    }
    f.set("c20_norm", c20 / blocks / c21);
    f.set("c40_norm", c40 / blocks / (c21 * c21));
    f.set("c42_norm", c42 / blocks / (c21 * c21));
}

/// Spectral flatness, symmetry, carrier line and mean spectral kurtosis over the occupied band.
///
/// The analysis transform is [`FEATURE_FFT_LEN`], **not** a function of the snippet length; see
/// [`feature_fft_len`] for why, and for what the pin costs a burst too short to supply it.
fn spectral_features(f: &mut Features, input: &FeatureInput<'_>) {
    let n = input.samples.len();
    let fft_len = feature_fft_len(n);
    if fft_len < FEATURE_FFT_LEN {
        f.reasons.push("spectrum_resolution_limited".into());
    }
    if !enough_segments(n, fft_len) {
        f.reasons.push("spectrum_too_few_segments".into());
        return;
    }
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
    // Segments the periodogram averaged, at 50 % overlap. `carrier_line_db` needs it to know what a
    // band of pure noise would have peaked at.
    let segments = (n - fft_len) / (fft_len / 2) + 1;
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
    // **The peak is reported against the peak a band of pure noise would have shown** (T-404).
    //
    // `10·log10(max bin / band median)` is a ratio of two order statistics, and the maximum's is the
    // one that bites: over `B` bins of an `M`-averaged periodogram — each bin `Gamma(M)/M` — the
    // largest sits far above the mean when `M` is small and close to it when `M` is large. That is a
    // reading of the **segment count**, which is the one property of the record T-312's transform pin
    // deliberately left moving, and T-313's guard measured it: a 4072-sample prefix of one 2-FSK
    // waveform read 32.05 dB where the full 16 290 read 14.13 — 17.9 dB on a dimension that separates
    // carrier-present from carrier-absent by about 20 — with `ofdm` and `noise-like` moving 4.4 and
    // 2.7 dB the same way.
    //
    // [`null_peak_over_median_db`] is what that ratio would read on noise alone at this `M` and this
    // many bins, and subtracting it turns the feature into a **CFAR statistic**: dB by which the
    // strongest line exceeds what chance would have put there, which is 0 for a band with no line in
    // it at every segment count. A carrier is 20–50 dB clear of that and loses only the few dB the
    // correction is worth.
    // The subtraction is in **power**, not in dB: `10·log10(1 + excess/median)` leaves a real
    // carrier — 20–50 dB clear of the null — reading what it always did, and takes a band with no
    // line in it to 0 dB at every segment count, where a dB subtraction would instead have dragged
    // the carrier down by the whole correction (measured: `pulse` 27.1 dB over 16 384 samples
    // against 22.1 over 2048, a defect swapped for its mirror image).
    let median = median_of(band);
    let peak = band.iter().copied().fold(0.0_f64, f64::max);
    if median > 0.0 && peak > 0.0 {
        let null = 10f64.powf(0.1 * null_peak_over_median_db(band.len(), segments));
        let excess = (peak / median - null).max(0.0);
        f.set("carrier_line_db", 10.0 * (1.0 + excess).log10());
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
/// delay-multiply, |d IF|²).
///
/// **It can therefore also be a statistic of the capture chain, not of the observation or the
/// signal (T-373).** A capture that modulates its own samples periodically puts a comb of lines
/// into every channel it holds, and `cyclic_db` is a max — so one comb member winning makes this
/// dimension a measurement of the receiver. Measured on the 100.4653 MHz box of
/// `capture-2026-09-15-fm-band`, whose stream carries an 8192-sample gain step: of 208 reported
/// lines over 50 narrowband boxes × 3 windows, 5 were comb harmonics, all of them on that one box,
/// at up to 25.2 dB, and the comb was that box's top rate candidate in 2 windows of 3 — so
/// `cyclic_db` there was the artefact's line, 22.8 dB, and not the emission's. C14 now excludes the
/// comb (derived from the capture's own provenance and sample rate), which takes that count to
/// 0 of 208 and moves this box's `cyclic_db` to 22.5 dB from a different line. Nothing in the
/// fitted models moves: the artefact is in one fixture, not in the synthetic dev grid the
/// densities are fitted on.
///
/// **Excluding one artefact promoted another, twice, and the third one drifts (T-382).** The 22.5
/// dB T-373 measured after its fix was *also* the receiver: over a 44-box grid across that
/// capture's passband, the argmax was an exact 8 kHz comb in 10 boxes and a free-running ~655.75
/// Hz modulation's second harmonic in 28 more, and 89 of 98 rate candidates were one of the three
/// receiver artefacts. T-382 identified both — the 8 kHz comb is the host's own clock grid (the
/// same artefact T-317 had separately found in the RF spectrum, proved by two observables 12 600×
/// apart agreeing on the receiver's clock error to 0.35 ppm) and the ~655.75 Hz family is a
/// thermally free-running amplitude modulation of the receiver's noise contribution, wandering
/// 2600 ppm, which is why it needed [`hk_model::CaptureArtefact::drift_ppm`] rather than a wider
/// flat notch. With all three excluded that grid reports **0 of 44** artefact argmaxes at every
/// window, the per-box median `cyclic_db` falls **27.4 → 14.0 dB**, and the boxes still above 20 dB
/// are the two broadcast stations' genuine 19 kHz pilot and 38 kHz subcarrier. A fourth family
/// spaced ~119.95 Hz is visible underneath at 13–17 dB and is not excluded.
///
/// **The general lesson is not "notch three combs".** On a real capture this dimension was, before
/// T-373, a measurement of the receiver in essentially every narrowband box; each exclusion has
/// been a named frequency derived from one fixture's provenance, and the floor it reveals is the
/// next artefact. A dimension that is safe on real captures needs the receiver-wide test itself —
/// a line at one frequency in channels holding nothing is the receiver's, whether or not anyone
/// has written it down — not a longer list.
///
/// **T-394 built that test** ([`hk_estimate::blind::receiver`]). It channelises the capture, finds
/// the channels whose power is at the local noise floor and whose neighbours' is too, whitens each
/// through C14's own transform and takes the **median across them**: a line more than half of
/// those channels carry is device-local by construction, and an emission's structure — confined to
/// its own band and skirt — is not. Nothing about it is a frequency, a drift or a fixture.
///
/// Measured on `capture-2026-09-15-fm-band` with the provenance records **stripped**, so only the
/// measurement can act: over 16 reference channels the three families that each needed a record
/// come back (the `fs/8192` comb's h1–h6, the 8 kHz comb's h1–h7, the ~655.75 Hz family's h1–h4),
/// **and so does the fourth, unrecorded, ~119.95 Hz family** (1439/1559/1679 and 2039/2159/2279
/// Hz, 12.4–16.1 dB) — caught with nothing naming it. Over a 22-box grid across the passband the
/// argmax is a receiver line in **18 of 22** boxes without it and **0 of 22** with it, the per-box
/// median `cyclic_db` falls **27.8 → 13.8 dB**, boxes above 20 dB go 22 → 4, and the three
/// strongest are the stations' genuine 19 kHz pilot (31.9, 30.1 dB) and 38 kHz subcarrier
/// (29.1 dB), 6.5 dB clear of anything else. The control the whole thing turns on holds: the
/// pilot reads **3.2 dB** in this statistic against 20–31 dB for every receiver family, because it
/// is in a handful of channels and they are in all of them.
///
/// It is **not yet wired into the pipeline's own call site**: the survey wants a second or more of
/// the raw tuned span, and `hk_pipeline::classify::classify_box` is handed one burst. Until a
/// caller with a capture window calls [`crate::SymbolEstimator::survey_receiver_lines`], this
/// dimension still leans on the recorded exclusions on a real capture.
///
/// T-281 reasoned that a coherent line's peak grows with the record
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
/// # The four `blind_*` scores: T-311's answer, and it was not a lock
///
/// T-281 reported the same window dependence in them, discontinuously — a 2-FSK burst scoring
/// `blind_fsk` 0.10 at N/8, 0.44 at N/4 and 1.00 at N/2 and N — and read it as the line C14 locks
/// on changing once the record is long enough. **That ladder does not reproduce** once T-404's
/// packet-preamble confound is off: `fsk2`, `gfsk` and `msk` read exactly 1.00 at all four rungs on
/// every dev seed. The mechanism was somewhere else, and it was not binary.
///
/// **Three of the four quantities the scores are built from are MAXIMA, and a maximum's null level
/// rises as the record shrinks.** The carrier-line coherence is a maximum over `n` bins, whose null
/// is `sqrt(Σ|z|²·ln n)/Σ|z|`; `FskCentreStats::periodicity` is a maximum over 8 lags about a
/// proportion's `0.5 ± K/sqrt(m)`; `fisher_j` is a maximum over 8 sampling phases and up to 4
/// candidate rates. Every one was compared against a **fixed** threshold, so a threshold sharper
/// than the statistic feeding it turned that statistic's own sampling noise into a ×3 to ×10 jump
/// in a number handed to a fitted Gaussian. Three fixes, all in `hk_estimate::blind`:
///
/// 1. The coherence is **bias-corrected in power**, so its null is 0 at every record length — the
///    move T-404 made for `cp_corr`, whose maximum-over-lags had the same defect.
/// 2. Every veto **ramps across its own statistic's 3σ** about the unchanged threshold
///    (`blind::family::soft_veto`), and so does the OOK/FSK competition, which had been a pair of
///    hard thresholds making each score a discontinuous function of the other.
/// 3. Where a lock genuinely **is** binary — too few members in the smaller IF cluster for the
///    Fisher ratio's denominator to exist, or too few keyings for an envelope contrast — the score
///    is **ABSENT**, not low (`blind::family::MIN_CLUSTER_MEMBERS`,
///    `blind::MIN_ENVELOPE_TRANSITIONS`). Absent means *not measured*, T-297's rule for
///    `sweep_rate_hz_per_s`, so a not-yet-locked reading can no longer be scored as evidence for a
///    different family.
///
/// What that bought, measured on the dev ladder: **a class's own family score is now flat** —
/// `ook`, `cw`, `fsk2`, `gfsk`, `msk`, `bpsk` and `qpsk` read 1.00 at N, N/2, N/4 and N/8 on every
/// seed, and `fsk4` 0.82–0.93, where `cw` used to collapse to a fabricated 0.00 at N/8 off nine
/// keyings. `feature_length_invariance::a_classs_own_family_score_survives_truncation` asserts it.
///
/// **They stay exempt anyway, for the other half.** A class's score for a family it is *not* is a
/// detector's response to a signal it was not built for, and nothing entitles that to be stable;
/// `qam16`'s `blind_qpsk` moves 0.9 → 0.18 because a QAM's x⁴ line is a property of the symbol
/// sequence, and `ppm`'s `blind_ook` moves 1.00 → 0.20 because PPM is framed with inter-frame gaps,
/// so a prefix is a different on/off mixture. Both are the emission, not the estimator.
///
/// **Cost, against main on the identical grid:** known top-1 0.9345, top-2 0.9861, wrong-label
/// 0.0040 overall and 0.0333 worst-bin — **every one unchanged**. Only the open set moved: M3
/// unknown recall 0.9596 → 0.9520 and false-known 0.0404 → 0.0480, three snippets in 396, both far
/// inside ADR-0016 §7's floors. That direction is the one to expect and it is T-312's, not T-404's:
/// the veto constants were a **categorical fingerprint** — every `am` snippet read `blind_bpsk`
/// exactly 0.30 because 0.30 meant "the c1 gate fired" — so the fitted sigma was near zero and the
/// χ² open set was leaning on a spike that measured which branch was taken rather than the signal.
/// Withdrawing it costs open-set recall and buys nothing back in known accuracy, because a spike
/// every class shares does not separate knowns.
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
///
/// **T-364 re-measured both over eight seed bases and found the number understates the gap, for a
/// reason that matters here** (`docs/17-burst-recall-vs-open-set.md`, re-derive with
/// `just t364-curves`). T-328 shortened **C14's window only** — the classifier still saw a
/// full-length snippet — and that reading reproduces at 38.4 % ±2.1 %. A *genuinely* short emission
/// is short in the classifier's snippet too, and measured that way the rejection is **70.3 %
/// ±1.7 %** against 9.4 % ±1.4 % on full windows. Pooling C14's window at fit time removes almost
/// all of the first (38.4 % → 11.1 %) and little of the second (70.3 % → 58.0 %), which locates most
/// of the remaining harm **outside** this statistic, in the other ~20 features' own dependence on
/// the snippet length. Pooling costs held-out unknown recall 0.9530 ±0.0084 → 0.8911 ±0.0112 with
/// the max, and 0.9561 ±0.0057 → 0.9088 ±0.0053 with the four dimensions — so T-328's ordering
/// reversal is real, and at a pooled fit the expansion is the only form measured that holds
/// ADR-0016 §7's 0.90 floor. **Nothing was adopted**: the trade is a product decision open for the
/// user, and the densities and this statistic are unchanged.
///
/// `cyclic_line_window.rs` pins the structural findings — that the band **does not** move, and that
/// a short burst puts three of the four line significances off at once — so neither the wrong law,
/// nor the fixed defect, nor the refused expansion can be re-derived from a single seed.
fn symbol_features(f: &mut Features, input: &FeatureInput<'_>) {
    let Some(s) = input.symbols else {
        return;
    };
    #[cfg(not(feature = "cyclic-dims"))]
    {
        let best = s
            .lines
            .iter()
            .map(|l| l.significance_db)
            .fold(f64::NEG_INFINITY, f64::max);
        if best.is_finite() {
            f.set("cyclic_db", best);
        }
    }
    // T-364's experiment build: the four significances as four dimensions instead of their max.
    // Off by default; see `FEATURE_NAMES`.
    #[cfg(feature = "cyclic-dims")]
    for l in &s.lines {
        if let Some(name) = CYCLIC_DIM_NAMES.get(l.method as usize) {
            f.set(name, l.significance_db);
        }
    }
    if let (Some(obw), Some(rate)) = (input.obw_hz, s.symbol_rate_bd.value()) {
        if rate > 0.0 {
            f.set("obw_over_rs", obw / rate);
        }
    }
    // ABSENT means NOT MEASURED (T-311, T-297's rule for `sweep_rate_hz_per_s`). C14 returns 0.0
    // for a family whose evidence it could not look at as well as for one it looked at and ruled
    // out, and a density fitted over both reads the first as evidence for whatever class sits near
    // zero — evidence manufactured from a failure to measure. The two conditions below are exactly
    // C14's own admission rules, so what stays is "measured, no evidence" and what goes is "could
    // not look".
    if s.family_features.envelope_changes >= hk_estimate::blind::MIN_ENVELOPE_TRANSITIONS {
        f.set("blind_ook", s.family_scores.ook);
    }
    if s.family_features.fsk.is_some() {
        f.set("blind_fsk", s.family_scores.fsk);
    }
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
            (norm > 0.0).then(|| {
                let raw = acc.norm() / norm;
                // **Subtract the null, because a correlation of more noise is smaller** (T-404).
                //
                // A sample coherence over `k` independent products has `E[r²] ≈ ρ² + 1/k` under
                // the null, so the raw value of a lag with no cyclic prefix behind it is not 0 but
                // `1/sqrt(k)` — and `k` is set by the record. For the one class that HAS a cyclic
                // prefix the correction is negligible (ρ² ≫ 1/k); for the other twenty it is the
                // whole reading. T-313 measured 0.535 / 0.440 / 0.110 over N/8, N/4 and the full
                // 16 290 samples of one 2-FSK waveform, a 4.9× move on a dimension whose OFDM
                // threshold is 0.15: a short burst of any class read a cyclic prefix it does not
                // have.
                //
                // `(r² − 1/k)/(1 − 1/k)` is the standard bias-corrected magnitude-squared
                // coherence, clamped at zero because a negative power estimate is not a
                // correlation.
                let k = (x.len() - lag) as f64 * CORRELATION_LOOKS_PER_SAMPLE;
                if k <= 1.0 {
                    return raw;
                }
                (((raw * raw) - 1.0 / k) / (1.0 - 1.0 / k)).max(0.0).sqrt()
            })
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
///
/// Sized by [`feature_fft_len`], for the reason given there: γ_max is a peak-to-mean over bins, so
/// a bin width that moves with the record makes it a statistic of the record.
fn psd_of_real(v: &[f64]) -> Option<Vec<f64>> {
    let fft_len = feature_fft_len(v.len());
    if !enough_segments(v.len(), fft_len) {
        return None;
    }
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

/// Adjacent bins of a Hann-windowed periodogram that count as **one** independent draw.
///
/// A Hann window correlates neighbouring DFT bins: their amplitude correlation is 2/3, so their
/// powers correlate at about 0.44 and a band of `B` bins carries roughly `B/2` independent looks.
/// [`null_peak_over_median_db`] needs the independent count, not the bin count, or it would credit
/// a smooth spectrum with twice the chances of throwing a high bin that it actually had.
const HANN_BIN_CORRELATION: f64 = 2.0;

/// The dB by which the largest of `bins` bins of an `M`-averaged periodogram **of pure noise**
/// exceeds their median.
///
/// This is the null level `carrier_line_db` is reported against. Each bin of an `M`-averaged
/// periodogram is `Gamma(M)/M`; the Wilson–Hilferty transform gives its quantile function in closed
/// form as `Q(p) = (1 − 2/(9M) + z_p·sqrt(2/(9M)))³`, accurate to better than 1 % for `M ≥ 3`
/// ([`FEATURE_MIN_SEGMENTS`] is 3, so this is only ever evaluated where it holds). The expected
/// largest of `k` independent draws is taken at `p = 1 − 1/(k+1)`, the plotting position of the top
/// order statistic, and the median at `p = 0.5`.
///
/// Worked, for the two ends of the corpus: at `M = 3` over 500 bins the ratio is about 8 dB, and at
/// `M = 31` about 3 dB. **That 5 dB is the whole of the length dependence** T-313 measured on this
/// feature, and it is a property of the estimator, not of any emission.
fn null_peak_over_median_db(bins: usize, segments: usize) -> f64 {
    let m = (segments.max(FEATURE_MIN_SEGMENTS)) as f64;
    let k = (bins as f64 / HANN_BIN_CORRELATION).max(2.0);
    let a = 2.0 / (9.0 * m);
    let q = |z: f64| (1.0 - a + z * a.sqrt()).max(1e-6).powi(3);
    let peak = q(normal_quantile(1.0 - 1.0 / (k + 1.0)));
    let median = q(0.0);
    if median <= 0.0 {
        return 0.0;
    }
    10.0 * (peak / median).log10()
}

/// Standard-normal quantile, Acklam's rational approximation (absolute error < 1.15e-9).
fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239e0,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838e0,
        -2.549_732_539_343_734e0,
        4.374_664_141_464_968e0,
        2.938_163_982_698_783e0,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996e0,
        3.754_408_661_907_416e0,
    ];
    const P_LOW: f64 = 0.024_25;
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        return (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0);
    }
    if p > 1.0 - P_LOW {
        return -normal_quantile(1.0 - p);
    }
    let q = p - 0.5;
    let r = q * q;
    (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
        / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
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

/// The sample median, **interpolated for an even count** (T-404).
///
/// Taking the upper of the two middle values is an upward bias that grows as the sample shrinks —
/// at two draws it is not a median at all but a maximum, and the aggregators in this module
/// (`if_slope_r2` over ramp windows, `if_local_*` over level windows) are handed as few as two.
/// T-313's guard measured the consequence: one `wfm` waveform read `if_slope_r2` 0.440 over 5430
/// samples and 0.790 over the first 678, where two windows of 256 made the "median" their larger.
fn median_of(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let mid = s.len() / 2;
    if s.len() % 2 == 0 {
        0.5 * (s[mid - 1] + s[mid])
    } else {
        s[mid]
    }
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
    const BINS: usize = 128;
    if v.len() < 8 {
        return 1;
    }
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
    // **A kernel density, not a raw histogram** (T-404).
    //
    // A fixed 3-bin box over 48 bins leaves the counts jagged, and a mode count is an integer read
    // off that jaggedness: T-313's guard measured `gfsk`, `msk` and `2fsk` reading 2 modes over the
    // full record and **4** over a quarter of it, and `ook` 1 and 3, from windows of the *same*
    // fixed length — the estimator inventing and losing whole modes because a histogram of fewer
    // counts is rougher. That matters more than the rest of the family because T-298 added
    // `if_local_modality` as the length-free *replacement* for `if_modality`, and the replacement
    // inherited the defect through its estimator instead of through its reference.
    //
    // The bandwidth is Silverman's rule of thumb, `0.9 · min(sd, IQR/1.349) · n^(-1/5)`, on the
    // robust scale — so the smoothing is set by the spread of the data and its count, not by a
    // number chosen here, and a sample with tight well-separated levels (whose IQR-derived scale is
    // small) is smoothed less than a diffuse one. The interquartile form is what keeps it from
    // over-smoothing a multimodal sample, which is the failure Silverman's rule is known for.
    let n = v.len() as f64;
    let q1 = s[s.len() / 4];
    let q3 = s[(3 * s.len() / 4).min(s.len() - 1)];
    let scale = std_dev(v).min(((q3 - q1) / 1.349).max(f64::MIN_POSITIVE));
    let bin_width = (hi - lo) / (BINS as f64 - 1.0);
    let sigma_bins = KDE_BANDWIDTH * scale * n.powf(-0.2) / bin_width;
    let h: Vec<f64> = if sigma_bins >= 0.5 {
        let half = ((3.0 * sigma_bins).ceil() as usize).min(BINS - 1);
        let taps: Vec<f64> = (0..=2 * half)
            .map(|i| {
                let d = (i as f64 - half as f64) / sigma_bins;
                (-0.5 * d * d).exp()
            })
            .collect();
        (0..BINS)
            .map(|i| {
                taps.iter()
                    .enumerate()
                    .map(|(t, w)| {
                        // Reflect at the edges: a level sitting against the 2nd or 98th percentile
                        // must not be halved by a kernel hanging off the end of the grid.
                        let j = i as isize + t as isize - half as isize;
                        let j = if j < 0 {
                            (-j) as usize
                        } else if j as usize >= BINS {
                            2 * (BINS - 1) - j as usize
                        } else {
                            j as usize
                        };
                        w * hist[j.min(BINS - 1)]
                    })
                    .sum()
            })
            .collect()
    } else {
        hist.to_vec()
    };
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
/// The **bimodality** is the median across windows, for the reason [`window_r2`] takes one: a
/// snippet may contain a gap, a retune or an interferer, and one ruined window must not decide a
/// continuous statistic.
///
/// The **mode count** is the mean, and that difference is the T-404 fix. A median of integers is
/// discontinuous in a way a median of a continuous statistic is not: an emission whose windows
/// genuinely split between one mode and two has a median that flips whole between 1 and 2 as the
/// window count changes parity, which is a reading of the record and not of the emission —
/// measured on one `wfm` waveform, 1.0000 over 5430 samples against 2.0000 over the first 2715.
/// The mean answers 1.4 for both. It is also the more efficient estimator (standard error
/// `σ/sqrt(W)` against the median's `1.25σ/sqrt(W)`), and the exemption this replaces named
/// exactly this: a continuous statistic rather than a count read off a histogram.
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
    (!bimodal.is_empty()).then(|| {
        (
            median_of(&bimodal),
            modes.iter().sum::<f64>() / modes.len() as f64,
        )
    })
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

/// Window lengths [`ramp_linearity`] fits a line over. All three, always, or none of them.
const RAMP_WINDOWS: [usize; 3] = [128, 256, 512];

/// The best median within-window R² over [`RAMP_WINDOWS`], or `None` when the record cannot supply
/// every one of them.
///
/// **The abstention is the length fix** (T-404). A maximum over whichever window lengths happened to
/// fit is a different estimand at every record length, and it is biased in the direction that makes
/// the defect: a shorter window of any smooth process is better approximated by a straight line, so
/// dropping the 512-sample fit from the set can only raise the answer. T-313's guard measured one
/// `wfm` waveform at 0.440 over 5430 samples against 0.790 over the first 678 — a wideband FM
/// carrier looking like a linear chirp once you stop watching it — and 678 samples is exactly a
/// record that cannot fit two 512-sample windows. Reporting the max over a two-element set as though
/// it were the max over the three-element one is the same error as measuring a spectrum at a
/// transform length that moves (T-312); abstaining is this module's standing answer to an input it
/// does not have.
fn ramp_linearity(fi: &[f64]) -> Option<f64> {
    let mut best = f64::NEG_INFINITY;
    for w in RAMP_WINDOWS {
        best = best.max(window_r2(fi, w)?);
    }
    Some(best)
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
            // 0.4, not 0: the block-incoherent estimator has a floor of `sqrt(π/4L)` ≈ 0.157 for
            // a constant-modulus rotationally-symmetric constellation (see [`CUMULANT_BLOCK`]),
            // which is the price of an integration length that does not move with the record.
            // What the dimension has to do is separate that floor from a real |C20|, and it does:
            // 0.21 for `qpsk` against 0.996 for `bpsk`.
            assert!(c20 < 0.4, "{} |C20| {c20}", class.label());
            assert!(c20 < bpsk, "{} must sit below BPSK", class.label());
        }
        // The fourth-order cumulants keep the textbook **ordering** — BPSK 2, QPSK 1, 8PSK 0 — but
        // not the textbook values: oversampling flattens them, and the block-incoherent sum
        // ([`CUMULANT_BLOCK`]) adds its own floor on top. Before T-404 the whole-record sum spun
        // QPSK's |C40| below 8PSK's on some seeds and above BPSK's on others, and the pinned number
        // here was one of those readings. The ordering is what the dimension is for, so the
        // ordering is what is pinned.
        let c40 = |class| of(class, 30.0, 7).get("c40_norm").unwrap();
        let (bpsk4, qpsk4, psk8_4) = (c40(Class::Bpsk), c40(Class::Qpsk), c40(Class::Psk8));
        assert!(
            bpsk4 > qpsk4 && qpsk4 > psk8_4,
            "|C40| orders the constellations: bpsk {bpsk4}, qpsk {qpsk4}, 8psk {psk8_4}"
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

    /// The three T-404 fixes whose **null level** is the thing that used to move.
    ///
    /// `tests/feature_length_invariance.rs` compares one emission at two record lengths, which is
    /// the right question for a bias that scales with the record. It is a blunt instrument for a
    /// *null* that scales with the record, because on a single waveform the move can hide inside
    /// the estimator's own spread at the short end. These assert the property directly: a band, a
    /// lag set and a window set with **nothing in them** must read the same at every record length.
    #[test]
    fn the_null_levels_do_not_move_with_the_record() {
        let long = of(Class::NoiseLike, 25.0, 5);
        let s = generate(Class::NoiseLike, &SynthConfig::new(25.0, 5));
        let short = |take: usize| {
            features(&FeatureInput {
                samples: &s.samples[..take],
                sample_rate_hz: s.sample_rate_hz,
                obw_hz: Some(s.obw_hz),
                snr_db: Some(25.0),
                symbols: None,
            })
        };
        let n = s.samples.len();
        let quarter = short(n / 4);

        // `cp_corr`: the bias-corrected coherence of a lag with no cyclic prefix behind it is 0 at
        // every record length. The raw correlation was 0.886/sqrt(k), so an eighth of the record
        // read it 2.8x higher and a short burst of any class claimed a cyclic prefix.
        let (a, b) = (
            long.get("cp_corr").unwrap(),
            quarter.get("cp_corr").unwrap(),
        );
        assert!(
            a < 0.10 && b < 0.10,
            "noise has no cyclic prefix at either length: {a:.4} over {n}, {b:.4} over {}",
            n / 4
        );

        // `carrier_line_db`: a band with no line in it reads ~0 dB whatever the segment count. The
        // raw peak-over-median read about 8 dB at M = 3 against 3 dB at M = 31, purely because the
        // largest of many gamma-distributed bins stands further above their median when each bin is
        // noisier.
        let (a, b) = (
            long.get("carrier_line_db").unwrap(),
            quarter.get("carrier_line_db").unwrap(),
        );
        assert!(
            a.abs() < 3.0 && b.abs() < 3.0,
            "noise has no carrier line at either length: {a:.2} dB over {n}, {b:.2} dB over {}",
            n / 4
        );
        // And the correction it is reported against really does move, which is what makes the
        // subtraction necessary rather than cosmetic.
        assert!(
            null_peak_over_median_db(500, 3) > null_peak_over_median_db(500, 31) + 3.0,
            "the null peak at M = 3 ({:.2} dB) must stand well above the one at M = 31 ({:.2} dB)",
            null_peak_over_median_db(500, 3),
            null_peak_over_median_db(500, 31),
        );

        // `if_slope_r2`: all three of RAMP_WINDOWS or none of them. A record that cannot fit two
        // 512-sample windows must abstain rather than report a maximum over the shorter fits,
        // which can only read higher.
        let short_ramp = ramp_linearity(&vec![0.0; 2 * RAMP_WINDOWS[2] - 1]);
        assert!(short_ramp.is_none(), "one window short must abstain");
        assert!(
            ramp_linearity(&vec![0.0; 2 * RAMP_WINDOWS[2]]).is_some()
                || ramp_linearity(
                    &(0..2 * RAMP_WINDOWS[2])
                        .map(|i| i as f64)
                        .collect::<Vec<_>>()
                )
                .is_some(),
            "exactly two of the longest window is enough"
        );
    }
}
