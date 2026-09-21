//! **A feature named as a property of the signal must not move when the signal is watched for
//! less time** (T-313), **and must not reorder two emissions when they are heard more loudly**
//! (T-429).
//!
//! Two axes of *the observation*, in one file. The first varies the record length at a fixed SNR;
//! the second varies the SNR at a fixed record length. They need different assertions, for a
//! reason stated under "The second axis" below, and each keeps its own exemption list.
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
//! # It found seven more on its first run, and T-404 fixed all seven
//!
//! None of them by accident, which is the point. Each was recorded in [`OBSERVATION_STATISTICS`]
//! with the mechanism and the measurement, so that fixing one would make an exemption **disappear
//! from a diff** — and that is what happened. The exempt list went from **seventeen to ten** and
//! [`RULES`] from thirteen to twenty:
//!
//! | feature | mechanism it had | what it does now |
//! |---|---|---|
//! | `c20_norm` | a coherent sum, so its loss is the residual offset **times** the record | sums over a fixed block ([`hk_classify::features::CUMULANT_BLOCK`]) and combines the blocks incoherently |
//! | `c40_norm` | the same sum at four times the offset | the same block |
//! | `c42_norm` | subtracts `|C20|²`, so it inherited the above — 93 % attributed | recovers with the other two, as the attribution predicted |
//! | `cp_corr` | a maximum over lags, whose **null** falls as `1/sqrt(record)` | reports each lag as a bias-corrected coherence, whose null is 0 |
//! | `if_slope_r2` | a maximum over whichever window lengths happened to fit | all three window lengths or an abstention |
//! | `if_local_modality` | a mode **count** off a raw histogram, aggregated by a median of integers | a kernel-smoothed histogram, aggregated by a mean |
//! | `carrier_line_db` | a peak-over-median, biased in the segment count T-312 left moving | the line's excess over the peak a band of pure **noise** would have shown at that segment count |
//!
//! Two are worth noticing. `if_local_modality` was added by T-298 **as the length-free
//! replacement** for `if_modality`, and inherited the defect through its estimator instead of
//! through its reference — the replacement for an observation statistic was one. `carrier_line_db`
//! is the residue T-312 knew it was leaving: pinning the transform fixes *what* is measured and
//! leaves the segment count moving, which only matters for the features that are order statistics
//! rather than sums.
//!
//! # T-311 did the same for the four `blind_*` scores, and the exemption stayed
//!
//! Because the answer was **both**. T-281 read them as a lock/no-lock transition; measured with the
//! preamble off, three of the four quantities underneath turned out to be MAXIMA compared against
//! fixed thresholds — the carrier-line coherence over `n` bins, `periodicity` over 8 lags,
//! `fisher_j` over 8 phases and 4 candidate rates — so their NULLS moved with the record and a
//! threshold sharper than its own statistic read sampling noise as a ×10 jump. Those were fixed at
//! source (`hk_estimate::blind`: the coherence bias-corrected, every veto ramped across its own 3σ,
//! the OOK/FSK competition made continuous). The one condition that genuinely *is* binary — too few
//! members in the smaller IF cluster for the Fisher ratio's denominator to exist — now makes the
//! score **absent**, which is T-297's rule.
//!
//! What that fixed is the half the defect was reported on: **a class's own family score**, now flat
//! across the ladder and asserted by [`a_classs_own_family_score_survives_truncation`]. What it did
//! not fix, and cannot, is the half a class reads for a family it is *not* — a detector's response
//! to a signal it was not built for. So all four stay in [`OBSERVATION_STATISTICS`], with entries
//! that now say which half is which and give the measurement for each. **An exemption with a
//! narrower guard beside it says more than either alone**, and it is the honest shape here: taking
//! the names off would have required a tolerance wide enough to pass `qam16`'s `blind_qpsk` moving
//! 0.9 → 0.18, which is a rubber stamp, and leaving them off with no assertion would have let the
//! exemption cover the very case T-281 measured.
//!
//! Three of the seven — `cp_corr`, `if_slope_r2` and `carrier_line_db` — have a **null** that moved
//! rather than a value, and on a single waveform a moving null can hide inside the estimator's own
//! spread at the short end. `features::tests::the_null_levels_do_not_move_with_the_record` asserts
//! that property directly, because this file is the wrong instrument for it.
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
//!
//! One thing had to change for that claim to be true, and T-404 found it while clearing the
//! cumulants: the **packet preamble is turned off** (`SynthConfig::packet_preamble`). `synth` gives
//! every FSK generator an alternating preamble over the first 10–35 % of the record, so an N/8
//! prefix of an FSK burst is entirely preamble and the full record is mostly data — two different
//! emissions, not two observations of one, and the cumulants say so loudly. Measured on one 2-FSK
//! waveform with the block fix in: prefixes read `c20_norm` 0.371 / 0.182 / 0.152 / 0.176, the full
//! record standing apart from every rung, where suffixes (all data) read 0.182 / 0.152 / 0.176 with
//! nothing to explain. Removing the confound at its source is not the same as widening a tolerance
//! to cover it; with the preamble left on, the same ladder over the **old** feature code still fails
//! 51 comparisons across the cumulant family and the mode count, so nothing was given up.
//!
//! # The second axis: the receiver (T-429)
//!
//! Everything above holds the SNR at [`SNR_DB`] and varies the record. That is the right choice
//! for isolating the record — and it makes the guard **structurally blind to a feature that
//! measures the SNR rather than the signal**, which passes every run because at a fixed 25 dB
//! there is nothing for it to track. Two such features were found the hard way on one day:
//! `sigma_af` (T-249: `cw` required `sigma_af < 0.02`, written from noiseless physics, against an
//! IF estimator that is noise-limited — the conjunct could not fire below ~28 dB and cost a whole
//! class its top-1) and `duty` (T-427: the fraction of samples over *the snippet's own* mean
//! envelope, which for a sparse train is dominated by the off time, so the noise clears the
//! threshold; fixed at source by T-431). **Both are among the thirty features above, and both
//! pass the length axis.**
//!
//! ## "Does not move with SNR" is the wrong assertion, and the right one is an ordering
//!
//! Every estimator's **variance** grows as the SNR falls: that is physics, not a defect. What must
//! not move is the **expectation** — but measured on this grid, nearly every feature's expectation
//! moves too, and legitimately: the noise is *part of the record*, so a normalised cumulant is
//! attenuated by the noise in its normaliser, a flatness tends to 1, a line-over-floor statistic
//! is an SNR by construction. An assertion of flat expectation would exempt twenty-five features
//! of thirty, which is a rubber stamp.
//!
//! The statement that separates the two, and which both defects violate, is about **order**:
//!
//! > A feature named as a property of the signal may *move* with the SNR. It may not **rank two
//! > emissions one way at one SNR and the other way at another**. A dimension that says a
//! > 5 %-duty radar train is busier than a 33 %-duty PPM train at 15 dB and quieter at 20 dB is
//! > not measuring duty, whatever its spread — and any constant written across it is a constant
//! > on the receiver, not on the transmitter.
//!
//! This is scale-free (no per-feature tolerance is fitted for it at all), it is the *within-family*
//! comparison the class call actually makes, and it distinguishes the two cases the ticket names
//! by construction:
//!
//! - **spread widens, expectation stable** → at low SNR the pair stops being *resolved* and simply
//!   drops out of the comparison. Honest degradation is not a failure.
//! - **expectation slides** → the pair stays resolved and comes back with the opposite sign. The
//!   feature states confidently opposite things about the same two emitters.
//!
//! A pair is **resolved** on a feature at an SNR when the two class means are further apart than
//! [`RESOLVED_SIGMA`] times the spread of the *difference of two single draws*,
//! `sqrt(sd_a² + sd_b²)`. Single draws, not standard errors of the mean: the classifier decides
//! from **one** snippet, so the difference it can act on is one two single snippets would show —
//! and it keeps the criterion independent of how many seeds this file happens to run, which a
//! standard error would not be. Seeds therefore buy **a reliable spread estimate**, not
//! sensitivity: [`SNR_SEEDS`] is 24, where a sample sd carries ~15 % of its own error, and the
//! finding set is measured stable there (identical at 10, 12 and 24 seeds, and identical on a
//! 4-, 5- and 7-rung ladder; at 6 and 8 seeds three further pairs flicker in and out, which is the
//! sd estimate wobbling, not the features moving).
//!
//! ## What it found: seven, and the first two are the two that motivated it — five remain
//!
//! Over 24 features × 28 within-family class pairs × 5 rungs — 854 of 3 238 comparisons resolved —
//! [`SNR_ORDER_EXCEPTIONS`] is the audited list, each entry carrying its mechanism and the measured
//! ladder. `duty` reproduced T-427's published table (`pulse` 0.658 / 0.539 / 0.197 / 0.142 /
//! 0.051 here against their 0.655 / 0.538 / 0.200 / 0.143 / 0.051) and `sigma_af` reproduces
//! T-249's (`cw` 0.206 at 10 dB against their 0.170–0.207, 0.047 at 20 against their 0.038–0.050),
//! which is the check that this is measuring the same thing those two tickets measured.
//!
//! **Two of the seven are gone (T-431), which is the mechanism working.** `duty` and
//! `low_fraction` were the two fixable ones — an envelope threshold referenced to the record's own
//! mean — and referencing them to the emission's own on level instead
//! (`features::on_level`) removed both inversions at source. Five remain, every one of them a
//! statistic that *is* an SNR, where no fix exists short of not ordering on it.
//!
//! The list is asserted as an **exact set**: an inversion that is not declared fails, *and* a
//! declared inversion that no longer reproduces fails. So fixing one makes its exemption disappear
//! from a diff by force rather than by discipline, which is the one improvement this axis makes on
//! the length axis's method.
//!
//! **C14 is not run on this axis.** Its six features (`cyclic_db`, `obw_over_rs` and the four
//! `blind_*` scores) are all exempt in [`NOISE_STATISTICS`] below — a family score is *evidence*,
//! and evidence is supposed to grow with the SNR — and skipping the estimator that feeds only them
//! is what makes 2 520 waveforms cost 8 s instead of 82 s. Measured: with C14 on and off, the other
//! 24 features are **bit-identical** across all 1 764 rows of the probe grid, and the six are the
//! only columns that differ.
//!
//! Unlike the length axis, the **packet preamble stays on** here. It is off above because
//! truncation cuts into it and compares two different emissions; nothing is truncated here, every
//! rung sees the whole record, and `SynthConfig::new` is the geometry the shipped densities are
//! fitted at.

