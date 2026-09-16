//! Ranking recipes against a signal's **measured** parameters (T-164; ADR-0011 §2.4 "Matching is a
//! hint, never a tune"; ADR-0013 §4.9 gap 7b). **Core interface**: changes are reviewed before
//! merge.
//!
//! [`rank`] scores every recipe's [`MatchHints`] against one [`MeasuredSignal`] and orders the
//! result, with a [`MatchReason`] per compared field saying what matched, what didn't and what was
//! never measured. It is pure arithmetic over values the caller already measured: this module
//! reads no repository, no band plan and no catalogue, and nothing it returns tunes anything.
//!
//! # The three rules this module exists to enforce
//!
//! 1. **Measurement decides the order.** A recipe's `freq_hz` — the one hint that is a band-plan
//!    prior rather than a measurement — carries [`FREQ_WEIGHT`] (`0.0`) and so can never move a
//!    recipe up the ranking. It survives only as [`Candidate::band_hint`], which [`rank`] consults
//!    **after** scores compare equal, exactly as T-212 established for C17 classification priors:
//!    a prior breaks ties, it never overrides measured evidence.
//! 2. **Unmeasured is not agreement.** A `None` measurement scores [`Verdict::Unmeasured`]: it
//!    contributes nothing to the numerator while its weight stays in the denominator, so a recipe
//!    declaring five expectations of a signal with one measured parameter scores low *by
//!    construction*. T-163 serves nulls rather than defaults precisely so this stays true; nothing
//!    here substitutes a default for an absent measurement.
//! 3. **Nothing fits ⇒ nothing is offered.** A measured family the recipe does not declare rules
//!    it out outright ([`Outcome::None`], reason [`RULED_OUT_FAMILY`]); a score below
//!    [`LOW_CONFIDENCE`], or a signal with nothing to compare, yields an empty candidate list, not
//!    a forced top choice.
//!
//! # Scoring
//!
//! Each hint the recipe *declares* opens one weighted slot ([`FAMILY_WEIGHT`] and friends). A slot
//! whose measurement exists is compared and earns `weight × exp(−z²/2)`, where `z` is the
//! normalised distance from the declared range (`0` inside it); a slot whose measurement is
//! missing earns nothing. The score is the earned weight over the **declared** weight, so it is
//! absolute and comparable across recipes rather than "best of what was found":
//!
//! ```text
//! score = Σ weight·exp(−z²/2)  /  Σ weight        over every declared slot
//! ```
//!
//! `z ≤ 1` reads as agreement, `z > 3` as an active conflict, the same normalised-distance
//! vocabulary `/api/signatures/match` (T-201) uses, so one reading of `z` serves both routes.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use hk_model::EstimatedParams;
use hk_model::classify::{TaxonomyRef, family_of};

use crate::recipe::MatchHints;

/// Weight of the modulation-family slot: the strongest single piece of evidence, and the only one
/// that can rule a recipe out on its own.
pub const FAMILY_WEIGHT: f64 = 3.0;
/// Weight of the symbol-rate slot.
pub const SYMBOL_RATE_WEIGHT: f64 = 2.0;
/// Weight of the occupied-bandwidth slot.
pub const BANDWIDTH_WEIGHT: f64 = 1.5;
/// Weight of the burstiness slot.
pub const BURSTY_WEIGHT: f64 = 1.0;
/// Weight of each declared measured feature (`pilot-19k`, …).
pub const FEATURE_WEIGHT: f64 = 1.0;
/// Weight of the `freq_hz` slot: **zero, deliberately**. Where a signal is usually found is a
/// band-plan prior, not a measurement of *this* emission, so it may break ties
/// ([`Candidate::band_hint`]) and nothing else. Raising this above zero would let a band plan
/// outrank measured evidence, which is the one thing this route must never do.
pub const FREQ_WEIGHT: f64 = 0.0;

