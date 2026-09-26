//! **A database suggestion explains a result and never becomes one** (T-569, ADR-0021 §9).
//!
//! RESEARCH-007 / RESEARCH-013. These are the behavioural half of the rule; the structural half —
//! that `hk-synth` cannot reach this crate at all — is
//! `crates/hk-synth/tests/explanation_boundary.rs`.

use hk_context::band_table::{BandTable, Region};
use hk_context::synth_explain::{
    MAX_EXPLANATIONS, MeasuredEmission, References, explain_resolution,
};
use hk_model::repo::synthesis::{
    Explanation, ExplanationSource, ExplanationStatus, Resolution, ResolutionKind,
    ResolutionReason, Verdict,
};

fn table() -> BandTable {
    BandTable::bundled(Region::Us).expect("bundled US allocation table")
}

fn refs(table: &BandTable) -> References<'_> {
    References {
        table,
        data_age_days: Some(41),
    }
}

/// A sealed resolution: what the search decided, before anything explained it.
fn sealed(kind: ResolutionKind, reason: ResolutionReason) -> Resolution {
    Resolution {
        kind,
        deepest_verdict: Some(Verdict::Framed),
        reason: Some(reason),
        suspected: None,
        summary: "Best: framed at 41 bits, no valid check.".into(),
        explanations: Vec::new(),
    }
}

/// Three suggestions, each as confident as a band plan ever gets.
fn three_confident() -> Vec<Explanation> {
    ["fm-broadcast", "aviation", "public-safety"]
        .into_iter()
        .map(|id| Explanation {
            source: ExplanationSource::BandPlan,
            identity: id.into(),
            score: 0.99,
            distance_hz: Some(0.0),
            status: ExplanationStatus::Expected,
            data_age_days: Some(1),
            reasoning: format!("{id} is allocated here and the centre is on its raster"),
        })
        .collect()
}

/// **The headline rule.** An `unknown` with three high-scoring explanations is still `unknown`
/// (ADR-0021 §9.2): the suggestion is beside the verdict, never in its place.
#[test]
fn an_unknown_with_three_high_scoring_explanations_is_still_unknown() {
    let before = sealed(ResolutionKind::Unknown, ResolutionReason::BudgetExhausted);
    let mut after = before.clone();
    after.attach_explanations(three_confident());

    assert_eq!(
        after.kind,
        ResolutionKind::Unknown,
        "a suggestion labelled an unknown"
    );
    assert_eq!(after.reason, before.reason);
    assert_eq!(after.deepest_verdict, before.deepest_verdict);
    assert_eq!(after.suspected, before.suspected);
    assert_eq!(
        after.summary, before.summary,
        "the sealed statement was rewritten by a suggestion"
    );
    assert_eq!(
        after.explanations.len(),
        3,
        "the suggestions are still offered"
    );

    // And the whole object differs in exactly one field.
    let mut stripped = after.clone();
    stripped.explanations = Vec::new();
    assert_eq!(stripped, before);
}

/// **The precise failure blind-first exists to prevent.** `tied` means two complete candidates sat
/// within the supersession margin and naming one would be a coin flip (ADR-0021 §7A.3). A band
/// plan is exactly the thing that would love to break that tie, and it may not: the reason stays
/// `tied`, whatever the suggestions score.
#[test]
fn a_tied_result_is_never_resolved_by_a_suggestion() {
    let before = sealed(ResolutionKind::Unknown, ResolutionReason::Tied);
    let mut after = before.clone();
    // One overwhelming suggestion — the tempting case.
    after.attach_explanations(vec![Explanation {
        source: ExplanationSource::Licence,
        identity: "a-licensed-assignment-at-this-exact-frequency".into(),
        score: 1.0,
        distance_hz: Some(0.0),
        status: ExplanationStatus::Expected,
        data_age_days: Some(0),
        reasoning: "a licence names this frequency".into(),
    }]);

    assert_eq!(after.reason, Some(ResolutionReason::Tied));
    assert_eq!(after.kind, ResolutionKind::Unknown);
    assert_eq!(after.summary, before.summary);
    assert_eq!(after.deepest_verdict, before.deepest_verdict);

    // Computing suggestions for a tied result does not narrow the tie either: the function is
    // handed a shared reference and hands back a list, and the list is not a decision.
    let t = table();
    let ex = explain_resolution(
        &after,
        &MeasuredEmission {
            center_hz: 88.5e6,
            bandwidth_hz: 180e3,
            family: Some("fm-broadcast".into()),
        },
        &refs(&t),
    );
    assert!(!ex.is_empty(), "a tied result is still explained");
    assert_eq!(after.reason, Some(ResolutionReason::Tied), "still tied");
}

