//! T-201: what signature matching guarantees (ADR-0016 §5).
//!
//! The load-bearing tests here are the two the contract is *for*: a match cannot change anything
//! about the emitter it describes, and too few fields can never produce a confident identity
//! however well they agree. Both are enforced, not commented.

use std::collections::BTreeMap;

use hk_model::classify::TaxonomyRef;
use hk_model::cluster::{Fingerprint, Sighting};
use hk_model::emitter::LinkTarget;
use hk_model::ids::TrackId;
use hk_model::region::TimeRange;
use hk_model::signature::{
    EmissionFeatures, Feat, FieldExpect, FieldSpec, MatchOutcome, SIGNATURE_SCHEMA, Signature,
    SignatureKind, SignatureProvenance, field,
};
use hk_model::{EmitterId, Repository, Timestamp};

use super::*;

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

/// The POCSAG-1200 sync word, as a bit pattern.
const POCSAG_SYNC: &str = "01111100110100100001010111011000";

fn spec(expect: FieldExpect, required: bool, weight: f64) -> FieldSpec {
    FieldSpec {
        expect,
        tolerance: None,
        required,
        weight,
    }
}

fn signature(
    id: &str,
    name: &str,
    family: Option<&str>,
    fields: Vec<(&str, FieldSpec)>,
) -> Signature {
    let fields: BTreeMap<String, FieldSpec> =
        fields.into_iter().map(|(n, s)| (n.to_owned(), s)).collect();
    let required = fields.values().filter(|s| s.required).count() as u32;
    Signature {
        schema: SIGNATURE_SCHEMA,
        id: id.to_owned(),
        version: 1,
        name: name.to_owned(),
        kind: SignatureKind::Protocol,
        taxonomy: Some(TaxonomyRef::current()),
        family: family.map(str::to_owned),
        class: None,
        fields,
        min_discriminating: required.clamp(1, 3),
        recipe: None,
        provenance: SignatureProvenance::Builtin,
        author: "hackriff".into(),
        created_at: t(0),
        supersedes: None,
        bands_hz: Vec::new(),
        notes: None,
    }
}

/// POCSAG 1200: rate + deviation + sync, the C18 card's discriminating trio.
fn pocsag() -> Signature {
    signature(
        "pocsag-1200",
        "POCSAG 1200",
        Some("fsk"),
        vec![
            (
                field::SYMBOL_RATE_HZ,
                spec(FieldExpect::Value { value: 1200.0 }, true, 2.0),
            ),
            (
                field::DEVIATION_HZ,
                spec(
                    FieldExpect::Range {
                        lo: 4000.0,
                        hi: 4800.0,
                    },
                    true,
                    1.0,
                ),
            ),
            (
                field::SYNC_WORD,
                spec(
                    FieldExpect::Bits {
                        bits: POCSAG_SYNC.into(),
                        max_errors: 2,
                    },
                    true,
                    1.0,
                ),
            ),
        ],
    )
}

fn features_of(fields: Vec<(&str, Feat)>) -> EmissionFeatures {
    let mut f = EmissionFeatures::new("features:test", EmitterId::new(), t(10));
    f.observe(fields.into_iter().map(|(n, v)| (n.to_owned(), v)), false);
    f
}

/// Everything POCSAG needs, measured well.
fn full_pocsag_measurement() -> EmissionFeatures {
    features_of(vec![
        (field::FAMILY, Feat::text("fsk", "classifier")),
        (field::SYMBOL_RATE_HZ, Feat::num(1201.0, 1.0, "c14-cyclic")),
        (field::DEVIATION_HZ, Feat::num(4450.0, 50.0, "chain")),
        (field::SYNC_WORD, Feat::bits(POCSAG_SYNC, "framer")),
    ])
}

fn matched(features: &EmissionFeatures, catalogue: &[Signature]) -> hk_model::SignatureMatch {
    let m = match_signatures(features, catalogue, 1, t(11));
    m.validate().expect("a produced match must validate");
    m
}

#[test]
fn a_complete_well_measured_emission_matches_fully() {
    let m = matched(&full_pocsag_measurement(), &[pocsag()]);
    assert_eq!(m.outcome, MatchOutcome::Full, "{m:?}");
    let top = m.top().unwrap();
    assert_eq!(top.signature.id, "pocsag-1200");
    assert!(top.score >= 0.8, "{top:?}");
    assert!(top.missing.is_empty() && top.conflicting.is_empty());
}