/// A candidate scoring below this is not offered at all: a weak fit is reported as nothing to
/// offer, never as the best of a bad set.
pub const LOW_CONFIDENCE: f64 = 0.2;
/// Score at or above which a candidate with no conflicts reads as a [`Outcome::Fit`].
pub const FIT_SCORE: f64 = 0.8;
/// Fewest compared (declared *and* measured) slots a recipe needs to be offered at all, and to
/// read as a [`Outcome::Fit`]. One agreeing field out of five declared is a coincidence, not a
/// match: a 3 kHz carrier with nothing but its bandwidth measured sits inside the ACARS recipe's
/// declared bandwidth, and offering ACARS on that alone would be exactly the forced answer this
/// route must not give.
pub const MIN_COMPARED: u32 = 2;
/// Two candidates whose scores differ by no more than this are reported as ambiguous.
pub const AMBIGUOUS_DELTA: f64 = 0.05;

/// Duty cycle at or below which an emission reads as bursty. A measured duty cycle is the only
/// thing that decides this; an unmeasured one leaves `bursty` `None` (unknown), never `false`.
pub const BURSTY_DUTY_MAX: f64 = 0.5;

/// Nominal FM stereo pilot, Hz, and the tolerance within which a *measured* pilot earns the
/// `pilot-19k` feature token (T-037b measures it on the receiver clock, so it is never exactly
/// 19 kHz).
pub const PILOT_19K_HZ: f64 = 19_000.0;
/// Tolerance around [`PILOT_19K_HZ`], Hz.
pub const PILOT_19K_TOL_HZ: f64 = 200.0;

/// Top-level reason: no recipe scored above [`LOW_CONFIDENCE`].
pub const NO_CANDIDATE: &str = "no_candidate";
/// Top-level reason: too little was measured for any ranking to mean much.
pub const TOO_FEW_MEASUREMENTS: &str = "too_few_measurements";
/// Top-level reason: the best two candidates are within [`AMBIGUOUS_DELTA`].
pub const AMBIGUOUS_CANDIDATES: &str = "ambiguous_candidates";
/// Top-level reason: candidates were found but none fits well enough to read as a match.
pub const ALL_PARTIAL: &str = "all_partial";
/// Ruled out: the measured family is not one the recipe declares.
pub const RULED_OUT_FAMILY: &str = "family_conflict";
/// Ruled out: the recipe scored below [`LOW_CONFIDENCE`].
pub const RULED_OUT_LOW_SCORE: &str = "low_score";
/// Ruled out: the recipe declares no expectations at all, so there is nothing to rank it by.
pub const RULED_OUT_NO_HINTS: &str = "no_match_hints";
/// Ruled out: fewer than [`MIN_COMPARED`] of the recipe's expectations have been measured.
pub const RULED_OUT_TOO_FEW_COMPARED: &str = "too_few_compared";

/// What was actually measured about one emission. Every field is optional and **`None` means "not
/// measured"**, never a default: [`rank`] scores an absent measurement as [`Verdict::Unmeasured`],
/// which earns nothing. Callers fill this from measurements only (blind detection, tracking and
/// the estimator's `EstimatedParams`), never from a band plan or a catalogue.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MeasuredSignal {
    /// Measured modulation family (`wfm`, `fsk2`, …), as classified. `None` or `unknown` rules
    /// nothing out.
    pub family: Option<String>,
    /// Measured centre, Hz. Used **only** for [`Candidate::band_hint`] (a tie-break), never scored.
    pub f_center_hz: Option<f64>,
    /// Measured occupied bandwidth, Hz.
    pub bandwidth_hz: Option<f64>,
    /// Measured symbol rate, Bd.
    pub symbol_rate_bd: Option<f64>,
    /// Measured burstiness, from a measured duty cycle ([`bursty_from_duty`]).
    pub bursty: Option<bool>,
    /// Feature tokens the estimator actually measured ([`features_from_params`]). Absence of a
    /// token is **not** evidence the feature is absent — it is unmeasured.
    #[serde(default)]
    pub features: Vec<String>,
}

impl MeasuredSignal {
    /// How many parameters were measured at all. Used for [`TOO_FEW_MEASUREMENTS`]; `f_center_hz`
    /// does not count, since it is never scored.
    pub fn measured_count(&self) -> u32 {
        u32::from(self.family.as_deref().is_some_and(|f| !is_unknown(f)))
            + u32::from(self.bandwidth_hz.is_some())
            + u32::from(self.symbol_rate_bd.is_some())
            + u32::from(self.bursty.is_some())
            + u32::from(!self.features.is_empty())
    }
}

