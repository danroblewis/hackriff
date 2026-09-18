//! **Harmonic families: several emitters against one fundamental nobody can see** (C40, T-374).
//!
//! [`crate::relate`] reasons about **one** emitter against the device: this box is the image,
//! harmonic or intermodulation product of *that* confirmed row, through *this* mixer (T-219,
//! T-302). That is the whole of it — and it cannot state the thing T-317 had to work out by hand
//! across four captures: that 100.465339 MHz, 102.801 MHz and 105.138 MHz are **harmonics 43, 44
//! and 45 of a free-running ~2.3364 MHz oscillator that was never itself detected**. There is no
//! source row to point at, because the fundamental sits outside every band that was ever tuned.
//!
//! So this module fits `f = n·f₀ + b` over a *set* of measured emitters and decides whether the
//! set is a family. T-317's method is the specification, and the reasoning is device-local for
//! exactly the reason [`crate::relate`]'s is (T-259/T-302/T-305): a harmonic family is a property
//! of **one receive chain plus one emitter**, never of shared air. Every member must have been
//! measured on one [`ReceiveChain`], and [`find_harmonic_families`] refuses a set that was not.
//!
//! # Blind, like everything else
//!
//! Nothing here reads a catalogue. `f₀` comes out of the measured centres; the indices come out of
//! the fit; the corroboration comes out of the measured widths. A band plan may later *suggest* a
//! name for the oscillator, and it may never supply one.
//!
//! # The four tests, and what each is a property of
//!
//! The honest accounting matters more than the count, because **a good fit is a property of the
//! numbers you chose to fit, not of the emitters being related** — a search over enough index
//! labellings will always find some `f₀`.
//!
//! 1. **The residual** (`f = n·f₀ + b` fitted unconstrained over the assigned indices). This is a
//!    property of the *frequency* measurements and of the search bounds. Its teeth come entirely
//!    from [`MAX_INDEX`] and from the tolerance being **absolute** — derived from each emitter's
//!    own measured width, never from `f₀` — so the acceptance window cannot grow with the order.
//!    (The same failure [`crate::relate::ARTIFACT_CENTER_BW_FRACTION`] documents: a tolerance that
//!    scales with the prediction eventually fits everything.)
//!
//! 2. **The intercept pins the indices.** Shifting every index by one leaves the least-squares
//!    slope and **every residual bit-identical**, and moves the intercept by exactly one `f₀`
//!    (`b → b − f₀`; see [`HarmonicFamily::off_by_one`], and the test that asserts it). So the
//!    residual can never choose between labellings — only the physics can, and the physics is that
//!    a harmonic family passes through the **origin**: `b = 0`. Two things follow, and only the
//!    second is evidence:
//!    - `|b| ≲ se(b)` — the grid passes through the origin at the precision the data supports.
//!      This is **partly selected for**, because the grouping step assigns indices under the
//!      through-origin hypothesis; it is reported, not leaned on.
//!    - `se(b)/f₀ ≪ ½` ([`INDEX_PIN_MAX`]) — the measurement's own precision, extrapolated down to
//!      zero frequency, is small compared with a whole fundamental. **This is the non-vacuity
//!      condition**, and it is not selected for by anything: the grouping bounds each residual by
//!      the tolerance and says nothing about whether the tolerance is small next to `f₀`. When it
//!      fails, the integer labelling is undetermined — every set of frequencies is then a
//!      "harmonic family" of *some* assignment — and the verdict is refused however good the
//!      residual looks.
//!
//! 3. **Width ∝ n — the one piece of genuinely independent corroboration.** A fundamental's
//!    frequency noise multiplies with the harmonic number, so harmonic `n` of an oscillator with
//!    `σ₀` of it is `n·σ₀` wide. The widths are a **different column**, never in the fit's
//!    objective and never used to choose an index, so `wᵢ/nᵢ` being constant is a genuine
//!    out-of-sample prediction of the labelling. T-317 measured 138 / 120 / 137 Hz at n = 43, 44,
//!    45. Any consistent width measure works (rms, −3 dB, occupied): all of them scale linearly
//!    with the frequency-noise scale. **It must be the same measure for every member.**
//!
//!    How much it discriminates depends on the **index leverage** `n_max/n_min`, and this module
//!    says so rather than claiming more than it has: at T-317's 45/43 = 1.05 the prediction
//!    `w ∝ n` differs from `w = const` by 5 %, which the 15 % measured scatter cannot resolve, so
//!    there the test confirms the *scale* (28 kHz is 43 × 134 Hz) without separating the two
//!    models. [`WidthEvidence::separates`] is true only when the leverage is large enough for the
//!    distinction to mean anything.
//!
//! 4. **Line shape** (optional). Harmonics of one oscillator carry one line shape, scaled by `n`.
//!    Supplied profiles are compared pairwise after that scaling; T-317 measured 0.967–0.988. Like
//!    the width, this comes from a column the fit never touched — and when profiles are supplied
//!    they can only ever **reject** (a family is never *created* by a shape agreeing), because
//!    evidence that can only merge is not evidence (T-233).
//!
//! # It must be able to say no
//!
//! Every gate above rejects, and each rejection is a distinct [`FamilyRejection`] naming what
//! failed, so a "no" is as legible as a "yes". The negative control is in this module's tests:
//! real unrelated emitters, and randomly drawn populations, must not be declared families.

