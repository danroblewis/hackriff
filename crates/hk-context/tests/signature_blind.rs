//! T-201: C18 signature matching, **blind** (docs/10 §3.2, CLAUDE.md).
//!
//! The rule these tests keep is the one that makes the whole feature honest: nothing is ever
//! looked up. No frequency, no class label and no generator parameter reaches the matcher — every
//! field it compares was *measured* from samples by the same code the pipeline runs (the C15
//! classifier for the family, the `features@1` vector for the spectral shape). The truth the
//! generator holds is used only to decide which assertion to make afterwards.
//!
//! That constraint shapes the test. The synthetic generator draws each emission's symbol rate from
//! its seed and never exposes it, so a hand-written catalogue entry could only be given the right
//! expected values by reading the truth — exactly the "look it up and then tune to it" move the
//! blind rule forbids. So the catalogue is **minted from a measurement** instead, which is also
//! how a real entry comes to exist (ADR-0016 §5: a `recipe-confirmed` signature is minted from an
//! emitter's measured features ± 3σ):
//!
//! 1. **Enrol.** Observe an emission several times at different SNRs, aggregate the measurements,
//!    and mint a catalogue entry from the aggregate's mean ± 3σ.
//! 2. **Recognise.** Observe the *same* emitter again, at an SNR and LO offset it was never
//!    enrolled at, measure it blind, and match. It should come back ranked first.
//! 3. **Reject.** Observe a generator deliberately outside the taxonomy and match it against the
//!    same catalogue. It must come back `none` — or at worst a low-scoring `partial` — never a
//!    confident identity.
//!
//! Step 3 is the one that matters most: an exploration tool that forces every unknown onto the
//! nearest catalogue entry is worse than one with no catalogue at all.

use hk_classify::features::{FeatureInput, features};
use hk_classify::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
use hk_classify::{Classifier, ClassifyRequest};
use hk_context::signature::{FeatureObservation, aggregate, match_signatures};
use hk_model::classify::TaxonomyRef;
use hk_model::classify::taxonomy::HK_MOD_V1;
use hk_model::signature::{
    EmissionFeatures, FULL_MATCH_MIN_SCORE, FieldExpect, FieldSpec, MatchOutcome, SIGNATURE_SCHEMA,
    Signature, SignatureKind, SignatureProvenance, field,
};
use hk_model::{EmitterId, Timestamp};
use std::collections::BTreeMap;

/// The measured fields these tests characterise an emission by. All three are spectral-shape
/// features the C18 card and ADR-0016 §5 name, and all three are measurable without a symbol
/// clock — so an emission is recognised by what it *looks* like, with no protocol knowledge.
const SHAPE_FIELDS: [&str; 3] = [field::FLATNESS, field::SYMMETRY, field::CARRIER_LINE_DB];

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

/// Measures one observation of an emission the way the pipeline would: the C15 classifier decides
/// the family, and `features@1` measures the spectral shape. **The class label is never read.**
fn observe(class: Class, snr_db: f64, seed: u64, lo_offset_hz: f64) -> FeatureObservation {
    let cfg = SynthConfig {
        lo_offset_hz,
        ..SynthConfig::new(snr_db, seed)
    };
    let s = generate(class, &cfg);

    let classification = Classifier::new().classify(&ClassifyRequest {
        obw_hz: Some(s.obw_hz),
        snr_db: Some(snr_db),
        ..ClassifyRequest::new(&s.samples, s.sample_rate_hz, t(0))
    });
    let shape = features(&FeatureInput {
        samples: &s.samples,
        sample_rate_hz: s.sample_rate_hz,
        obw_hz: Some(s.obw_hz),
        snr_db: Some(snr_db),
        symbols: None,
    });

    let mut obs = FeatureObservation::new()
        .num(field::OBW_HZ, s.obw_hz, 0.0, "c13")
        .with_classification(&classification);
    for name in SHAPE_FIELDS {
        if let Some(v) = shape.get(name) {
            obs = obs.num(name, v, 0.0, "features@1");
        }
    }
    obs
}

/// Aggregates several observations of one emitter into a features snapshot.
fn enrol(class: Class, snrs: &[f64], seed: u64) -> EmissionFeatures {
    aggregate(
        "features:enrolled",
        EmitterId::new(),
        t(1),
        snrs.iter().map(|snr| observe(class, *snr, seed, 0.0)),
    )
}

