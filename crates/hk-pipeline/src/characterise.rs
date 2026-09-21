//! T-242: the pipeline's single characterisation call site — where an observed emission's measured
//! features are aggregated, matched against the C18 signature catalogue (T-201) and offered to the
//! clustering of unknowns (T-202).
//!
//! T-201 landed [`hk_context::signature::match_emitter`] and T-202
//! [`hk_context::signature::assign_emitter`], but nothing in the pipeline called either: a normal
//! run wrote no match rows and no cluster memberships, so `/api/signatures/match` and
//! `/api/clusters` served nothing however much the radio saw. This module is the one place that
//! calls both, from the inventory seam ([`crate::inventory::TrackInventory`]) as emissions are
//! observed — one place rather than two, because a match and a cluster are two readings of the
//! *same* measurement and must be taken from the same snapshot.
//!
//! # What it writes, and what it must never write
//!
//! One [`EmissionFeatures`] snapshot per re-measurement, then a [`SignatureMatch`] and a cluster
//! [`Assignment`] derived from it. All three are **evidence about** an emitter, never a change
//! **to** one: no identity, no family, no `known_status`, no lifecycle state (CLAUDE.md's
//! exploration-first rule; ADR-0016 §5). The invariants T-201 and T-202 pinned in isolation are
//! re-proven here under pipeline conditions, in [`tests`].
//!
//! # Where it runs, and what bounds its cost
//!
//! **Off the ring, the DSP readers and the audio chains** — on the `hk-detect-writer` thread
//! (`crate::detect`) and the chain writer threads, under the repository lock, exactly where
//! [`crate::family::explain_emitter`] and the C15 classifier call site ([`crate::classify`])
//! already run (ADR-0007; ADR-0016 §4, "Placement"). It touches no sample buffer and never blocks
//! capture: the writer thread is already the slow, batched side of a bounded queue, and a stalled
//! database there costs detections nothing (`crate::detect`'s backpressure accounting).
//!
//! Its cost is bounded by construction:
//!
//! - **At most once per emitter per new measurement.** The seam skips an emitter whose sighting
//!   count has not moved since it was last characterised, so the live re-offers of one open track
//!   (T-109, every flush) cost one comparison, not one match and one clustering pass.
//! - **Per call:** one fingerprint projection and fold over at most the `features@1` field set
//!   (≤ 24 fields), one snapshot insert, then `O(catalogue entries × fields)` for the matcher and
//!   `O(open clusters × fields)` for the online leader assignment (plus `O(k²)` over the *accepted*
//!   clusters, which is 0 or 1 in the overwhelming case). No FFT, no sample access, no I/O beyond
//!   the repository it already holds.
//!
//! # Why the family is carried only when it is known
//!
//! `family` is both a matcher gate and a clustering field. An `unknown` call is "not measured",
//! never "measured as unknown": writing it as a field would let two unrelated unknowns agree on it
//! and count it toward the three-field floor that stops a thin measurement joining anything. So a
//! family reaches the snapshot only when a classifier actually named one
//! ([`hk_model::Fingerprint::known_family`] applies the same rule to the fingerprint's own).

use hk_context::signature::cluster::Assignment;
use hk_context::signature::{FeatureObservation, assign_emitter, fold_observation, match_emitter};
use hk_model::signature::field;
use hk_model::{
    EmissionFeatures, EmitterId, Fingerprint, RepoError, Repository, SignatureMatch, Timestamp,
};

/// `Feat::method` of the fields this call site contributes, and the author of the snapshots it
/// writes.
pub const FEATURES_METHOD: &str = "hk-pipeline/characterise@1";

/// What one characterisation produced.
#[derive(Clone, Debug)]
pub struct Characterised {
    /// The snapshot written (the previous one with this sighting folded in).
    pub features: EmissionFeatures,
    /// What the catalogue said, when there was a snapshot to say it about.
    pub signature_match: Option<SignatureMatch>,
    /// What the clusterer decided, including a deliberate abstention.
    pub cluster: Option<Assignment>,
}