use serde::{Deserialize, Serialize};

use crate::ids::EmitterId;
use crate::relate::ReceiveChain;

// ---------------------------------------------------------------------------------------------
// Thresholds. All a priori: fixed here, never fitted to a fixture.
// ---------------------------------------------------------------------------------------------

/// Fewest members a family may have.
///
/// Two points and two free parameters (`f₀`, `b`) leave no degrees of freedom: the fit is exact,
/// the residual is identically zero, and nothing has been tested. Three is the first count at
/// which the arithmetic can fail — and it is what T-317 had (n = 43, 44, 45). The chance of a
/// spurious fit falls steeply with each further member: the best residual a search over `M`
/// labellings can reach on unrelated frequencies scales as `M^(−1/(k−1))·f₀/2`, so k = 4 is an
/// order of magnitude harder to fake than k = 3.
pub const MIN_MEMBERS: usize = 3;

/// Lowest harmonic index a member may carry.
///
/// `n = 1` is the fundamental itself, and a **visible** fundamental is [`crate::relate`]'s case,
/// not this one: `predict_artifacts` already attributes `n·f` to a confirmed source row. This
/// module exists for the fundamental nobody can see.
pub const MIN_INDEX: u32 = 2;

/// Highest harmonic index searched.
///
/// **This bound is what stops the mechanism being vacuous**, and it is the only place the search
/// space is set. Raising it lowers the smallest `f₀` reachable (`f_max/MAX_INDEX`), which shrinks
/// the residual a chance fit can reach in direct proportion — the negative control in this
/// module's tests prices exactly that. 64 is generous against T-317's 45 and still leaves a
/// fundamental two orders of magnitude above the tolerance at VHF.
pub const MAX_INDEX: u32 = 64;

/// Residual tolerance as a fraction of a member's **own measured width**.
///
/// From the measurement, never from the prediction: a tolerance derived from `n·f₀` would widen
/// with the order until every frequency fit some family, which is the failure
/// [`crate::relate::ARTIFACT_CENTER_BW_FRACTION`] records.
///
/// **It can only tighten** [`RESIDUAL_PPM`], never loosen it — see [`residual_tolerance_hz`], and
/// see the measurement that forced the `min` there. T-317's fit sits at 0.036 of its narrowest
/// member's width.
pub const RESIDUAL_WIDTH_FRACTION: f64 = 0.10;

/// Residual tolerance as ppm of the highest member frequency: how closely two centres measured
/// through one front end at one time can be placed against each other.
///
/// **This is the family-agnostic half of the tolerance, and it is the half that does the work.**
/// An earlier draft used the width fraction alone, reasoning that an oscillator harmonic's width
/// *is* its frequency noise so the line wanders by about its own width. That is true — and it
/// assumes the conclusion: it is a property only a member of a real family has. Applied to a
/// 180 kHz broadcast station it granted a 15 kHz window to a centre that sits on a 100 kHz raster
/// and is measurable to a kilohertz, and this module's own negative control then declared
/// **55.5 %** of randomly drawn, wholly unrelated FM-band populations to be harmonic families.
/// A ppm-of-frequency tolerance assumes nothing about the set (it is the same form as
/// [`crate::relate::center_uncertainty_hz`]), and with the `min` below the rate fell to the figure
/// recorded in `harmonic_tests`. The common-mode part of a front end's clock error is far larger
/// than 5 ppm, but it scales every member alike and the fitted slope absorbs it; what this bounds
/// is the *differential* error between members of one set.
pub const RESIDUAL_PPM: f64 = 5.0;

/// How much slack the **grouping** step allows over [`residual_tolerance_hz`] before a member is
/// admitted to a proposal.
///
/// Grouping anchors the grid on one member (`f₀ = fᵢ/nᵢ`), which forces that member's error to
/// zero and pushes the set's whole error onto the others — roughly doubling what the fitted line
/// would show. Judging at the anchored error would therefore lose real families: T-317's own
/// members sit 409 Hz off an anchored grid and 191 Hz rms off the fitted one. The slack widens
/// only which subsets are **proposed**; every proposal is still judged at the strict tolerance, so
/// this cannot loosen a claim. Same shape as `hk_detect::comb`, which refines its spacing by least
/// squares on the members and recounts.
pub const GATHER_SLACK: f64 = 3.0;

/// Floor under the residual tolerance, Hz, so a very narrow emission is not held to a tolerance
/// finer than a centre can be measured to.
pub const RESIDUAL_MIN_HZ: f64 = 50.0;