/// **The degenerate case.** One or two fields must never identify anything, however exactly they
/// agree: the catalogue entry declares three discriminating fields and the matcher floors that at
/// [`MATCH_MIN_DISCRIMINATING`] besides. The answer is a ranked `partial`, which is what a MAUTO
/// search then uses to decide what to go and measure next.
#[test]
fn too_few_fields_yield_a_partial_with_candidates_never_an_identity() {
    // One field, dead on.
    let one = features_of(vec![(
        field::SYMBOL_RATE_HZ,
        Feat::num(1200.0, 0.0, "c14-cyclic"),
    )]);
    let m = matched(&one, &[pocsag()]);
    assert_eq!(m.outcome, MatchOutcome::Partial, "{m:?}");
    assert!(!m.candidates.is_empty(), "candidates are still ranked");
    assert_eq!(m.top().unwrap().signature.id, "pocsag-1200");
    assert!(m.reasons.iter().any(|r| r == "too_few_fields"), "{m:?}");
    assert_eq!(
        m.top().unwrap().missing,
        vec![field::DEVIATION_HZ, field::SYNC_WORD],
        "the match names what it would still need"
    );

    // Two fields, both dead on: still not an identity.
    let two = features_of(vec![
        (field::SYMBOL_RATE_HZ, Feat::num(1200.0, 0.0, "c14-cyclic")),
        (field::DEVIATION_HZ, Feat::num(4400.0, 0.0, "chain")),
    ]);
    let m = matched(&two, &[pocsag()]);
    assert_eq!(m.outcome, MatchOutcome::Partial, "{m:?}");
    assert!(m.top().unwrap().score < 0.8);

    // The third field is what makes it a full match.
    assert_eq!(
        matched(&full_pocsag_measurement(), &[pocsag()]).outcome,
        MatchOutcome::Full
    );
}

/// A catalogue entry may not declare itself identifiable from one field: the floor is the
/// matcher's, not the entry's, so an untrusted import cannot lower the bar.
#[test]
fn a_signature_cannot_lower_the_discriminating_floor() {
    let greedy = signature(
        "greedy",
        "One field is enough, honest",
        None,
        vec![(
            field::SYMBOL_RATE_HZ,
            spec(FieldExpect::Value { value: 1200.0 }, true, 1.0),
        )],
    );
    assert_eq!(greedy.min_discriminating, 1, "the entry asks for one field");
    let one = features_of(vec![(
        field::SYMBOL_RATE_HZ,
        Feat::num(1200.0, 0.0, "c14-cyclic"),
    )]);
    let m = matched(&one, &[greedy]);
    assert_ne!(m.outcome, MatchOutcome::Full, "{m:?}");
    assert!(m.reasons.iter().any(|r| r == "too_few_fields"), "{m:?}");
}