/// Whether a label is a measurement rather than "not measured".
fn known_label(label: &str) -> bool {
    let label = label.trim();
    !label.is_empty() && !label.eq_ignore_ascii_case("unknown")
}

/// One sighting's worth of measured fields for `emitter`: its fingerprint (what the tracker and
/// the demodulator chains measured) plus the family, class and SNR of its current classification.
///
/// `None` when the emitter carries no usable fingerprint — nothing measured is nothing to
/// characterise, which is not the same as "measured and featureless".
pub fn observation(
    repo: &Repository,
    emitter: EmitterId,
    suspect: bool,
) -> Result<Option<FeatureObservation>, RepoError> {
    let e = repo.emitter(emitter)?;
    let Some(fp) = Fingerprint::from_value(&e.fingerprint) else {
        return Ok(None);
    };
    let mut obs = FeatureObservation::from_fingerprint(&fp, FEATURES_METHOD).suspect(suspect);
    if let Some(rec) = repo.current_classification(emitter)? {
        match &rec.detail {
            // An M3 row carries the posterior, the class and the measured SNR.
            Some(c) if known_label(&c.family) => obs = obs.with_classification(c),
            // A pre-M3 or chain-written row carries a family and nothing else.
            _ if known_label(&rec.classification.family) => {
                obs = obs.text(field::FAMILY, &rec.classification.family, "classifier");
            }
            _ => {}
        }
    }
    Ok(Some(obs))
}

/// Folds this sighting into the emitter's features snapshot, then matches it against the catalogue
/// and offers it to the clusterer.
///
/// Snapshots are append-only (T-201): the aggregate carries the uncertainty repeated sightings
/// actually showed, so the matcher and the clusterer both widen their tolerances by a field this
/// device measured badly instead of manufacturing a conflict from it.
pub fn characterise(
    repo: &mut Repository,
    emitter: EmitterId,
    suspect: bool,
    t: Timestamp,
) -> Result<Option<Characterised>, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    let Some(obs) = observation(repo, id, suspect)? else {
        return Ok(None);
    };
    characterise_with(repo, id, obs, t).map(Some)
}

/// [`characterise`] for an observation a producer measured itself, rather than one read back off
/// the emitter's fingerprint and classification.
///
/// The fold, the snapshot and the match/cluster step are identical — this is the *same* call site,
/// entered with fields the caller measured. It exists because some measurements are not in the
/// fingerprint and cannot be: a sweep rate is measured from the IQ by a chain
/// ([`crate::chains::sweep`], T-297), and nothing the detector writes could carry it (ADR-0017
/// §1.3(b)). What it writes obeys the same rule as every other path through here — evidence about
/// an emitter, never a change to one.
pub fn characterise_with(
    repo: &mut Repository,
    emitter: EmitterId,
    obs: FeatureObservation,
    t: Timestamp,
) -> Result<Characterised, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    let mut features = repo
        .emitter_features(id)?
        .unwrap_or_else(|| EmissionFeatures::new(String::new(), id, t));
    // A merge re-points an emitter: the snapshot chain continues under the survivor.
    features.emitter_id = id;
    features.t = t;
    fold_observation(&mut features, obs);
    features.id = format!("features:{id}:{}", features.observations);
    repo.put_emission_features(&features)?;

    // Evidence, in this order: what the catalogue says, then whether this measures like something
    // seen before. Clustering reads the match (an identified emission is left alone), so the match
    // must be current first.
    let signature_match = match_emitter(repo, id, t)?;
    let cluster = assign_emitter(repo, id, t)?;
    Ok(Characterised {
        features,
        signature_match,
        cluster,
    })
}

#[cfg(test)]
mod tests {
    //! The T-201 and T-202 invariants, re-proven **through the pipeline's inventory seam**
    //! ([`crate::inventory::TrackInventory`], the `Inventory` trait a run drives) rather than by
    //! calling the matcher and the clusterer directly. Isolation tests already pin the algorithms;
    //! what these pin is that wiring them into a run did not quietly change what they promise.

    use std::collections::BTreeMap;