/// Largest `se(b)/f₀` at which the integer labelling is still determined.
///
/// At ½ the labelling is a coin flip between `n` and `n ± 1` and the family means nothing. 0.1
/// puts the neighbouring labelling ten standard errors away. **This is the non-vacuity gate**: it
/// is the one quantity in the frequency fit that the grouping step does not select for, because
/// the grouping bounds each residual by the tolerance and says nothing about whether the tolerance
/// is small next to `f₀`.
///
/// **Under the shipped constants it is also provable, and that is the point.** Combining
/// [`RESIDUAL_PPM`] with [`MAX_INDEX`] bounds it: with `σ̂ ≤ residual_rms·√(k/(k−2))`,
/// `residual_rms ≤ RESIDUAL_PPM·f_max` and `f₀ ≈ f_max/n_max`, the worst reachable value is
/// `RESIDUAL_PPM · √(k/(k−2)) · √(1/k + n̄²/Sₙₙ) · n_max`, maximised at k = 3 over the top three
/// indices: **0.0247**. So every set that clears the residual gate is pinned by construction, and
/// this gate fires only on sets the residual would have refused anyway — where it is the better
/// explanation. Its standing job is to catch a future widening of [`RESIDUAL_PPM`] or a raise of
/// [`MAX_INDEX`] that would make the labelling free again. `harmonic_tests` enumerates the bound
/// rather than asserting it.
pub const INDEX_PIN_MAX: f64 = 0.1;

/// How many standard errors the intercept may sit from the origin.
///
/// On its own this test is **self-scaling and therefore weak**: a sloppy fit has a large `se(b)`,
/// and a large `se(b)` makes any intercept "consistent with zero". The negative control caught it
/// doing exactly that — a spurious family whose grid missed the origin by 174 kHz passed at
/// 1.65 sigma because its intercept carried 105 kHz of standard error. So it is paired with
/// [`ORIGIN_MAX_FRACTION`], which has no such escape.
pub const ORIGIN_MAX_SIGMA: f64 = 3.0;

/// Largest `|b|/f₀` a harmonic family may show: how far the grid may miss the origin as a share of
/// one fundamental.
///
/// The claim being made is that these emitters are `n × f₀` — through the origin, with no offset.
/// A grid that misses by an eighth of a fundamental is an arithmetic comb offset from DC, which is
/// a different mechanism (a mixing-product grid), whatever its standard error says. T-317 misses
/// by 3.0e-5 of a fundamental, so 0.01 admits it with 330x of margin while rejecting the 0.077 the
/// negative control's best spurious fit reached.
pub const ORIGIN_MAX_FRACTION: f64 = 0.01;

/// Largest spread (max ÷ min) allowed in `wᵢ/nᵢ` across the members.
///
/// T-317 measured 1.15 (138 / 120 Hz). 2.0 admits ordinary width-measurement scatter and rejects
/// a set whose widths contradict `w ∝ n` outright — a 180 kHz broadcast station sitting on the
/// grid beside a 6 kHz line is not a harmonic of the same oscillator.
pub const WIDTH_RATIO_MAX: f64 = 2.0;

/// Index leverage (`n_max/n_min`) above which `w ∝ n` is distinguishable from `w = const`.
///
/// Below it the two models predict widths within `(leverage − 1)` of each other — 5 % at T-317's
/// 45/43 — which no real width measurement resolves. [`WidthEvidence::separates`] reports the
/// distinction honestly rather than counting a non-test as corroboration.
pub const WIDTH_LEVERAGE_MIN: f64 = 1.5;

/// Smallest pairwise line-shape correlation accepted when profiles are supplied. T-317 measured
/// 0.967–0.988.
pub const LINE_SHAPE_MIN_CORR: f64 = 0.9;

/// Floor under the fitted residual scale, Hz, so a synthetically exact set does not divide by
/// zero. No centre is measured better than this.
pub const CENTER_PRECISION_MIN_HZ: f64 = 1.0;

/// `true` when `x` is finite and at or below `limit`.
///
/// **NaN fails closed**, which is the whole reason the gates below are written as `!within(..)`
/// rather than `x > limit`: an unmeasurable statistic is not a passing one. Same principle as
/// `BiasTee::Unknown` is not `Off`.
fn within(x: f64, limit: f64) -> bool {
    x.is_finite() && x <= limit
}

/// `true` when `x` is finite and at or above `limit`. NaN fails closed, as above.
fn at_least(x: f64, limit: f64) -> bool {
    x.is_finite() && x >= limit
}

/// `true` when `x` is finite and strictly positive. NaN fails closed, as above.
fn positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// One measured emitter offered as a possible family member. Everything here is measured; nothing
/// comes from a database of known signals.
#[derive(Clone, Debug, PartialEq)]
pub struct FamilyMember {
    /// The inventory row.
    pub emitter_id: EmitterId,
    /// Measured centre, Hz.
    pub f_center_hz: f64,
    /// Measured width, Hz — rms, −3 dB or occupied, but **the same measure for every member**,
    /// since the corroboration is that `width/n` is constant and a mixed measure would make that
    /// meaningless.
    pub width_hz: f64,
    /// The receive chains this emitter was actually measured on (T-302). A family lives on one
    /// chain; a row seen only on another front end can never be a harmonic of an oscillator in
    /// this one.
    pub chains: Vec<ReceiveChain>,
    /// Optional normalised line-shape profile, already resampled onto this member's **own**
    /// width-scaled offset axis (so harmonic `n`'s `n`-fold wider line lands on the same grid as
    /// the others). Same length for every member that supplies one.
    pub line_shape: Option<Vec<f64>>,
}

// ---------------------------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------------------------

