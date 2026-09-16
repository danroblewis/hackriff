//! C18 signature matching (T-201, ADR-0016 §5): comparing one emitter's measured
//! [`EmissionFeatures`] against the catalogue and producing a ranked, reasoned [`SignatureMatch`].
//!
//! # What a match is
//!
//! **Evidence, never identity.** The output of this module sets no emitter field: not the
//! identity, not `known_status`, not the lifecycle, not the family. It is a ranked list of
//! explanations with the arithmetic disclosed, exactly like a band-plan prior (CLAUDE.md: the
//! known-signal database is never a source of truth, and mismatches are interesting). Only a
//! CRC-valid decode confirms a signal (docs/15 §1). The type system carries most of this — a
//! [`SignatureMatch`] has nowhere to put an identity — and [`super::tests`] pins the rest.
//!
//! # The comparison
//!
//! 1. **Family gate.** A signature that names a family is compared only against a measurement
//!    whose family maps to the same one under `hk-mod@1`. An `unknown`, absent or
//!    outside-the-taxonomy family gates nothing: "not measured" is never "ruled out". Bands are
//!    **rank-only** and gate nothing at all — an emission in the "wrong" band is the interesting
//!    case, not a disqualified one.
//! 2. **Per-field `z`.** Each expectation gives a distance normalised so that `z ≤ 1` is
//!    agreement whatever the units: numbers by their tolerance, bit patterns by their own
//!    `max_errors`, labels exactly.
//! 3. **Measurement uncertainty widens the tolerance**, in quadrature:
//!    `effective = hypot(tolerance, sigma)`. This is the half of the contract that makes the
//!    aggregation in [`super::features`] worth doing — a field measured badly (or one whose
//!    repeated observations disagreed) cannot manufacture a conflict, and a field measured well
//!    keeps its full discriminating power.
//! 4. **Score** `Σ w·exp(−z²/2) / Σ w_required`, clamped to `[0, 1]`. Missing required fields
//!    contribute nothing to the numerator while still counting in the denominator, so a
//!    measurement with few fields scores low **by construction** — which is why "too few fields"
//!    comes out as a ranked `partial` and never as a confident identity.
//! 5. **Outcome** per ADR-0016 §5, with two floors this module adds:
//!    - [`MATCH_MIN_DISCRIMINATING`]: a `full` match needs at least three agreeing required
//!      fields **however few the signature itself declares**, so an imported or hand-written
//!      entry can never lower the bar for identifying something (imports are untrusted input).
//!    - **Ambiguity.** When two entries both qualify as `full`, neither wins: the match is
//!      `partial` with both ranked and the reason `ambiguous_candidates`. This is the
//!      P25/DMR near-collision from the C18 card (both 4800 Bd, differing only in deviation and
//!      sync word) — guessing between them would be exactly the false identity the outcome floors
//!      exist to prevent.

use std::collections::BTreeMap;

use hk_model::classify::TaxonomyRef;
use hk_model::classify::taxonomy::{UNKNOWN, family_of};
pub use hk_model::signature::default_tolerance;
use hk_model::signature::{
    EmissionFeatures, FULL_MATCH_MIN_SCORE, Feat, FeatValue, FieldAgreement, FieldExpect,
    FieldSpec, MATCH_CANDIDATES_MAX, MatchOutcome, PARTIAL_MATCH_MIN_SCORE, RecipeRef,
    SIGNATURE_SCHEMA, Signature, SignatureCandidate, SignatureMatch, Z_CONFLICT, field,
};
use hk_model::time::Timestamp;
use serde_json::json;

/// Smallest number of agreeing required fields any `full` match needs, whatever a signature's own
/// `min_discriminating` says. A catalogue entry (especially an untrusted import) must not be able
/// to declare itself identifiable from one field.
pub const MATCH_MIN_DISCRIMINATING: u32 = 3;

