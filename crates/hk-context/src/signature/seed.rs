//! Assembling the MAUTO [`SearchSeed`] for one emitter (T-215, ADR-0016 §8).
//!
//! The rules — the posterior orders, only the likelihood prunes, the open share is always reserved
//! — live with the contract in [`hk_model::classify::seed`]. This module is the repository-backed
//! way to gather what they run on: the current classification, the measured features, what the
//! catalogue said, and the cluster of unknowns the emitter belongs to.
//!
//! **It reads, and only reads.** [`seed_emitter`] takes a shared `&Repository`, so seeding a
//! search cannot write a classification, mint a signature, promote a cluster or start a pipeline —
//! the type system says so, not a comment. It also runs no search: what a search does with the
//! seed is ADR-0015's business, and MAUTO is unscheduled.

use hk_model::classify::seed::{ClusterPipeline, ClusterSeed, SearchSeed, SeedInputs};
use hk_model::signature::cluster::SignatureCluster;
use hk_model::time::Timestamp;
use hk_model::{EmitterId, RepoError, Repository};

/// Gathers everything M3 knows about `emitter_id` into a [`SearchSeed`].
///
/// Returns `None` when the emitter has no M3 classification yet: nothing characterised is nothing
/// to seed a search from, which is not the same as "search nothing" and must not be recorded as
/// one. A pre-M3 legacy classification row (no `detail`) reads the same way.
pub fn seed_emitter(
    repo: &Repository,
    emitter_id: EmitterId,
    t: Timestamp,
) -> Result<Option<SearchSeed>, RepoError> {
    let live = repo.live_emitter_id(emitter_id)?;
    let Some(recorded) = repo.current_classification(live)? else {
        return Ok(None);
    };
    let Some(classification) = recorded.detail else {
        return Ok(None);
    };

    let features = repo.emitter_features(live)?;
    let signature_match = repo.current_signature_match(live)?;
    let catalogue = repo.signatures()?;
    let cluster = cluster_seed(repo, live)?;

    Ok(Some(SearchSeed::assemble(SeedInputs {
        emitter: live,
        t,
        classification: &classification,
        features: features.as_ref(),
        signature_match: signature_match.as_ref(),
        catalogue: &catalogue,
        cluster: cluster.as_ref(),
    })))
}

/// The emitter's cluster of unknowns, resolved through any merge.
fn cluster_seed(repo: &Repository, emitter: EmitterId) -> Result<Option<ClusterSeed>, RepoError> {
    let Some(id) = repo.emitter_cluster_id(emitter)? else {
        return Ok(None);
    };
    let id = repo.live_cluster_id(&id)?;
    let Some(cluster) = repo.cluster_opt(&id)? else {
        return Ok(None);
    };
    let best_pipeline = best_pipeline(repo, &cluster)?;
    Ok(Some(ClusterSeed {
        cluster_id: cluster.id.clone(),
        state: cluster.state,
        members: repo.cluster_members(&cluster.id)?,
        best_pipeline,
    }))
}