/// Why a set is **not** a harmonic family. Every gate has one, so a "no" is as legible as a "yes".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "rejected")]
pub enum FamilyRejection {
    /// Fewer than [`MIN_MEMBERS`] rows, or fewer than two distinct indices.
    TooFewMembers,
    /// A member's centre, width or index is missing, zero or not finite.
    Unmeasured,
    /// Two members carry the same harmonic index: one grid slot cannot hold two emissions.
    DuplicateIndex,
    /// Two members' measured bands overlap. Lines inside **one** emission are a comb
    /// (`hk_detect::comb`), not a family of emitters.
    OverlappingMembers,
    /// The members were not all measured on one receive chain (T-302): a harmonic family is a
    /// property of one front end, never of shared air.
    ChainsDiffer,
    /// An index outside `MIN_INDEX..=MAX_INDEX`.
    IndexOutOfRange,
    /// The measured centres do not sit on `n·f₀ + b` within the tolerance their own widths set.
    ResidualTooLarge,
    /// **The non-vacuity gate.** `se(b)/f₀` is not small enough for the integer labelling to be
    /// determined: `n` and `n ± 1` fit equally well, so there is no family to name.
    IndicesNotPinned,
    /// The grid does not pass through the origin: it is an arithmetic comb offset from DC, not
    /// harmonics of a fundamental.
    NotThroughOrigin,
    /// `width/n` is not constant across the members — the widths contradict `w ∝ n`.
    WidthsInconsistent,
    /// Supplied line shapes disagree, or only some members supplied one.
    LineShapesDisagree,
}

impl FamilyRejection {
    /// Rendered for display. Always disclosed, like every other claim in the model.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::TooFewMembers => "fewer than three emitters on two or more distinct indices",
            Self::Unmeasured => "a member has no finite measured centre, width or index",
            Self::DuplicateIndex => "two members claim the same harmonic index",
            Self::OverlappingMembers => {
                "two members' measured bands overlap: lines inside one emission are a comb, \
                 not a family of emitters"
            }
            Self::ChainsDiffer => {
                "the members were not all measured on one receive chain; a harmonic family is a \
                 property of one front end, never of shared air"
            }
            Self::IndexOutOfRange => "a harmonic index outside the searched range",
            Self::ResidualTooLarge => {
                "the measured centres do not sit on n x f0 + b within the tolerance their own \
                 widths set"
            }
            Self::IndicesNotPinned => {
                "the indices are not pinned: the intercept's standard error is comparable with a \
                 whole fundamental, so n and n +/- 1 fit equally well and no labelling is claimed"
            }
            Self::NotThroughOrigin => {
                "the grid does not pass through the origin: an arithmetic comb offset from DC, \
                 not harmonics of a fundamental"
            }
            Self::WidthsInconsistent => {
                "width / n is not constant across the members: the widths contradict the harmonic \
                 prediction that a fundamental's frequency noise multiplies with n"
            }
            Self::LineShapesDisagree => {
                "the supplied line shapes do not correlate after scaling by n, or only some \
                 members supplied one"
            }
        }
    }
}

/// One member of a claimed family, with the index the fit gave it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FamilyAssignment {
    /// The inventory row.
    pub emitter_id: EmitterId,
    /// Harmonic index `n`.
    pub index: u32,
    /// Measured centre, Hz.
    pub f_center_hz: f64,
    /// Measured minus fitted, Hz.
    pub residual_hz: f64,
    /// Measured width, Hz.
    pub width_hz: f64,
    /// `width / n` — the fundamental's own frequency-noise scale as this member reads it.
    pub sigma0_hz: f64,
}

/// The width corroboration: the one piece of evidence that comes from a column the frequency fit
/// never touched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidthEvidence {
    /// Least-squares `σ₀` in `w = σ₀·n`, Hz: the fundamental's frequency-noise scale.
    pub sigma0_hz: f64,
    /// Spread of `wᵢ/nᵢ` across the members, max ÷ min.
    pub ratio: f64,
    /// `n_max / n_min`: how much lever the indices give the `w ∝ n` prediction.
    pub index_leverage: f64,
    /// Residual sum of squares of `w = σ₀·n`.
    pub rss_harmonic: f64,
    /// Residual sum of squares of the rival one-parameter model `w = c`.
    pub rss_flat: f64,
    /// Whether the leverage is large enough ([`WIDTH_LEVERAGE_MIN`]) for `w ∝ n` to be
    /// **distinguishable** from `w = const`, and the harmonic model actually fits better.
    ///
    /// `false` is not a failure: it means the width agreed with the harmonic prediction but that
    /// agreement does not separate the two models, so it corroborates the *scale* and not the
    /// *slope*. Claiming otherwise would be counting one reading twice.
    pub separates: bool,
}

/// A family of emitters that are harmonics of one fundamental **that was never itself detected**.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarmonicFamily {
    /// The device id of the receive chain every member was measured on (T-302).
    pub device_id: String,
    /// The antenna port, when every member recorded one.
    pub antenna_port: Option<String>,
    /// Fitted fundamental, Hz — the slope of `f = n·f₀ + b`.
    pub f0_hz: f64,
    /// Fitted intercept `b`, Hz. A harmonic family's is zero.
    pub intercept_hz: f64,
    /// Standard error of the intercept, Hz.
    pub intercept_se_hz: f64,
    /// `se(b)/f₀`: how far the labelling is from the `n` ↔ `n ± 1` coin flip. **The non-vacuity
    /// number.** See [`INDEX_PIN_MAX`].
    pub index_pin: f64,
    /// `|b| / se(b)`: how many standard errors the grid misses the origin by.
    pub origin_sigmas: f64,
    /// `|b| / f₀`: how far the grid misses the origin as a share of one whole fundamental. The
    /// clause [`HarmonicFamily::origin_sigmas`] cannot supply, because it does not scale with the
    /// fit's own sloppiness.
    pub origin_fraction: f64,
    /// Rms of the members' residuals about the fitted line, Hz.
    pub residual_rms_hz: f64,
    /// The tolerance the residual had to meet, Hz — from the members' own measured widths.
    pub residual_tolerance_hz: f64,
    /// The members, by ascending index.
    pub members: Vec<FamilyAssignment>,
    /// The width corroboration.
    pub width: WidthEvidence,
    /// Smallest pairwise line-shape correlation, when profiles were supplied.
    pub line_shape_corr: Option<f64>,
}

