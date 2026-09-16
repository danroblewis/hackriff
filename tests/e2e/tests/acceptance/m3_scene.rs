//! T-206, the M3 exit gate — **the end-to-end half** (ADR-0016 §7, "all through the mock SDR").
//!
//! Blind scenes served by the mock SDR device, never fed to the pipeline as files. Each run goes
//! through `blind::replay_config`, which strips the recording's truth, seals it away from the run
//! and checks after the fact that nothing truth-only reached the outputs. The hidden truth lives
//! only in the assertions here; no test looks a frequency up and tunes to it, and nothing seeds the
//! catalogue or the inventory from truth.
//!
//! What the three dimensions of §7 need from a run, and what this module asserts:
//!
//! | Dimension | Written by | Asserted here |
//! |---|---|---|
//! | Classification (family, top-k, unknown) | `hk_pipeline::classify` | [`m3_classification_reaches_an_emitter_through_the_device`] |
//! | Signature match | `hk_pipeline::characterise` (T-242) | [`m3_signature_matching_runs_on_real_rows`], [`m3_a_signature_minted_from_measurement_matches_later`] |
//! | Clustering of unknowns | `hk_pipeline::characterise` (T-242) | [`m3_repeated_unknowns_form_one_visible_cluster`] |
//!
//! **A catalogue entry is minted from measurement, never from truth.** The signature this module
//! matches against is built from what the *first run* measured about the emitter
//! ([`mint_from_measurement`]) — the T-242 pattern — so the match proves the matcher ran on real
//! pipeline output rather than on a hand-written fixture that happens to agree with the generator.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::sigmf::SigmfMeta;
use hk_model::signature::{
    EmissionFeatures, FULL_MATCH_MIN_SCORE, FieldExpect, FieldSpec, MatchOutcome, SIGNATURE_SCHEMA,
    Signature, SignatureKind, SignatureProvenance, field,
};
use hk_model::{EmitterId, InventoryQuery, Repository, Timestamp};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T206: &str = "T-206";

/// The sensor scene every test here drives: 2-FSK bursts with a symbol rate, a deviation and a
/// repetition period, so an emitter carries enough measured fields for the matcher's
/// `min_discriminating` floor and the clusterer's three-shared-field floor.
///
/// A macro rather than a function because [`synth_or_skip!`] skips by returning from the enclosing
/// test, which only a `#[test]` body can do.
macro_rules! sensor_scene {
    () => {{
        let out = synth_or_skip!(
            SynthRequest::new("fsk_burst_train")
                .seed(206)
                .param("snr_db", 20.0)
                .param("duration_s", 1.2)
        );
        out.fixture(0).unwrap().meta_path
    }};
}

/// Runs `meta` through the mock SDR into `dir`, blind.
fn run_through_device(dir: &Path, meta: &Path) {
    let (cfg, replay) = replay_config(dir, meta, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, replay));
    assert_eq!(
        summary.always_on_lost_samples, 0,
        "[{T206}] the run lost samples"
    );
}

/// Every live emitter of a finished run.
fn emitters(repo: &Repository) -> Vec<EmitterId> {
    inventory(repo, InventoryQuery::default())
        .into_iter()
        .map(|e| e.emitter.id)
        .collect()
}

/// A copy of `meta` recorded at `center_hz`: the same emission met on a different channel, which is
/// what a *second unit of the same device type* looks like to the radio. Frequency is deliberately
/// not a clustering field, so these become separate emitters of one type.
fn at_centre(meta: &Path, dir: &Path, name: &str, center_hz: f64) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let mut m = SigmfMeta::read(meta).unwrap();
    for c in &mut m.captures {
        if c.frequency.is_some() {
            c.frequency = Some(center_hz);
        }
        if let Some(p) = &mut c.provenance {
            p.tune.center_hz = center_hz;
        }
    }
    if let Some(p) = &mut m.global.provenance {
        p.tune.center_hz = center_hz;
    }
    let out = dir.join(format!("{name}.sigmf-meta"));
    m.write(&out).unwrap();
    std::fs::copy(
        meta.with_extension("sigmf-data"),
        out.with_extension("sigmf-data"),
    )
    .unwrap();
    out
}