/// The C18 card's P25/DMR near-collision: both run at 4800 Bd and differ in deviation and sync
/// word. Until the sync word is measured, neither is the answer — both stay ranked.
#[test]
fn the_p25_dmr_near_collision_stays_partial_with_both_ranked() {
    let p25 = signature(
        "p25-c4fm",
        "P25 C4FM",
        Some("fsk"),
        vec![
            (
                field::SYMBOL_RATE_HZ,
                spec(FieldExpect::Value { value: 4800.0 }, true, 1.0),
            ),
            (
                field::DEVIATION_HZ,
                spec(FieldExpect::Value { value: 1800.0 }, true, 1.0),
            ),
            (
                field::SYNC_WORD,
                spec(
                    FieldExpect::Bits {
                        bits: "0101010111110000".into(),
                        max_errors: 1,
                    },
                    true,
                    1.0,
                ),
            ),
        ],
    );
    let dmr = signature(
        "dmr",
        "DMR",
        Some("fsk"),
        vec![
            (
                field::SYMBOL_RATE_HZ,
                spec(FieldExpect::Value { value: 4800.0 }, true, 1.0),
            ),
            (
                field::DEVIATION_HZ,
                spec(FieldExpect::Value { value: 1944.0 }, true, 1.0),
            ),
            (
                field::SYNC_WORD,
                spec(
                    FieldExpect::Bits {
                        bits: "1111000010101010".into(),
                        max_errors: 1,
                    },
                    true,
                    1.0,
                ),
            ),
        ],
    );

    // Rate and deviation measured, sync word not yet: the two are not separable.
    let ambiguous = features_of(vec![
        (field::FAMILY, Feat::text("fsk", "classifier")),
        (field::SYMBOL_RATE_HZ, Feat::num(4800.0, 5.0, "c14-cyclic")),
        (field::DEVIATION_HZ, Feat::num(1870.0, 120.0, "chain")),
    ]);
    let m = matched(&ambiguous, &[p25.clone(), dmr.clone()]);
    assert_eq!(m.outcome, MatchOutcome::Partial, "{m:?}");
    assert_eq!(m.candidates.len(), 2, "both stay ranked: {m:?}");
    assert!(
        m.candidates
            .iter()
            .all(|c| c.missing == vec![field::SYNC_WORD]),
        "{m:?}"
    );

    // Measure the sync word and the ambiguity resolves — on evidence, not on a guess.
    let mut decided = ambiguous.clone();
    decided.fold(field::SYNC_WORD, Feat::bits("0101010111110000", "framer"));
    let m = matched(&decided, &[p25, dmr]);
    assert_eq!(m.outcome, MatchOutcome::Full, "{m:?}");
    assert_eq!(m.top().unwrap().signature.id, "p25-c4fm");
    assert_eq!(m.candidates.len(), 1, "DMR's sync word rules it out");
}

/// Two entries that both fit every measured field completely: neither is an identity.
#[test]
fn two_entries_that_both_fit_completely_are_ambiguous_not_an_identity() {
    let fields = || {
        vec![
            (
                field::SYMBOL_RATE_HZ,
                spec(FieldExpect::Value { value: 2400.0 }, true, 1.0),
            ),
            (
                field::DEVIATION_HZ,
                spec(
                    FieldExpect::Range {
                        lo: 2000.0,
                        hi: 3000.0,
                    },
                    true,
                    1.0,
                ),
            ),
            (
                field::PERIOD_S,
                spec(FieldExpect::Value { value: 30.0 }, true, 1.0),
            ),
        ]
    };
    let a = signature("sensor-a", "Sensor A", Some("fsk"), fields());
    let b = signature("sensor-b", "Sensor B", Some("fsk"), fields());
    let measured = features_of(vec![
        (field::SYMBOL_RATE_HZ, Feat::num(2400.0, 2.0, "c14-cyclic")),
        (field::DEVIATION_HZ, Feat::num(2500.0, 50.0, "chain")),
        (field::PERIOD_S, Feat::num(30.0, 0.1, "track-timing")),
    ]);
    let m = matched(&measured, &[a, b]);
    assert_eq!(m.outcome, MatchOutcome::Partial, "{m:?}");
    assert!(
        m.reasons.iter().any(|r| r == "ambiguous_candidates"),
        "{m:?}"
    );
    assert_eq!(m.candidates.len(), 2);
}

/// Measurement uncertainty widens the tolerance, so a badly measured field cannot manufacture a
/// conflict — the aggregation in [`super::features`] and the matching are one contract.
#[test]
fn a_poorly_measured_field_cannot_manufacture_a_conflict() {
    let confident = features_of(vec![
        (field::SYMBOL_RATE_HZ, Feat::num(1200.0, 0.0, "c14-cyclic")),
        (field::DEVIATION_HZ, Feat::num(6000.0, 0.0, "chain")),
        (field::SYNC_WORD, Feat::bits(POCSAG_SYNC, "framer")),
    ]);
    let m = matched(&confident, &[pocsag()]);
    assert_ne!(m.outcome, MatchOutcome::Full, "6 kHz is out of range");

    // The same reading, honestly reported as barely measured, agrees instead of disagreeing.
    let uncertain = features_of(vec![
        (field::SYMBOL_RATE_HZ, Feat::num(1200.0, 0.0, "c14-cyclic")),
        (field::DEVIATION_HZ, Feat::num(6000.0, 2000.0, "chain")),
        (field::SYNC_WORD, Feat::bits(POCSAG_SYNC, "framer")),
    ]);
    let m = matched(&uncertain, &[pocsag()]);
    let dev = m
        .top()
        .unwrap()
        .agreement
        .iter()
        .find(|a| a.field == field::DEVIATION_HZ)
        .unwrap();
    assert!(
        dev.ok,
        "an uncertain reading is not evidence against: {dev:?}"
    );
}