impl HarmonicFamily {
    /// The arithmetic, spelled out for display. Every family claim carries this.
    pub fn arithmetic(&self) -> String {
        let idx: Vec<String> = self.members.iter().map(|m| m.index.to_string()).collect();
        format!(
            "harmonics {} of an undetected {:.6} MHz fundamental on {}: \
             residual {:.1} Hz rms (tolerance {:.1} Hz), intercept {:.1} +/- {:.1} Hz \
             ({:.2} sigma and {:.2e} of a fundamental from the origin), indices pinned to \
             {:.4} of a fundamental; \
             width/n = {:.1} Hz within {:.2}x over {:.2}x of index leverage{}",
            idx.join(", "),
            self.f0_hz / 1e6,
            self.device_id,
            self.residual_rms_hz,
            self.residual_tolerance_hz,
            self.intercept_hz,
            self.intercept_se_hz,
            self.origin_sigmas,
            self.origin_fraction,
            self.index_pin,
            self.width.sigma0_hz,
            self.width.ratio,
            self.width.index_leverage,
            match self.line_shape_corr {
                Some(c) => format!("; line shapes correlate to {c:.3}"),
                None => String::new(),
            },
        )
    }

    /// **The independence demonstration, as a method** (T-374).
    ///
    /// The same members relabelled `n + d`. The least-squares slope and **every residual** are
    /// unchanged — `n − n̄` is invariant under a uniform shift — and the intercept moves by exactly
    /// `d·f₀`. So the residual can never choose between labellings, and
    /// [`HarmonicFamily::index_pin`] is the only thing in the frequency fit that can. Returns
    /// `None` when the shift would move an index out of [`MIN_INDEX`]`..=`[`MAX_INDEX`].
    pub fn off_by_one(&self, d: i32) -> Option<LineFit> {
        let mut indices = Vec::with_capacity(self.members.len());
        for m in &self.members {
            let n = i64::from(m.index) + i64::from(d);
            if n < i64::from(MIN_INDEX) || n > i64::from(MAX_INDEX) {
                return None;
            }
            indices.push(n as u32);
        }
        let freqs: Vec<f64> = self.members.iter().map(|m| m.f_center_hz).collect();
        fit_line(&indices, &freqs)
    }
}

// ---------------------------------------------------------------------------------------------
// The fit
// ---------------------------------------------------------------------------------------------

/// A least-squares fit of `f = n·f₀ + b`.
#[derive(Clone, Debug, PartialEq)]
pub struct LineFit {
    /// Slope: the fundamental, Hz.
    pub f0_hz: f64,
    /// Intercept, Hz.
    pub intercept_hz: f64,
    /// Rms of the residuals, Hz.
    pub residual_rms_hz: f64,
    /// Residual scale `σ̂ = √(SSE/(k−2))`, floored at [`CENTER_PRECISION_MIN_HZ`], Hz.
    pub sigma_hz: f64,
    /// Standard error of the intercept, Hz.
    pub intercept_se_hz: f64,
    /// Measured minus fitted, per member, Hz.
    pub residuals: Vec<f64>,
}

/// Least squares of `f` against integer index `n`. `None` when there are fewer than
/// [`MIN_MEMBERS`] points, fewer than two distinct indices, or anything is not finite.
pub fn fit_line(indices: &[u32], freqs: &[f64]) -> Option<LineFit> {
    let k = indices.len();
    if k != freqs.len() || k < MIN_MEMBERS {
        return None;
    }
    if !freqs.iter().all(|f| f.is_finite()) {
        return None;
    }
    let kf = k as f64;
    let n_bar = indices.iter().map(|&n| f64::from(n)).sum::<f64>() / kf;
    let f_bar = freqs.iter().sum::<f64>() / kf;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for (&n, &f) in indices.iter().zip(freqs) {
        let dn = f64::from(n) - n_bar;
        sxx += dn * dn;
        sxy += dn * (f - f_bar);
    }
    if !positive(sxx) || !sxy.is_finite() {
        return None;
    }
    let f0 = sxy / sxx;
    let b = f_bar - f0 * n_bar;
    let residuals: Vec<f64> = indices
        .iter()
        .zip(freqs)
        .map(|(&n, &f)| f - (f64::from(n) * f0 + b))
        .collect();
    let sse: f64 = residuals.iter().map(|r| r * r).sum();
    if !sse.is_finite() {
        return None;
    }
    let residual_rms = (sse / kf).sqrt();
    // σ̂ carries the two parameters the fit spent. Floored, because no centre is measured to
    // better than a hertz and a synthetically exact set would otherwise divide by zero.
    let sigma = (sse / (kf - 2.0)).sqrt().max(CENTER_PRECISION_MIN_HZ);
    let se_b = sigma * (1.0 / kf + n_bar * n_bar / sxx).sqrt();
    Some(LineFit {
        f0_hz: f0,
        intercept_hz: b,
        residual_rms_hz: residual_rms,
        sigma_hz: sigma,
        intercept_se_hz: se_b,
        residuals,
    })
}