/// A catalogue entry built from what the pipeline **measured** about this emitter: every numeric
/// field of its features snapshot, each expected within 5 %.
///
/// This is the T-242 minting pattern and the reason the match below means anything. Nothing here
/// reads the scenario's truth: the expectations come from the run's own `EmissionFeatures`, so an
/// emitter the pipeline measured badly produces an entry that describes what it measured, not what
/// was transmitted.
fn mint_from_measurement(features: &EmissionFeatures, id: &str) -> Signature {
    let mut fields: BTreeMap<String, FieldSpec> = BTreeMap::new();
    for name in [
        field::OBW_HZ,
        field::SYMBOL_RATE_HZ,
        field::DEVIATION_HZ,
        field::PERIOD_S,
    ] {
        if let Some(value) = features.num(name) {
            fields.insert(
                name.to_owned(),
                FieldSpec {
                    expect: FieldExpect::Value { value },
                    tolerance: Some(0.05),
                    required: true,
                    weight: 1.0,
                },
            );
        }
    }
    Signature {
        schema: SIGNATURE_SCHEMA,
        id: id.to_owned(),
        version: 1,
        name: "minted from this run's measurement".into(),
        kind: SignatureKind::DeviceType,
        taxonomy: None,
        family: None,
        class: None,
        fields,
        min_discriminating: 3,
        recipe: None,
        provenance: SignatureProvenance::User,
        author: "t206-acceptance".into(),
        created_at: Timestamp::UNIX_EPOCH,
        supersedes: None,
        bands_hz: Vec::new(),
        notes: None,
    }
}

/// Everything about an emitter a match or a cluster must never touch (the T-242 invariant).
fn emitter_state(repo: &Repository, id: EmitterId) -> String {
    let e = repo.emitter(id).unwrap();
    format!(
        "identity={:?} status={:?} lifecycle={:?} families={:?}",
        e.identity,
        e.known_status,
        repo.emitter_lifecycle_state(id).unwrap(),
        e.classifications
            .iter()
            .map(|c| c.family.clone())
            .collect::<Vec<_>>(),
    )
}