/// Mints a catalogue entry from an aggregate: each shape field becomes a `mean ± 3σ` range, with a
/// floor under σ so a field that happened to be stable across the enrolment does not produce an
/// impossibly tight tolerance (ADR-0016 §5's "measured features ± 3σ").
fn mint(id: &str, name: &str, f: &EmissionFeatures) -> Signature {
    let mut fields: BTreeMap<String, FieldSpec> = BTreeMap::new();
    for key in SHAPE_FIELDS {
        let Some(feat) = f.get(key) else { continue };
        let Some(mean) = feat.value.num() else {
            continue;
        };
        let sigma = feat.sigma.max(0.05 * mean.abs()).max(0.01);
        fields.insert(
            key.to_owned(),
            FieldSpec {
                expect: FieldExpect::Range {
                    lo: mean - 3.0 * sigma,
                    hi: mean + 3.0 * sigma,
                },
                tolerance: None,
                required: true,
                weight: 1.0,
            },
        );
    }
    // The family is carried only when the classifier actually named one that the taxonomy knows;
    // an `unknown` call must never be written into the catalogue as if it were a family.
    let family = f
        .get(field::FAMILY)
        .and_then(|x| x.value.text())
        .filter(|label| HK_MOD_V1.is_family(label))
        .map(str::to_owned);
    let required = fields.values().filter(|s| s.required).count() as u32;
    Signature {
        schema: SIGNATURE_SCHEMA,
        id: id.to_owned(),
        version: 1,
        name: name.to_owned(),
        kind: SignatureKind::Learned,
        taxonomy: Some(TaxonomyRef::current()),
        family,
        class: None,
        fields,
        min_discriminating: required.max(1),
        recipe: None,
        // Minted from a measurement of this device's own observations.
        provenance: SignatureProvenance::RecipeConfirmed,
        author: "t201-blind-test".into(),
        created_at: t(1),
        supersedes: None,
        bands_hz: Vec::new(),
        notes: None,
    }
}

/// A known emission, enrolled from its own measurements, is recognised again later — and the
/// ranking is driven entirely by measured parameters, never by a frequency or a label.
#[test]
fn a_known_emission_is_recognised_from_what_was_measured() {
    let seed = ACCEPTANCE_SEED_BASE + 11;
    let enrolled = enrol(Class::Fsk2, &[18.0, 24.0, 30.0], seed);
    enrolled.validate().expect("the aggregate is well formed");
    assert!(
        enrolled.observations == 3 && enrolled.present() >= 4,
        "enrolment measured several fields over several looks: {enrolled:?}"
    );
    let entry = mint("learned-fsk", "Learned FSK emitter", &enrolled);
    entry.validate().expect("a minted entry must be valid");

    // A later look at the same emitter, at an SNR and LO offset it was never enrolled at.
    let again = aggregate(
        "features:again",
        EmitterId::new(),
        t(2),
        [observe(Class::Fsk2, 21.0, seed, 900.0)],
    );
    let m = match_signatures(&again, std::slice::from_ref(&entry), 1, t(3));
    m.validate().unwrap();

    eprintln!(
        "[T-201] known emission: outcome {:?}, top {:?}, reasons {:?}",
        m.outcome,
        m.top().map(|c| (c.signature.id.clone(), c.score)),
        m.reasons
    );
    assert_ne!(
        m.outcome,
        MatchOutcome::None,
        "the enrolled emitter should still be recognised: {m:?}"
    );
    assert_eq!(
        m.top().unwrap().signature.id,
        "learned-fsk",
        "the minted entry ranks first: {m:?}"
    );
    assert!(
        m.top().unwrap().conflicting.is_empty(),
        "nothing about the same emitter should conflict: {m:?}"
    );
}

/// **The important one.** An emission from a generator deliberately outside the taxonomy is not
/// forced onto the nearest catalogue entry: it comes back `none`, or at worst a low-scoring
/// `partial`, and never a confident identity.
#[test]
fn a_genuinely_unknown_emission_is_not_forced_onto_the_nearest_entry() {
    let seed = ACCEPTANCE_SEED_BASE + 11;
    let catalogue = vec![mint(
        "learned-fsk",
        "Learned FSK emitter",
        &enrol(Class::Fsk2, &[18.0, 24.0, 30.0], seed),
    )];

    // Every held-out generator: out-of-taxonomy emissions the catalogue has never seen.
    for (i, class) in Class::HELD_OUT.iter().enumerate() {
        let unknown = aggregate(
            "features:unknown",
            EmitterId::new(),
            t(4),
            [observe(
                *class,
                24.0,
                ACCEPTANCE_SEED_BASE + 500 + i as u64,
                0.0,
            )],
        );
        let m = match_signatures(&unknown, &catalogue, 1, t(5));
        m.validate().unwrap();

        let top = m.top().map(|c| (c.signature.id.as_str(), c.score));
        eprintln!(
            "[T-201] held-out {}: outcome {:?}, top {top:?}, reasons {:?}",
            class.label(),
            m.outcome,
            m.reasons
        );
        assert_ne!(
            m.outcome,
            MatchOutcome::Full,
            "{} must never be identified as a known emitter: {m:?}",
            class.label()
        );
        if let Some((_, score)) = top {
            assert!(
                score < FULL_MATCH_MIN_SCORE,
                "{} scored {score} against an entry it has nothing to do with: {m:?}",
                class.label()
            );
        }
    }
}

/// The catalogue never invents an answer: with nothing in it, every emission — known or not —
/// comes back `none` rather than being described by whatever happens to be nearest.
#[test]
fn an_empty_catalogue_explains_nothing_rather_than_guessing() {
    let measured = aggregate(
        "features:any",
        EmitterId::new(),
        t(6),
        [observe(Class::Fsk2, 24.0, ACCEPTANCE_SEED_BASE + 11, 0.0)],
    );
    let m = match_signatures(&measured, &[], 0, t(7));
    m.validate().unwrap();
    assert_eq!(m.outcome, MatchOutcome::None);
    assert!(m.candidates.is_empty());
    // `none` means the catalogue has nothing to say — not that the emission is unknown.
    assert!(m.reasons.iter().any(|r| r == "no_candidate"), "{m:?}");
}