/// Whether a measured duty cycle reads as bursty ([`BURSTY_DUTY_MAX`]). Callers pass a *measured*
/// duty cycle; there is deliberately no overload for an unmeasured one, which must stay `None`.
pub fn bursty_from_duty(duty_cycle: f64) -> bool {
    duty_cycle <= BURSTY_DUTY_MAX
}

/// The feature tokens a set of estimated parameters actually evidences. Only measurements become
/// tokens: an unmeasured pilot yields no token, so a recipe expecting `pilot-19k` scores it
/// [`Verdict::Unmeasured`] rather than a conflict (we cannot prove a feature absent).
pub fn features_from_params(params: &EstimatedParams) -> Vec<String> {
    let mut out = Vec::new();
    if params
        .pilot_hz
        .is_some_and(|f| (f - PILOT_19K_HZ).abs() <= PILOT_19K_TOL_HZ)
    {
        out.push("pilot-19k".to_owned());
    }
    out
}

/// One recipe offered for ranking: its identity and what it declares about the signal it expects.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Recipe id.
    pub id: String,
    /// Latest version.
    pub version: u32,
    /// Display name.
    pub name: String,
    /// What the recipe expects of the signal.
    pub hints: MatchHints,
}

/// How one compared field came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Measured and inside (or within `z ≤ 1` of) what the recipe declares.
    Agree,
    /// Measured and close, but outside: `1 < z ≤ 3`. Partial credit, and visibly off.
    Near,
    /// Measured and actively disagreeing: `z > 3`, or a mismatched boolean/family.
    Conflict,
    /// The recipe declares it; nothing has measured it yet. **Earns nothing.**
    Unmeasured,
}

impl Verdict {
    /// Stable token, as the API serves it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agree => "agree",
            Self::Near => "near",
            Self::Conflict => "conflict",
            Self::Unmeasured => "unmeasured",
        }
    }
}

/// How a candidate as a whole came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Everything compared agrees, nothing conflicts, and enough was compared to mean it.
    Fit,
    /// Ranked, with what is missing or off named.
    Partial,
    /// Nothing to offer. **Not** a claim that the emission is unknown.
    None,
}

impl Outcome {
    /// Stable token, as the API serves it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fit => "fit",
            Self::Partial => "partial",
            Self::None => "none",
        }
    }
}

/// One compared field: what the recipe expected, what was measured, and the verdict — the
/// "reasons" half of gap 7b. `detail` is rendered from the numbers here and names no identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchReason {
    /// Field compared (`family`, `bandwidth_hz`, `symbol_rate_bd`, `bursty`, `feature:<token>`).
    pub field: String,
    /// Outcome for this field.
    pub verdict: Verdict,
    /// What was measured, or `null` when nothing was.
    pub measured: Value,
    /// What the recipe declares.
    pub expected: Value,
    /// Normalised distance; `null` for a non-numeric comparison or an unmeasured field.
    pub z: Option<f64>,
    /// Weight this field carried in the score.
    pub weight: f64,
    /// Weight it earned (`weight × exp(−z²/2)`; `0.0` when unmeasured).
    pub earned: f64,
    /// Plain-language rendering of the comparison.
    pub detail: String,
}

/// One ranked recipe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Recipe id.
    pub id: String,
    /// Latest version.
    pub version: u32,
    /// Display name.
    pub name: String,
    /// `Σ earned / Σ weight` over declared slots, 0–1.
    pub score: f64,
    /// Outcome for this candidate.
    pub outcome: Outcome,
    /// Slots that were declared *and* measured.
    pub compared: u32,
    /// Compared slots verdicted [`Verdict::Agree`].
    pub agreed: u32,
    /// Compared slots verdicted [`Verdict::Conflict`].
    pub conflicting: u32,
    /// Declared slots nothing has measured yet.
    pub unmeasured: u32,
    /// The measured centre falls in a range the recipe declares. **A tie-break only**: it is not
    /// in `score` and never reorders candidates whose scores differ.
    pub band_hint: bool,
    /// Per-field reasons, in a stable order.
    pub reasons: Vec<MatchReason>,
}