/// `z` given to a label that simply differs (line code, CRC name): above [`Z_CONFLICT`], so a
/// wrong label is evidence *against* the entry rather than a near miss.
pub const TEXT_MISMATCH_Z: f64 = Z_CONFLICT + 1.0;

/// How one candidate scored, before the match-level outcome is decided.
struct Scored {
    candidate: SignatureCandidate,
    /// Required fields compared and agreeing (`z ≤ 1`).
    agreeing_required: u32,
    /// A required field actively disagrees (`z > Z_CONFLICT`): the entry is ruled out.
    conflicting_required: bool,
    /// Every required field present and agreeing, enough of them, and a high enough score.
    full_eligible: bool,
}

/// Whether the catalogue entry's family is compatible with what was measured (step 1 above).
fn family_compatible(sig: &Signature, features: &EmissionFeatures) -> bool {
    let Some(expected) = sig.family.as_deref() else {
        return true; // a modulation-agnostic entry (an RFI comb, a radar) gates on nothing
    };
    let Some(measured) = features.get(field::FAMILY).and_then(|f| f.value.text()) else {
        return true; // not measured is not ruled out
    };
    let taxonomy = sig.taxonomy.clone().unwrap_or_else(TaxonomyRef::current);
    let Some(measured_family) = family_of(measured, &taxonomy) else {
        return true; // a label outside the taxonomy (a service name) gates nothing
    };
    if measured_family == UNKNOWN {
        return true; // the open-set outcome never rules an entry out
    }
    let expected_family = family_of(expected, &taxonomy).unwrap_or(expected);
    measured_family == expected_family
}

/// The distance of a measured number from an expectation, with the scale the tolerance is
/// relative to.
fn numeric_distance(measured: f64, expect: &FieldExpect) -> Option<(f64, f64, serde_json::Value)> {
    match expect {
        FieldExpect::Value { value } => Some(((measured - value).abs(), value.abs(), json!(value))),
        FieldExpect::Range { lo, hi } => {
            if measured >= *lo && measured <= *hi {
                Some((0.0, measured.abs(), json!([lo, hi])))
            } else {
                let nearest = if measured < *lo { *lo } else { *hi };
                Some(((measured - nearest).abs(), nearest.abs(), json!([lo, hi])))
            }
        }
        FieldExpect::Set { values } => values
            .iter()
            .map(|v| ((measured - v).abs(), v.abs()))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(d, scale)| (d, scale, json!(values))),
        _ => None,
    }
}

/// Hamming distance of `measured` against `pattern`, trying both polarities and every alignment,
/// with `x` in the pattern meaning "don't care".
///
/// Both polarities are tried because a demodulator's slicer has no absolute sense of which level
/// is a one; the alignment slide covers a preamble captured longer than the entry specifies.
/// Rotations beyond polarity (the PSK constellation-rotation set of ADR-0016 §5) need the symbol
/// mapping, not the sliced bits, so they are not attempted here — a rotated PSK sync word reads as
/// a miss rather than a false agreement, which is the safe direction.
pub(crate) fn bit_errors(measured: &str, pattern: &str) -> Option<u32> {
    if measured.is_empty() || pattern.is_empty() {
        return None;
    }
    let pat: Vec<u8> = pattern.bytes().collect();
    let meas: Vec<u8> = measured.bytes().collect();
    if pat.len() > meas.len() {
        return None; // the measurement is shorter than the pattern: nothing to compare against
    }
    let mut best = u32::MAX;
    for invert in [false, true] {
        for start in 0..=(meas.len() - pat.len()) {
            let mut errors = 0u32;
            for (i, p) in pat.iter().enumerate() {
                if *p == b'x' {
                    continue;
                }
                let expected = if invert {
                    if *p == b'0' { b'1' } else { b'0' }
                } else {
                    *p
                };
                if meas[start + i] != expected {
                    errors += 1;
                }
            }
            best = best.min(errors);
        }
    }
    (best != u32::MAX).then_some(best)
}