/// **The C15 classifier must produce a classification for an emission the device served**
/// (ADR-0016 §7: the gate's family top-k, unknown rate and per-SNR conditions are all stated over
/// classifications, "all through the mock SDR").
///
/// An M3 row is one carrying `detail` — the full [`hk_model::classify::Classification`] with its
/// posterior, likelihood, open-set score and provenance. A pre-M3 row written by a demodulator
/// chain or a decoder carries a bare family string and `detail: None`; it is evidence, but it is
/// not what §7 measures and it cannot answer "what is the posterior over families, including
/// unknown".
///
/// # Why this reads the emitter's classification **history** and not its current family
///
/// This assertion was originally written over `current_classification` and, as T-247 established,
/// **no honest implementation could satisfy it on this scene.** ADR-0016 §2 ranks a demodulator
/// chain that locked at rank 2 and the classifier at rank 3, and arbitration is "lowest rank,
/// latest among equals". Every emitter here is demodulated and CRC-framed by the FSK chain, so the
/// chain's `2fsk` label *should* own the family — and a C15 row can only become
/// `current_classification` by taking it away, which would turn `2fsk` into `fsk`, regress the M0/M1
/// suites that assert the chain's label (`aware_036`), and contradict the ranking the ADR fixes.
///
/// So what §7 needs measured is that the C15 row **reached** the emitter, which is the classification
/// history; `docs/api.md` already describes exactly this arrangement, serving the arbitrating row as
/// `classification` and the later rank-3 row as `latest_classification`. The test therefore asserts
/// *more* than it used to, not less: an M3 row with a named family and its posterior reached an
/// emitter, **and** arbitration was left undisturbed while it did.
#[test]
fn m3_classification_reaches_an_emitter_through_the_device() {
    let meta = sensor_scene!();
    let dir = TempDir::new("m3cls");
    run_through_device(&dir.0, &meta);
    let repo = repo(&dir.0);

    let ids = emitters(&repo);
    assert!(
        !ids.is_empty(),
        "[{T206}] the run produced no emitters at all, so nothing could be classified"
    );
    let mut current = Vec::new();
    let mut m3 = Vec::new();
    for id in &ids {
        for r in repo.classification_history(*id).unwrap() {
            if let Some(d) = &r.detail {
                m3.push((*id, d.family.clone(), r.stage, r.arb_rank, d.top(3)));
            }
        }
        if let Some(r) = repo.current_classification(*id).unwrap() {
            current.push((*id, r.classification.family.clone(), r.stage, r.arb_rank));
        }
    }
    eprintln!(
        "[{T206}] {} emitters; arbitrating rows {current:?}; M3 rows {m3:?}",
        ids.len()
    );
    assert!(
        !m3.is_empty(),
        "[{T206}] no emitter carries an M3 classification row (one with `detail`) after a run \
         through the mock SDR. {} of {} emitters carry a row at all, each written by a chain or a \
         decoder: {current:?}.\n\
         The C15 cascade is not wired into the pipeline: `hk_pipeline::classify::classify_box` and \
         `hk_pipeline::classify::record` have no caller, so a run writes no posterior, no open-set \
         score and no `unknown`, and ADR-0016 §7's classification floors cannot be measured end to \
         end at all — the accuracy numbers in `m3_grid` are measured on the classifier directly, \
         which is not the same claim.",
        current.len(),
        ids.len()
    );

    // The row is a measurement, not a placeholder: a named family carries a posterior over
    // families that includes `unknown`, which is the thing §7's floors are stated over. An
    // `unknown` call is a legitimate outcome and is not required to name anything.
    for (id, family, _, _, top) in &m3 {
        assert!(
            !top.is_empty(),
            "[{T206}] the M3 row on {id:?} carries no posterior: {m3:?}"
        );
        assert!(
            top.iter().any(|l| l.label == *family),
            "[{T206}] the M3 row's family {family} is not in its own posterior: {top:?}"
        );
    }

    // **Arbitration was not disturbed.** The chain locked and framed these bursts, so ADR-0016 §2
    // leaves the family to it; recording the classifier's evidence beside it must not move it.
    for (id, family, stage, rank) in &current {
        assert!(
            *rank <= hk_model::classify::ArbRank::Classifier,
            "[{T206}] {id:?} fell back to track shape after classification: {current:?}"
        );
        if *stage == hk_model::classify::Stage::Chain {
            assert_eq!(
                family, "2fsk",
                "[{T206}] the C15 row took the demodulator chain's label off {id:?}: {current:?}"
            );
        }
    }
}

/// **Signature matching runs on rows a real run produced** (ADR-0016 §5, T-242).
///
/// Not "a match was found" — with an empty catalogue the honest outcome is `none`. What this pins
/// is that the pipeline *asked*: a features snapshot exists and a match row was written, so the
/// gate's signature dimension is exercising real output rather than reporting green off an empty
/// table.
#[test]
fn m3_signature_matching_runs_on_real_rows() {
    let meta = sensor_scene!();
    let dir = TempDir::new("m3sig");
    run_through_device(&dir.0, &meta);
    let repo = repo(&dir.0);

    let ids = emitters(&repo);
    let mut with_features = 0;
    let mut with_match = 0;
    for id in &ids {
        if let Some(f) = repo.emitter_features(*id).unwrap() {
            with_features += 1;
            eprintln!(
                "[{T206}] emitter {id:?}: {} measured fields over {} observations",
                f.present(),
                f.observations
            );
        }
        if let Some(m) = repo.current_signature_match(*id).unwrap() {
            with_match += 1;
            assert_eq!(
                m.outcome,
                MatchOutcome::None,
                "[{T206}] the built-in catalogue is empty, so the only honest outcome is `none`"
            );
        }
    }
    eprintln!(
        "[{T206}] {} emitters: {with_features} with a features snapshot, {with_match} with a match row",
        ids.len()
    );
    assert!(
        with_features > 0,
        "[{T206}] no emitter carries an EmissionFeatures snapshot: the characterisation call site \
         did not run, so the signature and cluster dimensions would be vacuously empty"
    );
    assert!(
        with_match > 0,
        "[{T206}] no emitter carries a SignatureMatch row after a run through the mock SDR"
    );
}