use hk_classify::features::{
    FEATURE_NAMES, FeatureInput, IF_LOCAL_WINDOW, MIN_SAMPLES, feature_fft_len, features,
};
use hk_classify::symbols::SymbolEstimator;
use hk_classify::synth::{Class, DEV_SEEDS, SynthConfig, generate};

/// SNR the **length** axis is measured at: well above every family gate, so what moves is the
/// record length and not the noise.
///
/// Holding this fixed is what isolates the record, and it is why the length axis is structurally
/// blind to a feature that measures the SNR instead of the signal — which is the second axis
/// below (T-429), where this constant becomes the variable and the record length is what is held.
const SNR_DB: f64 = 25.0;

/// Denominators of the truncation ladder: the full record, then halvings. Same ladder T-328 used.
///
/// The full record is whatever [`SynthConfig`]'s default produces — **the geometry the shipped
/// densities are fitted at**, 2328–16 384 normalised samples depending on class, rather than a
/// length chosen to make the ladder convenient. A rung shorter than [`MIN_SAMPLES`] is skipped
/// because `features` abstains wholesale there, and a rung where an individual feature abstains
/// (the spectral four under `FEATURE_MIN_SEGMENTS`, `if_slope_r2` under its window floor) is simply
/// not compared: an abstention is the honest answer, not a failure, and this file never treats it
/// as one.
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

/// 3σ standard error of the **median** of `n` draws of a statistic whose own spread is at most 0.5
/// — which is every statistic bounded to a unit interval. The sample median's asymptotic standard
/// error is `1.253/sqrt(n)` times the draws' standard deviation, so this is `3 · 1.253 · 0.5`.
const K_BOUNDED_MEDIAN: f64 = 1.88;

/// 3σ standard error of the **mean** of `n` draws whose own spread is at most 0.5: `3 · 0.5`. The
/// same bound as [`K_PROPORTION`] and for the same reason, stated separately because what it
/// bounds here is the per-window spread of a mode count over the gap of one whole mode, not the
/// variance of a Bernoulli.
const K_BOUNDED_MEAN: f64 = 1.5;