    use hk_model::signature::{
        FieldExpect, FieldSpec, MatchOutcome, SIGNATURE_SCHEMA, Signature, SignatureKind,
        SignatureProvenance, field,
    };
    use hk_model::{
        Fingerprint, Identity, KnownStatus, LifecycleState, LinkTarget, Repository, Sighting,
        TimeRange, Timestamp, TrackId,
    };

    use crate::inventory::{Inventory, TrackInventory};

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    /// A sensor-like fingerprint: what an FSK chain writes (family, symbol rate, deviation) over
    /// what the tracker measured (centre, bandwidth, period). `dev_hz` is `None` when the chain
    /// never measured a deviation — an absent field, not a zero.
    fn sensor_fp_dev(f_hz: f64, rate_bd: f64, dev_hz: Option<f64>) -> Fingerprint {
        Fingerprint {
            family: Some("2fsk".into()),
            symbol_rate_hz: Some(rate_bd),
            deviation_hz: dev_hz,
            period_s: Some(0.12),
            ..Fingerprint::new(f_hz, 36e3)
        }
    }

    fn sensor_fp(f_hz: f64, rate_bd: f64) -> Fingerprint {
        sensor_fp_dev(f_hz, rate_bd, Some(9600.0))
    }

    /// Drives one chain sighting through the inventory seam, exactly as a chain writer does.
    fn observe(
        repo: &mut Repository,
        inv: &mut TrackInventory,
        f_hz: f64,
        fp: Fingerprint,
        at: i64,
    ) -> hk_model::EmitterId {
        let s = Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(at), t(at + 1)),
            count: 4,
            f_center_hz: f_hz,
            bandwidth_hz: fp.bandwidth_hz,
            fingerprint: Some(fp),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        };
        let id = repo.record_sighting(&s, None).unwrap().emitter_id;
        inv.chain_emitter(repo, None, id).unwrap();
        repo.live_emitter_id(id).unwrap()
    }

    /// A catalogue entry expecting exactly these numeric fields, each within 2 %.
    fn entry(id: &str, fields: &[(&str, f64)]) -> Signature {
        let mut specs: BTreeMap<String, FieldSpec> = BTreeMap::new();
        for (name, value) in fields {
            specs.insert(
                (*name).to_owned(),
                FieldSpec {
                    expect: FieldExpect::Value { value: *value },
                    tolerance: Some(0.02),
                    required: true,
                    weight: 1.0,
                },
            );
        }
        Signature {
            schema: SIGNATURE_SCHEMA,
            id: id.to_owned(),
            version: 1,
            name: format!("test {id}"),
            kind: SignatureKind::Protocol,
            taxonomy: None,
            family: None,
            class: None,
            fields: specs,
            min_discriminating: 3,
            recipe: None,
            provenance: SignatureProvenance::User,
            author: "t242-test".into(),
            created_at: t(0),
            supersedes: None,
            bands_hz: Vec::new(),
            notes: None,
        }
    }

    /// Everything about an emitter that a match or a cluster must never touch.
    fn emitter_state(repo: &Repository, id: hk_model::EmitterId) -> String {
        let e = repo.emitter(id).unwrap();
        format!(
            "identity={:?} status={:?} lifecycle={:?} families={:?} tags={:?}",
            e.identity,
            e.known_status,
            repo.emitter_lifecycle_state(id).unwrap(),
            e.classifications
                .iter()
                .map(|c| c.family.clone())
                .collect::<Vec<_>>(),
            e.tags,
        )
    }

    /// **T-201's contract, at the seam.** A run with the catalogue loaded and a run without it
    /// produce the *same emitter*; the only difference is the match row. So a match sets no
    /// identity, no family, no status and no lifecycle — the strongest verdict (`full`) included.
    #[test]
    fn a_match_written_by_the_pipeline_changes_nothing_about_the_emitter() {
        let fields = [
            (field::SYMBOL_RATE_HZ, 4800.0),
            (field::DEVIATION_HZ, 9600.0),
            (field::PERIOD_S, 0.12),
            (field::OBW_HZ, 36e3),
        ];

        // Without a catalogue.
        let mut bare = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let a = observe(
            &mut bare,
            &mut inv,
            433.92e6,
            sensor_fp(433.92e6, 4800.0),
            10,
        );

        // With one the measurement fits completely.
        let mut cat = Repository::open_in_memory().unwrap();
        cat.insert_signature(&entry("sensor", &fields)).unwrap();
        let mut inv = TrackInventory::default();
        let b = observe(
            &mut cat,
            &mut inv,
            433.92e6,
            sensor_fp(433.92e6, 4800.0),
            10,
        );

        let m = cat
            .current_signature_match(b)
            .unwrap()
            .expect("a match row");
        assert_eq!(
            m.outcome,
            MatchOutcome::Full,
            "the measurement fits the entry: {m:?}"
        );
        assert_eq!(m.top().unwrap().signature.id, "sensor");
        assert_eq!(
            emitter_state(&cat, b),
            emitter_state(&bare, a),
            "a full match must leave the emitter exactly as the same run without a catalogue did"
        );
        assert_eq!(cat.emitter(b).unwrap().identity, Identity::Unknown);
        assert_eq!(cat.emitter(b).unwrap().known_status, KnownStatus::Unknown);
        assert_eq!(
            cat.emitter_lifecycle_state(b).unwrap(),
            LifecycleState::Candidate
        );
        // The bare run still recorded that the catalogue had nothing to say.
        let none = bare.current_signature_match(a).unwrap().expect("a row");
        assert_eq!(none.outcome, MatchOutcome::None);
    }

    /// **Too few fields is a ranked partial, never a confident identity.** An emitter the tracker
    /// has only sized (no chain, so no symbol rate, deviation or family) is scored against an
    /// entry that wants four fields: it may rank, it may not identify.
    #[test]
    fn a_thin_measurement_yields_a_ranked_partial_and_never_an_identity() {
        let mut repo = Repository::open_in_memory().unwrap();
        repo.insert_signature(&entry(
            "sensor",
            &[
                (field::SYMBOL_RATE_HZ, 4800.0),
                (field::DEVIATION_HZ, 9600.0),
                (field::PERIOD_S, 0.12),
                (field::OBW_HZ, 36e3),
            ],
        ))
        .unwrap();
        let mut inv = TrackInventory::default();
        // Track shape only: centre and bandwidth, the width the entry expects.
        let id = observe(
            &mut repo,
            &mut inv,
            433.92e6,
            Fingerprint::new(433.92e6, 36e3),
            10,
        );

        let m = repo.current_signature_match(id).unwrap().expect("a row");
        assert_ne!(
            m.outcome,
            MatchOutcome::Full,
            "one agreeing field must never identify: {m:?}"
        );
        if let Some(c) = m.top() {
            assert!(
                c.score < hk_model::signature::FULL_MATCH_MIN_SCORE,
                "a thin measurement scores low by construction: {c:?}"
            );
            assert!(
                !c.missing.is_empty(),
                "the unmeasured required fields are named: {c:?}"
            );
        }
        assert_eq!(repo.emitter(id).unwrap().identity, Identity::Unknown);
    }

    /// **T-202's contract, at the seam.** Emitters that measure alike share a cluster, and the
    /// cluster changes nothing about any of them.
    #[test]
    fn a_cluster_written_by_the_pipeline_changes_nothing_about_its_members() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        // Three of the same sensor model, on three different channels: frequency is deliberately
        // not part of the distance, so they are one *type* and stay three inventory rows.
        let ids: Vec<_> = [433.0e6, 434.5e6, 868.3e6]
            .into_iter()
            .enumerate()
            .map(|(i, f)| {
                let before = i as i64 * 10;
                observe(&mut repo, &mut inv, f, sensor_fp(f, 4800.0), 10 + before)
            })
            .collect();
        let before: Vec<String> = ids.iter().map(|id| emitter_state(&repo, *id)).collect();

        let cluster = repo.emitter_cluster_id(ids[0]).unwrap().expect("clustered");
        for id in &ids {
            assert_eq!(
                repo.emitter_cluster_id(*id).unwrap().as_deref(),
                Some(cluster.as_str()),
                "the three sensors measure alike, so they share one cluster"
            );
        }
        assert_eq!(repo.cluster_members(&cluster).unwrap().len(), 3);
        let after: Vec<String> = ids.iter().map(|id| emitter_state(&repo, *id)).collect();
        assert_eq!(after, before, "membership is not a change to a member");
        for id in &ids {
            assert_eq!(repo.emitter(*id).unwrap().identity, Identity::Unknown);
            assert_eq!(
                repo.emitter(*id).unwrap().known_status,
                KnownStatus::Unknown
            );
        }
    }

    /// **Ambiguity abstains rather than guessing**, through the seam. Two types that agree on
    /// everything they share except deviation, where they actively disagree (z > 3), are two
    /// clusters. A third emitter whose chain never measured a deviation agrees with *both* on
    /// everything it did measure, so both accept it — and joining either would invent a
    /// relationship the measurement does not support. It is placed in neither, with the reason
    /// recorded.
    #[test]
    fn an_emitter_between_two_incompatible_clusters_is_left_unassigned() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let wide = observe(
            &mut repo,
            &mut inv,
            433.0e6,
            sensor_fp_dev(433.0e6, 4800.0, Some(9600.0)),
            10,
        );
        let narrow = observe(
            &mut repo,
            &mut inv,
            434.0e6,
            sensor_fp_dev(434.0e6, 4800.0, Some(2400.0)),
            20,
        );
        let ca = repo.emitter_cluster_id(wide).unwrap().expect("seeded");
        let cb = repo.emitter_cluster_id(narrow).unwrap().expect("seeded");
        assert_ne!(
            ca, cb,
            "one conflicting field separates them unconditionally"
        );

        // Same family, symbol rate, period and width as both; deviation never measured.
        let mid = observe(
            &mut repo,
            &mut inv,
            868.0e6,
            sensor_fp_dev(868.0e6, 4800.0, None),
            30,
        );
        assert_eq!(
            repo.emitter_cluster_id(mid).unwrap(),
            None,
            "between two incompatible types the clusterer abstains"
        );
        let link = repo.emitter_cluster(mid).unwrap().expect("a decision");
        assert_eq!(link.reason, "ambiguous", "and it records why: {link:?}");
        assert_eq!(repo.emitter(mid).unwrap().identity, Identity::Unknown);
    }

    /// The seam does not pay for a re-offer that measured nothing new (T-109 offers an open track
    /// every flush), and an emitter with no fingerprint at all is never characterised.
    #[test]
    fn characterisation_is_bounded_to_one_per_new_measurement() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut inv = TrackInventory::default();
        let id = observe(
            &mut repo,
            &mut inv,
            433.92e6,
            sensor_fp(433.92e6, 4800.0),
            10,
        );
        let snapshots = repo.emitter_features_history(id, 100).unwrap().len();
        assert_eq!(snapshots, 1);
        // The same emitter offered again with nothing new measured.
        for _ in 0..5 {
            inv.chain_emitter(&mut repo, None, id).unwrap();
        }
        assert_eq!(
            repo.emitter_features_history(id, 100).unwrap().len(),
            snapshots,
            "re-offers of an unchanged emitter write no new snapshot"
        );
        // A genuinely new sighting does.
        observe(
            &mut repo,
            &mut inv,
            433.92e6,
            sensor_fp(433.92e6, 4800.0),
            40,
        );
        let after = repo.emitter_features_history(id, 100).unwrap();
        assert_eq!(after.len(), snapshots + 1);
        assert_eq!(after[0].observations, 2, "the aggregate accumulates");
    }
}