/// What a cluster has to offer its members: the recipe of the signature it was promoted to
/// (ADR-0016 §8, "cluster reuse").
///
/// A cluster that has not been promoted, or whose signature binds no recipe, offers nothing — and
/// offering nothing is not evidence about the emission, only silence about it.
fn best_pipeline(
    repo: &Repository,
    cluster: &SignatureCluster,
) -> Result<Option<ClusterPipeline>, RepoError> {
    let Some(signature) = cluster.signature.clone() else {
        return Ok(None);
    };
    let entry = repo.signature(&signature.id, signature.version)?;
    let Some(recipe) = entry.recipe.clone() else {
        return Ok(None);
    };
    Ok(Some(ClusterPipeline {
        recipe,
        family: entry.family.clone(),
        signature: Some(signature),
        // A seed never invents an evidence score: only a search that ran can report one.
        evidence_score: None,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use hk_model::classify::seed::{OPEN_SEARCH_MIN_SHARE, SeedBoost};
    use hk_model::classify::{
        ArbRank, CLASSIFICATION_SCHEMA, ClassProvenance, Classification, Coarse, HK_MOD_V1, LabelP,
        Stage, THRESHOLDS_VERSION, TaxonomyRef, UNKNOWN, entropy_norm,
    };
    use hk_model::signature::cluster::{ClusterState, EmitterClusterLink, SignatureCluster};
    use hk_model::signature::{
        EMISSION_FEATURES_VERSION, EmissionFeatures, Feat, FieldExpect, FieldSpec, MatchOutcome,
        RecipeRef, SIGNATURE_SCHEMA, Signature, SignatureCandidate, SignatureKind, SignatureMatch,
        SignatureProvenance, SignatureRef, field,
    };
    use hk_model::{
        EmitterId, Fingerprint, LinkTarget, Repository, Sighting, TimeRange, Timestamp, TrackId,
    };

    use super::*;

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    fn an_emitter(r: &mut Repository, f_hz: f64) -> EmitterId {
        r.record_sighting(
            &Sighting {
                source: LinkTarget::Track(TrackId::new()),
                seen: TimeRange::new(t(0), t(10)),
                count: 3,
                f_center_hz: f_hz,
                bandwidth_hz: 36e3,
                fingerprint: Some(Fingerprint::new(f_hz, 36e3)),
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

    /// A legal M3 classification putting `top_p` on `top` and sharing the rest out evenly.
    fn a_classification(top: &str, top_p: f64, snr_db: Option<f64>) -> Classification {
        let mut labels: Vec<String> = HK_MOD_V1
            .families
            .iter()
            .map(|f| f.name.to_owned())
            .collect();
        labels.push(UNKNOWN.to_owned());
        let rest = (1.0 - top_p) / (labels.len() - 1) as f64;
        let dist: Vec<LabelP> = labels
            .into_iter()
            .map(|label| {
                let p = if label == top { top_p } else { rest };
                LabelP { label, p }
            })
            .collect();

        let c = Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: t(10),
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse: HK_MOD_V1.coarse_of(top).unwrap_or(Coarse::Unknown),
            entropy_norm: entropy_norm(&dist, HK_MOD_V1.families.len() + 1),
            likelihood: dist.clone(),
            posterior: dist,
            prior: None,
            family: top.to_owned(),
            confidence: top_p,
            class: None,
            open_set_score: 0.1,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: "hk-classify/tree@1".to_owned(),
                features_version: EMISSION_FEATURES_VERSION,
                features_ref: None,
                ml: None,
                snr_db,
                snr_gate_db: 0.0,
                gated: false,
                thresholds: THRESHOLDS_VERSION.to_owned(),
                suspect: Default::default(),
                power_mode: None,
            },
            flags: Vec::new(),
            reasons: Vec::new(),
        };
        c.validate().expect("fixture must be a legal row");
        c
    }

    /// Asserts the seed carried the stored call through untouched: same family, same confidence,
    /// same distributions. `entropy_norm` is compared with a tolerance because it is a derived
    /// number that loses its last bit crossing SQLite — everything the rule is about is exact.
    fn assert_same_call(got: &Classification, want: &Classification) {
        assert_eq!(got.family, want.family, "the family was rewritten");
        assert_eq!(got.confidence, want.confidence);
        assert_eq!(got.coarse, want.coarse);
        assert_eq!(got.class, want.class);
        assert_eq!(got.posterior, want.posterior, "the posterior was rewritten");
        assert_eq!(
            got.likelihood, want.likelihood,
            "the evidence was rewritten"
        );
        assert_eq!(got.open_set_score, want.open_set_score);
        assert!((got.entropy_norm - want.entropy_norm).abs() < 1e-9);
    }

    fn store_features(r: &mut Repository, id: EmitterId) -> EmissionFeatures {
        let mut f = EmissionFeatures::new("features:1", id, t(10));
        f.observe(
            [
                (
                    field::SYMBOL_RATE_HZ.to_owned(),
                    Feat::num(4800.0, 20.0, "c14"),
                ),
                (
                    field::DEVIATION_HZ.to_owned(),
                    Feat::num(2400.0, 50.0, "c14"),
                ),
            ],
            false,
        );
        r.put_emission_features(&f).unwrap();
        f
    }

    fn a_signature(id: &str, family: &str) -> Signature {
        Signature {
            schema: SIGNATURE_SCHEMA,
            id: id.to_owned(),
            version: 1,
            name: format!("test {id}"),
            kind: SignatureKind::Protocol,
            taxonomy: Some(TaxonomyRef::current()),
            family: Some(family.to_owned()),
            class: None,
            // A signature with no fields would match everything, so the catalogue refuses one.
            fields: BTreeMap::from([
                (
                    field::SYMBOL_RATE_HZ.to_owned(),
                    FieldSpec::required(FieldExpect::Value { value: 4800.0 }),
                ),
                (
                    field::DEVIATION_HZ.to_owned(),
                    FieldSpec::required(FieldExpect::Value { value: 2400.0 }),
                ),
            ]),
            min_discriminating: 2,
            recipe: Some(RecipeRef {
                id: "test-recipe".to_owned(),
                version: 1,
            }),
            provenance: SignatureProvenance::Builtin,
            author: "test".to_owned(),
            created_at: t(0),
            supersedes: None,
            bands_hz: Vec::new(),
            notes: None,
        }
    }

    /// A match against `sig`. A `full` match has nothing missing by definition — what a `partial`
    /// still needs is exactly what a search must go and estimate.
    fn a_match(emitter: EmitterId, outcome: MatchOutcome, sig: &Signature) -> SignatureMatch {
        let missing = match outcome {
            MatchOutcome::Full => Vec::new(),
            _ => vec![field::SYNC_WORD.to_owned()],
        };
        let m = SignatureMatch {
            schema: SIGNATURE_SCHEMA,
            emitter_id: emitter,
            t: t(10),
            outcome,
            features_ref: Some("features:1".to_owned()),
            signatures_rev: 1,
            candidates: vec![SignatureCandidate {
                signature: SignatureRef {
                    id: sig.id.clone(),
                    version: sig.version,
                },
                name: sig.name.clone(),
                score: 0.9,
                agreement: Vec::new(),
                missing,
                conflicting: Vec::new(),
                recipe: sig.recipe.clone(),
            }],
            reasons: Vec::new(),
        };
        m.validate().expect("the fixture must be a legal match");
        m
    }

    #[test]
    fn an_emitter_without_a_classification_has_no_seed() {
        let mut r = Repository::open_in_memory().unwrap();
        let e = an_emitter(&mut r, 433.92e6);
        assert_eq!(seed_emitter(&r, e, t(20)).unwrap(), None);
    }

    #[test]
    fn a_seed_gathers_what_m3_knows_and_keeps_the_rules() {
        let mut r = Repository::open_in_memory().unwrap();
        let e = an_emitter(&mut r, 433.92e6);
        let c = a_classification("fsk", 0.7, Some(30.0));
        r.record_classification(e, &c, ArbRank::Classifier).unwrap();
        let features = store_features(&mut r, e);
        let sig = a_signature("test-sig", "fsk");
        r.insert_signature(&sig).unwrap();
        r.append_signature_match(&a_match(e, MatchOutcome::Full, &sig))
            .unwrap();

        let seed = seed_emitter(&r, e, t(20)).unwrap().expect("a seed");
        seed.validate().expect("the three rules must hold");

        assert_eq!(seed.emitter, e);
        assert_same_call(&seed.classification, &c);
        assert_eq!(seed.features.as_ref(), Some(&features));
        assert_eq!(
            seed.signature_match.as_ref().map(|m| m.outcome),
            Some(MatchOutcome::Full)
        );
        assert!(
            seed.budget_hint.open_search_min_share >= OPEN_SEARCH_MIN_SHARE,
            "open search must be funded even with a full match"
        );
        assert!(
            seed.budget_hint.families_ordered.len() >= HK_MOD_V1.families.len(),
            "every family is offered as a hypothesis"
        );
        assert!(
            seed.budget_hint.families_ordered.len() <= HK_MOD_V1.families.len() + 1,
            "and nothing is invented"
        );
        assert!(
            seed.budget_hint
                .families_ordered
                .iter()
                .all(|h| h.family != UNKNOWN)
        );
    }

    /// The end-to-end version of the rule: a `full` match on a family the evidence ranks low may
    /// move it to the front of the queue, and may do nothing else.
    #[test]
    fn a_full_match_raises_its_family_without_changing_the_numbers() {
        let mut r = Repository::open_in_memory().unwrap();
        let e = an_emitter(&mut r, 433.92e6);
        // The evidence likes `analog`; the catalogue matches an `ook-ask` entry.
        let c = a_classification("analog", 0.8, Some(30.0));
        r.record_classification(e, &c, ArbRank::Classifier).unwrap();
        store_features(&mut r, e);

        let bare = seed_emitter(&r, e, t(20)).unwrap().expect("a seed");
        assert_eq!(
            bare.budget_hint.families_ordered[0].family, "analog",
            "the posterior orders when nothing raises anything"
        );

        let sig = a_signature("ook-sig", "ook-ask");
        r.insert_signature(&sig).unwrap();
        r.append_signature_match(&a_match(e, MatchOutcome::Full, &sig))
            .unwrap();
        let seeded = seed_emitter(&r, e, t(21)).unwrap().expect("a seed");
        seeded.validate().unwrap();

        let top = &seeded.budget_hint.families_ordered[0];
        assert_eq!(top.family, "ook-ask", "a full match is tried first");
        assert!(top.boosts.contains(&SeedBoost::SignatureFull));
        assert_eq!(top.recipes, vec![sig.recipe.clone().unwrap()]);
        assert!(
            top.missing.is_empty(),
            "a full match needs nothing estimated"
        );

        // It set no identity and moved no number.
        // A match must never rewrite the classification.
        assert_same_call(&seeded.classification, &c);
        let names = |s: &SearchSeed| -> BTreeSet<String> {
            s.budget_hint
                .families_ordered
                .iter()
                .map(|h| h.family.clone())
                .collect()
        };
        assert_eq!(names(&bare), names(&seeded), "no hypothesis was removed");
        for h in &bare.budget_hint.families_ordered {
            let after = seeded
                .budget_hint
                .families_ordered
                .iter()
                .find(|o| o.family == h.family)
                .unwrap();
            assert_eq!(h.posterior, after.posterior);
            assert_eq!(h.likelihood, after.likelihood);
            assert_eq!(h.prune, after.prune);
        }
    }

    /// A promoted cluster hands its members a recipe to warm-start from, and nothing more.
    #[test]
    fn a_cluster_offers_its_pipeline_without_setting_a_family() {
        let mut r = Repository::open_in_memory().unwrap();
        let e = an_emitter(&mut r, 433.92e6);
        let c = a_classification("analog", 0.8, Some(30.0));
        r.record_classification(e, &c, ArbRank::Classifier).unwrap();
        store_features(&mut r, e);

        let sig = a_signature("cluster-sig", "fsk");
        r.insert_signature(&sig).unwrap();
        let mut cluster = SignatureCluster::new("cluster:0199aaaa", t(5));
        cluster.state = ClusterState::Promoted;
        cluster.signature = Some(SignatureRef {
            id: sig.id.clone(),
            version: sig.version,
        });
        r.put_cluster(&cluster).unwrap();
        r.link_emitter_cluster(&EmitterClusterLink {
            emitter_id: e,
            cluster_id: Some(cluster.id.clone()),
            t: t(6),
            reason: "joined".to_owned(),
            distance: Some(0.4),
        })
        .unwrap();

        let seed = seed_emitter(&r, e, t(20)).unwrap().expect("a seed");
        seed.validate().unwrap();

        let cs = seed.cluster.as_ref().expect("the cluster");
        assert_eq!(cs.cluster_id, cluster.id);
        assert_eq!(cs.state, ClusterState::Promoted);
        assert!(cs.members.contains(&e));
        let pipeline = cs.best_pipeline.as_ref().expect("a warm start");
        assert_eq!(pipeline.recipe, sig.recipe.clone().unwrap());
        assert_eq!(pipeline.family.as_deref(), Some("fsk"));
        assert_eq!(
            pipeline.evidence_score, None,
            "a seed never invents an evidence score"
        );

        let fsk = seed
            .budget_hint
            .families_ordered
            .iter()
            .find(|h| h.family == "fsk")
            .unwrap();
        assert!(fsk.boosts.contains(&SeedBoost::Cluster));
        // A cluster must never rewrite the classification.
        assert_same_call(&seed.classification, &c);
        assert_eq!(
            seed.classification.family, "analog",
            "the measured call stands; the cluster only suggests an order"
        );
    }
}