/// Compares one measured field against one expectation. `None` when the two are not comparable
/// (a number where a label is expected): that is a *missing* measurement, not a disagreement.
fn compare_field(name: &str, measured: &Feat, spec: &FieldSpec) -> Option<FieldAgreement> {
    let (z, measured_json, expected_json) = match (&measured.value, &spec.expect) {
        (FeatValue::Num { value }, expect) => {
            let (distance, scale, expected) = numeric_distance(*value, expect)?;
            let relative = spec.tolerance.unwrap_or_else(|| default_tolerance(name));
            // A tolerance relative to zero is meaningless; fall back to an absolute one.
            let tolerance = if scale > 0.0 {
                relative * scale
            } else {
                relative
            };
            // The catalogue's tolerance and the measurement's own uncertainty combine in
            // quadrature: a badly measured field cannot manufacture a conflict.
            let effective = tolerance.hypot(measured.sigma).max(f64::MIN_POSITIVE);
            (distance / effective, json!(value), expected)
        }
        (
            FeatValue::Bits { bits },
            FieldExpect::Bits {
                bits: pattern,
                max_errors,
            },
        ) => {
            let errors = bit_errors(bits, pattern)?;
            let budget = f64::from((*max_errors).max(1));
            (f64::from(errors) / budget, json!(bits), json!(pattern))
        }
        (FeatValue::Text { text }, FieldExpect::Text { text: expected }) => {
            let same = text.trim().eq_ignore_ascii_case(expected.trim());
            (
                if same { 0.0 } else { TEXT_MISMATCH_Z },
                json!(text),
                json!(expected),
            )
        }
        _ => return None,
    };
    let z = if z.is_finite() { z } else { TEXT_MISMATCH_Z };
    Some(FieldAgreement {
        field: name.to_owned(),
        measured: measured_json,
        expected: expected_json,
        z,
        ok: z <= 1.0,
    })
}