/// **A catalogue entry minted from measurement matches the emitter it was minted from — and
/// changes nothing about it** (ADR-0016 §5: a match is ranked evidence, never an identity).
///
/// # Why this runs two data directories rather than asserting `Identity::Unknown`
///
/// "A match sets no identity" cannot be checked by asserting an emitter has none. This scene's
/// sensor is framed and CRC-checked by the decoder chain, which legitimately writes a
/// `hk-framing` identity of its own — so an absolute assertion here fails on the *decoder's* work
/// and says nothing about the matcher. The invariant T-242 states is a **difference**: a run with
/// the catalogue and a run without it produce the same emitters.
///
/// So both directories see the same recording twice. `b` has the minted entry inserted before its
/// second run; `a` never has a catalogue at all. Every emitter state must agree, and only `b` may
/// carry a match.
#[test]
fn m3_a_signature_minted_from_measurement_matches_later() {
    let meta = sensor_scene!();
    let dir_a = TempDir::new("m3minta");
    let dir_b = TempDir::new("m3mintb");
    run_through_device(&dir_a.0, &meta);
    run_through_device(&dir_b.0, &meta);

    // The entry is built from what `b`'s own first run measured — never from the scenario's truth.
    let mut cat = repo(&dir_b.0);
    let minted: Vec<Signature> = emitters(&cat)
        .iter()
        .filter_map(|id| cat.emitter_features(*id).unwrap())
        .filter(|f| f.num(field::SYMBOL_RATE_HZ).is_some())
        .enumerate()
        .map(|(i, f)| mint_from_measurement(&f, &format!("t206-unit{i}")))
        .filter(|s| s.fields.len() >= 3)
        .collect();
    assert!(
        !minted.is_empty(),
        "[{T206}] no emitter measured enough numeric fields (rate, deviation, period, OBW) to mint \
         a discriminating entry from, so this dimension could not be exercised"
    );
    eprintln!(
        "[{T206}] minted {} entries from measurement: {:?}",
        minted.len(),
        minted
            .iter()
            .map(|s| (s.id.clone(), s.fields.keys().cloned().collect::<Vec<_>>()))
            .collect::<Vec<_>>()
    );
    for s in &minted {
        s.validate().expect("a minted entry is a valid signature");
        cat.insert_signature(s).unwrap();
    }
    drop(cat);

    // The same second run on both: `b` now has a catalogue, `a` still has none.
    run_through_device(&dir_a.0, &meta);
    run_through_device(&dir_b.0, &meta);
    let (bare, cat) = (repo(&dir_a.0), repo(&dir_b.0));

    let mut outcomes = Vec::new();
    for id in emitters(&cat) {
        if let Some(m) = cat.current_signature_match(id).unwrap() {
            outcomes.push((
                id,
                m.outcome,
                m.top().map(|c| (c.signature.id.clone(), c.score)),
            ));
            if m.outcome == MatchOutcome::Full {
                let top = m.top().expect("a full match has a top candidate");
                assert!(
                    top.score >= FULL_MATCH_MIN_SCORE,
                    "[{T206}] a full match scores at least {FULL_MATCH_MIN_SCORE}: {top:?}"
                );
            }
        }
    }
    eprintln!("[{T206}] outcomes on the second run: {outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|(_, outcome, _)| *outcome == MatchOutcome::Full),
        "[{T206}] an entry minted from this emitter's own measurement did not match it on a later \
         run: {outcomes:?}"
    );

    // The catalogue explained the emitters without changing any of them.
    let state = |repo: &Repository| {
        let mut s: Vec<String> = emitters(repo)
            .iter()
            .map(|id| emitter_state(repo, *id))
            .collect();
        s.sort();
        s
    };
    assert_eq!(
        state(&cat),
        state(&bare),
        "[{T206}] the run with a catalogue produced different emitters from the identical run \
         without one: a match changed the thing it described"
    );
}