/// The residual tolerance for a set, Hz: the **tighter** of [`RESIDUAL_PPM`] of the highest member
/// frequency and [`RESIDUAL_WIDTH_FRACTION`] of the **narrowest** member's own measured width,
/// floored at [`RESIDUAL_MIN_HZ`].
///
/// The narrowest width, not the widest: the tolerance is what the best-measured centre in the set
/// can support, and taking the widest would let one broad member buy slack for every narrow one.
///
/// **The `min` is what the negative control bought.** Either term alone is a scale a centre could
/// plausibly be measured to; taking whichever is *looser* means a set can always reach for the
/// looser one, and a 180 kHz emission then claims a 15 kHz window it does not need — which is how
/// 55.5 % of unrelated FM-band populations became "families" before this line read `min`.
pub fn residual_tolerance_hz(widths: &[f64], f_max_hz: f64) -> f64 {
    let narrowest = widths
        .iter()
        .copied()
        .filter(|w| w.is_finite() && *w > 0.0)
        .fold(f64::INFINITY, f64::min);
    let by_width = if narrowest.is_finite() {
        RESIDUAL_WIDTH_FRACTION * narrowest
    } else {
        f64::INFINITY
    };
    let by_ppm = if f_max_hz.is_finite() && f_max_hz > 0.0 {
        RESIDUAL_PPM * 1e-6 * f_max_hz
    } else {
        f64::INFINITY
    };
    let t = by_width.min(by_ppm);
    if t.is_finite() {
        t.max(RESIDUAL_MIN_HZ)
    } else {
        RESIDUAL_MIN_HZ
    }
}

// ---------------------------------------------------------------------------------------------
// The verdict on one proposed set
// ---------------------------------------------------------------------------------------------

/// Whether `members`, carrying the indices in `indices`, are harmonics of one undetected
/// fundamental — or which gate says they are not.
///
/// The indices are an input here on purpose: grouping and judging are different jobs, and keeping
/// them apart is what lets [`HarmonicFamily::off_by_one`] ask what a *different* labelling of the
/// same measurements would have looked like.
pub fn judge_family(
    members: &[FamilyMember],
    indices: &[u32],
) -> Result<HarmonicFamily, FamilyRejection> {
    let k = members.len();
    if k != indices.len() || k < MIN_MEMBERS {
        return Err(FamilyRejection::TooFewMembers);
    }
    for (m, &n) in members.iter().zip(indices) {
        if !m.f_center_hz.is_finite()
            || m.f_center_hz <= 0.0
            || !m.width_hz.is_finite()
            || m.width_hz <= 0.0
        {
            return Err(FamilyRejection::Unmeasured);
        }
        if !(MIN_INDEX..=MAX_INDEX).contains(&n) {
            return Err(FamilyRejection::IndexOutOfRange);
        }
    }
    // One grid slot cannot hold two emissions.
    let mut seen: Vec<u32> = indices.to_vec();
    seen.sort_unstable();
    seen.dedup();
    if seen.len() != k {
        return Err(FamilyRejection::DuplicateIndex);
    }
    if seen.len() < 2 {
        return Err(FamilyRejection::TooFewMembers);
    }
    // Members must be separate emissions. Lines *inside* one detection are a comb
    // (`hk_detect::comb`), a different mechanism with a different tolerance.
    for (i, a) in members.iter().enumerate() {
        for b in &members[i + 1..] {
            if (a.f_center_hz - b.f_center_hz).abs() <= 0.5 * (a.width_hz + b.width_hz) {
                return Err(FamilyRejection::OverlappingMembers);
            }
        }
    }
    // T-302: device-local physics. Every member on one chain, or no family.
    let chain = shared_chain(members).ok_or(FamilyRejection::ChainsDiffer)?;

    let freqs: Vec<f64> = members.iter().map(|m| m.f_center_hz).collect();
    let widths: Vec<f64> = members.iter().map(|m| m.width_hz).collect();
    let fit = fit_line(indices, &freqs).ok_or(FamilyRejection::TooFewMembers)?;
    if !positive(fit.f0_hz) {
        return Err(FamilyRejection::ResidualTooLarge);
    }
    let f_max = freqs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let tol = residual_tolerance_hz(&widths, f_max);
    // 2a. **Is the question even answerable?** Checked before the residual on purpose: the
    // residual asks whether the grid fits, and this asks whether there is a determinate grid to
    // fit. For a set of wide boxes at a high index the answer is no *whatever* the residual does —
    // extrapolating their centre precision down to zero frequency spans a sizeable share of a
    // whole fundamental, so `n` and `n ± 1` are the same hypothesis — and "no labelling can be
    // claimed" is the more informative refusal than "these are 7 kHz off some grid".
    let index_pin = fit.intercept_se_hz / fit.f0_hz;
    if !within(index_pin, INDEX_PIN_MAX) {
        return Err(FamilyRejection::IndicesNotPinned);
    }
    if !within(fit.residual_rms_hz, tol) {
        return Err(FamilyRejection::ResidualTooLarge);
    }
    // 2b. Does the grid pass through the origin — statistically, AND as a share of a whole
    // fundamental? The second clause is not redundant: the first is self-scaling, and a fit sloppy
    // enough to carry a large `se(b)` would otherwise buy its own acquittal.
    let origin_sigmas = fit.intercept_hz.abs() / fit.intercept_se_hz;
    let origin_fraction = fit.intercept_hz.abs() / fit.f0_hz;
    if !within(origin_sigmas, ORIGIN_MAX_SIGMA) || !within(origin_fraction, ORIGIN_MAX_FRACTION) {
        return Err(FamilyRejection::NotThroughOrigin);
    }
    // 3. The independent corroboration.
    let width = width_evidence(indices, &widths);
    if !within(width.ratio, WIDTH_RATIO_MAX) {
        return Err(FamilyRejection::WidthsInconsistent);
    }
    // 4. Line shape, when supplied. All or nothing.
    let line_shape_corr = line_shape_correlation(members)?;

    let mut assignments: Vec<FamilyAssignment> = members
        .iter()
        .zip(indices)
        .zip(&fit.residuals)
        .map(|((m, &n), &r)| FamilyAssignment {
            emitter_id: m.emitter_id,
            index: n,
            f_center_hz: m.f_center_hz,
            residual_hz: r,
            width_hz: m.width_hz,
            sigma0_hz: m.width_hz / f64::from(n),
        })
        .collect();
    assignments.sort_by_key(|a| a.index);

    Ok(HarmonicFamily {
        device_id: chain.device_id,
        antenna_port: chain.antenna_port,
        f0_hz: fit.f0_hz,
        intercept_hz: fit.intercept_hz,
        intercept_se_hz: fit.intercept_se_hz,
        index_pin,
        origin_sigmas,
        origin_fraction,
        residual_rms_hz: fit.residual_rms_hz,
        residual_tolerance_hz: tol,
        members: assignments,
        width,
        line_shape_corr,
    })
}