/// A different modulation family rules an entry out; an `unknown` one never does, and a band
/// never does — an emission in the "wrong" band is the interesting case.
#[test]
fn family_gates_but_unknown_and_bands_do_not() {
    let mut entry = pocsag();
    entry.bands_hz = vec![[137e6, 175e6]];

    let elsewhere = |family: &str| {
        features_of(vec![
            (field::FAMILY, Feat::text(family, "classifier")),
            (field::F_CENTER_HZ, Feat::num(915e6, 100.0, "detector")),
            (field::SYMBOL_RATE_HZ, Feat::num(1200.0, 1.0, "c14-cyclic")),
            (field::DEVIATION_HZ, Feat::num(4450.0, 50.0, "chain")),
            (field::SYNC_WORD, Feat::bits(POCSAG_SYNC, "framer")),
        ])
    };

    // Far outside the entry's band, and it still matches: bands are rank-only.
    let m = matched(&elsewhere("fsk"), &[entry.clone()]);
    assert_eq!(m.outcome, MatchOutcome::Full, "bands must not gate: {m:?}");

    // A different family rules it out.
    let m = matched(&elsewhere("psk-qam"), &[entry.clone()]);
    assert_eq!(m.outcome, MatchOutcome::None, "{m:?}");
    assert!(m.candidates.is_empty());

    // `unknown` gates nothing: not measured is not ruled out.
    let m = matched(&elsewhere("unknown"), &[entry]);
    assert_eq!(m.outcome, MatchOutcome::Full, "{m:?}");
}

/// A genuinely unknown emission is not forced onto the nearest entry.
#[test]
fn an_unrelated_emission_matches_nothing_rather_than_the_nearest_entry() {
    let unknown = features_of(vec![
        (field::FAMILY, Feat::text("fsk", "classifier")),
        (
            field::SYMBOL_RATE_HZ,
            Feat::num(38_400.0, 50.0, "c14-cyclic"),
        ),
        (field::DEVIATION_HZ, Feat::num(20_000.0, 500.0, "chain")),
        (field::SYNC_WORD, Feat::bits("1100110011001100", "framer")),
    ]);
    let m = matched(&unknown, &[pocsag()]);
    assert_eq!(m.outcome, MatchOutcome::None, "{m:?}");
    assert!(m.candidates.is_empty(), "nothing is offered as an identity");
    assert!(m.reasons.iter().any(|r| r == "no_candidate"), "{m:?}");
}

fn an_emitter(r: &mut Repository) -> EmitterId {
    r.record_sighting(
        &Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(0), t(1)),
            count: 3,
            f_center_hz: 148.5e6,
            bandwidth_hz: 12.5e3,
            fingerprint: Some(Fingerprint::new(148.5e6, 12.5e3)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        },
        None,
    )
    .unwrap()
    .emitter_id
}

/// **The contract, enforced.** Matching an emitter against the catalogue changes *nothing* about
/// the emitter: not its identity, its known status, its lifecycle, its family, its tags or its
/// classification history. The catalogue suggests; measurement decides (CLAUDE.md).
#[test]
fn a_match_never_changes_anything_about_the_emitter() {
    let mut r = Repository::open_in_memory().unwrap();
    let id = an_emitter(&mut r);
    r.insert_signature(&pocsag()).unwrap();

    let mut features = full_pocsag_measurement();
    features.emitter_id = id;
    features.id = "features:emitter".into();
    r.put_emission_features(&features).unwrap();

    // Everything observable about the emitter, before.
    let before = format!("{:?}", r.emitter(id).unwrap());
    let status_before = r.known_status_history(id).unwrap().len();
    let class_before = r.current_classification(id).unwrap().is_some();

    let m = match_emitter(&mut r, id, t(20)).unwrap().unwrap();
    assert_eq!(m.outcome, MatchOutcome::Full, "the match itself is strong");

    // ...and after. A `full` match is the strongest verdict the catalogue can reach, so if
    // anything could leak into the inventory, it would leak here.
    assert_eq!(format!("{:?}", r.emitter(id).unwrap()), before);
    assert_eq!(r.known_status_history(id).unwrap().len(), status_before);
    assert_eq!(
        r.current_classification(id).unwrap().is_some(),
        class_before
    );
    assert!(
        r.emitter(id).unwrap().identity == hk_model::Identity::Unknown,
        "a match never names an emitter"
    );

    // The match is recorded as evidence, and is re-read as the emitter's current one.
    let stored = r.current_signature_match(id).unwrap().unwrap();
    assert_eq!(stored.outcome, MatchOutcome::Full);
    assert_eq!(stored.features_ref.as_deref(), Some("features:emitter"));

    // Re-running on an unchanged measurement appends no duplicate row.
    match_emitter(&mut r, id, t(21)).unwrap().unwrap();
    assert_eq!(r.signature_matches(id, 10).unwrap().len(), 1);
}