/// **The same unknown, met repeatedly, becomes one visible cluster** (ADR-0016 §5, §7's clustering
/// row).
///
/// Three units of one device type on three channels: identical in everything the clusterer
/// compares (family, occupied bandwidth, symbol rate, deviation, period) and different only in
/// frequency, which is deliberately not a clustering field. They must land in **one** cluster with
/// three members — a *type* above three *instances* — and the cluster must set nothing on any of
/// them.
#[test]
fn m3_repeated_unknowns_form_one_visible_cluster() {
    let meta = sensor_scene!();
    let src = TempDir::new("m3clsrc");
    let dir = TempDir::new("m3cluster");
    let base = SigmfMeta::read(&meta)
        .unwrap()
        .captures
        .first()
        .and_then(|c| c.frequency)
        .expect("the scene records a centre");

    // Three appearances of one device type, each on its own channel.
    for (i, offset) in [0.0_f64, 1.5e6, 3.0e6].iter().enumerate() {
        let copy = at_centre(&meta, &src.0, &format!("unit{i}"), base + offset);
        run_through_device(&dir.0, &copy);
    }

    let repo = repo(&dir.0);
    let ids = emitters(&repo);
    let mut by_cluster: BTreeMap<String, Vec<EmitterId>> = BTreeMap::new();
    let mut unassigned = Vec::new();
    for id in &ids {
        match repo.emitter_cluster_id(*id).unwrap() {
            Some(c) => by_cluster.entry(c).or_default().push(*id),
            None => unassigned.push(*id),
        }
    }
    let visible = repo.visible_clusters().unwrap();
    eprintln!(
        "[{T206}] {} emitters over three appearances: {} clustered into {}, {} unassigned; {} \
         visible cluster(s)",
        ids.len(),
        ids.len() - unassigned.len(),
        by_cluster.len(),
        unassigned.len(),
        visible.len()
    );
    for (c, members) in &by_cluster {
        eprintln!("[{T206}]   cluster {c}: {} members", members.len());
    }

    assert!(
        !visible.is_empty(),
        "[{T206}] three appearances of one device type produced no visible cluster \
         (clusters: {by_cluster:?}, unassigned: {unassigned:?})"
    );
    let biggest = by_cluster
        .values()
        .map(Vec::len)
        .max()
        .expect("a cluster exists");
    assert!(
        biggest >= 3,
        "[{T206}] the three units did not group into one cluster: {by_cluster:?}"
    );
    // **A cluster is a *type* above *instances*.** The units that share one cluster stay separate
    // live inventory rows: grouping them is evidence about what they are, never a merge of who
    // they are (ADR-0016 §5, "Identity link"). Asserting they carry no identity would be wrong
    // here — the decoder chain frames this sensor and writes one of its own — so what is asserted
    // is that clustering did not collapse them into each other.
    for (cluster, members) in &by_cluster {
        for id in members {
            assert_eq!(
                repo.live_emitter_id(*id).unwrap(),
                *id,
                "[{T206}] cluster {cluster} membership merged emitter {id:?} into another row"
            );
        }
    }
}