#[cfg(test)]
mod analogue_field_supply_tests {
    //! **T-321: the clustering floor as a margin, not a snapshot.**
    //!
    //! T-309 removed the three observation statistics from `CLUSTER_FIELDS`, which left a purely
    //! analogue emitter with exactly the fields `family`, `obw_hz` and `class` — *exactly*
    //! `CLUSTER_MIN_SHARED_FIELDS`. Sufficient, with no margin: on a run where the within-family
    //! class call fell below its gate, two such emitters shared two fields, the clusterer abstained
    //! on every pair of them, and the operator saw no groups with nothing anywhere saying why.
    //!
    //! So these tests exercise the **degraded** case, not the healthy one only. Each reports the
    //! field counts it actually measured, because the count *is* the thing under test.

    use std::collections::BTreeMap;

    use hk_context::signature::FeatureObservation;
    use hk_context::signature::cluster::CLUSTER_MIN_SHARED_FIELDS;
    use hk_model::signature::field;
    use hk_model::{
        Fingerprint, LinkTarget, Repository, Sighting, TimeRange, Timestamp, TrackId,
        is_cluster_field,
    };

    use super::characterise_with;
    use crate::chains::analog::shape_observation;

    /// What C13 measured for the spectral flatness of one WFM window. Two stations of the same kind
    /// measure alike; the small difference is what a real pair of measurements looks like.
    const FLATNESS_A: f64 = 0.312;
    const FLATNESS_B: f64 = 0.305;
    const C13: &str = "hk-estimate/params@1";