/// The mismatch stays a **flag** (CLAUDE.md): an emission off the FM raster is `unexpected` and
/// keeps its measured centre. Nothing here snaps it to a channel.
#[test]
fn an_emission_off_the_fm_raster_is_flagged_and_keeps_its_measured_centre() {
    let t = table();
    // 88.65 MHz: 150 kHz above the 88.5 MHz assignment, 50 kHz from the nearest raster channel —
    // far outside the 10 % (20 kHz) tolerance either way.
    let measured = MeasuredEmission {
        center_hz: 88.65e6,
        bandwidth_hz: 180e3,
        family: Some("fm-broadcast".into()),
    };
    let res = sealed(ResolutionKind::Unknown, ResolutionReason::NothingScored);
    let ex = explain_resolution(&res, &measured, &refs(&t));

    let fm = ex
        .iter()
        .find(|e| e.identity.contains("fm"))
        .unwrap_or_else(|| panic!("an FM suggestion among {ex:#?}"));
    assert_eq!(fm.status, ExplanationStatus::Unexpected, "{fm:#?}");
    assert_eq!(fm.source, ExplanationSource::BandPlan);
    let d = fm.distance_hz.expect("a channel distance");
    assert!(
        (d.abs() - 50e3).abs() < 1.0,
        "distance {d} Hz from the nearest channel"
    );
    assert_eq!(
        fm.data_age_days,
        Some(41),
        "the age of the data it rests on"
    );
    assert!(
        fm.reasoning.contains("kept as measured"),
        "the reasoning must say the measurement stands: {}",
        fm.reasoning
    );
    // The measurement is untouched: `explain_resolution` never sees a mutable anything.
    assert_eq!(measured.center_hz, 88.65e6);
}

/// On-raster, expected family: the ordinary case, scored above the mismatch.
#[test]
fn an_on_raster_fm_emission_reads_expected_and_outranks_the_off_raster_one() {
    let t = table();
    let on = explain_resolution(
        &sealed(ResolutionKind::Unknown, ResolutionReason::NothingScored),
        &MeasuredEmission {
            center_hz: 88.5e6,
            bandwidth_hz: 180e3,
            family: Some("fm-broadcast".into()),
        },
        &refs(&t),
    );
    let top = on.first().expect("a suggestion");
    assert_eq!(top.status, ExplanationStatus::Expected, "{on:#?}");
    assert!(top.score > 0.9, "{top:#?}");
    assert!(on.len() <= MAX_EXPLANATIONS);

    let off = explain_resolution(
        &sealed(ResolutionKind::Unknown, ResolutionReason::NothingScored),
        &MeasuredEmission {
            center_hz: 88.65e6,
            bandwidth_hz: 180e3,
            family: Some("fm-broadcast".into()),
        },
        &refs(&t),
    );
    assert!(
        off[0].score < top.score,
        "an off-raster emission must not score like an on-raster one: {off:#?}"
    );
}

/// `not-searched` is not `unknown` (ADR-0021 §7A.4), so it is not explained either: a suggestion
/// beside an un-looked-at emitter reads as a finding about it.
#[test]
fn a_not_searched_resolution_is_never_explained() {
    let t = table();
    let ex = explain_resolution(
        &Resolution::not_searched(),
        &MeasuredEmission {
            center_hz: 88.5e6,
            bandwidth_hz: 180e3,
            family: Some("fm-broadcast".into()),
        },
        &refs(&t),
    );
    assert!(
        ex.is_empty(),
        "an un-looked-at emitter was explained: {ex:#?}"
    );
}

/// "Nothing covers this" is its own answer, and must never be served as "nothing is expected
/// here, so this is fine" — the same fail-safe `known_status` makes.
#[test]
fn no_covering_allocation_reads_as_no_reference_data_and_not_as_expected() {
    let t = table();
    // A frequency the compact table does not cover.
    let ex = explain_resolution(
        &sealed(ResolutionKind::Unknown, ResolutionReason::NothingScored),
        &MeasuredEmission {
            center_hz: 5.9e9,
            bandwidth_hz: 1e6,
            family: None,
        },
        &refs(&t),
    );
    if t.overlapping_center(5.9e9, 1e6).is_empty() {
        assert_eq!(ex.len(), 1, "{ex:#?}");
        assert_eq!(ex[0].status, ExplanationStatus::NoReferenceData);
        assert_eq!(ex[0].score, 0.0, "no reference data confirms nothing");
        assert!(ex[0].distance_hz.is_none());
    } else {
        // The bundled table grew to cover it: then every suggestion must still be a suggestion.
        assert!(ex.iter().all(|e| e.score <= 1.0));
    }
}

/// A family measured somewhere it is not expected is flagged, not corrected — the §9.2 mismatch
/// rule with the family rather than the frequency as the mismatch.
#[test]
fn a_family_that_does_not_belong_to_the_allocation_is_flagged_unexpected() {
    let t = table();
    let ex = explain_resolution(
        &sealed(ResolutionKind::Unknown, ResolutionReason::NothingScored),
        &MeasuredEmission {
            center_hz: 88.5e6,
            bandwidth_hz: 180e3,
            family: Some("adsb".into()),
        },
        &refs(&t),
    );
    assert!(
        ex.iter().any(|e| e.status == ExplanationStatus::Unexpected),
        "adsb in the FM band must read unexpected: {ex:#?}"
    );
}