/// Scores one catalogue entry against the measurement.
fn score(sig: &Signature, features: &EmissionFeatures) -> Option<Scored> {
    if !family_compatible(sig, features) {
        return None;
    }
    let mut agreement = Vec::new();
    let mut missing = Vec::new();
    let mut conflicting = Vec::new();
    let mut numerator = 0.0;
    let mut required_weight = 0.0;
    let mut agreeing_required = 0u32;
    let mut conflicting_required = false;
    let mut all_required_ok = true;

    for (name, spec) in &sig.fields {
        if spec.required {
            required_weight += spec.weight;
        }
        let compared = features
            .get(name)
            .and_then(|measured| compare_field(name, measured, spec));
        match compared {
            Some(a) => {
                numerator += spec.weight * (-0.5 * a.z * a.z).exp();
                if a.z > Z_CONFLICT {
                    conflicting.push(name.clone());
                    if spec.required {
                        conflicting_required = true;
                    }
                }
                if spec.required {
                    if a.ok {
                        agreeing_required += 1;
                    } else {
                        all_required_ok = false;
                    }
                }
                agreement.push(a);
            }
            None if spec.required => {
                missing.push(name.clone());
                all_required_ok = false;
            }
            None => {}
        }
    }

    let score = if required_weight > 0.0 {
        (numerator / required_weight).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let min_needed = sig.min_discriminating.max(MATCH_MIN_DISCRIMINATING);
    let full_eligible = all_required_ok
        && missing.is_empty()
        && conflicting.is_empty()
        && agreeing_required >= min_needed
        && score >= FULL_MATCH_MIN_SCORE;

    Some(Scored {
        candidate: SignatureCandidate {
            signature: sig.reference(),
            name: sig.name.clone(),
            score,
            agreement,
            missing,
            conflicting,
            recipe: sig.recipe.clone().map(|r| RecipeRef {
                id: r.id,
                version: r.version,
            }),
        },
        agreeing_required,
        conflicting_required,
        full_eligible,
    })
}

/// Compares `features` against `catalogue` and returns the ranked, reasoned match.
///
/// The result is **evidence about** the emitter, never a change to it. See the module docs for the
/// gate, the scoring and the two outcome floors.
pub fn match_signatures(
    features: &EmissionFeatures,
    catalogue: &[Signature],
    signatures_rev: u64,
    t: Timestamp,
) -> SignatureMatch {
    let mut reasons: Vec<String> = Vec::new();
    let mut ruled_out = 0u32;

    let mut scored: Vec<Scored> = catalogue
        .iter()
        .filter_map(|sig| score(sig, features))
        .filter(|s| {
            // A required field that actively disagrees rules the entry out: `partial` means "no
            // required field conflicting" (ADR-0016 §5). Absence is not disagreement, so an entry
            // merely missing that field stays a candidate.
            if s.conflicting_required {
                ruled_out += 1;
                return false;
            }
            true
        })
        .collect();

    // Best first; ties broken by id so the ranking is deterministic across runs.
    scored.sort_by(|a, b| {
        b.candidate
            .score
            .total_cmp(&a.candidate.score)
            .then_with(|| a.candidate.signature.id.cmp(&b.candidate.signature.id))
    });

    if ruled_out > 0 {
        reasons.push("conflicting_required".into());
    }
    if features.suspect_fraction >= 1.0 {
        // Matching still runs — the operator should see what a suspect emission resembles — but
        // the flag travels with it, and nothing may be minted from it (C18 card, ADR-0016 §5).
        reasons.push("all_suspect".into());
    }

    let full_count = scored.iter().filter(|s| s.full_eligible).count();
    let keep: Vec<&Scored> = scored
        .iter()
        .filter(|s| s.full_eligible || partial_eligible(s))
        .take(MATCH_CANDIDATES_MAX)
        .collect();

    let outcome = match (full_count, keep.first()) {
        (0, None) => MatchOutcome::None,
        (1, Some(top)) if top.full_eligible => MatchOutcome::Full,
        (n, Some(_)) => {
            if n > 1 {
                // Two entries both fit completely: neither is the answer (the P25/DMR case).
                reasons.push("ambiguous_candidates".into());
            }
            MatchOutcome::Partial
        }
        (_, None) => MatchOutcome::None,
    };

    if outcome != MatchOutcome::Full
        && let Some(top) = keep.first()
    {
        let min_needed = MATCH_MIN_DISCRIMINATING;
        if top.agreeing_required < min_needed {
            reasons.push("too_few_fields".into());
        }
        if !top.candidate.missing.is_empty() {
            reasons.push("missing_required".into());
        }
    }
    if outcome == MatchOutcome::None {
        reasons.push("no_candidate".into());
    }

    let candidates = match outcome {
        MatchOutcome::None => Vec::new(),
        _ => keep.into_iter().map(|s| s.candidate.clone()).collect(),
    };

    SignatureMatch {
        schema: SIGNATURE_SCHEMA,
        emitter_id: features.emitter_id,
        t,
        outcome,
        features_ref: Some(features.id.clone()),
        signatures_rev,
        candidates,
        reasons,
    }
}

/// Whether a scored entry may be listed as a ranked explanation: no required field disagrees, and
/// it either lacks fields (the interesting "not enough measured yet" case) or agrees well enough.
fn partial_eligible(s: &Scored) -> bool {
    !s.conflicting_required
        && (!s.candidate.missing.is_empty() || s.candidate.score >= PARTIAL_MATCH_MIN_SCORE)
}

/// The fields a `full` match would still need, best candidate first: what a MAUTO search should go
/// and estimate next (ADR-0016 §8).
pub fn missing_fields(m: &SignatureMatch) -> BTreeMap<String, Vec<String>> {
    m.candidates
        .iter()
        .filter(|c| !c.missing.is_empty())
        .map(|c| (c.signature.to_string(), c.missing.clone()))
        .collect()
}