/// A recipe that was not offered, and why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuledOut {
    /// Recipe id.
    pub id: String,
    /// Latest version.
    pub version: u32,
    /// Display name.
    pub name: String,
    /// [`RULED_OUT_FAMILY`], [`RULED_OUT_LOW_SCORE`] or [`RULED_OUT_NO_HINTS`].
    pub reason: String,
    /// Plain-language rendering.
    pub detail: String,
    /// Score it did reach (`0.0` for a family conflict, which is not a matter of degree).
    pub score: f64,
}

/// The ranking of every recipe against one measurement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ranked {
    /// The best candidate's outcome, or [`Outcome::None`] when there are none.
    pub outcome: Outcome,
    /// Offered candidates, best first. **Empty when nothing fits.**
    pub candidates: Vec<Candidate>,
    /// Recipes not offered, and why.
    pub ruled_out: Vec<RuledOut>,
    /// Machine reason codes for the ranking as a whole.
    pub reasons: Vec<String>,
}

/// Whether a family label carries no information (the open-set label, or nothing).
fn is_unknown(family: &str) -> bool {
    let f = family.trim();
    f.is_empty() || f.eq_ignore_ascii_case(hk_model::classify::UNKNOWN)
}

/// How a measured family compares with the families a recipe declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FamilyAgreement {
    /// The measurement is one the recipe decodes.
    Agree,
    /// The measurement says nothing about whether this recipe applies, either because the
    /// classifier resolved only as far as a *family* while the recipe names a *class* within it
    /// (measured `analog` against a recipe wanting `wfm`), or because the label is not a
    /// modulation label at all — a **service** label such as `fm-broadcast`, or a decoder id,
    /// which the modulation taxonomy deliberately does not contain. Not agreement, and **not** a
    /// rule-out: there is no modulation measurement to rule on.
    Inconclusive,
    /// A different modulation: the recipe cannot decode this.
    Conflict,
}

/// Compares a measured family label with the labels a recipe declares.
///
/// Labels come from two levels of one taxonomy, and folding them together carelessly is a real
/// hazard: `am` and `wfm` are both *classes* of the `analog` family, so resolving both to their
/// family would make an ACARS recipe (`am`) "agree" with an FM broadcast station (`wfm`). So an
/// exact label match is tried first, and a label is widened only in the safe direction — a recipe
/// declaring a whole **family** (`fsk`) accepts any class in it (`2fsk`, and the legacy spelling
/// `fsk2`), while a recipe declaring a **class** is only ever satisfied by that class.
fn family_agreement(declared: &[String], measured: &str) -> FamilyAgreement {
    let measured = measured.trim().to_ascii_lowercase();
    let taxonomy = TaxonomyRef::current();
    let resolved = taxonomy.resolve();
    let measured_family = family_of(&measured, &taxonomy);
    let mut inconclusive = false;
    for d in declared {
        let d = d.trim().to_ascii_lowercase();
        if d == measured {
            return FamilyAgreement::Agree;
        }
        let Some(t) = resolved else { continue };
        // The recipe names a family; the measurement is a class (or legacy spelling) inside it.
        if t.is_family(&d) && measured_family.is_some_and(|f| f == d) {
            return FamilyAgreement::Agree;
        }
        // The reverse: the measurement stopped at the family the recipe's class belongs to.
        if t.is_family(&measured) && family_of(&d, &taxonomy).is_some_and(|f| f == measured) {
            inconclusive = true;
        }
    }
    // A label the modulation taxonomy does not know is not a measurement of modulation: the
    // pipeline also writes *service* families (`fm-broadcast`, decoder ids), and treating one of
    // those as a conflicting modulation would rule every recipe out on a perfectly good signal.
    if inconclusive || measured_family.is_none() {
        FamilyAgreement::Inconclusive
    } else {
        FamilyAgreement::Conflict
    }
}