#[test]
fn an_emitter_with_no_measurement_has_no_match_at_all() {
    let mut r = Repository::open_in_memory().unwrap();
    let id = an_emitter(&mut r);
    r.insert_signature(&pocsag()).unwrap();
    assert!(match_emitter(&mut r, id, t(20)).unwrap().is_none());
    assert!(r.current_signature_match(id).unwrap().is_none());
}

#[test]
fn the_catalogue_serves_current_versions_and_its_revision_moves_on_every_write() {
    let mut r = Repository::open_in_memory().unwrap();
    let rev0 = r.signatures_rev().unwrap();
    r.insert_signature(&pocsag()).unwrap();
    let rev1 = r.signatures_rev().unwrap();
    assert!(rev1 > rev0);

    let mut v2 = pocsag();
    v2.version = 2;
    v2.supersedes = Some(1);
    v2.name = "POCSAG 1200 (revised)".into();
    r.insert_signature(&v2).unwrap();
    let current = r.signatures().unwrap();
    assert_eq!(current.len(), 1, "one current entry per id");
    assert_eq!(current[0].version, 2);
    // Every version stays readable, so an old match can still be explained.
    assert_eq!(r.signature("pocsag-1200", 1).unwrap().version, 1);

    // Retiring is a catalogue write and removes the entry from the current set.
    r.retire_signature("pocsag-1200", 2, t(30)).unwrap();
    assert!(r.signatures_rev().unwrap() > rev1);
    assert_eq!(
        r.signatures().unwrap()[0].version,
        1,
        "the previous version is current again"
    );
}

#[test]
fn a_signature_is_never_minted_from_an_all_suspect_emitter() {
    let mut clean = EmissionFeatures::new("features:clean", EmitterId::new(), t(10));
    clean.observe([], false);
    assert!(may_mint_from(&clean));

    let mut suspect = EmissionFeatures::new("features:suspect", EmitterId::new(), t(10));
    suspect.observe([], true);
    suspect.observe([], true);
    assert!(!may_mint_from(&suspect));

    // It still *matches* — the operator should see what a suspect emission resembles — but the
    // flag travels with the verdict.
    let mut features = full_pocsag_measurement();
    features.suspect_fraction = 1.0;
    let m = matched(&features, &[pocsag()]);
    assert!(m.reasons.iter().any(|r| r == "all_suspect"), "{m:?}");
}

/// Aggregation feeds matching: a fingerprint and a classification become the measured fields.
#[test]
fn observations_aggregate_into_the_fields_the_matcher_compares() {
    let fp = Fingerprint {
        family: Some("2fsk".into()),
        symbol_rate_hz: Some(1200.0),
        deviation_hz: Some(4450.0),
        ..Fingerprint::new(148.5e6, 12.5e3)
    };
    let features = aggregate(
        "features:agg",
        EmitterId::new(),
        t(10),
        [
            FeatureObservation::from_fingerprint(&fp, "tracker"),
            FeatureObservation::from_fingerprint(&fp, "tracker").bits(
                field::SYNC_WORD,
                POCSAG_SYNC,
                "framer",
            ),
        ],
    );
    features.validate().unwrap();
    assert_eq!(features.observations, 2);
    assert_eq!(features.num(field::SYMBOL_RATE_HZ), Some(1200.0));

    // `2fsk` is a class of the `fsk` family, so the family gate accepts it.
    let m = matched(&features, &[pocsag()]);
    assert_eq!(m.outcome, MatchOutcome::Full, "{m:?}");
}