/// dB per unit relative error, `10/ln 10`. A feature reported in dB moves this much for a given
/// relative error of the ratio underneath it, so its tolerance is this times the relative-error
/// constant of that ratio.
const DB_PER_RELATIVE: f64 = 4.343;

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
         the quantity itself is cumulative. THIS IS THE FIRST OF TWO DEFECTS IN THIS FEATURE AND \
         IT IS THE ONE STILL OPEN: T-447 fixed the SECOND, independent one (the strong-envelope \
         subset it is measured over was selected by 0.5 x the record's own mean, so for a sparse \
         train it was 93% noise BY COUNT). Fixing which samples are read does not stop the walk \
         that already happened in the off gaps before they were read, so this exemption stands.",
    ),
    (
        "sigma_dp",
        "the same unwrapped-phase residual as sigma_ap, before the absolute value - and so the \
         same random walk with the observation, growing as sqrt(record length) rather than \
         settling on a value. Measured 50.9 -> 407.5 over N/8 -> N of one wfm waveform. Carries \
         the same second defect and the same T-447 fix as sigma_ap, and is exempt here for the \
         same first one: with the corrected subset a 5%-duty `pulse` still reads 41-87 rad at \
         10 dB across six seeds, which is the walk and not the subset.",
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
        "obw_over_rs",
        "OBW99 divided by C14's symbol-rate estimate. OBW and the true Rs are both properties of \
         the emission, but the DENOMINATOR is C14's estimate, which inherits the window \
         dependence of `cyclic_db` above - when the winning line changes, Rs jumps by an integer \
         factor. Exempt as a C14 statistic, not as a feature defect: the fix is T-311's, and this \
         entry is here so that fixing it makes an exemption disappear from a diff.",
    ),
    (
        "blind_ook",
        "C14's OOK family score. NOT a defect of length any more, and the measurement says which \
         half is which: where a class BELONGS to the family the score is now flat across the \
         ladder (`ook` and `cw` read 1.00 at N, N/2, N/4 and N/8 on every dev seed, where `cw` \
         used to fall to 0.00 at N/8 off nine keyings), because T-311 gave every veto a transition \
         as wide as its own statistic's 3-sigma and made the score ABSENT below \
         MIN_ENVELOPE_TRANSITIONS instead of a fabricated 0. What remains is the score a class \
         reads for a family it is NOT - a detector's response to a signal it was not built for, \
         which nothing entitles to be stable. Measured: `ppm` reads 1.00 over its full record and \
         0.20 over its last quarter, because its off-level sits at 1.7 noise sigmas over the whole \
         record and 3.7 over a quarter of it. PPM is framed with inter-frame gaps, so a prefix is \
         a different on/off MIXTURE - the same confound as the packet preamble T-404 removed, \
         except that here it is the emission rather than a synth artefact.",
    ),
    (
        "blind_fsk",
        "C14's FSK family score - the one T-281 measured the discontinuity on, at 0.10 / 0.44 / \
         1.00 / 1.00 over N/8..N of one 2-FSK burst. That ladder does NOT reproduce once the \
         packet preamble is off (T-404): `fsk2`, `gfsk` and `msk` read exactly 1.00 at all four \
         rungs on every dev seed, and `fsk4` 0.82-0.93. T-311 found the real mechanism elsewhere - \
         three of the quantities underneath are MAXIMA whose null moves with the record, each \
         compared against a threshold that did not - and fixed it at source. What remains is \
         off-family, as blind_ook: `ppm` at 1.0-1.2x tolerance, from the same frame/gap mixture.",
    ),
    (
        "blind_bpsk",
        "C14's BPSK family score. `bpsk` itself now reads 1.00 at all four rungs on every dev \
         seed. Off-family it still moves: `msk` and `ssb` read 0.01-0.06 over the full record and \
         0.15-0.20 over a half or an eighth of it. The x-squared coherence's maximum-over-bins \
         null is now subtracted in power (T-311) so the null is 0 at every length; what is left is \
         the x-squared line a signal of another family genuinely carries, which depends on which \
         symbols were sent and so on which part of the record was seen. Confirmed not to be the \
         null: estimating the continuum by the periodogram's own median rather than its mean power \
         - a strictly better null for a shaped spectrum - changes the failure count by one \
         comparison in 3446.",
    ),
    (
        "blind_qpsk",
        "C14's QPSK family score, and the one with the most off-family movement: `qpsk` reads 1.00 \
         at all four rungs, while `qam16` and `qam64` move 0.9 -> 0.18 across the ladder. MEASURED \
         to be the emission and not the estimator: a QAM's x-to-the-fourth line is a property of \
         the symbol SEQUENCE, its coherence moves 0.33 -> 0.28 between the full record and an \
         eighth of it - about four times the null's own spread - and the 0.2-wide ramp above the \
         0.2 onset turns that into most of the unit interval. Widening the ramp to the \
         coherence's own 3-sigma changes nothing, because the ramp is already wider than it.",
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
    /// Whole [`IF_LOCAL_WINDOW`]-sample windows of instantaneous frequency the feature takes a
    /// median over. `frequency_features` keeps a pair only when both its samples clear the
    /// Azzouz–Nandi envelope threshold, so the usable count is `n·duty²/IF_LOCAL_WINDOW` — the same
    /// squared duty [`Looks::SamplePairs`] carries, divided by the window rather than by
    /// [`SAMPLES_PER_LOOK`]. These are the aggregators whose error falls with how many *windows*
    /// there were, not with how many samples.
    LocalWindows,
    /// Members of the **smaller** of the two instantaneous-frequency clusters C14's FSK evidence is
    /// formed over — the count the Fisher ratio's denominator, the occupancy, the separation and
    /// the periodicity are every one of them estimated from.
    ///
    /// That count is a property of the emission's symbol rate and keying balance, neither of which
    /// this file knows, so the tolerance uses the floor **the estimator itself enforces**
    /// ([`hk_estimate::blind::family::MIN_CLUSTER_MEMBERS`], below which C14 refuses the candidate
    /// and the feature is absent rather than low). Using the enforced floor rather than the actual
    /// count is the [`MIN_SHAPE_BINS`] precedent: it keeps the tolerance from depending on the
    /// waveform, and it is the conservative end, because every rung that reports at all held at
    /// least that many.
    MinorityCluster,
    /// Envelope level changes C14's OOK evidence is formed over, bounded the same way and for the
    /// same reason by [`hk_estimate::blind::MIN_ENVELOPE_TRANSITIONS`].
    EnvelopeTransitions,
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
        why: "the fraction of samples whose envelope is under 0.3 of the emission's own ON level \
              (T-431; it was 0.3 of the record's mean, which for a mostly-off emission is set by \
              the off time) - a proportion, so its scale is 1 and its error is binomial. A keyed \
              emission's gaps are part of the emission, so what a shorter record may change is \
              only which draws it saw.",
    },
    Rule {
        feature: "duty",
        basis: Basis::Abs(1.0),
        looks: Looks::Samples,
        k: K_PROPORTION,
        why: "fraction of samples above half the emission's own ON level (T-431; it was half the \
              record's mean envelope, which measured the SNR): a proportion, binomial error. \
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
    // ---- The seven T-313 found on its first run, fixed by T-404 and asserted from here on. ----
    Rule {
        feature: "c20_norm",
        basis: Basis::Abs(1.0),
        looks: Looks::Samples,
        k: K_ORDER2,
        why: "|Ĉ20|/C21, now accumulated over CUMULANT_BLOCK-sample blocks and combined \
              incoherently, so the coherence loss a residual carrier offset costs the sum is set by \
              the BLOCK and not by the record (T-404). Bounded to [0, 1] by Cauchy-Schwarz, so 1.0 \
              is its scale; a second-order per-sample moment, so K_ORDER2 over the independent \
              draws the record carries. What is left for this to cover is the residual offset's own \
              estimation error, which falls as 1/sqrt(n) and so cannot outrun a 1/sqrt(n) \
              tolerance.",
    },
    Rule {
        feature: "c40_norm",
        basis: Basis::RelFloor(0.5),
        looks: Looks::Samples,
        k: K_ORDER4,
        why: "|Ĉ40|/C21², the same block-wise sum at four times the residual offset (T-404). A \
              fourth-order moment ratio, hence K_ORDER4, and relative because the feature spans \
              0.05 (noise-like) to 2.4 (cw) and a statistic sitting at 2 is entitled to forty times \
              the absolute wander of one sitting at 0.05. The floor is the gap the dimension exists \
              to resolve — BPSK near 2 against QPSK near 1 against the noise floor — so a waveform \
              passing through zero does not get a tolerance of zero with it.",
    },
    Rule {
        feature: "c42_norm",
        basis: Basis::RelFloor(0.5),
        looks: Looks::Samples,
        k: K_ORDER4,
        why: "Ĉ42/C21². Its own leading term mean(|x|⁴) was always offset-immune; what made it \
              move was the |C20|² it subtracts, which is now the block-wise quantity above — so \
              this recovers with the other two rather than separately, which is what T-313's 93 % \
              attribution predicted. Fourth-order and signed, so the same relative basis and floor \
              as c40_norm.",
    },
    Rule {
        feature: "cp_corr",
        basis: Basis::Abs(1.0),
        looks: Looks::Samples,
        k: K_ORDER2,
        why: "the largest cyclic-prefix autocorrelation peak, each lag now reported as a \
              bias-corrected coherence — (r² − 1/k)/(1 − 1/k) — so the null level a lag with no \
              cyclic prefix behind it reads is 0 at every record length instead of 1/sqrt(k) \
              (T-404). A correlation coefficient, so 1.0 is the scale. The statistic is a \
              DIFFERENCE of two such coherences, each with null standard error 1/sqrt(k), and \
              K_ORDER2 is exactly 3·sqrt(2) — the 3σ allowance for a difference of two. \
              Looks::Samples counts n/4 where the products carry about n/2, which makes the \
              tolerance conservative by sqrt(2) rather than fitted.",
    },
    Rule {
        feature: "if_slope_r2",
        basis: Basis::Abs(1.0),
        looks: Looks::LocalWindows,
        k: K_BOUNDED_MEDIAN,
        why: "R² of a straight-line fit to the smoothed instantaneous frequency, taken as the best \
              median within-window R² over ALL THREE of RAMP_WINDOWS or not at all (T-404) — a \
              maximum over whichever lengths happened to fit was a different estimand at every \
              record length, and biased in the direction that makes the defect. Bounded to [0, 1], \
              so 1.0 is the scale, and what remains is the sampling error of a median over however \
              many 512-sample windows the record held.",
    },
    Rule {
        feature: "if_local_modality",
        basis: Basis::Abs(1.0),
        looks: Looks::LocalWindows,
        k: K_BOUNDED_MEAN,
        why: "the count of instantaneous-frequency modes about the carrier's local trend, read off \
              a kernel-smoothed histogram rather than a raw 48-bin one (T-404), so the estimator no \
              longer invents and loses whole modes as the counts per bin thin out. The scale is ONE \
              WHOLE MODE, which is the gap the dimension exists to resolve: 1 for an angle \
              modulation, 2 for a keyed pair, 4 for a 4-FSK. A MEAN over windows, not a median: a \
              median of integers flips whole as the window count changes parity, which is a \
              reading of the record. So K_BOUNDED_MEAN, and a move of a whole mode fails wherever \
              the record held three windows or more.",
    },
    Rule {
        feature: "carrier_line_db",
        basis: Basis::Abs(DB_PER_RELATIVE),
        looks: Looks::Segments,
        k: K_ORDER2,
        why: "the strongest line's excess over what a band of pure noise would have peaked at, \
              10·log10(1 + peak/median − null) with the null taken from the Wilson-Hilferty \
              quantile of Gamma(M) (T-404). The raw peak-over-median was an order statistic whose \
              upward bias is set by the segment count, which is the one property of the record \
              T-312's transform pin left moving. What remains is the sampling error of the peak bin \
              itself, a second-order statistic over M segments, converted from relative error to dB \
              by DB_PER_RELATIVE.",
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
                // over the Azzouz-Nandi strong-envelope subset. A pulse train at 5 % duty gives a
                // twentieth of the independent looks a continuous emission of the same length
                // does, so its statistics are entitled to sqrt(20) times the wander. `duty` is the
                // feature's own full-record value - a signal property, asserted below in its own
                // right - not a number chosen to make this pass. T-431 made that value the
                // emission's ON fraction rather than the fraction over half the record's mean, so
                // the looks counted here are now looks AT THE EMISSION: at 25 dB `pulse` read
                // 0.142 and reads 0.050, and the 0.092 difference was noise samples over a
                // collapsed threshold, which carry no look at the signal.
                scale * self.k / (n as f64 * duty / SAMPLES_PER_LOOK).sqrt()
            }
            Looks::Segments => scale * self.k / segments(n).max(1.0).sqrt(),
            Looks::SegmentBins => scale * self.k / (segments(n).max(1.0) * MIN_SHAPE_BINS).sqrt(),
            Looks::LocalWindows => {
                let windows = (n as f64 * duty * duty / IF_LOCAL_WINDOW as f64).max(1.0);
                scale * self.k / windows.sqrt()
            }
            Looks::MinorityCluster => {
                scale * self.k / hk_estimate::blind::family::MIN_CLUSTER_MEMBERS.sqrt()
            }
            Looks::EnvelopeTransitions => {
                scale * self.k / (hk_estimate::blind::MIN_ENVELOPE_TRANSITIONS as f64).sqrt()
            }
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
            // **The packet preamble is off** — see [`SynthConfig::packet_preamble`]. It is a
            // deliberate non-stationarity, 10–35 % of the record, and truncating into it compares
            // two different emissions rather than two observations of one.
            let cfg = SynthConfig {
                packet_preamble: false,
                ..SynthConfig::new(SNR_DB, seed)
            };
            let s = generate(*class, &cfg);
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
            let reference = measure(&mut c14, n_full);
            // The emission's own on-fraction, measured over the full record and used below to
            // convert samples into independent looks.
            let duty = reference.get("duty").unwrap_or(1.0).clamp(0.05, 1.0);
            for div in PREFIX_DIVISORS {
                let take = n_full / div;
                if take < MIN_SAMPLES {
                    continue;
                }
                let cut = measure(&mut c14, take);
                for rule in RULES {
                    let (Some(a), Some(b)) = (reference.get(rule.feature), cut.get(rule.feature))
                    else {
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

/// The family a class **belongs to** scores the same however long it is watched (T-311).
///
/// [`OBSERVATION_STATISTICS`] exempts the four `blind_*` scores from the comparison above, and its
/// entries say which half of them the exemption is for: off-family, where a class reads a score for
/// a family it is not, they are a detector's response to a signal it was not built for and nothing
/// entitles them to be stable. **That exemption must not be allowed to cover the case the defect
/// was reported on** — T-281 measured one 2-FSK burst's own `blind_fsk` going 0.10 / 0.44 / 1.00 /
/// 1.00 across the ladder. So this asserts that case directly, and it is why removing four names
/// from the exempt list would have been the *weaker* result: an exemption with a narrower guard
/// beside it says more than either alone.
///
/// Same waveform, same truncation, same derived tolerances as [`RULES`] — only the pairs are
/// narrowed to the family each class is a member of.
const SIGNATURE_FAMILIES: &[(Class, &str)] = &[
    (Class::Ook, "blind_ook"),
    (Class::Cw, "blind_ook"),
    (Class::Fsk2, "blind_fsk"),
    (Class::Gfsk, "blind_fsk"),
    (Class::Msk, "blind_fsk"),
    (Class::Fsk4, "blind_fsk"),
    (Class::Bpsk, "blind_bpsk"),
    (Class::Qpsk, "blind_qpsk"),
];

/// Tolerances for [`SIGNATURE_FAMILIES`], derived exactly as [`RULES`]'s are.
///
/// Each score is a **product** of gated factors, and relative errors add in quadrature, so a
/// product of `F` comparably-noisy factors carries `sqrt(F)` times one factor's 3σ — the count is
/// read off `hk_estimate::blind`'s scoring, not chosen. The scale is 0.5, the gap between a score
/// that says "this family" and one that says "some other family", which is what these dimensions
/// exist to resolve. The look counts are the floors **the estimator itself enforces** below which
/// the feature is absent, which is the [`MIN_SHAPE_BINS`] precedent: conservative, and independent
/// of the waveform.
const SIGNATURE_RULES: &[Rule] = &[
    Rule {
        feature: "blind_ook",
        basis: Basis::Abs(0.5),
        looks: Looks::EnvelopeTransitions,
        // product_of(3): an envelope-contrast ramp, the off-level veto, the FSK competition.
        k: 2.598,
        why: "C14's OOK evidence for a class that IS OOK-keyed. Three gated factors over the \
              MIN_ENVELOPE_TRANSITIONS floor - a contrast between two envelope levels cannot be \
              measured with fewer keyings, and below it the feature is absent rather than a \
              fabricated 0 (T-311).",
    },
    Rule {
        feature: "blind_fsk",
        basis: Basis::Abs(0.5),
        looks: Looks::MinorityCluster,
        // product_of(7): the Fisher ramp, occupancy, separation, valley, periodicity, envelope
        // CV, and the OOK competition.
        k: 3.969,
        why: "C14's FSK evidence for a class that IS frequency-keyed. Seven gated factors over the \
              smaller of the two IF clusters, whose MIN_CLUSTER_MEMBERS floor is where the Fisher \
              ratio's denominator stops being a measurement and the feature goes absent (T-311).",
    },
    Rule {
        feature: "blind_bpsk",
        basis: Basis::Abs(0.5),
        looks: Looks::Samples,
        // product_of(3): the x² coherence ramp and its two ratio vetoes.
        k: 2.598,
        why: "C14's BPSK evidence for a class that IS binary-phase-keyed. Three gated factors over \
              the whole on-record, since a carrier-line coherence is formed from all of it. The \
              coherence is a MAXIMUM over bins, whose null falls as 1/sqrt(n) and is now \
              subtracted in power (T-311), which is what lets it be compared across records.",
    },
    Rule {
        feature: "blind_qpsk",
        basis: Basis::Abs(0.5),
        looks: Looks::Samples,
        // product_of(2): the x⁴ coherence ramp and its one ratio veto.
        k: 2.121,
        why: "C14's QPSK evidence for a class that IS quaternary-phase-keyed. Two gated factors \
              over the whole on-record, bias-corrected against the same maximum-over-bins null as \
              blind_bpsk.",
    },
];

#[test]
fn a_classs_own_family_score_survives_truncation() {
    let mut c14 = SymbolEstimator::default();
    let mut failures: Vec<String> = Vec::new();
    let mut worst: Vec<(f64, String)> = Vec::new();
    let mut compared = 0usize;
    for (class, feature) in SIGNATURE_FAMILIES {
        let rule = SIGNATURE_RULES
            .iter()
            .find(|r| r.feature == *feature)
            .expect("every signature family has a tolerance");
        for seed in DEV_SEEDS.start..DEV_SEEDS.start + 3 {
            let cfg = SynthConfig {
                packet_preamble: false,
                ..SynthConfig::new(SNR_DB, seed)
            };
            let s = generate(*class, &cfg);
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
            let reference = measure(&mut c14, n_full);
            let duty = reference.get("duty").unwrap_or(1.0).clamp(0.05, 1.0);
            let Some(a) = reference.get(feature) else {
                continue;
            };
            for div in PREFIX_DIVISORS {
                let take = n_full / div;
                if take < MIN_SAMPLES {
                    continue;
                }
                // An abstention is the honest answer, never a failure: below the estimator's own
                // floors the feature is ABSENT, which is the whole point of T-311.
                let Some(b) = measure(&mut c14, take).get(feature) else {
                    continue;
                };
                compared += 1;
                let moved = (b - a).abs();
                let tol = rule.tolerance(take, a, duty);
                worst.push((
                    moved / tol.max(f64::MIN_POSITIVE),
                    format!("{feature} {class:?}/s{seed}/N over {div}"),
                ));
                if moved > tol {
                    failures.push(format!(
                        "{feature:<12} {class:?}/seed {seed}: N/{div} ({take} samples) reads \
                         {b:.4} against {a:.4} over the full {n_full}; moved {moved:.4}, \
                         tolerance {tol:.4} ({:.1}x). {}",
                        moved / tol,
                        rule.why,
                    ));
                }
            }
        }
    }
    assert!(
        compared > 40,
        "only {compared} comparisons: the ladder collapsed"
    );
    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!("tightest own-family margins over {compared} comparisons:");
    for (ratio, at) in worst.iter().take(8) {
        eprintln!("  {ratio:>6.2}  {at}");
    }
    assert!(
        failures.is_empty(),
        "{} class(es) scored their OWN family differently for being watched for less time. That \
         is the defect T-281 reported and T-311 fixed; OBSERVATION_STATISTICS covers the \
         off-family half of these scores and must not be widened to cover this half.\n\n{}",
        failures.len(),
        failures.join("\n\n"),
    );
}

// ============================================================================================
// The second axis (T-429): the RECEIVER. The record length is held; the SNR is the variable.
// ============================================================================================

/// SNR rungs, dB. Spans the operating range both defects were measured over — from the lowest
/// family gate (`analog`/`pulsed`/`ofdm`/`css` gate at 10 dB) to 10 dB above the highest
/// (`fsk`/`ook-ask` at 20 dB) — so a crossing anywhere a family is allowed to answer is inside it.
///
/// The finding set is **measured insensitive to the ladder**: a 4-rung (10/15/20/30), this 5-rung
/// and a 7-rung (adding 12.5 and 17.5) ladder return the identical seven inversions at 10, 12 and
/// 24 seeds. Five rungs is the cheapest of the three that still shows where each crossing sits.
const SNR_LADDER: &[f64] = &[10.0, 15.0, 20.0, 25.0, 30.0];

/// Waveforms per class per rung.
///
/// Seeds buy a **reliable spread estimate**, not sensitivity — [`RESOLVED_SIGMA`] is stated over
/// the spread of single draws, which does not shrink with the seed count, so adding seeds cannot
/// make more pairs fail by itself. 24 is where a sample standard deviation carries about 15 % of
/// its own error and the finding set stops moving: identical at 10, 12 and 24 seeds, where at 6
/// and 8 three further pairs flicker in and out (`cyclic_db` analog am/cw, `carrier_line_db`
/// analog cw/ssb, `sigma_af` analog am/nbfm and am/wfm) — the sd estimate wobbling, not the
/// features moving.
const SNR_SEEDS: u64 = 24;

/// How far apart two class means must be, in units of the spread of a **difference of two single
/// draws** `sqrt(sd_a² + sd_b²)`, before this file will say the feature told them apart.
///
/// Single draws rather than standard errors of the mean, for two reasons. The classifier decides
/// from **one** snippet, so the difference it can act on is the one two single snippets would
/// show. And a standard error would make the criterion a function of [`SNR_SEEDS`], so the guard's
/// sensitivity would depend on how long this file is willing to run — exactly the "tolerance
/// fitted to the harness" the length axis's first rule forbids.
///
/// 3 is the same one-sided ~3σ allowance every constant above uses.
const RESOLVED_SIGMA: f64 = 3.0;

/// Waveforms that must report a value at a rung before the rung is used. Below this the class is
/// simply not compared there: an abstention is the honest answer and this file never treats one as
/// a failure (the length axis's rule, unchanged).
const SNR_MIN_PRESENT: usize = 3;

/// Features exempt from the SNR axis **by name**, and why each is a statistic of the noise.
///
/// The length axis's rule holds verbatim: nothing may be added here without saying which property
/// of the receiver it is a statistic of, and every entry carries a measurement. All six are also
/// in [`OBSERVATION_STATISTICS`] — for a *different* mechanism there (they move with the record)
/// than here (they move with the noise), which is why both lists name them rather than one
/// deferring to the other.
///
/// All six come from C14, and **C14 is not run on this axis at all** — which is what makes the
/// 2 520-waveform ladder cost 8 s rather than 82. Measured before relying on it: with the symbol
/// estimator on and off, the other 24 features are bit-identical across all 1 764 rows of the
/// probe grid, and these six are the only columns that differ.
const NOISE_STATISTICS: &[(&str, &str)] = &[
    (
        "cyclic_db",
        "significance of C14's strongest cyclic line, in dB above its own whitened floor. A line's \
         excess over the noise floor is a signal-to-noise ratio BY DEFINITION - it is the quantity \
         a detector thresholds to decide whether the line is there - so it rises with the SNR by \
         construction, and no ordering of two emissions on it is meaningful across rungs. \
         Measured at 6 seeds before it fell below the resolution bound at 24: analog am/cw \
         inverts, am reading 4.6 dB over cw at 10 dB and 3.1 dB under it at 30.",
    ),
    (
        "obw_over_rs",
        "OBW99 over C14's symbol-rate estimate. The numerator is a property of the emission; the \
         DENOMINATOR is an estimate whose winning cyclic line is chosen against a noise-referenced \
         floor, so when the noise changes which line wins, Rs jumps by an integer factor and the \
         ratio with it. Exempt as a C14 statistic, exactly as on the length axis, and for the same \
         reason: the fix is T-311's, and this entry exists so that fixing it removes an exemption.",
    ),
    (
        "blind_ook",
        "C14's OOK family score. A family score is EVIDENCE, and evidence is supposed to grow with \
         the SNR: T-311 gave every veto in it a transition as wide as its own statistic's 3-sigma, \
         and a statistic's 3-sigma is a function of the noise. A detector that answered equally \
         confidently at 10 dB and 30 dB would be the defect. Measured: `ook` reads 0.59 at 10 dB \
         and 1.00 from 20 dB up, the ramp the design asks for.",
    ),
    (
        "blind_fsk",
        "C14's FSK family score, and the same design: seven gated factors over the two \
         instantaneous-frequency clusters, each ramped across its own statistic's spread, which \
         the noise sets. Measured: `ppm`'s off-family read moves 0.18 across the ladder, which is \
         a detector's response to a signal it was not built for at two noise levels - nothing \
         entitles that to be ordered.",
    ),
    (
        "blind_bpsk",
        "C14's BPSK family score. The x-squared carrier-line coherence underneath it is \
         bias-corrected against a null that is itself a noise quantity (T-311), so the score is a \
         calibrated statement ABOUT the noise the line stands in. Same mechanism as blind_ook.",
    ),
    (
        "blind_qpsk",
        "C14's QPSK family score, the x-to-the-fourth form of blind_bpsk and exempt for the same \
         mechanism. Measured on the length axis at 0.9 -> 0.18 off-family; on this axis it is the \
         ramp width that moves, because the ramp is stated in the coherence's own 3-sigma.",
    ),
];

/// One audited exception to [`a_feature_ranks_two_emissions_the_same_way_however_loudly_they_were_heard`].
///
/// Scoped to the **pair**, not the feature: when this axis was built `duty` inverted exactly one
/// of the 28 within-family class pairs, and exempting the whole feature would have thrown away the
/// other 27 pairs' worth of guard. The name being exempted is `feature @ family: a vs b`.
struct OrderException {
    feature: &'static str,
    a: Class,
    b: Class,
    /// The mechanism, and the measured ladder that shows it. Never "what it currently does".
    why: &'static str,
}

/// Every within-family class pair whose order this axis found inverting, with its mechanism.
///
/// **Asserted as an exact set.** An inversion that is not here fails the test; an entry here that
/// no longer reproduces *also* fails it, naming the entry to delete. That is the one thing this
/// axis does better than the length axis's method rather than differently: "fixing one makes the
/// exemption disappear from a diff" stops being a discipline and becomes a failing test.
///
/// Read the grouping before the entries. Four of the five fall into **two mechanisms**: a statistic
/// measured against the noise floor (`carrier_line_db`, `gamma_max`) and a cumulant normalised by
/// the total power (`c42_norm`, twice); `sigma_af` is the fifth, an IF estimator that is
/// noise-limited on a feature named as a frequency excursion. All five are inherent to what the
/// statistic is — an SNR cannot be ordered at two SNRs — which is why they are the ones left.
///
/// **Two entries have been deleted, by this mechanism working as designed (T-431).** `duty` and
/// `low_fraction` were the two defects that motivated this axis, both fractions over an envelope
/// threshold set by the *record's own mean*, which for a mostly-off emission is set by the off
/// time. That was fixable at source and T-431 fixed it: both are now taken against the emission's
/// own on level (`features::on_level`), `pulse` reads 0.050 / 0.950 at every rung from 10 to 30 dB
/// against a true 0.050 / 0.950, and the `ppm`-vs-`pulse` order no longer inverts on either. The
/// exact-set assertion is what made that a completion check rather than a discipline: leaving
/// either entry behind fails the test naming it.
/// **Four entries were deleted by T-564, all of them `am`'s** — `sigma_af` Am-vs-Cw, `gamma_max`
/// Am-vs-Cw, `c42_norm` Am-vs-Cw and `c42_norm` Am-vs-Wfm. They stopped reproducing when the
/// harness stopped re-deriving its analysis geometry from the noisy snippet at every rung, and
/// that is a fix at source, not a weakened guard: `am` was the worst case of the defect. 99 % of
/// its power is its carrier, so the old noise-referenced OBW99 read 61 kHz for a 9 kHz emission at
/// 10 dB and collapsed onto the carrier bin once the noise fell (T-435's ladder: 82 / 31 / 1 / 1 /
/// 1 / 1 / 1 kHz). `am` was therefore delivered in a five-times-too-wide band at the bottom of the
/// ladder and a hundred-times-too-narrow one at the top. **Most of what these four entries
/// recorded as an inherent SNR law was `am` being handed a different filter at each rung.**
///
/// Re-measured on the corrected grid, 6 seeds at 10/15/20/25/30 dB, each pair now ordered the same
/// way at **every** rung:
///
/// - `sigma_af`: `am` 0.3181 / 0.1832 / 0.1042 / 0.0589 / 0.0332 against `cw` 0.1540 / 0.0857 /
///   0.0480 / 0.0270 / 0.0152 — both still slide about 1/sqrt(rho), which is the real law T-249
///   found; what vanished is the *crossing* (`am` used to run 1.030 at 10 dB and 0.004 at 30 dB,
///   a 264-fold move, and dived under `cw`).
/// - `gamma_max`: `am` 75.0 / 122.4 / 153.0 / 165.9 / 170.5 against `cw` 53.6 / 58.9 / 60.7 /
///   61.4 / 61.6 (`am` used to start at 11.2, far below `cw`).
/// - `c42_norm`: `am` -1.480 / -1.749 / -1.851 / -1.886 / -1.897 against `cw` -1.245 / -1.399 /
///   -1.454 / -1.473 / -1.479 and `wfm` -0.934 / -1.011 / -1.038 / -1.047 / -1.050.
///
/// The estimators were not touched. What changed is that the emission they were measured on is now
/// the same emission at every rung.
const SNR_ORDER_EXCEPTIONS: &[OrderException] = &[
    OrderException {
        feature: "if_modality",
        a: Class::Fsk2,
        b: Class::Fsk4,
        why: "INHERENT to a MODE COUNT, and surfaced by T-564 rather than caused by it. `if_modality` counts resolvable modes in the instantaneous-frequency histogram, and four tones can only be counted once the noise is small enough to separate them: below that SNR a 4-FSK genuinely PRESENTS as one broad mode, and no estimator reading only this record can say otherwise. Measured ladder, 6 seeds at 10/15/20/25/30 dB: `4fsk` 1.0000 +- 0.0000, 2.5000 +- 1.1180, 4.0000 +- 0.0000, 4.0000 +- 0.0000, 4.0000 +- 0.0000 - a 1 -> 4 climb as the tones resolve - against `2fsk` flat at 2.0000 +- 0.0000 (2.3333 +- 0.7454 at 30 dB), `gfsk` 1.8333 +- 0.3727 then 2.0000 +- 0.0000, and `msk` 2.0000 +- 0.0000 (2.5000 +- 1.1180 at 30 dB). Each of the three crosses `4fsk` between 10 and 20 dB, where it reads FEWER modes than a 2-level emission. It became visible when T-564 stopped the analysis geometry moving with the noise: the old grid widened the channel at low SNR, decimated the snippet less, and left `4fsk`'s IF histogram wide enough to keep more than one mode. That was the harness hiding an SNR dependence of the statistic, not the statistic being SNR-independent. What is forbidden remains a CONSTANT across it: no rule may test `if_modality` against a fixed number without bounding the SNR it was written at.",
    },
    OrderException {
        feature: "if_modality",
        a: Class::Gfsk,
        b: Class::Fsk4,
        why: "INHERENT to a MODE COUNT, and surfaced by T-564 rather than caused by it. `if_modality` counts resolvable modes in the instantaneous-frequency histogram, and four tones can only be counted once the noise is small enough to separate them: below that SNR a 4-FSK genuinely PRESENTS as one broad mode, and no estimator reading only this record can say otherwise. Measured ladder, 6 seeds at 10/15/20/25/30 dB: `4fsk` 1.0000 +- 0.0000, 2.5000 +- 1.1180, 4.0000 +- 0.0000, 4.0000 +- 0.0000, 4.0000 +- 0.0000 - a 1 -> 4 climb as the tones resolve - against `2fsk` flat at 2.0000 +- 0.0000 (2.3333 +- 0.7454 at 30 dB), `gfsk` 1.8333 +- 0.3727 then 2.0000 +- 0.0000, and `msk` 2.0000 +- 0.0000 (2.5000 +- 1.1180 at 30 dB). Each of the three crosses `4fsk` between 10 and 20 dB, where it reads FEWER modes than a 2-level emission. It became visible when T-564 stopped the analysis geometry moving with the noise: the old grid widened the channel at low SNR, decimated the snippet less, and left `4fsk`'s IF histogram wide enough to keep more than one mode. That was the harness hiding an SNR dependence of the statistic, not the statistic being SNR-independent. What is forbidden remains a CONSTANT across it: no rule may test `if_modality` against a fixed number without bounding the SNR it was written at.",
    },
    OrderException {
        feature: "if_modality",
        a: Class::Msk,
        b: Class::Fsk4,
        why: "INHERENT to a MODE COUNT, and surfaced by T-564 rather than caused by it. `if_modality` counts resolvable modes in the instantaneous-frequency histogram, and four tones can only be counted once the noise is small enough to separate them: below that SNR a 4-FSK genuinely PRESENTS as one broad mode, and no estimator reading only this record can say otherwise. Measured ladder, 6 seeds at 10/15/20/25/30 dB: `4fsk` 1.0000 +- 0.0000, 2.5000 +- 1.1180, 4.0000 +- 0.0000, 4.0000 +- 0.0000, 4.0000 +- 0.0000 - a 1 -> 4 climb as the tones resolve - against `2fsk` flat at 2.0000 +- 0.0000 (2.3333 +- 0.7454 at 30 dB), `gfsk` 1.8333 +- 0.3727 then 2.0000 +- 0.0000, and `msk` 2.0000 +- 0.0000 (2.5000 +- 1.1180 at 30 dB). Each of the three crosses `4fsk` between 10 and 20 dB, where it reads FEWER modes than a 2-level emission. It became visible when T-564 stopped the analysis geometry moving with the noise: the old grid widened the channel at low SNR, decimated the snippet less, and left `4fsk`'s IF histogram wide enough to keep more than one mode. That was the harness hiding an SNR dependence of the statistic, not the statistic being SNR-independent. What is forbidden remains a CONSTANT across it: no rule may test `if_modality` against a fixed number without bounding the SNR it was written at.",
    },
    OrderException {
        feature: "carrier_line_db",
        a: Class::Ppm,
        b: Class::Pulse,
        why: "INHERENT, and the feature's own definition says so: T-404 defines it as the \
              strongest line's excess over the peak a band of PURE NOISE would have shown at that \
              segment count. An excess over the noise is an SNR, so raising the SNR raises it, and \
              two emissions can only be ordered on it at a fixed noise level. Measured: pulse \
              20.99 / 25.37 / 28.42 / 29.88 / 30.56 dB - +9.6 dB over 20 dB of SNR, the line \
              rising against a fixed floor - against ppm flat at 23.0-23.6, whose strongest line \
              is its frame rate rather than a carrier and so sits at a fixed distance from its own \
              sidebands. Not fixable without changing what the dimension means.",
    },
];

/// One class's reading of one feature at one rung: mean and sample standard deviation over
/// [`SNR_SEEDS`] waveforms, or `None` where fewer than [`SNR_MIN_PRESENT`] of them reported a
/// value (an abstention is the honest answer and is never compared).
type MeanSd = Option<(f64, f64)>;

/// Mean and sample (n-1) standard deviation.
fn mean_sd(v: &[f64]) -> (f64, f64) {
    let n = v.len() as f64;
    let m = v.iter().sum::<f64>() / n;
    let var = v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1.0);
    (m, var.sqrt())
}

/// Every feature is either asserted on the SNR axis or named as a noise statistic, and nothing is
/// silently neither — the completeness rule of the length axis, applied to the second axis.
#[test]
fn every_feature_is_either_snr_order_preserving_or_a_named_noise_statistic() {
    // The asserted set is "every feature not exempt by name", so on this axis a new dimension is
    // guarded BY DEFAULT rather than landing unclassified — the opposite default from the length
    // axis, and the safe one here: a feature added without a thought about the receiver gets
    // asserted, and the thing that has to appear in a diff is the *exemption*. There is therefore
    // no completeness hole to check, and the checks below are on the exemption lists themselves.
    for (i, (name, why)) in NOISE_STATISTICS.iter().enumerate() {
        assert!(
            !NOISE_STATISTICS[..i].iter().any(|(n, _)| n == name),
            "{name} is exempt from the SNR axis twice: two mechanisms for one name means one of \
             them was never checked"
        );
        assert!(
            FEATURE_NAMES.contains(name),
            "{name} is exempt from the SNR axis but is not a feature: delete the exemption"
        );
        assert!(
            why.len() > 80,
            "{name} is exempt from the SNR axis without saying which property of the NOISE it is a \
             statistic of. An exemption without a measurement is not an exemption."
        );
    }
    for e in SNR_ORDER_EXCEPTIONS {
        assert!(
            FEATURE_NAMES.contains(&e.feature),
            "{} has an order exception but is not a feature",
            e.feature
        );
        assert!(
            !NOISE_STATISTICS.iter().any(|(n, _)| *n == e.feature),
            "{} is both exempt wholesale and has a per-pair exception: pick one",
            e.feature
        );
        assert!(
            e.a.family() == e.b.family() && e.a.family().is_some(),
            "{}: {:?} and {:?} are not in one family, so no within-family call compares them",
            e.feature,
            e.a,
            e.b
        );
        assert!(
            e.why.len() > 80,
            "{} {:?}/{:?} is excepted without a mechanism and a measurement",
            e.feature,
            e.a,
            e.b
        );
    }
}

/// The guard: a feature may move with the SNR, but it may not **reorder two emissions** as the SNR
/// changes. See the module header for why this, and not flat expectation, is the assertion.
#[test]
fn a_feature_ranks_two_emissions_the_same_way_however_loudly_they_were_heard() {
    let asserted: Vec<&str> = FEATURE_NAMES
        .iter()
        .copied()
        .filter(|n| !NOISE_STATISTICS.iter().any(|(x, _)| x == n))
        .collect();

    // [class][rung][feature] -> (mean, sd) over SNR_SEEDS waveforms, or None where fewer than
    // SNR_MIN_PRESENT of them reported a value.
    let mut stats: Vec<Vec<Vec<MeanSd>>> = Vec::new();
    for class in Class::TAXONOMY {
        let mut per_rung = Vec::new();
        for snr in SNR_LADDER {
            let mut draws: Vec<Vec<f64>> = vec![Vec::new(); asserted.len()];
            for seed in DEV_SEEDS.start..DEV_SEEDS.start + SNR_SEEDS {
                // The packet preamble stays ON: nothing is truncated here, so it is not a confound,
                // and `SynthConfig::new` is the geometry the shipped densities are fitted at.
                let s = generate(*class, &SynthConfig::new(*snr, seed));
                // `symbols: None` — C14 is not run; its six features are exempt by name in
                // NOISE_STATISTICS and are the ONLY columns that change when it is omitted.
                let f = features(&FeatureInput {
                    samples: &s.samples,
                    sample_rate_hz: s.sample_rate_hz,
                    obw_hz: Some(s.obw_hz),
                    snr_db: Some(*snr),
                    symbols: None,
                });
                for (i, name) in asserted.iter().enumerate() {
                    if let Some(v) = f.get(name) {
                        draws[i].push(v);
                    }
                }
            }
            per_rung.push(
                draws
                    .iter()
                    .map(|d| (d.len() >= SNR_MIN_PRESENT).then(|| mean_sd(d)))
                    .collect(),
            );
        }
        stats.push(per_rung);
    }

    // Within-family class pairs: the comparison a within-family class call actually makes.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..Class::TAXONOMY.len() {
        for j in i + 1..Class::TAXONOMY.len() {
            let (a, b) = (Class::TAXONOMY[i], Class::TAXONOMY[j]);
            if a.family().is_some() && a.family() == b.family() {
                pairs.push((i, j));
            }
        }
    }

    let mut compared = 0usize;
    let mut resolved = 0usize;
    let mut found: Vec<(&str, usize, usize, String)> = Vec::new();
    for (fi, name) in asserted.iter().enumerate() {
        for &(i, j) in &pairs {
            let mut signs: Vec<(f64, i8)> = Vec::new();
            let mut table = String::new();
            for (r, snr) in SNR_LADDER.iter().enumerate() {
                let (Some((ma, sa)), Some((mb, sb))) = (stats[i][r][fi], stats[j][r][fi]) else {
                    continue;
                };
                compared += 1;
                let apart = ma - mb;
                let spread = RESOLVED_SIGMA * (sa * sa + sb * sb).sqrt();
                table.push_str(&format!(
                    "\n      {snr:>4.0} dB  {:>10.4} +- {:<8.4} vs {:>10.4} +- {:<8.4}  apart \
                     {apart:>9.4}, resolved at {spread:.4}{}",
                    ma,
                    sa,
                    mb,
                    sb,
                    if apart.abs() > spread { "  <-" } else { "" },
                ));
                if apart.abs() > spread {
                    resolved += 1;
                    signs.push((*snr, if apart > 0.0 { 1 } else { -1 }));
                }
            }
            if signs.iter().any(|(_, s)| *s > 0) && signs.iter().any(|(_, s)| *s < 0) {
                found.push((*name, i, j, table));
            }
        }
    }

    // The ladder must still be a ladder: if generation or the resolution bound collapsed, an empty
    // finding set would look like a pass.
    assert!(
        compared > 3000 && resolved > 700,
        "only {compared} comparisons and {resolved} resolved: the SNR ladder collapsed, so an \
         empty result would mean nothing"
    );
    eprintln!(
        "SNR axis: {} features x {} within-family pairs x {} rungs, {compared} comparisons, \
         {resolved} resolved, {} inversions",
        asserted.len(),
        pairs.len(),
        SNR_LADDER.len(),
        found.len(),
    );

    // Exact-set comparison, both ways.
    let mut undeclared = Vec::new();
    let mut matched = vec![false; SNR_ORDER_EXCEPTIONS.len()];
    for (name, i, j, table) in &found {
        let (a, b) = (Class::TAXONOMY[*i], Class::TAXONOMY[*j]);
        match SNR_ORDER_EXCEPTIONS
            .iter()
            .position(|e| e.feature == *name && ((e.a == a && e.b == b) || (e.a == b && e.b == a)))
        {
            Some(k) => matched[k] = true,
            None => undeclared.push(format!(
                "{name:<20} {} {a:?} vs {b:?}: ranks them one way at one SNR and the other way at \
                 another.{table}",
                a.family().unwrap_or("?"),
            )),
        }
    }
    let stale: Vec<String> = SNR_ORDER_EXCEPTIONS
        .iter()
        .zip(&matched)
        .filter(|(_, m)| !**m)
        .map(|(e, _)| format!("{:<20} {:?} vs {:?}", e.feature, e.a, e.b))
        .collect();

    assert!(
        undeclared.is_empty(),
        "{} feature(s) named as a property of the signal REORDER two emissions of one family as \
         the SNR changes. The feature is reading the noise, and any constant written across it is \
         a constant on the receiver (T-249's `sigma_af < 0.02`, T-427's `duty > 0.25`). Either fix \
         the estimator, or add it to SNR_ORDER_EXCEPTIONS with the mechanism AND the measured \
         ladder - an exemption without a measurement is not an exemption.\n\n{}",
        undeclared.len(),
        undeclared.join("\n\n"),
    );
    assert!(
        stale.is_empty(),
        "{} declared SNR order exception(s) no longer reproduce. If the estimator was fixed, \
         DELETE the entry - that is the point of listing them by name. If it was not, something \
         moved the resolution bound and the guard just got weaker without anyone deciding to \
         weaken it.\n\n{}",
        stale.len(),
        stale.join("\n"),
    );
}