/// Normalised distance of `x` from the closed range `[lo, hi]`. Inside the range: `0`. Outside:
/// the distance to the nearer edge over a scale that is the range's half-width, or a quarter of
/// the edge value for a degenerate (point) range such as a fixed 2400 Bd, so a fractional
/// tolerance applies where an absolute one would be meaningless.
fn range_z(x: f64, lo: f64, hi: f64) -> f64 {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    if x >= lo && x <= hi {
        return 0.0;
    }
    let edge = if x < lo { lo } else { hi };
    let scale = ((hi - lo) / 2.0).max(edge.abs() * 0.25);
    if scale <= 0.0 {
        return if (x - edge).abs() > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };
    }
    (x - edge).abs() / scale
}

/// `exp(−z²/2)`, the credit a compared slot earns.
fn credit(z: f64) -> f64 {
    if z.is_finite() {
        (-0.5 * z * z).exp()
    } else {
        0.0
    }
}

/// Verdict for a normalised distance: `z ≤ 1` agrees, `z > 3` conflicts.
fn verdict_for(z: f64) -> Verdict {
    if z <= 1.0 {
        Verdict::Agree
    } else if z <= 3.0 {
        Verdict::Near
    } else {
        Verdict::Conflict
    }
}

/// kHz/MHz-aware rendering of a frequency-like quantity.
fn hz(v: f64) -> String {
    if v.abs() >= 1e6 {
        format!("{:.4} MHz", v / 1e6)
    } else if v.abs() >= 1e3 {
        format!("{:.3} kHz", v / 1e3)
    } else {
        format!("{v:.1} Hz")
    }
}

/// A declared slot nothing has measured.
fn unmeasured(field: &str, expected: Value, weight: f64, what: &str) -> MatchReason {
    MatchReason {
        field: field.to_owned(),
        verdict: Verdict::Unmeasured,
        measured: Value::Null,
        expected,
        z: None,
        weight,
        earned: 0.0,
        detail: format!("{what} is not measured yet, so it counts as no evidence, not agreement"),
    }
}

/// Scores a numeric range slot against an optional measurement.
fn range_reason(
    field: &str,
    what: &str,
    range: [f64; 2],
    measured: Option<f64>,
    weight: f64,
    render: fn(f64) -> String,
) -> MatchReason {
    let expected = json!([range[0], range[1]]);
    let Some(x) = measured.filter(|v| v.is_finite()) else {
        return unmeasured(field, expected, weight, what);
    };
    let z = range_z(x, range[0], range[1]);
    let verdict = verdict_for(z);
    let earned = weight * credit(z);
    let detail = match verdict {
        Verdict::Agree if z == 0.0 => format!(
            "measured {what} {} is inside the recipe's {}–{}",
            render(x),
            render(range[0]),
            render(range[1])
        ),
        Verdict::Conflict => format!(
            "measured {what} {} is well outside the recipe's {}–{} (z {z:.1})",
            render(x),
            render(range[0]),
            render(range[1])
        ),
        _ => format!(
            "measured {what} {} is outside the recipe's {}–{} but close (z {z:.1})",
            render(x),
            render(range[0]),
            render(range[1])
        ),
    };
    MatchReason {
        field: field.to_owned(),
        verdict,
        measured: json!(x),
        expected,
        z: Some(z),
        weight,
        earned,
        detail,
    }
}