    fn t(sec: i64) -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
    }

    /// One broadcast-FM-shaped emitter: a centre and an occupied bandwidth, and **no** symbol rate,
    /// deviation, line code, sync word, packet length or CRC — a purely analogue emission has none
    /// of those to measure.
    fn analogue_emitter(repo: &mut Repository, f_hz: f64, at: i64) -> hk_model::EmitterId {
        let s = Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(at), t(at + 1)),
            count: 1,
            f_center_hz: f_hz,
            bandwidth_hz: 180e3,
            fingerprint: Some(Fingerprint::new(f_hz, 180e3)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        };
        let id = repo.record_sighting(&s, None).unwrap().emitter_id;
        repo.live_emitter_id(id).unwrap()
    }

    /// The observation the analogue chain writes: what it measured about the emission, with the
    /// class carried only when the classifier actually called one.
    fn analogue_observation(class: Option<&str>, flatness: Option<f64>) -> FeatureObservation {
        let mut obs = FeatureObservation::new()
            .text(field::FAMILY, "wfm", "classifier")
            .num(field::OBW_HZ, 180e3, 0.0, C13);
        if let Some(c) = class {
            obs = obs.text(field::CLASS, c, "classifier");
        }
        // T-321: the same call the chain makes, so the field name and the "not measured means no
        // field" rule are the ones under test rather than a copy of them.
        if let Some(shape) = shape_observation(flatness, C13) {
            for (name, feat) in shape.fields {
                obs.fields.push((name, feat));
            }
        }
        obs
    }

    /// The cluster-eligible fields one emitter has actually measured.
    fn measured_cluster_fields(repo: &Repository, id: hk_model::EmitterId) -> BTreeMap<String, ()> {
        repo.emitter_features(id)
            .unwrap()
            .map(|f| {
                f.fields
                    .keys()
                    .filter(|n| is_cluster_field(n))
                    .map(|n| (n.clone(), ()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What two emitters' cluster ids say about them.
    #[derive(Debug, PartialEq, Eq)]
    enum Grouping {
        /// Both in the same cluster: they measure alike and the system says so.
        OneCluster,
        /// Both clustered, but apart.
        TwoClusters,
        /// At least one is in no cluster at all — the void.
        NoCluster,
    }

    /// Runs two analogue emitters through the characterisation seam and reports what happened:
    /// `(fields measured on each, how the two were grouped, the reason the clusterer recorded for
    /// the second)`.
    fn two_analogue_emitters(
        class: Option<&str>,
        flatness: (Option<f64>, Option<f64>),
    ) -> (usize, usize, Grouping, String) {
        let mut repo = Repository::open_in_memory().unwrap();
        let a = analogue_emitter(&mut repo, 101.3e6, 10);
        let b = analogue_emitter(&mut repo, 95.8e6, 20);
        characterise_with(&mut repo, a, analogue_observation(class, flatness.0), t(11)).unwrap();
        let out = characterise_with(&mut repo, b, analogue_observation(class, flatness.1), t(21))
            .unwrap();
        // `None` (no cluster at all) is counted as its own answer and never as a shared group:
        // "both are in nothing" is precisely the silent outcome this ticket exists to stop.
        let shared = match (
            repo.emitter_cluster_id(a).unwrap(),
            repo.emitter_cluster_id(b).unwrap(),
        ) {
            (Some(x), Some(y)) if x == y => Grouping::OneCluster,
            (Some(_), Some(_)) => Grouping::TwoClusters,
            _ => Grouping::NoCluster,
        };
        (
            measured_cluster_fields(&repo, a).len(),
            measured_cluster_fields(&repo, b).len(),
            shared,
            out.cluster.map(|c| c.reason.to_owned()).unwrap_or_default(),
        )
    }

    /// **The degraded case, which is the bug.** An analogue emitter whose `class` did not measure
    /// keeps `family`, `obw_hz` and the C13 `flatness` the chain now supplies — three fields, so it
    /// still clusters. Before T-321 it had two, and every pair of such emitters abstained with no
    /// group and no explanation.
    #[test]
    fn an_analogue_emitter_whose_class_did_not_measure_still_clusters() {
        let (fields_a, fields_b, grouping, reason) =
            two_analogue_emitters(None, (Some(FLATNESS_A), Some(FLATNESS_B)));
        assert_eq!(
            (fields_a, fields_b),
            (CLUSTER_MIN_SHARED_FIELDS, CLUSTER_MIN_SHARED_FIELDS),
            "class absent leaves family + obw_hz + flatness, which is exactly the floor"
        );
        assert_eq!(
            grouping,
            Grouping::OneCluster,
            "two analogue emitters that measure alike share one cluster; reason was {reason:?}"
        );
        assert_eq!(reason, "joined");
    }

    /// **The healthy case, so the margin was not bought by dropping the floor to nothing.** With
    /// the class measured too, such an emitter carries four fields — one *above*
    /// `CLUSTER_MIN_SHARED_FIELDS`, which is the margin T-321 exists to create.
    #[test]
    fn a_fully_measured_analogue_emitter_now_clusters_with_a_field_to_spare() {
        let (fields_a, fields_b, grouping, reason) =
            two_analogue_emitters(Some("wfm-stereo"), (Some(FLATNESS_A), Some(FLATNESS_B)));
        assert_eq!((fields_a, fields_b), (4, 4));
        assert!(
            fields_a > CLUSTER_MIN_SHARED_FIELDS,
            "a healthy analogue emitter must sit above the floor, not on it"
        );
        assert_eq!(grouping, Grouping::OneCluster, "reason was {reason:?}");
        assert_eq!(reason, "joined");
    }

    /// **The floor still holds, and the absence is still explained.** With neither the class nor the
    /// flatness measured, an analogue emitter has two fields — below the floor — and the clusterer
    /// must abstain *and say so*. A vector that thin is close to everything; joining on it is the
    /// wrong-merge failure the whole guard exists to prevent.
    #[test]
    fn below_the_floor_the_clusterer_still_abstains_and_names_the_shortfall() {
        let (fields_a, fields_b, grouping, reason) = two_analogue_emitters(None, (None, None));
        assert_eq!((fields_a, fields_b), (2, 2));
        assert!(fields_a < CLUSTER_MIN_SHARED_FIELDS);
        assert_eq!(
            grouping,
            Grouping::NoCluster,
            "too thin to compare is not a group, and must not be served as one"
        );
        assert_eq!(
            reason, "too_few_fields",
            "the absence of a group is an explained result, never a void"
        );
    }

    /// An unmeasurable window contributes **nothing**, never a placeholder: absent means not
    /// measured, which is the rule every other field on this path already follows.
    #[test]
    fn an_unmeasured_flatness_contributes_no_field() {
        assert!(shape_observation(None, C13).is_none());
        let obs = shape_observation(Some(FLATNESS_A), C13).expect("a measured flatness is offered");
        assert_eq!(obs.fields.len(), 1);
        assert_eq!(obs.fields[0].0, field::FLATNESS);
        assert!(is_cluster_field(field::FLATNESS));
    }
}