/// A receive chain every member was measured on, or `None` (T-302).
fn shared_chain(members: &[FamilyMember]) -> Option<ReceiveChain> {
    let first = members.first()?;
    for c in &first.chains {
        if members
            .iter()
            .all(|m| m.chains.iter().any(|x| x.same_chain(c)))
        {
            // Report the most specific port any member recorded for this device: `same_chain`
            // treats an unrecorded port as unknown rather than as a different path.
            let port = members
                .iter()
                .flat_map(|m| m.chains.iter())
                .find(|x| x.same_chain(c) && x.antenna_port.is_some())
                .and_then(|x| x.antenna_port.clone());
            return Some(ReceiveChain {
                device_id: c.device_id.clone(),
                antenna_port: port,
            });
        }
    }
    None
}

/// The width corroboration: `w = σ₀·n` against the rival `w = c`, plus the leverage that says
/// whether the two are even distinguishable.
pub fn width_evidence(indices: &[u32], widths: &[f64]) -> WidthEvidence {
    let ratios: Vec<f64> = indices
        .iter()
        .zip(widths)
        .map(|(&n, &w)| w / f64::from(n))
        .collect();
    let hi = ratios.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let lo = ratios.iter().copied().fold(f64::INFINITY, f64::min);
    let ratio = if lo > 0.0 && hi.is_finite() {
        hi / lo
    } else {
        f64::INFINITY
    };
    // Least squares through the origin, both models one parameter.
    let snn: f64 = indices.iter().map(|&n| f64::from(n) * f64::from(n)).sum();
    let snw: f64 = indices
        .iter()
        .zip(widths)
        .map(|(&n, &w)| f64::from(n) * w)
        .sum();
    let sigma0 = if snn > 0.0 { snw / snn } else { f64::NAN };
    let rss_harmonic: f64 = indices
        .iter()
        .zip(widths)
        .map(|(&n, &w)| {
            let e = w - sigma0 * f64::from(n);
            e * e
        })
        .sum();
    let c = widths.iter().sum::<f64>() / widths.len() as f64;
    let rss_flat: f64 = widths
        .iter()
        .map(|&w| {
            let e = w - c;
            e * e
        })
        .sum();
    let n_hi = indices.iter().copied().max().unwrap_or(1);
    let n_lo = indices.iter().copied().min().unwrap_or(1);
    let leverage = f64::from(n_hi) / f64::from(n_lo.max(1));
    WidthEvidence {
        sigma0_hz: sigma0,
        ratio,
        index_leverage: leverage,
        rss_harmonic,
        rss_flat,
        separates: leverage >= WIDTH_LEVERAGE_MIN && rss_harmonic < rss_flat,
    }
}