/// Scores one recipe's hints against one measurement. `None` when the measured family rules the
/// recipe out, or when the recipe declares nothing to rank it by.
fn score(entry: &Entry, m: &MeasuredSignal) -> Result<Candidate, RuledOut> {
    let h = &entry.hints;
    let out = |reason: &str, detail: String| RuledOut {
        id: entry.id.clone(),
        version: entry.version,
        name: entry.name.clone(),
        reason: reason.to_owned(),
        detail,
        score: 0.0,
    };

    let declares_anything = !h.families.is_empty()
        || h.bandwidth_hz.is_some()
        || h.symbol_rate_bd.is_some()
        || h.bursty.is_some()
        || !h.features.is_empty();
    if !declares_anything {
        return Err(out(
            RULED_OUT_NO_HINTS,
            "the recipe declares no expectations, so there is nothing to rank it by".to_owned(),
        ));
    }

    let mut reasons: Vec<MatchReason> = Vec::new();

    // Family. A measured family the recipe does not declare rules it out outright; an unmeasured
    // or `unknown` family rules nothing out (ADR-0016 §5).
    if !h.families.is_empty() {
        let expected = json!(h.families);
        match m.family.as_deref().filter(|f| !is_unknown(f)) {
            None => reasons.push(unmeasured(
                "family",
                expected,
                FAMILY_WEIGHT,
                "the modulation family",
            )),
            Some(fam) => match family_agreement(&h.families, fam) {
                FamilyAgreement::Agree => reasons.push(MatchReason {
                    field: "family".to_owned(),
                    verdict: Verdict::Agree,
                    measured: json!(fam),
                    expected,
                    z: Some(0.0),
                    weight: FAMILY_WEIGHT,
                    earned: FAMILY_WEIGHT,
                    detail: format!(
                        "measured family {fam} is one the recipe decodes ({})",
                        h.families.join(", ")
                    ),
                }),
                FamilyAgreement::Inconclusive => reasons.push(MatchReason {
                    field: "family".to_owned(),
                    verdict: Verdict::Unmeasured,
                    measured: json!(fam),
                    expected,
                    z: None,
                    weight: FAMILY_WEIGHT,
                    earned: 0.0,
                    detail: format!(
                        "measured {fam} does not resolve the modulation the recipe expects ({}): \
                         it is either a coarser family or not a modulation label, so it counts as \
                         no evidence either way",
                        h.families.join(", ")
                    ),
                }),
                FamilyAgreement::Conflict => {
                    return Err(out(
                        RULED_OUT_FAMILY,
                        format!(
                            "measured family {fam} is not one this recipe decodes ({})",
                            h.families.join(", ")
                        ),
                    ));
                }
            },
        }
    }

    if let Some(range) = h.bandwidth_hz {
        reasons.push(range_reason(
            "bandwidth_hz",
            "bandwidth",
            range,
            m.bandwidth_hz,
            BANDWIDTH_WEIGHT,
            hz,
        ));
    }
    if let Some(range) = h.symbol_rate_bd {
        reasons.push(range_reason(
            "symbol_rate_bd",
            "symbol rate",
            range,
            m.symbol_rate_bd,
            SYMBOL_RATE_WEIGHT,
            |v| format!("{v:.1} Bd"),
        ));
    }
    if let Some(want) = h.bursty {
        let expected = json!(want);
        reasons.push(match m.bursty {
            None => unmeasured("bursty", expected, BURSTY_WEIGHT, "burstiness"),
            Some(got) => {
                let agree = got == want;
                let say = |b: bool| if b { "bursty" } else { "continuous" };
                MatchReason {
                    field: "bursty".to_owned(),
                    verdict: if agree {
                        Verdict::Agree
                    } else {
                        Verdict::Conflict
                    },
                    measured: json!(got),
                    expected,
                    z: Some(if agree { 0.0 } else { 4.0 }),
                    weight: BURSTY_WEIGHT,
                    earned: if agree { BURSTY_WEIGHT } else { 0.0 },
                    detail: if agree {
                        format!("the emission is {}, as the recipe expects", say(got))
                    } else {
                        format!(
                            "the emission is {}; the recipe expects {}",
                            say(got),
                            say(want)
                        )
                    },
                }
            }
        });
    }
    // Features. A token the estimator measured agrees; a token it did not measure is unmeasured,
    // never a conflict — absence of a measurement is not measurement of absence.
    for want in &h.features {
        let field = format!("feature:{want}");
        let expected = json!(want);
        if m.features.iter().any(|f| f.eq_ignore_ascii_case(want)) {
            reasons.push(MatchReason {
                field,
                verdict: Verdict::Agree,
                measured: json!(want),
                expected,
                z: Some(0.0),
                weight: FEATURE_WEIGHT,
                earned: FEATURE_WEIGHT,
                detail: format!("{want} was measured on this signal"),
            });
        } else {
            reasons.push(unmeasured(&field, expected, FEATURE_WEIGHT, want));
        }
    }

    let total: f64 = reasons.iter().map(|r| r.weight).sum();
    let earned: f64 = reasons.iter().map(|r| r.earned).sum();
    let score = if total > 0.0 { earned / total } else { 0.0 };

    let count = |v: Verdict| reasons.iter().filter(|r| r.verdict == v).count() as u32;
    let agreed = count(Verdict::Agree);
    let conflicting = count(Verdict::Conflict);
    let unmeasured_n = count(Verdict::Unmeasured);
    let compared = reasons.len() as u32 - unmeasured_n;

    let outcome = if score < LOW_CONFIDENCE {
        Outcome::None
    } else if score >= FIT_SCORE && conflicting == 0 && compared >= MIN_COMPARED {
        Outcome::Fit
    } else {
        Outcome::Partial
    };

    // `freq_hz` is a band-plan prior, not a measurement: recorded as a tie-break flag, never
    // scored. Weight FREQ_WEIGHT (0.0) is asserted by this module's tests.
    let band_hint = match m.f_center_hz {
        Some(f) => h.freq_hz.iter().any(|r| {
            let (lo, hi) = if r[0] <= r[1] {
                (r[0], r[1])
            } else {
                (r[1], r[0])
            };
            f >= lo && f <= hi
        }),
        None => false,
    };

    Ok(Candidate {
        id: entry.id.clone(),
        version: entry.version,
        name: entry.name.clone(),
        score,
        outcome,
        compared,
        agreed,
        conflicting,
        unmeasured: unmeasured_n,
        band_hint,
        reasons,
    })
}