/// Smallest pairwise Pearson correlation of the supplied line shapes, or `None` when no member
/// supplied one. Refuses a partial set: a shape test only some members took is not a test.
fn line_shape_correlation(members: &[FamilyMember]) -> Result<Option<f64>, FamilyRejection> {
    let with: Vec<&Vec<f64>> = members
        .iter()
        .filter_map(|m| m.line_shape.as_ref())
        .collect();
    if with.is_empty() {
        return Ok(None);
    }
    if with.len() != members.len() {
        return Err(FamilyRejection::LineShapesDisagree);
    }
    let len = with[0].len();
    if len < 8 || with.iter().any(|p| p.len() != len) {
        return Err(FamilyRejection::LineShapesDisagree);
    }
    let mut worst = f64::INFINITY;
    for (i, a) in with.iter().enumerate() {
        for b in &with[i + 1..] {
            let Some(r) = pearson(a, b) else {
                return Err(FamilyRejection::LineShapesDisagree);
            };
            worst = worst.min(r);
        }
    }
    if !at_least(worst, LINE_SHAPE_MIN_CORR) {
        return Err(FamilyRejection::LineShapesDisagree);
    }
    Ok(Some(worst))
}

/// Pearson correlation, or `None` when either series is constant or not finite.
fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len() as f64;
    if a.len() != b.len() || a.len() < 2 {
        return None;
    }
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let (mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        let (dx, dy) = (x - ma, y - mb);
        saa += dx * dx;
        sbb += dy * dy;
        sab += dx * dy;
    }
    let d = (saa * sbb).sqrt();
    (d > 0.0 && d.is_finite()).then(|| (sab / d).clamp(-1.0, 1.0))
}

// ---------------------------------------------------------------------------------------------
// The search
// ---------------------------------------------------------------------------------------------

/// Search `members` for harmonic families, strongest first (most members, then smallest residual).
///
/// **Grouping and judging are separate.** The grouping step proposes `f₀ = fᵢ/nᵢ` for every member
/// and every index in [`MIN_INDEX`]`..=`[`MAX_INDEX`], assigns each other member the nearest index
/// on that grid, and keeps those within [`residual_tolerance_hz`]. It is the *hypothesis
/// generator*, and because it assigns indices under the through-origin model it partly selects for
/// a small intercept — which is exactly why the verdict leans on [`HarmonicFamily::index_pin`] and
/// on the widths instead. Each proposal is then handed to [`judge_family`], which can and does say
/// no.
///
/// Members claimed by one family are removed before the next is sought, so a row belongs to at
/// most one family.
pub fn find_harmonic_families(members: &[FamilyMember]) -> Vec<HarmonicFamily> {
    let mut pool: Vec<FamilyMember> = members
        .iter()
        .filter(|m| {
            m.f_center_hz.is_finite()
                && m.f_center_hz > 0.0
                && m.width_hz.is_finite()
                && m.width_hz > 0.0
        })
        .cloned()
        .collect();
    let mut out: Vec<HarmonicFamily> = Vec::new();
    while pool.len() >= MIN_MEMBERS {
        let Some(found) = best_family(&pool) else {
            break;
        };
        let claimed: Vec<EmitterId> = found.members.iter().map(|m| m.emitter_id).collect();
        pool.retain(|m| !claimed.contains(&m.emitter_id));
        out.push(found);
    }
    out
}

/// The best family in `pool`, or `None`.
fn best_family(pool: &[FamilyMember]) -> Option<HarmonicFamily> {
    let mut best: Option<HarmonicFamily> = None;
    for anchor in pool {
        for n0 in MIN_INDEX..=MAX_INDEX {
            let f0 = anchor.f_center_hz / f64::from(n0);
            if !positive(f0) {
                continue;
            }
            let (subset, indices) = gather(pool, f0);
            if subset.len() < MIN_MEMBERS {
                continue;
            }
            let Ok(fam) = judge_family(&subset, &indices) else {
                continue;
            };
            let better = match &best {
                None => true,
                Some(b) => {
                    fam.members.len() > b.members.len()
                        || (fam.members.len() == b.members.len()
                            && fam.residual_rms_hz < b.residual_rms_hz)
                }
            };
            if better {
                best = Some(fam);
            }
        }
    }
    best
}

/// The members of `pool` that sit on the through-origin grid of `f0`, with their indices. When two
/// members claim one index the closer one keeps it, because one grid slot cannot hold two
/// emissions.
///
/// Each member is admitted on **its own** width's tolerance, not the pool's narrowest: how closely
/// a centre can be measured is a property of that emission, and letting one narrow unrelated row
/// in the pool tighten the grid for everybody would lose real families for a reason that has
/// nothing to do with them. The set-level tolerance in [`judge_family`] is the claim; this is the
/// admission test.
fn gather(pool: &[FamilyMember], f0: f64) -> (Vec<FamilyMember>, Vec<u32>) {
    let mut hits: Vec<(u32, f64, &FamilyMember)> = Vec::new();
    for m in pool {
        let n = (m.f_center_hz / f0).round();
        if !(n >= f64::from(MIN_INDEX) && n <= f64::from(MAX_INDEX)) {
            continue;
        }
        let err = m.f_center_hz - n * f0;
        if err.abs() > GATHER_SLACK * residual_tolerance_hz(&[m.width_hz], m.f_center_hz) {
            continue;
        }
        hits.push((n as u32, err.abs(), m));
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    hits.dedup_by_key(|h| h.0);
    (
        hits.iter().map(|h| h.2.clone()).collect(),
        hits.iter().map(|h| h.0).collect(),
    )
}