/// Ranks `entries` against one measurement.
///
/// Order: **score first, always**. Only when two scores are equal (to within `1e-9`) does anything
/// else speak, and then in this order: a `freq_hz` band hint, more agreeing fields, fewer
/// unmeasured fields, then the recipe id for determinism. A band hint therefore breaks ties and
/// nothing more — the T-212 rule, applied to recipe ranking.
///
/// Candidates scoring below [`LOW_CONFIDENCE`] are not offered; they appear in
/// [`Ranked::ruled_out`] with [`RULED_OUT_LOW_SCORE`], so an emission that fits nothing gets an
/// empty ranking rather than a forced top choice.
pub fn rank(entries: &[Entry], measured: &MeasuredSignal) -> Ranked {
    let mut candidates = Vec::new();
    let mut ruled_out = Vec::new();
    for e in entries {
        match score(e, measured) {
            Ok(c) if c.compared < MIN_COMPARED => ruled_out.push(RuledOut {
                id: c.id,
                version: c.version,
                name: c.name,
                reason: RULED_OUT_TOO_FEW_COMPARED.to_owned(),
                detail: format!(
                    "only {} of the recipe's {} expectations have been measured, too few to rank it on",
                    c.compared,
                    c.compared + c.unmeasured
                ),
                score: c.score,
            }),
            Ok(c) if c.outcome == Outcome::None => ruled_out.push(RuledOut {
                id: c.id,
                version: c.version,
                name: c.name,
                reason: RULED_OUT_LOW_SCORE.to_owned(),
                detail: format!(
                    "scored {:.2}, below the {LOW_CONFIDENCE:.2} needed to be worth offering",
                    c.score
                ),
                score: c.score,
            }),
            Ok(c) => candidates.push(c),
            Err(r) => ruled_out.push(r),
        }
    }

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                if (a.score - b.score).abs() <= 1e-9 {
                    // Ties only: the band-plan hint speaks here and nowhere else.
                    b.band_hint
                        .cmp(&a.band_hint)
                        .then_with(|| b.agreed.cmp(&a.agreed))
                        .then_with(|| a.unmeasured.cmp(&b.unmeasured))
                        .then_with(|| a.id.cmp(&b.id))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
    });
    ruled_out.sort_by(|a, b| a.id.cmp(&b.id));

    let outcome = candidates.first().map_or(Outcome::None, |c| c.outcome);
    let mut reasons = Vec::new();
    if candidates.is_empty() {
        reasons.push(NO_CANDIDATE.to_owned());
    }
    if measured.measured_count() < 2 {
        reasons.push(TOO_FEW_MEASUREMENTS.to_owned());
    }
    if candidates.len() >= 2 && (candidates[0].score - candidates[1].score).abs() <= AMBIGUOUS_DELTA
    {
        reasons.push(AMBIGUOUS_CANDIDATES.to_owned());
    }
    if !candidates.is_empty() && candidates.iter().all(|c| c.outcome == Outcome::Partial) {
        reasons.push(ALL_PARTIAL.to_owned());
    }

    Ranked {
        outcome,
        candidates,
        ruled_out,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_zero_inside_and_grows_outside() {
        assert_eq!(range_z(150e3, 100e3, 300e3), 0.0);
        assert_eq!(range_z(100e3, 100e3, 300e3), 0.0);
        // Half-width 100 kHz: 50 kHz below the low edge is z = 0.5.
        assert!((range_z(50e3, 100e3, 300e3) - 0.5).abs() < 1e-9);
        // A point range falls back to a quarter of the value: 2400 Bd ± 600 Bd is z = 1.
        assert!((range_z(1800.0, 2400.0, 2400.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_band_plan_hint_carries_no_weight_at_all() {
        // The invariant the blind rule rests on, asserted on the constant itself.
        assert_eq!(FREQ_WEIGHT, 0.0);
    }

    /// Label folding is asymmetric on purpose. `am` and `wfm` are sibling *classes* of the
    /// `analog` family, so resolving a measurement to its family before comparing would let an
    /// ACARS recipe (`am`) claim an FM broadcast station (`wfm`).
    #[test]
    fn family_labels_widen_only_in_the_safe_direction() {
        let decl = |s: &str| vec![s.to_owned()];
        // Exact, and case-insensitive.
        assert_eq!(
            family_agreement(&decl("wfm"), "WFM"),
            FamilyAgreement::Agree
        );
        // A declared family accepts a class in it, legacy spellings included (`fsk2` → `fsk`).
        assert_eq!(
            family_agreement(&decl("fsk"), "fsk2"),
            FamilyAgreement::Agree
        );
        assert_eq!(
            family_agreement(&decl("fsk"), "gfsk"),
            FamilyAgreement::Agree
        );
        // Sibling classes are a conflict, never a match: the case that rules ACARS out on FM.
        assert_eq!(
            family_agreement(&decl("am"), "wfm"),
            FamilyAgreement::Conflict
        );
        assert_eq!(
            family_agreement(&decl("ook"), "wfm"),
            FamilyAgreement::Conflict
        );
        // A class declared against a family-level measurement is unresolved, not a rule-out.
        assert_eq!(
            family_agreement(&decl("wfm"), "analog"),
            FamilyAgreement::Inconclusive
        );
        // A *service* label is not a modulation measurement at all. The pipeline writes these
        // (`fm-broadcast`, decoder ids), and reading one as a conflicting modulation would rule
        // every recipe out on a signal that is perfectly well characterised otherwise.
        assert_eq!(
            family_agreement(&decl("wfm"), "fm-broadcast"),
            FamilyAgreement::Inconclusive
        );
        assert_eq!(
            family_agreement(&decl("ook"), "adsb"),
            FamilyAgreement::Inconclusive
        );
    }

    /// A `None` measurement must never earn credit: the whole point of T-163 serving nulls.
    #[test]
    fn an_unmeasured_field_earns_nothing_but_keeps_its_weight() {
        let r = range_reason(
            "bandwidth_hz",
            "bandwidth",
            [100e3, 300e3],
            None,
            BANDWIDTH_WEIGHT,
            hz,
        );
        assert_eq!(r.verdict, Verdict::Unmeasured);
        assert_eq!(r.earned, 0.0);
        assert_eq!(r.weight, BANDWIDTH_WEIGHT);
        assert!(r.measured.is_null());
    }

    #[test]
    fn a_measured_pilot_becomes_a_feature_token_and_an_unmeasured_one_does_not() {
        let mut p = EstimatedParams::default();
        assert!(features_from_params(&p).is_empty());
        p.pilot_hz = Some(19_000.4);
        assert_eq!(features_from_params(&p), vec!["pilot-19k".to_owned()]);
        p.pilot_hz = Some(57_000.0);
        assert!(features_from_params(&p).is_empty());
    }

    #[test]
    fn burstiness_comes_from_a_measured_duty_cycle() {
        assert!(bursty_from_duty(0.02));
        assert!(!bursty_from_duty(0.99));
    }
}
